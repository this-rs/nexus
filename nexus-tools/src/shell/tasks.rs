//! Background commands: their registry, `TaskStop` and `Monitor`.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use super::ShellConfig;
use super::process::{self, Running};
use crate::tool::{Annotations, CallContext, Notifier, SessionState, Tool, ToolResult};

/// Where a background command stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Exited(i32),
    /// Ended by `TaskStop`, a timeout or the end of the session.
    Stopped,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => f.write_str("running"),
            Self::Exited(code) => write!(f, "exited with code {code}"),
            Self::Stopped => f.write_str("stopped"),
        }
    }
}

/// One background command.
pub struct Task {
    pub id: String,
    pub command: String,
    pub output: PathBuf,
    pgid: u32,
    status: Mutex<Status>,
    finished: Notify,
}

impl Task {
    pub fn status(&self) -> Status {
        *self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, new: Status, only_if_running: bool) {
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        if !only_if_running || *status == Status::Running {
            *status = new;
        }
        drop(status);
        self.finished.notify_waiters();
    }

    /// Stops the command and everything it started. Safe to call twice.
    pub async fn stop(&self) {
        if self.status() != Status::Running {
            return;
        }
        // Mark it first: the watcher must not record the signal as a normal exit.
        self.set(Status::Stopped, true);
        process::terminate_group(self.pgid, Duration::from_millis(1500)).await;
    }
}

/// The background tasks of one session, and the session's working directory.
#[derive(Default)]
pub struct ShellState {
    pub(crate) cwd: Mutex<Option<PathBuf>>,
    tasks: Mutex<HashMap<String, Arc<Task>>>,
}

impl ShellState {
    pub fn get(&self, id: &str) -> Option<Arc<Task>> {
        self.tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }
}

impl Drop for ShellState {
    /// The session is over: nothing it started may keep running.
    fn drop(&mut self) {
        let tasks = self.tasks.get_mut().unwrap_or_else(PoisonError::into_inner);
        for task in tasks.values() {
            if task.status() == Status::Running {
                process::kill_group_now(task.pgid);
            }
        }
    }
}

pub(crate) fn shell_state(state: &SessionState) -> Arc<ShellState> {
    state.get_or_init(ShellState::default)
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A short unique id (9 hex characters) for a task and its output file.
pub fn new_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut hash = Sha256::new();
    hash.update(nanos.to_le_bytes());
    hash.update(COUNTER.fetch_add(1, Ordering::SeqCst).to_le_bytes());
    hash.update(std::process::id().to_le_bytes());
    hash.finalize()[..5]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..9]
        .to_owned()
}

/// Registers a started command as a background task and watches it end.
pub fn adopt(
    state: &ShellState,
    id: String,
    command: &str,
    output: PathBuf,
    running: Running,
    max_output_bytes: u64,
) -> Arc<Task> {
    let task = Arc::new(Task {
        id: id.clone(),
        command: command.to_owned(),
        output,
        pgid: running.pgid,
        status: Mutex::new(Status::Running),
        finished: Notify::new(),
    });
    state
        .tasks
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, Arc::clone(&task));
    let watched = Arc::clone(&task);
    let mut child = running.child;
    let pgid = running.pgid;
    let output = watched.output.clone();
    tokio::spawn(async move {
        let code = match super::process::wait_capped(&mut child, pgid, &output, max_output_bytes)
            .await
            .status
        {
            Ok(status) => super::bash::exit_code(status),
            Err(_) => -1,
        };
        watched.set(Status::Exited(code), true);
    });
    task
}

// ---------------------------------------------------------------------------
// TaskStop
// ---------------------------------------------------------------------------

/// The `TaskStop` tool.
#[derive(Debug, Default)]
pub struct TaskStopTool;

impl TaskStopTool {
    /// A `TaskStop`.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        "TaskStop"
    }

    fn description(&self) -> &str {
        "Stops a background task started with Bash (run_in_background) or Monitor, and every \
         process it started. Stopping a task that is not running is not an error."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"task_id": {"type": "string"}},
            "required": ["task_id"]
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
        let Some(id) = arguments
            .get("task_id")
            .or_else(|| arguments.get("shell_id"))
            .and_then(Value::as_str)
        else {
            return ToolResult::error("`task_id` is required and must be a string");
        };
        let state = shell_state(&context.state);
        let Some(task) = state.get(id) else {
            return ToolResult::error(format!("No task found with ID: {id}"));
        };
        if task.status() != Status::Running {
            return ToolResult::ok(format!(
                "Task {id} is not running (status: {}). Nothing to stop.",
                task.status()
            ));
        }
        task.stop().await;
        ToolResult::ok(format!(
            "Successfully stopped task: {id} ({})",
            task.command
        ))
    }
}

