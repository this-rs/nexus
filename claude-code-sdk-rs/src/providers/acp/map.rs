//! Projection of ACP messages onto [`AgentEvent`]s: pure state machine, no I/O
//! (contract §4, §14).
//!
//! # `session/update` table
//!
//! | ACP | `AgentEvent` |
//! |---|---|
//! | `agent_message_chunk` | `delta { text }` (only when the session asked for deltas); the chunks are accumulated and flushed as one `text` when something else arrives or the turn ends |
//! | `agent_thought_chunk` | `delta { thinking }`, flushed as one `thinking` (only when `thinking` is declared; otherwise dropped, `provider_notice { thinking_not_declared }` once) |
//! | `user_message_chunk` | nothing (history replay of `session/load`) |
//! | `tool_call`, `tool_call_update` | `tool_call` (`input_complete: false` until a `rawInput` is known, then complete), `tool_result` when `status` is `completed` / `failed` (`is_error` for `failed`) |
//! | `plan` | `provider_notice { plan, entries }` (no neutral plan event exists in the contract) |
//! | `available_commands_update` | `provider_notice { available_commands, commands }` |
//! | `current_mode_update` | `policy_mode_changed` when the mode id maps to a neutral mode, else `provider_notice { agent_mode_changed }` |
//! | `usage_update` (unstable) | nothing now: `used` becomes `done.usage.context_tokens` |
//! | anything else | dropped |
//!
//! `session/prompt` result → `done`:
//!
//! | `stopReason` | `done` |
//! |---|---|
//! | `end_turn` | `completed` |
//! | `max_tokens` | `max_tokens` |
//! | `max_turn_requests` | `max_turns` |
//! | `refusal` | `refusal`, `is_error: false` (as the native harness does) |
//! | `cancelled` | `interrupted` |
//! | anything else | `error`, `is_error`, `error: protocol` |
//! | a JSON-RPC error | `error`, `is_error`, `error: <classified>` ([`classify_rpc_error`]) |
//!
//! # Tool names
//!
//! ACP names a call by its `title` and gives it a `kind`. `category` follows `kind`
//! (`read`→read, `edit`/`delete`/`move`→edit, `execute`→command, `search`→search,
//! `fetch`→web, `think`/`switch_mode`/`other`→other). `canonical` is
//! `mcp__<server>__<tool>` (category `mcp`) when the title is `<server>_<tool>` for a
//! server of the session (opencode's spelling), or already `mcp__…`; otherwise
//! `Read`/`Edit`/`Bash`/`Grep`/`WebFetch` by `kind`, and `None` for `other`.
//!
//! # Usage and cost
//!
//! The stable protocol has no token usage. When the agent gives one (`usage` of the
//! prompt result, **unstable, NOT VERIFIED**) it is used as reported. The cost follows
//! the instance configuration and is never `reported`: `unknown` → no amount; `free` →
//! 0; `priced` → the price table, `None` while the usage or the price is unknown.

use std::collections::HashMap;
use std::time::Instant;

use serde_json::{Value, json};

use super::wire::{
    ChunkUpdate, ModeInfo, PermissionOption, PermissionRequest, PromptResult, RpcError, Update,
};
use crate::agent::{
    AgentEvent, Cost, CostBasis, DeltaKind, PermissionDecision, PermissionScope, PolicyMode,
    ProviderError, StopReason, ToolCategory, ToolOutput, Usage, redact,
};
use crate::model::PriceTable;

/// What the mapper needs from the instance and the session.
#[derive(Debug, Clone)]
pub struct MapConfig {
    /// The session asked for streaming deltas.
    pub deltas: bool,
    /// Where `done.cost` comes from.
    pub cost_basis: CostBasis,
    /// Prices, for `priced`.
    pub prices: PriceTable,
    /// Command a human runs to log in, put on an `auth_required` error.
    pub login_hint: Option<String>,
    /// Names of the MCP servers of the session (for `canonical`).
    pub mcp_servers: Vec<String>,
    /// `thinking` is declared by the capabilities of the session.
    pub thinking: bool,
}

#[derive(Debug, Clone)]
struct Call {
    name: String,
    kind: Option<String>,
    category: ToolCategory,
    canonical: Option<String>,
    input: Option<Value>,
    announced: bool,
    complete: bool,
    finished: bool,
}

/// Mapping state of one session.
#[derive(Debug)]
pub struct MapState {
    config: MapConfig,
    model: Option<String>,
    text: String,
    thinking: String,
    calls: HashMap<String, Call>,
    used: Option<u64>,
    turn_started: Option<Instant>,
    last_text: Option<String>,
    thinking_notice: bool,
    thinking_seen: bool,
}

