//! Wire types of `codex app-server`: JSON-RPC 2.0 **without** the `jsonrpc`
//! header, one JSON object per line on stdio.
//!
//! **Provenance: the documentation, not a session.** Every shape below comes from
//! the README of `codex-rs/app-server` at tag `rust-v0.130.0` (stable surface only:
//! `capabilities.experimentalApi` is never sent), plus a few fields named in the
//! module documentation of `providers/codex` as "from memory". No real
//! `codex app-server` was run: the installed `codex` is 0.38.0, which has no
//! `app-server`. The versioned schema in
//! `tests/transcripts/codex/<version>/schema/` states what this module accepts and
//! sends; `tests/codex_schema_drift.rs` fails when the two diverge.
//!
//! Unknown fields are always ignored (a newer server may add some); a field this
//! adapter reads is `Option` unless the adapter cannot work without it.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Name the adapter gives itself in `initialize.clientInfo` (OpenAI uses it for
/// its compliance logs).
pub const CLIENT_NAME: &str = "nexus_po";

/// JSON-RPC error object.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    /// Error code (`-32001` = server overloaded, retry later).
    pub code: i64,
    /// Message.
    pub message: String,
}

/// One line read from the server.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// Answer to a request of ours.
    Response {
        /// Request id, as the server echoes it.
        id: Value,
        /// Result or error.
        outcome: Result<Value, RpcError>,
    },
    /// Request initiated by the server (an approval): we must answer it.
    Request {
        /// Id to echo back, untouched.
        id: Value,
        /// Method name.
        method: String,
        /// Parameters.
        params: Value,
    },
    /// Notification: no answer.
    Notification {
        /// Method name.
        method: String,
        /// Parameters.
        params: Value,
    },
}

impl Frame {
    /// Parses one line. The shape decides: `method` + `id` is a request, `method`
    /// alone a notification, `id` + `result`/`error` a response.
    pub fn parse(line: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(line).map_err(|_| "not JSON".to_owned())?;
        let object = value.as_object().ok_or("not a JSON object")?;
        let id = object.get("id").filter(|id| !id.is_null()).cloned();
        let method = object.get("method").and_then(Value::as_str);
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        match (id, method) {
            (Some(id), Some(method)) => Ok(Self::Request {
                id,
                method: method.to_owned(),
                params,
            }),
            (None, Some(method)) => Ok(Self::Notification {
                method: method.to_owned(),
                params,
            }),
            (Some(id), None) => {
                if let Some(error) = object.get("error") {
                    let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                    let message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    Ok(Self::Response {
                        id,
                        outcome: Err(RpcError { code, message }),
                    })
                } else if let Some(result) = object.get("result") {
                    Ok(Self::Response {
                        id,
                        outcome: Ok(result.clone()),
                    })
                } else {
                    Err("a response with neither result nor error".to_owned())
                }
            },
            (None, None) => Err("neither an id nor a method".to_owned()),
        }
    }
}

/// A request line: `{"method","id","params"}`.
pub fn request_line(id: i64, method: &str, params: Value) -> Value {
    json!({ "method": method, "id": id, "params": params })
}

/// A notification line: `{"method","params"}`.
pub fn notification_line(method: &str, params: Value) -> Value {
    json!({ "method": method, "params": params })
}

/// A response line answering a server request, echoing `id` untouched.
pub fn response_line(id: &Value, result: Value) -> Value {
    json!({ "id": id, "result": result })
}

/// An error response line.
pub fn error_response_line(id: &Value, code: i64, message: &str) -> Value {
    json!({ "id": id, "error": { "code": code, "message": message } })
}

/// Serialises a value compactly with the keys of every object **sorted**, whatever the
/// `serde_json` features of the build (`preserve_order` or not): the bytes this
/// adapter writes are the same in every build, so a test can compare them to a literal.
pub fn canonical_line(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for key in keys {
                    out.insert(key.clone(), sorted(&map[key]));
                }
                Value::Object(out)
            },
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(value).to_string()
}

// ---------------------------------------------------------------------------
// Client -> server
// ---------------------------------------------------------------------------

/// `initialize.params.clientInfo`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientInfo {
    /// Client name.
    pub name: String,
    /// Display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Client version.
    pub version: String,
}

