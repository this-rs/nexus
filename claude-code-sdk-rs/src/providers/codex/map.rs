//! Projection of `codex app-server` messages onto [`AgentEvent`]s: pure state
//! machine, no I/O (contract §4, §14).
//!
//! # Notification table
//!
//! | `app-server` | `AgentEvent` |
//! |---|---|
//! | `item/agentMessage/delta` | `delta { text }` (only when the session asked for deltas) |
//! | `item/completed` `agentMessage` | `text` (the item's text, else what the deltas accumulated) |
//! | `item/reasoning/summaryTextDelta`, `textDelta` | `delta { thinking }` |
//! | `item/completed` `reasoning` | `thinking` (summaries, else raw blocks, else the deltas) |
//! | `item/started` `commandExecution` | `tool_call` `shell`, canonical `Bash`, category `command` |
//! | `item/started` `fileChange` | `tool_call` `apply_patch`, canonical `Edit`, category `edit` |
//! | `item/started` `mcpToolCall` | `tool_call` `mcp__<server>__<tool>`, category `mcp` |
//! | `item/started` `webSearch` | `tool_call` `web_search`, canonical `WebSearch`, category `web` |
//! | `item/started` `collabToolCall` | `tool_call` `<tool>` (`spawn_agent`…), category `agent` |
//! | `item/completed` of those | `tool_result` (`is_error` when `failed` / `declined` / exit code ≠ 0) |
//! | `item/started`, `item/completed` `contextCompaction` | `compaction` `started` / `completed`, trigger `auto` |
//! | `thread/tokenUsage/updated` | nothing now: folded into the next `done.usage` |
//! | `turn/completed` `completed` / `interrupted` | `done { completed | interrupted }` |
//! | `turn/completed` `failed` | `done { error, is_error, error: <classified> }` (contract v2) |
//! | `error` (mid-turn) | nothing now: its classification is used if `turn/completed` carries none |
//! | `model/rerouted` | `model_changed` |
//! | `mcpServer/startupStatus/updated` | `provider_notice { mcp_server_status }` |
//! | anything else (`turn/diff/updated`, `turn/plan/updated`, `warning`…) | dropped |
//!
//! An item of a thread other than the main one (a sub-agent) carries
//! `parent = <that thread id>` (`subagents: separate_thread`); its turn lifecycle
//! never ends the main turn.
//!
//! A `tool_result` or a `permission_ask` whose call was never announced is
//! preceded by a synthesised `tool_call`, so the invariant "a result names a call
//! already emitted" holds whatever the order the server used.
//!
//! # Usage and cost
//!
//! `done.usage` is the **difference** of the cumulative `total` between the start
//! and the end of the turn (a turn makes several model requests).
//! `input_tokens` excludes the cached tokens (`cache_read_tokens`), as for Claude
//! Code, so that the price table never counts them twice. The cost follows the
//! instance configuration and is never `reported`: `unknown` → no amount; `free` →
//! 0; `priced` → the price table, `None` while the usage or the price is unknown.

use std::collections::HashMap;
use std::time::Instant;

use serde_json::{Value, json};

use super::wire::{
    CodexErrorInfo, ElicitationRequest, ErrorPayload, Notification, ServerRequest, ThreadItem,
    TokenBreakdown, TurnEnvelope, decision_response, elicitation_response, permissions_response,
};
use crate::agent::{
    AgentEvent, CompactionPhase, CompactionTrigger, Cost, CostBasis, DeltaKind, PermissionDecision,
    PermissionScope, ProviderError, StopReason, ToolCategory, ToolOutput, Usage, redact,
};
use crate::model::PriceTable;

/// What the mapper needs from the instance configuration.
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
}

#[derive(Debug, Clone)]
struct ItemMeta {
    announced: bool,
}

/// Mapping state of one session.
#[derive(Debug)]
pub struct MapState {
    config: MapConfig,
    main_thread: Option<String>,
    model: Option<String>,
    items: HashMap<String, ItemMeta>,
    /// MCP tool calls announced and not finished, oldest first: `(server, item)`.
    open_mcp: Vec<(String, String)>,
    text: HashMap<String, String>,
    thinking: HashMap<String, String>,
    total: TokenBreakdown,
    baseline: TokenBreakdown,
    last: Option<TokenBreakdown>,
    saw_usage: bool,
    /// `turn/started` of the current turn was seen: usage before it is a replay.
    turn_live: bool,
    last_text: Option<String>,
    pending_error: Option<ErrorPayload>,
    turn_started: Option<Instant>,
}

impl MapState {
    /// A fresh state.
    pub fn new(config: MapConfig) -> Self {
        Self {
            config,
            main_thread: None,
            model: None,
            items: HashMap::new(),
            open_mcp: Vec::new(),
            text: HashMap::new(),
            thinking: HashMap::new(),
            total: TokenBreakdown::default(),
            baseline: TokenBreakdown::default(),
            last: None,
            saw_usage: false,
            turn_live: false,
            last_text: None,
            pending_error: None,
            turn_started: None,
        }
    }

