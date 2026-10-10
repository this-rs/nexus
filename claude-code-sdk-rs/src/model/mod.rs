//! Model endpoints (contract §12, decision A2): a bare model behind an HTTP API,
//! as opposed to an [`AgentProvider`](crate::agent::AgentProvider) that runs a whole
//! agent. `ModelEndpoint` is a distinct trait: the native harness composes an
//! `AgentProvider` on top of one.
//!
//! The wire-neutral types, [`quirks`] and [`pricing`] are always compiled (the
//! provider registry holds them in its instance configuration); the HTTP client
//! side (`guard`, `sse`, `wire`, `openai`) needs the `provider-native` cargo
//! feature (it pulls `reqwest`).
//!
//! | File | Role |
//! |---|---|
//! | `mod.rs` | the trait and the wire-neutral types |
//! | `quirks.rs` | per-instance dialect flags and presets (A39) |
//! | `pricing.rs` | the single price table (A1, A21) |
//! | `sse.rs` | incremental SSE decoder |
//! | `guard.rs` | endpoint guard: https, internal ranges, DNS pinning (A36) |
//! | `wire.rs` | request JSON and stream-to-chunks assembly (pure) |
//! | `openai.rs` | [`OpenAiEndpoint`]: chat/completions over HTTP + SSE, probe (A30) |

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::agent::{ModelInfo, ProviderError, ProviderHealth, Usage};

#[cfg(feature = "provider-native")]
pub mod guard;
#[cfg(feature = "provider-native")]
pub mod openai;
pub mod pricing;
pub mod quirks;
#[cfg(feature = "provider-native")]
pub mod sse;
#[cfg(feature = "provider-native")]
pub(crate) mod wire;

#[cfg(feature = "provider-native")]
pub use guard::{
    CheckedEndpoint, DnsResolver, EndpointGuard, IpClass, SystemResolver, classify_ip,
};
#[cfg(feature = "provider-native")]
pub use openai::{OpenAiEndpoint, OpenAiEndpointConfig};
pub use pricing::PriceTable;
pub use quirks::{EndpointQuirks, ReasoningField};
#[cfg(feature = "provider-native")]
pub use sse::{SseDecoder, SseEvent};

/// Stream of chunks of one completion. Errors end the stream.
pub type CompletionStream =
    Pin<Box<dyn Stream<Item = Result<CompletionChunk, ProviderError>> + Send>>;

/// Author of a [`ChatMessage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Role {
    /// Instructions.
    System,
    /// The human.
    User,
    /// The model.
    Assistant,
    /// A tool result.
    Tool,
}

impl Role {
    /// The role name used on the OpenAI wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// A tool call, whole: `arguments` is the complete JSON text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallChunk {
    /// Call identifier, echoed in the tool message that answers it.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments as one complete JSON document, in a string.
    pub arguments: String,
}

/// An image attached to a message (user messages): sent as an `image_url` part
/// carrying a `data:` URL, kept in the transcript as is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePart {
    /// MIME type (`image/png`, `image/jpeg`…).
    pub media_type: String,
    /// Base64 payload, without a `data:` prefix.
    pub data_base64: String,
}

impl ImagePart {
    /// The `data:` URL of the image, as the OpenAI wire takes it.
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data_base64)
    }
}

/// What stands for an image sent to a model that has no vision (a history
/// written on another model, after `set_model`).
pub const IMAGE_OMITTED: &str = "[image omitted: the active model has no vision]";

/// One message of the transcript sent to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Author.
    pub role: Role,
    /// Text, when the message has some.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Images of the message, after its text (user messages). On the wire the
    /// message becomes a list of parts; in the transcript they stay whole.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImagePart>,
    /// Reasoning the model produced for this message (assistant messages only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Tool calls requested by this assistant message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallChunk>,
    /// For a `tool` message: the call it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    fn with(role: Role, content: Option<String>) -> Self {
        Self {
            role,
            content,
            images: Vec::new(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// A system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self::with(Role::System, Some(content.into()))
    }

    /// A user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self::with(Role::User, Some(content.into()))
    }

    /// A user message with images after its text.
    pub fn user_with_images(content: impl Into<String>, images: Vec<ImagePart>) -> Self {
        let mut message = Self::with(Role::User, Some(content.into()));
        message.images = images;
        message
    }

    /// The message for a model without vision: each image replaced by
    /// [`IMAGE_OMITTED`] after the text. Said, never dropped in silence.
    pub fn images_as_text(&self) -> Self {
        let mut message = self.clone();
        if message.images.is_empty() {
            return message;
        }
        let mut text = message.content.take().unwrap_or_default();
        for image in std::mem::take(&mut message.images) {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("{IMAGE_OMITTED} ({})", image.media_type));
        }
        message.content = Some(text);
        message
    }

    /// An assistant text message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::with(Role::Assistant, Some(content.into()))
    }

    /// An assistant message that only requests tool calls.
    pub fn assistant_tool_calls(tool_calls: Vec<ToolCallChunk>) -> Self {
        let mut message = Self::with(Role::Assistant, None);
        message.tool_calls = tool_calls;
        message
    }

    /// A tool result answering `tool_call_id`.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        let mut message = Self::with(Role::Tool, Some(content.into()));
        message.tool_call_id = Some(tool_call_id.into());
        message
    }

    /// Attaches the reasoning produced with this message.
    pub fn with_reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.reasoning = Some(reasoning.into());
        self
    }
}

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Tool name.
    pub name: String,
    /// What the tool does, for the model.
    #[serde(default)]
    pub description: String,
    /// JSON Schema of the arguments.
    pub parameters: serde_json::Value,
}