/// `initialize` parameters. `capabilities` is never sent by this adapter
/// (stable surface only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitializeParams {
    /// Who is talking.
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
    /// Per-connection capabilities (`experimentalApi`, `optOutNotificationMethods`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Value>,
}

impl InitializeParams {
    /// The parameters this adapter sends: its name and version, no capability.
    pub fn for_adapter() -> Self {
        Self {
            client_info: ClientInfo {
                name: CLIENT_NAME.to_owned(),
                title: Some("Project Orchestrator".to_owned()),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            capabilities: None,
        }
    }
}

/// `initialize` result.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct InitializeResult {
    /// User agent the server presents upstream.
    #[serde(rename = "userAgent")]
    pub user_agent: String,
    /// The server's Codex home.
    #[serde(default, rename = "codexHome")]
    pub codex_home: Option<String>,
}

/// `approvalPolicy` values this adapter sends (contract §6). Spelling from the task
/// brief (`on-request`, `never`); the README example of `turn/start` also shows a
/// camelCase `unlessTrusted`, so the spelling of the *accepted* set is NOT VERIFIED.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalPolicy {
    /// The model decides when to ask.
    #[serde(rename = "on-request")]
    OnRequest,
    /// Never ask.
    #[serde(rename = "never")]
    Never,
}

/// `sandbox` of `thread/start` / `thread/resume`. `dangerFullAccess` is never sent
/// in v1 (contract §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxMode {
    /// Nothing is written.
    #[serde(rename = "readOnly")]
    ReadOnly,
    /// Writes confined to the workspace.
    #[serde(rename = "workspaceWrite")]
    WorkspaceWrite,
}

/// `sandboxPolicy` of `turn/start`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SandboxPolicy {
    /// Nothing is written.
    #[serde(rename = "readOnly")]
    ReadOnly,
    /// Writes confined to `writableRoots` (the workspace when empty).
    #[serde(rename = "workspaceWrite")]
    WorkspaceWrite {
        /// Extra writable directories.
        #[serde(
            default,
            rename = "writableRoots",
            skip_serializing_if = "Vec::is_empty"
        )]
        writable_roots: Vec<String>,
    },
}

/// `thread/start` parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadStartParams {
    /// Working directory.
    pub cwd: String,
    /// Model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Approval policy.
    #[serde(rename = "approvalPolicy")]
    pub approval_policy: ApprovalPolicy,
    /// Sandbox.
    pub sandbox: SandboxMode,
    /// System prompt, `replace` mode (NOT VERIFIED: field name from memory).
    #[serde(
        default,
        rename = "baseInstructions",
        skip_serializing_if = "Option::is_none"
    )]
    pub base_instructions: Option<String>,
    /// System prompt, `append` mode (NOT VERIFIED: field name from memory).
    #[serde(
        default,
        rename = "developerInstructions",
        skip_serializing_if = "Option::is_none"
    )]
    pub developer_instructions: Option<String>,
}

/// `thread/resume` parameters: the thread and the same overrides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadResumeParams {
    /// Thread to reopen.
    #[serde(rename = "threadId")]
    pub thread_id: String,
    /// Working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Model (disables the persisted fallback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Approval policy.
    #[serde(
        default,
        rename = "approvalPolicy",
        skip_serializing_if = "Option::is_none"
    )]
    pub approval_policy: Option<ApprovalPolicy>,
    /// Sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxMode>,
}

/// A thread, as much as the adapter reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadRef {
    /// Thread id (`thr_…`).
    pub id: String,
}

/// Result of `thread/start` and `thread/resume`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThreadResult {
    /// The thread.
    pub thread: ThreadRef,
    /// Model in effect (NOT VERIFIED: present in the response from memory).
    #[serde(default)]
    pub model: Option<String>,
}

/// One input of `turn/start`. Text only: `images` is declared absent (A12).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum UserInput {
    /// Text.
    #[serde(rename = "text")]
    Text {
        /// The text.
        text: String,
    },
}