impl MapState {
    /// A fresh state.
    pub fn new(config: MapConfig) -> Self {
        Self {
            config,
            model: None,
            text: String::new(),
            thinking: String::new(),
            calls: HashMap::new(),
            used: None,
            turn_started: None,
            last_text: None,
            thinking_notice: false,
            thinking_seen: false,
        }
    }

    /// Model in effect, for `done.model` and the price.
    pub fn set_model(&mut self, model: Option<String>) {
        self.model = model;
    }

    /// Whether a thought chunk was received (the provider learns it).
    pub fn thinking_seen(&self) -> bool {
        self.thinking_seen
    }

    /// A turn starts.
    pub fn begin_turn(&mut self) {
        self.text.clear();
        self.thinking.clear();
        self.used = None;
        self.last_text = None;
        self.turn_started = Some(Instant::now());
    }

    fn flush_text(&mut self, events: &mut Vec<AgentEvent>) {
        if !self.text.is_empty() {
            let text = std::mem::take(&mut self.text);
            self.last_text = Some(text.clone());
            events.push(AgentEvent::Text {
                text,
                seq: None,
                parent: None,
            });
        }
    }

    fn flush_thinking(&mut self, events: &mut Vec<AgentEvent>) {
        if !self.thinking.is_empty() {
            events.push(AgentEvent::Thinking {
                text: std::mem::take(&mut self.thinking),
                signature: None,
                seq: None,
                parent: None,
            });
        }
    }

    fn flush_messages(&mut self, events: &mut Vec<AgentEvent>) {
        self.flush_thinking(events);
        self.flush_text(events);
    }

