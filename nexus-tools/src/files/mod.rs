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
use std::time::Duration;

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

/// How long one `Glob` or `Grep` may walk when the scope is unscoped (`--unscoped`). A bounded
/// scope walks only its own directories; an unscoped one can be pointed at `/` and would walk
/// the whole disk. Past this, the walk stops and the answer says it is partial.
pub const UNSCOPED_SEARCH_TIME_LIMIT: Duration = Duration::from_secs(30);

/// What every file tool shares: where it may go, and where backups go (if anywhere).
#[derive(Debug, Clone)]
pub struct FileConfig {
    pub(crate) scope: Arc<Scope>,
    pub(crate) backup_dir: Option<PathBuf>,
    pub(crate) max_text_bytes: u64,
    /// How long one `Glob` or `Grep` may walk; `None`: until the walk ends.
    pub(crate) search_time_limit: Option<Duration>,
}

impl FileConfig {
    /// Files confined to `scope`, no backups. An unscoped scope gets a time limit on its
    /// searches ([`UNSCOPED_SEARCH_TIME_LIMIT`]).
    pub fn new(scope: Scope) -> Self {
        let search_time_limit = scope.is_unscoped().then_some(UNSCOPED_SEARCH_TIME_LIMIT);
        Self {
            scope: Arc::new(scope),
            backup_dir: None,
            max_text_bytes: MAX_TEXT_FILE_BYTES,
            search_time_limit,
        }
    }

    /// Replaces how long one `Glob` or `Grep` may walk (`None`: no limit).
    #[must_use]
    pub fn with_search_time_limit(mut self, limit: Option<Duration>) -> Self {
        self.search_time_limit = limit;
        self
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

/// The refusal for a path that exists but is not a regular file nor a directory.
pub(crate) fn not_regular(given: &str) -> String {
    format!("Not a regular file (a device, a pipe or a socket), it cannot be read: {given}")
}

/// Whether `path` exists and is neither a regular file nor a directory: `/dev/zero` never ends,
/// a FIFO blocks the call until someone writes, `/dev/fd/0` is this server's own input. Their
/// size says 0, so no size check stops them. Checked before the file is opened (opening a FIFO
/// already blocks).
pub(crate) fn is_special(path: &std::path::Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| !meta.is_file() && !meta.is_dir())
}

/// At most `max` bytes of the regular file `path`, read through `take`: a file that grew past
/// the size checked before is cut rather than read whole. `Err(InvalidData)` when it holds more
/// than `max` bytes, `Err(InvalidInput)` when it is not a regular file.
pub(crate) fn read_regular(path: &std::path::Path, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    if !std::fs::metadata(path)?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(max.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("over the {max} byte limit"),
        ));
    }
    Ok(bytes)
}