/// A completion request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// Model identifier.
    pub model: String,
    /// The transcript.
    pub messages: Vec<ChatMessage>,
    /// Tools offered; empty means none.
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    /// Output token limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Whether several tool calls may come in one turn; `None` leaves it to the
    /// instance quirks (and then to the server).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Ask for a stream. `complete` always streams on the wire; `false` is
    /// reserved and currently treated like `true`.
    #[serde(default = "default_true")]
    pub stream: bool,
}

fn default_true() -> bool {
    true
}

impl CompletionRequest {
    /// A streaming request without tools.
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            parallel_tool_calls: None,
            stream: true,
        }
    }
}

/// Why a completion ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FinishReason {
    /// The model finished its answer.
    Stop,
    /// The output limit was reached.
    Length,
    /// The model asks for tool calls.
    ToolCalls,
    /// The server filtered the answer.
    ContentFilter,
    /// Any other server-specific reason.
    Other(String),
}

/// One piece of a completion. Whole tool calls and fragmented ones both come out
/// as a single [`CompletionChunk::ToolCall`] (A39).
///
/// Order: texts and reasoning as they come, then the tool calls, then
/// `Usage` when the server reports it, and `Finish` last.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompletionChunk {
    /// A piece of the answer.
    Text(String),
    /// A piece of reasoning.
    Reasoning(String),
    /// A complete tool call.
    ToolCall(ToolCallChunk),
    /// Token usage of the whole completion.
    Usage(Usage),
    /// End of the completion.
    Finish(FinishReason),
}

/// Result of the tool-call probe (A30), cached per (instance, model).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointProbe {
    /// The model answered the probe with a tool call.
    pub tools: bool,
    /// Whether parallel tool calls are known to work (`None`: not verified; the
    /// instance's declared quirk is reported).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tools: Option<bool>,
    /// Name of the reasoning field the endpoint uses, when it emitted reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_field: Option<String>,
    /// Context window the endpoint's catalogue reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Whether the model takes images: what the catalogue says, else what the
    /// one-pixel probe found, else the instance's declaration; `None` when
    /// nothing said (the harness then claims nothing: `images: false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<bool>,
    /// When the probe ran, milliseconds since the Unix epoch.
    pub checked_at_ms: u64,
}

/// A bare model behind an API.
#[async_trait]
pub trait ModelEndpoint: Send + Sync {
    /// Identifier of the instance.
    fn id(&self) -> &str;

    /// State of the endpoint. Never fails: failure is a value.
    async fn health(&self) -> ProviderHealth;

    /// Models the endpoint serves.
    async fn models(&self) -> Result<Vec<ModelInfo>, ProviderError>;

    /// Starts a completion and returns its chunks.
    async fn complete(&self, request: CompletionRequest)
    -> Result<CompletionStream, ProviderError>;

    /// Tool-call probe (A30): cached per (instance, model).
    async fn probe(&self, model: &str) -> Result<EndpointProbe, ProviderError>;

    /// Whether `model` takes images, when something says so (see
    /// [`EndpointProbe::images`]). Asked on its own for a model whose tool probe
    /// failed: a model without tools may still have vision. `None` by default.
    async fn probe_images(&self, _model: &str) -> Option<bool> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_serialises_with_a_type_tag() {
        let json = serde_json::to_value(CompletionChunk::Text("hi".into())).unwrap();
        assert_eq!(json, serde_json::json!({"type": "text", "data": "hi"}));
        let back: CompletionChunk = serde_json::from_value(json).unwrap();
        assert_eq!(back, CompletionChunk::Text("hi".into()));
    }

    #[test]
    fn message_helpers_set_the_expected_fields() {
        let message = ChatMessage::tool("c1", "ok");
        assert_eq!(message.role, Role::Tool);
        assert_eq!(message.tool_call_id.as_deref(), Some("c1"));
        assert!(ChatMessage::assistant_tool_calls(vec![]).content.is_none());
    }

    #[test]
    fn images_as_text_leaves_a_marker_per_image() {
        let image = ImagePart {
            media_type: "image/png".into(),
            data_base64: "AAAA".into(),
        };
        let message = ChatMessage::user_with_images("look", vec![image.clone(), image]);
        let text = message.images_as_text();
        assert!(text.images.is_empty());
        assert_eq!(
            text.content.as_deref(),
            Some(
                "look\n[image omitted: the active model has no vision] (image/png)\n[image omitted: the active model has no vision] (image/png)"
            )
        );
        let plain = ChatMessage::user("x");
        assert_eq!(plain.images_as_text(), plain);
    }
}
