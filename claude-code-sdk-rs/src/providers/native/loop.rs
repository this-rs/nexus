//! The tool loop: one turn is as many model ↔ tool round trips as it takes to
//! get a message without a tool call (contract §4, §9, §10).
//!
//! Per round trip: budget check, compaction if the prompt nears the window, one
//! streamed model request, then — when the model asked for tools — every call is
//! announced (`tool_call`), the calls run **concurrently**, each ends with its
//! `tool_result`, and the results go back to the model **in call order**.
//!
//! How a turn ends (`done.stop_reason`):
//!
//! | Cause | Event |
//! |---|---|
//! | a message without tool call | `done completed` (`max_tokens` / `refusal` if the endpoint said so) |
//! | a step that called `AskUserQuestion` (once its calls are done) | `done completed`: the answer is the next turn (A45) |
//! | `max_turns` / `max_tool_iterations` reached | `done max_turns` |
//! | token or USD budget spent | `done budget_exceeded` |
//! | `interrupt` | `done interrupted` |
//! | endpoint failure (classified) | `done { is_error, error }`: usage and cost are kept |
//! | `limits.turn_timeout_ms` elapsed (only when set: no cap by default) | `provider_notice { turn_time_limit }`, then `done { error_timeout, error: timeout }` |
//! | an MCP server process died | `error { process_exited }`: the session is dead |
//! | `close` | `error { closed }` |
//!
//! A turn that fails or is closed leaves the transcript as it was before the
//! turn (nothing half-done is committed); a turn that completes, hits a limit or
//! is interrupted commits its messages. A turn stopped by its time limit commits
//! them too, and the next turn's user message starts with a `[system]` line that
//! says so (the model never sees the terminal event: without it, it guesses).

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::future::join_all;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::builtin::{self, BuiltinTool};
use super::cancel::CancelToken;
use super::compaction::{
    STUB_MARKER, apply, apply_stub, count_images, estimate_tokens, plan_stub, should_compact,
    should_stub, split_point, summarise, summary_request,
};
use super::mcp::{IMAGE_NOT_SHOWN, McpError};
use super::session::{ConsentRoute, Core, PendingAsk, StopCause, TurnSignal};
use super::tools::{ToolEntry, ToolRegistry};
use crate::agent::{
    AgentEvent, CompactionInfo, CompactionPhase, CompactionTrigger, Cost, CostBasis, DeltaKind,
    HookVerdict, InputBlock, ModelUsage, PermissionDecision, PolicyDecision, ProviderError,
    QuestionReply, StopReason, ToolCallInfo, ToolCategory, ToolOutput, ToolResultInfo, TurnContext,
    TurnInput, Usage,
};
use crate::model::{
    ChatMessage, CompletionChunk, CompletionRequest, FinishReason, ImagePart, Role, ToolCallChunk,
};

/// How the body of a turn ended, before it becomes a terminal event.
enum Outcome {
    Stop(StopKind, Option<String>),
    /// Stopped from outside; the cause is in the turn signal.
    Interrupted,
    /// Classified endpoint failure: `done { is_error, error }`.
    Failed(ProviderError),
    /// The session is dead: terminal `error`.
    Fatal(ProviderError),
}

#[derive(Clone, Copy)]
enum StopKind {
    Completed,
    MaxTurns,
    MaxTokens,
    Budget,
    Refusal,
}

#[derive(Default)]
struct UsageAcc {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
    reasoning: Option<u64>,
}

fn add(slot: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *slot = Some(slot.unwrap_or(0) + value);
    }
}

struct Acc {
    started: Instant,
    usage: UsageAcc,
    api: Duration,
    requests: u32,
    prompt_tokens: Option<u64>,
}

impl Acc {
    fn add_usage(&mut self, usage: &Usage) {
        add(&mut self.usage.input, usage.input_tokens);
        add(&mut self.usage.output, usage.output_tokens);
        add(&mut self.usage.cache_read, usage.cache_read_tokens);
        add(&mut self.usage.cache_creation, usage.cache_creation_tokens);
        add(&mut self.usage.reasoning, usage.reasoning_tokens);
        if usage.input_tokens.is_some() {
            self.prompt_tokens =
                Some(usage.input_tokens.unwrap_or(0) + usage.cache_read_tokens.unwrap_or(0));
        }
    }
}

/// Runs one turn to its terminal event. Spawned by `send_turn`.
pub(crate) async fn run_turn(core: Arc<Core>, input: TurnInput, signal: Arc<TurnSignal>) {
    let mut acc = Acc {
        started: Instant::now(),
        usage: UsageAcc::default(),
        api: Duration::ZERO,
        requests: 0,
        prompt_tokens: None,
    };
    let timer = core.limits.turn_timeout_ms.map(|ms| {
        let signal = Arc::clone(&signal);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            signal.stop(StopCause::TimedOut(ms));
        })
    });
    // Read, not taken: a turn that is not committed leaves it for the next one.
    let note = core.lock().pending_note.clone();
    let user = user_message(&input, note.as_deref());
    before_turn(&core, &signal, input.joined_text().len()).await;
    let (model, images) = {
        let state = core.lock();
        (state.model.clone(), state.active_images)
    };
    // A `before_turn` directive may have made a model without vision active after
    // `send_turn` accepted the image: the turn fails with the same typed refusal,
    // before anything reaches the model (nothing is committed).
    let outcome = if input.has_images() && !images {
        Outcome::Failed(ProviderError::unsupported("images"))
    } else {
        drive(&core, &signal, &model, user, &mut acc).await
    };
    if let Some(timer) = timer {
        timer.abort();
    }
    if let (Outcome::Interrupted, Some(StopCause::TimedOut(limit_ms))) = (&outcome, signal.cause())
    {
        let elapsed_ms = acc.started.elapsed().as_millis() as u64;
        let note = time_limit_note(limit_ms, elapsed_ms);
        core.lock().pending_note = Some(note.clone());
        core.emit(AgentEvent::ProviderNotice {
            kind: "turn_time_limit".to_owned(),
            data: json!({
                "limit_ms": limit_ms,
                "elapsed_ms": elapsed_ms,
                "next_turn_note": note,
            }),
        });
    }
    // A spent session budget stops every later turn before its first request: the host is
    // told which budget, how much was counted, and the model is told at the next turn
    // that runs (once the budget is raised, or in a resumed session, which counts anew).
    if let Outcome::Stop(StopKind::Budget, _) = &outcome
        && let Some(mut budget) = spent_budget(&core)
    {
        let note = budget_note(&budget, acc.requests);
        {
            // A refused turn (no request) keeps the note of the turn the budget cut, if
            // the model has not read it yet: that one says what happened to its work.
            let mut state = core.lock();
            if acc.requests > 0 || state.pending_note.is_none() {
                state.pending_note = Some(note.clone());
            }
        }
        budget["requests_this_turn"] = json!(acc.requests);
        budget["next_turn_note"] = json!(note);
        core.emit(AgentEvent::ProviderNotice {
            kind: "session_budget_reached".to_owned(),
            data: budget,
        });
    }
    let terminal = terminal_event(&core, &signal, &model, outcome, &acc);
    core.emit(terminal);
}

