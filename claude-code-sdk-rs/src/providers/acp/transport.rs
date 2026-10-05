//! The agent process: launched through the single launcher ([`isolated_command`]),
//! JSON-RPC lines on stdio, request/response correlation, and a shutdown that kills
//! the whole process tree.
//!
//! One process per session. The reader task parses every line: a response goes to the
//! waiting request, anything else (notification, agent request) goes to the session's
//! pump as an [`Inbound`]; the end of stdout is [`Inbound::Closed`].
//!
//! The answer to a `session/prompt` is **ordered** with the notifications: it reaches
//! the pump through the same channel, so the turn never ends before the last
//! `session/update` that preceded its answer has been mapped.
//!
//! # Duplication with `providers::codex::transport`
//!
//! This file is a deliberate copy of the Codex transport, adapted: the
//! `"jsonrpc":"2.0"` header (wire), ordered responses, no `-32001` mapping. Sharing it
//! would make `provider-acp` depend on `provider-codex` (a cargo feature of its own,
//! non-default) or move Codex code; neither was worth the risk in this slice. A later
//! `providers/jsonrpc/` extraction would absorb both.
//!
//! # Killing the tree
//!
//! The child is its own process group leader (`process_group(0)`), and
//! [`Process::shutdown`] first reads the child's descendants from the process table,
//! then sends `SIGKILL` to each of them and to the group, then to the child itself.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};

use super::map::classify_rpc_error;
use super::wire::{self, Frame, RpcError};
use crate::agent::{ProviderError, redact};
use crate::providers::claude_code::cancel;
use crate::transport::spawn::{EnvPolicy, isolated_command};

/// Longest line the reader accepts (a tool output can be large).
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// What the reader hands the session.
#[derive(Debug)]
pub enum Inbound {
    /// A notification.
    Notification {
        /// Method.
        method: String,
        /// Parameters.
        params: Value,
    },
    /// A request the agent expects an answer to.
    Request {
        /// Id, echoed untouched in the answer.
        id: Value,
        /// Method.
        method: String,
        /// Parameters.
        params: Value,
    },
    /// The answer to a request sent with [`Process::send_ordered`].
    Response {
        /// Id of the request.
        id: i64,
        /// Result or error.
        outcome: Result<Value, RpcError>,
    },
    /// A line that is not a JSON-RPC message (counted, never fatal).
    Malformed(String),
    /// The process is gone.
    Closed {
        /// Exit code, when it exited by itself.
        code: Option<i32>,
    },
}

/// How to start a process.
pub struct Launch {
    /// Program.
    pub program: PathBuf,
    /// Arguments, secrets excluded.
    pub args: Vec<String>,
    /// What to inherit from the host.
    pub env_policy: EnvPolicy,
    /// Variables set explicitly, **after** the allowlist: the instance's own and the
    /// session's (`SessionSpec::env.set`).
    pub env: Vec<(String, String)>,
    /// Working directory.
    pub cwd: PathBuf,
}

type Waiting = oneshot::Sender<Result<Value, RpcError>>;

/// A running agent.
pub struct Process {
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    pending: Mutex<HashMap<i64, Waiting>>,
    ordered: Mutex<HashSet<i64>>,
    next_id: AtomicI64,
    child: tokio::sync::Mutex<Child>,
    pid: Option<u32>,
    /// `Some(code)` once the process is gone.
    dead: Mutex<Option<Option<i32>>>,
    closing: AtomicBool,
}

impl std::fmt::Debug for Process {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Process")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

/// Maps a spawn failure to a typed error.
pub fn spawn_error(error: &std::io::Error, program: &std::path::Path) -> ProviderError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ProviderError::CliNotFound {
            program: redact(&program.display().to_string()),
        }
    } else {
        ProviderError::protocol(format!("the agent could not start: {:?}", error.kind()))
    }
}

/// Builds the command of a launch: the single launcher, then explicit variables.
pub fn command(launch: &Launch) -> tokio::process::Command {
    let mut cmd = isolated_command(&launch.program, &launch.env_policy);
    cmd.args(&launch.args).current_dir(&launch.cwd);
    for (name, value) in &launch.env {
        cmd.env(name, value);
    }
    cmd
}

