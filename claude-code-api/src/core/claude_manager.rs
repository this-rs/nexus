use anyhow::{Result, anyhow};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use nexus_claude::describe_command_redacted;
use uuid::Uuid;

use crate::core::config::{FileAccessConfig, MCPConfig};
use crate::models::claude::ClaudeCodeOutput;

pub struct ClaudeProcess {
    #[allow(dead_code)]
    pub id: String,
    pub child: Option<Child>,
    #[allow(dead_code)]
    pub project_path: Option<String>,
}

pub struct ClaudeManager {
    processes: Arc<RwLock<HashMap<String, ClaudeProcess>>>,
    claude_command: String,
    #[allow(dead_code)]
    file_access_config: FileAccessConfig,
    mcp_config: MCPConfig,
}

impl ClaudeManager {
    pub fn new(
        claude_command: String,
        file_access_config: FileAccessConfig,
        mcp_config: MCPConfig,
    ) -> Self {
        Self {
            processes: Arc::new(RwLock::new(HashMap::new())),
            claude_command,
            file_access_config,
            mcp_config,
        }
    }

    #[allow(dead_code)]
    pub async fn create_interactive_session(
        &self,
        session_id: Option<String>,
        project_path: Option<String>,
        model: Option<String>,
    ) -> Result<(String, mpsc::Receiver<ClaudeCodeOutput>)> {
        let session_id = session_id.unwrap_or_else(|| Uuid::new_v4().to_string());

        let mut cmd = Command::new(&self.claude_command);
        // 交互模式，使用 stream-json 输出以支持多轮对话
        cmd.arg("--output-format")
            .arg("stream-json")
            .arg("--verbose");

        if let Some(model) = model {
            cmd.arg("--model").arg(model);
        }

        if let Some(ref path) = project_path {
            cmd.arg("--cwd").arg(path);
        }

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Never `{:?}` a Command: its Debug prints every argument and every
        // environment value. Redacted description instead — see
        // nexus_claude::describe_command_redacted.
        info!(
            "Starting interactive Claude session {} with command: {}",
            session_id,
            describe_command_redacted(cmd.as_std())
        );

        let mut child = cmd.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to get stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Failed to get stderr"))?;

        let (tx, rx) = mpsc::channel(100);

        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    warn!("Claude stderr: {}", line);
                }
            }
        });

        let tx_clone = tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }

                match serde_json::from_str::<serde_json::Value>(&line) {
                    Ok(json) => {
                        // 转换为 ClaudeCodeOutput 格式
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

                        if tx_clone.send(output).await.is_err() {
                            break;
                        }
                    },
                    Err(e) => {
                        error!(
                            "Failed to parse Claude output as JSON: {} - Line: {}",
                            e, line
                        );
                    },
                }
            }
        });

        let process = ClaudeProcess {
            id: session_id.clone(),
            child: Some(child),
            project_path,
        };

        self.processes.write().insert(session_id.clone(), process);

        Ok((session_id, rx))
    }

    pub async fn create_session_with_message(
        &self,
        session_id: Option<String>,
        project_path: Option<String>,
        model: Option<String>,
        message: &str,
    ) -> Result<(String, mpsc::Receiver<ClaudeCodeOutput>)> {
        let session_id = session_id.unwrap_or_else(|| Uuid::new_v4().to_string());

        let mut cmd = Command::new(&self.claude_command);
        cmd.arg("--print")
            .arg("--verbose")  // stream-json 需要 verbose
            .arg("--output-format").arg("stream-json");

        if let Some(model) = model {
            cmd.arg("--model").arg(model);
        }

        if let Some(ref path) = project_path {
            cmd.arg("--cwd").arg(path);
        }

        // 默认跳过权限检查以提高性能
        cmd.arg("--dangerously-skip-permissions");

        if self.mcp_config.enabled {
            if let Some(ref config_file) = self.mcp_config.config_file {
                cmd.arg("--mcp-config").arg(config_file);
            } else if let Some(ref config_json) = self.mcp_config.config_json {
                cmd.arg("--mcp-config").arg(config_json);
            }

            if self.mcp_config.strict {
                cmd.arg("--strict-mcp-config");
            }

            if self.mcp_config.debug {
                cmd.arg("--debug");
            }
        }

        // 不要将 message 作为命令行参数
        // cmd.arg(message);

        cmd.stdin(Stdio::piped())  // 改为 piped 以便写入
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // This command carries `--mcp-config <json>`, which holds each MCP
        // server's env and headers: database password, search key, session
        // token. Logging the Command directly published them at info level.
        info!(
            "Starting Claude process for session {} with command: {}",
            session_id,
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

        // 将消息写入 stdin
        use tokio::io::AsyncWriteExt;
        let message_bytes = message.as_bytes().to_vec();
        tokio::spawn(async move {
            let mut stdin = stdin;
            if let Err(e) = stdin.write_all(&message_bytes).await {
                error!("Failed to write to stdin: {}", e);
            }
            // 关闭 stdin 以表示输入结束
            drop(stdin);
        });

        let (tx, rx) = mpsc::channel(100);

        let session_id_clone = session_id.clone();
        let child_id = child.id();
        tokio::spawn(async move {
            info!(
                "Monitoring Claude process {} for session {}",
                child_id.unwrap_or(0),
                session_id_clone
            );
        });

        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                error!("Claude stderr: {}", line);
            }
            info!("Claude stderr stream ended");
        });

        let tx_clone = tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }

                info!("Claude output line: {}", line);

                match serde_json::from_str::<ClaudeCodeOutput>(&line) {
                    Ok(output) => {
                        info!(
                            "Parsed Claude output: type={}, subtype={:?}",
                            output.r#type, output.subtype
                        );
                        if tx_clone.send(output).await.is_err() {
                            break;
                        }
                    },
                    Err(e) => {
                        error!("Failed to parse Claude output: {} - Line: {}", e, line);
                    },
                }
            }
            info!("Claude output stream ended");
        });

        let process = ClaudeProcess {
            id: session_id.clone(),
            child: Some(child),
            project_path,
        };

        self.processes.write().insert(session_id.clone(), process);

        Ok((session_id, rx))
    }

    #[allow(dead_code)]
    pub async fn send_message(&self, session_id: &str, message: &str) -> Result<()> {
        let stdin = {
            let mut processes = self.processes.write();
            let process = processes
                .get_mut(session_id)
                .ok_or_else(|| anyhow!("Session not found"))?;

            if let Some(ref mut child) = process.child {
                child.stdin.take()
            } else {
                None
            }
        };

        if let Some(mut stdin) = stdin {
            use tokio::io::AsyncWriteExt;
            info!("Writing message to stdin: {} bytes", message.len());
            stdin.write_all(message.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
            info!("Message sent successfully");

            // 把 stdin 放回去
            let mut processes = self.processes.write();
            if let Some(process) = processes.get_mut(session_id)
                && let Some(ref mut child) = process.child
            {
                child.stdin = Some(stdin);
            }
        } else {
            error!("No stdin available for session {}", session_id);
        }

        Ok(())
    }

    pub async fn close_session(&self, session_id: &str) -> Result<()> {
        let child = {
            let mut processes = self.processes.write();
            processes
                .remove(session_id)
                .and_then(|mut p| p.child.take())
        };

        if let Some(mut child) = child {
            child.kill().await?;
            info!("Closed session {}", session_id);
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub fn get_session_info(&self, session_id: &str) -> Option<(String, Option<String>)> {
        let processes = self.processes.read();
        processes
            .get(session_id)
            .map(|p| (p.id.clone(), p.project_path.clone()))
    }

    #[allow(dead_code)]
    pub async fn cleanup(&self) {
        let children: Vec<_> = {
            let mut processes = self.processes.write();
            processes
                .drain()
                .filter_map(|(_, mut p)| p.child.take())
                .collect()
        };

        for mut child in children {
            let _ = child.kill().await;
        }
    }
}

impl Drop for ClaudeManager {
    fn drop(&mut self) {
        let processes = self.processes.read();
        for id in processes.keys() {
            error!("Warning: Claude process {} still running at shutdown", id);
        }
    }
}

/// Tests for the two spawn paths of [`ClaudeManager`].
///
/// # Where the assertions live
///
/// Everything observable from a return value, from the argv the CLI received or
/// from the `processes` map is asserted here. Everything that is only observable
/// in the **log** — and the redaction of bug S2 is exactly that — lives in
/// `tests/claude_manager_logging.rs`, because capturing a `tracing` event needs a
/// *global* subscriber and therefore a test binary of its own. See that file's
/// header for why a thread-local subscriber cannot do it.
///
/// [`enable_logs`] still installs a global subscriber here, for a different
/// reason: `tracing` short-circuits on the global maximum level, so with no
/// subscriber at all the *arguments* of `info!` are never evaluated and
/// `describe_command_redacted(cmd.as_std())` would not run in a test process
/// while it always runs in production.
///
/// # Lines llvm-cov still reports as uncovered
///
/// * **the argument lines of the three multi-line `tracing` macros** —
///   `describe_command_redacted(cmd.as_std())` in both constructors, and
///   `child_id.unwrap_or(0)` in the monitoring task. `cargo llvm-cov` gives an
///   argument written on its own line a second region that is never taken, even
///   though the expression runs. It demonstrably does run: the logging tests
///   assert on the redacted text it produces and on the real pid it reports.
///   Measurement artifact, not a gap.
/// * **the end of the `if let … && let …` chain that puts stdin back** in
///   [`ClaudeManager::send_message`]. Its false arm needs the session's entry, or
///   its child, to disappear between the `await` on the write and the reacquired
///   lock — a race with `close_session` or `cleanup` on the same id. It cannot
///   happen in production: `send_message` is `#[allow(dead_code)]` and is called
///   from nowhere in the crate. Reaching it from a test would mean mutating the
///   map behind the function's back mid-write, which would assert on a scenario
///   the code never sees.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::{FileAccessConfig, MCPConfig};
    use serde_json::json;
    use std::io::Write as _;
    use std::time::Duration;
    use tempfile::TempDir;

    /// A value that must never reach a log, and never be mistaken for anything
    /// a description would print by accident.
    const MCP_SENTINEL: &str = "mcp-payload-sentinel-must-not-leak";

    /// How long a test waits on the CLI before declaring the channel stuck.
    const PATIENCE: Duration = Duration::from_secs(10);

    // ───────────────────────────── the fake CLI ─────────────────────────────

    /// A scripted stand-in for the `claude` binary.
    ///
    /// `ClaudeManager` has no transport abstraction: both constructors call
    /// `Command::new(&self.claude_command)` directly, so the only seam is that
    /// string. The script records the argv it was given (and optionally
    /// everything it read on stdin), replays a canned `stream-json` transcript
    /// and exits — no network, no real CLI, nothing long-lived unless a test
    /// explicitly asks for it.
    struct FakeCli {
        _dir: TempDir,
        script: std::path::PathBuf,
        argv: std::path::PathBuf,
        stdin: std::path::PathBuf,
        marker: std::path::PathBuf,
    }

    /// What [`FakeCli`] does with the stdin the gateway hands it.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Stdin {
        /// Never read it. The process exits whatever the gateway does with the
        /// pipe — which is what makes the `create_interactive_session` tests
        /// terminate at all.
        Ignore,
        /// Copy it to a file, which also keeps the process alive until the pipe
        /// is closed.
        Record,
    }

    struct FakeCliSpec<'a> {
        stdout: &'a str,
        stderr: &'a [&'a str],
        stdin: Stdin,
        exit_code: i32,
        /// Create `marker.txt`, but only if printing the transcript succeeded.
        mark_completion: bool,
    }

    impl FakeCli {
        fn build(spec: FakeCliSpec<'_>) -> Self {
            let dir = tempfile::tempdir().expect("tempdir for the fake CLI");
            let payload = dir.path().join("payload.ndjson");
            std::fs::write(&payload, spec.stdout).expect("write the fake CLI payload");
            let argv = dir.path().join("argv.txt");
            let stdin = dir.path().join("stdin.txt");
            let marker = dir.path().join("marker.txt");
            let script = dir
                .path()
                .join(if cfg!(windows) { "fake.cmd" } else { "fake.sh" });

            let body = if cfg!(windows) {
                let mut body = String::from("@echo off\r\n");
                // Redirection first: an argument ending in a digit would
                // otherwise be read by cmd.exe as a stream number.
                body.push_str(&format!("> \"{}\" echo %*\r\n", argv.display()));
                if spec.stdin == Stdin::Record {
                    body.push_str(&format!("sort > \"{}\"\r\n", stdin.display()));
                }
                if spec.mark_completion {
                    body.push_str(&format!(
                        "type \"{}\" && echo done > \"{}\"\r\n",
                        payload.display(),
                        marker.display()
                    ));
                } else {
                    body.push_str(&format!("type \"{}\"\r\n", payload.display()));
                }
                for line in spec.stderr {
                    body.push_str(&format!("echo {line} 1>&2\r\n"));
                }
                body.push_str(&format!("exit /b {}\r\n", spec.exit_code));
                body
            } else {
                let mut body = String::from("#!/bin/sh\n");
                body.push_str(&format!("echo \"$@\" > '{}'\n", argv.display()));
                if spec.stdin == Stdin::Record {
                    body.push_str(&format!("cat > '{}'\n", stdin.display()));
                }
                if spec.mark_completion {
                    body.push_str(&format!(
                        "cat '{}' && echo done > '{}'\n",
                        payload.display(),
                        marker.display()
                    ));
                } else {
                    body.push_str(&format!("cat '{}'\n", payload.display()));
                }
                for line in spec.stderr {
                    body.push_str(&format!("echo '{line}' 1>&2\n"));
                }
                body.push_str(&format!("exit {}\n", spec.exit_code));
                body
            };

            let mut file = std::fs::File::create(&script).expect("create the fake CLI script");
            file.write_all(body.as_bytes())
                .expect("write the fake CLI script");
            drop(file);

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod the fake CLI script");
            }

            Self {
                _dir: dir,
                script,
                argv,
                stdin,
                marker,
            }
        }

        /// Prints `transcript` on stdout and exits 0 without reading stdin.
        fn emitting(transcript: &str) -> Self {
            Self::build(FakeCliSpec {
                stdout: transcript,
                stderr: &[],
                stdin: Stdin::Ignore,
                exit_code: 0,
                mark_completion: false,
            })
        }

        /// Prints nothing and exits 0 almost at once.
        fn exiting() -> Self {
            Self::emitting("")
        }

        /// Copies stdin to a file, then prints `transcript`. Stays alive until
        /// the gateway closes the pipe.
        fn recording_stdin(transcript: &str) -> Self {
            Self::build(FakeCliSpec {
                stdout: transcript,
                stderr: &[],
                stdin: Stdin::Record,
                exit_code: 0,
                mark_completion: false,
            })
        }

        /// Prints `transcript`, then writes each of `stderr` on fd 2.
        fn noisy(transcript: &str, stderr: &[&str]) -> Self {
            Self::build(FakeCliSpec {
                stdout: transcript,
                stderr,
                stdin: Stdin::Ignore,
                exit_code: 0,
                mark_completion: false,
            })
        }

        /// Prints a transcript **larger than any pipe buffer**, then records
        /// that it got to the end.
        ///
        /// The marker is the witness for the `break` taken when the output
        /// channel's receiver is gone: a transcript that cannot fit in the pipe
        /// means the CLI is still writing when the pump lets go of stdout, so it
        /// is cut off and never reaches the marker.
        fn emitting_more_than_fits(transcript: &str) -> Self {
            Self::build(FakeCliSpec {
                stdout: transcript,
                stderr: &[],
                stdin: Stdin::Ignore,
                exit_code: 0,
                mark_completion: true,
            })
        }

        fn command(&self) -> String {
            self.script.to_string_lossy().into_owned()
        }

        fn reached_the_end_of_its_transcript(&self) -> bool {
            self.marker.exists()
        }

        /// The argv of the last spawn, waiting for the script to record it.
        ///
        /// Both constructors return as soon as the process is spawned, so the
        /// script may not have run yet.
        async fn argv(&self) -> String {
            poll_file(&self.argv, |s| !s.trim().is_empty())
                .await
                .expect("the fake CLI never recorded its argv")
        }

        /// Everything the fake CLI read on stdin, once it contains `needle`.
        async fn stdin_containing(&self, needle: &str) -> String {
            poll_file(&self.stdin, |s| s.contains(needle))
                .await
                .unwrap_or_else(|got| {
                    panic!("the CLI never read {needle:?} on stdin; it read {got:?}")
                })
        }
    }

    /// Reads `path` until `done` accepts its contents, or gives up.
    ///
    /// Never use this under `start_paused`: the sleep would not advance.
    async fn poll_file(
        path: &std::path::Path,
        done: impl Fn(&str) -> bool,
    ) -> std::result::Result<String, String> {
        let mut last = String::new();
        for _ in 0..500 {
            last = std::fs::read_to_string(path).unwrap_or_default();
            if done(&last) {
                return Ok(last);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(last)
    }

    // ────────────────────────────── log plumbing ─────────────────────────────

    /// Make `DEBUG` live for the whole test binary.
    ///
    /// `tracing` compares the level against a *global* maximum before touching a
    /// callsite, so with no subscriber registered the arguments of `info!` are
    /// never evaluated — and in this file the argument is
    /// `describe_command_redacted(cmd.as_std())`, i.e. the statement that keeps
    /// secrets out of the log. Without this the two spawn log lines are dead
    /// code in a test process while they always run in production, where `main`
    /// installs a subscriber.
    ///
    /// `DEBUG` rather than `INFO` on purpose: `api::chat`'s tests assert the
    /// global maximum is at least `DEBUG`, and whichever module gets there first
    /// wins for the whole process.
    fn enable_logs() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let _ = tracing::subscriber::set_global_default(
                tracing_subscriber::FmtSubscriber::builder()
                    .with_max_level(tracing::Level::DEBUG)
                    .with_test_writer()
                    .finish(),
            );
            tracing::callsite::rebuild_interest_cache();
        });
        assert!(
            tracing::level_filters::LevelFilter::current() >= tracing::Level::DEBUG,
            "the DEBUG level must be live, or the logging statements under test are skipped"
        );
    }

    // ───────────────────────────── manager builders ──────────────────────────

    fn manager(cli: &FakeCli) -> ClaudeManager {
        manager_with(
            cli.command(),
            FileAccessConfig::default(),
            MCPConfig::default(),
        )
    }

    fn manager_with(
        command: String,
        file_access_config: FileAccessConfig,
        mcp_config: MCPConfig,
    ) -> ClaudeManager {
        enable_logs();
        ClaudeManager::new(command, file_access_config, mcp_config)
    }

    fn manager_with_mcp(cli: &FakeCli, mcp: MCPConfig) -> ClaudeManager {
        manager_with(cli.command(), FileAccessConfig::default(), mcp)
    }

    /// A manager pointed at a path that is not an executable on any platform.
    fn unspawnable_manager() -> ClaudeManager {
        manager_with(
            "nexus-test-claude-does-not-exist".to_string(),
            FileAccessConfig::default(),
            MCPConfig::default(),
        )
    }

    // ───────────────────────────── transcripts ───────────────────────────────

    fn line(value: serde_json::Value) -> String {
        format!("{value}\n")
    }

    fn assistant_line(text: &str) -> String {
        line(json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
        }))
    }

    /// A transcript several megabytes long: no pipe buffer on any platform
    /// holds it, so the CLI cannot finish writing without a reader.
    fn transcript_larger_than_a_pipe_buffer() -> String {
        let mut transcript = String::new();
        while transcript.len() < 4 * 1024 * 1024 {
            transcript.push_str(&assistant_line("une ligne parmi beaucoup"));
        }
        transcript
    }

    fn result_line() -> String {
        line(json!({"type": "result", "subtype": "success", "is_error": false}))
    }

    // ───────────────────────────── channel draining ──────────────────────────

    /// Drains `rx` to its close, failing rather than hanging.
    async fn drain(mut rx: mpsc::Receiver<ClaudeCodeOutput>) -> Vec<ClaudeCodeOutput> {
        let mut collected = Vec::new();
        loop {
            match tokio::time::timeout(PATIENCE, rx.recv()).await {
                Ok(Some(output)) => collected.push(output),
                Ok(None) => return collected,
                Err(_) => panic!(
                    "the output channel never closed ({} message(s) collected)",
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

    // ─────────────────────── argv helpers ───────────────────────

    fn tokens(argv: &str) -> Vec<String> {
        argv.split_whitespace().map(|s| s.to_string()).collect()
    }

    /// True when `flag` is immediately followed by `value` in the argv.
    fn flag_with(argv: &str, flag: &str, value: &str) -> bool {
        tokens(argv)
            .windows(2)
            .any(|w| w[0] == flag && w[1] == value)
    }

    fn has_flag(argv: &str, flag: &str) -> bool {
        tokens(argv).iter().any(|t| t == flag)
    }

    // ───────────────────── new / get_session_info ─────────────────────

    #[test]
    fn a_new_manager_owns_no_process() {
        let manager = unspawnable_manager();
        assert!(
            manager.get_session_info("anything").is_none(),
            "a fresh manager must not claim to know a session"
        );
        assert!(manager.processes.read().is_empty());
    }

    #[tokio::test]
    async fn get_session_info_returns_the_id_and_the_project_path() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager(&cli);
        let (id, rx) = manager
            .create_session_with_message(
                Some("sess-info".to_string()),
                Some("project-root-xyz".to_string()),
                None,
                "hi",
            )
            .await
            .expect("the fake CLI must spawn");

        assert_eq!(
            manager.get_session_info(&id),
            Some((
                "sess-info".to_string(),
                Some("project-root-xyz".to_string())
            )),
            "the registered process must carry the session id and the project path"
        );
        assert!(
            manager.get_session_info("sess-info-other").is_none(),
            "lookup must not fall back to another session"
        );
        drain(rx).await;
    }

    // ───────────────── create_session_with_message: argv ─────────────────

    #[tokio::test]
    async fn a_one_shot_session_asks_the_cli_for_streaming_json_and_prints() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager(&cli);
        let (_, rx) = manager
            .create_session_with_message(Some("argv-default".to_string()), None, None, "hello")
            .await
            .expect("the fake CLI must spawn");
        drain(rx).await;

        let argv = cli.argv().await;
        assert!(has_flag(&argv, "--print"), "argv: {argv}");
        assert!(has_flag(&argv, "--verbose"), "argv: {argv}");
        assert!(
            flag_with(&argv, "--output-format", "stream-json"),
            "stream-json is what the output parser expects: {argv}"
        );
        assert!(
            !has_flag(&argv, "--model") && !has_flag(&argv, "--cwd"),
            "nothing may be invented when the caller passes None: {argv}"
        );
        assert!(
            !has_flag(&argv, "--mcp-config"),
            "MCP is off by default: {argv}"
        );
        assert!(
            !argv.contains("hello"),
            "the prompt must travel on stdin, never on the command line \
             where `ps` would show it: {argv}"
        );
    }

    #[tokio::test]
    async fn a_one_shot_session_forwards_the_model_and_the_project_path() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager(&cli);
        let (_, rx) = manager
            .create_session_with_message(
                Some("argv-model".to_string()),
                Some("project-root-xyz".to_string()),
                Some("claude-opus-5".to_string()),
                "hello",
            )
            .await
            .expect("the fake CLI must spawn");
        drain(rx).await;

        let argv = cli.argv().await;
        assert!(flag_with(&argv, "--model", "claude-opus-5"), "argv: {argv}");
        assert!(
            flag_with(&argv, "--cwd", "project-root-xyz"),
            "argv: {argv}"
        );
    }

    /// `file_access_config` is `#[allow(dead_code)]` and the flag is
    /// unconditional: a deployment that sets `file_access.skip_permissions =
    /// false` — the default — still gets `--dangerously-skip-permissions`.
    /// `InteractiveSessionManager` honours the same setting, so the two paths
    /// disagree. Pinned rather than changed: removing the flag would make every
    /// one-shot completion wait on a permission prompt that no one can answer.
    #[tokio::test]
    async fn a_one_shot_session_skips_permissions_whatever_the_configuration_says() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager_with(
            cli.command(),
            FileAccessConfig {
                skip_permissions: false,
                additional_dirs: vec!["should-be-forwarded".to_string()],
            },
            MCPConfig::default(),
        );
        let (_, rx) = manager
            .create_session_with_message(Some("argv-perms".to_string()), None, None, "hello")
            .await
            .expect("the fake CLI must spawn");
        drain(rx).await;

        let argv = cli.argv().await;
        assert!(
            has_flag(&argv, "--dangerously-skip-permissions"),
            "the flag is sent unconditionally today: {argv}"
        );
        assert!(
            !argv.contains("should-be-forwarded"),
            "file_access.additional_dirs reaches no argument at all: {argv}"
        );
    }

    #[tokio::test]
    async fn an_mcp_config_file_wins_over_the_inline_json() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager_with_mcp(
            &cli,
            MCPConfig {
                enabled: true,
                config_file: Some("mcp-servers.json".to_string()),
                config_json: Some(MCP_SENTINEL.to_string()),
                strict: false,
                debug: false,
            },
        );
        let (_, rx) = manager
            .create_session_with_message(Some("argv-mcp-file".to_string()), None, None, "hello")
            .await
            .expect("the fake CLI must spawn");
        drain(rx).await;

        let argv = cli.argv().await;
        assert!(
            flag_with(&argv, "--mcp-config", "mcp-servers.json"),
            "argv: {argv}"
        );
        assert!(
            !argv.contains(MCP_SENTINEL),
            "the file wins; the inline JSON must not also be sent: {argv}"
        );
    }

    #[tokio::test]
    async fn the_inline_mcp_json_is_sent_when_no_file_is_configured() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager_with_mcp(
            &cli,
            MCPConfig {
                enabled: true,
                config_file: None,
                config_json: Some(MCP_SENTINEL.to_string()),
                strict: true,
                debug: true,
            },
        );
        let (_, rx) = manager
            .create_session_with_message(Some("argv-mcp-json".to_string()), None, None, "hello")
            .await
            .expect("the fake CLI must spawn");
        drain(rx).await;

        let argv = cli.argv().await;
        assert!(
            flag_with(&argv, "--mcp-config", MCP_SENTINEL),
            "argv: {argv}"
        );
        assert!(has_flag(&argv, "--strict-mcp-config"), "argv: {argv}");
        assert!(has_flag(&argv, "--debug"), "argv: {argv}");
    }

    #[tokio::test]
    async fn a_disabled_mcp_block_is_ignored_in_full() {
        // `enabled: false` must suppress every MCP argument, not just the
        // configuration itself: `strict` and `debug` live inside the same `if`.
        let cli = FakeCli::emitting(&result_line());
        let manager = manager_with_mcp(
            &cli,
            MCPConfig {
                enabled: false,
                config_file: Some("mcp-servers.json".to_string()),
                config_json: Some(MCP_SENTINEL.to_string()),
                strict: true,
                debug: true,
            },
        );
        let (_, rx) = manager
            .create_session_with_message(Some("argv-mcp-off".to_string()), None, None, "hello")
            .await
            .expect("the fake CLI must spawn");
        drain(rx).await;

        let argv = cli.argv().await;
        for forbidden in [
            "--mcp-config",
            "--strict-mcp-config",
            "--debug",
            "mcp-servers.json",
            MCP_SENTINEL,
        ] {
            assert!(!argv.contains(forbidden), "{forbidden} leaked into {argv}");
        }
    }

    /// Both `config_file` and `config_json` unset while `enabled` is true: the
    /// CLI is launched with no MCP configuration at all and no diagnostic. A
    /// deployment that enables MCP and forgets to fill either field gets a
    /// gateway that silently has no MCP server.
    #[tokio::test]
    async fn enabling_mcp_without_a_configuration_is_silent() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager_with_mcp(
            &cli,
            MCPConfig {
                enabled: true,
                config_file: None,
                config_json: None,
                strict: true,
                debug: false,
            },
        );
        let (_, rx) = manager
            .create_session_with_message(Some("argv-mcp-empty".to_string()), None, None, "hello")
            .await
            .expect("the fake CLI must spawn");
        drain(rx).await;

        let argv = cli.argv().await;
        assert!(!has_flag(&argv, "--mcp-config"), "argv: {argv}");
        assert!(
            has_flag(&argv, "--strict-mcp-config"),
            "`--strict-mcp-config` with nothing to be strict about: {argv}"
        );
    }

    // ────────────── create_session_with_message: identity ──────────────

    #[tokio::test]
    async fn a_one_shot_session_keeps_the_caller_session_id() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager(&cli);
        let (id, rx) = manager
            .create_session_with_message(Some("chosen-id".to_string()), None, None, "hello")
            .await
            .expect("the fake CLI must spawn");
        assert_eq!(id, "chosen-id");
        drain(rx).await;
    }

    #[tokio::test]
    async fn a_one_shot_session_mints_a_uuid_when_given_no_id() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager(&cli);
        let (id, rx) = manager
            .create_session_with_message(None, None, None, "hello")
            .await
            .expect("the fake CLI must spawn");
        assert!(
            Uuid::parse_str(&id).is_ok(),
            "a minted id must be a UUID, got {id:?}"
        );
        assert!(manager.get_session_info(&id).is_some());
        drain(rx).await;
    }

    // ─────────────── create_session_with_message: stdin ───────────────

    #[tokio::test]
    async fn the_prompt_is_written_to_stdin_and_the_pipe_is_closed() {
        let cli = FakeCli::recording_stdin(&result_line());
        let manager = manager(&cli);
        let (_, rx) = manager
            .create_session_with_message(
                Some("stdin-session".to_string()),
                None,
                None,
                "explique-moi le mecanisme",
            )
            .await
            .expect("the fake CLI must spawn");

        // The CLI only gets to print its transcript after its stdin reaches
        // EOF, so a closed channel is itself proof the pipe was dropped.
        assert_eq!(types_of(&drain(rx).await), vec!["result"]);
        assert_eq!(
            cli.stdin_containing("mecanisme").await.trim(),
            "explique-moi le mecanisme",
            "the prompt must reach the CLI verbatim, with no trailing newline added"
        );
    }

    // ──────────── create_session_with_message: output parsing ────────────

    #[tokio::test]
    async fn parsed_outputs_arrive_in_order_and_the_channel_then_closes() {
        let cli = FakeCli::emitting(&format!(
            "{}{}",
            assistant_line("Bonjour Nexus"),
            result_line()
        ));
        let manager = manager(&cli);
        let (_, rx) = manager
            .create_session_with_message(Some("order".to_string()), None, None, "hello")
            .await
            .expect("the fake CLI must spawn");

        let outputs = drain(rx).await;
        assert_eq!(types_of(&outputs), vec!["assistant", "result"]);
        assert_eq!(texts_of(&outputs), vec!["Bonjour Nexus".to_string()]);
        assert_eq!(outputs[1].subtype.as_deref(), Some("success"));
    }

    #[tokio::test]
    async fn blank_lines_in_the_transcript_are_skipped() {
        let cli = FakeCli::emitting(&format!(
            "\n   \n{}\n\n{}\n",
            assistant_line("un"),
            result_line()
        ));
        let manager = manager(&cli);
        let (_, rx) = manager
            .create_session_with_message(Some("blanks".to_string()), None, None, "hello")
            .await
            .expect("the fake CLI must spawn");

        assert_eq!(types_of(&drain(rx).await), vec!["assistant", "result"]);
    }

    /// A line the parser cannot read is logged and dropped. The caller sees a
    /// turn that is simply shorter: nothing in the channel says a message was
    /// lost, so a CLI that emits one bad line yields a silently truncated
    /// answer rather than an error.
    #[tokio::test]
    async fn an_unparseable_line_is_dropped_without_telling_the_caller() {
        let cli = FakeCli::emitting(&format!(
            "not json at all\n{}{}",
            assistant_line("survivor"),
            result_line()
        ));
        let manager = manager(&cli);
        let (_, rx) = manager
            .create_session_with_message(Some("garbage".to_string()), None, None, "hello")
            .await
            .expect("the fake CLI must spawn");

        let outputs = drain(rx).await;
        assert_eq!(types_of(&outputs), vec!["assistant", "result"]);
        assert_eq!(texts_of(&outputs), vec!["survivor".to_string()]);
    }

    /// Valid JSON without a `type` member is dropped here, because
    /// `ClaudeCodeOutput` makes `type` mandatory — while
    /// `create_interactive_session`, two functions up, relabels the very same
    /// line `"unknown"` and forwards it. Two parsers, two answers.
    #[tokio::test]
    async fn json_without_a_type_member_is_dropped_on_the_one_shot_path() {
        let cli = FakeCli::emitting(&format!(
            "{}{}",
            line(json!({"note": "no type member"})),
            result_line()
        ));
        let manager = manager(&cli);
        let (_, rx) = manager
            .create_session_with_message(Some("typeless".to_string()), None, None, "hello")
            .await
            .expect("the fake CLI must spawn");

        assert_eq!(types_of(&drain(rx).await), vec!["result"]);
    }

    #[tokio::test]
    async fn a_cli_that_says_nothing_yields_an_empty_closed_channel() {
        let cli = FakeCli::exiting();
        let manager = manager(&cli);
        let (_, rx) = manager
            .create_session_with_message(Some("silent".to_string()), None, None, "hello")
            .await
            .expect("the fake CLI must spawn");
        assert!(drain(rx).await.is_empty());
    }

    /// Noise on stderr is logged and changes nothing the caller can observe —
    /// including when the CLI also failed.
    #[tokio::test]
    async fn stderr_noise_does_not_disturb_the_output_channel() {
        let cli = FakeCli::noisy(&result_line(), &["diagnostic one", "diagnostic two"]);
        let manager = manager(&cli);
        let (_, rx) = manager
            .create_session_with_message(Some("noisy".to_string()), None, None, "hello")
            .await
            .expect("the fake CLI must spawn");
        assert_eq!(types_of(&drain(rx).await), vec!["result"]);
    }

    // ─────────────── create_session_with_message: spawn failure ───────────────

    #[tokio::test]
    async fn a_one_shot_session_fails_when_the_cli_cannot_be_spawned() {
        let manager = unspawnable_manager();
        let error = manager
            .create_session_with_message(Some("no-cli".to_string()), None, None, "hello")
            .await
            .expect_err("spawning a non-existent program must fail");
        assert!(
            error.downcast_ref::<std::io::Error>().is_some(),
            "the spawn error should still be an io::Error: {error:?}"
        );
        assert!(
            manager.get_session_info("no-cli").is_none(),
            "a failed spawn must not leave a registration behind"
        );
    }

    // ───────────────────── create_interactive_session ─────────────────────

    #[tokio::test]
    async fn an_interactive_session_asks_for_streaming_json_without_print() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager(&cli);
        let (id, rx) = manager
            .create_interactive_session(Some("inter-argv".to_string()), None, None)
            .await
            .expect("the fake CLI must spawn");
        assert_eq!(id, "inter-argv");
        drain(rx).await;

        let argv = cli.argv().await;
        assert!(
            flag_with(&argv, "--output-format", "stream-json"),
            "argv: {argv}"
        );
        assert!(has_flag(&argv, "--verbose"), "argv: {argv}");
        assert!(
            !has_flag(&argv, "--print"),
            "`--print` would end the conversation after one turn: {argv}"
        );
        assert!(
            !has_flag(&argv, "--dangerously-skip-permissions"),
            "unlike the one-shot path, this one leaves the prompts on: {argv}"
        );
    }

    /// `create_interactive_session` reads `self.mcp_config` nowhere: an
    /// interactive session launched through `ClaudeManager` runs with no MCP
    /// server, silently, however the gateway is configured. The one-shot path
    /// one function down honours the same field.
    #[tokio::test]
    async fn an_interactive_session_drops_the_mcp_configuration_entirely() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager_with_mcp(
            &cli,
            MCPConfig {
                enabled: true,
                config_file: Some("mcp-servers.json".to_string()),
                config_json: Some(MCP_SENTINEL.to_string()),
                strict: true,
                debug: true,
            },
        );
        let (_, rx) = manager
            .create_interactive_session(Some("inter-mcp".to_string()), None, None)
            .await
            .expect("the fake CLI must spawn");
        drain(rx).await;

        let argv = cli.argv().await;
        for forbidden in [
            "--mcp-config",
            "--strict-mcp-config",
            "--debug",
            "mcp-servers.json",
            MCP_SENTINEL,
        ] {
            assert!(!argv.contains(forbidden), "{forbidden} appeared in {argv}");
        }
    }

    #[tokio::test]
    async fn an_interactive_session_forwards_the_model_and_the_project_path() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager(&cli);
        let (_, rx) = manager
            .create_interactive_session(
                Some("inter-model".to_string()),
                Some("project-root-xyz".to_string()),
                Some("claude-opus-5".to_string()),
            )
            .await
            .expect("the fake CLI must spawn");

        assert_eq!(
            manager.get_session_info("inter-model"),
            Some((
                "inter-model".to_string(),
                Some("project-root-xyz".to_string())
            ))
        );
        drain(rx).await;

        let argv = cli.argv().await;
        assert!(flag_with(&argv, "--model", "claude-opus-5"), "argv: {argv}");
        assert!(
            flag_with(&argv, "--cwd", "project-root-xyz"),
            "argv: {argv}"
        );
    }

    #[tokio::test]
    async fn an_interactive_session_mints_a_uuid_when_given_no_id() {
        let cli = FakeCli::emitting(&result_line());
        let manager = manager(&cli);
        let (id, rx) = manager
            .create_interactive_session(None, None, None)
            .await
            .expect("the fake CLI must spawn");
        assert!(Uuid::parse_str(&id).is_ok(), "got {id:?}");
        drain(rx).await;
    }

    /// The interactive parser deserialises to `serde_json::Value` and then
    /// *invents* a type: `json["type"]` missing becomes `"unknown"`. The whole
    /// line, `type` member included, is also kept in `data` — which the one-shot
    /// path strips. Both facts are pinned here because the two channels feed the
    /// same `ClaudeCodeOutput` consumers.
    #[tokio::test]
    async fn the_interactive_parser_relabels_a_typeless_line_unknown() {
        let cli = FakeCli::emitting(&format!(
            "{}{}",
            line(json!({"note": "no type member"})),
            line(json!({"type": "result", "subtype": "success"}))
        ));
        let manager = manager(&cli);
        let (_, rx) = manager
            .create_interactive_session(Some("inter-typeless".to_string()), None, None)
            .await
            .expect("the fake CLI must spawn");

        let outputs = drain(rx).await;
        assert_eq!(types_of(&outputs), vec!["unknown", "result"]);
        assert_eq!(outputs[0].subtype, None);
        assert_eq!(
            outputs[0].data,
            json!({"note": "no type member"}),
            "the whole line is kept in `data`"
        );
        assert_eq!(outputs[1].subtype.as_deref(), Some("success"));
        assert_eq!(
            outputs[1].data["type"], "result",
            "unlike the one-shot path, `type` stays inside `data` too"
        );
    }

    /// A JSON line that is not an object at all: `Value::get("type")` answers
    /// `None` on a scalar, so the line is forwarded as `"unknown"` rather than
    /// rejected.
    #[tokio::test]
    async fn the_interactive_parser_forwards_a_bare_scalar_as_unknown() {
        let cli = FakeCli::emitting("42\n");
        let manager = manager(&cli);
        let (_, rx) = manager
            .create_interactive_session(Some("inter-scalar".to_string()), None, None)
            .await
            .expect("the fake CLI must spawn");

        let outputs = drain(rx).await;
        assert_eq!(types_of(&outputs), vec!["unknown"]);
        assert_eq!(outputs[0].data, json!(42));
    }

    #[tokio::test]
    async fn the_interactive_parser_skips_blank_and_unparseable_lines() {
        let cli = FakeCli::emitting(&format!(
            "\n  \nnot json at all\n{}",
            line(json!({"type": "result", "subtype": "success"}))
        ));
        let manager = manager(&cli);
        let (_, rx) = manager
            .create_interactive_session(Some("inter-garbage".to_string()), None, None)
            .await
            .expect("the fake CLI must spawn");

        assert_eq!(types_of(&drain(rx).await), vec!["result"]);
    }

    /// The answer to "does the interactive channel ever close?": it closes
    /// exactly when the CLI's stdout reaches EOF, i.e. when the CLI exits.
    ///
    /// Nothing else can close it, and that is the trap: `create_interactive_session`
    /// pipes stdin and then *keeps* the write end inside
    /// `ClaudeManager.processes`, so a real `claude` in interactive mode never
    /// sees EOF and never exits. A test that awaits the close of such a channel
    /// hangs for ever. The expectation is legitimate — an interactive session is
    /// meant to outlive a turn — but the channel carries no end-of-turn marker
    /// of its own, so every consumer needs its own deadline.
    #[tokio::test]
    async fn an_interactive_channel_stays_open_while_the_cli_lives() {
        let cli = FakeCli::recording_stdin("");
        let manager = manager(&cli);
        let (id, mut rx) = manager
            .create_interactive_session(Some("inter-alive".to_string()), None, None)
            .await
            .expect("the fake CLI must spawn");

        // The CLI is blocked reading a stdin the manager holds open.
        assert!(
            tokio::time::timeout(Duration::from_millis(500), rx.recv())
                .await
                .is_err(),
            "the channel must not close while the CLI is alive"
        );

        // Closing the session is what releases it.
        manager.close_session(&id).await.expect("close the session");
        assert!(
            matches!(tokio::time::timeout(PATIENCE, rx.recv()).await, Ok(None)),
            "killing the CLI must close the channel"
        );
    }

    #[tokio::test]
    async fn an_interactive_session_fails_when_the_cli_cannot_be_spawned() {
        let manager = unspawnable_manager();
        let error = manager
            .create_interactive_session(Some("inter-no-cli".to_string()), None, None)
            .await
            .expect_err("spawning a non-existent program must fail");
        assert!(
            error.downcast_ref::<std::io::Error>().is_some(),
            "{error:?}"
        );
        assert!(manager.get_session_info("inter-no-cli").is_none());
    }

    // ──────────────── an abandoned receiver stops the pump ────────────────

    /// A caller that drops its receiver — an HTTP client that disconnects
    /// mid-answer — must stop the reader task instead of letting it pump into
    /// the void.
    ///
    /// `#[tokio::test]` runs on a current-thread runtime, so `drop(rx)` below
    /// happens before the reader task has had a single chance to poll: the very
    /// first `tx_clone.send(...)` fails and the loop breaks. The transcript is
    /// then larger than any pipe buffer, so the CLI is still writing when the
    /// pump drops stdout — it is cut off and never reaches its marker. The
    /// control test underneath proves the marker really does appear otherwise.
    #[tokio::test]
    async fn dropping_the_receiver_cuts_the_one_shot_pump_short() {
        let cli = FakeCli::emitting_more_than_fits(&transcript_larger_than_a_pipe_buffer());
        let manager = manager(&cli);
        let (id, rx) = manager
            .create_session_with_message(Some("abandoned".to_string()), None, None, "hello")
            .await
            .expect("the fake CLI must spawn");

        drop(rx);
        await_child_exit(&manager, &id).await;

        assert!(
            !cli.reached_the_end_of_its_transcript(),
            "the CLI should have been cut off, not drained to the end"
        );
        assert!(
            manager.get_session_info(&id).is_some(),
            "and nothing deregisters it: the entry survives its own consumer,              waiting for a `close_session` that only `api::chat` makes"
        );
    }

    /// The control: with a receiver that keeps reading, the same CLI does reach
    /// the end of the same transcript.
    #[tokio::test]
    async fn a_drained_receiver_lets_the_cli_finish_its_transcript() {
        let cli = FakeCli::emitting_more_than_fits(&transcript_larger_than_a_pipe_buffer());
        let manager = manager(&cli);
        let (_, rx) = manager
            .create_session_with_message(Some("drained".to_string()), None, None, "hello")
            .await
            .expect("the fake CLI must spawn");

        assert!(
            drain(rx).await.len() > 1000,
            "the whole transcript must come through"
        );
        assert!(
            poll_file(&cli.marker, |s| s.contains("done")).await.is_ok(),
            "so the CLI reached the end of its transcript"
        );
    }

    /// Same on the interactive constructor, whose pump is a separate copy of the
    /// same loop.
    #[tokio::test]
    async fn dropping_the_receiver_cuts_the_interactive_pump_short() {
        let cli = FakeCli::emitting_more_than_fits(&transcript_larger_than_a_pipe_buffer());
        let manager = manager(&cli);
        let (id, rx) = manager
            .create_interactive_session(Some("inter-abandoned".to_string()), None, None)
            .await
            .expect("the fake CLI must spawn");

        drop(rx);
        await_child_exit(&manager, &id).await;

        assert!(
            !cli.reached_the_end_of_its_transcript(),
            "the interactive pump must break too"
        );
    }

    // ───────────────────────────── send_message ─────────────────────────────

    #[tokio::test]
    async fn send_message_to_an_unknown_session_is_an_error() {
        let manager = unspawnable_manager();
        let error = manager
            .send_message("never-created", "hello")
            .await
            .expect_err("an unknown session must be refused");
        assert_eq!(error.to_string(), "Session not found");
    }

    #[tokio::test]
    async fn send_message_writes_a_line_and_puts_stdin_back() {
        let cli = FakeCli::recording_stdin("");
        let manager = manager(&cli);
        let (id, _rx) = manager
            .create_interactive_session(Some("send".to_string()), None, None)
            .await
            .expect("the fake CLI must spawn");

        assert!(
            stdin_is_present(&manager, &id),
            "an interactive session keeps its stdin, that is the point"
        );
        manager
            .send_message(&id, "premier")
            .await
            .expect("first send");
        assert!(
            stdin_is_present(&manager, &id),
            "stdin must be handed back, or no second message can ever be sent"
        );
        manager
            .send_message(&id, "second")
            .await
            .expect("second send");

        // Release the pipe so the fake CLI flushes what it read.
        take_stdin(&manager, &id);
        let read = cli.stdin_containing("second").await;
        assert_eq!(
            read.lines().collect::<Vec<_>>(),
            vec!["premier", "second"],
            "each message must arrive as its own newline-terminated line"
        );
    }

    /// A session whose child is gone keeps `send_message` on its happy path:
    /// the call logs and answers `Ok(())`, so a caller cannot tell a delivered
    /// message from a discarded one.
    #[tokio::test]
    async fn send_message_reports_success_when_there_is_no_stdin_at_all() {
        let manager = unspawnable_manager();
        manager.processes.write().insert(
            "childless".to_string(),
            ClaudeProcess {
                id: "childless".to_string(),
                child: None,
                project_path: None,
            },
        );

        manager
            .send_message("childless", "into the void")
            .await
            .expect("today this is reported as a success");
        assert!(
            manager.get_session_info("childless").is_some(),
            "the session is left registered, still unable to receive anything"
        );
    }

    /// Writing to the stdin of a CLI that has already exited fails, and the
    /// error is propagated — but the `?` returns before stdin is put back, so
    /// the session silently degrades to the "no stdin" case above: the next
    /// `send_message` answers `Ok(())` while delivering nothing.
    #[tokio::test]
    async fn a_write_failure_is_propagated_and_then_loses_stdin_for_good() {
        let cli = FakeCli::exiting();
        let manager = manager(&cli);
        let (id, _rx) = manager
            .create_interactive_session(Some("dead".to_string()), None, None)
            .await
            .expect("the fake CLI must spawn");

        await_child_exit(&manager, &id).await;

        let error = manager
            .send_message(&id, "too late")
            .await
            .expect_err("writing to a closed pipe must fail");
        let io = error
            .downcast_ref::<std::io::Error>()
            .expect("the write error should surface as an io::Error");
        assert_eq!(
            io.kind(),
            std::io::ErrorKind::BrokenPipe,
            "got {io:?} instead of a broken pipe"
        );

        assert!(
            !stdin_is_present(&manager, &id),
            "the failed write swallowed the stdin handle"
        );
        manager
            .send_message(&id, "and again")
            .await
            .expect("so the next call now reports success while sending nothing");
    }

    fn stdin_is_present(manager: &ClaudeManager, session_id: &str) -> bool {
        manager
            .processes
            .read()
            .get(session_id)
            .and_then(|p| p.child.as_ref())
            .is_some_and(|c| c.stdin.is_some())
    }

    fn take_stdin(manager: &ClaudeManager, session_id: &str) {
        let mut processes = manager.processes.write();
        let child = processes
            .get_mut(session_id)
            .and_then(|p| p.child.as_mut())
            .expect("the session must still hold its child");
        drop(child.stdin.take());
    }

    /// Waits until the registered child has exited **and been reaped**, so the
    /// read end of its stdin pipe is certainly closed.
    async fn await_child_exit(manager: &ClaudeManager, session_id: &str) {
        for _ in 0..500 {
            {
                let mut processes = manager.processes.write();
                let child = processes
                    .get_mut(session_id)
                    .and_then(|p| p.child.as_mut())
                    .expect("the session must still hold its child");
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the fake CLI never exited");
    }

    // ──────────────────────────── close_session ────────────────────────────

    #[tokio::test]
    async fn close_session_kills_the_cli_and_deregisters_it() {
        // The fake only prints its transcript *after* its stdin closes, so the
        // transcript is a witness: a killed process never gets to say it.
        let cli = FakeCli::recording_stdin(&assistant_line("never-printed"));
        let manager = manager(&cli);
        let (id, rx) = manager
            .create_interactive_session(Some("to-close".to_string()), None, None)
            .await
            .expect("the fake CLI must spawn");

        manager.close_session(&id).await.expect("close once");
        assert!(
            manager.get_session_info(&id).is_none(),
            "a closed session must be forgotten"
        );
        assert!(
            drain(rx).await.is_empty(),
            "`close_session` kills: the CLI must not have reached its transcript"
        );
    }

    /// `close_session` answers `Ok(())` for an id it has never heard of.
    ///
    /// That is the whole of the interactive gap: a session created by
    /// `InteractiveSessionManager` lives in *its* map, so when
    /// `handle_non_streaming_response` times out and calls
    /// `claude_manager.close_session(&session_id)` the call removes nothing,
    /// kills nothing and still reports success. The caller has no way to learn
    /// that the process it meant to stop is still running.
    #[tokio::test]
    async fn close_session_silently_succeeds_on_a_session_it_does_not_own() {
        let manager = unspawnable_manager();
        manager
            .close_session("owned-by-the-interactive-manager")
            .await
            .expect("today an unknown session is a silent success");
        assert!(manager.processes.read().is_empty());
    }

    #[tokio::test]
    async fn closing_twice_is_idempotent() {
        let cli = FakeCli::recording_stdin("");
        let manager = manager(&cli);
        let (id, _rx) = manager
            .create_interactive_session(Some("twice".to_string()), None, None)
            .await
            .expect("the fake CLI must spawn");
        manager.close_session(&id).await.expect("first close");
        manager.close_session(&id).await.expect("second close");
        assert!(manager.get_session_info(&id).is_none());
    }

    #[tokio::test]
    async fn close_session_deregisters_a_process_that_has_no_child() {
        let manager = unspawnable_manager();
        manager.processes.write().insert(
            "childless".to_string(),
            ClaudeProcess {
                id: "childless".to_string(),
                child: None,
                project_path: Some("project-root-xyz".to_string()),
            },
        );
        manager.close_session("childless").await.expect("close");
        assert!(
            manager.get_session_info("childless").is_none(),
            "the registration must go even when there is nothing to kill"
        );
    }

    // ────────────────────────────── cleanup ──────────────────────────────

    #[tokio::test]
    async fn cleanup_kills_every_session_and_empties_the_map() {
        let cli = FakeCli::recording_stdin("");
        let manager = manager(&cli);
        let (first, mut rx_first) = manager
            .create_interactive_session(Some("one".to_string()), None, None)
            .await
            .expect("spawn one");
        let (second, mut rx_second) = manager
            .create_interactive_session(Some("two".to_string()), None, None)
            .await
            .expect("spawn two");
        // A registration with nothing to kill must not stop the sweep.
        manager.processes.write().insert(
            "three".to_string(),
            ClaudeProcess {
                id: "three".to_string(),
                child: None,
                project_path: None,
            },
        );
        assert_eq!(manager.processes.read().len(), 3);

        manager.cleanup().await;

        assert!(
            manager.processes.read().is_empty(),
            "the map must be drained"
        );
        assert!(manager.get_session_info(&first).is_none());
        assert!(manager.get_session_info(&second).is_none());
        for rx in [&mut rx_first, &mut rx_second] {
            assert!(
                matches!(tokio::time::timeout(PATIENCE, rx.recv()).await, Ok(None)),
                "every killed process must close its channel"
            );
        }
    }

    #[tokio::test]
    async fn cleanup_on_an_idle_manager_does_nothing() {
        let manager = unspawnable_manager();
        manager.cleanup().await;
        assert!(manager.processes.read().is_empty());
    }

    // ──────────────────────────────── Drop ────────────────────────────────

    /// `Drop` notices the leak and does nothing about it: killing a child is
    /// `async`, so the impl only logs, and `tokio::process::Child` does not kill
    /// on drop unless `kill_on_drop` was set — which it is not here. The CLI is
    /// released, never stopped, and only the closing of its stdin pipe (a side
    /// effect of dropping the `Child`, not a decision) ends it. `cleanup()`
    /// exists for this and is called from nowhere in the crate.
    ///
    /// Contrast `close_session_kills_the_cli_and_deregisters_it`: there the same
    /// transcript never arrives, because that path really does kill.
    ///
    /// The warning line itself is asserted in
    /// `tests/claude_manager_logging.rs`, where a global subscriber can capture
    /// it.
    #[tokio::test]
    async fn dropping_a_manager_releases_its_cli_instead_of_killing_it() {
        let cli = FakeCli::recording_stdin(&assistant_line("spoke-after-being-abandoned"));
        let manager = manager(&cli);
        let (id, rx) = manager
            .create_interactive_session(Some("leaked".to_string()), None, None)
            .await
            .expect("the fake CLI must spawn");
        assert!(manager.get_session_info(&id).is_some());

        drop(manager);

        assert_eq!(
            texts_of(&drain(rx).await),
            vec!["spoke-after-being-abandoned".to_string()],
            "`Drop` warned and walked away: the CLI ran on to its transcript"
        );
    }
}
