use anyhow::{Result, anyhow};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::core::claude_manager::ClaudeManager;
use crate::core::config::{FileAccessConfig, MCPConfig};
use crate::models::claude::ClaudeCodeOutput;

/// `describe_command_redacted` is shared with the SDK rather than copied.
///
/// `Debug for Command` prints every argument verbatim, so logging `{:?}` on the
/// command leaked the `--mcp-config` payload at `info` level on every session
/// creation — in practice the orchestrator's database password, search key and
/// session token, since `--mcp-config` carries each MCP server's `env` block and
/// HTTP headers.
///
/// This crate used to carry a byte-for-byte copy of that function and of the
/// `SECRET_BEARING_ARGS` list it reads, because the SDK's were `pub(crate)`.
/// They are `pub` and re-exported now, so the copy is gone: a list of
/// secret-bearing flags that exists twice is a list that gets extended once.
use nexus_claude::describe_command_redacted;

/// Interactive session manager — reuses one Claude CLI process per session.
///
/// ## Message queueing and concurrency
///
/// The Claude CLI in `--input-format stream-json` mode is **synchronous per turn**:
/// it processes one user message at a time and emits a `result` message when done.
/// There is no correlation_id in the protocol — a `result` message does not reference
/// which request it closes. Sending concurrent messages would cause responses to mix
/// in the broadcast channel with no way to demux them.
///
/// To handle this safely, each session has an `interaction_lock` that serializes
/// requests. The lock is held for the entire duration of send + response collection:
///
/// 1. Acquire `interaction_lock`
/// 2. Subscribe to broadcast
/// 3. Send message on stdin
/// 4. Collect responses until `type == "result"` (NOT a timeout heuristic)
/// 5. Forward responses to the caller's mpsc channel
/// 6. Drop the lock guard (implicit, end of scope)
///
/// Messages from subagent sidechains (`parent_tool_use_id != None`) are filtered out
/// during collection — they don't affect end-of-response detection.
///
/// The 30-second timeout in the collector is a **safety net only**, not the primary
/// end-of-response signal. Normal responses terminate via the `result` message.
///
/// ## Process death detection & recovery
///
/// When a CLI process dies unexpectedly (kill, crash, OOM):
///
/// 1. **Stdout reader** detects EOF and emits a synthetic `result/process_died` event,
///    unblocking any waiting response collectors and frontend subscribers.
/// 2. **Liveness check** (`try_wait`) runs before every message send. If the process
///    is dead, the session is removed and a new one is created with `--continue` to
///    resume the conversation context.
/// 3. **Cleanup task** also checks `try_wait()` every 5 minutes, proactively removing
///    dead sessions (in addition to expired ones).
#[derive(Clone)]
pub struct InteractiveSessionManager {
    sessions: Arc<RwLock<HashMap<String, InteractiveSession>>>,
    claude_command: String,
    file_access_config: FileAccessConfig,
    mcp_config: MCPConfig,
}

struct InteractiveSession {
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    conversation_id: String,
    child: Child,
    stdin_tx: mpsc::Sender<String>,
    output_tx: broadcast::Sender<ClaudeCodeOutput>,
    #[allow(dead_code)]
    model: String,
    #[allow(dead_code)]
    created_at: std::time::Instant,
    last_used: Arc<parking_lot::Mutex<std::time::Instant>>,
    /// Serializes requests: held from send through Result message reception.
    /// See struct-level docs for the full protocol explanation.
    interaction_lock: Arc<tokio::sync::Mutex<()>>,
}

/// Result of checking whether an existing session's process is still alive.
enum SessionStatus {
    /// Process is alive — reuse the session.
    Alive,
    /// Process is dead — session was removed; should recover with `--continue`.
    Dead,
    /// No session found for this conversation_id.
    NotFound,
}

/// Build a synthetic `result/process_died` [`ClaudeCodeOutput`].
///
/// This event mimics a real `result` message so that response collectors and
/// frontend subscribers treat it as end-of-response (the `type == "result"`
/// check triggers a break).
fn build_process_died_event(reason: &str) -> ClaudeCodeOutput {
    ClaudeCodeOutput {
        r#type: "result".to_string(),
        subtype: Some("process_died".to_string()),
        data: serde_json::json!({
            "type": "result",
            "subtype": "process_died",
            "is_error": true,
            "error": reason,
        }),
    }
}

/// How long the initial-response collector waits for *any* message from a freshly
/// spawned CLI before giving up on the turn.
///
/// A safety net, not the normal exit: a `result` message is. Thirty seconds of
/// total silence from the CLI means something is wrong, and the caller gets an
/// empty, closed channel rather than hanging.
const INITIAL_RESPONSE_SAFETY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The body of the initial-response collector that [`InteractiveSessionManager::create_session`]
/// spawns: forward everything the CLI says about its first turn to `caller_tx`,
/// stop on `result`/`error`, and give up after `safety_timeout` of silence.
///
/// Sidechain messages (from Task tool subagents) are filtered out; they do not
/// end a turn.
///
/// `safety_timeout` is a parameter rather than a constant read in place so that
/// the giving-up arm can be exercised in real time, with no child process and no
/// paused clock. The test that used to do it the other way round — `create_session`
/// under `#[tokio::test(start_paused = true)]` plus a 31-second jump — hung
/// forever on Windows; see
/// `test_initial_collector_gives_up_after_the_safety_timeout`.
async fn collect_initial_response(
    mut cli_rx: mpsc::Receiver<ClaudeCodeOutput>,
    caller_tx: mpsc::Sender<ClaudeCodeOutput>,
    safety_timeout: std::time::Duration,
) {
    let start_time = std::time::Instant::now();

    loop {
        match tokio::time::timeout(safety_timeout, cli_rx.recv()).await {
            Ok(Some(output)) => {
                // Skip sidechain messages (from Task tool subagents)
                if output.is_sidechain() {
                    debug!(
                        "Initial: skipping sidechain message (parent_tool_use_id: {:?})",
                        output.parent_tool_use_id()
                    );
                    continue;
                }

                // Detect end-of-response via Result message
                let is_result = output.r#type == "result";
                let is_error = output.r#type == "error";

                if caller_tx.send(output).await.is_err() {
                    break;
                }

                if is_result {
                    info!("Initial response complete (received result message)");
                    break;
                }
                if is_error {
                    break;
                }
            },
            Ok(None) => break, // Channel closed
            Err(_) => {
                // Safety timeout (30s with no messages at all)
                error!(
                    "Safety timeout waiting for initial response after {:?}",
                    start_time.elapsed()
                );
                break;
            },
        }
    }
}

/// Drains a CLI's stderr, logging every non-empty line at WARN, and returns on
/// EOF.
///
/// Extracted from [`InteractiveSessionManager::create_session`] so that a test can
/// `await` it to completion. That await is a **real** barrier: it returns only once
/// the child has closed its stderr, which means the child has run.
/// `tokio::task::yield_now()` cannot stand in for it — yielding re-polls Rust tasks
/// that are already runnable, it does not make a forked `sh` execute its next
/// command — which is how
/// `test_stderr_is_logged_and_never_mixed_into_the_response` came to depend on a
/// race. See that test for the measurements.
async fn log_stderr_lines(stderr: tokio::process::ChildStderr) {
    let reader = BufReader::new(stderr);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        warn!("Claude stderr: {}", line);
    }
}

impl InteractiveSessionManager {
    pub fn new(_claude_manager: Arc<ClaudeManager>, claude_command: String) -> Self {
        let manager = Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            claude_command,
            file_access_config: FileAccessConfig::default(),
            mcp_config: MCPConfig::default(),
        };