/// Asks the host, through `SessionHooks::before_turn`, which model the turn runs
/// on. A directive naming another model makes it active (window and cost basis
/// included) and announces it with `model_changed` before anything else of the
/// turn. No hook, a hook that says nothing, that times out or that names the
/// current model: nothing happens.
async fn before_turn(core: &Core, signal: &TurnSignal, input_chars: usize) {
    let Some(hooks) = &core.hooks else {
        return;
    };
    let ctx = {
        let mut state = core.lock();
        let index = state.turn_index;
        state.turn_index += 1;
        let mut ctx = TurnContext::new(index, state.model.clone());
        ctx.input_chars = input_chars;
        ctx.context_tokens = state.last_prompt_tokens;
        ctx.tokens_spent = state.tokens_spent;
        ctx.usd_spent = (state.active_cost != CostBasis::Unknown).then_some(state.usd_spent);
        ctx
    };
    let directive = hook_call(core, signal, "before_turn", hooks.before_turn(&ctx)).await;
    let Some(model) = directive.and_then(|directive| directive.model) else {
        return;
    };
    if model.trim().is_empty() {
        return;
    }
    if core.apply_model(&model).await {
        core.emit(AgentEvent::ModelChanged { model });
    }
}

fn terminal_event(
    core: &Core,
    signal: &TurnSignal,
    model: &str,
    outcome: Outcome,
    acc: &Acc,
) -> AgentEvent {
    let mut usage = Usage {
        input_tokens: acc.usage.input,
        output_tokens: acc.usage.output,
        cache_read_tokens: acc.usage.cache_read,
        cache_creation_tokens: acc.usage.cache_creation,
        reasoning_tokens: acc.usage.reasoning,
        context_tokens: acc.prompt_tokens,
        by_model: Vec::new(),
    };
    // `None` without a price, never a zero (A21); the basis of the model the turn
    // ran on rules (the snapshot's until a model change).
    let (active_cost, active_window) = {
        let state = core.lock();
        (state.active_cost, state.active_window)
    };
    let cost = match active_cost {
        CostBasis::Unknown => Cost::unknown(),
        basis => core.settings.prices.cost(model, &usage, basis),
    };
    usage.by_model = vec![ModelUsage {
        model: model.to_owned(),
        input_tokens: acc.usage.input,
        output_tokens: acc.usage.output,
        cache_read_tokens: acc.usage.cache_read,
        cache_creation_tokens: acc.usage.cache_creation,
        cost_usd: cost.usd,
        context_window: active_window,
    }];
    let done = |stop_reason: StopReason,
                subtype: &str,
                is_error: bool,
                result_text: Option<String>,
                error: Option<ProviderError>| AgentEvent::Done {
        stop_reason,
        subtype: Some(subtype.to_owned()),
        is_error,
        result_text,
        usage: usage.clone(),
        cost,
        duration_ms: acc.started.elapsed().as_millis() as u64,
        duration_api_ms: Some(acc.api.as_millis() as u64),
        num_turns: acc.requests,
        model: Some(model.to_owned()),
        provider_session_id: Some(core.transcript_id.clone()),
        structured_output: None,
        error,
    };
    match outcome {
        Outcome::Stop(kind, text) => match kind {
            StopKind::Completed => done(StopReason::Completed, "success", false, text, None),
            StopKind::MaxTurns => done(StopReason::MaxTurns, "error_max_turns", false, text, None),
            StopKind::MaxTokens => done(StopReason::MaxTokens, "max_tokens", false, text, None),
            StopKind::Budget => done(
                StopReason::BudgetExceeded,
                "error_max_budget",
                false,
                text,
                None,
            ),
            StopKind::Refusal => done(StopReason::Refusal, "refusal", false, text, None),
        },
        Outcome::Interrupted => match signal.cause() {
            Some(StopCause::Closed) => AgentEvent::Error {
                error: ProviderError::Closed,
            },
            Some(StopCause::TimedOut(after_ms)) => done(
                StopReason::Error,
                "error_timeout",
                true,
                None,
                Some(ProviderError::Timeout { after_ms }),
            ),
            _ => done(StopReason::Interrupted, "interrupted", false, None, None),
        },
        Outcome::Failed(error) => done(
            StopReason::Error,
            "error_during_execution",
            true,
            None,
            Some(error),
        ),
        Outcome::Fatal(error) => AgentEvent::Error { error },
    }
}

