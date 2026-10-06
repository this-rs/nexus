//! `Bash`: run a command in the session's shell directory.

use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::process::{self, GroupGuard, Running};
use super::tasks::{ShellState, adopt, new_id, shell_state};
use super::{DEFAULT_TIMEOUT_MS, INLINE_OUTPUT_LIMIT, MAX_TIMEOUT_MS, ShellConfig};
use crate::tool::{Annotations, CallContext, Tool, ToolResult};

/// Longest command accepted.
pub const MAX_COMMAND_BYTES: usize = 100_000;
const PREVIEW_BYTES: usize = 2_000;
const STOP_GRACE: Duration = Duration::from_millis(1500);

/// The `Bash` tool.
#[derive(Debug)]
pub struct BashTool {
    config: ShellConfig,
}

impl BashTool {
    /// A `Bash` for the given shell configuration.
    pub fn new(config: ShellConfig) -> Self {
        Self { config }
    }
}

/// `128 + signal` for a command killed by a signal, as shells report it.
pub fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| status.signal().map_or(-1, |s| 128 + s))
}

/// Starts `command` where the session's directory is, output to a new file in the output
/// directory. `cwd_file` is where the wrapper records the directory the command ended in.
pub(crate) fn start(
    config: &ShellConfig,
    state: &ShellState,
    command: &str,
    cwd_file: Option<&std::path::Path>,
) -> Result<(String, PathBuf, Running), String> {
    let cwd = state
        .cwd
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .unwrap_or_else(|| config.scope.cwd().to_path_buf());
    let id = new_id();
    let output = config.output_dir.join(format!("{id}.output"));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&output)
        .map_err(|e| format!("cannot create the output file: {e}"))?;
    let env = config.env.build(std::env::vars(), &config.home);
    let running = process::spawn(&config.shell, command, &cwd, &env, &file, cwd_file)
        .map_err(|e| format!("cannot start the command: {e}"))?;
    Ok((id, output, running))
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Runs a shell command and returns its output (stdout and stderr together). The working \
         directory persists between calls. `timeout` is in milliseconds (default 120000, at most \
         600000). With run_in_background the command keeps running and its output file path is \
         returned; read it with Read, stop it with TaskStop. The command runs with an empty \
         environment (PATH, LANG, TZ only) and a HOME of its own."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "description": {"type": "string"},
                "timeout": {"type": "integer", "minimum": 1},
                "run_in_background": {"type": "boolean"}
            },
            "required": ["command"]
        })
    }

    fn annotations(&self) -> Annotations {
        Annotations {
            destructive: true,
            open_world: true,
            ..Annotations::default()
        }
    }

    async fn call(&self, context: &CallContext, arguments: Value) -> ToolResult {
        let Some(command) = arguments.get("command").and_then(Value::as_str) else {
            return ToolResult::error("`command` is required and must be a string");
        };
        if command.trim().is_empty() {
            return ToolResult::error("`command` is empty");
        }
        if command.len() > MAX_COMMAND_BYTES {
            return ToolResult::error(format!(
                "`command` is longer than {MAX_COMMAND_BYTES} bytes"
            ));
        }
        let timeout_ms = arguments
            .get("timeout")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);
        let background = arguments
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let state = shell_state(&context.state);

        if background {
            let (id, output, running) = match start(&self.config, &state, command, None) {
                Ok(started) => started,
                Err(message) => return ToolResult::error(message),
            };
            adopt(
                &state,
                id.clone(),
                command,
                output.clone(),
                running,
                self.config.max_output_bytes,
            );
            return ToolResult::ok(format!(
                "Command running in background with ID: {id}. Output is being written to: {}. \
                 To check interim output, use Read on that file path; to stop it, use TaskStop.",
                output.display()
            ));
        }

        let cwd_file = self.config.output_dir.join(format!("{}.cwd", new_id()));
        let (_id, output, mut running) = match start(&self.config, &state, command, Some(&cwd_file))
        {
            Ok(started) => started,
            Err(message) => return ToolResult::error(message),
        };
        // From here, abandoning this call (cancelled request, session over) kills the group.
        let mut guard = GroupGuard::new(running.pgid);
        let pgid = running.pgid;
        let waited = tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            process::wait_capped(
                &mut running.child,
                pgid,
                &output,
                self.config.max_output_bytes,
            ),
        )
        .await;
        let mut capped = false;
        let (code, timed_out) = match waited {
            Ok(done) => {
                guard.disarm();
                capped = done.capped;
                match done.status {
                    Ok(status) => (exit_code(status), false),
                    Err(error) => {
                        let _ = std::fs::remove_file(&output);
                        return ToolResult::error(format!(
                            "waiting for the command failed: {error}"
                        ));
                    },
                }
            },
            Err(_) => {
                process::terminate_group(pgid, STOP_GRACE).await;
                let _ = running.child.wait().await;
                guard.disarm();
                (143, true)
            },
        };

        // Never read the whole file: it can be as large as the ceiling. The head is all that is
        // returned or previewed; the size comes from the file.
        let size = std::fs::metadata(&output)
            .map_or(0, |m| usize::try_from(m.len()).unwrap_or(usize::MAX));
        let mut text = read_head(&output, INLINE_OUTPUT_LIMIT + 1);
        let persisted = size > INLINE_OUTPUT_LIMIT;
        if !timed_out && let Some(note) = self.adopt_cwd(&state, &cwd_file) {
            text = format!("{}\n{note}", text.trim_end());
        }
        let _ = std::fs::remove_file(&cwd_file);
        if persisted {
            text = persisted_text(&text, size, &output);
        } else {
            let _ = std::fs::remove_file(&output);
        }
        let text = text.trim_end().to_owned();
        if capped {
            return ToolResult::error(format!(
                "Exit code {code}\nCommand ended: its output went past {}\n{text}",
                process::human_bytes(self.config.max_output_bytes)
            ));
        }
        if timed_out {
            let seconds = timeout_ms.div_ceil(1000);
            let mut out = format!("Exit code 143\nCommand timed out after {seconds}s");
            if !text.is_empty() {
                out.push('\n');
                out.push_str(&text);
            }
            return ToolResult::error(out);
        }
        match (code, text.is_empty()) {
            (0, true) => ToolResult::ok("(Bash completed with no output)"),
            (0, false) => ToolResult::ok(text),
            (code, true) => ToolResult::error(format!("Exit code {code}")),
            (code, false) => ToolResult::error(format!("Exit code {code}\n{text}")),
        }
    }
}