        // Start background cleanup task
        let sessions_clone = manager.sessions.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(300)).await; // every 5 min
                Self::cleanup_expired_sessions(sessions_clone.clone(), 30).await; // 30 min timeout
            }
        });

        manager
    }

    /// Get or create a session and send a message.
    ///
    /// If a session exists and its process is alive, reuse it. If the process
    /// has died, recover with `--continue` to preserve conversation context.
    /// Otherwise create a brand new session.
    pub async fn get_or_create_session_and_send(
        &self,
        conversation_id: Option<String>,
        model: String,
        message: String,
    ) -> Result<(String, mpsc::Receiver<ClaudeCodeOutput>)> {
        let conversation_id = conversation_id.unwrap_or_else(|| Uuid::new_v4().to_string());

        // Output channel for this request
        let (response_tx, response_rx) = mpsc::channel(100);

        // Check session status: alive, dead, or nonexistent
        let status = {
            let mut sessions = self.sessions.write();
            if let Some(session) = sessions.get_mut(&conversation_id) {
                match session.child.try_wait() {
                    Ok(Some(exit_status)) => {
                        warn!(
                            "Session {} process died (exit: {:?}), removing for recovery",
                            conversation_id, exit_status
                        );
                        sessions.remove(&conversation_id);
                        SessionStatus::Dead
                    },
                    Ok(None) => SessionStatus::Alive,
                    Err(e) => {
                        warn!(
                            "Failed to check process status for session {}: {}, removing",
                            conversation_id, e
                        );
                        sessions.remove(&conversation_id);
                        SessionStatus::Dead
                    },
                }
            } else {
                SessionStatus::NotFound
            }
        };

        match status {
            SessionStatus::Alive => {
                info!("Reusing existing session: {}", conversation_id);
                self.send_to_existing_session(conversation_id.clone(), message, response_tx)
                    .await;
            },
            SessionStatus::Dead => {
                info!(
                    "Recovering dead session with --continue: {}",
                    conversation_id
                );
                self.create_session(
                    conversation_id.clone(),
                    model,
                    message,
                    response_tx,
                    true, // continue_conversation
                )
                .await?;
            },
            SessionStatus::NotFound => {
                info!("Creating new interactive session: {}", conversation_id);
                self.create_session(conversation_id.clone(), model, message, response_tx, false)
                    .await?;
            },
        }

        Ok((conversation_id, response_rx))
    }

    /// Send a message to an existing (alive) session.
    ///
    /// Spawns a background task that acquires the interaction lock, subscribes
    /// to the broadcast channel, sends the message, and collects responses
    /// until a `result` or `error` message is received.
    async fn send_to_existing_session(
        &self,
        conversation_id: String,
        message: String,
        response_tx: mpsc::Sender<ClaudeCodeOutput>,
    ) {
        let sessions = self.sessions.clone();

        tokio::spawn(async move {
            let session_info = {
                let sessions_guard = sessions.read();
                sessions_guard.get(&conversation_id).map(|s| {
                    (
                        s.stdin_tx.clone(),
                        s.output_tx.clone(),
                        Arc::clone(&s.last_used),
                        Arc::clone(&s.interaction_lock),
                    )
                })
            };

            if let Some((stdin_tx, output_tx, last_used, interaction_lock)) = session_info {
                // Acquire interaction lock for serialized access
                let _lock = interaction_lock.lock().await;
                info!("Acquired interaction lock for session: {}", conversation_id);

                // Update last-used timestamp
                *last_used.lock() = std::time::Instant::now();

                // Subscribe to output broadcast
                let mut output_rx = output_tx.subscribe();

                // Spawn response collector
                // Uses Result message detection instead of timeout heuristic.
                // Sidechain messages (from Task tool subagents) are filtered out.
                let response_handle = tokio::spawn(async move {
                    let mut responses = Vec::new();
                    let start_time = std::time::Instant::now();

                    loop {
                        // Use a longer timeout (30s) as safety net only.
                        // Normal termination is via the "result" message type.
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(30),
                            output_rx.recv(),
                        )
                        .await
                        {
                            Ok(Ok(output)) => {
                                // Skip sidechain messages (from Task tool subagents)
                                if output.is_sidechain() {
                                    debug!(
                                        "Interactive: skipping sidechain message (parent_tool_use_id: {:?})",
                                        output.parent_tool_use_id()
                                    );
                                    continue;
                                }

                                responses.push(output.clone());

                                // Detect end-of-response via Result message
                                if output.r#type == "result" {
                                    info!("Response complete (received result message)");
                                    break;
                                }

                                // Also break on error
                                if output.r#type == "error" {
                                    break;
                                }
                            },
                            Ok(Err(_)) => {
                                // Broadcast channel closed
                                break;
                            },
                            Err(_) => {
                                // Safety timeout (30s with no messages at all)
                                error!(
                                    "Safety timeout waiting for response after {:?}",
                                    start_time.elapsed()
                                );
                                break;
                            },
                        }
                    }

                    responses
                });

                // Send message
                if let Err(e) = stdin_tx.send(message).await {
                    error!("Failed to send message to session: {}", e);
                    response_tx
                        .send(ClaudeCodeOutput {
                            r#type: "error".to_string(),
                            subtype: None,
                            data: serde_json::json!({
                                "error": format!("Failed to send message: {}", e)
                            }),
                        })
                        .await
                        .ok();
                    return;
                }

                // Wait for response collection to complete
                let responses = response_handle.await.unwrap_or_default();

                // Forward responses to caller
                for output in responses {
                    response_tx.send(output).await.ok();
                }

                // Close channel
                drop(response_tx);

                info!("Released interaction lock for session: {}", conversation_id);
            }
        });
    }

    /// Create a new interactive CLI session.
    ///
    /// When `continue_conversation` is true, passes `--continue` to the CLI
    /// to resume the most recent conversation (used for process death recovery).
    async fn create_session(
        &self,
        conversation_id: String,
        model: String,
        initial_message: String,
        initial_response_tx: mpsc::Sender<ClaudeCodeOutput>,
        continue_conversation: bool,
    ) -> Result<()> {
        let mut cmd = Command::new(&self.claude_command);

        cmd.arg("--model").arg(&model);

        // Resume conversation context after process death
        if continue_conversation {
            cmd.arg("--continue");
            info!("Session {} using --continue for recovery", conversation_id);
        }

        // File access permissions
        if self.file_access_config.skip_permissions {
            cmd.arg("--dangerously-skip-permissions");
        }

        // MCP configuration.
        // `config_file` wins over `config_json`, same precedence as
        // `ClaudeManager::create_session`. Before this, `config_json` was read
        // nowhere on the interactive path and the option was silently ignored.
        if self.mcp_config.enabled {
            if let Some(ref config_file) = self.mcp_config.config_file {
                cmd.arg("--mcp-config").arg(config_file);
            } else if let Some(ref config_json) = self.mcp_config.config_json {
                cmd.arg("--mcp-config").arg(config_json);
            }
        }

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Create a new process group so we can kill the entire tree
        // (CLI + its child processes like bash, find, sleep, etc.)
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }

        // Never `{:?}` the command: that prints the `--mcp-config` payload,
        // which carries the MCP servers' credentials.
        info!(
            "Starting interactive Claude session with command: {}",
            describe_command_redacted(cmd.as_std())
        );

        let mut child = cmd.spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to get stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to get stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Failed to get stderr"))?;

        // Create channels
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(100);
        let (output_tx, _) = broadcast::channel(100);

        // Dedicated channel for initial request
        let (initial_tx, initial_rx) = mpsc::channel::<ClaudeCodeOutput>(100);

        // Initial response collector task
        // Uses Result message detection instead of timeout heuristic.
        // Sidechain messages (from Task tool subagents) are filtered out.
        tokio::spawn(collect_initial_response(
            initial_rx,
            initial_response_tx,
            INITIAL_RESPONSE_SAFETY_TIMEOUT,
        ));

        // Handle stdin
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(msg) = stdin_rx.recv().await {
                if let Err(e) = stdin.write_all(msg.as_bytes()).await {
                    error!("Failed to write to stdin: {}", e);
                    break;
                }
                if let Err(e) = stdin.write_all(b"\n").await {
                    error!("Failed to write newline: {}", e);
                    break;
                }
                if let Err(e) = stdin.flush().await {
                    error!("Failed to flush stdin: {}", e);
                    break;
                }
                info!("Sent message to Claude process");
            }
        });

        // Handle stdout — parse JSON lines and broadcast
        let conversation_id_clone = conversation_id.clone();
        let output_tx_clone = output_tx.clone();
        let initial_tx_clone = initial_tx.clone();
        let is_first_response = Arc::new(parking_lot::Mutex::new(true));

        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }

                info!("Claude output: {}", line);

                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) {
                    let output = ClaudeCodeOutput {
                        r#type: json
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string(),
                        subtype: json
                            .get("subtype")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        data: json,
                    };

                    // Send to initial channel if still collecting first response
                    let should_send = {
                        let mut is_first = is_first_response.lock();
                        if *is_first {
                            if output.r#type == "error"
                                || (output.r#type == "text" && line.contains("Human:"))
                            {
                                *is_first = false;
                            }
                            true
                        } else {
                            false
                        }
                    };

                    if should_send {
                        let _ = initial_tx_clone.send(output.clone()).await;
                    }

                    // Broadcast to all subscribers
                    let _ = output_tx_clone.send(output);
                }
            }

            // ── Process died or stdout closed ──
            // Emit a synthetic result/process_died event so that:
            // 1. Response collectors break out of their recv loop (type == "result")
            // 2. Frontend subscribers learn that streaming is done
            let synthetic = build_process_died_event("CLI process terminated unexpectedly");

            // Notify initial-response collector (if still listening)
            let _ = initial_tx_clone.send(synthetic.clone()).await;
            // Notify all broadcast subscribers
            let _ = output_tx_clone.send(synthetic);

            info!(
                "Claude stdout stream ended for session: {} — emitted process_died event",
                conversation_id_clone
            );
        });

        // Handle stderr
        tokio::spawn(log_stderr_lines(stderr));

        // Send initial message (if not empty)
        if !initial_message.is_empty() {
            stdin_tx
                .send(initial_message)
                .await
                .map_err(|e| anyhow!("Failed to send initial message: {}", e))?;
        }

        // Store session
        let session = InteractiveSession {
            id: Uuid::new_v4().to_string(),
            conversation_id: conversation_id.clone(),
            child,
            stdin_tx,
            output_tx,
            model,
            created_at: std::time::Instant::now(),
            last_used: Arc::new(parking_lot::Mutex::new(std::time::Instant::now())),
            interaction_lock: Arc::new(tokio::sync::Mutex::new(())),
        };

        self.sessions.write().insert(conversation_id, session);

        Ok(())
    }

    /// Clean up expired and dead sessions.
    ///
    /// Runs every 5 minutes from the background task. Removes sessions that:
    /// - Have been idle longer than `timeout_minutes`
    /// - Have a dead process (detected via `try_wait()`)
    ///
    /// For dead sessions, a synthetic `result/process_died` event is emitted
    /// before removal to notify any remaining subscribers.
    async fn cleanup_expired_sessions(
        sessions: Arc<RwLock<HashMap<String, InteractiveSession>>>,
        timeout_minutes: u64,
    ) {
        let now = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(timeout_minutes * 60);

        // Collect sessions to remove while holding the lock
        let removed_sessions: Vec<(String, InteractiveSession, bool)> = {
            let mut sessions = sessions.write();

            // First pass: identify sessions to remove
            let mut to_remove: Vec<(String, bool)> = Vec::new();
            for (id, session) in sessions.iter_mut() {
                let last_used = *session.last_used.lock();
                let is_expired = now.duration_since(last_used) > timeout;
                let is_dead = matches!(session.child.try_wait(), Ok(Some(_)));

                if is_expired || is_dead {
                    to_remove.push((id.clone(), is_dead));
                }
            }

            // Second pass: remove them
            to_remove
                .into_iter()
                .filter_map(|(id, is_dead)| sessions.remove(&id).map(|s| (id, s, is_dead)))
                .collect()
        };
        // Lock is released here

        // Now kill/notify the removed sessions without holding the lock
        for (id, mut session, is_dead) in removed_sessions {
            if is_dead {
                info!("Cleaning up dead session: {} (process exited)", id);
                // Emit synthetic event for subscribers still listening
                let synthetic = build_process_died_event(
                    "CLI process terminated unexpectedly (detected during cleanup)",
                );
                let _ = session.output_tx.send(synthetic);
            } else {
                info!("Cleaning up expired session: {} (idle timeout)", id);
            }
            // Kill the entire process group to avoid orphan child processes
            #[cfg(unix)]
            if let Some(pid) = session.child.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            let _ = session.child.kill().await;
        }
    }

    /// Interrupt the active request in a session without closing it.
    ///
    /// Sends a `control_request` interrupt to the CLI via `stdin_tx` (lock-free,
    /// does not require the interaction lock). The CLI will abort the current
    /// tool execution (Bash, Read, etc.) and emit a `result` message.
    ///
    /// Returns `Ok(true)` if the session was found and the interrupt was sent,
    /// `Ok(false)` if no session exists for this conversation_id.
    pub fn interrupt_session(&self, conversation_id: &str) -> Result<bool> {
        let sessions = self.sessions.read();
        if let Some(session) = sessions.get(conversation_id) {
            // Build the interrupt control_request JSON
            let interrupt_json = serde_json::json!({
                "type": "control_request",
                "request": {
                    "type": "interrupt",
                    "request_id": Uuid::new_v4().to_string()
                }
            })
            .to_string();

            // Send via stdin_tx — lock-free, non-blocking
            match session.stdin_tx.try_send(interrupt_json) {
                Ok(()) => {
                    info!(
                        "Sent interrupt to session: {} (conversation_id={})",
                        session.id, conversation_id
                    );
                    Ok(true)
                },
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(
                        "Stdin channel full for session {}, interrupt may be delayed",
                        conversation_id
                    );
                    // Channel is full but message will be processed eventually
                    Ok(true)
                },
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    warn!(
                        "Stdin channel closed for session {}, process may have died",
                        conversation_id
                    );
                    Err(anyhow!(
                        "Session {} stdin channel is closed",
                        conversation_id
                    ))
                },
            }
        } else {
            Ok(false)
        }
    }

    /// Close a specific session.
    #[allow(dead_code)]
    pub async fn close_session(&self, conversation_id: &str) -> Result<()> {
        let session_opt = {
            let mut sessions = self.sessions.write();
            sessions.remove(conversation_id)
        };
        if let Some(mut session) = session_opt {
            info!("Closing session: {}", conversation_id);
            // Kill the entire process group to avoid orphan child processes
            #[cfg(unix)]
            if let Some(pid) = session.child.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            session.child.kill().await?;
            Ok(())
        } else {
            Err(anyhow!("Session not found: {}", conversation_id))
        }
    }

    /// Does nothing, successfully.
    ///
    /// **Not implemented.** Pre-warming an interactive session would mean
    /// spawning a CLI process with no conversation to attach it to: the process
    /// is keyed by `conversation_id` in `create_session`, and the first
    /// request brings its own id, so a pre-warmed process could never be
    /// claimed by it. Nothing is spawned, and `Ok(())` is returned
    /// unconditionally — no caller can observe a failure here.
    ///
    /// `create_app` logs "Failed to pre-warm Claude process" if this returns
    /// `Err`; that branch is therefore dead. Kept as a no-op (rather than
    /// removed) because it is part of the startup sequence in `lib.rs`, which
    /// belongs to another owner.
    pub async fn prewarm_default_session(&self) -> Result<()> {
        debug!("Pre-warming requested — interactive sessions have no pre-warm path, skipping");
        Ok(())
    }

    /// Get the number of active sessions.
    #[allow(dead_code)]
    pub fn active_sessions(&self) -> usize {
        self.sessions.read().len()
    }
}

