//! Projection of the Claude Code message stream onto [`AgentEvent`]s: the table
//! of `docs/agent-contract.md` §14.1, row by row.
//!
//! [`map_message`] is a pure function of the message and of a small
//! [`MapState`]; it does no I/O, so a host can replay a recorded stream through
//! it. The two rows of the table that do not come from a [`Message`] — the
//! `can_use_tool` control request — are projected by [`permission_event`].

use std::collections::HashMap;

use serde_json::{Value, json};

use super::control::PermissionRequest;
use super::error_map::classify_result_error;
use super::policy_map::{canonical_name, native_to_neutral, tool_category};
use crate::agent::{
    AgentEvent, BackgroundTask, BackgroundTaskKind, BackgroundTaskStatus, CompactionPhase,
    CompactionTrigger, Cost, CostBasis, DeltaKind, McpServerStatus, ModelUsage, PermissionScope,
    QuestionOption, QuestionReply, QuestionSpec, StopReason, TaskPhase, ToolOutput, Usage,
};
use crate::types::{ContentBlock, ContentValue, Message, StreamDelta, StreamEventData};

/// What [`map_message`] remembers from one message to the next.
#[derive(Debug, Clone, PartialEq)]
pub struct MapState {
    seq: u64,
    last_parent: Option<String>,
    interrupt_requested: bool,
    cost_basis: CostBasis,
    /// Tool call opened at a content-block index of the message being streamed,
    /// by `(parent, index)`: what gives a `tool_input` delta its `tool_call_id`.
    open_tool_blocks: HashMap<(Option<String>, usize), String>,
}

impl Default for MapState {
    fn default() -> Self {
        Self::new()
    }
}

impl MapState {
    /// A fresh state: no message seen, costs `reported`.
    pub fn new() -> Self {
        Self::with_cost_basis(CostBasis::Reported)
    }

    /// A fresh state whose `done.cost.basis` is `basis` (an instance on a
    /// subscription, or one whose reported cost must not be trusted).
    pub fn with_cost_basis(basis: CostBasis) -> Self {
        Self {
            seq: 0,
            last_parent: None,
            interrupt_requested: false,
            cost_basis: basis,
            open_tool_blocks: HashMap::new(),
        }
    }

    /// Number of the last message mapped (0 before the first one).
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// `parent_tool_use_id` of the last user, assistant or stream message; what a
    /// permission request arriving on the control channel is attributed to.
    pub fn last_parent(&self) -> Option<&str> {
        self.last_parent.as_deref()
    }

    /// Basis written into `done.cost`.
    pub fn cost_basis(&self) -> CostBasis {
        self.cost_basis
    }

    /// Whether an interruption was requested during the running turn.
    pub fn interrupt_requested(&self) -> bool {
        self.interrupt_requested
    }

    /// Records (or forgets) that an interruption was requested: the next
    /// `error_during_execution` result then maps to `stop_reason: interrupted`.
    /// Cleared by every `result`.
    pub fn set_interrupt_requested(&mut self, requested: bool) {
        self.interrupt_requested = requested;
    }
}

fn str_at<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
}

fn owned_at(value: &Value, keys: &[&str]) -> Option<String> {
    str_at(value, keys).map(str::to_owned)
}

fn u64_at(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
}

/// Maps one message of the CLI to the events it stands for (§14.1).
///
/// Every call numbers the message (`seq`), whatever it maps to; the events of
/// one message share that number.
pub fn map_message(message: &Message, state: &mut MapState) -> Vec<AgentEvent> {
    state.seq += 1;
    let seq = Some(state.seq);
    match message {
        Message::Assistant {
            message,
            parent_tool_use_id,
        } => {
            state.last_parent.clone_from(parent_tool_use_id);
            message
                .content
                .iter()
                .filter_map(|block| map_block(block, seq, parent_tool_use_id, false))
                .collect()
        },
        Message::User {
            message,
            parent_tool_use_id,
        } => {
            state.last_parent.clone_from(parent_tool_use_id);
            match &message.content_blocks {
                Some(blocks) => blocks
                    .iter()
                    .filter_map(|block| map_block(block, seq, parent_tool_use_id, true))
                    .collect(),
                None if message.content.is_empty() => Vec::new(),
                None => vec![AgentEvent::UserEcho {
                    text: message.content.clone(),
                    seq,
                    parent: parent_tool_use_id.clone(),
                }],
            }
        },
        Message::StreamEvent {
            event,
            parent_tool_use_id,
            ..
        } => {
            state.last_parent.clone_from(parent_tool_use_id);
            map_stream_event(event, seq, parent_tool_use_id, state)
        },
        Message::System { subtype, data } => vec![map_system(subtype, data)],
        Message::Result {
            subtype,
            duration_ms,
            duration_api_ms,
            is_error,
            num_turns,
            session_id,
            total_cost_usd,
            usage,
            result,
            structured_output,
        } => {
            let stop_reason = stop_reason(subtype, *is_error, state.interrupt_requested);
            state.interrupt_requested = false;
            state.last_parent = None;
            state.open_tool_blocks.clear();
            let usd = match state.cost_basis {
                CostBasis::Unknown => None,
                CostBasis::Free => Some(0.0),
                _ => *total_cost_usd,
            };
            vec![AgentEvent::Done {
                stop_reason,
                subtype: Some(subtype.clone()),
                is_error: *is_error,
                result_text: result.clone(),
                usage: usage.as_ref().map(map_usage).unwrap_or_default(),
                cost: Cost {
                    usd,
                    basis: state.cost_basis,
                },
                duration_ms: u64::try_from(*duration_ms).unwrap_or(0),
                duration_api_ms: Some(u64::try_from(*duration_api_ms).unwrap_or(0)),
                num_turns: u32::try_from(*num_turns).unwrap_or(0),
                model: None,
                provider_session_id: Some(session_id.clone()),
                structured_output: structured_output.clone(),
                // §7: an Anthropic API failure reported in the result text is
                // classified here, on the `done` that keeps usage and cost.
                error: (*is_error && stop_reason == StopReason::Error)
                    .then(|| result.as_deref().and_then(classify_result_error))
                    .flatten(),
            }]
        },
    }
}

