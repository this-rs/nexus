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
    AgentEvent, CompactOutcome, CompactionInfo, CompactionPhase, CompactionTrigger, Cost,
    CostBasis, DeltaKind, HookReply, HookVerdict, InputBlock, ModelUsage, NotificationInfo,
    PermissionDecision, PolicyDecision, PromptInfo, ProviderError, QuestionReply,
    STOP_HOOK_BLOCK_CAP, SessionStartInfo, StopInfo, StopReason, SubagentStopInfo, ToolCallInfo,
    ToolCategory, ToolOutput, ToolResultInfo, TurnContext, TurnInput, Usage, prompt_blocked_text,
    stop_hook_feedback,
};
use crate::model::{
    ChatMessage, CompletionChunk, CompletionRequest, FinishReason, ImagePart, Role, ToolCallChunk,
    WireEcho, forget_echoed_reasoning,
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
    // Claude Code's `SessionStart` then `UserPromptSubmit` (contract v7), before anything of
    // the turn runs. A blocked prompt ends the turn here: no request, nothing committed.
    session_start(&core, &signal).await;
    let prompt = input.joined_text();
    let submitted = user_prompt_submit(&core, &signal, &prompt).await;
    if let Some(reason) = submitted.block_reason() {
        if let Some(timer) = timer {
            timer.abort();
        }
        core.emit(AgentEvent::ProviderNotice {
            kind: "prompt_blocked".to_owned(),
            data: json!({ "provider": "native", "hook": "user_prompt_submit", "reason": reason }),
        });
        let model = core.lock().model.clone();
        let text = prompt_blocked_text(reason, &prompt);
        let terminal = terminal_event(
            &core,
            &signal,
            &model,
            Outcome::Stop(StopKind::Completed, Some(text)),
            &acc,
        );
        core.emit(terminal);
        return;
    }
    // Read, not taken: a turn that is not committed leaves them for the next one.
    let (note, session_context) = {
        let state = core.lock();
        (state.pending_note.clone(), state.session_context.clone())
    };
    let user = user_message(
        &input,
        [note.as_deref(), session_context.as_deref()],
        submitted.context_text(),
    );
    before_turn(&core, &signal, prompt.len()).await;
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
/// as `image_url` parts, kept whole in the transcript). What comes `before` (a
/// pending note, the `session_start` hook's context) opens it and what a
/// `user_prompt_submit` hook added (`after`) closes it, each in its own
/// paragraph: they are part of the new message, so the prefix the endpoint has
/// cached (system prompt, earlier messages) does not change.
fn user_message(input: &TurnInput, before: [Option<&str>; 2], after: Option<&str>) -> ChatMessage {
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
    let joined = input.joined_text();
    let text = before
        .into_iter()
        .flatten()
        .chain(std::iter::once(joined.as_str()))
        .chain(after)
        .collect::<Vec<_>>()
        .join("\n\n");
    ChatMessage::user_with_images(text, images)
}

/// Asks the `session_start` hook once per session, before its first turn (Claude Code's
/// `SessionStart`): what it adds for the model waits in `State::session_context` until a turn
/// commits it; its `system_message` is shown to the user.
async fn session_start(core: &Core, signal: &TurnSignal) {
    let Some(hooks) = &core.hooks else {
        return;
    };
    {
        let mut state = core.lock();
        if state.session_start_asked {
            return;
        }
        state.session_start_asked = true;
    }
    let source = if core.resumed { "resume" } else { "startup" };
    let info = SessionStartInfo::new(source);
    let Some(reply) = hook_call(core, signal, "session_start", hooks.session_start(&info)).await
    else {
        return;
    };
    hook_system_message(core, "session_start", &reply);
    if let Some(context) = reply.context_text() {
        core.lock().session_context = Some(context.to_owned());
    }
}

/// Asks the `user_prompt_submit` hook about the input of the turn (Claude Code's
/// `UserPromptSubmit`). No hook, or one that does not answer in time: nothing to add, nothing
/// blocked.
async fn user_prompt_submit(core: &Core, signal: &TurnSignal, prompt: &str) -> HookReply {
    let Some(hooks) = &core.hooks else {
        return HookReply::none();
    };
    let info = PromptInfo::new(prompt);
    let reply = hook_call(
        core,
        signal,
        "user_prompt_submit",
        hooks.user_prompt_submit(&info),
    )
    .await
    .unwrap_or_default();
    hook_system_message(core, "user_prompt_submit", &reply);
    reply
}

/// Asks the stop hook (`before_stop`, or `before_subagent_stop` for a sub-agent) whether the
/// turn may end after the model answered `last` without calling a tool. `Some(reason)`: it may
/// not, the model reads the reason and goes on. `blocks` counts the blocks of this turn; the
/// one that reaches [`STOP_HOOK_BLOCK_CAP`] is overridden, as the Claude Code CLI does.
async fn stop_hook(
    core: &Core,
    signal: &TurnSignal,
    last: &str,
    blocks: &mut u32,
) -> Option<String> {
    let hooks = core.hooks.as_ref()?;
    let active = *blocks > 0;
    let last = (!last.is_empty()).then(|| last.to_owned());
    let (name, reply) = if core.is_subagent {
        let info = SubagentStopInfo::new(active, last);
        let name = "before_subagent_stop";
        (
            name,
            hook_call(core, signal, name, hooks.before_subagent_stop(&info)).await,
        )
    } else {
        let info = StopInfo::new(active, last);
        let name = "before_stop";
        (
            name,
            hook_call(core, signal, name, hooks.before_stop(&info)).await,
        )
    };
    let reply = reply?;
    hook_system_message(core, name, &reply);
    let reason = reply.block_reason()?.to_owned();
    *blocks += 1;
    if *blocks >= STOP_HOOK_BLOCK_CAP {
        core.emit(AgentEvent::ProviderNotice {
            kind: "stop_hook_block_cap".to_owned(),
            data: json!({ "provider": "native", "hook": name, "cap": STOP_HOOK_BLOCK_CAP }),
        });
        return None;
    }
    Some(reason)
}

/// Shows a hook's `system_message` to the user (Claude Code: "<Event> says: <message>").
fn hook_system_message(core: &Core, hook: &str, reply: &HookReply) {
    if let Some(message) = reply.system_text() {
        core.emit(AgentEvent::ProviderNotice {
            kind: "hook_system_message".to_owned(),
            data: json!({ "provider": "native", "hook": hook, "message": message }),
        });
    }
}

/// Tells the host, without waiting for it, that a tool call waits for the user's permission
/// (Claude Code's `Notification`, type `permission_prompt`). Its answer is not read.
fn notify_permission_prompt(core: &Core, tool: &str) {
    let Some(hooks) = &core.hooks else {
        return;
    };
    let hooks = Arc::clone(hooks);
    let info = NotificationInfo::permission_prompt(tool);
    let timeout = core.settings.hook_timeout;
    tokio::spawn(async move {
        let _ = tokio::time::timeout(timeout, hooks.notification(&info)).await;
    });
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
            // The user message just committed carries the note and the session start's
            // context, if there were any.
            state.pending_note = None;
            state.session_context = None;
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
        wire_echo: None,
    }
}