/// `turn/start` parameters. The overrides are sticky on the server (README), so
/// the adapter sends the policy and the model it holds on every turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnStartParams {
    /// Thread.
    #[serde(rename = "threadId")]
    pub thread_id: String,
    /// Input.
    pub input: Vec<UserInput>,
    /// Model override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Approval policy override.
    #[serde(
        default,
        rename = "approvalPolicy",
        skip_serializing_if = "Option::is_none"
    )]
    pub approval_policy: Option<ApprovalPolicy>,
    /// Sandbox override.
    #[serde(
        default,
        rename = "sandboxPolicy",
        skip_serializing_if = "Option::is_none"
    )]
    pub sandbox_policy: Option<SandboxPolicy>,
}

/// A turn as `turn/start` and the turn notifications carry it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Turn {
    /// Turn id.
    pub id: String,
    /// `inProgress`, `completed`, `interrupted`, `failed`.
    #[serde(default)]
    pub status: String,
    /// Failure of a `failed` turn.
    #[serde(default)]
    pub error: Option<ErrorPayload>,
}

/// Result of `turn/start`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TurnStartResult {
    /// The new turn.
    pub turn: Turn,
}

/// `turn/interrupt` parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnInterruptParams {
    /// Thread.
    #[serde(rename = "threadId")]
    pub thread_id: String,
    /// Turn to cancel.
    #[serde(rename = "turnId")]
    pub turn_id: String,
}

// ---------------------------------------------------------------------------
// Server -> client: notifications
// ---------------------------------------------------------------------------

/// `{ message, codexErrorInfo?, additionalDetails? }` of a failed turn and of the
/// `error` notification.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ErrorPayload {
    /// Message (redacted by the adapter before it reaches an event).
    #[serde(default)]
    pub message: String,
    /// Classification.
    #[serde(default, rename = "codexErrorInfo")]
    pub info: Option<CodexErrorInfo>,
    /// More details.
    #[serde(default, rename = "additionalDetails")]
    pub additional_details: Option<String>,
}

/// `CodexErrorInfo`: a bare name (`"UsageLimitExceeded"`) or a one-key object
/// (`{"HttpConnectionFailed":{"httpStatusCode":502}}`).
#[derive(Debug, Clone, PartialEq)]
pub struct CodexErrorInfo {
    /// Variant name.
    pub kind: String,
    /// `httpStatusCode` of the variants that carry one.
    pub http_status: Option<u16>,
}

impl<'de> Deserialize<'de> for CodexErrorInfo {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match &value {
            Value::String(name) => Ok(Self {
                kind: name.clone(),
                http_status: None,
            }),
            Value::Object(map) if map.len() == 1 => {
                let (name, inner) = map
                    .iter()
                    .next()
                    .ok_or_else(|| serde::de::Error::custom("empty codexErrorInfo"))?;
                Ok(Self {
                    kind: name.clone(),
                    http_status: inner
                        .get("httpStatusCode")
                        .and_then(Value::as_u64)
                        .and_then(|code| u16::try_from(code).ok()),
                })
            },
            _ => Err(serde::de::Error::custom(
                "codexErrorInfo is a name or a one-key object",
            )),
        }
    }
}

/// Token counts of `thread/tokenUsage/updated` (NOT VERIFIED: shape from memory,
/// absent from the README of the tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
pub struct TokenBreakdown {
    /// Total.
    #[serde(default, rename = "totalTokens")]
    pub total_tokens: u64,
    /// Input tokens, **cached ones included**.
    #[serde(rename = "inputTokens")]
    pub input_tokens: u64,
    /// Cached input tokens.
    #[serde(default, rename = "cachedInputTokens")]
    pub cached_input_tokens: u64,
    /// Output tokens.
    #[serde(rename = "outputTokens")]
    pub output_tokens: u64,
    /// Reasoning tokens.
    #[serde(default, rename = "reasoningOutputTokens")]
    pub reasoning_output_tokens: u64,
}

/// `thread/tokenUsage/updated.tokenUsage`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThreadTokenUsage {
    /// Cumulative over the thread.
    pub total: TokenBreakdown,
    /// Last model request.
    #[serde(default)]
    pub last: Option<TokenBreakdown>,
    /// Context window of the model, when the server says (NOT VERIFIED).
    #[serde(default, rename = "modelContextWindow")]
    pub model_context_window: Option<u64>,
}