/// `subtype` → `stop_reason` (rule under the table of §14.1).
fn stop_reason(subtype: &str, is_error: bool, interrupt_requested: bool) -> StopReason {
    match subtype {
        "success" => StopReason::Completed,
        "error_max_turns" => StopReason::MaxTurns,
        "error_during_execution" if interrupt_requested => StopReason::Interrupted,
        "error_during_execution" => StopReason::Error,
        "error_max_budget_usd" => StopReason::BudgetExceeded,
        _ if is_error => StopReason::Error,
        _ => StopReason::Completed,
    }
}

/// One content block of an assistant message, or of a user message
/// (`from_user`: a text block is then the echo of what the user said).
fn map_block(
    block: &ContentBlock,
    seq: Option<u64>,
    parent: &Option<String>,
    from_user: bool,
) -> Option<AgentEvent> {
    let parent = parent.clone();
    match block {
        ContentBlock::Text(text) if from_user => Some(AgentEvent::UserEcho {
            text: text.text.clone(),
            seq,
            parent,
        }),
        ContentBlock::Text(text) => Some(AgentEvent::Text {
            text: text.text.clone(),
            seq,
            parent,
        }),
        ContentBlock::Thinking(_) | ContentBlock::ToolUse(_) if from_user => None,
        ContentBlock::Thinking(thinking) => Some(AgentEvent::Thinking {
            text: thinking.thinking.clone(),
            signature: (!thinking.signature.is_empty()).then(|| thinking.signature.clone()),
            seq,
            parent,
        }),
        ContentBlock::ToolUse(tool) => Some(AgentEvent::ToolCall {
            id: tool.id.clone(),
            name: tool.name.clone(),
            input: tool.input.clone(),
            category: tool_category(&tool.name),
            canonical: canonical_name(&tool.name),
            input_complete: true,
            seq,
            parent,
        }),
        ContentBlock::ToolResult(result) => Some(AgentEvent::ToolResult {
            id: result.tool_use_id.clone(),
            output: result.content.as_ref().map(|content| match content {
                ContentValue::Text(text) => ToolOutput::Text(text.clone()),
                ContentValue::Structured(blocks) => ToolOutput::Blocks(blocks.clone()),
            }),
            is_error: result.is_error.unwrap_or(false),
            seq,
            parent,
        }),
    }
}

fn map_stream_event(
    event: &StreamEventData,
    seq: Option<u64>,
    parent: &Option<String>,
    state: &mut MapState,
) -> Vec<AgentEvent> {
    match event {
        StreamEventData::ContentBlockStart {
            index,
            content_block,
        } => {
            let key = (parent.clone(), *index);
            if content_block.get("type").and_then(Value::as_str) != Some("tool_use") {
                state.open_tool_blocks.remove(&key);
                return Vec::new();
            }
            let id = owned_at(content_block, &["id"]).unwrap_or_default();
            let name = owned_at(content_block, &["name"]).unwrap_or_default();
            state.open_tool_blocks.insert(key, id.clone());
            vec![AgentEvent::ToolCall {
                id,
                category: tool_category(&name),
                canonical: canonical_name(&name),
                name,
                input: content_block
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
                input_complete: false,
                seq,
                parent: parent.clone(),
            }]
        },
        StreamEventData::ContentBlockDelta { index, delta } => {
            let (kind, text, tool_call_id) = match delta {
                StreamDelta::TextDelta { text } => (DeltaKind::Text, text, None),
                StreamDelta::ThinkingDelta { thinking } => (DeltaKind::Thinking, thinking, None),
                StreamDelta::InputJsonDelta { partial_json } => (
                    DeltaKind::ToolInput,
                    partial_json,
                    state
                        .open_tool_blocks
                        .get(&(parent.clone(), *index))
                        .cloned(),
                ),
            };
            vec![AgentEvent::Delta {
                kind,
                text: text.clone(),
                index: u32::try_from(*index).ok(),
                tool_call_id,
                parent: parent.clone(),
            }]
        },
        StreamEventData::MessageStart { .. } => {
            // Block indices restart with every message of a given author.
            state
                .open_tool_blocks
                .retain(|(owner, _), _| owner != parent);
            Vec::new()
        },
        StreamEventData::ContentBlockStop { .. }
        | StreamEventData::MessageDelta { .. }
        | StreamEventData::MessageStop => Vec::new(),
    }
}

