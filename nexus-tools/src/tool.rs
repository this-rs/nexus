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
}

impl ToolResult {
    /// A successful result.
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    /// A failed result.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
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

/// What a call knows about its session.
#[derive(Debug, Clone)]
pub struct CallContext {
    /// The session id of the profile token.
    pub session_id: String,
    /// The session's state.
    pub state: Arc<SessionState>,
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
