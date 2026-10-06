//! Shell tools (N20): `Bash`, `TaskStop`, `Monitor`.
//!
//! What they promise, and what they do **not**:
//!
//! - the command runs in its own process group, with an empty environment plus an
//!   allow-list and a `HOME` of its own: no secret of the host reaches it by inheritance;
//! - a timeout, a stop, a cancelled call and the end of the session kill the **whole group**;
//! - the working directory persists across calls of a session, and a `cd` that leaves the
//!   session's scope is undone.
//!
//! It is **not a sandbox**. A shell can read any file its operating-system user can read;
//! the scope confines the file tools, not what `cat /etc/passwd` does. The protection is
//! the approval policy of the harness (what may run) and the clean environment (what the
//! command inherits), and a real sandbox is a separate capability.

mod bash;
mod process;
mod tasks;

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use bash::BashTool;
pub use process::EnvPolicy;
pub use tasks::{MonitorTool, TaskStopTool};

use crate::files::Scope;
use crate::registry::ToolRegistry;

/// Timeout when the call gives none (Claude Code: 120 s).
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Longest timeout a call may ask for (Claude Code: 600 s).
pub const MAX_TIMEOUT_MS: u64 = 600_000;
/// Output above this many bytes is not returned inline.
pub const INLINE_OUTPUT_LIMIT: usize = 30_000;
/// A command whose output file grows past this is ended. The output goes to disk, and a loop
/// such as `yes` writes hundreds of megabytes a second: without a ceiling it fills the disk
/// of the host (N26).
pub const MAX_OUTPUT_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// What the shell tools share.
#[derive(Debug, Clone)]
pub struct ShellConfig {
    pub(crate) scope: Arc<Scope>,
    pub(crate) output_dir: PathBuf,
    pub(crate) home: PathBuf,
    pub(crate) env: EnvPolicy,
    pub(crate) shell: PathBuf,
    pub(crate) max_output_bytes: u64,
}

impl ShellConfig {
    /// Shell tools confined (for `cd`) to `scope`, keeping outputs in `output_dir` and
    /// giving commands a `HOME` inside it. For `Read` to open a task's output file,
    /// `output_dir` must be one of the scope's directories.
    pub fn new(scope: Arc<Scope>, output_dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let output_dir = output_dir.into();
        let home = output_dir.join("home");
        private_dir(&output_dir)?;
        private_dir(&home)?;
        Ok(Self {
            scope,
            output_dir,
            home,
            env: EnvPolicy::default(),
            shell: process::default_shell(),
            max_output_bytes: MAX_OUTPUT_FILE_BYTES,
        })
    }

    /// Replaces the environment policy.
    #[must_use]
    pub fn with_env(mut self, env: EnvPolicy) -> Self {
        self.env = env;
        self
    }

    /// Replaces the ceiling on one command's output file (default [`MAX_OUTPUT_FILE_BYTES`]).
    #[must_use]
    pub fn with_max_output_bytes(mut self, bytes: u64) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    /// Where outputs are kept.
    pub fn output_dir(&self) -> &Path {
        &self.output_dir
    }
}

fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Adds the shell tools to a registry.
pub fn register(registry: ToolRegistry, config: &ShellConfig) -> ToolRegistry {
    registry
        .with(BashTool::new(config.clone()))
        .with(TaskStopTool::new())
        .with(MonitorTool::new(config.clone()))
}