/// A thread item (`item/started`, `item/completed`). Unknown kinds are
/// [`ThreadItem::Other`] and ignored.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type")]
pub enum ThreadItem {
    /// Echo of the user's input.
    #[serde(rename = "userMessage")]
    UserMessage {
        /// Item id.
        id: String,
    },
    /// Assistant text.
    #[serde(rename = "agentMessage")]
    AgentMessage {
        /// Item id.
        id: String,
        /// Accumulated text.
        #[serde(default)]
        text: String,
    },
    /// Reasoning: readable summaries and/or raw blocks.
    #[serde(rename = "reasoning")]
    Reasoning {
        /// Item id.
        id: String,
        /// Streamed summaries.
        #[serde(default)]
        summary: Vec<String>,
        /// Raw reasoning blocks (open models).
        #[serde(default)]
        content: Vec<String>,
    },
    /// A command run in the sandbox.
    #[serde(rename = "commandExecution")]
    CommandExecution {
        /// Item id.
        id: String,
        /// The command (a string, or the argv).
        #[serde(default)]
        command: Value,
        /// Working directory.
        #[serde(default)]
        cwd: Option<String>,
        /// `inProgress`, `completed`, `failed`, `declined`.
        #[serde(default)]
        status: String,
        /// Output, once finished.
        #[serde(default, rename = "aggregatedOutput")]
        aggregated_output: Option<String>,
        /// Exit code, once finished.
        #[serde(default, rename = "exitCode")]
        exit_code: Option<i64>,
    },
    /// A file edit.
    #[serde(rename = "fileChange")]
    FileChange {
        /// Item id.
        id: String,
        /// `{path, kind, diff}` entries.
        #[serde(default)]
        changes: Vec<Value>,
        /// `inProgress`, `completed`, `failed`, `declined`.
        #[serde(default)]
        status: String,
    },
    /// An MCP tool call.
    #[serde(rename = "mcpToolCall")]
    McpToolCall {
        /// Item id.
        id: String,
        /// MCP server name.
        server: String,
        /// Tool name.
        tool: String,
        /// `inProgress`, `completed`, `failed`.
        #[serde(default)]
        status: String,
        /// Arguments.
        #[serde(default)]
        arguments: Value,
        /// Result, once finished.
        #[serde(default)]
        result: Option<Value>,
        /// Error, once finished.
        #[serde(default)]
        error: Option<Value>,
    },
    /// A collaboration call (`spawn_agent`, `send_input`, `wait`…).
    #[serde(rename = "collabToolCall")]
    CollabToolCall {
        /// Item id.
        id: String,
        /// `spawn_agent`, `send_input`, `resume_agent`, `wait`, `close_agent`.
        #[serde(default)]
        tool: String,
        /// `inProgress`, `completed`, `failed`.
        #[serde(default)]
        status: String,
        /// Prompt given to the receiver.
        #[serde(default)]
        prompt: Option<String>,
        /// Thread the call spawned.
        #[serde(default, rename = "newThreadId")]
        new_thread_id: Option<String>,
    },
    /// A web search.
    #[serde(rename = "webSearch")]
    WebSearch {
        /// Item id.
        id: String,
        /// Query.
        #[serde(default)]
        query: String,
    },
    /// The history was compacted (automatically, or by `thread/compact/start`).
    #[serde(rename = "contextCompaction")]
    ContextCompaction {
        /// Item id.
        id: String,
    },
    /// Any other item.
    #[serde(other)]
    Other,
}

/// `item/started` and `item/completed`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ItemEnvelope {
    /// Thread the item belongs to (`None`: the main thread).
    #[serde(default, rename = "threadId")]
    pub thread_id: Option<String>,
    /// Turn.
    #[serde(default, rename = "turnId")]
    pub turn_id: Option<String>,
    /// The item.
    pub item: ThreadItem,
}

/// `turn/started` and `turn/completed`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TurnEnvelope {
    /// Thread (`None`: the main thread).
    #[serde(default, rename = "threadId")]
    pub thread_id: Option<String>,
    /// The turn.
    pub turn: Turn,
}

/// `item/agentMessage/delta`, `item/reasoning/*Delta`,
/// `item/commandExecution/outputDelta`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DeltaEnvelope {
    /// Thread (`None`: the main thread).
    #[serde(default, rename = "threadId")]
    pub thread_id: Option<String>,
    /// Item the fragment belongs to.
    #[serde(rename = "itemId")]
    pub item_id: String,
    /// The fragment.
    pub delta: String,
}

