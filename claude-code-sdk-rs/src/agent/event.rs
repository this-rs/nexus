//! Provider-neutral events of an agent session (contract §4, decision A5).
//!
//! Field by field correspondence with the Claude Code `Message` stream and with
//! the backend's `ChatEvent` is in `docs/agent-contract.md` §14.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::capabilities::{CostBasis, PermissionScope};
use super::error::ProviderError;
use super::policy::{PolicyMode, ToolCategory};

/// One event of an agent session.
///
/// A turn stream ends with exactly one terminal event: [`AgentEvent::Done`] or
/// [`AgentEvent::Error`]. Consumers must keep a wildcard arm: the enum is
/// `#[non_exhaustive]` and an unknown event is ignored, never a panic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AgentEvent {
    /// The provider session exists. At most once per provider process.
    SessionStarted {
        /// Provider's own session identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_session_id: Option<String>,
        /// Model in effect.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Policy mode in effect, in neutral terms.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy_mode: Option<PolicyMode>,
        /// Policy mode in the provider's own vocabulary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native_mode: Option<String>,
        /// Tools the provider exposes to the model.
        #[serde(default)]
        tools: Vec<String>,
        /// MCP servers attached to the session and their state.
        #[serde(default)]
        mcp_servers: Vec<McpServerStatus>,
        /// Working directory reported by the provider.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// A user message echoed back by the provider (replay, or out-of-turn input).
    UserEcho {
        /// Text of the message.
        text: String,
        /// Provider message number this event comes from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
        /// Parent tool call when the event comes from a sub-agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    /// Assistant text, complete.
    Text {
        /// The text.
        text: String,
        /// Provider message number this event comes from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
        /// Parent tool call when the event comes from a sub-agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    /// Reasoning, complete.
    Thinking {
        /// The reasoning text.
        text: String,
        /// Provider signature, when the provider signs reasoning blocks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        /// Provider message number this event comes from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
        /// Parent tool call when the event comes from a sub-agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    /// Streaming progress. A hint only: the complete `text`, `thinking` or
    /// `tool_call` always follows.
    Delta {
        /// What is being streamed.
        kind: DeltaKind,
        /// The fragment.
        text: String,
        /// Index of the content block inside the provider message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<u32>,
        /// Tool call the fragment belongs to (for `tool_input`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        /// Parent tool call when the event comes from a sub-agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    /// The model calls a tool. May be emitted twice for one `id`: first with
    /// `input_complete: false` (block start while streaming), then complete.
    ToolCall {
        /// Identifier of the call.
        id: String,
        /// Tool name as the provider spells it.
        name: String,
        /// Tool input.
        input: Value,
        /// Category, supplied by the adapter.
        #[serde(default)]
        category: ToolCategory,
        /// Stable alias for rendering and patterns (`mcp__<server>__<tool>` for MCP).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        canonical: Option<String>,
        /// Whether `input` is the whole input.
        input_complete: bool,
        /// Provider message number this event comes from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
        /// Parent tool call when the event comes from a sub-agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    /// Result of a tool call.
    ToolResult {
        /// Identifier of the `tool_call` this answers.
        id: String,
        /// Output; `None` when the provider gave none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<ToolOutput>,
        /// Whether the tool failed.
        #[serde(default)]
        is_error: bool,
        /// Provider message number this event comes from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
        /// Parent tool call when the event comes from a sub-agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    /// The provider asks whether a tool may run. Answer with `answer_permission`.
    PermissionAsk {
        /// Identifier to answer with.
        request_id: String,
        /// Tool name as the provider spells it.
        tool_name: String,
        /// Tool input.
        input: Value,
        /// Category, supplied by the adapter.
        #[serde(default)]
        category: ToolCategory,
        /// Stable alias of the tool.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        canonical: Option<String>,
        /// The `tool_call` concerned, when the provider says.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        /// Scopes the answer may carry.
        #[serde(default)]
        scopes: Vec<PermissionScope>,
        /// Parent tool call when the request comes from a sub-agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    /// The provider asks the user a question.
    Question {
        /// Identifier of the question.
        question_id: String,
        /// The `tool_call` concerned, when the provider says.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        /// How to answer: by a regular turn, or by `answer_question`.
        reply: QuestionReply,
        /// The questions, parsed leniently.
        #[serde(default)]
        questions: Vec<QuestionSpec>,
        /// The provider's raw input for the question tool.
        #[serde(default)]
        input: Value,
        /// Parent tool call when the question comes from a sub-agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    /// Context compaction.
    Compaction {
        /// Started or completed.
        phase: CompactionPhase,
        /// What triggered it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<CompactionTrigger>,
        /// Tokens in context before compaction.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pre_tokens: Option<u64>,
    },
    /// Full snapshot of the provider's background tasks.
    BackgroundTasks {
        /// Every task currently known; a task absent from the list is gone.
        tasks: Vec<BackgroundTask>,
    },
    /// Progress of a long-running task (sub-agent, workflow).
    TaskUpdate {
        /// Which step of the task's life this is.
        phase: TaskPhase,
        /// Provider identifier of the task.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        /// The `tool_call` that started the task.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        /// Human description.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Provider status string.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        /// Summary or last message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// Unique identifier of this update, for de-duplication.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_id: Option<String>,
        /// The provider's payload, forwarded for display.
        #[serde(default)]
        data: Value,
    },
    /// The provider changed model on its own.
    ModelChanged {
        /// Model now in effect.
        model: String,
    },
    /// The provider changed policy mode on its own.
    PolicyModeChanged {
        /// Mode now in effect, in neutral terms.
        mode: PolicyMode,
        /// Mode in the provider's own vocabulary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native_mode: Option<String>,
    },
    /// End of a turn. Terminal.
    Done {
        /// Why the turn ended, in neutral terms.
        stop_reason: StopReason,
        /// Provider's own end-of-turn label (`success`, `error_max_turns`…).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subtype: Option<String>,
        /// Whether the turn ended in error.
        #[serde(default)]
        is_error: bool,
        /// Final text of the turn.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result_text: Option<String>,
        /// Token usage of the turn.
        #[serde(default)]
        usage: Usage,
        /// Cost of the turn and where the figure comes from.
        #[serde(default)]
        cost: Cost,
        /// Wall-clock duration in milliseconds.
        #[serde(default)]
        duration_ms: u64,
        /// Time spent in model API calls, in milliseconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_api_ms: Option<u64>,
        /// Number of model round-trips in the turn.
        #[serde(default)]
        num_turns: u32,
        /// Model that actually answered.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Provider's own session identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_session_id: Option<String>,
        /// Structured output, when one was requested.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        structured_output: Option<Value>,
    },
    /// The turn (or the session) failed. Terminal.
    Error {
        /// What went wrong.
        error: ProviderError,
    },
    /// Provider-specific diagnostic. Nothing a user sees for Claude Code today
    /// travels through it (A5).
    ProviderNotice {
        /// Provider's label for the notice.
        kind: String,
        /// Payload, opaque to the contract.
        #[serde(default)]
        data: Value,
    },
}

impl AgentEvent {
    /// Whether this event ends a turn stream.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Error { .. })
    }

    /// The serialised `type` tag.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::SessionStarted { .. } => "session_started",
            Self::UserEcho { .. } => "user_echo",
            Self::Text { .. } => "text",
            Self::Thinking { .. } => "thinking",
            Self::Delta { .. } => "delta",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::PermissionAsk { .. } => "permission_ask",
            Self::Question { .. } => "question",
            Self::Compaction { .. } => "compaction",
            Self::BackgroundTasks { .. } => "background_tasks",
            Self::TaskUpdate { .. } => "task_update",
            Self::ModelChanged { .. } => "model_changed",
            Self::PolicyModeChanged { .. } => "policy_mode_changed",
            Self::Done { .. } => "done",
            Self::Error { .. } => "error",
            Self::ProviderNotice { .. } => "provider_notice",
        }
    }

    /// Every `type` tag of the contract, in declaration order.
    pub const TYPE_NAMES: [&'static str; 17] = [
        "session_started",
        "user_echo",
        "text",
        "thinking",
        "delta",
        "tool_call",
        "tool_result",
        "permission_ask",
        "question",
        "compaction",
        "background_tasks",
        "task_update",
        "model_changed",
        "policy_mode_changed",
        "done",
        "error",
        "provider_notice",
    ];
}

/// State of an MCP server attached to a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerStatus {
    /// Server name.
    pub name: String,
    /// Provider's status string (`connected`, `failed`…).
    #[serde(default)]
    pub status: String,
}

/// What a [`AgentEvent::Delta`] streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DeltaKind {
    /// Assistant text.
    Text,
    /// Reasoning.
    Thinking,
    /// JSON input of a tool call.
    ToolInput,
}

/// Output of a tool. Serialised untagged: a string or an array of blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolOutput {
    /// Plain text.
    Text(String),
    /// Structured content blocks, as the provider gave them.
    Blocks(Vec<Value>),
}

/// How a [`AgentEvent::Question`] is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum QuestionReply {
    /// The user's answer is sent as a regular turn (`send_turn`).
    Turn,
    /// The answer is given with `answer_question`.
    Call,
}

/// One question put to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionSpec {
    /// The question.
    pub question: String,
    /// Short label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// Suggested answers.
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    /// Whether several options may be chosen.
    #[serde(default)]
    pub multi_select: bool,
}