    /// Records the id of the main thread (events of any other thread are a
    /// sub-agent's).
    pub fn set_main_thread(&mut self, id: String) {
        self.main_thread.get_or_insert(id);
    }

    /// Main thread id, when known.
    pub fn main_thread(&self) -> Option<&str> {
        self.main_thread.as_deref()
    }

    /// Model in effect, for `done.model` and the price.
    pub fn set_model(&mut self, model: Option<String>) {
        self.model = model;
    }

    /// A turn starts: the usage of the turn is counted from here.
    pub fn begin_turn(&mut self) {
        self.baseline = self.total;
        self.saw_usage = false;
        self.turn_live = false;
        self.last_text = None;
        self.pending_error = None;
        self.turn_started = Some(Instant::now());
    }

    fn parent(&self, thread: &Option<String>) -> Option<String> {
        match (thread, &self.main_thread) {
            (Some(thread), Some(main)) if thread != main => Some(thread.clone()),
            _ => None,
        }
    }

    fn is_main(&self, thread: &Option<String>) -> bool {
        self.parent(thread).is_none()
    }

    /// Maps one notification to zero or more events.
    pub fn map(&mut self, notification: Notification) -> Vec<AgentEvent> {
        match notification {
            Notification::AgentMessageDelta(delta) => {
                self.text
                    .entry(delta.item_id.clone())
                    .or_default()
                    .push_str(&delta.delta);
                if !self.config.deltas || delta.delta.is_empty() {
                    return Vec::new();
                }
                vec![AgentEvent::Delta {
                    kind: DeltaKind::Text,
                    text: delta.delta,
                    index: None,
                    tool_call_id: None,
                    parent: self.parent(&delta.thread_id),
                }]
            },
            Notification::ReasoningSummaryDelta(delta)
            | Notification::ReasoningTextDelta(delta) => {
                self.thinking
                    .entry(delta.item_id.clone())
                    .or_default()
                    .push_str(&delta.delta);
                if !self.config.deltas || delta.delta.is_empty() {
                    return Vec::new();
                }
                vec![AgentEvent::Delta {
                    kind: DeltaKind::Thinking,
                    text: delta.delta,
                    index: None,
                    tool_call_id: None,
                    parent: self.parent(&delta.thread_id),
                }]
            },
            Notification::ItemStarted(envelope) => {
                let parent = self.parent(&envelope.thread_id);
                self.item_started(envelope.item, parent)
            },
            Notification::ItemCompleted(envelope) => {
                let parent = self.parent(&envelope.thread_id);
                self.item_completed(envelope.item, parent)
            },
            Notification::TokenUsage(envelope) => {
                if self.is_main(&envelope.thread_id) {
                    self.total = envelope.token_usage.total;
                    self.last = envelope.token_usage.last;
                    if self.turn_live {
                        self.saw_usage = true;
                    } else {
                        // Before `turn/started` (or between turns): the usage the server
                        // replays after `thread/resume`, which is not this turn's.
                        self.baseline = self.total;
                    }
                }
                Vec::new()
            },
            Notification::TurnStarted(envelope) => {
                if self.is_main(&envelope.thread_id) {
                    self.turn_live = true;
                }
                Vec::new()
            },
            Notification::Error(envelope) => {
                if self.is_main(&envelope.thread_id) && !envelope.will_retry {
                    self.pending_error = Some(envelope.error);
                }
                Vec::new()
            },
            Notification::TurnCompleted(envelope) => {
                if self.is_main(&envelope.thread_id) {
                    vec![self.done(envelope)]
                } else {
                    Vec::new()
                }
            },
            Notification::ModelRerouted(rerouted) => {
                if self.is_main(&rerouted.thread_id) {
                    self.model = Some(rerouted.to_model.clone());
                    vec![AgentEvent::ModelChanged {
                        model: rerouted.to_model,
                    }]
                } else {
                    Vec::new()
                }
            },
            Notification::McpStartup(status) => vec![AgentEvent::ProviderNotice {
                kind: "mcp_server_status".to_owned(),
                data: json!({ "name": status.name, "status": status.status }),
            }],
            Notification::ThreadStarted(_)
            | Notification::RequestResolved(_)
            | Notification::Other(_) => Vec::new(),
        }
    }

    fn announce(&mut self, item: &ThreadItem, parent: Option<String>) -> Option<AgentEvent> {
        let call = describe(item)?;
        if let Some(server) = call.server {
            self.open_mcp.push((server, call.id.to_owned()));
        }
        self.items
            .insert(call.id.to_owned(), ItemMeta { announced: true });
        Some(AgentEvent::ToolCall {
            id: call.id.to_owned(),
            name: call.name,
            canonical: call.canonical,
            category: call.category,
            input: call.input,
            input_complete: true,
            seq: None,
            parent,
        })
    }