/// `thread/tokenUsage/updated`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TokenUsageEnvelope {
    /// Thread (`None`: the main thread).
    #[serde(default, rename = "threadId")]
    pub thread_id: Option<String>,
    /// Counts.
    #[serde(rename = "tokenUsage")]
    pub token_usage: ThreadTokenUsage,
}

/// `error`: a failure mid-turn, which may precede `turn/completed { failed }`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ErrorEnvelope {
    /// Thread (`None`: the main thread).
    #[serde(default, rename = "threadId")]
    pub thread_id: Option<String>,
    /// The failure.
    pub error: ErrorPayload,
    /// The server will retry by itself.
    #[serde(default, rename = "willRetry")]
    pub will_retry: bool,
}

/// `mcpServer/startupStatus/updated`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct McpStartup {
    /// Server name.
    pub name: String,
    /// `starting`, `ready`, `failed`, `cancelled`.
    pub status: String,
}

/// `serverRequest/resolved`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RequestResolved {
    /// The request id that was resolved or cleared.
    #[serde(rename = "requestId")]
    pub request_id: Value,
}

/// `model/rerouted`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ModelRerouted {
    /// Thread (`None`: the main thread).
    #[serde(default, rename = "threadId")]
    pub thread_id: Option<String>,
    /// New model.
    #[serde(rename = "toModel")]
    pub to_model: String,
}

/// `thread/started`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThreadStartedEnvelope {
    /// The thread.
    pub thread: ThreadRef,
}

/// A notification the adapter understands. Anything else is [`Notification::Other`].
#[derive(Debug, Clone, PartialEq)]
pub enum Notification {
    /// `thread/started`.
    ThreadStarted(ThreadStartedEnvelope),
    /// `turn/started`.
    TurnStarted(TurnEnvelope),
    /// `turn/completed`.
    TurnCompleted(TurnEnvelope),
    /// `item/started`.
    ItemStarted(ItemEnvelope),
    /// `item/completed`.
    ItemCompleted(ItemEnvelope),
    /// `item/agentMessage/delta`.
    AgentMessageDelta(DeltaEnvelope),
    /// `item/reasoning/summaryTextDelta`.
    ReasoningSummaryDelta(DeltaEnvelope),
    /// `item/reasoning/textDelta`.
    ReasoningTextDelta(DeltaEnvelope),
    /// `thread/tokenUsage/updated`.
    TokenUsage(TokenUsageEnvelope),
    /// `error`.
    Error(ErrorEnvelope),
    /// `mcpServer/startupStatus/updated`.
    McpStartup(McpStartup),
    /// `serverRequest/resolved`.
    RequestResolved(RequestResolved),
    /// `model/rerouted`.
    ModelRerouted(ModelRerouted),
    /// A method the adapter does not use (`item/commandExecution/outputDelta`,
    /// `turn/diff/updated`, `turn/plan/updated`, `warning`, …).
    Other(String),
}

impl Notification {
    /// Parses a notification; `Err` names the method whose parameters are not the
    /// documented shape.
    pub fn parse(method: &str, params: &Value) -> Result<Self, String> {
        fn typed<T: serde::de::DeserializeOwned>(
            method: &str,
            params: &Value,
        ) -> Result<T, String> {
            serde_json::from_value(params.clone())
                .map_err(|error| format!("malformed `{method}`: {error}"))
        }
        Ok(match method {
            "thread/started" => Self::ThreadStarted(typed(method, params)?),
            "turn/started" => Self::TurnStarted(typed(method, params)?),
            "turn/completed" => Self::TurnCompleted(typed(method, params)?),
            "item/started" => Self::ItemStarted(typed(method, params)?),
            "item/completed" => Self::ItemCompleted(typed(method, params)?),
            "item/agentMessage/delta" => Self::AgentMessageDelta(typed(method, params)?),
            "item/reasoning/summaryTextDelta" => {
                Self::ReasoningSummaryDelta(typed(method, params)?)
            },
            "item/reasoning/textDelta" => Self::ReasoningTextDelta(typed(method, params)?),
            "thread/tokenUsage/updated" => Self::TokenUsage(typed(method, params)?),
            "error" => Self::Error(typed(method, params)?),
            "mcpServer/startupStatus/updated" => Self::McpStartup(typed(method, params)?),
            "serverRequest/resolved" => Self::RequestResolved(typed(method, params)?),
            "model/rerouted" => Self::ModelRerouted(typed(method, params)?),
            other => Self::Other(other.to_owned()),
        })
    }
}