/// A duration as a person reads it: `25 min`, `25 min 3 s`, `40 s`, `400 ms`.
fn human_duration(ms: u64) -> String {
    let (minutes, seconds) = (ms / 60_000, (ms % 60_000) / 1000);
    match (minutes, seconds) {
        (0, 0) => format!("{ms} ms"),
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}

/// The line the model reads at the start of the turn after one its time limit stopped.
pub(crate) fn time_limit_note(limit_ms: u64, elapsed_ms: u64) -> String {
    format!(
        "[system] Your previous turn was stopped after {} by the configured turn time limit \
         ({}). It was not interrupted by the user. What it did up to that point is above; \
         a tool that was running was stopped.",
        human_duration(elapsed_ms),
        human_duration(limit_ms),
    )
}

/// The user message of a turn: the text blocks joined, then the images (sent
/// as `image_url` parts, kept whole in the transcript). A pending `note` comes
/// first, on its own line: it is part of the new message, so the prefix the
/// endpoint has cached (system prompt, earlier messages) does not change.
fn user_message(input: &TurnInput, note: Option<&str>) -> ChatMessage {
    let images = input
        .blocks
        .iter()
        .filter_map(|block| match block {
            InputBlock::Image {
                media_type,
                data_base64,
            } => Some(ImagePart {
                media_type: media_type.clone(),
                data_base64: data_base64.clone(),
            }),
            _ => None,
        })
        .collect();
    let text = match note {
        Some(note) => format!("{note}\n\n{}", input.joined_text()),
        None => input.joined_text(),
    };
    ChatMessage::user_with_images(text, images)
}

async fn drive(
    core: &Core,
    signal: &TurnSignal,
    model: &str,
    user: ChatMessage,
    acc: &mut Acc,
) -> Outcome {
    let mut messages = core.lock().messages.clone();
    messages.push(user);
    let outcome = iterate(core, signal, model, &mut messages, acc).await;
    let commit = match &outcome {
        // A turn a spent budget refused before its first request did not run: its user
        // message is not committed (the model would later read questions nobody answered).
        Outcome::Stop(StopKind::Budget, _) => acc.requests > 0,
        Outcome::Stop(..) => true,
        // A turn its time limit stopped keeps its work, like an interrupted one: the
        // next turn goes on from it (and is told why it stopped, `pending_note`).
        Outcome::Interrupted => matches!(
            signal.cause(),
            Some(StopCause::Interrupted | StopCause::TimedOut(_))
        ),
        Outcome::Failed(_) | Outcome::Fatal(_) => false,
    };
    if commit {
        {
            let mut state = core.lock();
            state.messages = messages.clone();
            // The user message just committed carries the note, if there was one.
            state.pending_note = None;
        }
        if let Err(error) = core.store.save(&core.transcript_id, &messages) {
            tracing::warn!(
                kind = error.kind(),
                "the native transcript could not be saved"
            );
        }
    }
    outcome
}

fn budget_spent(core: &Core) -> bool {
    spent_budget(core).is_some()
}

/// The session budget that is spent, if one is: what it is, its limit and what was counted.
fn spent_budget(core: &Core) -> Option<Value> {
    let state = core.lock();
    if let Some(max) = core.limits.max_tokens
        && state.tokens_spent >= max
    {
        return Some(json!({
            "budget": "tokens",
            "limit": max,
            "spent": state.tokens_spent,
            "counted": "input not served from the cache + cache writes + output; cache reads excluded",
        }));
    }
    if let Some(max) = core.limits.max_cost_usd
        && state.usd_spent >= max
    {
        return Some(json!({ "budget": "usd", "limit": max, "spent": state.usd_spent }));
    }
    None
}

/// The line the model reads at the start of the turn after one a session budget stopped.
fn budget_note(budget: &Value, requests: u32) -> String {
    let what = match budget["budget"].as_str() {
        Some("usd") => format!(
            "the session cost budget (USD {}) was reached",
            budget["limit"]
        ),
        _ => format!(
            "the session token budget ({} tokens, cache reads not counted) was reached",
            budget["limit"]
        ),
    };
    if requests == 0 {
        format!(
            "[system] Your previous turn did not run: {what} before it started. It was not \
             interrupted by the user."
        )
    } else {
        format!(
            "[system] Your previous turn was stopped because {what}. It was not interrupted by \
             the user. What it did up to that point is above; tool calls it asked for last did \
             not run."
        )
    }
}

fn assistant(text: &str, reasoning: &str, tool_calls: Vec<ToolCallChunk>) -> ChatMessage {
    ChatMessage {
        role: Role::Assistant,
        content: (!text.is_empty() || tool_calls.is_empty()).then(|| text.to_owned()),
        images: Vec::new(),
        reasoning: (!reasoning.is_empty()).then(|| reasoning.to_owned()),
        tool_calls,
        tool_call_id: None,
    }
}

async fn iterate(
    core: &Core,
    signal: &TurnSignal,
    model: &str,
    messages: &mut Vec<ChatMessage>,
    acc: &mut Acc,
) -> Outcome {
    let max_iterations = [core.max_turns, core.limits.max_tool_iterations]
        .into_iter()
        .flatten()
        .min();
    let mut iterations = 0u32;
    loop {
        if signal.token.is_cancelled() {
            return Outcome::Interrupted;
        }
        if max_iterations.is_some_and(|max| iterations >= max) {
            return Outcome::Stop(StopKind::MaxTurns, None);
        }
        if budget_spent(core) {
            return Outcome::Stop(StopKind::Budget, None);
        }
        // The cheap tier first: stubbing tool results the model no longer reads is a
        // free win over a summary, so it takes every chance it gets. Then compaction,
        // which is a convenience: a summary that cannot be written leaves the history
        // as it was and the turn goes on (it says so itself).
        maybe_stub(core, messages);
        maybe_compact(core, signal, model, messages, acc).await;
        if signal.token.is_cancelled() {
            return Outcome::Interrupted;
        }
        let request = build_request(core, model, messages);
        let step = match model_step(core, signal, model, request, acc).await {
            Ok(step) => step,
            Err(error) => return Outcome::Failed(error),
        };
        iterations += 1;
        if step.interrupted {
            if !step.text.is_empty() {
                messages.push(assistant(&step.text, &step.reasoning, Vec::new()));
            }
            return Outcome::Interrupted;
        }
        if step.tool_calls.is_empty() {
            messages.push(assistant(&step.text, &step.reasoning, Vec::new()));
            let kind = match step.finish {
                Some(FinishReason::Length) => StopKind::MaxTokens,
                Some(FinishReason::ContentFilter) => StopKind::Refusal,
                _ => StopKind::Completed,
            };
            return Outcome::Stop(kind, Some(step.text));
        }
        if budget_spent(core) {
            // The tools would run past the budget: keep the words, drop the calls.
            if !step.text.is_empty() {
                messages.push(assistant(&step.text, &step.reasoning, Vec::new()));
            }
            return Outcome::Stop(StopKind::Budget, Some(step.text));
        }
        let calls = normalise_ids(step.tool_calls);
        messages.push(assistant(&step.text, &step.reasoning, calls.clone()));
        let ran = run_tools(core, signal, &calls).await;
        push_results(core, messages, &calls, &ran);
        let asked = ran.iter().any(|run| run.ends_turn);
        if let Some(fatal) = ran.into_iter().find_map(|run| run.fatal) {
            core.mark_dead(fatal.clone());
            return Outcome::Fatal(fatal);
        }
        // A question was put to the user: the turn ends here, the answer is the next one (A45).
        if asked {
            return Outcome::Stop(StopKind::Completed, Some(step.text));
        }
    }
}

/// The tool results, in call order, and the images they returned.
///
/// The OpenAI wire takes images in **user** messages only (a `tool` message is
/// text on most servers), so a model with vision gets them in one user message
/// right after the results, which names the calls; in the result text the marker
/// says where the image went. A model without vision keeps the marker
/// ([`IMAGE_NOT_SHOWN`]) and nothing else.
fn push_results(
    core: &Core,
    messages: &mut Vec<ChatMessage>,
    calls: &[ToolCallChunk],
    ran: &[Run],
) {
    let vision = core.lock().active_images;
    let mut images = Vec::new();
    let mut from = Vec::new();
    for (call, run) in calls.iter().zip(ran) {
        let mut text = with_context(run);
        if vision && !run.images.is_empty() {
            text = text.replace(IMAGE_NOT_SHOWN, TOOL_IMAGE_ATTACHED);
            images.extend(run.images.iter().cloned());
            from.push(call.id.as_str());
        }
        messages.push(ChatMessage::tool(call.id.clone(), text));
    }
    if !images.is_empty() {
        messages.push(ChatMessage::user_with_images(
            format!("[images returned by the tool call(s) {}]", from.join(", ")),
            images,
        ));
    }
}

/// What stands for a tool image in the result text when the image follows.
pub(crate) const TOOL_IMAGE_ATTACHED: &str = "[image: attached in the next message]";

/// An endpoint may omit or repeat tool call ids; the transcript needs unique ones.
fn normalise_ids(mut calls: Vec<ToolCallChunk>) -> Vec<ToolCallChunk> {
    let mut seen = std::collections::HashSet::new();
    for call in &mut calls {
        if call.id.is_empty() || !seen.insert(call.id.clone()) {
            call.id = format!("call_{}", uuid::Uuid::new_v4().simple());
            seen.insert(call.id.clone());
        }
    }
    calls
}

fn build_request(core: &Core, model: &str, messages: &[ChatMessage]) -> CompletionRequest {
    let (policy, vision) = {
        let state = core.lock();
        (state.policy.clone(), state.active_images)
    };
    let mut wire = Vec::with_capacity(messages.len() + 1);
    if let Some(system) = &core.system_prompt {
        wire.push(ChatMessage::system(system.clone()));
    }
    // A history written on a model with vision, now replayed to one without:
    // its images become a marker on the wire (the transcript keeps them).
    wire.extend(messages.iter().map(|message| {
        if vision || message.images.is_empty() {
            message.clone()
        } else {
            message.images_as_text()
        }
    }));
    let mut request = CompletionRequest::new(model, wire);
    request.tools = core
        .registry
        .specs(&policy, core.settings.strict_tool_exposure);
    request.max_tokens = core.settings.max_tokens;
    if !request.tools.is_empty() {
        request.parallel_tool_calls = core.settings.parallel_tool_calls;
    }
    request
}

#[derive(Default)]
struct Step {
    text: String,
    reasoning: String,
    tool_calls: Vec<ToolCallChunk>,
    finish: Option<FinishReason>,
    interrupted: bool,
}

/// Characters per token of the estimate made when an endpoint reports no usage.
const CHARS_PER_TOKEN: u64 = 4;

/// What a request is assumed to have cost when the endpoint reported no usage.
struct Estimate {
    prompt_tokens: u64,
    completion_tokens: u64,
}

/// Records what a response cost against the session's budgets.
///
/// With a `reported` usage that is what counts. Without one, the budgets still
/// have to bite: the tokens are `estimate` (about [`CHARS_PER_TOKEN`] characters
/// per token, over the text sent and received; tool schemas and tokenizer quirks
/// are not counted, so the real figure is higher) and, when the model has a
/// price, the USD budget advances by the price of those tokens. The estimate is
/// returned so the caller can say so (`usage_estimated` notice).
fn spend(
    core: &Core,
    model: &str,
    reported: Option<&Usage>,
    estimate: Estimate,
) -> Option<(u64, Option<f64>)> {
    let mut state = core.lock();
    match reported {
        Some(usage) => {
            // Cache reads are not counted (`SessionLimits::max_tokens`): billed at a
            // fraction, they are most of a long conversation's tokens.
            state.tokens_spent += usage.input_tokens.unwrap_or(0)
                + usage.cache_creation_tokens.unwrap_or(0)
                + usage.output_tokens.unwrap_or(0);
            if let Some(usd) = core.settings.prices.cost_usd(model, usage) {
                state.usd_spent += usd;
            }
            if usage.input_tokens.is_some() {
                state.last_prompt_tokens =
                    Some(usage.input_tokens.unwrap_or(0) + usage.cache_read_tokens.unwrap_or(0));
            }
            None
        },
        None => {
            let tokens = estimate.prompt_tokens + estimate.completion_tokens;
            state.tokens_spent += tokens;
            let usd = core.settings.prices.cost_usd(
                model,
                &Usage {
                    input_tokens: Some(estimate.prompt_tokens),
                    output_tokens: Some(estimate.completion_tokens),
                    ..Usage::default()
                },
            );
            if let Some(usd) = usd {
                state.usd_spent += usd;
            }
            Some((tokens, usd))
        },
    }
}

/// Tells the host that a request was counted on an estimate, not on a report.
fn announce_estimate(core: &Core, estimated: Option<(u64, Option<f64>)>) {
    if let Some((tokens, usd)) = estimated {
        core.emit(AgentEvent::ProviderNotice {
            kind: "usage_estimated".to_owned(),
            data: json!({
                "estimated": true,
                "chars_per_token": CHARS_PER_TOKEN,
                "tokens": tokens,
                "usd": usd,
            }),
        });
    }
}

/// What the request log says of a request: its size and a fingerprint of the parts that
/// a prompt cache needs unchanged from one request to the next.
struct RequestShape {
    messages: usize,
    tools: usize,
    /// Hash of the system prompt (`0` without one).
    system_hash: u64,
    /// Hash of the tool list, in the order sent.
    tools_hash: u64,
    /// What the history check found (`None` when the endpoint cannot show its wire form).
    history: Option<HistoryCheck>,
}

/// The wire messages of a request (the endpoint's dialect applied), against the previous
/// request of the session: a prompt cache serves the longest common prefix, and an
/// append-only history keeps it whole.
struct HistoryCheck {
    /// Hash of every wire message but the last (the part a cache could already hold).
    prefix_hash: u64,
    /// Whether the wire messages of the previous request are, byte for byte, the first
    /// ones of this request; `None` on the first request of the session.
    extends_previous: Option<bool>,
}

impl HistoryCheck {
    /// Hashes `wire` message by message, compares with `core`'s previous request and
    /// records this one as the new previous.
    fn of(core: &Core, wire: &[Value]) -> Self {
        use std::hash::Hasher;
        let previous = core.lock().last_wire;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let (mut prefix_hash, mut at_previous) = (hasher.finish(), None);
        for (index, message) in wire.iter().enumerate() {
            if previous.is_some_and(|(count, _)| count == index) {
                at_previous = Some(hasher.finish());
            }
            if index + 1 == wire.len() {
                prefix_hash = hasher.finish();
            }
            hasher.write(message.to_string().as_bytes());
            hasher.write_u8(0);
        }
        let all = hasher.finish();
        if previous.is_some_and(|(count, _)| count == wire.len()) {
            at_previous = Some(all);
        }
        core.lock().last_wire = Some((wire.len(), all));
        Self {
            prefix_hash,
            extends_previous: previous.map(|(_, hash)| at_previous == Some(hash)),
        }
    }
}

impl RequestShape {
    fn of(core: &Core, request: &CompletionRequest) -> Self {
        use std::hash::{Hash, Hasher};
        let hash = |feed: &dyn Fn(&mut std::collections::hash_map::DefaultHasher)| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            feed(&mut hasher);
            hasher.finish()
        };
        let system = request
            .messages
            .first()
            .filter(|message| message.role == Role::System);
        Self {
            messages: request.messages.len(),
            tools: request.tools.len(),
            system_hash: system.map_or(0, |message| hash(&|h| message.content.hash(h))),
            tools_hash: hash(&|h| {
                for tool in &request.tools {
                    tool.name.hash(h);
                    tool.description.hash(h);
                    tool.parameters.to_string().hash(h);
                }
            }),
            history: core
                .endpoint
                .wire_messages(request)
                .map(|wire| HistoryCheck::of(core, &wire)),
        }
    }
}

