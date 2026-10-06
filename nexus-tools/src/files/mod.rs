//! File tools at parity with Claude Code 2.1.287 (N19): `Read`, `Write`, `Edit`, `NotebookEdit`, `Glob`, `Grep`.
//!
//! The behaviours, limits and messages come from recordings of the real CLI
//! (`claude-code-sdk-rs/tests/parity/claude-code-2.1.287/`), not from the tools' descriptions:
//! where the two disagree, the recording wins (docs/agent-tools-parity.md §3).

mod atomic;
mod edit;
mod notebook;
mod ordered;
mod pdf;
mod read;
mod scope;
mod search;
mod state;
mod write;

use std::path::PathBuf;
use std::sync::Arc;

pub use edit::EditTool;
pub use notebook::NotebookEditTool;
pub use read::ReadTool;
pub use scope::{Scope, ScopeError};
pub use search::{GlobTool, GrepTool};
pub use state::FileState;
pub use write::WriteTool;

use crate::registry::ToolRegistry;
use crate::tool::{CallContext, ToolResult};

/// The largest text file `Read` and `Edit` take whole. Both load the file into memory, so a
/// ceiling is what keeps one call on a huge file from exhausting the host (N26). Pictures and
/// PDFs have their own, smaller limits.
pub const MAX_TEXT_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// What every file tool shares: where it may go, and where backups go (if anywhere).
#[derive(Debug, Clone)]
pub struct FileConfig {
    pub(crate) scope: Arc<Scope>,
    pub(crate) backup_dir: Option<PathBuf>,
    pub(crate) max_text_bytes: u64,
}

impl FileConfig {
    /// Files confined to `scope`, no backups.
    pub fn new(scope: Scope) -> Self {
        Self {
            scope: Arc::new(scope),
            backup_dir: None,
            max_text_bytes: MAX_TEXT_FILE_BYTES,
        }
    }

    /// The scope, shared with the other tools of the session.
    pub fn scope(&self) -> Arc<Scope> {
        Arc::clone(&self.scope)
    }

    /// Replaces the largest text file `Read` and `Edit` accept (default [`MAX_TEXT_FILE_BYTES`]).
    #[must_use]
    pub fn with_max_text_bytes(mut self, bytes: u64) -> Self {
        self.max_text_bytes = bytes;
        self
    }

    /// The refusal for a file over the ceiling, naming a way out.
    pub(crate) fn too_large(&self, given: &str, size: u64, tool: &str) -> ToolResult {
        tool_error(format!(
            "{given} is {size} bytes, over the {} byte limit of {tool}. Read part of it with \
             Grep, or with Bash (head, sed -n) when that is allowed.",
            self.max_text_bytes
        ))
    }

    /// Copies a file here before overwriting it.
    #[must_use]
    pub fn with_backup_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.backup_dir = Some(dir.into());
        self
    }
}

/// Adds the file tools to a registry.
pub fn register(registry: ToolRegistry, config: &FileConfig) -> ToolRegistry {
    registry
        .with(ReadTool::new(config.clone()))
        .with(WriteTool::new(config.clone()))
        .with(EditTool::new(config.clone()))
        .with(NotebookEditTool::new(config.clone()))
        .with(GlobTool::new(config.clone()))
        .with(GrepTool::new(config.clone()))
}

pub(crate) fn file_state(context: &CallContext) -> Arc<FileState> {
    context.state.get_or_init(FileState::default)
}

/// A failure, in the `<tool_use_error>` wrapper Claude Code uses for write-side tools.
pub(crate) fn tool_error(message: impl std::fmt::Display) -> ToolResult {
    ToolResult::error(format!("<tool_use_error>{message}</tool_use_error>"))
}

pub(crate) const NOT_READ: &str = "File has not been read yet. Read it first before writing to it.";
pub(crate) const MODIFIED: &str = "File has been modified since read, either by the user or by a linter. Read it again before attempting to write it.";

pub(crate) fn required_str<'a>(
    arguments: &'a serde_json::Value,
    name: &str,
) -> Result<&'a str, String> {
    arguments
        .get(name)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("`{name}` is required and must be a string"))
}