// ---------------------------------------------------------------------------
// Server -> client: requests (approvals)
// ---------------------------------------------------------------------------

/// `mcpServer/elicitation/request`. For an MCP tool approval, `meta` holds
/// `codex_approval_kind: "mcp_tool_call"` and `persist` (README).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ElicitationRequest {
    /// Thread.
    #[serde(default, rename = "threadId")]
    pub thread_id: Option<String>,
    /// Turn, best effort.
    #[serde(default, rename = "turnId")]
    pub turn_id: Option<String>,
    /// MCP server that asks.
    #[serde(rename = "serverName")]
    pub server_name: String,
    /// `form` (default) or `url`.
    #[serde(default)]
    pub mode: Option<String>,
    /// Message shown to the user.
    #[serde(default)]
    pub message: String,
    /// `_meta` (the README calls it `meta`; both are read).
    #[serde(default, rename = "_meta", alias = "meta")]
    pub meta: Option<Value>,
}

/// `item/commandExecution/requestApproval`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CommandApprovalRequest {
    /// Item the approval is about.
    #[serde(rename = "itemId")]
    pub item_id: String,
    /// Why the command needs approval.
    #[serde(default)]
    pub reason: Option<String>,
    /// The command.
    #[serde(default)]
    pub command: Option<Value>,
    /// Working directory.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Decisions the server accepts; preferred over the fixed set when present.
    #[serde(default, rename = "availableDecisions")]
    pub available_decisions: Option<Vec<Value>>,
}

/// `item/fileChange/requestApproval`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FileChangeApprovalRequest {
    /// Item the approval is about.
    #[serde(rename = "itemId")]
    pub item_id: String,
    /// Why.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `item/permissions/requestApproval` (the `request_permissions` tool).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PermissionsApprovalRequest {
    /// Item.
    #[serde(rename = "itemId")]
    pub item_id: String,
    /// Why.
    #[serde(default)]
    pub reason: Option<String>,
    /// The permission profile asked for.
    pub permissions: Value,
}

/// The server request the adapter understands.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerRequest {
    /// `mcpServer/elicitation/request`.
    Elicitation(ElicitationRequest),
    /// `item/commandExecution/requestApproval`.
    CommandApproval(CommandApprovalRequest),
    /// `item/fileChange/requestApproval`.
    FileChangeApproval(FileChangeApprovalRequest),
    /// `item/permissions/requestApproval`.
    PermissionsApproval(PermissionsApprovalRequest),
    /// A method this adapter does not serve.
    Unknown(String),
}

impl ServerRequest {
    /// Parses a server request.
    pub fn parse(method: &str, params: &Value) -> Result<Self, String> {
        fn typed<T: serde::de::DeserializeOwned>(
            method: &str,
            params: &Value,
        ) -> Result<T, String> {
            serde_json::from_value(params.clone())
                .map_err(|error| format!("malformed `{method}`: {error}"))
        }
        Ok(match method {
            "mcpServer/elicitation/request" => Self::Elicitation(typed(method, params)?),
            "item/commandExecution/requestApproval" => {
                Self::CommandApproval(typed(method, params)?)
            },
            "item/fileChange/requestApproval" => Self::FileChangeApproval(typed(method, params)?),
            "item/permissions/requestApproval" => Self::PermissionsApproval(typed(method, params)?),
            other => Self::Unknown(other.to_owned()),
        })
    }
}

/// Answer to `mcpServer/elicitation/request`: `{action, content: null}`; a
/// persistent approval adds `_meta.persist` (NOT VERIFIED: how the README's
/// `persist` hint is answered is from memory).
pub fn elicitation_response(action: &str, persist: Option<&str>) -> Value {
    let mut response = json!({ "action": action, "content": Value::Null });
    if let Some(persist) = persist {
        response["_meta"] = json!({ "persist": persist });
    }
    response
}

/// Answer to a command or file-change approval: `{decision}`.
pub fn decision_response(decision: &str) -> Value {
    json!({ "decision": decision })
}