    /// Maps one `session/update`.
    pub fn map_update(&mut self, update: Update) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        match update {
            Update::AgentMessageChunk(chunk) => {
                if let Some(text) = chunk_text(&chunk) {
                    self.flush_thinking(&mut events);
                    self.text.push_str(&text);
                    if self.config.deltas {
                        events.push(AgentEvent::Delta {
                            kind: DeltaKind::Text,
                            text,
                            index: None,
                            tool_call_id: None,
                            parent: None,
                        });
                    }
                }
            },
            Update::AgentThoughtChunk(chunk) => {
                if let Some(text) = chunk_text(&chunk) {
                    self.thinking_seen = true;
                    if self.config.thinking {
                        self.flush_text(&mut events);
                        self.thinking.push_str(&text);
                        if self.config.deltas {
                            events.push(AgentEvent::Delta {
                                kind: DeltaKind::Thinking,
                                text,
                                index: None,
                                tool_call_id: None,
                                parent: None,
                            });
                        }
                    } else if !self.thinking_notice {
                        self.thinking_notice = true;
                        events.push(AgentEvent::ProviderNotice {
                            kind: "thinking_not_declared".to_owned(),
                            data: json!({ "detail": "the agent sent reasoning the capabilities do not declare; dropped" }),
                        });
                    }
                }
            },
            Update::Usage(usage) => self.used = usage.used.or(self.used),
            Update::UserMessageChunk(_) | Update::Unknown(_) => {},
            Update::ToolCall(call) | Update::ToolCallUpdate(call) => {
                self.flush_messages(&mut events);
                self.on_call(&call, &mut events);
            },
            Update::Plan(plan) => {
                self.flush_messages(&mut events);
                events.push(AgentEvent::ProviderNotice {
                    kind: "plan".to_owned(),
                    data: json!({ "entries": plan.entries }),
                });
            },
            Update::AvailableCommands(commands) => {
                self.flush_messages(&mut events);
                events.push(AgentEvent::ProviderNotice {
                    kind: "available_commands".to_owned(),
                    data: json!({ "commands": commands.available_commands }),
                });
            },
            Update::CurrentMode(mode) => {
                self.flush_messages(&mut events);
                events.push(match neutral_of_mode(&mode.current_mode_id) {
                    Some(neutral) => AgentEvent::PolicyModeChanged {
                        mode: neutral,
                        native_mode: Some(mode.current_mode_id),
                    },
                    None => AgentEvent::ProviderNotice {
                        kind: "agent_mode_changed".to_owned(),
                        data: json!({ "mode": mode.current_mode_id }),
                    },
                });
            },
        }
        events
    }

    fn on_call(&mut self, update: &super::wire::ToolCallUpdateIn, events: &mut Vec<AgentEvent>) {
        let mcp = &self.config.mcp_servers;
        let call = self
            .calls
            .entry(update.tool_call_id.clone())
            .or_insert_with(|| Call {
                name: String::new(),
                kind: None,
                category: ToolCategory::Other,
                canonical: None,
                input: None,
                announced: false,
                complete: false,
                finished: false,
            });
        if let Some(title) = update.title.as_ref().filter(|title| !title.is_empty()) {
            call.name.clone_from(title);
        }
        if update.kind.is_some() {
            call.kind.clone_from(&update.kind);
        }
        if call.name.is_empty() {
            call.name = call.kind.clone().unwrap_or_else(|| "tool".to_owned());
        }
        let (category, canonical) = describe_tool(&call.name, call.kind.as_deref(), mcp);
        call.category = category;
        call.canonical = canonical;
        if let Some(input) = &update.raw_input {
            call.input = Some(input.clone());
        }
        let id = update.tool_call_id.clone();
        let event = |call: &Call, complete: bool| AgentEvent::ToolCall {
            id: id.clone(),
            name: call.name.clone(),
            input: call.input.clone().unwrap_or_else(|| json!({})),
            category: call.category,
            canonical: call.canonical.clone(),
            input_complete: complete,
            seq: None,
            parent: None,
        };
        let finishing = matches!(update.status.as_deref(), Some("completed" | "failed"));
        if !call.announced {
            call.announced = true;
            call.complete = call.input.is_some() || finishing;
            events.push(event(call, call.complete));
        } else if !call.complete && (call.input.is_some() || finishing) {
            call.complete = true;
            events.push(event(call, true));
        }
        if finishing && !call.finished {
            call.finished = true;
            events.push(AgentEvent::ToolResult {
                id,
                output: tool_output(update),
                is_error: update.status.as_deref() == Some("failed"),
                seq: None,
                parent: None,
            });
        }
    }

    /// Builds the permission request of an agent request, with the events to emit
    /// first (a `tool_call` when the call was not announced). The last event is the
    /// `permission_ask`, whose `request_id` is empty and filled in by the session.
    pub fn ask_for(&mut self, request: &PermissionRequest) -> Ask {
        let mut events = Vec::new();
        self.flush_messages(&mut events);
        let mut call = request.tool_call.clone();
        // A request is not a result: a status it carries does not finish the call.
        if matches!(call.status.as_deref(), Some("completed" | "failed")) {
            call.status = None;
        }
        self.on_call(&call, &mut events);
        let meta = &self.calls[&call.tool_call_id];
        let input = meta.input.clone().unwrap_or_else(|| json!({}));
        let arg = policy_argument(&input, &meta.name);
        let mut scopes = Vec::new();
        if request.options.iter().any(|o| o.kind == "allow_once") {
            scopes.push(PermissionScope::Once);
        }
        if request.options.iter().any(|o| o.kind == "allow_always") {
            scopes.push(PermissionScope::Always);
        }
        let tool = meta.canonical.clone().unwrap_or_else(|| meta.name.clone());
        events.push(AgentEvent::PermissionAsk {
            request_id: String::new(),
            tool_name: meta.name.clone(),
            input,
            category: meta.category,
            canonical: meta.canonical.clone(),
            tool_call_id: Some(call.tool_call_id.clone()),
            scopes: scopes.clone(),
            parent: None,
        });
        Ask {
            events,
            options: request.options.clone(),
            scopes,
            tool,
            arg,
            category: meta.category,
        }
    }

    /// Ends the turn: flushes what is pending, completes the calls whose input never
    /// arrived, and builds `done`.
    pub fn finish(&mut self, outcome: Result<PromptResult, RpcError>) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        self.flush_messages(&mut events);
        let mut pending: Vec<(&String, &mut Call)> = self
            .calls
            .iter_mut()
            .filter(|(_, call)| call.announced && !call.complete)
            .collect();
        pending.sort_by(|a, b| a.0.cmp(b.0));
        for (id, call) in pending {
            call.complete = true;
            events.push(AgentEvent::ToolCall {
                id: id.clone(),
                name: call.name.clone(),
                input: call.input.clone().unwrap_or_else(|| json!({})),
                category: call.category,
                canonical: call.canonical.clone(),
                input_complete: true,
                seq: None,
                parent: None,
            });
        }
        let duration_ms = self
            .turn_started
            .map(|start| u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let (stop_reason, is_error, subtype, usage, error, result_text) = match outcome {
            Ok(result) => {
                let (stop_reason, is_error, error) = stop_of(&result.stop_reason);
                let usage = self.usage(result.usage.as_ref());
                (
                    stop_reason,
                    is_error,
                    Some(result.stop_reason),
                    usage,
                    error,
                    self.last_text.clone(),
                )
            },
            Err(rpc) => {
                let error =
                    with_login_hint(classify_rpc_error(&rpc), self.config.login_hint.as_deref());
                (
                    StopReason::Error,
                    true,
                    Some("error".to_owned()),
                    self.usage(None),
                    Some(error),
                    Some(redact(&rpc.message)),
                )
            },
        };
        let cost = self.cost(&usage);
        events.push(AgentEvent::Done {
            stop_reason,
            subtype,
            is_error,
            result_text,
            usage,
            cost,
            duration_ms,
            duration_api_ms: None,
            num_turns: 0,
            model: self.model.clone(),
            provider_session_id: None,
            structured_output: None,
            error,
        });
        events
    }

    fn usage(&self, reported: Option<&super::wire::PromptUsage>) -> Usage {
        let mut usage = Usage {
            context_tokens: self.used,
            ..Usage::default()
        };
        if let Some(reported) = reported {
            usage.input_tokens = reported.input_tokens;
            usage.output_tokens = reported.output_tokens;
            usage.cache_read_tokens = reported.cached_read_tokens;
            usage.reasoning_tokens = reported.thought_tokens;
        }
        usage
    }

    fn cost(&self, usage: &Usage) -> Cost {
        match self.config.cost_basis {
            CostBasis::Free => Cost {
                usd: Some(0.0),
                basis: CostBasis::Free,
            },
            CostBasis::Priced => Cost {
                usd: self
                    .model
                    .as_deref()
                    .and_then(|model| self.config.prices.cost_usd(model, usage)),
                basis: CostBasis::Priced,
            },
            _ => Cost::default(),
        }
    }
}