    fn item_started(&mut self, item: ThreadItem, parent: Option<String>) -> Vec<AgentEvent> {
        if let ThreadItem::ContextCompaction { .. } = item {
            return vec![AgentEvent::Compaction {
                phase: CompactionPhase::Started,
                trigger: Some(CompactionTrigger::Auto),
                pre_tokens: self.last.map(|last| last.input_tokens),
            }];
        }
        self.announce(&item, parent).into_iter().collect()
    }

    fn item_completed(&mut self, item: ThreadItem, parent: Option<String>) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        match &item {
            ThreadItem::AgentMessage { id, text } => {
                let accumulated = self.text.remove(id).unwrap_or_default();
                let text = if text.is_empty() {
                    accumulated
                } else {
                    text.clone()
                };
                if !text.is_empty() {
                    if parent.is_none() {
                        self.last_text = Some(text.clone());
                    }
                    events.push(AgentEvent::Text {
                        text,
                        seq: None,
                        parent,
                    });
                }
            },
            ThreadItem::Reasoning {
                id,
                summary,
                content,
            } => {
                let accumulated = self.thinking.remove(id).unwrap_or_default();
                let joined = |parts: &[String]| {
                    parts
                        .iter()
                        .filter(|part| !part.is_empty())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n\n")
                };
                let mut text = joined(summary);
                if text.is_empty() {
                    text = joined(content);
                }
                if text.is_empty() {
                    text = accumulated;
                }
                if !text.is_empty() {
                    events.push(AgentEvent::Thinking {
                        text,
                        signature: None,
                        seq: None,
                        parent,
                    });
                }
            },
            ThreadItem::ContextCompaction { .. } => events.push(AgentEvent::Compaction {
                phase: CompactionPhase::Completed,
                trigger: Some(CompactionTrigger::Auto),
                pre_tokens: self.last.map(|last| last.input_tokens),
            }),
            ThreadItem::CommandExecution { .. }
            | ThreadItem::FileChange { .. }
            | ThreadItem::McpToolCall { .. }
            | ThreadItem::WebSearch { .. }
            | ThreadItem::CollabToolCall { .. } => {
                let id = item_id(&item);
                let announced = self.items.get(id).is_some_and(|meta| meta.announced);
                if !announced {
                    events.extend(self.announce(&item, parent.clone()));
                }
                self.open_mcp.retain(|(_, open)| open != id);
                self.items.remove(id);
                events.push(tool_result(&item, id.to_owned(), parent));
            },
            ThreadItem::UserMessage { .. } | ThreadItem::Other => {},
        }
        events
    }

    fn done(&mut self, envelope: TurnEnvelope) -> AgentEvent {
        let turn = envelope.turn;
        let (stop_reason, is_error) = match turn.status.as_str() {
            "completed" => (StopReason::Completed, false),
            "interrupted" => (StopReason::Interrupted, false),
            _ => (StopReason::Error, true),
        };
        let error = is_error.then(|| {
            let payload = turn.error.clone().or_else(|| self.pending_error.take());
            match payload {
                Some(payload) => classify(&payload, self.config.login_hint.as_deref()),
                None => ProviderError::protocol(format!(
                    "turn ended with status `{}` and no error",
                    turn.status
                )),
            }
        });
        let usage = self.usage();
        let cost = self.cost(&usage);
        let duration_ms = self
            .turn_started
            .take()
            .map(|started| u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        self.turn_live = false;
        self.items.clear();
        self.open_mcp.clear();
        self.text.clear();
        self.thinking.clear();
        AgentEvent::Done {
            stop_reason,
            subtype: Some(turn.status),
            is_error,
            result_text: self.last_text.take(),
            usage,
            cost,
            duration_ms,
            duration_api_ms: None,
            num_turns: 0,
            model: self.model.clone(),
            provider_session_id: self.main_thread.clone(),
            structured_output: None,
            error,
        }
    }

    fn usage(&self) -> Usage {
        if !self.saw_usage {
            return Usage::default();
        }
        let delta = |now: u64, before: u64| now.saturating_sub(before);
        let input = delta(self.total.input_tokens, self.baseline.input_tokens);
        let cached = delta(
            self.total.cached_input_tokens,
            self.baseline.cached_input_tokens,
        );
        Usage {
            input_tokens: Some(input.saturating_sub(cached)),
            output_tokens: Some(delta(self.total.output_tokens, self.baseline.output_tokens)),
            cache_read_tokens: Some(cached),
            cache_creation_tokens: None,
            reasoning_tokens: Some(delta(
                self.total.reasoning_output_tokens,
                self.baseline.reasoning_output_tokens,
            )),
            context_tokens: self.last.map(|last| last.total_tokens),
            by_model: Vec::new(),
        }
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

    /// Builds the permission request of a server request, with the events to emit
    /// first (a synthesised `tool_call` when the item was not announced).
    pub fn ask_for(&mut self, request: &ServerRequest) -> Option<Ask> {
        match request {
            ServerRequest::Elicitation(elicitation) => self.ask_elicitation(elicitation),
            ServerRequest::CommandApproval(approval) => {
                let command = approval.command.as_ref().map(command_text);
                let item = ThreadItem::CommandExecution {
                    id: approval.item_id.clone(),
                    command: approval.command.clone().unwrap_or(Value::Null),
                    cwd: approval.cwd.clone(),
                    status: "inProgress".to_owned(),
                    aggregated_output: None,
                    exit_code: None,
                };
                let mut events = self.announce_if_missing(&item);
                let scopes = decision_scopes(approval.available_decisions.as_deref());
                let input = json!({
                    "command": command, "cwd": approval.cwd, "reason": approval.reason,
                });
                events.push(AgentEvent::PermissionAsk {
                    request_id: String::new(),
                    tool_name: "shell".to_owned(),
                    input: input.clone(),
                    category: ToolCategory::Command,
                    canonical: Some("Bash".to_owned()),
                    tool_call_id: Some(approval.item_id.clone()),
                    scopes: scopes.clone(),
                    parent: None,
                });
                Some(Ask {
                    events,
                    kind: AskKind::Decision,
                    scopes,
                    tool: "Bash".to_owned(),
                    arg: command.unwrap_or_default(),
                    category: ToolCategory::Command,
                })
            },
            ServerRequest::FileChangeApproval(approval) => {
                let item = ThreadItem::FileChange {
                    id: approval.item_id.clone(),
                    changes: Vec::new(),
                    status: "inProgress".to_owned(),
                };
                let mut events = self.announce_if_missing(&item);
                let scopes = vec![PermissionScope::Once, PermissionScope::Session];
                events.push(AgentEvent::PermissionAsk {
                    request_id: String::new(),
                    tool_name: "apply_patch".to_owned(),
                    input: json!({ "reason": approval.reason }),
                    category: ToolCategory::Edit,
                    canonical: Some("Edit".to_owned()),
                    tool_call_id: Some(approval.item_id.clone()),
                    scopes: scopes.clone(),
                    parent: None,
                });
                Some(Ask {
                    events,
                    kind: AskKind::Decision,
                    scopes,
                    tool: "Edit".to_owned(),
                    arg: String::new(),
                    category: ToolCategory::Edit,
                })
            },
            ServerRequest::PermissionsApproval(approval) => {
                let scopes = vec![PermissionScope::Once, PermissionScope::Session];
                Some(Ask {
                    events: vec![AgentEvent::PermissionAsk {
                        request_id: String::new(),
                        tool_name: "request_permissions".to_owned(),
                        input: approval.permissions.clone(),
                        category: ToolCategory::Other,
                        canonical: None,
                        tool_call_id: None,
                        scopes: scopes.clone(),
                        parent: None,
                    }],
                    kind: AskKind::Permissions(approval.permissions.clone()),
                    scopes,
                    tool: "request_permissions".to_owned(),
                    arg: approval.permissions.to_string(),
                    category: ToolCategory::Other,
                })
            },
            ServerRequest::Unknown(_) => None,
        }
    }

    fn announce_if_missing(&mut self, item: &ThreadItem) -> Vec<AgentEvent> {
        if self
            .items
            .get(item_id(item))
            .is_some_and(|meta| meta.announced)
        {
            return Vec::new();
        }
        self.announce(item, None).into_iter().collect()
    }

    fn ask_elicitation(&mut self, request: &ElicitationRequest) -> Option<Ask> {
        let meta = request.meta.as_ref();
        let kind = meta
            .and_then(|meta| meta.get("codex_approval_kind"))
            .and_then(Value::as_str);
        if kind != Some("mcp_tool_call") || request.mode.as_deref().is_some_and(|m| m != "form") {
            return None;
        }
        let text = |key: &str| {
            meta.and_then(|meta| meta.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let tool = text("tool_name").or_else(|| text("tool_title"));
        let name = match &tool {
            Some(tool) => format!("mcp__{}__{tool}", request.server_name),
            None => format!("mcp__{}", request.server_name),
        };
        let input = meta
            .and_then(|meta| meta.get("tool_params"))
            .cloned()
            .unwrap_or_else(|| json!({ "message": redact(&request.message) }));
        let tool_call_id = text("tool_call_id").or_else(|| {
            self.open_mcp
                .iter()
                .rev()
                .find(|(server, _)| *server == request.server_name)
                .map(|(_, id)| id.clone())
        });
        let persist: Vec<String> = match meta.and_then(|meta| meta.get("persist")) {
            Some(Value::String(one)) => vec![one.clone()],
            Some(Value::Array(many)) => many
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        };
        let mut scopes = vec![PermissionScope::Once];
        if persist.iter().any(|p| p == "session") {
            scopes.push(PermissionScope::Session);
        }
        if persist.iter().any(|p| p == "always") {
            scopes.push(PermissionScope::Always);
        }
        let arg = input.to_string();
        Some(Ask {
            events: vec![AgentEvent::PermissionAsk {
                request_id: String::new(),
                tool_name: name.clone(),
                input,
                category: ToolCategory::Mcp,
                canonical: tool.is_some().then(|| name.clone()),
                tool_call_id,
                scopes: scopes.clone(),
                parent: None,
            }],
            kind: AskKind::Elicitation,
            scopes,
            tool: name,
            arg,
            category: ToolCategory::Mcp,
        })
    }
}

/// The `tool_call` an item announces.
struct CallSpec<'a> {
    id: &'a str,
    name: String,
    canonical: Option<String>,
    category: ToolCategory,
    input: Value,
    /// MCP server of an MCP call (to match its approval with it).
    server: Option<String>,
}

/// What a tool-like item looks like as a `tool_call`; `None` for the others.
fn describe(item: &ThreadItem) -> Option<CallSpec<'_>> {
    Some(match item {
        ThreadItem::CommandExecution {
            id, command, cwd, ..
        } => CallSpec {
            id,
            name: "shell".to_owned(),
            canonical: Some("Bash".to_owned()),
            category: ToolCategory::Command,
            input: json!({ "command": command_text(command), "cwd": cwd }),
            server: None,
        },
        ThreadItem::FileChange { id, changes, .. } => CallSpec {
            id,
            name: "apply_patch".to_owned(),
            canonical: Some("Edit".to_owned()),
            category: ToolCategory::Edit,
            input: json!({ "changes": changes }),
            server: None,
        },
        ThreadItem::McpToolCall {
            id,
            server,
            tool,
            arguments,
            ..
        } => {
            let name = format!("mcp__{server}__{tool}");
            CallSpec {
                id,
                canonical: Some(name.clone()),
                name,
                category: ToolCategory::Mcp,
                input: arguments.clone(),
                server: Some(server.clone()),
            }
        },
        ThreadItem::WebSearch { id, query } => CallSpec {
            id,
            name: "web_search".to_owned(),
            canonical: Some("WebSearch".to_owned()),
            category: ToolCategory::Web,
            input: json!({ "query": query }),
            server: None,
        },
        ThreadItem::CollabToolCall {
            id,
            tool,
            prompt,
            new_thread_id,
            ..
        } => CallSpec {
            id,
            name: tool.clone(),
            canonical: Some(tool.clone()),
            category: ToolCategory::Agent,
            input: json!({ "prompt": prompt, "new_thread_id": new_thread_id }),
            server: None,
        },
        _ => return None,
    })
}

/// A permission request ready to be shown, and what is needed to answer it.
#[derive(Debug, Clone)]
pub struct Ask {
    /// Events to emit, in order; the last one is the `permission_ask`, whose
    /// `request_id` is empty and is filled in by the session.
    pub events: Vec<AgentEvent>,
    /// Which answer shape the server expects.
    pub kind: AskKind,
    /// Scopes this request offers.
    pub scopes: Vec<PermissionScope>,
    /// Tool name for the local policy (`decide`).
    pub tool: String,
    /// Argument for the local policy.
    pub arg: String,
    /// Category for the local policy.
    pub category: ToolCategory,
}

/// The answer shape of a server request.
#[derive(Debug, Clone, PartialEq)]
pub enum AskKind {
    /// `mcpServer/elicitation/request`: `{action, content}`.
    Elicitation,
    /// Command and file-change approvals: `{decision}`.
    Decision,
    /// `item/permissions/requestApproval`: the granted subset of this profile.
    Permissions(Value),
}

/// The wire answer to a permission decision, and whether the turn is to be
/// interrupted after it.
///
/// `Allow` with an `updated_input` is `Unsupported { permission_updated_input }`
/// (Codex has no way to rewrite the input of an approved call); a scope the
/// request did not offer is `Unsupported { permission_scope }`.
pub fn answer_for(
    kind: &AskKind,
    scopes: &[PermissionScope],
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
            if !scopes.contains(scope) {
                return Err(ProviderError::unsupported("permission_scope"));
            }
            let answer = match kind {
                AskKind::Elicitation => elicitation_response(
                    "accept",
                    match scope {
                        PermissionScope::Session => Some("session"),
                        PermissionScope::Always => Some("always"),
                        _ => None,
                    },
                ),
                AskKind::Decision => decision_response(match scope {
                    PermissionScope::Session => "acceptForSession",
                    _ => "accept",
                }),
                AskKind::Permissions(requested) => {
                    permissions_response(requested.clone(), *scope == PermissionScope::Session)
                },
            };
            Ok((answer, false))
        },
        PermissionDecision::Deny { interrupt, .. } => {
            let answer = match kind {
                AskKind::Elicitation => {
                    elicitation_response(if *interrupt { "cancel" } else { "decline" }, None)
                },
                AskKind::Decision => {
                    decision_response(if *interrupt { "cancel" } else { "decline" })
                },
                AskKind::Permissions(_) => permissions_response(json!({}), false),
            };
            Ok((answer, *interrupt))
        },
    }
}