impl Drop for InteractiveSessionManager {
    fn drop(&mut self) {
        let mut sessions = self.sessions.write();
        for (id, mut session) in sessions.drain() {
            info!("Cleaning up session on shutdown: {}", id);
            // Kill the entire process group to avoid orphan child processes
            #[cfg(unix)]
            if let Some(pid) = session.child.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            // Fallback: kill the child directly.
            // Note: In Drop we can't await, so we start the kill and let it complete.
            // The process will be cleaned up by the OS regardless.
            #[allow(clippy::let_underscore_future)]
            let _ = session.child.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::TempDir;

    // ───────────────────────────── test harness ─────────────────────────────

    const STDERR_LINE: &str = "fake CLI diagnostic on stderr";

    /// A stand-in for the `claude` CLI, portable between Unix and Windows.
    ///
    /// This crate has no transport abstraction: [`InteractiveSessionManager::create_session`]
    /// calls `Command::new(&self.claude_command)` directly, so the command string
    /// is the only seam. [`FakeCli`] writes a tiny script into a `TempDir` which
    ///
    /// 1. records its own argv in a file — that is how the tests below assert on
    ///    the command line the production code actually built;
    /// 2. prints a canned `stream-json` transcript on stdout;
    /// 3. optionally writes one line on stderr;
    /// 4. then either exits, or blocks until its stdin is closed (which models a
    ///    live CLI waiting for the next turn).
    ///
    /// A blocking variant needs no explicit kill: its stdin is closed when the
    /// session (or the `Child`) is dropped, the reader sees EOF and the process
    /// exits on its own. Nothing here spawns a real `claude`, touches the network
    /// or outlives the test.
    struct FakeCli {
        _dir: TempDir,
        script: PathBuf,
        argv: PathBuf,
    }

    impl FakeCli {
        fn build(stdout: &str, with_stderr: bool, keep_alive: bool) -> Self {
            let dir = tempfile::tempdir().expect("tempdir for the fake CLI");
            let payload = dir.path().join("payload.ndjson");
            std::fs::write(&payload, stdout).expect("write the fake CLI payload");
            let argv = dir.path().join("argv.txt");

            let body = if cfg!(windows) {
                let mut body = String::from("@echo off\r\n");
                // Redirection first: an argument that ends in a digit would
                // otherwise be read by cmd.exe as a stream number.
                body.push_str(&format!("> \"{}\" echo %*\r\n", argv.display()));
                body.push_str(&format!("type \"{}\"\r\n", payload.display()));
                if with_stderr {
                    body.push_str(&format!("echo {STDERR_LINE} 1>&2\r\n"));
                }
                if keep_alive {
                    // `sort` reads stdin until EOF; while the session holds stdin
                    // open it never returns, so the process stays alive.
                    body.push_str("sort >nul 2>nul\r\n");
                }
                body.push_str("exit /b 0\r\n");
                body
            } else {
                let mut body = String::from("#!/bin/sh\n");
                body.push_str(&format!("echo \"$@\" > '{}'\n", argv.display()));
                body.push_str(&format!("cat '{}'\n", payload.display()));
                if with_stderr {
                    body.push_str(&format!("echo '{STDERR_LINE}' 1>&2\n"));
                }
                if keep_alive {
                    body.push_str("cat > /dev/null\n");
                }
                body.push_str("exit 0\n");
                body
            };

            let script = crate::fake_exec::plant_fake_cli(dir.path(), &body);

            Self {
                _dir: dir,
                script,
                argv,
            }
        }

        /// Prints `transcript`, then exits 0.
        fn emitting(transcript: &str) -> Self {
            Self::build(transcript, false, false)
        }

        /// Prints nothing and exits 0 — the process is dead almost at once.
        fn exiting() -> Self {
            Self::build("", false, false)
        }

        /// Prints nothing and stays alive until its stdin is closed.
        fn blocking() -> Self {
            Self::build("", false, true)
        }

        /// Prints `transcript` and then stays alive until its stdin is closed.
        fn emitting_then_blocking(transcript: &str) -> Self {
            Self::build(transcript, false, true)
        }

        /// Writes one line on stderr, prints `transcript`, then exits.
        fn noisy_on_stderr(transcript: &str) -> Self {
            Self::build(transcript, true, false)
        }

        fn command(&self) -> String {
            self.script.to_string_lossy().into_owned()
        }

        fn recorded_argv(&self) -> String {
            std::fs::read_to_string(&self.argv).unwrap_or_default()
        }

        /// Forgets the argv of a previous spawn, so [`Self::argv`] can wait for
        /// the next one instead of reading a stale line.
        fn forget_argv(&self) {
            let _ = std::fs::remove_file(&self.argv);
        }

        /// The argv of the last spawn, waiting for the script to record it.
        ///
        /// `create_session` returns as soon as the process is spawned, so the
        /// script may not have run yet. Never use this in a `start_paused` test:
        /// the sleep below would not advance real time.
        async fn argv(&self) -> String {
            for _ in 0..400 {
                let argv = self.recorded_argv();
                if !argv.trim().is_empty() {
                    return argv;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("the fake CLI never recorded its argv");
        }
    }

    // ── transcript builders ──

    fn line(value: serde_json::Value) -> String {
        format!("{value}\n")
    }

    fn assistant_line(text: &str) -> String {
        line(json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
        }))
    }

    fn sidechain_line(text: &str) -> String {
        line(json!({
            "type": "assistant",
            "parent_tool_use_id": "toolu_sidechain",
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
        }))
    }

    fn result_line() -> String {
        line(json!({"type": "result", "subtype": "success", "is_error": false}))
    }

    // ── manager & session builders ──

    fn claude_manager(command: &str) -> Arc<ClaudeManager> {
        Arc::new(ClaudeManager::new(
            command.to_string(),
            FileAccessConfig::default(),
            MCPConfig::default(),
        ))
    }

    /// A manager built field by field, i.e. **without** the background cleanup
    /// task `new()` spawns. Tests that need the task use `new()` explicitly.
    fn manager_with(
        command: String,
        file_access_config: FileAccessConfig,
        mcp_config: MCPConfig,
    ) -> InteractiveSessionManager {
        InteractiveSessionManager {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            claude_command: command,
            file_access_config,
            mcp_config,
        }
    }

    fn manager(cli: &FakeCli) -> InteractiveSessionManager {
        manager_with(
            cli.command(),
            FileAccessConfig::default(),
            MCPConfig::default(),
        )
    }

    fn spawn_fake(cli: &FakeCli) -> Child {
        Command::new(cli.command())
            .arg("--model")
            .arg("manual-test")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the fake CLI")
    }

    /// A child process that has already exited **and been reaped**, so
    /// `try_wait()` is guaranteed to report it dead without any sleeping.
    async fn dead_child(cli: &FakeCli) -> Child {
        let mut child = spawn_fake(cli);
        let status = child.wait().await.expect("wait on the fake CLI");
        assert!(status.success(), "the fake CLI should exit 0");
        child
    }

    /// A session wired by hand, so a test owns both ends of its channels.
    fn manual_session(
        conversation_id: &str,
        child: Child,
        stdin_capacity: usize,
        broadcast_capacity: usize,
    ) -> (
        InteractiveSession,
        mpsc::Receiver<String>,
        broadcast::Sender<ClaudeCodeOutput>,
    ) {
        let (stdin_tx, stdin_rx) = mpsc::channel::<String>(stdin_capacity);
        let (output_tx, _) = broadcast::channel(broadcast_capacity);
        let session = InteractiveSession {
            id: format!("session-of-{conversation_id}"),
            conversation_id: conversation_id.to_string(),
            child,
            stdin_tx,
            output_tx: output_tx.clone(),
            model: "manual-model".to_string(),
            created_at: std::time::Instant::now(),
            last_used: Arc::new(parking_lot::Mutex::new(std::time::Instant::now())),
            interaction_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        (session, stdin_rx, output_tx)
    }

    /// Drains a response channel to its close, failing rather than hanging.
    async fn collect_all(mut rx: mpsc::Receiver<ClaudeCodeOutput>) -> Vec<ClaudeCodeOutput> {
        let mut collected = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
                Ok(Some(output)) => collected.push(output),
                Ok(None) => return collected,
                Err(_) => panic!(
                    "the response channel never closed ({} message(s) collected)",
                    collected.len()
                ),
            }
        }
    }

    fn types_of(outputs: &[ClaudeCodeOutput]) -> Vec<&str> {
        outputs.iter().map(|o| o.r#type.as_str()).collect()
    }

    fn texts_of(outputs: &[ClaudeCodeOutput]) -> Vec<String> {
        outputs
            .iter()
            .filter_map(|o| {
                o.data
                    .pointer("/message/content/0/text")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect()
    }

    /// Waits until the response collector of `send_to_existing_session` has
    /// subscribed to the broadcast channel, so a test can inject messages
    /// without racing it.
    ///
    /// Yielding is a sound barrier *here*, and the reason is worth keeping: the
    /// event waited for is a Rust task being polled, and that is exactly what a
    /// yield hands it. It is **not** a sound barrier for anything that needs an
    /// OS process to run — the whole budget below is microseconds of wall clock,
    /// and no number of yields will make a child write to a pipe. Wait for the
    /// side effect itself in that case; see [`log_stderr_lines`].
    async fn await_subscriber(output_tx: &broadcast::Sender<ClaudeCodeOutput>) {
        for _ in 0..1_000 {
            if output_tx.receiver_count() > 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("no collector ever subscribed to the broadcast channel");
    }

    /// An in-memory `tracing` sink, to assert on what actually reaches the log.
    #[derive(Clone, Default)]
    struct LogSink(Arc<parking_lot::Mutex<Vec<u8>>>);

    impl LogSink {
        fn contents(&self) -> String {
            String::from_utf8_lossy(&self.0.lock()).into_owned()
        }
    }

    impl std::io::Write for LogSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
        type Writer = LogSink;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Installs a thread-local `tracing` subscriber capturing up to `level`.
    ///
    /// The `rebuild_interest_cache` call is not optional: a callsite first hit by
    /// another test while no subscriber was installed is cached as
    /// `Interest::never`, and a never-interested callsite does not even evaluate
    /// its arguments — so the assertions below would otherwise depend on the
    /// order the tests happen to run in.
    fn capture_logs(level: tracing::Level) -> (LogSink, tracing::subscriber::DefaultGuard) {
        let sink = LogSink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_max_level(level)
            .with_ansi(false)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        (sink, guard)
    }

    // ───────────────────── build_process_died_event ─────────────────────

    #[test]
    fn test_process_died_event_has_result_type() {
        let event = build_process_died_event("test reason");
        assert_eq!(event.r#type, "result");
        assert_eq!(event.subtype.as_deref(), Some("process_died"));
    }

    #[test]
    fn test_process_died_event_carries_error_info() {
        let event = build_process_died_event("CLI killed by OOM");
        assert_eq!(event.data["is_error"], json!(true));
        assert_eq!(event.data["error"], json!("CLI killed by OOM"));
    }

    #[test]
    fn test_process_died_event_is_not_sidechain() {
        let event = build_process_died_event("crash");
        assert!(!event.is_sidechain());
        assert!(event.parent_tool_use_id().is_none());
    }

    #[test]
    fn test_process_died_event_detected_as_result_by_collector() {
        // Response collectors break on `type == "result"` — verify the synthetic
        // event would trigger that break condition.
        let event = build_process_died_event("dead");
        assert_eq!(event.r#type, "result");
        // The subtype distinguishes it from a normal result
        assert_eq!(event.subtype, Some("process_died".to_string()));
    }

    // ───────────────────────── SessionStatus enum ─────────────────────────

    #[test]
    fn test_session_status_variants_exist() {
        // Compile-time test: all variants are constructible
        let _alive = SessionStatus::Alive;
        let _dead = SessionStatus::Dead;
        let _not_found = SessionStatus::NotFound;
    }

    // ──────────────────── describe_command_redacted (S2) ────────────────────

    /// A value that must never reach the log. Not a real credential: the point
    /// is that the redaction is keyed on the *argument*, not on the shape of the
    /// value.
    const MCP_SENTINEL: &str =
        r#"{"mcpServers":{"orc":{"env":{"TOKEN":"sentinel-must-not-be-logged"}}}}"#;

    #[test]
    fn test_redaction_hides_the_value_after_mcp_config() {
        let mut cmd = std::process::Command::new("claude");
        cmd.arg("--model")
            .arg("opus")
            .arg("--mcp-config")
            .arg(MCP_SENTINEL);

        let described = describe_command_redacted(&cmd);

        assert!(
            !described.contains("sentinel-must-not-be-logged"),
            "the MCP payload must not appear in the log line: {described}"
        );
        assert!(
            described.contains(&format!("<redacted {} bytes>", MCP_SENTINEL.len())),
            "the redaction should state the size it replaced: {described}"
        );
        // What a human debugging a spawn still needs is kept.
        assert!(described.contains("program=claude"), "{described}");
        assert!(described.contains("--mcp-config"), "{described}");
        assert!(described.contains("--model"), "{described}");
        assert!(described.contains("opus"), "{described}");
    }

    #[test]
    fn test_redaction_only_swallows_one_argument() {
        let mut cmd = std::process::Command::new("claude");
        cmd.arg("--mcp-config")
            .arg("secret-payload")
            .arg("--continue")
            .arg("--dangerously-skip-permissions");

        let described = describe_command_redacted(&cmd);

        assert!(!described.contains("secret-payload"), "{described}");
        assert!(
            described.contains("--continue"),
            "the argument after the secret must stay readable: {described}"
        );
        assert!(
            described.contains("--dangerously-skip-permissions"),
            "{described}"
        );
    }

    #[test]
    fn test_redaction_keeps_env_names_but_no_env_values() {
        let mut cmd = std::process::Command::new("claude");
        cmd.env("ANTHROPIC_AUTH_TOKEN", "sentinel-env-value");

        let described = describe_command_redacted(&cmd);

        assert!(
            described.contains("ANTHROPIC_AUTH_TOKEN"),
            "the variable name is useful: {described}"
        );
        assert!(
            !described.contains("sentinel-env-value"),
            "its value is not: {described}"
        );
    }

    #[test]
    fn test_redaction_reports_the_working_directory() {
        let mut cmd = std::process::Command::new("claude");
        assert!(
            describe_command_redacted(&cmd).contains("cwd=<inherited>"),
            "an unset cwd must be shown as inherited, not omitted"
        );

        cmd.current_dir("/workspace/nexus");
        assert!(describe_command_redacted(&cmd).contains("/workspace/nexus"));
    }

    #[test]
    fn test_redaction_tolerates_a_trailing_secret_flag() {
        // `--mcp-config` with no value: the loop must not look past the end.
        let mut cmd = std::process::Command::new("claude");
        cmd.arg("--mcp-config");

        let described = describe_command_redacted(&cmd);
        assert!(described.contains("--mcp-config"), "{described}");
        assert!(!described.contains("<redacted"), "{described}");
    }

    #[tokio::test]
    async fn test_create_session_never_logs_the_inline_mcp_payload() {
        // End-to-end proof for S2: the `info!` of `create_session` is fed by
        // `describe_command_redacted`. Replayed against the previous code
        // (`info!("… {:?}", cmd)`) this fails, because `Debug for Command` prints
        // every argument verbatim.
        let (logs, argv) = create_session_capturing_logs(MCPConfig {
            enabled: true,
            config_file: None,
            config_json: Some(MCP_SENTINEL.to_string()),
            strict: false,
            debug: false,
        })
        .await;

        assert!(
            logs.contains("Starting interactive Claude session with command:"),
            "the spawn must still be logged: {logs}"
        );
        assert!(
            !logs.contains("sentinel-must-not-be-logged"),
            "the MCP payload reached the log: {logs}"
        );
        assert!(
            logs.contains(&format!("<redacted {} bytes>", MCP_SENTINEL.len())),
            "the payload should be replaced by a redaction marker: {logs}"
        );
        // And the argument really was on the command line.
        assert!(argv.contains("--mcp-config"), "argv: {argv}");
    }

    /// Spawns one session with `mcp` and returns everything that reached the
    /// log, plus the argv the CLI actually received.
    async fn create_session_capturing_logs(mcp: MCPConfig) -> (String, String) {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager_with(cli.command(), FileAccessConfig::default(), mcp);

        let (tx, rx) = mpsc::channel(16);
        let sink = {
            let (sink, _guard) = capture_logs(tracing::Level::INFO);
            manager
                .create_session(
                    "log-check".to_string(),
                    "opus".to_string(),
                    String::new(),
                    tx,
                    false,
                )
                .await
                .expect("the fake CLI spawns");
            let _ = collect_all(rx).await;
            sink
        };

        (sink.contents(), cli.argv().await)
    }

    #[tokio::test]
    async fn test_create_session_never_logs_the_mcp_config_path() {
        // The pre-existing leak: `config_file` was the one MCP form this file did
        // pass, and `info!("… {:?}", cmd)` printed it verbatim. Replayed without
        // the correction this assertion fails on the value itself.
        const PATH_SENTINEL: &str = "sentinel-must-not-be-logged.json";
        let (logs, argv) = create_session_capturing_logs(MCPConfig {
            enabled: true,
            config_file: Some(PATH_SENTINEL.to_string()),
            config_json: None,
            strict: false,
            debug: false,
        })
        .await;

        assert!(argv.contains("--mcp-config"), "argv: {argv}");
        assert!(
            !logs.contains("sentinel-must-not-be-logged"),
            "whatever follows --mcp-config must be redacted, path or document: {logs}"
        );
        assert!(
            logs.contains(&format!("<redacted {} bytes>", PATH_SENTINEL.len())),
            "{logs}"
        );
    }

    // ───────────────────────────── new() ─────────────────────────────

    #[tokio::test]
    async fn test_new_starts_empty_and_ignores_its_claude_manager() {
        // `new` takes an `Arc<ClaudeManager>` it binds to `_claude_manager` and
        // never uses; only the command string matters.
        let manager =
            InteractiveSessionManager::new(claude_manager("claude"), "claude".to_string());
        assert_eq!(manager.active_sessions(), 0);
        assert_eq!(manager.claude_command, "claude");
    }

    #[tokio::test]
    async fn test_new_spawns_a_cleanup_task_that_reaps_dead_sessions() {
        // The background loop sleeps 300 s then calls `cleanup_expired_sessions`.
        // Real-process setup happens first, on the real clock; the clock is only
        // paused to jump over the sleep.
        let cli = FakeCli::exiting();
        let manager = InteractiveSessionManager::new(claude_manager("claude"), cli.command());
        let sessions = manager.sessions.clone();

        let (session, _stdin_rx, _output_tx) =
            manual_session("reaped", dead_child(&cli).await, 4, 4);
        sessions.write().insert("reaped".to_string(), session);
        assert_eq!(manager.active_sessions(), 1);

        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(301)).await;
        for _ in 0..1_000 {
            if sessions.read().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert_eq!(
            manager.active_sessions(),
            0,
            "the 5-minute cleanup task should have removed the dead session"
        );
    }

    // ───────────────── get_or_create_session_and_send ─────────────────

    #[tokio::test]
    async fn test_unknown_conversation_creates_a_session_and_streams_to_the_result() {
        let transcript = format!("{}{}", assistant_line("Bonjour Nexus"), result_line());
        let cli = FakeCli::emitting(&transcript);
        let manager = manager(&cli);

        let (id, rx) = manager
            .get_or_create_session_and_send(
                Some("conv-new".to_string()),
                "claude-opus-4-5".to_string(),
                "hello".to_string(),
            )
            .await
            .expect("session creation");

        assert_eq!(id, "conv-new");
        let outputs = collect_all(rx).await;
        assert_eq!(types_of(&outputs), vec!["assistant", "result"]);
        assert_eq!(texts_of(&outputs), vec!["Bonjour Nexus".to_string()]);
        assert_eq!(outputs[1].subtype.as_deref(), Some("success"));
        assert_eq!(manager.active_sessions(), 1);
        // The requested model is what the CLI was started with.
        let argv = cli.argv().await;
        assert!(argv.contains("--model"), "argv: {argv}");
        assert!(argv.contains("claude-opus-4-5"), "argv: {argv}");
        assert!(
            !argv.contains("--continue"),
            "a brand new session must not resume another one: {argv}"
        );
    }

    #[tokio::test]
    async fn test_absent_conversation_id_is_replaced_by_a_fresh_uuid() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager(&cli);

        let (id, rx) = manager
            .get_or_create_session_and_send(None, "opus".to_string(), "hi".to_string())
            .await
            .expect("session creation");

        assert!(
            Uuid::parse_str(&id).is_ok(),
            "a generated conversation_id should be a UUID, got {id}"
        );
        assert!(manager.sessions.read().contains_key(&id));
        let _ = collect_all(rx).await;
    }

    #[tokio::test]
    async fn test_dead_session_is_removed_and_recovered_with_continue() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager(&cli);
        let (session, _stdin_rx, _output_tx) =
            manual_session("conv-dead", dead_child(&cli).await, 4, 4);
        manager
            .sessions
            .write()
            .insert("conv-dead".to_string(), session);
        cli.forget_argv(); // only the recovery spawn should be inspected

        let (id, rx) = manager
            .get_or_create_session_and_send(
                Some("conv-dead".to_string()),
                "opus".to_string(),
                "still there?".to_string(),
            )
            .await
            .expect("recovery");

        assert_eq!(id, "conv-dead");
        let outputs = collect_all(rx).await;
        assert_eq!(types_of(&outputs), vec!["result"]);
        assert_eq!(
            manager.active_sessions(),
            1,
            "the dead session is replaced, not duplicated"
        );
        let argv = cli.argv().await;
        assert!(
            argv.contains("--continue"),
            "recovery must resume the conversation: {argv}"
        );
    }

    #[tokio::test]
    async fn test_alive_session_is_reused_without_spawning_a_process() {
        let cli = FakeCli::blocking();
        let manager = manager(&cli);
        let (session, mut stdin_rx, output_tx) =
            manual_session("conv-alive", spawn_fake(&cli), 4, 16);
        manager
            .sessions
            .write()
            .insert("conv-alive".to_string(), session);

        let (id, rx) = manager
            .get_or_create_session_and_send(
                Some("conv-alive".to_string()),
                "a-model-never-spawned".to_string(),
                "second turn".to_string(),
            )
            .await
            .expect("reuse");
        assert_eq!(id, "conv-alive");

        // The message goes to the existing process, and the collector filters
        // sidechain traffic out of the caller's stream.
        await_subscriber(&output_tx).await;
        output_tx
            .send(
                serde_json::from_str::<ClaudeCodeOutput>(sidechain_line("subagent").trim())
                    .unwrap(),
            )
            .expect("broadcast the sidechain message");
        output_tx
            .send(
                serde_json::from_str::<ClaudeCodeOutput>(assistant_line("deuxième tour").trim())
                    .unwrap(),
            )
            .expect("broadcast the answer");
        output_tx
            .send(serde_json::from_str::<ClaudeCodeOutput>(result_line().trim()).unwrap())
            .expect("broadcast the result");

        let outputs = collect_all(rx).await;
        assert_eq!(
            types_of(&outputs),
            vec!["assistant", "result"],
            "the sidechain message must not reach the caller"
        );
        assert_eq!(texts_of(&outputs), vec!["deuxième tour".to_string()]);
        assert_eq!(
            stdin_rx.recv().await.as_deref(),
            Some("second turn"),
            "the message should be written to the existing process stdin"
        );
        let argv = cli.argv().await;
        assert!(
            argv.contains("manual-test") && !argv.contains("a-model-never-spawned"),
            "reusing a session must not start a second CLI: {argv}"
        );
    }

    // ──────────────────── send_to_existing_session ────────────────────

    #[tokio::test]
    async fn test_send_to_a_missing_session_just_closes_the_channel() {
        let cli = FakeCli::exiting();
        let manager = manager(&cli);
        let (tx, rx) = mpsc::channel(4);

        manager
            .send_to_existing_session("ghost".to_string(), "hello".to_string(), tx)
            .await;

        assert!(
            collect_all(rx).await.is_empty(),
            "no session means no output at all — not an error message"
        );
    }

    #[tokio::test]
    async fn test_closed_stdin_channel_is_reported_as_an_error_output() {
        let cli = FakeCli::blocking();
        let manager = manager(&cli);
        let (session, stdin_rx, _output_tx) = manual_session("conv-x", spawn_fake(&cli), 4, 16);
        drop(stdin_rx); // the stdin writer task is gone
        manager
            .sessions
            .write()
            .insert("conv-x".to_string(), session);

        let (tx, rx) = mpsc::channel(4);
        manager
            .send_to_existing_session("conv-x".to_string(), "hello".to_string(), tx)
            .await;

        let outputs = collect_all(rx).await;
        assert_eq!(types_of(&outputs), vec!["error"]);
        assert!(
            outputs[0].data["error"]
                .as_str()
                .unwrap_or_default()
                .starts_with("Failed to send message:"),
            "unexpected payload: {:?}",
            outputs[0].data
        );
    }

    #[tokio::test]
    async fn test_error_message_also_ends_the_collection() {
        let cli = FakeCli::blocking();
        let manager = manager(&cli);
        let (session, mut stdin_rx, output_tx) = manual_session("conv-e", spawn_fake(&cli), 4, 16);
        manager
            .sessions
            .write()
            .insert("conv-e".to_string(), session);

        let (tx, rx) = mpsc::channel(4);
        manager
            .send_to_existing_session("conv-e".to_string(), "boom".to_string(), tx)
            .await;
        await_subscriber(&output_tx).await;
        output_tx
            .send(ClaudeCodeOutput {
                r#type: "error".to_string(),
                subtype: None,
                data: json!({"error": "CLI refused"}),
            })
            .expect("broadcast the error");

        let outputs = collect_all(rx).await;
        assert_eq!(
            types_of(&outputs),
            vec!["error"],
            "an error message terminates the response like a result does"
        );
        assert_eq!(stdin_rx.recv().await.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn test_a_lagging_collector_drops_the_whole_response() {
        // Broadcast capacity 1 and three messages published before the collector
        // is ever polled: `recv()` returns `RecvError::Lagged`, which the code
        // treats like a closed channel — the caller gets nothing at all, not a
        // partial answer. Worth knowing: it is silent.
        let cli = FakeCli::blocking();
        let manager = manager(&cli);
        let (session, _stdin_rx, output_tx) = manual_session("conv-lag", spawn_fake(&cli), 4, 1);
        manager
            .sessions
            .write()
            .insert("conv-lag".to_string(), session);

        let (tx, rx) = mpsc::channel(8);
        manager
            .send_to_existing_session("conv-lag".to_string(), "hello".to_string(), tx)
            .await;
        await_subscriber(&output_tx).await;

        // `broadcast::Sender::send` is synchronous, so nothing can poll the
        // collector between these three lines.
        for i in 0..3 {
            output_tx
                .send(ClaudeCodeOutput {
                    r#type: "assistant".to_string(),
                    subtype: None,
                    data: json!({"n": i}),
                })
                .expect("broadcast");
        }

        assert!(
            collect_all(rx).await.is_empty(),
            "a lagged collector forwards nothing"
        );
    }

    /// DO NOT add a read of this child's stdout or stderr. It would hang on
    /// Windows, and the hang would be silent.
    ///
    /// This test combines a **paused clock** with a **real child process**, and
    /// it only survives that combination because nothing ever reads the child's
    /// pipes. On Windows a child's stdio is `Blocking<ArcFile>`, so a pending
    /// read is handed to `spawn_blocking`, whose `BlockingSchedule` calls
    /// `clock.inhibit_auto_advance()`. The virtual clock then stops advancing
    /// while a real `park_timeout` waits for a timer that can never fire:
    /// a genuine deadlock, not a slow test. Unix takes a different path
    /// (`PollEvented` on a pipe) and never shows it.
    ///
    /// Two tests in this crate already cost a 49-minute Windows job that wrote
    /// no `test result:` line at all — a hang is an *absence of verdict*, so it
    /// also hid two further Windows defects behind it. They were fixed by
    /// extracting the timing logic so it is tested without a live child
    /// (`collect_initial_response`, `sweep_expired_idle`). This test is the last
    /// one still holding the dangerous pair, and it is only safe by omission.
    ///
    /// If you need the child's output here, drop `start_paused` and use a real
    /// short timeout instead, or extract the logic under test the way the other
    /// two were.
    #[tokio::test(start_paused = true)]
    async fn test_safety_timeout_ends_an_unanswered_turn() {
        // No `result` ever arrives: the 30-second net is the only way out.
        let cli = FakeCli::blocking();
        let manager = manager(&cli);
        let (session, mut stdin_rx, output_tx) = manual_session("conv-t", spawn_fake(&cli), 4, 16);
        manager
            .sessions
            .write()
            .insert("conv-t".to_string(), session);

        let (tx, mut rx) = mpsc::channel(4);
        manager
            .send_to_existing_session("conv-t".to_string(), "no answer".to_string(), tx)
            .await;
        await_subscriber(&output_tx).await;
        assert_eq!(stdin_rx.recv().await.as_deref(), Some("no answer"));

        let (sink, _guard) = capture_logs(tracing::Level::ERROR);
        tokio::time::advance(Duration::from_secs(31)).await;

        assert!(
            tokio::time::timeout(Duration::from_secs(600), rx.recv())
                .await
                .expect("the collector must give up")
                .is_none(),
            "after the safety timeout the caller gets an empty, closed channel"
        );
        assert!(
            sink.contents()
                .contains("Safety timeout waiting for response after"),
            "giving up must be audible in the log: {}",
            sink.contents()
        );
    }

    // ───────────────────────── create_session ─────────────────────────

    #[tokio::test]
    async fn test_spawn_failure_is_surfaced_as_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("no-such-claude");
        let manager = manager_with(
            missing.to_string_lossy().into_owned(),
            FileAccessConfig::default(),
            MCPConfig::default(),
        );

        let (tx, _rx) = mpsc::channel(4);
        let err = manager
            .create_session(
                "conv".to_string(),
                "opus".to_string(),
                "hi".to_string(),
                tx,
                false,
            )
            .await
            .expect_err("spawning a missing binary must fail");

        assert!(
            err.downcast_ref::<std::io::Error>().is_some(),
            "the spawn error should be propagated as-is, got: {err}"
        );
        assert_eq!(
            manager.active_sessions(),
            0,
            "a failed spawn must not register a session"
        );
    }

    #[tokio::test]
    async fn test_skip_permissions_is_forwarded_only_when_configured() {
        let cli = FakeCli::emitting(&result_line());
        let permissive = manager_with(
            cli.command(),
            FileAccessConfig {
                skip_permissions: true,
                additional_dirs: vec![],
            },
            MCPConfig::default(),
        );
        let (tx, rx) = mpsc::channel(4);
        permissive
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();
        let _ = collect_all(rx).await;
        assert!(cli.argv().await.contains("--dangerously-skip-permissions"));

        let strict_cli = FakeCli::emitting(&result_line());
        let strict = manager(&strict_cli);
        let (tx, rx) = mpsc::channel(4);
        strict
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();
        let _ = collect_all(rx).await;
        assert!(
            !strict_cli
                .argv()
                .await
                .contains("--dangerously-skip-permissions"),
            "the default config must keep the CLI permission prompts"
        );
    }

    #[tokio::test]
    async fn test_mcp_config_file_is_passed_to_the_cli() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager_with(
            cli.command(),
            FileAccessConfig::default(),
            MCPConfig {
                enabled: true,
                config_file: Some("mcp-servers.json".to_string()),
                config_json: Some(MCP_SENTINEL.to_string()),
                strict: true,
                debug: true,
            },
        );

        let (tx, rx) = mpsc::channel(4);
        manager
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();
        let _ = collect_all(rx).await;

        let argv = cli.argv().await;
        assert!(argv.contains("--mcp-config"), "argv: {argv}");
        assert!(argv.contains("mcp-servers.json"), "argv: {argv}");
        assert!(
            !argv.contains("sentinel-must-not-be-logged"),
            "the file wins over the inline JSON: {argv}"
        );
        // Known divergence with `ClaudeManager::create_session`: `strict` and
        // `debug` are read nowhere on the interactive path.
        assert!(
            !argv.contains("--strict-mcp-config") && !argv.contains("--debug"),
            "argv: {argv}"
        );
    }

    #[tokio::test]
    async fn test_inline_mcp_json_is_passed_when_no_file_is_configured() {
        // Before the fix, `config_json` was read nowhere in this file: an
        // interactive session silently ran without any MCP server while
        // `ClaudeManager` honoured the same setting. This test fails on the old
        // code (no `--mcp-config` at all in the argv).
        let cli = FakeCli::emitting(&result_line());
        let manager = manager_with(
            cli.command(),
            FileAccessConfig::default(),
            MCPConfig {
                enabled: true,
                config_file: None,
                config_json: Some(MCP_SENTINEL.to_string()),
                strict: false,
                debug: false,
            },
        );

        let (tx, rx) = mpsc::channel(4);
        manager
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();
        let _ = collect_all(rx).await;

        let argv = cli.argv().await;
        assert!(argv.contains("--mcp-config"), "argv: {argv}");
        assert!(argv.contains("mcpServers"), "argv: {argv}");
    }

    #[tokio::test]
    async fn test_disabled_mcp_config_passes_nothing() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager_with(
            cli.command(),
            FileAccessConfig::default(),
            MCPConfig {
                enabled: false,
                config_file: Some("mcp-servers.json".to_string()),
                config_json: Some(MCP_SENTINEL.to_string()),
                strict: false,
                debug: false,
            },
        );

        let (tx, rx) = mpsc::channel(4);
        manager
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();
        let _ = collect_all(rx).await;

        assert!(
            !cli.argv().await.contains("--mcp-config"),
            "`enabled: false` must disable the flag even when a config is present"
        );
    }

    #[tokio::test]
    async fn test_unparseable_and_blank_lines_are_skipped() {
        let transcript = format!("\n   \nnot json at all\n{}", result_line());
        let cli = FakeCli::emitting(&transcript);
        let manager = manager(&cli);

        let (tx, rx) = mpsc::channel(8);
        manager
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();

        let outputs = collect_all(rx).await;
        assert_eq!(
            types_of(&outputs),
            vec!["result"],
            "garbage is dropped, the stream keeps going"
        );
    }

    #[tokio::test]
    async fn test_a_line_without_a_type_becomes_unknown() {
        let transcript = format!("{}{}", line(json!({"hello": "world"})), result_line());
        let cli = FakeCli::emitting(&transcript);
        let manager = manager(&cli);

        let (tx, rx) = mpsc::channel(8);
        manager
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();

        let outputs = collect_all(rx).await;
        assert_eq!(types_of(&outputs), vec!["unknown", "result"]);
        assert!(
            outputs[0].subtype.is_none(),
            "a missing subtype stays None rather than becoming a placeholder"
        );
        assert_eq!(outputs[0].data["hello"], json!("world"));
    }

    #[tokio::test]
    async fn test_initial_collector_filters_sidechain_output() {
        let transcript = format!(
            "{}{}{}",
            sidechain_line("subagent chatter"),
            assistant_line("real answer"),
            result_line()
        );
        let cli = FakeCli::emitting(&transcript);
        let manager = manager(&cli);

        let (tx, rx) = mpsc::channel(8);
        manager
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();

        let outputs = collect_all(rx).await;
        assert_eq!(texts_of(&outputs), vec!["real answer".to_string()]);
        assert_eq!(types_of(&outputs), vec!["assistant", "result"]);
    }

    #[tokio::test]
    async fn test_initial_collector_stops_on_an_error_line() {
        let transcript = format!(
            "{}{}",
            line(json!({"type": "error", "error": "quota exhausted"})),
            assistant_line("never read")
        );
        let cli = FakeCli::emitting(&transcript);
        let manager = manager(&cli);

        let (tx, rx) = mpsc::channel(8);
        manager
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();

        let outputs = collect_all(rx).await;
        assert_eq!(types_of(&outputs), vec!["error"]);
        assert_eq!(outputs[0].data["error"], json!("quota exhausted"));
    }

    #[tokio::test]
    async fn test_only_the_first_response_is_forwarded_and_eof_closes_it() {
        // A `text` line containing "Human:" flips `is_first_response` off: the
        // next lines are broadcast but no longer forwarded to the caller. The
        // caller only learns the turn is over through the synthetic
        // `result/process_died` emitted at stdout EOF.
        let transcript = format!(
            "{}{}",
            line(json!({"type": "text", "text": "Human: next question?"})),
            assistant_line("answered after the handover")
        );
        let cli = FakeCli::emitting(&transcript);
        let manager = manager(&cli);

        let (tx, rx) = mpsc::channel(8);
        manager
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();

        let outputs = collect_all(rx).await;
        assert_eq!(types_of(&outputs), vec!["text", "result"]);
        assert_eq!(
            outputs[1].subtype.as_deref(),
            Some("process_died"),
            "stdout EOF must be reported as an end-of-response"
        );
        assert_eq!(outputs[1].data["is_error"], json!(true));
        assert!(
            texts_of(&outputs).is_empty(),
            "the assistant line after the handover is not forwarded"
        );
    }

    #[tokio::test]
    async fn test_silent_cli_only_yields_the_process_died_event() {
        let cli = FakeCli::exiting();
        let manager = manager(&cli);

        let (tx, rx) = mpsc::channel(8);
        manager
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();

        let outputs = collect_all(rx).await;
        assert_eq!(types_of(&outputs), vec!["result"]);
        assert_eq!(outputs[0].subtype.as_deref(), Some("process_died"));
        assert_eq!(
            outputs[0].data["error"],
            json!("CLI process terminated unexpectedly")
        );
        assert_eq!(
            manager.active_sessions(),
            1,
            "a session whose process already died stays in the map until a send or a cleanup"
        );
    }

    /// Two facts about stderr, each behind its own barrier.
    ///
    /// The logging half used to go through `create_session` and then spin
    /// `tokio::task::yield_now()` up to 200 times waiting for the warning to show
    /// up in the capture. That was never a barrier. `collect_all` returns as soon
    /// as the *stdout* `result` line has been read, and the fake CLI writes its
    /// stderr line only afterwards — with a `cat` process exiting in between — so
    /// what the loop had to wait for was **a separate OS process running its next
    /// command**. Yielding cannot make that happen: it re-polls Rust tasks that are
    /// already runnable, and the whole 200-yield budget is a few tens of
    /// microseconds of wall clock. Instrumented over 200 samples the line was
    /// already there after 3 yields at the median (max 13) on an idle machine, and
    /// after 5 (max 19) at load average 180 — a margin that looks wide in yields
    /// and is paper-thin in time. On `ubuntu-latest / beta` it ran out and the
    /// capture came back empty.
    ///
    /// Awaiting [`log_stderr_lines`] to EOF replaces it: the reader returns only
    /// once the child has closed its stderr, so the write has provably happened,
    /// in real time, with no budget to exhaust. It also runs first, while no other
    /// stderr reader exists, so the capture can only hold what this reader wrote.
    #[tokio::test]
    async fn test_stderr_is_logged_and_never_mixed_into_the_response() {
        let cli = FakeCli::noisy_on_stderr(&result_line());

        // Logged verbatim — the production reader, driven to EOF.
        let mut child = spawn_fake(&cli);
        let stderr = child.stderr.take().expect("stderr is piped");
        let logs = {
            let (sink, _guard) = capture_logs(tracing::Level::WARN);
            tokio::time::timeout(Duration::from_secs(30), log_stderr_lines(stderr))
                .await
                .expect("the reader must reach EOF once the CLI exits");
            sink.contents()
        };
        child.wait().await.expect("reap the fake CLI");

        assert!(
            logs.contains(&format!("Claude stderr: {STDERR_LINE}")),
            "the stderr line must reach the log verbatim: {logs}"
        );

        // Never mixed in — through a whole session, the caller sees `result` only.
        let manager = manager(&cli);
        let (tx, rx) = mpsc::channel(8);
        manager
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            types_of(&collect_all(rx).await),
            vec!["result"],
            "stderr is logged, never mixed into the output stream"
        );
    }

    #[tokio::test]
    async fn test_a_dropped_caller_stops_the_initial_collector() {
        let cli = FakeCli::emitting_then_blocking(&result_line());
        let manager = manager(&cli);

        let (tx, rx) = mpsc::channel(8);
        drop(rx); // the HTTP client went away before the first line arrived

        manager
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .expect("the session is created anyway");

        // Subscribing before the first await guarantees we see the CLI output
        // the initial collector is about to fail to forward.
        let mut broadcast = {
            let sessions = manager.sessions.read();
            sessions
                .get("c")
                .expect("session stored")
                .output_tx
                .subscribe()
        };
        let event = tokio::time::timeout(Duration::from_secs(10), broadcast.recv())
            .await
            .expect("the CLI output must still be broadcast")
            .expect("the broadcast is open");
        assert_eq!(
            event.r#type, "result",
            "the output still reaches the subscribers, only the caller is gone"
        );
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        assert_eq!(
            manager.active_sessions(),
            1,
            "losing the caller must not tear the session down"
        );
        let mut sessions = manager.sessions.write();
        let session = sessions.get_mut("c").expect("session stored");
        assert!(
            matches!(session.child.try_wait(), Ok(None)),
            "the CLI process must survive the loss of its caller"
        );
    }

    /// A CLI that says nothing at all: the collector gives up, closes the
    /// caller's channel empty, and says so in the log.
    ///
    /// Deliberately **not** `start_paused`, and deliberately without a child
    /// process. This test used to run `create_session` against
    /// `FakeCli::blocking()` under a paused clock and jump the clock by 31
    /// seconds. It hung forever on Windows, for two compounding reasons:
    ///
    /// * `create_session` spawns the collector and returns without ever
    ///   yielding (an empty initial message writes nothing), so the 31-second
    ///   jump landed *before* the collector had been polled even once. The
    ///   collector then registered its 30-second timer against the clock as
    ///   already advanced, and the jump was wasted — only auto-advance could
    ///   still reach the deadline.
    /// * Auto-advance was unavailable. On Windows a child's stdout/stderr are
    ///   `tokio::io::blocking::Blocking` handles, so each pending read is a task
    ///   on the blocking pool, and `BlockingSchedule::new` calls
    ///   `Clock::inhibit_auto_advance()` for the lifetime of that task. The fake
    ///   CLI never writes and never exits, so the read never completes, the
    ///   inhibition never lifts, and `park_thread_timeout` parks for the *real*
    ///   duration left on a clock that will never move. On Unix those pipes are
    ///   `PollEvented`, no blocking task exists, and auto-advance works — which
    ///   is why the same test passed on macOS and Linux.
    ///
    /// So the collector is called directly, in real time, with a short timeout.
    /// The production value is asserted on its own below, and the collector
    /// being wired into `create_session` is covered by
    /// `test_initial_collector_filters_sidechain_output`,
    /// `test_initial_collector_stops_on_an_error_line`,
    /// `test_only_the_first_response_is_forwarded_and_eof_closes_it` and
    /// `test_a_dropped_caller_stops_the_initial_collector`.
    #[tokio::test]
    async fn test_initial_collector_gives_up_after_the_safety_timeout() {
        assert_eq!(
            INITIAL_RESPONSE_SAFETY_TIMEOUT,
            Duration::from_secs(30),
            "production still gives a fresh CLI 30 s of silence before giving up"
        );

        // `_cli_tx` is the CLI side of the collector's input. A named binding, so
        // that it stays alive and silent for the whole call: dropping it early
        // would close the channel and send the collector down its `Ok(None)` arm
        // instead of the timeout arm. The log assertion below is what tells the
        // two arms apart — only the timeout arm writes anything.
        let (_cli_tx, cli_rx) = mpsc::channel::<ClaudeCodeOutput>(8);
        let (tx, mut rx) = mpsc::channel(8);

        let (sink, _guard) = capture_logs(tracing::Level::ERROR);
        // A real-time bound two orders of magnitude over the timeout under test,
        // so that a regression fails the job instead of holding a runner for
        // GitHub's six-hour limit.
        tokio::time::timeout(
            Duration::from_secs(30),
            collect_initial_response(cli_rx, tx, Duration::from_millis(100)),
        )
        .await
        .expect("the collector must give up on its own");

        assert!(
            rx.recv().await.is_none(),
            "a CLI that says nothing for the safety timeout closes the caller's \
             channel empty"
        );
        assert!(
            sink.contents()
                .contains("Safety timeout waiting for initial response after"),
            "giving up must be audible in the log: {}",
            sink.contents()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_empty_message_writes_nothing_and_a_real_one_makes_the_round_trip() {
        // `cat` with no redirection echoes stdin to stdout, so the round trip
        // proves the stdin writer, the stdout reader and the broadcast are wired
        // together — and that an empty initial message is written nowhere.
        // Unix only: cmd.exe has no portable `cat`.
        let dir = tempfile::tempdir().expect("tempdir");
        let script = crate::fake_exec::plant_fake_cli(dir.path(), "#!/bin/sh\nexec cat\n");

        let manager = manager_with(
            script.to_string_lossy().into_owned(),
            FileAccessConfig::default(),
            MCPConfig::default(),
        );

        let (tx, mut rx) = mpsc::channel(8);
        manager
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(300), rx.recv())
                .await
                .is_err(),
            "an empty initial message must not be written to the CLI stdin"
        );

        let stdin_tx = {
            let sessions = manager.sessions.read();
            sessions.get("c").expect("session stored").stdin_tx.clone()
        };
        stdin_tx
            .send(json!({"type": "result", "subtype": "echo"}).to_string())
            .await
            .expect("the stdin writer is still draining the channel");

        let echoed = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the echo must come back")
            .expect("the channel is still open");
        assert_eq!(echoed.r#type, "result");
        assert_eq!(echoed.subtype.as_deref(), Some("echo"));
    }

    #[tokio::test]
    async fn test_stdin_writer_stops_after_the_pipe_breaks() {
        // The CLI exits at once; the first write to its stdin fails, the writer
        // task breaks out of its loop and the channel it was draining closes —
        // which is the only way a caller can observe the failure.
        let cli = FakeCli::exiting();
        let manager = manager(&cli);

        let (tx, rx) = mpsc::channel(8);
        manager
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();
        let _ = collect_all(rx).await;

        let stdin_tx = {
            let sessions = manager.sessions.read();
            sessions.get("c").expect("session stored").stdin_tx.clone()
        };

        let mut closed = false;
        for _ in 0..200 {
            if stdin_tx.send("knock".to_string()).await.is_err() {
                closed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            closed,
            "writing to a dead process should kill the stdin writer task"
        );
    }

    #[tokio::test]
    async fn test_session_is_stored_under_its_conversation_id() {
        let cli = FakeCli::emitting_then_blocking(&result_line());
        let manager = manager(&cli);

        let (tx, rx) = mpsc::channel(8);
        manager
            .create_session(
                "conv-key".to_string(),
                "opus-for-the-record".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();
        let _ = collect_all(rx).await;

        let sessions = manager.sessions.read();
        let session = sessions.get("conv-key").expect("stored under its id");
        assert_eq!(session.conversation_id, "conv-key");
        assert_eq!(session.model, "opus-for-the-record");
        assert!(
            Uuid::parse_str(&session.id).is_ok(),
            "each session also gets its own UUID: {}",
            session.id
        );
    }

    #[tokio::test]
    async fn test_both_collectors_name_the_sidechain_they_skip() {
        // The `debug!` that explains the filtering is only useful if it carries
        // the parent_tool_use_id; this is the only place that is checked.
        let transcript = format!("{}{}", sidechain_line("subagent"), result_line());
        let cli = FakeCli::emitting(&transcript);
        let manager = manager(&cli);

        let (tx, rx) = mpsc::channel(8);
        let logs = {
            let (sink, _guard) = capture_logs(tracing::Level::DEBUG);
            manager
                .create_session(
                    "conv-side".to_string(),
                    "opus".to_string(),
                    String::new(),
                    tx,
                    false,
                )
                .await
                .unwrap();
            let outputs = collect_all(rx).await;
            assert_eq!(types_of(&outputs), vec!["result"]);

            // Second turn on the same session, through the other collector.
            let (tx, rx) = mpsc::channel(8);
            manager
                .send_to_existing_session("conv-side".to_string(), "again".to_string(), tx)
                .await;
            let output_tx = {
                let sessions = manager.sessions.read();
                sessions
                    .get("conv-side")
                    .expect("session stored")
                    .output_tx
                    .clone()
            };
            await_subscriber(&output_tx).await;
            output_tx
                .send(
                    serde_json::from_str::<ClaudeCodeOutput>(sidechain_line("subagent").trim())
                        .unwrap(),
                )
                .expect("broadcast the sidechain message");
            output_tx
                .send(serde_json::from_str::<ClaudeCodeOutput>(result_line().trim()).unwrap())
                .expect("broadcast the result");
            assert_eq!(types_of(&collect_all(rx).await), vec!["result"]);
            sink.contents()
        };

        assert!(
            logs.contains("Initial: skipping sidechain message")
                && logs.contains("Interactive: skipping sidechain message"),
            "both collectors must say what they dropped: {logs}"
        );
        assert_eq!(
            logs.lines()
                .filter(
                    |l| l.contains("skipping sidechain message") && l.contains("toolu_sidechain")
                )
                .count(),
            2,
            "each skip must name the sidechain it belongs to: {logs}"
        );
    }

    #[tokio::test]
    async fn test_mcp_enabled_without_any_configuration_passes_nothing() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager_with(
            cli.command(),
            FileAccessConfig::default(),
            MCPConfig {
                enabled: true,
                config_file: None,
                config_json: None,
                strict: false,
                debug: false,
            },
        );

        let (tx, rx) = mpsc::channel(4);
        manager
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();
        let _ = collect_all(rx).await;

        assert!(
            !cli.argv().await.contains("--mcp-config"),
            "`enabled: true` with nothing to point at must not pass a bare flag"
        );
    }

    #[tokio::test]
    async fn test_a_failed_spawn_is_propagated_to_the_caller_on_both_paths() {
        // The two `self.create_session(...).await?` of
        // `get_or_create_session_and_send`: a new conversation, and the recovery
        // of a dead one. Neither may swallow the spawn error.
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("no-such-claude");
        let manager = manager_with(
            missing.to_string_lossy().into_owned(),
            FileAccessConfig::default(),
            MCPConfig::default(),
        );

        let err = manager
            .get_or_create_session_and_send(
                Some("fresh".to_string()),
                "opus".to_string(),
                "hi".to_string(),
            )
            .await
            .expect_err("a new session that cannot spawn must fail");
        assert!(err.downcast_ref::<std::io::Error>().is_some(), "{err}");

        let cli = FakeCli::exiting();
        let (session, _stdin_rx, _output_tx) = manual_session("dead", dead_child(&cli).await, 4, 4);
        manager.sessions.write().insert("dead".to_string(), session);

        let err = manager
            .get_or_create_session_and_send(
                Some("dead".to_string()),
                "opus".to_string(),
                "hi".to_string(),
            )
            .await
            .expect_err("a recovery that cannot spawn must fail too");
        assert!(err.downcast_ref::<std::io::Error>().is_some(), "{err}");
        assert_eq!(
            manager.active_sessions(),
            0,
            "the dead session is gone even though the replacement never started"
        );
    }

    #[tokio::test]
    async fn test_an_empty_message_fails_on_the_newline_alone() {
        // `write_all(b"")` is a no-op that cannot fail, so an empty message sent
        // to a dead process fails on the newline that follows it — never on the
        // body. Nothing prevents an empty message here: only `create_session`
        // filters those. Which *operation* first notices the dead pipe is
        // platform-dependent; see the assertions at the bottom.
        let cli = FakeCli::exiting();
        let manager = manager(&cli);

        let (tx, rx) = mpsc::channel(8);
        manager
            .create_session(
                "c".to_string(),
                "opus".to_string(),
                String::new(),
                tx,
                false,
            )
            .await
            .unwrap();
        let _ = collect_all(rx).await;

        let stdin_tx = {
            let sessions = manager.sessions.read();
            sessions.get("c").expect("session stored").stdin_tx.clone()
        };

        let logs = {
            let (sink, _guard) = capture_logs(tracing::Level::ERROR);
            let mut closed = false;
            for _ in 0..200 {
                if stdin_tx.send(String::new()).await.is_err() {
                    closed = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(closed, "the stdin writer must give up on a dead process");
            sink.contents()
        };

        // The dead pipe is reported one step apart on the two platforms, so both
        // shapes are named here rather than loosening the pattern:
        //
        // * Unix — `ChildStdin` is a `PollEvented` pipe, so the one-byte write
        //   gets `EPIPE` on the spot: "Failed to write newline".
        // * Windows — `ChildStdin` is a `tokio::io::blocking::Blocking` handle.
        //   The byte is copied into its buffer and handed to the blocking pool,
        //   so `write_all` reports success and the closed pipe is only observed
        //   by the next operation: "Failed to flush stdin: The pipe is being
        //   closed. (os error 232)".
        //
        // Either way the proven fact is the same one: the newline that follows an
        // empty message could not be delivered, and the writer said so before
        // giving up. The negative assertion is what keeps this from degenerating
        // into "any error will do" — the empty body itself must never be the
        // failure, because `write_all(b"")` does not call `poll_write` at all.
        let failed_at_the_newline = logs.contains("Failed to write newline:");
        let failed_at_the_flush = logs.contains("Failed to flush stdin:");
        assert!(
            failed_at_the_newline || failed_at_the_flush,
            "the newline after an empty message must fail and be logged — at the \
             write on Unix, at the flush on Windows: {logs}"
        );
        assert!(
            !logs.contains("Failed to write to stdin:"),
            "an empty payload is a no-op write and must never be the failure \
             itself: {logs}"
        );
    }

    // ───────────────────────── interrupt_session ─────────────────────────

    #[tokio::test]
    async fn test_interrupt_sends_a_control_request_on_stdin() {
        let cli = FakeCli::blocking();
        let manager = manager(&cli);
        let (session, mut stdin_rx, _output_tx) = manual_session("conv-i", spawn_fake(&cli), 4, 4);
        manager
            .sessions
            .write()
            .insert("conv-i".to_string(), session);

        assert!(manager.interrupt_session("conv-i").expect("interrupt sent"));

        let sent = stdin_rx.recv().await.expect("something on stdin");
        let parsed: serde_json::Value = serde_json::from_str(&sent).expect("valid JSON");
        assert_eq!(parsed["type"], json!("control_request"));
        assert_eq!(parsed["request"]["type"], json!("interrupt"));
        assert!(
            Uuid::parse_str(parsed["request"]["request_id"].as_str().unwrap()).is_ok(),
            "the interrupt must carry a fresh request_id: {parsed}"
        );
    }

    #[tokio::test]
    async fn test_interrupt_on_an_unknown_session_is_not_an_error() {
        let cli = FakeCli::exiting();
        let manager = manager(&cli);
        assert!(
            !manager.interrupt_session("ghost").expect("no error"),
            "an unknown conversation yields Ok(false), not Err"
        );
    }

    #[tokio::test]
    async fn test_interrupt_tolerates_a_full_stdin_channel() {
        let cli = FakeCli::blocking();
        let manager = manager(&cli);
        let (session, _stdin_rx, _output_tx) = manual_session("conv-f", spawn_fake(&cli), 1, 4);
        // Fill the single slot so `try_send` reports Full.
        session
            .stdin_tx
            .try_send("occupied".to_string())
            .expect("first slot");
        manager
            .sessions
            .write()
            .insert("conv-f".to_string(), session);

        assert!(
            manager.interrupt_session("conv-f").expect("no error"),
            "a full channel still reports success — the interrupt is simply delayed"
        );
    }

    #[tokio::test]
    async fn test_interrupt_fails_when_the_stdin_channel_is_closed() {
        let cli = FakeCli::blocking();
        let manager = manager(&cli);
        let (session, stdin_rx, _output_tx) = manual_session("conv-c", spawn_fake(&cli), 1, 4);
        drop(stdin_rx);
        manager
            .sessions
            .write()
            .insert("conv-c".to_string(), session);

        let err = manager
            .interrupt_session("conv-c")
            .expect_err("a closed channel must be an error");
        assert_eq!(err.to_string(), "Session conv-c stdin channel is closed");
    }

    // ────────────────────────── close_session ──────────────────────────

    #[tokio::test]
    async fn test_close_session_removes_it_and_kills_the_process() {
        let cli = FakeCli::blocking();
        let manager = manager(&cli);
        let (session, _stdin_rx, _output_tx) = manual_session("conv-k", spawn_fake(&cli), 4, 4);
        manager
            .sessions
            .write()
            .insert("conv-k".to_string(), session);

        manager.close_session("conv-k").await.expect("close");

        assert_eq!(manager.active_sessions(), 0);
        assert!(
            manager.close_session("conv-k").await.is_err(),
            "closing twice must not silently succeed"
        );
    }

    #[tokio::test]
    async fn test_close_unknown_session_names_it_in_the_error() {
        let cli = FakeCli::exiting();
        let manager = manager(&cli);
        let err = manager
            .close_session("ghost")
            .await
            .expect_err("unknown session");
        assert_eq!(err.to_string(), "Session not found: ghost");
    }

    #[tokio::test]
    async fn test_close_session_on_an_already_reaped_process_still_succeeds() {
        // `child.id()` is None once the process has been reaped, so the process
        // group kill is skipped; `child.kill()` is a no-op on a child whose exit
        // status tokio already holds, and the close reports success.
        let cli = FakeCli::exiting();
        let manager = manager(&cli);
        let (session, _stdin_rx, _output_tx) =
            manual_session("conv-reaped", dead_child(&cli).await, 4, 4);
        manager
            .sessions
            .write()
            .insert("conv-reaped".to_string(), session);

        manager
            .close_session("conv-reaped")
            .await
            .expect("closing a session whose process already exited is not an error");
        assert_eq!(manager.active_sessions(), 0);
        assert_eq!(
            manager
                .close_session("conv-reaped")
                .await
                .expect_err("it is gone now")
                .to_string(),
            "Session not found: conv-reaped"
        );
    }

    #[tokio::test]
    async fn test_drop_tolerates_a_session_whose_process_is_already_reaped() {
        let cli = FakeCli::exiting();
        let manager = manager(&cli);
        let (session, _stdin_rx, _output_tx) =
            manual_session("conv-reaped", dead_child(&cli).await, 4, 4);
        manager
            .sessions
            .write()
            .insert("conv-reaped".to_string(), session);

        let sessions = manager.sessions.clone();
        drop(manager);

        assert!(
            sessions.read().is_empty(),
            "Drop must drain the map even when there is no pid left to signal"
        );
    }

    // ─────────────────────── prewarm / active_sessions ───────────────────────

    #[tokio::test]
    async fn test_prewarm_does_nothing_and_cannot_fail() {
        // Pins the documented behaviour of a function whose name promises work:
        // no process is spawned, no session is registered, and the `Err` branch
        // `create_app` logs about is unreachable.
        let cli = FakeCli::blocking();
        let manager = manager(&cli);

        manager
            .prewarm_default_session()
            .await
            .expect("the no-op cannot fail");

        assert_eq!(
            manager.active_sessions(),
            0,
            "prewarm_default_session registers nothing"
        );
        assert!(
            cli.recorded_argv().is_empty(),
            "prewarm_default_session spawns no CLI"
        );
    }

    #[tokio::test]
    async fn test_active_sessions_counts_the_map() {
        let cli = FakeCli::blocking();
        let manager = manager(&cli);
        assert_eq!(manager.active_sessions(), 0);
        for id in ["a", "b"] {
            let (session, _stdin_rx, _output_tx) = manual_session(id, spawn_fake(&cli), 4, 4);
            manager.sessions.write().insert(id.to_string(), session);
        }
        assert_eq!(manager.active_sessions(), 2);
    }

    // ───────────────────────────── Drop ─────────────────────────────

    #[tokio::test]
    async fn test_dropping_the_manager_drains_every_session() {
        let cli = FakeCli::blocking();
        let manager = manager(&cli);
        let (session, _stdin_rx, _output_tx) = manual_session("conv-d", spawn_fake(&cli), 4, 4);
        manager
            .sessions
            .write()
            .insert("conv-d".to_string(), session);

        // The map is shared, so it can still be observed after the drop.
        let sessions = manager.sessions.clone();
        let clone = manager.clone();
        drop(manager);

        assert!(
            sessions.read().is_empty(),
            "Drop must drain the session map and kill the processes"
        );
        assert_eq!(
            clone.active_sessions(),
            0,
            "InteractiveSessionManager is Clone over a shared map: dropping ONE clone \
             kills the sessions of all the others"
        );
    }

    // ─────────────────────── cleanup_expired_sessions ───────────────────────

    #[tokio::test]
    async fn test_cleanup_removes_dead_sessions() {
        let cli = FakeCli::exiting();
        let sessions: Arc<RwLock<HashMap<String, InteractiveSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let (session, _stdin_rx, _output_tx) =
            manual_session("conv-dead", dead_child(&cli).await, 1, 1);
        sessions.write().insert("conv-dead".to_string(), session);

        // A very long timeout, so only the liveness check can trigger.
        InteractiveSessionManager::cleanup_expired_sessions(sessions.clone(), 9999).await;

        assert!(
            sessions.read().is_empty(),
            "a dead process must be reaped even when the session is fresh"
        );
    }

    #[tokio::test]
    async fn test_cleanup_keeps_alive_sessions() {
        let cli = FakeCli::blocking();
        let sessions: Arc<RwLock<HashMap<String, InteractiveSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let (session, _stdin_rx, _output_tx) = manual_session("conv-alive", spawn_fake(&cli), 1, 1);
        sessions.write().insert("conv-alive".to_string(), session);

        InteractiveSessionManager::cleanup_expired_sessions(sessions.clone(), 9999).await;

        assert_eq!(
            sessions.read().len(),
            1,
            "a live process within its idle window must be kept"
        );

        let removed = sessions.write().remove("conv-alive");
        if let Some(mut session) = removed {
            let _ = session.child.kill().await;
        }
    }

    #[tokio::test]
    async fn test_cleanup_removes_expired_sessions() {
        let cli = FakeCli::blocking();
        let sessions: Arc<RwLock<HashMap<String, InteractiveSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let (session, _stdin_rx, _output_tx) =
            manual_session("conv-expired", spawn_fake(&cli), 1, 1);
        sessions.write().insert("conv-expired".to_string(), session);

        // `timeout_minutes = 0` makes every session immediately expired, which
        // also avoids an `Instant` subtraction overflow on Windows.
        InteractiveSessionManager::cleanup_expired_sessions(sessions.clone(), 0).await;

        assert!(
            sessions.read().is_empty(),
            "an idle session past its timeout must be removed even though it is alive"
        );
    }

    #[tokio::test]
    async fn test_cleanup_emits_process_died_for_dead_sessions() {
        let cli = FakeCli::exiting();
        let sessions: Arc<RwLock<HashMap<String, InteractiveSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let (session, _stdin_rx, output_tx) =
            manual_session("conv-dead-notify", dead_child(&cli).await, 1, 16);
        let mut subscriber = output_tx.subscribe();
        sessions
            .write()
            .insert("conv-dead-notify".to_string(), session);

        InteractiveSessionManager::cleanup_expired_sessions(sessions.clone(), 9999).await;

        let event = tokio::time::timeout(Duration::from_secs(5), subscriber.recv())
            .await
            .expect("the synthetic event must be emitted")
            .expect("the broadcast is still open");
        assert_eq!(event.r#type, "result");
        assert_eq!(event.subtype.as_deref(), Some("process_died"));
        assert_eq!(
            event.data["error"],
            json!("CLI process terminated unexpectedly (detected during cleanup)")
        );
    }

    #[tokio::test]
    async fn test_cleanup_notifies_nothing_for_a_merely_expired_session() {
        let cli = FakeCli::blocking();
        let sessions: Arc<RwLock<HashMap<String, InteractiveSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let (session, _stdin_rx, output_tx) = manual_session("conv-idle", spawn_fake(&cli), 1, 16);
        let mut subscriber = output_tx.subscribe();
        sessions.write().insert("conv-idle".to_string(), session);

        InteractiveSessionManager::cleanup_expired_sessions(sessions.clone(), 0).await;

        assert!(
            subscriber.try_recv().is_err(),
            "an idle session is killed without a process_died event, so a subscriber \
             waiting on it is left hanging"
        );
    }

    #[tokio::test]
    async fn test_cleanup_on_an_empty_map_is_a_no_op() {
        let sessions: Arc<RwLock<HashMap<String, InteractiveSession>>> =
            Arc::new(RwLock::new(HashMap::new()));
        InteractiveSessionManager::cleanup_expired_sessions(sessions.clone(), 0).await;
        assert!(sessions.read().is_empty());
    }

    // ───────────── liveness detection, with a portable fake process ─────────────

    #[tokio::test]
    async fn test_try_wait_on_dead_process() {
        let cli = FakeCli::exiting();
        let mut child = dead_child(&cli).await;

        let result = child.try_wait();
        assert!(result.is_ok());
        assert!(
            result.unwrap().is_some(),
            "try_wait should return Some(ExitStatus) for a dead process"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_an_unreadable_process_status_is_treated_as_death() {
        // `waitpid` steals the exit status, so tokio's `try_wait` can only answer
        // ECHILD. That is the `Err` arm of the liveness probe, unreachable from a
        // test any other way, and the code treats it like a dead process:
        // the session is dropped and recreated with `--continue`.
        let cli = FakeCli::exiting();
        let manager = manager(&cli);
        let mut child = spawn_fake(&cli);
        let pid = child.id().expect("a fresh child has a pid") as i32;
        let mut status = 0;
        assert!(
            unsafe { libc::waitpid(pid, &mut status, 0) } > 0,
            "waitpid should have reaped the fake CLI"
        );
        assert!(
            child.try_wait().is_err(),
            "tokio cannot reap what waitpid already took"
        );

        let (session, _stdin_rx, _output_tx) = manual_session("conv-ghost", child, 4, 4);
        manager
            .sessions
            .write()
            .insert("conv-ghost".to_string(), session);
        cli.forget_argv();

        let (id, rx) = manager
            .get_or_create_session_and_send(
                Some("conv-ghost".to_string()),
                "opus".to_string(),
                "hi".to_string(),
            )
            .await
            .expect("recovery");

        assert_eq!(id, "conv-ghost");
        let outputs = collect_all(rx).await;
        assert_eq!(types_of(&outputs), vec!["result"]);
        assert_eq!(outputs[0].subtype.as_deref(), Some("process_died"));
        assert!(
            cli.argv().await.contains("--continue"),
            "an unreadable status must trigger the same recovery as a dead process"
        );
        assert_eq!(manager.active_sessions(), 1);
    }

    #[tokio::test]
    async fn test_try_wait_on_alive_process() {
        // The fake CLI blocks on stdin instead of calling `sleep 60`, so nothing
        // survives the test and the test runs on Windows too.
        let cli = FakeCli::blocking();
        let mut child = spawn_fake(&cli);

        let result = child.try_wait();
        assert!(result.is_ok());
        assert!(
            result.unwrap().is_none(),
            "try_wait should return None for a running process"
        );

        let _ = child.kill().await;
    }
}
