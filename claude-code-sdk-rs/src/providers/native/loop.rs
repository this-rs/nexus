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
//! | `max_turns` / `max_tool_iterations` reached | `done max_turns` |
//! | token or USD budget spent | `done budget_exceeded` |
//! | `interrupt` | `done interrupted` |
//! | endpoint failure (classified), turn timeout | `done { is_error, error }`: usage and cost are kept |
//! | an MCP server process died | `error { process_exited }`: the session is dead |
//! | `close` | `error { closed }` |
//!
//! A turn that fails or is closed leaves the transcript as it was before the
//! turn (nothing half-done is committed); a turn that completes, hits a limit or
//! is interrupted commits its messages.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::future::join_all;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::cancel::CancelToken;
use super::compaction::{
    apply, count_images, estimate_tokens, should_compact, split_point, summarise, summary_request,
};
use super::mcp::{IMAGE_NOT_SHOWN, McpError};
use super::session::{Core, PendingAsk, StopCause, TurnSignal};
use super::tools::{ToolEntry, ToolRegistry};
use crate::agent::{
    AgentEvent, CompactionInfo, CompactionPhase, CompactionTrigger, Cost, CostBasis, DeltaKind,
    HookVerdict, InputBlock, ModelUsage, PermissionDecision, PolicyDecision, ProviderError,
    StopReason, ToolCallInfo, ToolCategory, ToolOutput, ToolResultInfo, TurnContext, TurnInput,
    Usage,
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
    let user = user_message(&input);
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

/// The user message of a turn: the text blocks joined, then the images (sent
/// as `image_url` parts, kept whole in the transcript).
fn user_message(input: &TurnInput) -> ChatMessage {
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
    ChatMessage::user_with_images(input.joined_text(), images)
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
        Outcome::Stop(..) => true,
        Outcome::Interrupted => signal.cause() == Some(StopCause::Interrupted),
        Outcome::Failed(_) | Outcome::Fatal(_) => false,
    };
    if commit {
        core.lock().messages = messages.clone();
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
    let state = core.lock();
    core.limits
        .max_tokens
        .is_some_and(|max| state.tokens_spent >= max)
        || core
            .limits
            .max_cost_usd
            .is_some_and(|max| state.usd_spent >= max)
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
        if let Err(error) = maybe_compact(core, signal, model, messages, acc).await {
            return Outcome::Failed(error);
        }
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
        if let Some(fatal) = ran.into_iter().find_map(|run| run.fatal) {
            core.mark_dead(fatal.clone());
            return Outcome::Fatal(fatal);
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
            state.tokens_spent += usage.input_tokens.unwrap_or(0)
                + usage.cache_read_tokens.unwrap_or(0)
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

async fn model_step(
    core: &Core,
    signal: &TurnSignal,
    model: &str,
    request: CompletionRequest,
    acc: &mut Acc,
) -> Result<Step, ProviderError> {
    let started = Instant::now();
    let prompt_estimate = estimate_tokens(&request.messages);
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

async fn maybe_compact(
    core: &Core,
    signal: &TurnSignal,
    model: &str,
    messages: &mut Vec<ChatMessage>,
    acc: &mut Acc,
) -> Result<(), ProviderError> {
    let config = core.settings.compaction;
    // The window of the model now active, not the snapshot's: a smaller model
    // chosen for this turn compacts where the opening one would not.
    let window = core.lock().active_window;
    let system = core
        .system_prompt
        .as_deref()
        .map_or(0, |text| text.len() as u64 / 4);
    let estimate = estimate_tokens(messages) + system;
    let reported = core.lock().last_prompt_tokens.unwrap_or(0);
    let size = estimate.max(reported);
    if !should_compact(&config, window, size) {
        return Ok(());
    }
    let Some(split) = split_point(messages, config.keep_recent) else {
        return Ok(());
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
        summary = summarise(core.endpoint.as_ref(), request) => summary?,
        () = signal.token.cancelled() => return Ok(()),
    };
    acc.api += started.elapsed();
    let (text, usage) = summary;
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
    Ok(())
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
    let (content, is_error, fatal) =
        execute(core, signal, call, &token, &mut context, &mut images).await;
    core.end_tool(&call.id);
    core.emit(AgentEvent::ToolResult {
        id: call.id.clone(),
        output: Some(ToolOutput::Text(content.clone())),
        is_error,
        seq: None,
        parent: None,
    });
    Run {
        content,
        context,
        images,
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

async fn ask(
    core: &Core,
    signal: &TurnSignal,
    entry: &ToolEntry,
    call: &ToolCallChunk,
    input: &Value,
    token: &CancelToken,
) -> Asked {
    let request_id = format!("perm_{}", uuid::Uuid::new_v4().simple());
    let (answer, answered) = oneshot::channel();
    core.lock().pending.insert(
        request_id.clone(),
        PendingAsk {
            tool: entry.name.clone(),
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

/// Runs one call: `(content for the model, is_error, fatal)`.
async fn execute(
    core: &Core,
    signal: &TurnSignal,
    call: &ToolCallChunk,
    token: &CancelToken,
    context: &mut Vec<String>,
    images: &mut Vec<ImagePart>,
) -> (String, bool, Option<ProviderError>) {
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
            _ => {},
        }
        if signal.token.is_cancelled() || token.is_cancelled() {
            return (stopped_message(signal, token), true, None);
        }
    }
    match core.decide(entry, &input) {
        PolicyDecision::Allow => {},
        PolicyDecision::Ask if core.capabilities.interactive_permissions => {
            match ask(core, signal, entry, call, &input, token).await {
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
    if let Some(hooks) = &core.hooks {
        let info = ToolResultInfo {
            call: ToolCallInfo {
                id: Some(call.id.clone()),
                name: call.name.clone(),
                canonical: entry.canonical.clone(),
                category: entry.category,
                input: ran_input,
            },
            output: Value::String(content.clone()),
            is_error,
        };
        if let Some(Some(text)) =
            hook_call(core, signal, "after_tool", hooks.after_tool(&info)).await
        {
            context.push(text);
        }
    }
    (content, is_error, fatal)
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
