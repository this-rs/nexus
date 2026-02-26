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

        // MCP configuration
        if self.mcp_config.enabled
            && let Some(ref config_file) = self.mcp_config.config_file
        {
            cmd.arg("--mcp-config").arg(config_file);
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

        info!(
            "Starting interactive Claude session with command: {:?}",
            cmd
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
        let (initial_tx, mut initial_rx) = mpsc::channel::<ClaudeCodeOutput>(100);

        // Initial response collector task
        // Uses Result message detection instead of timeout heuristic.
        // Sidechain messages (from Task tool subagents) are filtered out.
        let initial_response_tx_clone = initial_response_tx.clone();
        tokio::spawn(async move {
            let start_time = std::time::Instant::now();

            loop {
                match tokio::time::timeout(std::time::Duration::from_secs(30), initial_rx.recv())
                    .await
                {
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

                        if initial_response_tx_clone.send(output).await.is_err() {
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
        });

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
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                warn!("Claude stderr: {}", line);
            }
        });

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

    /// Pre-warm a default process for faster first request.
    pub async fn prewarm_default_session(&self) -> Result<()> {
        info!("Pre-warming default Claude process for faster first request");

        // TODO: Implement pre-warming logic
        // Skipped for now — called from main.rs

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

    // ── build_process_died_event ──

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
        // Response collectors break on `type == "result"` — verify the synthetic event
        // would trigger that break condition.
        let event = build_process_died_event("dead");
        assert_eq!(event.r#type, "result");
        // The subtype distinguishes it from a normal result
        assert_eq!(event.subtype, Some("process_died".to_string()));
    }

    // ── SessionStatus enum ──

    #[test]
    fn test_session_status_variants_exist() {
        // Compile-time test: all variants are constructible
        let _alive = SessionStatus::Alive;
        let _dead = SessionStatus::Dead;
        let _not_found = SessionStatus::NotFound;
    }

    // ── Liveness detection with a real process ──

    #[tokio::test]
    async fn test_try_wait_on_dead_process() {
        // Spawn a process that exits immediately
        let mut child = Command::new("true")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn `true`");

        // Wait for it to finish
        let _ = child.wait().await;

        // try_wait should report it as exited
        let result = child.try_wait();
        assert!(result.is_ok());
        assert!(
            result.unwrap().is_some(),
            "try_wait should return Some(ExitStatus) for a dead process"
        );
    }

    #[tokio::test]
    async fn test_try_wait_on_alive_process() {
        // Spawn a long-running process
        let mut child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn `sleep`");

        // try_wait should report it as still running
        let result = child.try_wait();
        assert!(result.is_ok());
        assert!(
            result.unwrap().is_none(),
            "try_wait should return None for a running process"
        );

        // Clean up
        let _ = child.kill().await;
    }

    // ── Cleanup integration test ──

    #[tokio::test]
    async fn test_cleanup_removes_dead_sessions() {
        let sessions: Arc<RwLock<HashMap<String, InteractiveSession>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // Spawn a process that exits immediately
        let mut child = Command::new("true")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn");
        let stdin = child.stdin.take().unwrap();
        let _ = child.wait().await; // Wait for it to die

        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(1);
        let (output_tx, _) = broadcast::channel(1);

        // Consume stdin_rx so the channel doesn't hang
        tokio::spawn(async move { while stdin_rx.recv().await.is_some() {} });
        // Drop the real stdin so the process doesn't hang
        drop(stdin);

        let session = InteractiveSession {
            id: "test-id".to_string(),
            conversation_id: "conv-dead".to_string(),
            child,
            stdin_tx,
            output_tx,
            model: "test".to_string(),
            created_at: std::time::Instant::now(),
            last_used: Arc::new(parking_lot::Mutex::new(std::time::Instant::now())),
            interaction_lock: Arc::new(tokio::sync::Mutex::new(())),
        };

        sessions.write().insert("conv-dead".to_string(), session);
        assert_eq!(sessions.read().len(), 1);

        // Run cleanup with a very long timeout (so only dead detection triggers)
        InteractiveSessionManager::cleanup_expired_sessions(sessions.clone(), 9999).await;

        assert_eq!(
            sessions.read().len(),
            0,
            "Dead session should have been removed"
        );
    }

    #[tokio::test]
    async fn test_cleanup_keeps_alive_sessions() {
        let sessions: Arc<RwLock<HashMap<String, InteractiveSession>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // Spawn a long-running process
        let mut child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn");
        let stdin = child.stdin.take().unwrap();

        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(1);
        let (output_tx, _) = broadcast::channel(1);

        tokio::spawn(async move { while stdin_rx.recv().await.is_some() {} });
        drop(stdin);

        let session = InteractiveSession {
            id: "test-id".to_string(),
            conversation_id: "conv-alive".to_string(),
            child,
            stdin_tx,
            output_tx,
            model: "test".to_string(),
            created_at: std::time::Instant::now(),
            last_used: Arc::new(parking_lot::Mutex::new(std::time::Instant::now())),
            interaction_lock: Arc::new(tokio::sync::Mutex::new(())),
        };

        sessions.write().insert("conv-alive".to_string(), session);

        // Cleanup with long timeout — alive process should stay
        InteractiveSessionManager::cleanup_expired_sessions(sessions.clone(), 9999).await;

        assert_eq!(
            sessions.read().len(),
            1,
            "Alive session should not be removed"
        );

        // Clean up the process (extract from lock scope to avoid holding RwLock across await)
        let removed = sessions.write().remove("conv-alive");
        if let Some(mut s) = removed {
            let _ = s.child.kill().await;
        }
    }

    #[tokio::test]
    async fn test_cleanup_removes_expired_sessions() {
        let sessions: Arc<RwLock<HashMap<String, InteractiveSession>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // Spawn a long-running process
        let mut child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn");
        let stdin = child.stdin.take().unwrap();

        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(1);
        let (output_tx, _) = broadcast::channel(1);

        tokio::spawn(async move { while stdin_rx.recv().await.is_some() {} });
        drop(stdin);

        let session = InteractiveSession {
            id: "test-id".to_string(),
            conversation_id: "conv-expired".to_string(),
            child,
            stdin_tx,
            output_tx,
            model: "test".to_string(),
            created_at: std::time::Instant::now(),
            last_used: Arc::new(parking_lot::Mutex::new(std::time::Instant::now())),
            interaction_lock: Arc::new(tokio::sync::Mutex::new(())),
        };

        sessions.write().insert("conv-expired".to_string(), session);

        // Use timeout_minutes=0 so ANY session is immediately "expired".
        // This avoids Instant subtraction overflow on Windows.
        InteractiveSessionManager::cleanup_expired_sessions(sessions.clone(), 0).await;

        assert_eq!(
            sessions.read().len(),
            0,
            "Expired session should have been removed"
        );
    }

    #[tokio::test]
    async fn test_cleanup_emits_process_died_for_dead_sessions() {
        let sessions: Arc<RwLock<HashMap<String, InteractiveSession>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // Spawn a process that exits immediately
        let mut child = Command::new("true")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn");
        let stdin = child.stdin.take().unwrap();
        let _ = child.wait().await;

        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(1);
        let (output_tx, _) = broadcast::channel(16);

        // Subscribe BEFORE cleanup so we can receive the synthetic event
        let mut subscriber = output_tx.subscribe();

        tokio::spawn(async move { while stdin_rx.recv().await.is_some() {} });
        drop(stdin);

        let session = InteractiveSession {
            id: "test-id".to_string(),
            conversation_id: "conv-dead-notify".to_string(),
            child,
            stdin_tx,
            output_tx,
            model: "test".to_string(),
            created_at: std::time::Instant::now(),
            last_used: Arc::new(parking_lot::Mutex::new(std::time::Instant::now())),
            interaction_lock: Arc::new(tokio::sync::Mutex::new(())),
        };

        sessions
            .write()
            .insert("conv-dead-notify".to_string(), session);

        // Run cleanup
        InteractiveSessionManager::cleanup_expired_sessions(sessions.clone(), 9999).await;

        // Should have received a process_died event
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(1), subscriber.recv()).await;

        assert!(event.is_ok(), "Should receive event within timeout");
        let event = event.unwrap().expect("Should receive event");
        assert_eq!(event.r#type, "result");
        assert_eq!(event.subtype.as_deref(), Some("process_died"));
    }
}