/// Answer to `item/permissions/requestApproval`: the granted subset, and
/// `scope: "session"` to keep it past the turn.
pub fn permissions_response(granted: Value, session: bool) -> Value {
    let mut response = json!({ "permissions": granted });
    if session {
        response["scope"] = json!("session");
    }
    response
}

// ---------------------------------------------------------------------------
// Schema check
// ---------------------------------------------------------------------------

/// Every message kind the adapter knows, the keys of `schema/messages.json`.
pub const MESSAGE_KINDS: &[&str] = &[
    "initialize.params",
    "initialize.result",
    "thread/start.params",
    "thread/resume.params",
    "thread/start.result",
    "thread/resume.result",
    "turn/start.params",
    "turn/start.result",
    "turn/interrupt.params",
    "turn/interrupt.result",
    "thread/started",
    "turn/started",
    "turn/completed",
    "item/started",
    "item/completed",
    "item/agentMessage/delta",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/textDelta",
    "item/commandExecution/outputDelta",
    "thread/tokenUsage/updated",
    "error",
    "mcpServer/startupStatus/updated",
    "serverRequest/resolved",
    "model/rerouted",
    "mcpServer/elicitation/request",
    "mcpServer/elicitation/request.response",
    "item/commandExecution/requestApproval",
    "item/commandExecution/requestApproval.response",
    "item/fileChange/requestApproval",
    "item/fileChange/requestApproval.response",
    "item/permissions/requestApproval",
    "item/permissions/requestApproval.response",
];

