//! `NotebookEdit`: replace, insert or delete a cell of a Jupyter notebook, by cell id.

use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::ordered::Json;
use super::state::Unsafe;
use super::{FileConfig, MODIFIED, NOT_READ, atomic, file_state, required_str, tool_error};
use crate::tool::{Annotations, CallContext, Tool, ToolResult};

/// The `NotebookEdit` tool.
#[derive(Debug)]
pub struct NotebookEditTool {
    config: FileConfig,
}

impl NotebookEditTool {
    /// A `NotebookEdit` confined to the config's scope.
    pub fn new(config: FileConfig) -> Self {
        Self { config }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Replace,
    Insert,
    Delete,
}

#[async_trait]
impl Tool for NotebookEditTool {
    fn name(&self) -> &str {
        "NotebookEdit"
    }

    fn description(&self) -> &str {
        "Replaces, inserts (after `cell_id`, or first without it) or deletes a cell of a Jupyter \
         notebook you have read. `edit_mode` is replace (default), insert or delete."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "notebook_path": {"type": "string"},
                "cell_id": {"type": "string"},
                "new_source": {"type": "string"},
                "cell_type": {"type": "string", "enum": ["code", "markdown"]},
                "edit_mode": {"type": "string", "enum": ["replace", "insert", "delete"]}
            },
            "required": ["notebook_path", "new_source"]
        })
    }

    fn annotations(&self) -> Annotations {
        Annotations {
            destructive: true,
            ..Annotations::default()
        }
    }

    async fn call(&self, context: &CallContext, arguments: Value) -> ToolResult {
        let given = match required_str(&arguments, "notebook_path") {
            Ok(path) => path,
            Err(message) => return tool_error(message),
        };
        let new_source = match required_str(&arguments, "new_source") {
            Ok(source) => source,
            Err(message) => return tool_error(message),
        };
        let cell_id = arguments.get("cell_id").and_then(Value::as_str);
        let cell_type = arguments.get("cell_type").and_then(Value::as_str);
        let mode = match arguments.get("edit_mode").and_then(Value::as_str) {
            None | Some("replace") => Mode::Replace,
            Some("insert") => Mode::Insert,
            Some("delete") => Mode::Delete,
            Some(other) => {
                return tool_error(format!(
                    "Edit mode must be replace, insert or delete, not `{other}`."
                ));
            },
        };
        if let Some(kind) = cell_type
            && kind != "code"
            && kind != "markdown"
        {
            return tool_error(format!("cell_type must be code or markdown, not `{kind}`."));
        }
        let path = match self.config.scope.resolve(given) {
            Ok(path) => path,
            Err(error) => return tool_error(error),
        };
        if path.extension().is_none_or(|e| e != "ipynb") {
            return tool_error("File must be a Jupyter notebook (.ipynb file).");
        }
        let state = file_state(context);
        let _serialised = state.writing.lock().await;
        let Ok(text) = std::fs::read_to_string(&path) else {
            return tool_error(format!(
                "File does not exist. Note: your current working directory is {}.",
                self.config.scope.cwd().display()
            ));
        };
        match state.check(&path) {
            Ok(()) => {},
            Err(Unsafe::NotRead) => return tool_error(NOT_READ),
            Err(Unsafe::Modified) => return tool_error(MODIFIED),
        }
        let Some(mut document) = Json::parse(&text) else {
            return tool_error("Notebook is not valid JSON.");
        };
        let Some(Json::Array(cells)) = document.get_mut("cells") else {
            return tool_error("Notebook has no `cells` array.");
        };

        let position = cell_id.and_then(|wanted| {
            cells.iter().enumerate().position(|(index, cell)| {
                cell.get("id")
                    .and_then(Json::as_str)
                    .map_or_else(|| index.to_string() == wanted, |id| id == wanted)
            })
        });
        if let (Some(wanted), None) = (cell_id, position) {
            return tool_error(format!("Cell with ID \"{wanted}\" not found in notebook."));
        }
        let answer = match mode {
            Mode::Replace => {
                let Some(at) = position else {
                    return tool_error("cell_id is required to replace a cell.");
                };
                let cell = &mut cells[at];
                cell.set("source", Json::string(new_source));
                if let Some(kind) = cell_type {
                    cell.set("cell_type", Json::string(kind));
                }
                // The outputs belonged to the old source: they would now describe code that
                // is no longer there.
                if cell.get("cell_type").and_then(Json::as_str) == Some("code") {
                    if cell.has("outputs") {
                        cell.set("outputs", Json::empty_array());
                    }
                    if cell.has("execution_count") {
                        cell.set("execution_count", Json::Null);
                    }
                }
                format!(
                    "Updated cell {} with {new_source}",
                    cell_id.unwrap_or_default()
                )
            },
            Mode::Insert => {
                let Some(kind) = cell_type else {
                    return tool_error("cell_type is required when inserting a cell.");
                };
                let id = fresh_id(&text, new_source, cells.len());
                let mut cell = Json::empty_object();
                cell.set("cell_type", Json::string(kind));
                cell.set("id", Json::string(id.clone()));
                cell.set("source", Json::string(new_source));
                cell.set("metadata", Json::empty_object());
                if kind == "code" {
                    cell.set("execution_count", Json::Null);
                    cell.set("outputs", Json::empty_array());
                }
                cells.insert(position.map_or(0, |p| p + 1), cell);
                format!("Inserted cell {id} with {new_source}")
            },
            Mode::Delete => {
                let Some(at) = position else {
                    return tool_error("cell_id is required to delete a cell.");
                };
                cells.remove(at);
                format!("Deleted cell {}", cell_id.unwrap_or_default())
            },
        };

        let mut out = document.pretty(" ");
        if text.ends_with('\n') {
            out.push('\n');
        }
        if let Err(error) = atomic::write(&path, out.as_bytes(), self.config.backup_dir.as_deref())
        {
            return tool_error(format!("Could not write {given}: {error}"));
        }
        state.record(&path);
        ToolResult::ok(answer)
    }
}

/// Eight hex characters, unique in practice within the notebook.
fn fresh_id(notebook: &str, source: &str, cells: usize) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut hash = Sha256::new();
    hash.update(notebook.as_bytes());
    hash.update(source.as_bytes());
    hash.update(nanos.to_le_bytes());
    hash.update(cells.to_le_bytes());
    hash.finalize()[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