fn chunk_text(chunk: &ChunkUpdate) -> Option<String> {
    let kind = chunk.content.r#type.as_deref().unwrap_or("text");
    if kind != "text" {
        return None;
    }
    chunk.content.text.clone().filter(|text| !text.is_empty())
}

fn tool_output(update: &super::wire::ToolCallUpdateIn) -> Option<ToolOutput> {
    let mut parts: Vec<String> = Vec::new();
    for item in update.content.iter().flatten() {
        match item.get("type").and_then(Value::as_str) {
            Some("content") => {
                if let Some(text) = item
                    .get("content")
                    .and_then(|c| c.get("text"))
                    .and_then(Value::as_str)
                {
                    parts.push(text.to_owned());
                }
            },
            Some("diff") => {
                if let Some(path) = item.get("path").and_then(Value::as_str) {
                    parts.push(format!("diff of {path}"));
                }
            },
            _ => {},
        }
    }
    if !parts.is_empty() {
        return Some(ToolOutput::Text(parts.join("\n")));
    }
    match &update.raw_output {
        Some(Value::String(text)) => Some(ToolOutput::Text(text.clone())),
        Some(Value::Null) | None => None,
        Some(other) => Some(ToolOutput::Text(other.to_string())),
    }
}

/// `category` and `canonical` of a call (see the module documentation).
pub fn describe_tool(
    name: &str,
    kind: Option<&str>,
    mcp_servers: &[String],
) -> (ToolCategory, Option<String>) {
    if name.starts_with("mcp__") {
        return (ToolCategory::Mcp, Some(name.to_owned()));
    }
    if !name.is_empty() && !name.chars().any(char::is_whitespace) {
        let best = mcp_servers
            .iter()
            .filter(|server| {
                name.strip_prefix(server.as_str())
                    .and_then(|rest| rest.strip_prefix('_'))
                    .is_some_and(|rest| !rest.is_empty())
            })
            .max_by_key(|server| server.len());
        if let Some(server) = best {
            let tool = &name[server.len() + 1..];
            return (ToolCategory::Mcp, Some(format!("mcp__{server}__{tool}")));
        }
    }
    match kind {
        Some("read") => (ToolCategory::Read, Some("Read".to_owned())),
        Some("edit" | "delete" | "move") => (ToolCategory::Edit, Some("Edit".to_owned())),
        Some("execute") => (ToolCategory::Command, Some("Bash".to_owned())),
        Some("search") => (ToolCategory::Search, Some("Grep".to_owned())),
        Some("fetch") => (ToolCategory::Web, Some("WebFetch".to_owned())),
        _ => (ToolCategory::Other, None),
    }
}