/// Scopes of a command approval from its `availableDecisions` (preferred over the
/// fixed set when present).
fn decision_scopes(available: Option<&[Value]>) -> Vec<PermissionScope> {
    let mut scopes = vec![PermissionScope::Once];
    let offers = |name: &str| match available {
        None => true,
        Some(list) => list.iter().any(|entry| entry.as_str() == Some(name)),
    };
    if offers("acceptForSession") {
        scopes.push(PermissionScope::Session);
    }
    scopes
}

fn item_id(item: &ThreadItem) -> &str {
    match item {
        ThreadItem::UserMessage { id }
        | ThreadItem::AgentMessage { id, .. }
        | ThreadItem::Reasoning { id, .. }
        | ThreadItem::CommandExecution { id, .. }
        | ThreadItem::FileChange { id, .. }
        | ThreadItem::McpToolCall { id, .. }
        | ThreadItem::CollabToolCall { id, .. }
        | ThreadItem::WebSearch { id, .. }
        | ThreadItem::ContextCompaction { id } => id,
        ThreadItem::Other => "",
    }
}

fn command_text(command: &Value) -> String {
    match command {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn tool_result(item: &ThreadItem, id: String, parent: Option<String>) -> AgentEvent {
    let failed = |status: &str| matches!(status, "failed" | "declined");
    let (output, is_error) = match item {
        ThreadItem::CommandExecution {
            status,
            aggregated_output,
            exit_code,
            ..
        } => (
            aggregated_output.clone().map(ToolOutput::Text),
            failed(status) || exit_code.is_some_and(|code| code != 0),
        ),
        ThreadItem::FileChange { status, .. } => (
            (status == "declined").then(|| ToolOutput::Text("declined".to_owned())),
            failed(status),
        ),
        ThreadItem::McpToolCall {
            status,
            result,
            error,
            ..
        } => {
            if let Some(error) = error.as_ref().filter(|error| !error.is_null()) {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| error.to_string());
                (Some(ToolOutput::Text(redact(&message))), true)
            } else {
                let output = result.as_ref().map(|result| {
                    match result.get("content").and_then(Value::as_array) {
                        Some(blocks) => ToolOutput::Blocks(blocks.clone()),
                        None => ToolOutput::Text(result.to_string()),
                    }
                });
                (output, failed(status))
            }
        },
        ThreadItem::CollabToolCall { status, .. } => (None, failed(status)),
        _ => (None, false),
    };
    AgentEvent::ToolResult {
        id,
        output,
        is_error,
        seq: None,
        parent,
    }
}

/// `codexErrorInfo` → [`ProviderError`] (contract §7).
///
/// | `codexErrorInfo` | error |
/// |---|---|
/// | `ContextWindowExceeded` | `context_too_small` |
/// | `UsageLimitExceeded` | `rate_limited` |
/// | `HttpConnectionFailed`, `ResponseStreamDisconnected`, `ResponseStreamConnectionFailed`, `ResponseTooManyFailedAttempts` | `endpoint_unreachable` |
/// | `Unauthorized` | `unauthorized`, or `auth_required` when the message says nobody is logged in |
/// | `BadRequest` | `invalid_request` |
/// | `SandboxError`, `InternalServerError`, `Other`, unknown, none | `protocol` |
pub fn classify(payload: &ErrorPayload, login_hint: Option<&str>) -> ProviderError {
    let message = match &payload.additional_details {
        Some(details) if !details.is_empty() => format!("{} ({details})", payload.message),
        _ => payload.message.clone(),
    };
    let Some(CodexErrorInfo { kind, http_status }) = &payload.info else {
        return ProviderError::protocol(message);
    };
    match kind.as_str() {
        "ContextWindowExceeded" => ProviderError::ContextTooSmall {
            needed: None,
            available: None,
        },
        "UsageLimitExceeded" => ProviderError::RateLimited {
            retry_after_ms: None,
        },
        "HttpConnectionFailed"
        | "ResponseStreamDisconnected"
        | "ResponseStreamConnectionFailed"
        | "ResponseTooManyFailedAttempts" => ProviderError::unreachable(match http_status {
            Some(status) => format!("{kind} (HTTP {status}): {message}"),
            None => format!("{kind}: {message}"),
        }),
        "Unauthorized" => {
            let lower = message.to_ascii_lowercase();
            let logged_out = [
                "log in",
                "login",
                "sign in",
                "not logged",
                "not authenticated",
            ]
            .iter()
            .any(|needle| lower.contains(needle));
            if logged_out {
                ProviderError::AuthRequired {
                    login_hint: login_hint.map(str::to_owned),
                }
            } else {
                ProviderError::Unauthorized
            }
        },
        "BadRequest" => ProviderError::invalid(message),
        _ => ProviderError::protocol(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::codex::wire::{DeltaEnvelope, ItemEnvelope, Turn};

    fn state() -> MapState {
        let mut state = MapState::new(MapConfig {
            deltas: true,
            cost_basis: CostBasis::Unknown,
            prices: PriceTable::new(),
            login_hint: Some("codex login".to_owned()),
        });
        state.set_main_thread("thr_1".to_owned());
        state
    }

    fn payload(info: Value) -> ErrorPayload {
        serde_json::from_value(json!({"message": "boom", "codexErrorInfo": info})).unwrap()
    }

    #[test]
    fn error_info_table_is_the_documented_one() {
        let kind = |info: Value| classify(&payload(info), Some("codex login")).kind();
        assert_eq!(kind(json!("ContextWindowExceeded")), "context_too_small");
        assert_eq!(kind(json!("UsageLimitExceeded")), "rate_limited");
        assert_eq!(
            kind(json!({"HttpConnectionFailed": {"httpStatusCode": 502}})),
            "endpoint_unreachable"
        );
        assert_eq!(
            kind(json!("ResponseStreamDisconnected")),
            "endpoint_unreachable"
        );
        assert_eq!(kind(json!("Unauthorized")), "unauthorized");
        assert_eq!(kind(json!("BadRequest")), "invalid_request");
        for other in [
            "SandboxError",
            "InternalServerError",
            "Other",
            "SomethingNew",
        ] {
            assert_eq!(kind(json!(other)), "protocol", "{other}");
        }
        let no_info: ErrorPayload = serde_json::from_value(json!({"message": "x"})).unwrap();
        assert_eq!(classify(&no_info, None).kind(), "protocol");
        let logged_out: ErrorPayload = serde_json::from_value(
            json!({"message": "Please log in again", "codexErrorInfo": "Unauthorized"}),
        )
        .unwrap();
        assert!(matches!(
            classify(&logged_out, Some("codex login")),
            ProviderError::AuthRequired { login_hint: Some(hint) } if hint == "codex login"
        ));
    }

    #[test]
    fn retryable_classes_are_the_retryable_errors() {
        assert!(classify(&payload(json!("UsageLimitExceeded")), None).retryable());
        assert!(classify(&payload(json!("ResponseStreamDisconnected")), None).retryable());
        assert!(!classify(&payload(json!("ContextWindowExceeded")), None).retryable());
        assert!(!classify(&payload(json!("InternalServerError")), None).retryable());
    }

    #[test]
    fn a_message_is_text_after_its_deltas_and_the_turn_ends_with_done() {
        let mut state = state();
        state.begin_turn();
        let delta = |text: &str| {
            Notification::AgentMessageDelta(DeltaEnvelope {
                thread_id: Some("thr_1".into()),
                item_id: "m".into(),
                delta: text.into(),
            })
        };
        let mut events = state.map(delta("Hel"));
        events.extend(state.map(delta("lo")));
        events.extend(state.map(Notification::ItemCompleted(ItemEnvelope {
            thread_id: None,
            turn_id: None,
            item: ThreadItem::AgentMessage {
                id: "m".into(),
                text: String::new(),
            },
        })));
        events.extend(state.map(Notification::TurnCompleted(TurnEnvelope {
            thread_id: Some("thr_1".into()),
            turn: Turn {
                id: "t".into(),
                status: "completed".into(),
                error: None,
            },
        })));
        let names: Vec<&str> = events.iter().map(AgentEvent::type_name).collect();
        assert_eq!(names, ["delta", "delta", "text", "done"]);
        assert!(matches!(&events[2], AgentEvent::Text { text, .. } if text == "Hello"));
        assert!(
            matches!(&events[3], AgentEvent::Done { result_text: Some(t), .. } if t == "Hello")
        );
    }

    #[test]
    fn a_result_without_a_call_synthesises_the_call_first() {
        let mut state = state();
        let events = state.map(Notification::ItemCompleted(ItemEnvelope {
            thread_id: None,
            turn_id: None,
            item: ThreadItem::McpToolCall {
                id: "c1".into(),
                server: "po".into(),
                tool: "plan".into(),
                status: "completed".into(),
                arguments: json!({}),
                result: Some(json!({"content": [{"type": "text", "text": "ok"}]})),
                error: None,
            },
        }));
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], AgentEvent::ToolCall { name, .. } if name == "mcp__po__plan"));
        assert!(matches!(
            &events[1],
            AgentEvent::ToolResult {
                is_error: false,
                ..
            }
        ));
    }

    #[test]
    fn a_sub_agent_turn_does_not_end_the_main_turn_and_carries_its_parent() {
        let mut state = state();
        let ended = state.map(Notification::TurnCompleted(TurnEnvelope {
            thread_id: Some("thr_child".into()),
            turn: Turn {
                id: "t".into(),
                status: "completed".into(),
                error: None,
            },
        }));
        assert!(ended.is_empty());
        let events = state.map(Notification::ItemCompleted(ItemEnvelope {
            thread_id: Some("thr_child".into()),
            turn_id: None,
            item: ThreadItem::AgentMessage {
                id: "m".into(),
                text: "hi".into(),
            },
        }));
        assert!(matches!(&events[0], AgentEvent::Text { parent: Some(p), .. } if p == "thr_child"));
    }

    #[test]
    fn usage_is_the_difference_and_input_excludes_cached_tokens() {
        let mut state = state();
        state.map(Notification::TokenUsage(
            serde_json::from_value(json!({"tokenUsage": {"total": {"totalTokens": 150, "inputTokens": 100, "cachedInputTokens": 40, "outputTokens": 50, "reasoningOutputTokens": 5}}}))
                .unwrap(),
        ));
        state.begin_turn();
        state.map(Notification::TurnStarted(TurnEnvelope {
            thread_id: None,
            turn: Turn {
                id: "t".into(),
                status: "inProgress".into(),
                error: None,
            },
        }));
        state.map(Notification::TokenUsage(
            serde_json::from_value(json!({"tokenUsage": {"total": {"totalTokens": 400, "inputTokens": 300, "cachedInputTokens": 100, "outputTokens": 100, "reasoningOutputTokens": 25}, "last": {"totalTokens": 180, "inputTokens": 120, "outputTokens": 60}}}))
                .unwrap(),
        ));
        let usage = state.usage();
        assert_eq!(
            usage.input_tokens,
            Some(140),
            "(300-100) - (100-40) cached excluded: 200 - 60"
        );
        assert_eq!(usage.output_tokens, Some(50));
        assert_eq!(usage.cache_read_tokens, Some(60));
        assert_eq!(usage.reasoning_tokens, Some(20));
        assert_eq!(usage.context_tokens, Some(180));
    }

    #[test]
    fn cost_follows_the_configured_basis_and_is_never_reported() {
        let mut config = MapConfig {
            deltas: true,
            cost_basis: CostBasis::Unknown,
            prices: PriceTable::new(),
            login_hint: None,
        };
        let usage = Usage {
            input_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..Usage::default()
        };
        assert_eq!(MapState::new(config.clone()).cost(&usage), Cost::default());
        config.cost_basis = CostBasis::Free;
        assert_eq!(MapState::new(config.clone()).cost(&usage).usd, Some(0.0));
        config.cost_basis = CostBasis::Priced;
        config.prices = PriceTable::new().with(
            "m",
            crate::agent::ModelPrice {
                input_per_mtok: 1.0,
                output_per_mtok: 2.0,
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
        );
        let mut priced = MapState::new(config);
        assert_eq!(priced.cost(&usage).usd, None, "no model yet: no amount");
        priced.set_model(Some("m".into()));
        assert_eq!(
            priced.cost(&usage),
            Cost {
                usd: Some(3.0),
                basis: CostBasis::Priced
            }
        );
    }

    #[test]
    fn answers_are_exact_and_refuse_what_codex_cannot_do() {
        let scopes = [PermissionScope::Once, PermissionScope::Session];
        let (answer, interrupt) = answer_for(
            &AskKind::Elicitation,
            &scopes,
            &PermissionDecision::allow_once(),
        )
        .unwrap();
        assert_eq!(answer, json!({"action": "accept", "content": null}));
        assert!(!interrupt);
        let (cancel, interrupt) = answer_for(
            &AskKind::Elicitation,
            &scopes,
            &PermissionDecision::Deny {
                message: None,
                interrupt: true,
            },
        )
        .unwrap();
        assert_eq!(cancel["action"], "cancel");
        assert!(interrupt);
        let always = PermissionDecision::Allow {
            scope: PermissionScope::Always,
            updated_input: None,
        };
        assert_eq!(
            answer_for(&AskKind::Decision, &scopes, &always).unwrap_err(),
            ProviderError::unsupported("permission_scope")
        );
        let rewritten = PermissionDecision::Allow {
            scope: PermissionScope::Once,
            updated_input: Some(json!({})),
        };
        assert_eq!(
            answer_for(&AskKind::Decision, &scopes, &rewritten).unwrap_err(),
            ProviderError::unsupported("permission_updated_input")
        );
    }
}