/// One suggested answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    /// The answer.
    pub label: String,
    /// What choosing it means.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Phase of a compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompactionPhase {
    /// Compaction is starting.
    Started,
    /// Compaction is done; the context was replaced by a summary.
    Completed,
}

/// What triggered a compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompactionTrigger {
    /// Asked for by the user.
    Manual,
    /// Decided by the provider (context nearly full).
    Auto,
}

impl CompactionTrigger {
    /// The wire string (`manual` / `auto`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }
}

/// Step of a long-running task's life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TaskPhase {
    /// The task started.
    Started,
    /// The task made progress.
    Progress,
    /// The task's description or state changed.
    Updated,
    /// The task notified the session (usually: it ended).
    Notification,
}

impl TaskPhase {
    /// The wire string (`started`, `progress`, `updated`, `notification`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Progress => "progress",
            Self::Updated => "updated",
            Self::Notification => "notification",
        }
    }
}

/// A background task of the provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTask {
    /// Provider identifier of the task.
    pub id: String,
    /// What kind of task it is.
    #[serde(default)]
    pub kind: BackgroundTaskKind,
    /// Human description (often the command line).
    #[serde(default)]
    pub description: String,
    /// Current state.
    #[serde(default)]
    pub status: BackgroundTaskStatus,
    /// Start time, milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    /// The `tool_call` that started the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Parent tool call when a sub-agent started the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Process identifier. Diagnostic only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