fn policy_argument(input: &Value, fallback: &str) -> String {
    for key in ["command", "path", "filePath", "file_path", "url", "pattern"] {
        if let Some(text) = input.get(key).and_then(Value::as_str) {
            return text.to_owned();
        }
    }
    fallback.to_owned()
}

/// A permission request ready to be shown, and what is needed to answer it.
#[derive(Debug, Clone)]
pub struct Ask {
    /// Events to emit, in order; the last is the `permission_ask`.
    pub events: Vec<AgentEvent>,
    /// The options the agent offered.
    pub options: Vec<PermissionOption>,
    /// Scopes this request offers (those with an `allow_*` option).
    pub scopes: Vec<PermissionScope>,
    /// Tool name for the local policy (`decide`).
    pub tool: String,
    /// Argument for the local policy.
    pub arg: String,
    /// Category for the local policy.
    pub category: ToolCategory,
}

fn option_of<'a>(options: &'a [PermissionOption], kind: &str) -> Option<&'a PermissionOption> {
    options.iter().find(|option| option.kind == kind)
}

/// The wire answer to a permission decision, and whether the turn is to be cancelled
/// after it.
///
/// `Allow` with `Once` / `Always` selects the `allow_once` / `allow_always` option; a
/// scope the request does not offer (and `Session`, which ACP has no option for) is
/// `Unsupported { permission_scope }`; an `updated_input` is
/// `Unsupported { permission_updated_input }`. `Deny` selects `reject_once`, else
/// `reject_always`, else answers `cancelled`.
pub fn answer_for(
    options: &[PermissionOption],
    decision: &PermissionDecision,
) -> Result<(Value, bool), ProviderError> {
    match decision {
        PermissionDecision::Allow {
            scope,
            updated_input,
        } => {
            if updated_input.is_some() {
                return Err(ProviderError::unsupported("permission_updated_input"));
            }
            let kind = match scope {
                PermissionScope::Once => "allow_once",
                PermissionScope::Always => "allow_always",
                _ => return Err(ProviderError::unsupported("permission_scope")),
            };
            let option = option_of(options, kind)
                .ok_or_else(|| ProviderError::unsupported("permission_scope"))?;
            Ok((super::wire::permission_selected(&option.option_id), false))
        },
        PermissionDecision::Deny { interrupt, .. } => {
            let answer = option_of(options, "reject_once")
                .or_else(|| option_of(options, "reject_always"))
                .map_or_else(super::wire::permission_cancelled, |option| {
                    super::wire::permission_selected(&option.option_id)
                });
            Ok((answer, *interrupt))
        },
    }
}

/// The answer of the local policy: `allow` selects an `allow_once` option (else
/// `allow_always`), `None` when the agent offered neither; a denial is
/// [`answer_for`]'s `Deny`.
pub fn auto_answer(options: &[PermissionOption], allow: bool) -> Option<Value> {
    if allow {
        option_of(options, "allow_once")
            .or_else(|| option_of(options, "allow_always"))
            .map(|option| super::wire::permission_selected(&option.option_id))
    } else {
        answer_for(options, &PermissionDecision::deny())
            .ok()
            .map(|(answer, _)| answer)
    }
}

/// `stopReason` → neutral stop reason, `is_error`, and the error behind it.
pub fn stop_of(reason: &str) -> (StopReason, bool, Option<ProviderError>) {
    match reason {
        "end_turn" => (StopReason::Completed, false, None),
        "max_tokens" => (StopReason::MaxTokens, false, None),
        "max_turn_requests" => (StopReason::MaxTurns, false, None),
        "refusal" => (StopReason::Refusal, false, None),
        "cancelled" => (StopReason::Interrupted, false, None),
        other => (
            StopReason::Error,
            true,
            Some(ProviderError::protocol(format!(
                "unknown stopReason `{}`",
                redact(other)
            ))),
        ),
    }
}