/// One INFO line per model request, at its end: what the endpoint reported (prompt,
/// cached part, completion), how long it took, and the fingerprint of the cacheable
/// prefix. Read over a conversation, it gives the real cache-hit ratio and shows a
/// prefix that changed (`system_hash` / `tools_hash` differ between two requests).
/// `extends_previous=false` says the HISTORY changed under the cache (a reasoning no
/// longer echoed, a resumed transcript that differs from the live one, an old tool
/// result stubbed, a compaction…): expected only at a declared rewrite of the history —
/// a stub pass or a summary — never between two ordinary round trips. The hashes are
/// `DefaultHasher`: stable within one process (one build), NOT across Rust versions:
/// compare them inside one run of the server only. No content, no credential: counts,
/// hashes, ids.
fn log_request(
    core: &Core,
    model: &str,
    // Rank of the request in the turn, from 1.
    request: u32,
    shape: &RequestShape,
    usage: Option<&Usage>,
    latency: Duration,
    first_chunk: Option<Duration>,
) {
    let cached = usage.and_then(|usage| usage.cache_read_tokens);
    let prefix_hash = shape
        .history
        .as_ref()
        .map(|history| format!("{:016x}", history.prefix_hash));
    let prompt = usage
        .and_then(|usage| usage.input_tokens)
        .map(|input| input + cached.unwrap_or(0));
    tracing::info!(
        target: "nexus_claude::native::request",
        transcript_id = %core.transcript_id,
        host_session_id = core.host_session_id.as_deref().unwrap_or("-"),
        model,
        request,
        prompt_tokens = prompt,
        cached_tokens = cached,
        completion_tokens = usage.and_then(|usage| usage.output_tokens),
        reasoning_tokens = usage.and_then(|usage| usage.reasoning_tokens),
        usage_reported = usage.is_some(),
        latency_ms = latency.as_millis() as u64,
        first_chunk_ms = first_chunk.map(|elapsed| elapsed.as_millis() as u64),
        messages = shape.messages,
        tools = shape.tools,
        system_hash = format_args!("{:016x}", shape.system_hash),
        tools_hash = format_args!("{:016x}", shape.tools_hash),
        prefix_hash = prefix_hash.as_deref().map(tracing::field::display),
        extends_previous = shape.history.as_ref().and_then(|history| history.extends_previous),
        "native model request"
    );
}

