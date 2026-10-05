//! `Edit`: replace an exact string in a file the session has read.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::state::Unsafe;
use super::{FileConfig, MODIFIED, NOT_READ, atomic, file_state, required_str, tool_error};
use crate::tool::{Annotations, CallContext, Tool, ToolResult};

const CURRENT: &str = "(file state is current in your context — no need to Read it back)";

/// The `Edit` tool.
#[derive(Debug)]
pub struct EditTool {
    config: FileConfig,
}

impl EditTool {
    /// An `Edit` confined to the config's scope.
    pub fn new(config: FileConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Replaces an exact string in a file you have read. Fails if the string is absent, if it \
         is not unique (unless replace_all is true) or if old_string equals new_string. The text \
         is matched literally: do not include the line-number prefix that Read adds."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean", "default": false}
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    fn annotations(&self) -> Annotations {
        Annotations {
            destructive: true,
            ..Annotations::default()
        }
    }

    async fn call(&self, context: &CallContext, arguments: Value) -> ToolResult {
        let strings = (
            required_str(&arguments, "file_path"),
            required_str(&arguments, "old_string"),
            required_str(&arguments, "new_string"),
        );
        let (given, old, new) = match strings {
            (Ok(path), Ok(old), Ok(new)) => (path, old, new),
            (Err(message), ..) | (_, Err(message), _) | (.., Err(message)) => {
                return tool_error(message);
            },
        };
        let replace_all = arguments
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let path = match self.config.scope.resolve(given) {
            Ok(path) => path,
            Err(error) => return tool_error(error),
        };
        let state = file_state(context);
        let _serialised = state.writing.lock().await;
        let text = match std::fs::read(&path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => return tool_error(format!("{given} is not valid UTF-8 text")),
            },
            Err(_) if path.is_dir() => {
                return tool_error(format!(
                    "EISDIR: illegal operation on a directory, read '{given}'"
                ));
            },
            Err(_) => {
                return tool_error(format!(
                    "File does not exist. Note: your current working directory is {}.",
                    self.config.scope.cwd().display()
                ));
            },
        };
        match state.check(&path) {
            Ok(()) => {},
            Err(Unsafe::NotRead) => return tool_error(NOT_READ),
            Err(Unsafe::Modified) => return tool_error(MODIFIED),
        }
        if old == new {
            return tool_error(
                "No changes to make: old_string and new_string are exactly the same.",
            );
        }
        let found = if old.is_empty() {
            0
        } else {
            text.matches(old).count()
        };
        if found == 0 {
            return tool_error(format!(
                "String to replace not found in file.\nString: {old}"
            ));
        }
        if found > 1 && !replace_all {
            return tool_error(format!(
                "Found {found} matches of the string to replace, but replace_all is false. To replace all occurrences, set replace_all to true. To replace only one occurrence, please provide more context to uniquely identify the instance.\nString: {old}"
            ));
        }
        let edited = if replace_all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
        if let Err(error) =
            atomic::write(&path, edited.as_bytes(), self.config.backup_dir.as_deref())
        {
            return tool_error(format!("Could not write {given}: {error}"));
        }
        state.record(&path);
        if replace_all {
            ToolResult::ok(format!(
                "The file {given} has been updated. All occurrences were successfully replaced. {CURRENT}"
            ))
        } else {
            ToolResult::ok(format!(
                "The file {given} has been updated successfully. {CURRENT}"
            ))
        }
    }
}
