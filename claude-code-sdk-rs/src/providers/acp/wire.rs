//! Wire types of the Agent Client Protocol (ACP): JSON-RPC 2.0 (with the
//! `"jsonrpc":"2.0"` header), one JSON object per line on the agent's stdio.
//!
//! **Provenance: the public specification, not a session.** Every shape below comes
//! from the pages of `https://agentclientprotocol.com/protocol/` (initialization,
//! session-setup, prompt-turn, tool-calls, session-modes) read on 2026-10-05, protocol
//! version `1`. No real agent (opencode, Gemini CLI…) was run. The versioned schema in
//! `tests/transcripts/acp/<version>/schema/` states what this module accepts and sends;
//! `tests/acp_schema_drift.rs` fails when the two diverge.
//!
//! Unknown fields are always ignored (a newer agent may add some); a field this adapter
//! reads is `Option` / defaulted unless the adapter cannot work without it. What we
//! *send* is strict: every field is required by the type, so the schema lists it.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The protocol version this client speaks (an integer, major version only).
pub const PROTOCOL_VERSION: u32 = 1;

/// Name the adapter gives itself in `initialize.clientInfo`.
pub const CLIENT_NAME: &str = "nexus_po";

/// JSON-RPC error object.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    /// Error code (`-32000` is `auth_required` in ACP, `-32601` method not found).
    pub code: i64,
    /// Message.
    pub message: String,
}

/// One line read from the agent.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// Answer to a request of ours.
    Response {
        /// Request id as the agent echoes it.
        id: Value,
        /// Result or error.
        outcome: Result<Value, RpcError>,
    },
    /// Request initiated by the agent: we must answer it.
    Request {
        /// Id to echo back untouched.
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

/// A request line.
pub fn request_line(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "id": id, "params": params })
}

/// A notification line.
pub fn notification_line(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// A response line answering an agent request, echoing `id` untouched.
pub fn response_line(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// An error response line.
pub fn error_response_line(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Serialises a value compactly with the keys of every object **sorted**, whatever the
/// `serde_json` features of the build: the bytes this adapter writes are the same in
/// every build, so a test can compare them to a literal.
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
    serde_json::to_string(&sorted(value)).unwrap_or_else(|_| "null".to_owned())
}

// ---------------------------------------------------------------------------
// initialize
// ---------------------------------------------------------------------------

/// `fs` capabilities of the client: this client announces **none**.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCapabilities {
    /// `fs/read_text_file` is served.
    pub read_text_file: bool,
    /// `fs/write_text_file` is served.
    pub write_text_file: bool,
}

/// What the client can do for the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    /// File system access.
    pub fs: FsCapabilities,
    /// `terminal/*` is served.
    pub terminal: bool,
}

/// `clientInfo` / `agentInfo`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Implementation {
    /// Name.
    pub name: String,
    /// Display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Version.
    pub version: String,
}

/// `initialize` parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// Protocol version.
    pub protocol_version: u32,
    /// What this client serves (nothing).
    pub client_capabilities: ClientCapabilities,
    /// Who we are.
    pub client_info: Implementation,
}

impl InitializeParams {
    /// The parameters of this adapter: no `fs`, no `terminal`.
    pub fn for_adapter() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            client_capabilities: ClientCapabilities {
                fs: FsCapabilities {
                    read_text_file: false,
                    write_text_file: false,
                },
                terminal: false,
            },
            client_info: Implementation {
                name: CLIENT_NAME.to_owned(),
                title: Some("Project Orchestrator".to_owned()),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
        }
    }
}

/// `agentInfo`, read leniently.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct AgentInfo {
    /// Name.
    #[serde(default)]
    pub name: Option<String>,
    /// Display name.
    #[serde(default)]
    pub title: Option<String>,
    /// Version.
    #[serde(default)]
    pub version: Option<String>,
}

/// `promptCapabilities`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    /// Images accepted (never used: A12).
    #[serde(default)]
    pub image: bool,
    /// Audio accepted.
    #[serde(default)]
    pub audio: bool,
    /// Embedded resources accepted.
    #[serde(default)]
    pub embedded_context: bool,
}

/// `mcpCapabilities`: stdio is mandatory, the rest is announced.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct McpCapabilities {
    /// HTTP MCP servers accepted at `session/new`.
    #[serde(default)]
    pub http: bool,
    /// SSE MCP servers accepted.
    #[serde(default)]
    pub sse: bool,
}