async fn model_step(
    core: &Core,
    signal: &TurnSignal,
    model: &str,
    request: CompletionRequest,
    acc: &mut Acc,
) -> Result<Step, ProviderError> {
    let started = Instant::now();
    let prompt_estimate = estimate_tokens(&request.messages);
    let shape = RequestShape::of(core, &request);
    let mut first_chunk: Option<Duration> = None;
    let mut step = Step::default();
    let opened = tokio::select! {
        opened = core.endpoint.complete(request) => opened,
        () = signal.token.cancelled() => {
            step.interrupted = true;
            return Ok(step);
        },
    };
    let mut stream = opened?;
    let mut usage: Option<Usage> = None;
    loop {
        let next = tokio::select! {
            next = stream.next() => next,
            () = signal.token.cancelled() => {
                step.interrupted = true;
                break;
            },
        };
        let Some(chunk) = next else { break };
        first_chunk.get_or_insert_with(|| started.elapsed());
        match chunk? {
            CompletionChunk::Text(text) => {
                if core.deltas {
                    core.emit(AgentEvent::Delta {
                        kind: DeltaKind::Text,
                        text: text.clone(),
                        index: None,
                        tool_call_id: None,
                        parent: None,
                    });
                }
                step.text.push_str(&text);
            },
            CompletionChunk::Reasoning(text) => {
                if core.deltas && core.capabilities.thinking {
                    core.emit(AgentEvent::Delta {
                        kind: DeltaKind::Thinking,
                        text: text.clone(),
                        index: None,
                        tool_call_id: None,
                        parent: None,
                    });
                }
                step.reasoning.push_str(&text);
            },
            CompletionChunk::ToolCall(call) => step.tool_calls.push(call),
            CompletionChunk::Usage(reported) => usage = Some(reported),
            CompletionChunk::Finish(reason) => step.finish = Some(reason),
            #[allow(unreachable_patterns)]
            _ => {},
        }
    }
    drop(stream);
    acc.api += started.elapsed();
    acc.requests += 1;
    log_request(
        core,
        model,
        acc.requests,
        &shape,
        usage.as_ref(),
        started.elapsed(),
        first_chunk,
    );
    let completion_estimate = (step.text.len() + step.reasoning.len()) as u64 / CHARS_PER_TOKEN
        + step
            .tool_calls
            .iter()
            .map(|c| (c.name.len() + c.arguments.len()) as u64 / CHARS_PER_TOKEN)
            .sum::<u64>();
    let estimated = spend(
        core,
        model,
        usage.as_ref(),
        Estimate {
            prompt_tokens: prompt_estimate,
            completion_tokens: completion_estimate,
        },
    );
    announce_estimate(core, estimated);
    if let Some(usage) = &usage {
        acc.add_usage(usage);
    }
    // The reasoning stays in the transcript either way; it is only shown when
    // the model is known to think (an event for an absent capability is refused).
    if !step.reasoning.is_empty() && core.capabilities.thinking {
        core.emit(AgentEvent::Thinking {
            text: step.reasoning.clone(),
            signature: None,
            seq: None,
            parent: None,
        });
    }
    if !step.text.is_empty() {
        core.emit(AgentEvent::Text {
            text: step.text.clone(),
            seq: None,
            parent: None,
        });
    }
    Ok(step)
}

/// What the prompt costs now, and the window of the model now active: the larger of
/// the estimate and the last size the endpoint reported (its report is the truth when
/// it has one).
fn prompt_pressure(core: &Core, messages: &[ChatMessage]) -> (u64, Option<u64>) {
    // The window of the model now active, not the snapshot's: a smaller model chosen
    // for this turn compacts where the opening one would not.
    let window = core.lock().active_window;
    let system = core
        .system_prompt
        .as_deref()
        .map_or(0, |text| text.len() as u64 / 4);
    let estimate = estimate_tokens(messages) + system;
    let reported = core.lock().last_prompt_tokens.unwrap_or(0);
    (estimate.max(reported), window)
}

/// Stubs the tool results older than the last rounds: the cheap tier of the context
/// economy, taken before the summary (see [`super::compaction`]).
///
/// It costs nothing at the endpoint — no call, no summary, and nothing the model still
/// reads is touched. What it does cost is **one cache break** (the history changes
/// under the prompt cache, so the suffix is written again), which is why it happens
/// only under real pressure ([`should_stub`]) and one block at a time
/// (`CompactionConfig::stub_block`). The host is told: nothing goes in silence.
fn maybe_stub(core: &Core, messages: &mut [ChatMessage]) {
    let config = core.settings.compaction;
    let (size, window) = prompt_pressure(core, messages);
    if !should_stub(&config, window, size) {
        return;
    }
    let Some(plan) = plan_stub(messages, &config) else {
        return;
    };
    let elided = apply_stub(messages, &plan);
    if elided == 0 {
        return;
    }
    // The size the endpoint last reported describes a history that has just shrunk: kept
    // as is, it would make the compaction below fire on a prompt that no longer exists
    // (the tier would relieve the window and be summarised anyway). Dropped, the next
    // round trip measures the real one — estimate first, then the endpoint's report.
    core.lock().last_prompt_tokens = None;
    core.emit(AgentEvent::ProviderNotice {
        kind: "tool_results_stubbed".to_owned(),
        data: json!({
            "count": elided,
            "kept_rounds": config.keep_tool_rounds,
            "chars_freed": plan.chars_freed,
            "stub": STUB_MARKER,
        }),
    });
}