impl Process {
    /// Starts the process and its reader.
    pub fn spawn(
        launch: &Launch,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Inbound>), ProviderError> {
        let mut cmd = command(launch);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|error| spawn_error(&error, &launch.program))?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProviderError::protocol("the agent has no stdout"))?;
        let process = Arc::new(Self {
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            ordered: Mutex::new(HashSet::new()),
            next_id: AtomicI64::new(1),
            child: tokio::sync::Mutex::new(child),
            pid,
            dead: Mutex::new(None),
            closing: AtomicBool::new(false),
        });
        let (sender, receiver) = mpsc::unbounded_channel();
        tokio::spawn(read_loop(Arc::clone(&process), stdout, sender));
        Ok((process, receiver))
    }

    /// The process id.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// `Some(exit code)` once the process is gone.
    pub fn death(&self) -> Option<Option<i32>> {
        *self.dead.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn died_error(&self) -> ProviderError {
        ProviderError::ProcessExited {
            code: self.death().flatten(),
        }
    }

    /// Writes one JSON line.
    async fn write(&self, message: &Value) -> Result<(), ProviderError> {
        if self.death().is_some() {
            return Err(self.died_error());
        }
        let mut line = wire::canonical_line(message);
        line.push('\n');
        let mut guard = self.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(self.died_error());
        };
        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            drop(guard);
            self.wait_for_exit().await;
            return Err(self.died_error());
        }
        Ok(())
    }

    async fn wait_for_exit(&self) {
        for _ in 0..50 {
            if self.death().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Reserves the id of a request (so a caller can record it before sending).
    pub fn next_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Sends a request and waits for its answer. A JSON-RPC error is classified
    /// ([`classify_rpc_error`]).
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        within: Duration,
    ) -> Result<Value, ProviderError> {
        let id = self.next_id();
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, sender);
        if let Err(error) = self.write(&wire::request_line(id, method, params)).await {
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(within, receiver).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => Err(classify_rpc_error(&error)),
            // The reader dropped the sender: the process is gone.
            Ok(Err(_)) => Err(self.died_error()),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                Err(ProviderError::Timeout {
                    after_ms: u64::try_from(within.as_millis()).unwrap_or(u64::MAX),
                })
            },
        }
    }

    /// Sends a request whose answer comes back **in order** on the inbound channel
    /// ([`Inbound::Response`]), with the id reserved by [`Process::next_id`].
    pub async fn send_ordered(
        &self,
        id: i64,
        method: &str,
        params: Value,
    ) -> Result<(), ProviderError> {
        self.ordered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id);
        if let Err(error) = self.write(&wire::request_line(id, method, params)).await {
            self.ordered
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            return Err(error);
        }
        Ok(())
    }

    /// Sends a notification.
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), ProviderError> {
        self.write(&wire::notification_line(method, params)).await
    }

    /// Answers a request of the agent.
    pub async fn respond(&self, id: &Value, result: Value) -> Result<(), ProviderError> {
        self.write(&wire::response_line(id, result)).await
    }

    /// Refuses a request of the agent (`-32601`: method not found).
    pub async fn respond_error(
        &self,
        id: &Value,
        code: i64,
        message: &str,
    ) -> Result<(), ProviderError> {
        self.write(&wire::error_response_line(id, code, message))
            .await
    }

    fn fail_all(&self, code: Option<i32>) {
        *self.dead.lock().unwrap_or_else(PoisonError::into_inner) = Some(code);
        // Dropping the senders wakes every waiting request with the death.
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Kills the process and **all its descendants**. Idempotent.
    pub async fn shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
        if let Some(pid) = self.pid {
            // Read the tree before touching anything: killing the root first would
            // re-parent the children and hide them.
            let descendants = cancel::descendant_pids(pid).await;
            for descendant in descendants {
                #[cfg(unix)]
                cancel::signal_pid(descendant, libc::SIGKILL);
                #[cfg(not(unix))]
                let _ = descendant;
            }
            #[cfg(unix)]
            if let Ok(group) = i32::try_from(pid)
                && group > 1
            {
                // SAFETY: `killpg` takes two integers; `group` is the pid of our own
                // child, which `process_group(0)` made the leader of its group.
                unsafe {
                    libc::killpg(group, libc::SIGKILL);
                }
            }
        }
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.start_kill();
        }
        self.stdin.lock().await.take();
        self.fail_all(self.death().flatten());
    }
}

async fn read_loop(
    process: Arc<Process>,
    stdout: ChildStdout,
    inbound: mpsc::UnboundedSender<Inbound>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > MAX_LINE_BYTES {
            let _ = inbound.send(Inbound::Malformed("line too long".to_owned()));
            continue;
        }
        match Frame::parse(&line) {
            Ok(Frame::Response { id, outcome }) => {
                let Some(id) = id.as_i64() else { continue };
                let ordered = process
                    .ordered
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                if ordered {
                    let _ = inbound.send(Inbound::Response { id, outcome });
                    continue;
                }
                let waiting = process
                    .pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id);
                if let Some(waiting) = waiting {
                    let _ = waiting.send(outcome);
                }
            },
            Ok(Frame::Request { id, method, params }) => {
                let _ = inbound.send(Inbound::Request { id, method, params });
            },
            Ok(Frame::Notification { method, params }) => {
                let _ = inbound.send(Inbound::Notification { method, params });
            },
            Err(reason) => {
                let _ = inbound.send(Inbound::Malformed(reason));
            },
        }
    }
    // stdout closed: the process exited, or is about to.
    let code = {
        let mut child = process.child.lock().await;
        match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
            Ok(Ok(status)) => status.code(),
            _ => None,
        }
    };
    process.fail_all(code);
    if !process.closing.load(Ordering::SeqCst) {
        let _ = inbound.send(Inbound::Closed { code });
    }
}