/// The whole answer of a step, with the echo the endpoint asked to get back verbatim.
/// Only for a step kept whole: a step whose calls were dropped or renamed is not what
/// the echo says, and goes without it.
fn answered(step: &mut Step, tool_calls: Vec<ToolCallChunk>) -> ChatMessage {
    let mut message = assistant(&step.text, &step.reasoning, tool_calls);
    if message.tool_calls == step.tool_calls {
        message.wire_echo = step.echo.take();
    }
    message
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
    // Stop-hook blocks of this turn (Claude Code's `stop_hook_active` and its cap).
    let mut stop_blocks = 0u32;
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
        let mut step = match model_step(core, signal, model, request, acc).await {
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
            messages.push(answered(&mut step, Vec::new()));
            let kind = match step.finish {
                Some(FinishReason::Length) => StopKind::MaxTokens,
                Some(FinishReason::ContentFilter) => StopKind::Refusal,
                _ => StopKind::Completed,
            };
            // Claude Code's `Stop` / `SubagentStop`: a block keeps the turn going, the
            // model reads the reason as the next user message (contract v7).
            if matches!(kind, StopKind::Completed)
                && let Some(reason) = stop_hook(core, signal, &step.text, &mut stop_blocks).await
            {
                messages.push(ChatMessage::user(stop_hook_feedback(&reason)));
                continue;
            }
            return Outcome::Stop(kind, Some(step.text));
        }
        if budget_spent(core) {
            // The tools would run past the budget: keep the words, drop the calls.
            if !step.text.is_empty() {
                messages.push(assistant(&step.text, &step.reasoning, Vec::new()));
            }
            return Outcome::Stop(StopKind::Budget, Some(step.text));
        }
        let calls = normalise_ids(step.tool_calls.clone());
        messages.push(answered(&mut step, calls.clone()));
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
    let (policy, vision, loaded) = {
        let state = core.lock();
        (
            state.policy.clone(),
            state.active_images,
            state.loaded_tools.clone(),
        )
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
    let strict = core.settings.strict_tool_exposure;
    // With no deferred configuration the list is exactly what it always was; with one, the
    // deferred definitions are withheld and the loaded ones follow the base, at the end (CTX2).
    request.tools = if core.settings.deferred_tools.is_empty() {
        core.registry.specs(&policy, strict)
    } else {
        core.registry
            .specs_deferred(&policy, strict, &core.settings.deferred_tools, &loaded)
    };
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
    /// The answer in the endpoint's own wire form, to send back verbatim.
    echo: Option<WireEcho>,
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
                // The whole prompt: what was read from the cache and what was written
                // to it count as much as the rest (Anthropic reports the three apart).
                state.last_prompt_tokens = Some(
                    usage.input_tokens.unwrap_or(0)
                        + usage.cache_read_tokens.unwrap_or(0)
                        + usage.cache_creation_tokens.unwrap_or(0),
                );
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
            CompletionChunk::Echo(echo) => step.echo = Some(echo),
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

/// Estimated tokens of the system prompt (four characters per token).
fn system_tokens(core: &Core) -> u64 {
    core.system_prompt
        .as_deref()
        .map_or(0, |text| text.len() as u64 / 4)
}

/// What the prompt costs now, and the window of the model now active: the larger of
/// the estimate and the last size the endpoint reported (its report is the truth when
/// it has one).
fn prompt_pressure(core: &Core, messages: &[ChatMessage]) -> (u64, Option<u64>) {
    // The window of the model now active, not the snapshot's: a smaller model chosen
    // for this turn compacts where the opening one would not.
    let window = core.lock().active_window;
    let estimate = estimate_tokens(messages) + system_tokens(core);
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
    // The history changed under the messages that echo reasoning: a thinking block
    // replayed after an edit before it is refused, so none is replayed any more.
    forget_echoed_reasoning(messages);
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
    // Failure is already said (`compaction_failed`) and leaves the history intact; a
    // cancelled summary has nothing to say.
    let _ = compact_history(
        core,
        signal,
        model,
        messages,
        Some(acc),
        Compaction {
            trigger: CompactionTrigger::Auto,
            size,
            system: system_tokens(core),
            split,
            instructions: None,
        },
    )
    .await;
}

/// What one compaction is asked to do.
struct Compaction<'a> {
    trigger: CompactionTrigger,
    /// Estimated size of the prompt now, in tokens (the system prompt included).
    size: u64,
    /// Estimated tokens of the system prompt (kept out of the history).
    system: u64,
    /// Index where the kept tail starts.
    split: usize,
    /// What the user asked the summary to keep in mind (manual compaction).
    instructions: Option<&'a str>,
}

/// How a compaction ended.
enum Compacted {
    /// The history was replaced: messages summarised, estimated size after.
    Done { summarised: usize, after: u64 },
    /// The summary could not be written; the history is intact.
    Failed(ProviderError),
    /// The turn (or the session) was stopped while the summary was written.
    Cancelled,
}

/// The one compaction, automatic or manual: same events, same summary, same hook.
///
/// `before_compaction` is asked with the trigger and the user's instructions, and its
/// `systemMessage` is **added** to them. `acc` is the turn's accounting when there is a
/// turn; a manual compaction has none and spends through the session counters only.
async fn compact_history(
    core: &Core,
    signal: &TurnSignal,
    model: &str,
    messages: &mut Vec<ChatMessage>,
    mut acc: Option<&mut Acc>,
    job: Compaction<'_>,
) -> Compacted {
    let Compaction {
        trigger,
        size,
        system,
        split,
        instructions: asked,
    } = job;
    core.emit(AgentEvent::Compaction {
        phase: CompactionPhase::Started,
        trigger: Some(trigger),
        pre_tokens: Some(size),
    });
    let from_hook = match &core.hooks {
        Some(hooks) => {
            let info = CompactionInfo {
                trigger: trigger.as_str().to_owned(),
                custom_instructions: asked.map(str::to_owned),
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
    let instructions = match (asked, from_hook) {
        (Some(asked), Some(hook)) => Some(format!("{asked}\n\n{hook}")),
        (Some(text), None) => Some(text.to_owned()),
        (None, hook) => hook,
    };
    let config = core.settings.compaction;
    let request = summary_request(model, &messages[..split], instructions.as_deref(), &config);
    let summary_prompt_estimate = estimate_tokens(&request.messages);
    let started = Instant::now();
    let summary = tokio::select! {
        summary = summarise(core.endpoint.as_ref(), request) => summary,
        () = signal.token.cancelled() => return Compacted::Cancelled,
    };
    let (text, usage) = match summary {
        Ok(summary) => {
            if let Some(acc) = acc.as_deref_mut() {
                acc.api += started.elapsed();
            }
            summary
        },
        Err(error) => {
            // Fail open: the history stays exactly as it was and the host is told what
            // went wrong, with the typed code and a message that already went through
            // `redact` (a credential the endpoint echoed never reaches an event).
            core.emit(AgentEvent::ProviderNotice {
                kind: "compaction_failed".to_owned(),
                data: json!({
                    "trigger": trigger.as_str(),
                    "code": error.kind(),
                    "message": error.to_string(),
                }),
            });
            return Compacted::Failed(error);
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
    if let Some(acc) = &mut acc {
        if let Some(usage) = &usage {
            acc.add_usage(usage);
        }
        // Neither the summary's prompt nor the old reports say how big the new history is.
        acc.prompt_tokens = None;
    }
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
    // The kept messages follow a summary now: their echoed reasoning is not valid any more.
    forget_echoed_reasoning(messages);
    core.lock().last_prompt_tokens = None;
    core.emit(AgentEvent::Compaction {
        phase: CompactionPhase::Completed,
        trigger: Some(trigger),
        pre_tokens: Some(size),
    });
    Compacted::Done {
        summarised: split,
        after: estimate_tokens(messages) + system,
    }
}

/// Manual compaction (`AgentSession::compact`): the same summary as the automatic one,
/// outside any threshold, on the committed history. The session is marked busy for its
/// whole length (a turn is refused), and the new history is committed and saved only
/// when the summary was written.
pub(crate) async fn compact_now(
    core: &Core,
    instructions: Option<String>,
) -> Result<CompactOutcome, ProviderError> {
    let signal = TurnSignal::new();
    {
        let mut state = core.lock();
        if state.closed {
            return Err(ProviderError::Closed);
        }
        if state.turn.is_some() || state.compacting {
            return Err(ProviderError::TurnInProgress);
        }
        state.compacting = true;
        // `interrupt` stops the summary like it stops a turn.
        state.signal = Some(Arc::clone(&signal));
    }
    let result = compact_now_inner(core, &signal, instructions).await;
    {
        let mut state = core.lock();
        state.compacting = false;
        state.signal = None;
    }
    result
}

async fn compact_now_inner(
    core: &Core,
    signal: &Arc<TurnSignal>,
    instructions: Option<String>,
) -> Result<CompactOutcome, ProviderError> {
    let (mut messages, model) = {
        let state = core.lock();
        (state.messages.clone(), state.model.clone())
    };
    let system = system_tokens(core);
    let (size, _) = prompt_pressure(core, &messages);
    let Some(split) = split_point(&messages, core.settings.compaction.keep_recent) else {
        return Ok(CompactOutcome {
            compacted: false,
            tokens_before: Some(size),
            tokens_after: None,
            messages_summarised: 0,
        });
    };
    let instructions = instructions.filter(|text| !text.trim().is_empty());
    let job = Compaction {
        trigger: CompactionTrigger::Manual,
        size,
        system,
        split,
        instructions: instructions.as_deref(),
    };
    match compact_history(core, signal, &model, &mut messages, None, job).await {
        Compacted::Done { summarised, after } => {
            core.lock().messages = messages.clone();
            if let Err(error) = core.store.save(&core.transcript_id, &messages) {
                tracing::warn!(
                    kind = error.kind(),
                    "the native transcript could not be saved"
                );
            }
            Ok(CompactOutcome {
                compacted: true,
                tokens_before: Some(size),
                tokens_after: Some(after),
                messages_summarised: summarised as u32,
            })
        },
        Compacted::Failed(error) => Err(error),
        Compacted::Cancelled => Err(ProviderError::invalid(
            "the compaction was interrupted: the history is as it was",
        )),
    }
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
    notify_permission_prompt(
        core,
        entry.canonical.as_deref().unwrap_or(entry.name.as_str()),
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
    // A deferred tool is never run silently (CTX2): until a `ToolSearch` call loads it, a
    // call to it is answered by an error that says how to load it.
    if super::tools::is_deferred(entry, &core.settings.deferred_tools)
        && !core
            .lock()
            .loaded_tools
            .iter()
            .any(|name| name == &entry.name)
    {
        return fail(&format!(
            "`{name}` is deferred and not loaded yet: call ToolSearch with query \
             \"select:{name}\" (or keywords) to load its definition, then call it again",
            name = entry.name
        ));
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
            BuiltinTool::ToolSearch => tool_search(core, &input),
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

/// `ToolSearch` (CTX2): finds deferred tools — `select:A,B` names them exactly, anything else
/// is a keyword search over names and descriptions (`max_results` best matches, 5 by default) —
/// loads what it found for the rest of the session, and answers with their full definitions.
/// Only tools the current policy exposes can be found; nothing found is an error result, so the
/// model sees it and rephrases.
fn tool_search(core: &Core, input: &Value) -> (String, bool) {
    let Some(query) = input
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|query| !query.is_empty())
    else {
        return (
            "ToolSearch needs `query`: \"select:<name>,<name>\" for exact tools, or keywords \
             to search"
                .to_owned(),
            true,
        );
    };
    let policy = core.lock().policy.clone();
    let deferred = &core.settings.deferred_tools;
    let candidates: Vec<&ToolEntry> = core
        .registry
        .exposed(&policy, core.settings.strict_tool_exposure)
        .into_iter()
        .filter(|entry| super::tools::is_deferred(entry, deferred))
        .collect();
    let mut found: Vec<&ToolEntry> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    if let Some(names) = query.strip_prefix("select:") {
        for name in names
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            match candidates.iter().find(|entry| entry.name == name) {
                Some(entry) => found.push(entry),
                None => missing.push(name),
            }
        }
    } else {
        let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let mut scored: Vec<(usize, &ToolEntry)> = candidates
            .iter()
            .filter_map(|entry| {
                let haystack = format!("{} {}", entry.name, entry.description).to_lowercase();
                let score = terms.iter().filter(|term| haystack.contains(*term)).count();
                (score > 0).then_some((score, *entry))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        let max = input
            .get("max_results")
            .and_then(Value::as_f64)
            .map_or(5, |n| (n.max(1.0)) as usize);
        found = scored
            .into_iter()
            .take(max)
            .map(|(_, entry)| entry)
            .collect();
    }
    if found.is_empty() {
        return (
            format!(
                "no deferred tool matches `{query}`: nothing was loaded. Use \
                 \"select:<exact name>\" or other keywords"
            ),
            true,
        );
    }
    {
        // Loaded for the rest of the session, in load order; the list never shrinks (CTX0).
        let mut state = core.lock();
        for entry in &found {
            if !state.loaded_tools.iter().any(|name| name == &entry.name) {
                state.loaded_tools.push(entry.name.clone());
            }
        }
    }
    let mut out = String::from("<functions>\n");
    for entry in &found {
        let definition = json!({
            "description": entry.description,
            "name": entry.name,
            "parameters": entry.schema,
        });
        out.push_str(&format!("<function>{definition}</function>\n"));
    }
    out.push_str("</functions>\n");
    out.push_str(&format!(
        "{} tool(s) loaded: callable from now on, for the rest of the session.",
        found.len()
    ));
    if !missing.is_empty() {
        out.push_str(&format!(
            "\nNot found (no deferred tool of that name): {}.",
            missing.join(", ")
        ));
    }
    (out, false)
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
            is_subagent: false,
            resumed: false,
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