/// Compacts the prompt when it nears the context window (see [`super::compaction`]).
///
/// Compaction is a **convenience**, never a condition of the turn: its summary is
/// written by the same endpoint, and when that call fails — an endpoint error, a
/// summary that comes back empty — the turn goes on with the history **intact**, and
/// the failure is said with `provider_notice { compaction_failed }` (the trigger, the
/// typed `code`, the redacted message). A turn ends on the endpoint's own refusal of a
/// prompt it cannot hold, with that refusal's typed error (`context_too_small` and the
/// like) — never on this step.
///
/// Nothing is retried here: the summary is asked once per round trip of the tool loop
/// (whose own bound is what stops a runaway turn), and a retry inside this function
/// would ask a failing endpoint again and again for a summary the turn can do without.
async fn maybe_compact(
    core: &Core,
    signal: &TurnSignal,
    model: &str,
    messages: &mut Vec<ChatMessage>,
    acc: &mut Acc,
) {
    let config = core.settings.compaction;
    let (size, window) = prompt_pressure(core, messages);
    if !should_compact(&config, window, size) {
        return;
    }
    let Some(split) = split_point(messages, config.keep_recent) else {
        return;
    };
    core.emit(AgentEvent::Compaction {
        phase: CompactionPhase::Started,
        trigger: Some(CompactionTrigger::Auto),
        pre_tokens: Some(size),
    });
    let instructions = match &core.hooks {
        Some(hooks) => {
            let info = CompactionInfo {
                trigger: "auto".to_owned(),
                custom_instructions: None,
            };
            hook_call(
                core,
                signal,
                "before_compaction",
                hooks.before_compaction(&info),
            )
            .await
            .flatten()
        },
        None => None,
    };
    let request = summary_request(model, &messages[..split], instructions.as_deref(), &config);
    let summary_prompt_estimate = estimate_tokens(&request.messages);
    let started = Instant::now();
    let summary = tokio::select! {
        summary = summarise(core.endpoint.as_ref(), request) => summary,
        () = signal.token.cancelled() => return,
    };
    let (text, usage) = match summary {
        Ok(summary) => {
            acc.api += started.elapsed();
            summary
        },
        Err(error) => {
            // Fail open: the history stays exactly as it was and the host is told what
            // went wrong, with the typed code and a message that already went through
            // `redact` (a credential the endpoint echoed never reaches an event).
            core.emit(AgentEvent::ProviderNotice {
                kind: "compaction_failed".to_owned(),
                data: json!({
                    "trigger": "auto",
                    "code": error.kind(),
                    "message": error.to_string(),
                }),
            });
            return;
        },
    };
    let estimated = spend(
        core,
        model,
        usage.as_ref(),
        Estimate {
            prompt_tokens: summary_prompt_estimate,
            completion_tokens: text.len() as u64 / CHARS_PER_TOKEN,
        },
    );
    announce_estimate(core, estimated);
    if let Some(usage) = &usage {
        acc.add_usage(usage);
    }
    // Neither the summary's prompt nor the old reports say how big the new history is.
    acc.prompt_tokens = None;
    // Images of the summarised history reached the summary as "[image]": the
    // model keeps what it said of them, not the pixels. Said, never silent.
    let images = count_images(&messages[..split]);
    if images > 0 {
        core.emit(AgentEvent::ProviderNotice {
            kind: "images_compacted".to_owned(),
            data: json!({ "count": images, "summarised_as": "[image]" }),
        });
    }
    *messages = apply(std::mem::take(messages), split, &text);
    core.lock().last_prompt_tokens = None;
    core.emit(AgentEvent::Compaction {
        phase: CompactionPhase::Completed,
        trigger: Some(CompactionTrigger::Auto),
        pre_tokens: Some(size),
    });
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

struct Run {
    content: String,
    /// Text a hook asked to put in front of the model with this result. It is not part of the
    /// `tool_result` event: the host that wrote the hook already has it.
    context: Vec<String>,
    /// Images the tool returned (MCP `image` blocks): for a model with vision.
    images: Vec<ImagePart>,
    /// The call asked the user a question (`AskUserQuestion`): the turn ends once the step's
    /// calls are done, the answer being the next turn (A45).
    ends_turn: bool,
    fatal: Option<ProviderError>,
}

async fn run_tools(core: &Core, signal: &TurnSignal, calls: &[ToolCallChunk]) -> Vec<Run> {
    // Tokens first: a consumer that reacts to `tool_call` may cancel at once.
    let tokens: Vec<_> = calls.iter().map(|call| core.begin_tool(&call.id)).collect();
    for call in calls {
        // A `nexus-tools` call is rendered under its canonical name and real category (N24).
        let entry = core.registry.get(&call.name);
        core.emit(AgentEvent::ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            input: parse_arguments(&call.arguments).unwrap_or_else(|()| json!({})),
            category: entry.map_or(ToolCategory::Mcp, |entry| entry.category),
            canonical: Some(
                entry
                    .and_then(|entry| entry.canonical.clone())
                    .unwrap_or_else(|| call.name.clone()),
            ),
            input_complete: true,
            seq: None,
            parent: None,
        });
    }
    join_all(
        calls
            .iter()
            .zip(tokens)
            .map(|(call, token)| run_one(core, signal, call, token)),
    )
    .await
}

fn parse_arguments(raw: &str) -> Result<Value, ()> {
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value @ Value::Object(_)) => Ok(value),
        _ => Err(()),
    }
}

async fn run_one(
    core: &Core,
    signal: &TurnSignal,
    call: &ToolCallChunk,
    token: CancelToken,
) -> Run {
    let mut context = Vec::new();
    let mut images = Vec::new();
    let mut task = None;
    let mut ends_turn = false;
    let (content, is_error, fatal) = execute(
        core,
        signal,
        call,
        &token,
        Outputs {
            context: &mut context,
            images: &mut images,
            task: &mut task,
            ends_turn: &mut ends_turn,
        },
    )
    .await;
    core.end_tool(&call.id);
    core.emit(AgentEvent::ToolResult {
        id: call.id.clone(),
        output: Some(ToolOutput::Text(content.clone())),
        is_error,
        seq: None,
        parent: None,
    });
    // The task the call started or stopped, once its result is out: `background_tasks` follows.
    if let Some(structured) = task {
        core.track_task(&structured, &call.id);
    }
    Run {
        content,
        context,
        images,
        ends_turn,
        fatal,
    }
}

fn stopped_message(signal: &TurnSignal, token: &CancelToken) -> String {
    if token.is_cancelled() && !signal.token.is_cancelled() {
        "cancelled by the user".to_owned()
    } else {
        "interrupted".to_owned()
    }
}

enum Asked {
    Allow(Option<Value>),
    Deny(String),
    Stopped,
}

/// Asks the human about a call. `subject` names what the answer is scoped to when the ask comes
/// from a hook (`HookVerdict::RequireConsent`, N23b): a `session` answer is then remembered for
/// that subject alone; `None` is an ordinary tool approval.
async fn ask(
    core: &Core,
    signal: &TurnSignal,
    entry: &ToolEntry,
    call: &ToolCallChunk,
    input: &Value,
    token: &CancelToken,
    subject: Option<&str>,
) -> Asked {
    let request_id = format!("perm_{}", uuid::Uuid::new_v4().simple());
    let (answer, answered) = oneshot::channel();
    core.lock().pending.insert(
        request_id.clone(),
        PendingAsk {
            tool: entry.name.clone(),
            subject: subject.map(str::to_owned),
            answer,
        },
    );
    core.emit(AgentEvent::PermissionAsk {
        request_id: request_id.clone(),
        tool_name: entry.name.clone(),
        input: input.clone(),
        category: entry.category,
        canonical: Some(
            entry
                .canonical
                .clone()
                .unwrap_or_else(|| entry.name.clone()),
        ),
        tool_call_id: Some(call.id.clone()),
        scopes: core.capabilities.permission_scopes.clone(),
        parent: None,
    });
    let decision = tokio::select! {
        decision = answered => decision.ok(),
        () = signal.token.cancelled() => None,
        () = token.cancelled() => None,
    };
    let Some(decision) = decision else {
        core.lock().pending.remove(&request_id);
        return Asked::Stopped;
    };
    match decision {
        PermissionDecision::Allow { updated_input, .. } => Asked::Allow(updated_input),
        PermissionDecision::Deny { message, interrupt } => {
            if interrupt {
                signal.stop(StopCause::Interrupted);
            }
            Asked::Deny(message.unwrap_or_else(|| "permission denied by the user".to_owned()))
        },
        #[allow(unreachable_patterns)]
        _ => Asked::Deny("permission denied".to_owned()),
    }
}

/// Whether a result of this tool may report a background task: `Bash`, `Monitor` or `TaskStop`
/// of the session's `nexus` server, never a tool of another server.
fn is_task_tool(entry: &ToolEntry) -> bool {
    entry.server == super::tools::NEXUS_TOOLS_SERVER
        && matches!(
            entry.canonical.as_deref(),
            Some("Bash" | "Monitor" | "TaskStop")
        )
}

/// What a call produces besides its result text.
struct Outputs<'a> {
    context: &'a mut Vec<String>,
    images: &'a mut Vec<ImagePart>,
    task: &'a mut Option<Value>,
    ends_turn: &'a mut bool,
}

