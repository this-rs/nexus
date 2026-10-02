//! A stand-in for the `claude` CLI.
//!
//! # Why a script and not a mock object
//!
//! `claude-code-api` has **no transport abstraction**. `ClaudeManager`,
//! `InteractiveSessionManager` and `SessionProcessManager` each call
//! `tokio::process::Command::new(&self.claude_command)` directly, and `ChatState`
//! stores them as concrete `Arc<ClaudeManager>` / `Arc<ProcessPool>` /
//! `Arc<InteractiveSessionManager>`. The sibling crate `nexus-claude` does expose
//! a `Transport` trait with a mock implementation, but nothing in this crate
//! references it.
//!
//! The **only** injection point is therefore the `claude.command` string from
//! `Settings`. [`FakeClaudeCli`] exploits exactly that seam: it writes a tiny
//! shell script (`.sh` on Unix, `.cmd` on Windows) into a `TempDir`, which prints
//! a canned `stream-json` transcript on stdout and exits. No network, no real
//! CLI, nothing long-lived — the process is gone before the assertion runs.
//!
//! ```no_run
//! let cli = FakeClaudeCli::replying("Bonjour");
//! let server = test_app_with(TestSettings::new().command(cli.command()).build()).await;
//! ```
//!
//! Keep the [`FakeClaudeCli`] alive for as long as the server: dropping it
//! deletes the temporary directory holding the script.

use std::path::PathBuf;

use claude_code_api::models::claude::ClaudeCodeOutput;
use tempfile::TempDir;

use super::claude_output;

/// A single-shot fake `claude` executable backed by a temporary directory.
pub struct FakeClaudeCli {
    dir: TempDir,
    script: PathBuf,
}

impl FakeClaudeCli {
    /// A CLI that emits `transcript` (one JSON object per line) and exits 0.
    pub fn new(transcript: &[ClaudeCodeOutput]) -> Self {
        Self::with_stdout_and_code(&claude_output::ndjson(transcript), 0)
    }

    /// A CLI that answers every prompt with a single assistant text block
    /// followed by a `result` message — the ordinary happy path.
    pub fn replying(text: &str) -> Self {
        Self::new(&[
            claude_output::assistant_text(text),
            claude_output::result_success(),
        ])
    }

    /// A CLI that produces one assistant `tool_use` block and then a `result`.
    pub fn calling_tool(id: &str, name: &str, input: serde_json::Value) -> Self {
        Self::new(&[
            claude_output::assistant_tool_use(id, name, input),
            claude_output::result_success(),
        ])
    }

    /// A CLI that exits 0 without printing anything.
    ///
    /// The gateway sees an immediately-closed stdout, so the completion comes
    /// back `200` with empty content — the "CLI said nothing" path.
    pub fn silent() -> Self {
        Self::with_stdout_and_code("", 0)
    }

    /// A CLI that prints `line` on stdout — use it for malformed output, since
    /// `ClaudeManager` logs and skips lines it cannot parse as `ClaudeCodeOutput`.
    pub fn emitting_raw(stdout: &str) -> Self {
        Self::with_stdout_and_code(stdout, 0)
    }

    /// A CLI that prints nothing and exits with `code`.
    pub fn failing(code: i32) -> Self {
        Self::with_stdout_and_code("", code)
    }

    /// Lowest-level constructor: exact stdout bytes and exit code.
    pub fn with_stdout_and_code(stdout: &str, exit_code: i32) -> Self {
        let dir = tempfile::tempdir().expect("create tempdir for fake claude CLI");

        let payload = dir.path().join("payload.ndjson");
        std::fs::write(&payload, stdout).expect("write fake claude payload");

        let script =
            super::fake_exec::plant_fake_cli(dir.path(), &script_body(&payload, exit_code));

        Self { dir, script }
    }

    /// The value to feed to `TestSettings::command` / `settings.claude.command`.
    pub fn command(&self) -> String {
        self.script.to_string_lossy().into_owned()
    }

    /// The temporary directory holding the script and its payload.
    pub fn dir(&self) -> &std::path::Path {
        self.dir.path()
    }
}

fn script_body(payload: &std::path::Path, exit_code: i32) -> String {
    let payload = payload.display();
    if cfg!(windows) {
        // `type` on an empty file prints nothing, which is what `silent()` wants.
        format!("@echo off\r\ntype \"{payload}\"\r\nexit /b {exit_code}\r\n")
    } else {
        format!("#!/bin/sh\ncat '{payload}'\nexit {exit_code}\n")
    }
}