/// Kind of a background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BackgroundTaskKind {
    /// A shell command left running.
    Shell,
    /// A watcher that streams events.
    Monitor,
    /// A sub-agent.
    Agent,
    /// Anything else.
    #[default]
    Other,
}

/// State of a background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BackgroundTaskStatus {
    /// Still running.
    #[default]
    Running,
    /// Ended normally.
    Completed,
    /// Ended in error.
    Failed,
    /// Stopped on request.
    Killed,
}

/// Why a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StopReason {
    /// The model finished its answer.
    Completed,
    /// The turn limit was reached; the host may continue.
    MaxTurns,
    /// The output token limit was reached.
    MaxTokens,
    /// The turn was interrupted on request.
    Interrupted,
    /// The model refused.
    Refusal,
    /// The budget was exhausted.
    BudgetExceeded,
    /// The turn failed.
    Error,
}

/// Token usage. An unknown counter is `None`, never zero.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Input tokens read from the prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Input tokens written to the prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    /// Reasoning tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Tokens in context after the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// Breakdown by model, when the provider gives one.
    #[serde(default)]
    pub by_model: Vec<ModelUsage>,
}

impl Usage {
    /// Input plus output tokens, when both are known.
    pub fn total_tokens(&self) -> Option<u64> {
        Some(self.input_tokens? + self.output_tokens?)
    }
}

/// Usage of one model inside a turn.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    /// Model identifier.
    pub model: String,
    /// Input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Input tokens read from the prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Input tokens written to the prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
    /// Cost attributed to this model, in USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Context window the provider reports for this model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

/// Cost of a turn. `usd` is `None` when no price is known: never a fake zero.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Cost {
    /// Amount in USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd: Option<f64>,
    /// Where the amount comes from.
    #[serde(default)]
    pub basis: CostBasis,
}

impl Cost {
    /// No price known.
    pub fn unknown() -> Self {
        Self::default()
    }

    /// A free endpoint: zero, and said to be free.
    pub fn free() -> Self {
        Self {
            usd: Some(0.0),
            basis: CostBasis::Free,
        }
    }
}