/// Runs one call: `(content for the model, is_error, fatal)`.
async fn execute(
    core: &Core,
    signal: &TurnSignal,
    call: &ToolCallChunk,
    token: &CancelToken,
    out: Outputs<'_>,
) -> (String, bool, Option<ProviderError>) {
    let Outputs {
        context,
        images,
        task,
        ends_turn,
    } = out;
    let fail = |message: &str| (message.to_owned(), true, None);
    let Some(entry) = core.registry.get(&call.name) else {
        return fail("unknown tool");
    };
    let policy = core.lock().policy.clone();
    if !ToolRegistry::is_exposed(entry, &policy, core.settings.strict_tool_exposure) {
        return fail("tool not available under the current policy");
    }
    let Ok(mut input) = parse_arguments(&call.arguments) else {
        return fail("the tool arguments are not a JSON object");
    };
    // The browser's destinations are judged before anyone is asked about them (N23).
    if entry.server == super::browser::BROWSER_SERVER
        && let Err(reason) = super::browser::guard_call(&entry.tool, &input)
    {
        return fail(&reason);
    }
    // The host's say comes first, and what it changes is judged by the policy like anything else.
    if let Some(hooks) = &core.hooks {
        let info = ToolCallInfo {
            id: Some(call.id.clone()),
            name: call.name.clone(),
            canonical: entry.canonical.clone(),
            category: entry.category,
            input: input.clone(),
        };
        match hook_call(core, signal, "before_tool", hooks.before_tool(&info)).await {
            Some(HookVerdict::Deny { reason }) => return (reason, true, None),
            Some(HookVerdict::ReplaceInput(replaced)) => {
                if !replaced.is_object() {
                    return fail(
                        "a hook replaced the tool input with something that is not a JSON object",
                    );
                }
                input = replaced;
            },
            Some(HookVerdict::AddContext(text)) => context.push(text),
            // A hook does not decide for a human (N23b): it names the subject, and the answer goes
            // through the permission channel the session already has. An approval already given
            // for this very subject lets the call through; a session where nobody can be asked —
            // an agent session, a delegation, a runner — refuses, and there is no path around it.
            Some(HookVerdict::RequireConsent { subject, reason }) => {
                match super::session::consent_route(
                    core.consented(&entry.name, &subject),
                    core.capabilities.interactive_permissions,
                ) {
                    ConsentRoute::Approved => {},
                    ConsentRoute::Ask => {
                        match ask(core, signal, entry, call, &input, token, Some(&subject)).await {
                            Asked::Allow(updated) => {
                                if let Some(updated) = updated {
                                    input = updated;
                                }
                            },
                            Asked::Deny(message) => return (message, true, None),
                            Asked::Stopped => return (stopped_message(signal, token), true, None),
                        }
                    },
                    ConsentRoute::Refused => return (reason, true, None),
                }
            },
            _ => {},
        }
        if signal.token.is_cancelled() || token.is_cancelled() {
            return (stopped_message(signal, token), true, None);
        }
    }
    // The harness's own tools ask no permission and reach no server (`builtin.rs`); a `deny`
    // still refuses them, judged on the input the hooks left (an argument included).
    if let Some(kind) = entry.builtin {
        if builtin::denied(&core.lock().policy, entry, &input) {
            return fail("permission denied by policy");
        }
        let ran_input = input.clone();
        let (content, is_error) = match kind {
            BuiltinTool::AskUserQuestion => ask_user(core, call, input, ends_turn),
            BuiltinTool::Agent => match run_subagent(core, signal, call, token, &input).await {
                Some(outcome) => outcome,
                None => return (stopped_message(signal, token), true, None),
            },
        };
        after_tool(
            core, signal, call, entry, ran_input, &content, is_error, context,
        )
        .await;
        return (content, is_error, None);
    }
    match core.decide(entry, &input) {
        PolicyDecision::Allow => {},
        PolicyDecision::Ask if core.capabilities.interactive_permissions => {
            match ask(core, signal, entry, call, &input, token, None).await {
                Asked::Allow(updated) => {
                    if let Some(updated) = updated {
                        input = updated;
                    }
                },
                Asked::Deny(message) => return (message, true, None),
                Asked::Stopped => return (stopped_message(signal, token), true, None),
            }
        },
        // Denied, or would ask and nobody can be asked (contract §5 fallback).
        _ => return fail("permission denied by policy"),
    }
    let Some(client) = core.mcp.get(&entry.server) else {
        return fail("the tool's server is not connected");
    };
    let cancelled = async {
        tokio::select! {
            () = signal.token.cancelled() => {},
            () = token.cancelled() => {},
        }
    };
    let ran_input = input.clone();
    let (content, is_error, fatal) = match client.call_tool(&entry.tool, input, cancelled).await {
        Ok(result) => {
            *images = result.images;
            if is_task_tool(entry) {
                *task = result.structured;
            }
            (result.text, result.is_error, None)
        },
        Err(McpError::Cancelled) => return (stopped_message(signal, token), true, None),
        Err(McpError::Died { code }) => {
            return (
                "the tool server exited".to_owned(),
                true,
                Some(ProviderError::ProcessExited { code }),
            );
        },
        Err(McpError::Failed(error)) => (format!("tool call failed: {error}"), true, None),
        Err(McpError::Rpc { message }) => (message, true, None),
    };
    after_tool(
        core, signal, call, entry, ran_input, &content, is_error, context,
    )
    .await;
    (content, is_error, fatal)
}

/// What the host adds after a call that ran (`after_tool`): pushed to `context`.
#[allow(clippy::too_many_arguments)]
async fn after_tool(
    core: &Core,
    signal: &TurnSignal,
    call: &ToolCallChunk,
    entry: &ToolEntry,
    ran_input: Value,
    content: &str,
    is_error: bool,
    context: &mut Vec<String>,
) {
    let Some(hooks) = &core.hooks else {
        return;
    };
    let info = ToolResultInfo {
        call: ToolCallInfo {
            id: Some(call.id.clone()),
            name: call.name.clone(),
            canonical: entry.canonical.clone(),
            category: entry.category,
            input: ran_input,
        },
        output: Value::String(content.to_owned()),
        is_error,
    };
    if let Some(Some(text)) = hook_call(core, signal, "after_tool", hooks.after_tool(&info)).await {
        context.push(text);
    }
}

/// `AskUserQuestion`: the question goes to the host (`question { reply: turn }`) and the turn
/// will end after this step (A45). A call without a readable question is an error result, and
/// the turn goes on.
fn ask_user(
    core: &Core,
    call: &ToolCallChunk,
    input: Value,
    ends_turn: &mut bool,
) -> (String, bool) {
    match builtin::questions(&input) {
        Err(reason) => (reason, true),
        Ok(questions) => {
            core.emit(AgentEvent::Question {
                question_id: format!("q_{}", call.id),
                tool_call_id: Some(call.id.clone()),
                reply: QuestionReply::Turn,
                questions,
                input,
                parent: None,
            });
            *ends_turn = true;
            (builtin::QUESTION_ASKED.to_owned(), false)
        },
    }
}

