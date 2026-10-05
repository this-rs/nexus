//! `Write`: create a file or replace one the session has read.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::state::Unsafe;
use super::{FileConfig, MODIFIED, NOT_READ, atomic, file_state, required_str, tool_error};
use crate::tool::{Annotations, CallContext, Tool, ToolResult};

const CURRENT: &str = "(file state is current in your context — no need to Read it back)";

/// The `Write` tool.
#[derive(Debug)]
pub struct WriteTool {
    config: FileConfig,
}

impl WriteTool {
    /// A `Write` confined to the config's scope.
    pub fn new(config: FileConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Writes a file. Overwriting an existing file requires having read it in this session; \
         a new file is created with its parent directories."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["file_path", "content"]
        })
    }

    fn annotations(&self) -> Annotations {
        Annotations {
            destructive: true,
            idempotent: true,
            ..Annotations::default()
        }
    }

    async fn call(&self, context: &CallContext, arguments: Value) -> ToolResult {
        let given = match required_str(&arguments, "file_path") {
            Ok(path) => path,
            Err(message) => return tool_error(message),
        };
        let content = match required_str(&arguments, "content") {
            Ok(content) => content,
            Err(message) => return tool_error(message),
        };
        let path = match self.config.scope.resolve(given) {
            Ok(path) => path,
            Err(error) => return tool_error(error),
        };
        if path.is_dir() {
            return tool_error(format!(
                "EISDIR: illegal operation on a directory, write '{given}'"
            ));
        }
        let state = file_state(context);
        let _serialised = state.writing.lock().await;
        let existed = path.exists();
        if existed {
            match state.check(&path) {
                Ok(()) => {},
                Err(Unsafe::NotRead) => return tool_error(NOT_READ),
                Err(Unsafe::Modified) => return tool_error(MODIFIED),
            }
        }
        if let Err(error) =
            atomic::write(&path, content.as_bytes(), self.config.backup_dir.as_deref())
        {
            return tool_error(format!("Could not write {given}: {error}"));
        }
        state.record(&path);
        if existed {
            ToolResult::ok(format!(
                "The file {given} has been updated successfully. {CURRENT}"
            ))
        } else {
            ToolResult::ok(format!("File created successfully at: {given} {CURRENT}"))
        }
    }
}