impl BashTool {
    /// Adopts the directory the command ended in, if it is inside the scope. A `cd` that
    /// leaves the scope is undone, and the output says so.
    fn adopt_cwd(&self, state: &Arc<ShellState>, cwd_file: &std::path::Path) -> Option<String> {
        let ended = std::fs::read_to_string(cwd_file).ok()?;
        let ended = ended.trim();
        if ended.is_empty() {
            return None;
        }
        let mut cwd = state
            .cwd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match self.config.scope.resolve(ended) {
            Ok(path) => {
                *cwd = Some(path);
                None
            },
            Err(_) => {
                let kept = cwd
                    .clone()
                    .unwrap_or_else(|| self.config.scope.cwd().to_path_buf());
                Some(format!("Shell cwd was reset to {}", kept.display()))
            },
        }
    }
}

/// The first `max` bytes of a file, as text; empty when it cannot be read.
fn read_head(path: &std::path::Path, max: usize) -> String {
    use std::io::Read;
    let mut bytes = Vec::new();
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.take(max as u64).read_to_end(&mut bytes);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Claude Code's shape for output too big to return: size, where the whole of it is, and a
/// preview of the first 2 KB, cut at a line.
fn persisted_text(text: &str, size: usize, path: &std::path::Path) -> String {
    let mut end = PREVIEW_BYTES.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut preview = &text[..end];
    if end < text.len()
        && let Some(newline) = preview.rfind('\n')
    {
        preview = &preview[..newline];
    }
    format!(
        "<persisted-output>\nOutput too large ({:.1}KB). Full output saved to: {}\n\nPreview (first 2KB):\n{preview}\n...\n</persisted-output>",
        size as f64 / 1024.0,
        path.display()
    )
}