/// A JSON-RPC error → [`ProviderError`] (contract §7). ACP defines `-32000` as
/// `auth_required`; the rest is read from the message, which never carries more than
/// what [`redact`] lets through.
///
/// | error | `ProviderError` |
/// |---|---|
/// | `-32000` | `auth_required` |
/// | `-32602` | `invalid_request` |
/// | message with `rate limit`, `429`, `too many requests` | `rate_limited` |
/// | `overloaded`, `503`, `529` | `overloaded` |
/// | `context` + `length` / `window` / `too long` | `context_too_small` |
/// | `unauthorized`, `401`, `invalid api key` | `unauthorized` |
/// | `unreachable`, `connection refused`, `network`, `econn` | `endpoint_unreachable` |
/// | anything else | `protocol` |
pub fn classify_rpc_error(error: &RpcError) -> ProviderError {
    let lower = error.message.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));
    if error.code == -32000 {
        return ProviderError::AuthRequired { login_hint: None };
    }
    if error.code == -32602 {
        return ProviderError::invalid(&error.message);
    }
    if has(&["rate limit", "rate_limit", "429", "too many requests"]) {
        return ProviderError::RateLimited {
            retry_after_ms: None,
        };
    }
    if has(&["overloaded", "503", "529"]) {
        return ProviderError::Overloaded;
    }
    if lower.contains("context") && has(&["length", "window", "too long"]) {
        return ProviderError::ContextTooSmall {
            needed: None,
            available: None,
        };
    }
    if has(&["unauthorized", "401", "invalid api key"]) {
        return ProviderError::Unauthorized;
    }
    if has(&["unreachable", "connection refused", "network", "econn"]) {
        return ProviderError::unreachable(&error.message);
    }
    match &error.detail {
        Some(detail) => ProviderError::protocol(format!(
            "the agent refused ({}): {}: {}",
            error.code,
            error.message,
            redact(detail)
        )),
        None => ProviderError::protocol(format!(
            "the agent refused ({}): {}",
            error.code, error.message
        )),
    }
}

/// The agent refuses the `mcpServers` of `session/new` / `session/load`: its message or
/// detail names MCP and says it does not support them. OpenClaw's `openclaw acp`
/// (2026.9.x) answers `-32603 Internal error` with `data.details` = "ACP bridge mode
/// does not support per-session MCP servers. …" (read in its published code, not run).
pub fn refuses_mcp_servers(error: &RpcError) -> bool {
    [Some(&error.message), error.detail.as_ref()]
        .into_iter()
        .flatten()
        .any(|text| {
            let lower = text.to_ascii_lowercase();
            lower.contains("mcp")
                && ["not support", "unsupported", "not allowed", "not accepted"]
                    .iter()
                    .any(|needle| lower.contains(needle))
        })
}