fn map_system(subtype: &str, data: &Value) -> AgentEvent {
    match subtype {
        "init" => {
            let native_mode = owned_at(data, &["permissionMode", "permission_mode"]);
            AgentEvent::SessionStarted {
                provider_session_id: owned_at(data, &["session_id"]),
                model: owned_at(data, &["model"]),
                policy_mode: native_mode.as_deref().and_then(native_to_neutral),
                native_mode,
                tools: data
                    .get("tools")
                    .and_then(Value::as_array)
                    .map(|tools| {
                        tools
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
                mcp_servers: data
                    .get("mcp_servers")
                    .and_then(Value::as_array)
                    .map(|servers| {
                        servers
                            .iter()
                            .filter_map(|server| {
                                Some(McpServerStatus {
                                    name: owned_at(server, &["name"])?,
                                    status: owned_at(server, &["status"]).unwrap_or_default(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                cwd: owned_at(data, &["cwd"]),
            }
        },
        "compact_boundary" => {
            let metadata = data
                .get("compact_metadata")
                .or_else(|| data.get("compactMetadata"))
                .unwrap_or(&Value::Null);
            AgentEvent::Compaction {
                phase: CompactionPhase::Completed,
                trigger: Some(compaction_trigger(str_at(metadata, &["trigger"]))),
                pre_tokens: u64_at(metadata, &["pre_tokens", "preTokens"]),
            }
        },
        "task_started" | "task_progress" | "task_updated" | "task_notification" => {
            let phase = match subtype {
                "task_started" => TaskPhase::Started,
                "task_progress" => TaskPhase::Progress,
                "task_updated" => TaskPhase::Updated,
                _ => TaskPhase::Notification,
            };
            AgentEvent::TaskUpdate {
                phase,
                task_id: owned_at(data, &["task_id"]),
                tool_call_id: owned_at(data, &["tool_use_id"]),
                description: owned_at(data, &["description"]),
                status: owned_at(data, &["status"]),
                summary: owned_at(data, &["summary"]),
                event_id: owned_at(data, &["uuid"]),
                data: data.clone(),
            }
        },
        "background_tasks_changed" => AgentEvent::BackgroundTasks {
            tasks: data
                .get("tasks")
                .and_then(Value::as_array)
                .map(|tasks| tasks.iter().filter_map(background_task).collect())
                .unwrap_or_default(),
        },
        other => AgentEvent::ProviderNotice {
            kind: other.to_owned(),
            data: data.clone(),
        },
    }
}

/// `"manual"` is manual; anything else, or nothing, is automatic.
pub(crate) fn compaction_trigger(trigger: Option<&str>) -> CompactionTrigger {
    match trigger {
        Some("manual") => CompactionTrigger::Manual,
        _ => CompactionTrigger::Auto,
    }
}

/// Lenient reading of one background task; `None` when it has no identifier.
fn background_task(task: &Value) -> Option<BackgroundTask> {
    let kind = match str_at(task, &["type", "kind"]).unwrap_or_default() {
        "shell" | "bash" | "local_bash" => BackgroundTaskKind::Shell,
        "monitor" => BackgroundTaskKind::Monitor,
        "agent" | "subagent" | "task" | "local_agent" => BackgroundTaskKind::Agent,
        _ => BackgroundTaskKind::Other,
    };
    let status = match str_at(task, &["status"]).unwrap_or_default() {
        "completed" | "done" | "success" => BackgroundTaskStatus::Completed,
        "failed" | "error" => BackgroundTaskStatus::Failed,
        "killed" | "cancelled" | "canceled" | "stopped" => BackgroundTaskStatus::Killed,
        _ => BackgroundTaskStatus::Running,
    };
    Some(BackgroundTask {
        id: owned_at(task, &["id", "task_id"])?,
        kind,
        description: owned_at(task, &["description", "command"]).unwrap_or_default(),
        status,
        started_at_ms: u64_at(task, &["started_at_ms", "startedAtMs"]),
        tool_call_id: owned_at(task, &["tool_use_id", "toolUseId"]),
        parent: owned_at(task, &["parent_tool_use_id"]),
        pid: u64_at(task, &["pid"]).and_then(|pid| u32::try_from(pid).ok()),
    })
}

/// `result.usage` → [`Usage`]. A counter the CLI did not give stays `None`.
fn map_usage(usage: &Value) -> Usage {
    let by_model = usage
        .get("modelUsage")
        .or_else(|| usage.get("model_usage"))
        .and_then(Value::as_object)
        .map(|models| {
            models
                .iter()
                .map(|(model, counters)| ModelUsage {
                    model: model.clone(),
                    input_tokens: u64_at(counters, &["inputTokens", "input_tokens"]),
                    output_tokens: u64_at(counters, &["outputTokens", "output_tokens"]),
                    cache_read_tokens: u64_at(
                        counters,
                        &["cacheReadInputTokens", "cache_read_input_tokens"],
                    ),
                    cache_creation_tokens: u64_at(
                        counters,
                        &["cacheCreationInputTokens", "cache_creation_input_tokens"],
                    ),
                    cost_usd: counters
                        .get("costUSD")
                        .or_else(|| counters.get("cost_usd"))
                        .and_then(Value::as_f64),
                    context_window: u64_at(counters, &["contextWindow", "context_window"]),
                })
                .collect()
        })
        .unwrap_or_default();
    Usage {
        input_tokens: u64_at(usage, &["input_tokens"]),
        output_tokens: u64_at(usage, &["output_tokens"]),
        cache_read_tokens: u64_at(usage, &["cache_read_input_tokens"]),
        cache_creation_tokens: u64_at(usage, &["cache_creation_input_tokens"]),
        reasoning_tokens: None,
        context_tokens: None,
        by_model,
    }
}

/// Lenient reading of `AskUserQuestion`'s `questions`.
fn question_specs(input: &Value) -> Vec<QuestionSpec> {
    input
        .get("questions")
        .and_then(Value::as_array)
        .map(|questions| {
            questions
                .iter()
                .filter_map(|question| {
                    Some(QuestionSpec {
                        question: owned_at(question, &["question"])?,
                        header: owned_at(question, &["header"]),
                        options: question
                            .get("options")
                            .and_then(Value::as_array)
                            .map(|options| {
                                options
                                    .iter()
                                    .filter_map(|option| {
                                        // An option is an object, or a bare label.
                                        let label = owned_at(option, &["label"])
                                            .or_else(|| option.as_str().map(str::to_owned))?;
                                        Some(QuestionOption {
                                            label,
                                            description: owned_at(option, &["description"]),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        multi_select: question
                            .get("multiSelect")
                            .or_else(|| question.get("multi_select"))
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Projects a `can_use_tool` control request (§14.1, the two control rows):
/// `question { reply: turn }` for the CLI's question tool, `permission_ask`
/// otherwise. `parent` is [`MapState::last_parent`].
pub fn permission_event(request: &PermissionRequest, state: &MapState) -> AgentEvent {
    let parent = state.last_parent.clone();
    if request.is_question() {
        return AgentEvent::Question {
            question_id: request.request_id.clone(),
            tool_call_id: request.tool_use_id.clone(),
            reply: QuestionReply::Turn,
            questions: question_specs(&request.input),
            input: request.input.clone(),
            parent,
        };
    }
    AgentEvent::PermissionAsk {
        request_id: request.request_id.clone(),
        tool_name: request.tool_name.clone(),
        input: request.input.clone(),
        category: tool_category(&request.tool_name),
        canonical: canonical_name(&request.tool_name),
        tool_call_id: request.tool_use_id.clone(),
        scopes: vec![
            PermissionScope::Once,
            PermissionScope::Session,
            PermissionScope::Always,
        ],
        parent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{PolicyMode, ToolCategory};

    fn message(value: Value) -> Message {
        crate::message_parser::parse_message(value)
            .expect("the message parses")
            .expect("the message is one the SDK keeps")
    }

    fn assistant(content: Value, parent: Option<&str>) -> Message {
        let mut value = json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": content},
        });
        if let Some(parent) = parent {
            value["parent_tool_use_id"] = json!(parent);
        }
        message(value)
    }

    fn stream(event: Value, parent: Option<&str>) -> Message {
        let mut value = json!({"type": "stream_event", "session_id": "s", "event": event});
        if let Some(parent) = parent {
            value["parent_tool_use_id"] = json!(parent);
        }
        message(value)
    }

    fn system(subtype: &str, mut payload: Value) -> Message {
        payload["type"] = json!("system");
        payload["subtype"] = json!(subtype);
        message(payload)
    }

    fn one(message: &Message, state: &mut MapState) -> AgentEvent {
        let mut events = map_message(message, state);
        assert_eq!(events.len(), 1, "{events:?}");
        events.remove(0)
    }

    // ----- Assistant ---------------------------------------------------------

    #[test]
    fn assistant_text_maps_to_text_with_seq_and_parent() {
        let mut state = MapState::new();
        let event = one(
            &assistant(json!([{"type": "text", "text": "bonjour"}]), None),
            &mut state,
        );
        assert_eq!(
            event,
            AgentEvent::Text {
                text: "bonjour".into(),
                seq: Some(1),
                parent: None
            }
        );
        let nested = one(
            &assistant(
                json!([{"type": "text", "text": "child"}]),
                Some("toolu_task"),
            ),
            &mut state,
        );
        assert_eq!(
            nested,
            AgentEvent::Text {
                text: "child".into(),
                seq: Some(2),
                parent: Some("toolu_task".into())
            }
        );
        assert_eq!(state.last_parent(), Some("toolu_task"));
        assert_eq!(state.seq(), 2);
    }

    #[test]
    fn assistant_thinking_maps_to_thinking_and_an_empty_signature_is_none() {
        let mut state = MapState::new();
        assert_eq!(
            one(
                &assistant(
                    json!([{"type": "thinking", "thinking": "hmm", "signature": "sig"}]),
                    None
                ),
                &mut state
            ),
            AgentEvent::Thinking {
                text: "hmm".into(),
                signature: Some("sig".into()),
                seq: Some(1),
                parent: None
            }
        );
        assert_eq!(
            one(
                &assistant(
                    json!([{"type": "thinking", "thinking": "hmm", "signature": ""}]),
                    None
                ),
                &mut state
            ),
            AgentEvent::Thinking {
                text: "hmm".into(),
                signature: None,
                seq: Some(2),
                parent: None
            }
        );
    }

    #[test]
    fn assistant_tool_use_maps_to_a_complete_tool_call() {
        let mut state = MapState::new();
        assert_eq!(
            one(
                &assistant(
                    json!([{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}}]),
                    None
                ),
                &mut state
            ),
            AgentEvent::ToolCall {
                id: "toolu_1".into(),
                name: "Bash".into(),
                input: json!({"command": "ls"}),
                category: ToolCategory::Command,
                canonical: Some("Bash".into()),
                input_complete: true,
                seq: Some(1),
                parent: None
            }
        );
    }

    #[test]
    fn the_blocks_of_one_message_share_its_seq_and_keep_their_order() {
        let mut state = MapState::new();
        let events = map_message(
            &assistant(
                json!([
                    {"type": "thinking", "thinking": "t", "signature": "s"},
                    {"type": "text", "text": "a"},
                    {"type": "tool_use", "id": "toolu_1", "name": "mcp__po__plan", "input": {}},
                ]),
                None,
            ),
            &mut state,
        );
        assert_eq!(
            events.iter().map(AgentEvent::type_name).collect::<Vec<_>>(),
            ["thinking", "text", "tool_call"]
        );
        assert!(matches!(
            &events[2],
            AgentEvent::ToolCall { category: ToolCategory::Mcp, canonical: Some(name), seq: Some(1), .. }
                if name == "mcp__po__plan"
        ));
    }

    // ----- Tool results and user messages -------------------------------------

    #[test]
    fn a_tool_result_maps_from_a_user_message_text_structured_or_absent() {
        let mut state = MapState::new();
        let user = |content: Value| {
            message(json!({"type": "user", "message": {"role": "user", "content": content}}))
        };
        assert_eq!(
            one(
                &user(json!([{"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}])),
                &mut state
            ),
            AgentEvent::ToolResult {
                id: "toolu_1".into(),
                output: Some(ToolOutput::Text("ok".into())),
                is_error: false,
                seq: Some(1),
                parent: None
            }
        );
        assert_eq!(
            one(
                &user(json!([{
                    "type": "tool_result", "tool_use_id": "toolu_2",
                    "content": [{"type": "text", "text": "boom"}], "is_error": true
                }])),
                &mut state
            ),
            AgentEvent::ToolResult {
                id: "toolu_2".into(),
                output: Some(ToolOutput::Blocks(vec![
                    json!({"type": "text", "text": "boom"})
                ])),
                is_error: true,
                seq: Some(2),
                parent: None
            }
        );
        assert_eq!(
            one(
                &user(json!([{"type": "tool_result", "tool_use_id": "toolu_3"}])),
                &mut state
            ),
            AgentEvent::ToolResult {
                id: "toolu_3".into(),
                output: None,
                is_error: false,
                seq: Some(3),
                parent: None
            }
        );
    }

    #[test]
    fn a_tool_result_maps_from_an_assistant_message_too() {
        let mut state = MapState::new();
        assert!(matches!(
            one(
                &assistant(
                    json!([{"type": "tool_result", "tool_use_id": "toolu_9", "content": "x"}]),
                    Some("toolu_task")
                ),
                &mut state
            ),
            AgentEvent::ToolResult { id, parent: Some(parent), .. }
                if id == "toolu_9" && parent == "toolu_task"
        ));
    }

    #[test]
    fn a_plain_user_message_maps_to_user_echo() {
        let mut state = MapState::new();
        assert_eq!(
            one(
                &message(json!({"type": "user", "message": {"role": "user", "content": "salut"}})),
                &mut state
            ),
            AgentEvent::UserEcho {
                text: "salut".into(),
                seq: Some(1),
                parent: None
            }
        );
        // A text block inside an array content is an echo as well.
        assert_eq!(
            one(
                &message(json!({
                    "type": "user",
                    "message": {"role": "user", "content": [{"type": "text", "text": "bloc"}]}
                })),
                &mut state
            ),
            AgentEvent::UserEcho {
                text: "bloc".into(),
                seq: Some(2),
                parent: None
            }
        );
        // An empty user message stands for nothing, but is still numbered.
        assert!(
            map_message(
                &message(json!({"type": "user", "message": {"role": "user", "content": []}})),
                &mut state
            )
            .is_empty()
        );
        assert_eq!(state.seq(), 3);
    }

    // ----- Stream events ------------------------------------------------------

    #[test]
    fn a_tool_use_block_start_maps_to_an_incomplete_tool_call() {
        let mut state = MapState::new();
        assert_eq!(
            one(
                &stream(
                    json!({"type": "content_block_start", "index": 1,
                           "content_block": {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}}}),
                    None
                ),
                &mut state
            ),
            AgentEvent::ToolCall {
                id: "toolu_1".into(),
                name: "Read".into(),
                input: json!({}),
                category: ToolCategory::Read,
                canonical: Some("Read".into()),
                input_complete: false,
                seq: Some(1),
                parent: None
            }
        );
        // Defaults of the table: empty id and name, `{}` input.
        assert_eq!(
            one(
                &stream(
                    json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use"}}),
                    None
                ),
                &mut state
            ),
            AgentEvent::ToolCall {
                id: String::new(),
                name: String::new(),
                input: json!({}),
                category: ToolCategory::Other,
                canonical: None,
                input_complete: false,
                seq: Some(2),
                parent: None
            }
        );
    }

    #[test]
    fn a_text_delta_maps_to_a_text_delta() {
        let mut state = MapState::new();
        assert_eq!(
            one(
                &stream(
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "bon"}}),
                    Some("toolu_task")
                ),
                &mut state
            ),
            AgentEvent::Delta {
                kind: DeltaKind::Text,
                text: "bon".into(),
                index: Some(0),
                tool_call_id: None,
                parent: Some("toolu_task".into())
            }
        );
    }

    #[test]
    fn a_thinking_delta_maps_to_a_thinking_delta() {
        let mut state = MapState::new();
        assert_eq!(
            one(
                &stream(
                    json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hm"}}),
                    None
                ),
                &mut state
            ),
            AgentEvent::Delta {
                kind: DeltaKind::Thinking,
                text: "hm".into(),
                index: Some(0),
                tool_call_id: None,
                parent: None
            }
        );
    }

    #[test]
    fn an_input_json_delta_carries_the_tool_call_of_its_block_index() {
        let mut state = MapState::new();
        let start = |index: u64, id: &str| {
            stream(
                json!({"type": "content_block_start", "index": index,
                       "content_block": {"type": "tool_use", "id": id, "name": "Bash"}}),
                None,
            )
        };
        let delta = |index: u64| {
            stream(
                json!({"type": "content_block_delta", "index": index,
                       "delta": {"type": "input_json_delta", "partial_json": "{\"c"}}),
                None,
            )
        };
        map_message(&start(1, "toolu_a"), &mut state);
        map_message(&start(2, "toolu_b"), &mut state);
        assert_eq!(
            one(&delta(2), &mut state),
            AgentEvent::Delta {
                kind: DeltaKind::ToolInput,
                text: "{\"c".into(),
                index: Some(2),
                tool_call_id: Some("toolu_b".into()),
                parent: None
            }
        );
        assert!(matches!(
            one(&delta(1), &mut state),
            AgentEvent::Delta { tool_call_id: Some(id), .. } if id == "toolu_a"
        ));
        // A new message restarts the block indices: index 1 is no longer toolu_a.
        map_message(
            &stream(json!({"type": "message_start", "message": {}}), None),
            &mut state,
        );
        assert!(matches!(
            one(&delta(1), &mut state),
            AgentEvent::Delta {
                tool_call_id: None,
                ..
            }
        ));
    }

    #[test]
    fn the_other_stream_events_map_to_nothing() {
        let mut state = MapState::new();
        for event in [
            json!({"type": "message_start", "message": {"id": "m"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_stop"}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        ] {
            assert!(
                map_message(&stream(event.clone(), None), &mut state).is_empty(),
                "{event}"
            );
        }
        assert_eq!(state.seq(), 5, "every message is numbered");
    }

    // ----- System -------------------------------------------------------------

    #[test]
    fn init_maps_to_session_started_with_native_and_neutral_mode() {
        let mut state = MapState::new();
        assert_eq!(
            one(
                &system(
                    "init",
                    json!({
                        "session_id": "sess-1", "model": "claude-opus-4", "cwd": "/work",
                        "tools": ["Bash", "Read", 7],
                        "mcp_servers": [{"name": "po", "status": "connected"}, {"status": "no-name"}],
                        "permissionMode": "acceptEdits",
                    })
                ),
                &mut state
            ),
            AgentEvent::SessionStarted {
                provider_session_id: Some("sess-1".into()),
                model: Some("claude-opus-4".into()),
                policy_mode: Some(PolicyMode::AutoEdits),
                native_mode: Some("acceptEdits".into()),
                tools: vec!["Bash".into(), "Read".into()],
                mcp_servers: vec![McpServerStatus {
                    name: "po".into(),
                    status: "connected".into()
                }],
                cwd: Some("/work".into()),
            }
        );
        // A mode the table does not know keeps its native name and has no neutral one.
        assert!(matches!(
            one(&system("init", json!({"permissionMode": "yolo"})), &mut state),
            AgentEvent::SessionStarted { policy_mode: None, native_mode: Some(native), .. }
                if native == "yolo"
        ));
    }

    #[test]
    fn compact_boundary_maps_to_compaction_completed() {
        let mut state = MapState::new();
        assert_eq!(
            one(
                &system(
                    "compact_boundary",
                    json!({"compact_metadata": {"trigger": "manual", "pre_tokens": 154_000}})
                ),
                &mut state
            ),
            AgentEvent::Compaction {
                phase: CompactionPhase::Completed,
                trigger: Some(CompactionTrigger::Manual),
                pre_tokens: Some(154_000)
            }
        );
        // No metadata: the trigger defaults to auto.
        assert_eq!(
            one(&system("compact_boundary", json!({})), &mut state),
            AgentEvent::Compaction {
                phase: CompactionPhase::Completed,
                trigger: Some(CompactionTrigger::Auto),
                pre_tokens: None
            }
        );
    }

    #[test]
    fn task_messages_map_to_task_update() {
        let mut state = MapState::new();
        for (subtype, phase) in [
            ("task_started", TaskPhase::Started),
            ("task_progress", TaskPhase::Progress),
            ("task_updated", TaskPhase::Updated),
            ("task_notification", TaskPhase::Notification),
        ] {
            let payload = json!({
                "task_id": "t1", "tool_use_id": "toolu_task", "description": "explore",
                "status": "running", "summary": "half way", "uuid": "evt-1",
                "workflow_progress": [1, 2],
            });
            assert_eq!(
                one(&system(subtype, payload.clone()), &mut state),
                AgentEvent::TaskUpdate {
                    phase,
                    task_id: Some("t1".into()),
                    tool_call_id: Some("toolu_task".into()),
                    description: Some("explore".into()),
                    status: Some("running".into()),
                    summary: Some("half way".into()),
                    event_id: Some("evt-1".into()),
                    data: payload,
                },
                "{subtype}"
            );
        }
    }

    #[test]
    fn background_tasks_changed_maps_to_a_snapshot_read_leniently() {
        let mut state = MapState::new();
        assert_eq!(
            one(
                &system(
                    "background_tasks_changed",
                    json!({"tasks": [
                        {"id": "b1", "type": "shell", "command": "npm run dev", "status": "running", "pid": 4242},
                        {"task_id": "b2", "kind": "agent", "description": "review", "status": "completed",
                         "tool_use_id": "toolu_7"},
                        {"id": "b3", "type": "martian", "status": "killed"},
                        {"description": "no identifier: skipped"},
                    ]})
                ),
                &mut state
            ),
            AgentEvent::BackgroundTasks {
                tasks: vec![
                    BackgroundTask {
                        id: "b1".into(),
                        kind: BackgroundTaskKind::Shell,
                        description: "npm run dev".into(),
                        status: BackgroundTaskStatus::Running,
                        started_at_ms: None,
                        tool_call_id: None,
                        parent: None,
                        pid: Some(4242),
                    },
                    BackgroundTask {
                        id: "b2".into(),
                        kind: BackgroundTaskKind::Agent,
                        description: "review".into(),
                        status: BackgroundTaskStatus::Completed,
                        started_at_ms: None,
                        tool_call_id: Some("toolu_7".into()),
                        parent: None,
                        pid: None,
                    },
                    BackgroundTask {
                        id: "b3".into(),
                        kind: BackgroundTaskKind::Other,
                        description: String::new(),
                        status: BackgroundTaskStatus::Killed,
                        started_at_ms: None,
                        tool_call_id: None,
                        parent: None,
                        pid: None,
                    },
                ]
            }
        );
        assert_eq!(
            one(&system("background_tasks_changed", json!({})), &mut state),
            AgentEvent::BackgroundTasks { tasks: vec![] }
        );
    }

    #[test]
    fn any_other_system_message_maps_to_a_provider_notice() {
        let mut state = MapState::new();
        assert_eq!(
            one(
                &system("status", json!({"status": "compacting"})),
                &mut state
            ),
            AgentEvent::ProviderNotice {
                kind: "status".into(),
                data: json!({"status": "compacting"})
            }
        );
        assert!(matches!(
            one(&system("brand_new_subtype", json!({"x": 1})), &mut state),
            AgentEvent::ProviderNotice { kind, .. } if kind == "brand_new_subtype"
        ));
    }

    // ----- Result -------------------------------------------------------------

    fn result(subtype: &str, is_error: bool) -> Message {
        message(json!({
            "type": "result", "subtype": subtype, "duration_ms": 812, "duration_api_ms": 640,
            "is_error": is_error, "num_turns": 3, "session_id": "sess-1",
            "total_cost_usd": 0.0421, "result": "fini",
            "usage": {
                "input_tokens": 12, "output_tokens": 3,
                "cache_read_input_tokens": 100, "cache_creation_input_tokens": 7,
                "modelUsage": {
                    "claude-opus-4": {
                        "inputTokens": 10, "outputTokens": 2, "cacheReadInputTokens": 90,
                        "cacheCreationInputTokens": 5, "costUSD": 0.04, "contextWindow": 200_000
                    }
                }
            },
            "structured_output": {"answer": 42},
        }))
    }

    #[test]
    fn result_maps_to_done_and_no_field_is_lost() {
        let mut state = MapState::new();
        assert_eq!(
            one(&result("success", false), &mut state),
            AgentEvent::Done {
                stop_reason: StopReason::Completed,
                subtype: Some("success".into()),
                is_error: false,
                result_text: Some("fini".into()),
                usage: Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(3),
                    cache_read_tokens: Some(100),
                    cache_creation_tokens: Some(7),
                    reasoning_tokens: None,
                    context_tokens: None,
                    by_model: vec![ModelUsage {
                        model: "claude-opus-4".into(),
                        input_tokens: Some(10),
                        output_tokens: Some(2),
                        cache_read_tokens: Some(90),
                        cache_creation_tokens: Some(5),
                        cost_usd: Some(0.04),
                        context_window: Some(200_000),
                    }],
                },
                cost: Cost {
                    usd: Some(0.0421),
                    basis: CostBasis::Reported
                },
                duration_ms: 812,
                duration_api_ms: Some(640),
                num_turns: 3,
                model: None,
                provider_session_id: Some("sess-1".into()),
                structured_output: Some(json!({"answer": 42})),
                error: None,
            }
        );
    }

    #[test]
    fn a_result_without_usage_keeps_its_counters_unknown() {
        let mut state = MapState::new();
        let bare = message(json!({
            "type": "result", "subtype": "success", "duration_ms": 1, "duration_api_ms": 1,
            "is_error": false, "num_turns": 1, "session_id": "s"
        }));
        let AgentEvent::Done {
            usage,
            cost,
            result_text,
            structured_output,
            ..
        } = one(&bare, &mut state)
        else {
            panic!("a result maps to done");
        };
        assert_eq!(usage, Usage::default());
        assert_eq!(usage.input_tokens, None, "unknown is not zero");
        assert_eq!(
            cost,
            Cost {
                usd: None,
                basis: CostBasis::Reported
            }
        );
        assert_eq!(result_text, None);
        assert_eq!(structured_output, None);
    }

    #[test]
    fn the_stop_reason_follows_the_rule_under_the_table() {
        let stop = |subtype: &str, is_error: bool, interrupted: bool| {
            let mut state = MapState::new();
            state.set_interrupt_requested(interrupted);
            match one(&result(subtype, is_error), &mut state) {
                AgentEvent::Done { stop_reason, .. } => {
                    assert!(
                        !state.interrupt_requested(),
                        "a result clears the interruption flag"
                    );
                    stop_reason
                },
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(stop("success", false, false), StopReason::Completed);
        assert_eq!(stop("error_max_turns", true, false), StopReason::MaxTurns);
        assert_eq!(
            stop("error_during_execution", true, true),
            StopReason::Interrupted
        );
        assert_eq!(
            stop("error_during_execution", true, false),
            StopReason::Error
        );
        assert_eq!(
            stop("error_max_budget_usd", true, false),
            StopReason::BudgetExceeded
        );
        assert_eq!(stop("error_something_new", true, false), StopReason::Error);
        assert_eq!(stop("something_new", false, false), StopReason::Completed);
        // An interruption request does not rewrite a turn that completed anyway.
        assert_eq!(stop("success", false, true), StopReason::Completed);
    }

    #[test]
    fn the_cost_basis_comes_from_the_state() {
        let cost = |basis| {
            let mut state = MapState::with_cost_basis(basis);
            assert_eq!(state.cost_basis(), basis);
            match one(&result("success", false), &mut state) {
                AgentEvent::Done { cost, .. } => cost,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            cost(CostBasis::Subscription),
            Cost {
                usd: Some(0.0421),
                basis: CostBasis::Subscription
            }
        );
        // Unknown: never the CLI's figure for a model it cannot price.
        assert_eq!(
            cost(CostBasis::Unknown),
            Cost {
                usd: None,
                basis: CostBasis::Unknown
            }
        );
        assert_eq!(cost(CostBasis::Free), Cost::free());
    }

    #[test]
    fn a_result_resets_the_parent() {
        let mut state = MapState::new();
        map_message(
            &assistant(json!([{"type": "text", "text": "x"}]), Some("toolu_task")),
            &mut state,
        );
        assert_eq!(state.last_parent(), Some("toolu_task"));
        map_message(&result("success", false), &mut state);
        assert_eq!(state.last_parent(), None);
    }

    // ----- Control channel ----------------------------------------------------

    #[test]
    fn can_use_tool_maps_to_permission_ask_with_the_last_parent() {
        let mut state = MapState::new();
        map_message(
            &assistant(json!([{"type": "text", "text": "x"}]), Some("toolu_task")),
            &mut state,
        );
        let request = PermissionRequest {
            request_id: "req-1".into(),
            tool_name: "Bash".into(),
            input: json!({"command": "rm -rf build"}),
            tool_use_id: Some("toolu_1".into()),
            permission_suggestions: None,
        };
        assert_eq!(
            permission_event(&request, &state),
            AgentEvent::PermissionAsk {
                request_id: "req-1".into(),
                tool_name: "Bash".into(),
                input: json!({"command": "rm -rf build"}),
                category: ToolCategory::Command,
                canonical: Some("Bash".into()),
                tool_call_id: Some("toolu_1".into()),
                scopes: vec![
                    PermissionScope::Once,
                    PermissionScope::Session,
                    PermissionScope::Always
                ],
                parent: Some("toolu_task".into()),
            }
        );
    }

    #[test]
    fn ask_user_question_maps_to_a_question_answered_by_a_turn() {
        let state = MapState::new();
        let input = json!({"questions": [
            {"question": "Which one?", "header": "Choice", "multiSelect": true,
             "options": [{"label": "A", "description": "first"}, "B", {"no": "label"}]},
            {"header": "no question: skipped"},
        ]});
        let request = PermissionRequest {
            request_id: "req-q".into(),
            tool_name: "AskUserQuestion".into(),
            input: input.clone(),
            tool_use_id: Some("toolu_q".into()),
            permission_suggestions: None,
        };
        assert_eq!(
            permission_event(&request, &state),
            AgentEvent::Question {
                question_id: "req-q".into(),
                tool_call_id: Some("toolu_q".into()),
                reply: QuestionReply::Turn,
                questions: vec![QuestionSpec {
                    question: "Which one?".into(),
                    header: Some("Choice".into()),
                    options: vec![
                        QuestionOption {
                            label: "A".into(),
                            description: Some("first".into())
                        },
                        QuestionOption {
                            label: "B".into(),
                            description: None
                        },
                    ],
                    multi_select: true,
                }],
                input,
                parent: None,
            }
        );
    }

    #[test]
    fn every_mapped_event_survives_a_json_round_trip() {
        let mut state = MapState::new();
        let messages = [
            assistant(
                json!([{"type": "tool_use", "id": "t", "name": "Bash", "input": {}}]),
                None,
            ),
            system("init", json!({"session_id": "s", "permissionMode": "plan"})),
            system("task_started", json!({"task_id": "t1"})),
            result("error_max_turns", true),
        ];
        for message in &messages {
            for event in map_message(message, &mut state) {
                let back: AgentEvent =
                    serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
                assert_eq!(back, event);
            }
        }
    }
}