/// `agentCapabilities`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    /// `session/load` exists.
    #[serde(default)]
    pub load_session: bool,
    /// Prompt content the agent accepts.
    #[serde(default)]
    pub prompt_capabilities: PromptCapabilities,
    /// MCP transports the agent accepts.
    #[serde(default)]
    pub mcp_capabilities: McpCapabilities,
}

/// One entry of `authMethods`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AuthMethod {
    /// Identifier.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Description.
    #[serde(default)]
    pub description: Option<String>,
}

/// `initialize` result.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    /// Version the agent speaks.
    pub protocol_version: u32,
    /// What the agent can do.
    #[serde(default)]
    pub agent_capabilities: AgentCapabilities,
    /// Ways to authenticate (never run by this client).
    #[serde(default)]
    pub auth_methods: Vec<AuthMethod>,
    /// Who the agent is.
    #[serde(default)]
    pub agent_info: Option<AgentInfo>,
}

// ---------------------------------------------------------------------------
// session/new, session/load
// ---------------------------------------------------------------------------

/// A `{name, value}` pair (environment variable, HTTP header).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NameValue {
    /// Name.
    pub name: String,
    /// Value (a secret, for credentials: never logged).
    pub value: String,
}

/// An HTTP or SSE MCP server of `session/new`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpMcpServer {
    /// `"http"` or `"sse"`.
    pub r#type: String,
    /// Name.
    pub name: String,
    /// URL.
    pub url: String,
    /// Headers.
    pub headers: Vec<NameValue>,
}

/// A stdio MCP server of `session/new`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct StdioMcpServer {
    /// Name.
    pub name: String,
    /// Executable.
    pub command: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Environment.
    pub env: Vec<NameValue>,
}

/// An MCP server given to the agent.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpServer {
    /// HTTP or SSE (carries `type`).
    Http(HttpMcpServer),
    /// A child process on stdio.
    Stdio(StdioMcpServer),
}

impl std::fmt::Debug for McpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Env values and headers routinely hold credentials: never printed.
        match self {
            Self::Http(server) => write!(
                f,
                "McpServer::Http({}, <{} headers>)",
                server.name,
                server.headers.len()
            ),
            Self::Stdio(server) => write!(
                f,
                "McpServer::Stdio({}, <{} env>)",
                server.name,
                server.env.len()
            ),
        }
    }
}

/// `session/new` parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams {
    /// Absolute working directory.
    pub cwd: String,
    /// MCP servers of the session.
    pub mcp_servers: Vec<McpServer>,
}

/// `session/load` parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionParams {
    /// Session to load.
    pub session_id: String,
    /// Absolute working directory.
    pub cwd: String,
    /// MCP servers of the session.
    pub mcp_servers: Vec<McpServer>,
}

/// One mode an agent publishes.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ModeInfo {
    /// Identifier, the value of `session/set_mode.modeId`.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Description.
    #[serde(default)]
    pub description: Option<String>,
}

/// `modes` of a `session/new` result.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeState {
    /// Mode in effect.
    pub current_mode_id: String,
    /// Modes the agent offers.
    pub available_modes: Vec<ModeInfo>,
}

/// `session/new` result.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResult {
    /// Identifier of the session.
    pub session_id: String,
    /// Modes, when the agent has some.
    #[serde(default)]
    pub modes: Option<ModeState>,
}

/// `session/load` result (may be `null`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct LoadSessionResult {
    /// Modes, when the agent has some.
    #[serde(default)]
    pub modes: Option<ModeState>,
}

// ---------------------------------------------------------------------------
// session/prompt, session/cancel, session/set_mode
// ---------------------------------------------------------------------------

/// A text block of a prompt (images are never sent: A12).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextBlock {
    /// Always `"text"`.
    pub r#type: String,
    /// The text.
    pub text: String,
}

/// `session/prompt` parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
    /// Session.
    pub session_id: String,
    /// Content blocks.
    pub prompt: Vec<TextBlock>,
}

/// Token usage of a prompt response (**unstable** in the specification, NOT VERIFIED).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptUsage {
    /// Input tokens.
    #[serde(default)]
    pub input_tokens: Option<u64>,
    /// Output tokens.
    #[serde(default)]
    pub output_tokens: Option<u64>,
    /// Total tokens.
    #[serde(default)]
    pub total_tokens: Option<u64>,
    /// Cached tokens read.
    #[serde(default)]
    pub cached_read_tokens: Option<u64>,
    /// Reasoning tokens.
    #[serde(default)]
    pub thought_tokens: Option<u64>,
}