// ---------------------------------------------------------------------------
// Monitor
// ---------------------------------------------------------------------------

/// The `Monitor` tool: runs a command in the background and sends each line of its output
/// to the client as a notification, until the command ends.
#[derive(Debug)]
pub struct MonitorTool {
    config: ShellConfig,
}

impl MonitorTool {
    /// A `Monitor` for the given shell configuration.
    pub fn new(config: ShellConfig) -> Self {
        Self { config }
    }
}

const POLL: Duration = Duration::from_millis(50);
const DEFAULT_MONITOR_MS: u64 = 300_000;
const MAX_MONITOR_MS: u64 = 3_600_000;

#[async_trait]
impl Tool for MonitorTool {
    fn name(&self) -> &str {
        "Monitor"
    }

    fn description(&self) -> &str {
        "Starts a command in the background and streams each line of its output (stdout and \
         stderr) to you as a notification, ending when the command exits or after `timeout_ms` \
         (default 300000, at most 3600000) unless `persistent`. Stop it with TaskStop."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "description": {"type": "string"},
                "timeout_ms": {"type": "integer", "minimum": 1},
                "persistent": {"type": "boolean"}
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
        if command.trim().is_empty() || command.len() > super::bash::MAX_COMMAND_BYTES {
            return ToolResult::error("`command` is empty or too long");
        }
        let persistent = arguments
            .get("persistent")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let timeout = Duration::from_millis(
            arguments
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_MONITOR_MS)
                .clamp(1, MAX_MONITOR_MS),
        );
        let state = shell_state(&context.state);
        let (id, output, running) = match super::bash::start(&self.config, &state, command, None) {
            Ok(started) => started,
            Err(message) => return ToolResult::error(message),
        };
        let task = adopt(
            &state,
            id.clone(),
            command,
            output.clone(),
            running,
            self.config.max_output_bytes,
        );
        let notifier = context.notifier.clone();
        tokio::spawn(stream(
            Arc::clone(&task),
            notifier.clone(),
            timeout,
            persistent,
        ));
        let how = if notifier.is_some() {
            "Each line of output arrives as a notification (`notifications/message`, logger \
             `nexus-tools/monitor`); a final one says when the command ends."
        } else {
            "This transport has no notification channel: read the output file."
        };
        ToolResult::ok(format!(
            "Monitor started with ID: {id}. {how} Output is also written to: {}. Stop it with TaskStop.",
            output.display()
        ))
    }
}

/// Follows the output file of `task`, one notification per complete line.
async fn stream(task: Arc<Task>, notifier: Option<Notifier>, timeout: Duration, persistent: bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut offset = 0u64;
    let mut pending = String::new();
    loop {
        let done = task.status() != Status::Running;
        // Read what was appended since the last pass (after the end is known, one last time).
        if let Ok(mut file) = std::fs::File::open(&task.output) {
            use std::io::Seek;
            if file.seek(std::io::SeekFrom::Start(offset)).is_ok() {
                let mut chunk = Vec::new();
                if file.read_to_end(&mut chunk).is_ok() {
                    offset += chunk.len() as u64;
                    pending.push_str(&String::from_utf8_lossy(&chunk));
                }
            }
        }
        while let Some(newline) = pending.find('\n') {
            let line: String = pending.drain(..=newline).collect();
            emit(
                &notifier,
                &task.id,
                json!({"task_id": task.id, "line": line.trim_end_matches(['\n', '\r'])}),
            );
        }
        if done {
            if !pending.is_empty() {
                emit(
                    &notifier,
                    &task.id,
                    json!({"task_id": task.id, "line": pending}),
                );
            }
            emit(
                &notifier,
                &task.id,
                json!({"task_id": task.id, "event": "ended", "status": task.status().to_string()}),
            );
            return;
        }
        if !persistent && tokio::time::Instant::now() >= deadline {
            task.stop().await;
            continue; // one more pass reads what the stop left, then reports the end
        }
        tokio::time::sleep(POLL).await;
    }
}

fn emit(notifier: &Option<Notifier>, _id: &str, data: Value) {
    if let Some(notifier) = notifier {
        notifier.notify(json!({
            "jsonrpc": "2.0",
            "method": "notifications/message",
            "params": {"level": "info", "logger": "nexus-tools/monitor", "data": data}
        }));
    }
}