/// Whether `value` is accepted as message kind `kind` by the types above. The
/// kinds are the keys of `schema/messages.json`; `tests/codex_schema_drift.rs`
/// checks every message of every transcript, and every message the adapter wrote
/// in the conformance runs, through this function.
pub fn validate_kind(kind: &str, value: &Value) -> Result<(), String> {
    fn accepts<T: serde::de::DeserializeOwned>(value: &Value) -> Result<(), String> {
        serde_json::from_value::<T>(value.clone())
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
    match kind {
        "initialize.params" => accepts::<InitializeParams>(value),
        "initialize.result" => accepts::<InitializeResult>(value),
        "thread/start.params" => accepts::<ThreadStartParams>(value),
        "thread/resume.params" => accepts::<ThreadResumeParams>(value),
        "thread/start.result" | "thread/resume.result" => accepts::<ThreadResult>(value),
        "turn/start.params" => accepts::<TurnStartParams>(value),
        "turn/start.result" => accepts::<TurnStartResult>(value),
        "turn/interrupt.params" => accepts::<TurnInterruptParams>(value),
        "turn/interrupt.result" => Ok(()),
        "thread/started" => accepts::<ThreadStartedEnvelope>(value),
        "turn/started" | "turn/completed" => accepts::<TurnEnvelope>(value),
        "item/started" | "item/completed" => accepts::<ItemEnvelope>(value),
        "item/agentMessage/delta"
        | "item/reasoning/summaryTextDelta"
        | "item/reasoning/textDelta"
        | "item/commandExecution/outputDelta" => accepts::<DeltaEnvelope>(value),
        "thread/tokenUsage/updated" => accepts::<TokenUsageEnvelope>(value),
        "error" => accepts::<ErrorEnvelope>(value),
        "mcpServer/startupStatus/updated" => accepts::<McpStartup>(value),
        "serverRequest/resolved" => accepts::<RequestResolved>(value),
        "model/rerouted" => accepts::<ModelRerouted>(value),
        "mcpServer/elicitation/request" => accepts::<ElicitationRequest>(value),
        "item/commandExecution/requestApproval" => accepts::<CommandApprovalRequest>(value),
        "item/fileChange/requestApproval" => accepts::<FileChangeApprovalRequest>(value),
        "item/permissions/requestApproval" => accepts::<PermissionsApprovalRequest>(value),
        "mcpServer/elicitation/request.response" => check_elicitation_response(value),
        "item/commandExecution/requestApproval.response"
        | "item/fileChange/requestApproval.response" => check_decision_response(value),
        "item/permissions/requestApproval.response" => {
            if value.get("permissions").is_some() {
                Ok(())
            } else {
                Err("missing `permissions`".to_owned())
            }
        },
        other => Err(format!("unknown message kind `{other}`")),
    }
}

fn check_elicitation_response(value: &Value) -> Result<(), String> {
    match value.get("action").and_then(Value::as_str) {
        Some("accept" | "decline" | "cancel") if value.get("content").is_some() => Ok(()),
        _ => Err("expected {action: accept|decline|cancel, content}".to_owned()),
    }
}

fn check_decision_response(value: &Value) -> Result<(), String> {
    if value.get("decision").is_some() {
        Ok(())
    } else {
        Err("missing `decision`".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_told_apart_by_their_shape() {
        assert!(matches!(
            Frame::parse(r#"{"id":1,"result":{}}"#),
            Ok(Frame::Response { outcome: Ok(_), .. })
        ));
        assert!(matches!(
            Frame::parse(
                r#"{"id":"a","error":{"code":-32001,"message":"Server overloaded; retry later."}}"#
            ),
            Ok(Frame::Response {
                outcome: Err(RpcError { code: -32001, .. }),
                ..
            })
        ));
        assert!(matches!(
            Frame::parse(r#"{"id":7,"method":"mcpServer/elicitation/request","params":{}}"#),
            Ok(Frame::Request { .. })
        ));
        assert!(matches!(
            Frame::parse(r#"{"method":"turn/started","params":{}}"#),
            Ok(Frame::Notification { .. })
        ));
        assert!(Frame::parse("not json").is_err());
        assert!(Frame::parse("[]").is_err());
        assert!(Frame::parse(r#"{"id":1}"#).is_err());
    }

    #[test]
    fn request_lines_carry_no_jsonrpc_header() {
        let line = request_line(3, "turn/start", json!({"threadId": "t"}));
        assert!(line.get("jsonrpc").is_none());
        assert_eq!(line["id"], 3);
        assert!(
            notification_line("initialized", json!({}))
                .get("id")
                .is_none()
        );
    }

    #[test]
    fn error_info_is_a_name_or_a_one_key_object() {
        let bare: CodexErrorInfo = serde_json::from_value(json!("UsageLimitExceeded")).unwrap();
        assert_eq!(bare.kind, "UsageLimitExceeded");
        let with_status: CodexErrorInfo =
            serde_json::from_value(json!({"HttpConnectionFailed": {"httpStatusCode": 502}}))
                .unwrap();
        assert_eq!(
            (with_status.kind.as_str(), with_status.http_status),
            ("HttpConnectionFailed", Some(502))
        );
        assert!(serde_json::from_value::<CodexErrorInfo>(json!({"a": 1, "b": 2})).is_err());
        assert!(serde_json::from_value::<CodexErrorInfo>(json!(3)).is_err());
    }

    #[test]
    fn an_unknown_item_and_an_unknown_notification_are_not_errors() {
        let item: ThreadItem =
            serde_json::from_value(json!({"type": "imageView", "id": "i"})).unwrap();
        assert_eq!(item, ThreadItem::Other);
        assert!(matches!(
            Notification::parse("turn/diff/updated", &json!({})),
            Ok(Notification::Other(_))
        ));
        assert!(Notification::parse("turn/started", &json!({})).is_err());
    }

    #[test]
    fn approval_policy_and_sandbox_spellings_are_the_documented_ones() {
        assert_eq!(
            serde_json::to_value(ApprovalPolicy::OnRequest).unwrap(),
            "on-request"
        );
        assert_eq!(
            serde_json::to_value(ApprovalPolicy::Never).unwrap(),
            "never"
        );
        assert_eq!(
            serde_json::to_value(SandboxMode::WorkspaceWrite).unwrap(),
            "workspaceWrite"
        );
        assert_eq!(
            serde_json::to_value(SandboxMode::ReadOnly).unwrap(),
            "readOnly"
        );
        assert_eq!(
            serde_json::to_value(SandboxPolicy::WorkspaceWrite {
                writable_roots: vec![]
            })
            .unwrap(),
            json!({"type": "workspaceWrite"})
        );
    }

    #[test]
    fn the_elicitation_answers_are_exact() {
        assert_eq!(
            canonical_line(&elicitation_response("accept", None)),
            r#"{"action":"accept","content":null}"#
        );
        assert_eq!(
            canonical_line(&elicitation_response("accept", Some("session"))),
            r#"{"_meta":{"persist":"session"},"action":"accept","content":null}"#
        );
    }
}