/// `session/prompt` result.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResult {
    /// Why the turn ended (`end_turn`, `max_tokens`, `max_turn_requests`, `refusal`,
    /// `cancelled`).
    pub stop_reason: String,
    /// Token usage, when the agent gives it.
    #[serde(default)]
    pub usage: Option<PromptUsage>,
}

/// `session/cancel` parameters (a notification).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelParams {
    /// Session.
    pub session_id: String,
}

/// `session/set_mode` parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModeParams {
    /// Session.
    pub session_id: String,
    /// Mode to enter.
    pub mode_id: String,
}

// ---------------------------------------------------------------------------
// session/update
// ---------------------------------------------------------------------------

/// A content block of a chunk, read leniently (only text is used).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ContentIn {
    /// `text`, `image`, `audio`, `resource_link`, `resource`.
    #[serde(default)]
    pub r#type: Option<String>,
    /// Text of a `text` block.
    #[serde(default)]
    pub text: Option<String>,
}

/// `agent_message_chunk` / `agent_thought_chunk` / `user_message_chunk`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkUpdate {
    /// The fragment.
    pub content: ContentIn,
    /// Message identifier (unstable).
    #[serde(default)]
    pub message_id: Option<String>,
}

/// `tool_call`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdateIn {
    /// Identifier of the call.
    pub tool_call_id: String,
    /// Human title.
    #[serde(default)]
    pub title: Option<String>,
    /// `read`, `edit`, `delete`, `move`, `search`, `execute`, `think`, `fetch`,
    /// `switch_mode`, `other`.
    #[serde(default)]
    pub kind: Option<String>,
    /// `pending`, `in_progress`, `completed`, `failed`.
    #[serde(default)]
    pub status: Option<String>,
    /// Input of the call.
    #[serde(default)]
    pub raw_input: Option<Value>,
    /// Output of the call.
    #[serde(default)]
    pub raw_output: Option<Value>,
    /// Content produced.
    #[serde(default)]
    pub content: Option<Vec<Value>>,
}

/// `plan`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PlanUpdate {
    /// Entries (`content`, `priority`, `status`).
    pub entries: Vec<Value>,
}

/// `available_commands_update`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandsUpdate {
    /// Commands (`name`, `description`).
    pub available_commands: Vec<Value>,
}

/// `current_mode_update`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeUpdate {
    /// Mode now in effect.
    pub current_mode_id: String,
}

/// `usage_update` (**unstable**, NOT VERIFIED).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct UsageUpdate {
    /// Tokens in context.
    #[serde(default)]
    pub used: Option<u64>,
    /// Size of the window.
    #[serde(default)]
    pub size: Option<u64>,
}

/// A `session/update`, by its `sessionUpdate` discriminator.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// `agent_message_chunk`.
    AgentMessageChunk(ChunkUpdate),
    /// `agent_thought_chunk`.
    AgentThoughtChunk(ChunkUpdate),
    /// `user_message_chunk` (history replay).
    UserMessageChunk(ChunkUpdate),
    /// `tool_call`.
    ToolCall(ToolCallUpdateIn),
    /// `tool_call_update`.
    ToolCallUpdate(ToolCallUpdateIn),
    /// `plan`.
    Plan(PlanUpdate),
    /// `available_commands_update`.
    AvailableCommands(CommandsUpdate),
    /// `current_mode_update`.
    CurrentMode(ModeUpdate),
    /// `usage_update`.
    Usage(UsageUpdate),
    /// A discriminator this adapter does not know.
    Unknown(String),
}

/// `session/update` parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionNotification {
    /// Session the update is for (absent: the only one).
    pub session_id: Option<String>,
    /// The update.
    pub update: Update,
}

fn parse_as<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, String> {
    serde_json::from_value(value.clone()).map_err(|error| error.to_string())
}

impl SessionNotification {
    /// Parses the parameters of a `session/update`.
    pub fn parse(params: &Value) -> Result<Self, String> {
        let session_id = match params.get("sessionId") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) => Some(id.clone()),
            Some(_) => return Err("sessionId is not a string".to_owned()),
        };
        let update = params.get("update").ok_or("missing `update`")?;
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .ok_or("missing `update.sessionUpdate`")?;
        let update = match kind {
            "agent_message_chunk" => Update::AgentMessageChunk(parse_as(update)?),
            "agent_thought_chunk" => Update::AgentThoughtChunk(parse_as(update)?),
            "user_message_chunk" => Update::UserMessageChunk(parse_as(update)?),
            "tool_call" => Update::ToolCall(parse_as(update)?),
            "tool_call_update" => Update::ToolCallUpdate(parse_as(update)?),
            "plan" => Update::Plan(parse_as(update)?),
            "available_commands_update" => Update::AvailableCommands(parse_as(update)?),
            "current_mode_update" => Update::CurrentMode(parse_as(update)?),
            "usage_update" => Update::Usage(parse_as(update)?),
            other => Update::Unknown(other.to_owned()),
        };
        Ok(Self { session_id, update })
    }
}