/// `Agent`: the host runs the sub-agent (A18). `None` when the turn was interrupted or the call
/// cancelled meanwhile: the future is dropped, which tells the host to stop it. Not bounded by
/// the hook timeout (a sub-agent takes as long as its work; the turn timeout still applies).
async fn run_subagent(
    core: &Core,
    signal: &TurnSignal,
    call: &ToolCallChunk,
    token: &CancelToken,
    input: &Value,
) -> Option<(String, bool)> {
    let request = match builtin::subagent_request(&call.id, input) {
        Ok(request) => request,
        Err(reason) => return Some((reason, true)),
    };
    let Some(hooks) = &core.hooks else {
        return Some(("this host runs no sub-agent".to_owned(), true));
    };
    tokio::select! {
        outcome = hooks.run_subagent(&request) => Some(match outcome {
            Ok(text) => (text, false),
            Err(message) => (message, true),
        }),
        () = signal.token.cancelled() => None,
        () = token.cancelled() => None,
    }
}

/// The tool result as the model reads it: the output, then what hooks added.
fn with_context(run: &Run) -> String {
    let mut text = run.content.clone();
    for added in &run.context {
        text.push_str("\n\n");
        text.push_str(added);
    }
    text
}

/// Awaits a host hook, but never past the turn's end or `NativeConfig::hook_timeout`. `None` means the hook
/// gave no answer (the turn was stopped, or it took too long: a notice says so).
async fn hook_call<T>(
    core: &Core,
    signal: &TurnSignal,
    name: &str,
    call: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        answer = tokio::time::timeout(core.settings.hook_timeout, call) => match answer {
            Ok(answer) => Some(answer),
            Err(_) => {
                core.emit(AgentEvent::ProviderNotice {
                    kind: "hook_timeout".to_owned(),
                    data: json!({ "provider": "native", "hook": name }),
                });
                None
            },
        },
        () = signal.token.cancelled() => None,
    }
}

#[cfg(test)]
mod consent_tests {
    //! The fail-closed half of `HookVerdict::RequireConsent` (N23b), at the level of the one
    //! function that decides it: a session where nobody can be asked never emits a
    //! `PermissionAsk` and never runs the call. A native session always *can* ask, so this state
    //! is reached here with a `Core` whose capabilities say so — the same shape a runner, a
    //! delegation or an agent session has on the host side.

    use std::collections::HashMap;
    use std::sync::Arc;

    use serde_json::json;

    use super::{Outputs, execute};
    use crate::agent::{
        Capabilities, HookVerdict, ModelInfo, ProviderError, ProviderHealth, SessionHooks,
        ToolCallInfo, ToolPolicy,
    };
    use crate::model::{
        CompletionRequest, CompletionStream, EndpointProbe, ModelEndpoint, ToolCallChunk,
    };
    use crate::providers::native::ModelFacts;
    use crate::providers::native::cancel::CancelToken;
    use crate::providers::native::session::{Core, CoreParts, TurnSignal};
    use crate::providers::native::tools::{ToolEntry, ToolRegistry};
    use crate::providers::native::transcript::MemoryTranscriptStore;
    use crate::providers::native::{NativeConfig, tools};

    /// An endpoint nothing ever asks anything of: this test never reaches the model.
    struct UnusedEndpoint;

    #[async_trait::async_trait]
    impl ModelEndpoint for UnusedEndpoint {
        fn id(&self) -> &str {
            "unused"
        }

        async fn health(&self) -> ProviderHealth {
            ProviderHealth::ok(None)
        }

        async fn models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionStream, ProviderError> {
            panic!("the consent path never reaches the model")
        }

        async fn probe(&self, _model: &str) -> Result<EndpointProbe, ProviderError> {
            panic!("the consent path never probes the model")
        }
    }

    /// A hook that judges the destination of a browser call, as the graph does.
    struct OriginHook {
        verdict: HookVerdict,
        calls: std::sync::Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl SessionHooks for OriginHook {
        async fn before_tool(&self, _call: &ToolCallInfo) -> HookVerdict {
            *self.calls.lock().unwrap() += 1;
            self.verdict.clone()
        }
    }

    /// A session whose capabilities say whether a human can be asked, with one browser tool.
    fn core(interactive: bool, verdict: HookVerdict) -> (Arc<Core>, Arc<OriginHook>) {
        let mut capabilities = Capabilities::none();
        capabilities.interactive_permissions = interactive;
        let settings = Arc::new(NativeConfig::new("consent-test"));
        let mut registry = ToolRegistry::new();
        registry
            .insert(ToolEntry {
                name: tools::exposed_name("browser", "navigate"),
                server: "browser".to_owned(),
                tool: "navigate".to_owned(),
                description: "go to a URL".to_owned(),
                schema: json!({"type": "object"}),
                read_only: false,
                canonical: None,
                category: crate::agent::ToolCategory::Web,
                builtin: None,
            })
            .expect("one tool");
        let hooks = Arc::new(OriginHook {
            verdict,
            calls: std::sync::Mutex::new(0),
        });
        let core = Core::new(CoreParts {
            capabilities,
            facts: ModelFacts::new(Arc::clone(&settings)),
            endpoint: Arc::new(UnusedEndpoint),
            settings,
            registry,
            mcp: HashMap::new(),
            system_prompt: None,
            deltas: false,
            max_turns: None,
            limits: Default::default(),
            ceiling: None,
            cwd: std::env::temp_dir(),
            hooks: Some(hooks.clone()),
            transcript_id: "consent-test".to_owned(),
            host_session_id: None,
            store: Arc::new(MemoryTranscriptStore::new()),
            policy: ToolPolicy::default(),
            model: "m".to_owned(),
            messages: Vec::new(),
            initial_events: Vec::new(),
        });
        (core, hooks)
    }

    /// The verdict of the graph: the origin is not allowed yet, and a human must say so.
    fn consent() -> HookVerdict {
        HookVerdict::RequireConsent {
            subject: "https://example.com".to_owned(),
            reason: "origin not allowed: https://example.com".to_owned(),
        }
    }

    fn navigate() -> ToolCallChunk {
        ToolCallChunk {
            id: "c1".to_owned(),
            name: tools::exposed_name("browser", "navigate"),
            arguments: json!({"url": "https://example.com"}).to_string(),
        }
    }

    async fn run(core: &Core) -> (String, bool) {
        let signal = TurnSignal::new();
        let token = CancelToken::new();
        let mut context = Vec::new();
        let mut images = Vec::new();
        let mut task = None;
        let mut ends_turn = false;
        let (content, is_error, fatal) = execute(
            core,
            &signal,
            &navigate(),
            &token,
            Outputs {
                context: &mut context,
                images: &mut images,
                task: &mut task,
                ends_turn: &mut ends_turn,
            },
        )
        .await;
        assert!(fatal.is_none());
        (content, is_error)
    }

    /// (b) Nobody can be asked: the call is REFUSED with the hook's reason, and no
    /// `PermissionAsk` is emitted — nothing waits for an answer that would never come.
    #[tokio::test]
    async fn a_consent_nobody_can_be_asked_about_refuses_the_call_without_asking_anyone() {
        let (core, hooks) = core(false, consent());
        let (content, is_error) = run(&core).await;
        assert_eq!(
            (content.as_str(), is_error),
            ("origin not allowed: https://example.com", true),
            "the hook's reason is what the model reads"
        );
        assert_eq!(*hooks.calls.lock().unwrap(), 1, "the hook did speak");
        let state = core.lock();
        assert!(
            state.pending.is_empty(),
            "no permission request was left waiting for an answer"
        );
        assert!(
            !state
                .out_of_band
                .iter()
                .any(|event| matches!(event, crate::agent::AgentEvent::PermissionAsk { .. })),
            "{:?}",
            state.out_of_band
        );
    }
}
