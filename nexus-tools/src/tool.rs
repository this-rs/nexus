//! The contract of one tool.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use serde_json::Value;

/// MCP tool annotations. `read_only` is what the harness's policy reads: a read-only tool
/// can run in `plan_only` mode and is never asked about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Annotations {
    /// The tool changes nothing.
    pub read_only: bool,
    /// The tool may destroy data.
    pub destructive: bool,
    /// Calling it twice with the same arguments does the same as once.
    pub idempotent: bool,
    /// The tool reaches outside the machine (network).
    pub open_world: bool,
}

impl Annotations {
    /// A tool that only reads.
    pub const fn read_only() -> Self {
        Self {
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        }
    }
}

/// What a tool answers: text, and whether it is an error the **model** should see (as
/// opposed to a protocol error, which the server answers itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// The text the model reads.
    pub text: String,
    /// Whether the call failed from the tool's point of view.
    pub is_error: bool,
    /// Images that go with the text (`Read` of a picture): MCP image content blocks.
    pub images: Vec<ImageBlock>,
}

/// An image result: base64 data and its media type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBlock {
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub mime_type: String,
    /// The bytes, base64 (standard alphabet).
    pub data: String,
}

impl ToolResult {
    /// A successful result.
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            images: Vec::new(),
        }
    }

    /// Adds an image to the result.
    #[must_use]
    pub fn with_image(mut self, image: ImageBlock) -> Self {
        self.images.push(image);
        self
    }

    /// A failed result.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
            images: Vec::new(),
        }
    }
}

/// State that lives as long as a session and is shared by its tool calls (the files a
/// session has read, the working directory of its shell…). Typed: one value per type.
#[derive(Default)]
pub struct SessionState {
    values: Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl SessionState {
    /// The value of type `T`, created with `init` on first use.
    pub fn get_or_init<T: Any + Send + Sync>(&self, init: impl FnOnce() -> T) -> Arc<T> {
        let mut values = self.values.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = values
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Arc::new(init()) as Arc<dyn Any + Send + Sync>);
        Arc::clone(entry)
            .downcast::<T>()
            .unwrap_or_else(|_| unreachable!("keyed by TypeId"))
    }
}

impl std::fmt::Debug for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionState { .. }")
    }
}

/// Sends a JSON-RPC notification to the client of the session (stdio only: an HTTP request
/// has no channel back once it is answered).
#[derive(Clone)]
pub struct Notifier(Arc<dyn Fn(Value) + Send + Sync>);

impl Notifier {
    /// A notifier that calls `send` with each notification.
    pub fn new(send: impl Fn(Value) + Send + Sync + 'static) -> Self {
        Self(Arc::new(send))
    }

    /// Sends one notification.
    pub fn notify(&self, notification: Value) {
        (self.0)(notification);
    }
}

impl std::fmt::Debug for Notifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Notifier")
    }
}

/// What a call knows about its session.
#[derive(Debug, Clone)]
pub struct CallContext {
    /// The session id of the profile token.
    pub session_id: String,
    /// The session's state.
    pub state: Arc<SessionState>,
    /// Where to send notifications, when the transport has such a channel.
    pub notifier: Option<Notifier>,
}

impl CallContext {
    /// A context without a notification channel.
    pub fn new(session_id: impl Into<String>, state: Arc<SessionState>) -> Self {
        Self {
            session_id: session_id.into(),
            state,
            notifier: None,
        }
    }
}

/// One tool.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The canonical name (`Read`, `Edit`, `Bash`…): the harness's policy patterns are
    /// written with it.
    fn name(&self) -> &str;

    /// What the model reads to decide when to call it.
    fn description(&self) -> &str;

    /// JSON Schema of the arguments.
    fn input_schema(&self) -> Value;

    /// What the tool does to the world.
    fn annotations(&self) -> Annotations {
        Annotations::default()
    }

    /// Runs the tool. Arguments are an object (possibly empty).
    async fn call(&self, context: &CallContext, arguments: Value) -> ToolResult;
}