// ---------------------------------------------------------------------------
// session/request_permission
// ---------------------------------------------------------------------------

/// One option of a permission request.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    /// Identifier to answer with.
    pub option_id: String,
    /// Label.
    #[serde(default)]
    pub name: String,
    /// `allow_once`, `allow_always`, `reject_once`, `reject_always`.
    pub kind: String,
}

/// `session/request_permission` parameters.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    /// Session.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The call concerned.
    pub tool_call: ToolCallUpdateIn,
    /// What the user may choose.
    pub options: Vec<PermissionOption>,
}

/// The answer to a permission request.
pub fn permission_selected(option_id: &str) -> Value {
    json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
}

/// The answer to a permission request that nobody answers (turn cancelled).
pub fn permission_cancelled() -> Value {
    json!({ "outcome": { "outcome": "cancelled" } })
}

/// Shape check of an answer to `session/request_permission` (what we send).
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct PermissionAnswer {
    outcome: PermissionOutcome,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum PermissionOutcome {
    Selected {
        #[serde(rename = "optionId")]
        option_id: String,
    },
    Cancelled,
}

// ---------------------------------------------------------------------------
// The kinds the schema describes
// ---------------------------------------------------------------------------

/// Every message kind of `schema/messages.json`. Kinds are `<method>.params`,
/// `<method>.result`, `session/update:<sessionUpdate>`, `<method>.response`
/// (our answer to an agent request).
pub const MESSAGE_KINDS: [&str; 21] = [
    "initialize.params",
    "initialize.result",
    "session/new.params",
    "session/new.result",
    "session/load.params",
    "session/load.result",
    "session/prompt.params",
    "session/prompt.result",
    "session/cancel.params",
    "session/set_mode.params",
    "session/update:agent_message_chunk",
    "session/update:agent_thought_chunk",
    "session/update:tool_call",
    "session/update:tool_call_update",
    "session/update:plan",
    "session/update:available_commands_update",
    "session/update:current_mode_update",
    "session/update:usage_update",
    "session/update:user_message_chunk",
    "session/request_permission.params",
    "session/request_permission.response",
];

/// Whether `value` is a well-formed message of `kind` for this adapter.
pub fn validate_kind(kind: &str, value: &Value) -> Result<(), String> {
    fn check<T: serde::de::DeserializeOwned>(value: &Value) -> Result<(), String> {
        parse_as::<T>(value).map(|_| ())
    }
    if let Some(update) = kind.strip_prefix("session/update:") {
        let parsed = SessionNotification::parse(value)?;
        let matches = matches!(
            (&parsed.update, update),
            (Update::AgentMessageChunk(_), "agent_message_chunk")
                | (Update::AgentThoughtChunk(_), "agent_thought_chunk")
                | (Update::UserMessageChunk(_), "user_message_chunk")
                | (Update::ToolCall(_), "tool_call")
                | (Update::ToolCallUpdate(_), "tool_call_update")
                | (Update::Plan(_), "plan")
                | (Update::AvailableCommands(_), "available_commands_update")
                | (Update::CurrentMode(_), "current_mode_update")
                | (Update::Usage(_), "usage_update")
        );
        return if matches {
            Ok(())
        } else {
            Err(format!("the update is not a `{update}`"))
        };
    }
    match kind {
        "initialize.params" => check::<InitializeParams>(value),
        "initialize.result" => check::<InitializeResult>(value),
        "session/new.params" => check::<NewSessionParams>(value),
        "session/new.result" => check::<NewSessionResult>(value),
        "session/load.params" => check::<LoadSessionParams>(value),
        "session/load.result" => {
            if value.is_null() {
                Ok(())
            } else {
                check::<LoadSessionResult>(value)
            }
        },
        "session/prompt.params" => check::<PromptParams>(value),
        "session/prompt.result" => check::<PromptResult>(value),
        "session/cancel.params" => check::<CancelParams>(value),
        "session/set_mode.params" => check::<SetModeParams>(value),
        "session/request_permission.params" => check::<PermissionRequest>(value),
        "session/request_permission.response" => check::<PermissionAnswer>(value),
        other => Err(format!("unknown message kind `{other}`")),
    }
}