/// Puts the login hint on an `auth_required` error.
pub fn with_login_hint(error: ProviderError, hint: Option<&str>) -> ProviderError {
    match error {
        ProviderError::AuthRequired { login_hint: None } => ProviderError::AuthRequired {
            login_hint: hint.map(redact),
        },
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------

/// Ids an agent commonly gives each neutral mode (NOT VERIFIED: ACP leaves the ids
/// to the agent; `SessionSpec::policy.native_mode` names one exactly).
fn mode_ids(mode: PolicyMode) -> &'static [&'static str] {
    match mode {
        PolicyMode::PlanOnly => &["plan", "architect", "read-only", "readonly"],
        PolicyMode::Ask => &["ask", "default"],
        PolicyMode::AutoEdits => &["acceptEdits", "accept_edits", "accept-edits", "auto-edit"],
        PolicyMode::Trust => &["bypassPermissions", "yolo", "trust"],
        #[allow(unreachable_patterns)]
        _ => &[],
    }
}

/// The neutral mode of an agent mode id, when it is one of the conventional ids.
pub fn neutral_of_mode(id: &str) -> Option<PolicyMode> {
    [
        PolicyMode::PlanOnly,
        PolicyMode::Ask,
        PolicyMode::AutoEdits,
        PolicyMode::Trust,
    ]
    .into_iter()
    .find(|mode| mode_ids(*mode).contains(&id))
}

/// The id of the agent's mode for `mode`: `native` when the agent publishes it, else
/// the first published mode with a conventional id. `None`: nothing matches.
pub fn mode_id_for(
    mode: PolicyMode,
    native: Option<&str>,
    available: &[ModeInfo],
) -> Option<String> {
    if let Some(native) = native
        && available.iter().any(|published| published.id == native)
    {
        return Some(native.to_owned());
    }
    mode_ids(mode)
        .iter()
        .find_map(|id| available.iter().find(|published| published.id == *id))
        .map(|published| published.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::acp::wire::SessionNotification;

    fn state(thinking: bool) -> MapState {
        MapState::new(MapConfig {
            deltas: true,
            cost_basis: CostBasis::Unknown,
            prices: PriceTable::new(),
            login_hint: Some("agent login".to_owned()),
            mcp_servers: vec!["po".to_owned(), "po_admin".to_owned()],
            thinking,
        })
    }

    fn update(value: Value) -> Update {
        SessionNotification::parse(&json!({ "sessionId": "s", "update": value }))
            .unwrap()
            .update
    }

    #[test]
    fn chunks_become_deltas_then_one_text_before_the_next_other_event() {
        let mut state = state(true);
        state.begin_turn();
        let mut events = Vec::new();
        for text in ["Hel", "lo"] {
            events.extend(state.map_update(update(
                json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}),
            )));
        }
        events.extend(state.map_update(update(
            json!({"sessionUpdate":"tool_call","toolCallId":"c1","title":"ls","kind":"execute","status":"pending","rawInput":{"command":"ls"}}),
        )));
        let names: Vec<&str> = events.iter().map(AgentEvent::type_name).collect();
        assert_eq!(names, ["delta", "delta", "text", "tool_call"]);
        assert!(matches!(&events[2], AgentEvent::Text { text, .. } if text == "Hello"));
    }

    #[test]
    fn thinking_not_declared_is_dropped_with_one_notice() {
        let mut state = state(false);
        let chunk =
            json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"hm"}});
        let first = state.map_update(update(chunk.clone()));
        assert!(
            matches!(&first[..], [AgentEvent::ProviderNotice { kind, .. }] if kind == "thinking_not_declared")
        );
        assert!(state.map_update(update(chunk)).is_empty());
        assert!(state.thinking_seen());
    }

    #[test]
    fn tool_calls_are_completed_once_and_results_follow_status() {
        let mut state = state(true);
        state.begin_turn();
        let first = state.map_update(update(
            json!({"sessionUpdate":"tool_call","toolCallId":"c1","title":"po_task","kind":"other","status":"pending"}),
        ));
        assert!(
            matches!(&first[..], [AgentEvent::ToolCall { input_complete: false, category: ToolCategory::Mcp, canonical: Some(c), .. }] if c == "mcp__po__task")
        );
        let second = state.map_update(update(
            json!({"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"in_progress","rawInput":{"a":1}}),
        ));
        assert!(matches!(
            &second[..],
            [AgentEvent::ToolCall {
                input_complete: true,
                ..
            }]
        ));
        let third = state.map_update(update(
            json!({"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"failed","content":[{"type":"content","content":{"type":"text","text":"boom"}}]}),
        ));
        assert!(
            matches!(&third[..], [AgentEvent::ToolResult { is_error: true, output: Some(ToolOutput::Text(t)), .. }] if t == "boom")
        );
        // The longest matching server wins.
        let (category, canonical) =
            describe_tool("po_admin_reset", None, &["po".into(), "po_admin".into()]);
        assert_eq!(category, ToolCategory::Mcp);
        assert_eq!(canonical.as_deref(), Some("mcp__po_admin__reset"));
    }

    #[test]
    fn kinds_give_categories_and_canonical_names() {
        let none: [String; 0] = [];
        for (kind, category, canonical) in [
            ("read", ToolCategory::Read, Some("Read")),
            ("edit", ToolCategory::Edit, Some("Edit")),
            ("delete", ToolCategory::Edit, Some("Edit")),
            ("move", ToolCategory::Edit, Some("Edit")),
            ("execute", ToolCategory::Command, Some("Bash")),
            ("search", ToolCategory::Search, Some("Grep")),
            ("fetch", ToolCategory::Web, Some("WebFetch")),
            ("think", ToolCategory::Other, None),
            ("switch_mode", ToolCategory::Other, None),
            ("other", ToolCategory::Other, None),
            ("brand_new_kind", ToolCategory::Other, None),
        ] {
            let (got, name) = describe_tool("a title with spaces", Some(kind), &none);
            assert_eq!((got, name.as_deref()), (category, canonical), "{kind}");
        }
    }

    #[test]
    fn stop_reasons_and_rpc_errors_follow_the_documented_tables() {
        assert_eq!(stop_of("end_turn").0, StopReason::Completed);
        assert_eq!(stop_of("max_tokens").0, StopReason::MaxTokens);
        assert_eq!(stop_of("max_turn_requests").0, StopReason::MaxTurns);
        assert_eq!(stop_of("cancelled").0, StopReason::Interrupted);
        let (refusal, is_error, error) = stop_of("refusal");
        assert_eq!(
            (refusal, is_error, error),
            (StopReason::Refusal, false, None)
        );
        assert!(stop_of("new_reason").1);
        let rpc = |code, message: &str| {
            classify_rpc_error(&RpcError {
                code,
                message: message.to_owned(),
                detail: None,
            })
            .kind()
        };
        assert_eq!(rpc(-32000, "Authentication required"), "auth_required");
        assert_eq!(rpc(-32602, "bad"), "invalid_request");
        assert_eq!(rpc(-32603, "Rate limit exceeded"), "rate_limited");
        assert_eq!(rpc(-32603, "provider overloaded"), "overloaded");
        assert_eq!(rpc(-32603, "context length exceeded"), "context_too_small");
        assert_eq!(rpc(-32603, "Unauthorized"), "unauthorized");
        assert_eq!(rpc(-32603, "network error"), "endpoint_unreachable");
        assert_eq!(rpc(-32603, "whatever"), "protocol");
    }

    #[test]
    fn modes_map_both_ways_and_native_wins() {
        let available = vec![
            ModeInfo {
                id: "ask".into(),
                name: String::new(),
                description: None,
            },
            ModeInfo {
                id: "acceptEdits".into(),
                name: String::new(),
                description: None,
            },
            ModeInfo {
                id: "custom".into(),
                name: String::new(),
                description: None,
            },
        ];
        assert_eq!(
            mode_id_for(PolicyMode::AutoEdits, None, &available).as_deref(),
            Some("acceptEdits")
        );
        assert_eq!(mode_id_for(PolicyMode::Trust, None, &available), None);
        assert_eq!(
            mode_id_for(PolicyMode::Trust, Some("custom"), &available).as_deref(),
            Some("custom")
        );
        assert_eq!(
            mode_id_for(PolicyMode::Trust, Some("unknown"), &available),
            None
        );
        assert_eq!(neutral_of_mode("plan"), Some(PolicyMode::PlanOnly));
        assert_eq!(neutral_of_mode("custom"), None);
    }

    #[test]
    fn answers_select_the_option_of_the_scope_and_refuse_the_rest() {
        let options = vec![
            PermissionOption {
                option_id: "o1".into(),
                name: String::new(),
                kind: "allow_once".into(),
            },
            PermissionOption {
                option_id: "o2".into(),
                name: String::new(),
                kind: "reject_once".into(),
            },
        ];
        let (allow, _) = answer_for(&options, &PermissionDecision::allow_once()).unwrap();
        assert_eq!(
            allow,
            json!({"outcome":{"outcome":"selected","optionId":"o1"}})
        );
        let (deny, _) = answer_for(&options, &PermissionDecision::deny()).unwrap();
        assert_eq!(
            deny,
            json!({"outcome":{"outcome":"selected","optionId":"o2"}})
        );
        for scope in [PermissionScope::Always, PermissionScope::Session] {
            let decision = PermissionDecision::Allow {
                scope,
                updated_input: None,
            };
            assert_eq!(
                answer_for(&options, &decision).unwrap_err(),
                ProviderError::unsupported("permission_scope")
            );
        }
        let only_allow = &options[..1];
        let (cancelled, _) = answer_for(only_allow, &PermissionDecision::deny()).unwrap();
        assert_eq!(cancelled, json!({"outcome":{"outcome":"cancelled"}}));
    }

    /// OpenClaw's refusal arrives as `-32603 Internal error` with its text in
    /// `data.details`: it is read there, recognised, and shown in the error.
    #[test]
    fn a_refusal_of_mcp_servers_is_read_in_the_data_of_the_error() {
        let line = r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32603,"message":"Internal error","data":{"details":"ACP bridge mode does not support per-session MCP servers. Configure MCP on the OpenClaw gateway or agent instead."}}}"#;
        let Ok(super::super::wire::Frame::Response {
            outcome: Err(error),
            ..
        }) = super::super::wire::Frame::parse(line)
        else {
            panic!("an error response");
        };
        assert!(refuses_mcp_servers(&error));
        let shown = classify_rpc_error(&error).to_string();
        assert!(shown.contains("per-session MCP servers"), "{shown}");
        let other = |message: &str, detail: Option<&str>| RpcError {
            code: -32603,
            message: message.to_owned(),
            detail: detail.map(str::to_owned),
        };
        assert!(refuses_mcp_servers(&other(
            "mcpServers are not supported",
            None
        )));
        assert!(!refuses_mcp_servers(&other(
            "Internal error",
            Some("rate limit exceeded")
        )));
        assert!(!refuses_mcp_servers(&other("the MCP server crashed", None)));
        assert!(!refuses_mcp_servers(&other("Internal error", None)));
    }
}
