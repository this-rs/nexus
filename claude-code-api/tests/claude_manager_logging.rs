//! What `ClaudeManager` actually writes to the log — bug S2 and its neighbours.
//!
//! # Why this is a file of its own
//!
//! A thread-local `tracing` subscriber cannot reliably capture a log line in a
//! multi-threaded test binary: `tracing` caches each callsite's *interest* for
//! the whole process, so a callsite first reached while no global subscriber
//! exists is cached as "never" and stops evaluating its arguments — which is
//! exactly where the redaction lives. `rebuild_interest_cache()` does not undo
//! that reliably either. The cure is a **global** subscriber installed as the
//! first statement of the test, and one test binary per file means one process
//! per file, so nothing else in the suite can poison the cache first.
//!
//! Every test here is `#[serial]` and clears the sink on entry, which is what
//! lets the assertions count occurrences instead of merely looking for a
//! substring.
//!
//! # What is pinned
//!
//! `ClaudeManager::create_session_with_message` builds `--mcp-config <json>` and
//! then logs the command. `Debug for Command` prints every argument and every
//! environment value, so the MCP configuration — which carries each server's
//! `env` and headers, in practice a database password and a session token — used
//! to be written verbatim at `info`, the service's default level. The repair
//! routes both spawn sites through `nexus_claude::describe_command_redacted`.
//! `tests/command_redaction.rs` tests that helper in isolation; this file tests
//! the *call sites in `core/claude_manager.rs`*, which is the part a no-op
//! helper or a forgotten site would get wrong.

// Only `fake_exec` is needed here, not the whole harness: `mod support;` would
// pull in the axum/wiremock scaffolding this file never touches.
#[path = "support/fake_exec.rs"]
mod fake_exec;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use claude_code_api::core::claude_manager::ClaudeManager;
use claude_code_api::core::config::{FileAccessConfig, MCPConfig};
use claude_code_api::models::claude::ClaudeCodeOutput;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::mpsc;

/// An MCP configuration of exactly the documented shape, carrying a value no
/// description would print by accident.
const SECRET: &str = "pg-password-cEaa41f6Qv-and-token-sk-ant-9z8y7x";

fn mcp_payload() -> String {
    format!(
        r#"{{"mcpServers":{{"orchestrator":{{"command":"po","env":{{"DATABASE_URL":"{SECRET}"}}}}}}}}"#
    )
}

const PATIENCE: Duration = Duration::from_secs(10);

// ───────────────────────────────── the sink ─────────────────────────────────

#[derive(Clone, Default)]
struct LogSink(Arc<std::sync::Mutex<Vec<u8>>>);

impl LogSink {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log sink mutex")).into_owned()
    }

    fn clear(&self) {
        self.0.lock().expect("log sink mutex").clear();
    }

    /// Polls the sink until `needle` shows up, or fails with everything seen.
    ///
    /// The reader tasks log from other tasks, so a line can land after the
    /// call that triggered it has returned.
    async fn wait_for(&self, needle: &str) -> String {
        for _ in 0..500 {
            let seen = self.contents();
            if seen.contains(needle) {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "{needle:?} never reached the log; what did:\n{}",
            self.contents()
        );
    }

    fn lines_with(&self, needle: &str) -> Vec<String> {
        self.contents()
            .lines()
            .filter(|l| l.contains(needle))
            .map(|l| l.to_string())
            .collect()
    }
}

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log sink mutex")
            .extend_from_slice(buf);
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

/// The process-wide sink, with the subscriber installed on first use.
///
/// Call this as the **first statement** of every test: it is what makes the
/// `info!` arguments — and therefore `describe_command_redacted` — run at all.
fn logs() -> &'static LogSink {
    static SINK: std::sync::OnceLock<LogSink> = std::sync::OnceLock::new();
    let sink = SINK.get_or_init(|| {
        let sink = LogSink::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_writer(sink.clone())
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .finish(),
        )
        .expect("this binary installs the only global subscriber");
        tracing::callsite::rebuild_interest_cache();
        sink
    });
    assert!(
        tracing::level_filters::LevelFilter::current() >= tracing::Level::DEBUG,
        "without a live level the logging statements under test are skipped entirely"
    );
    sink.clear();
    sink
}

// ─────────────────────────────── the fake CLI ───────────────────────────────

/// A scripted stand-in for the `claude` binary.
///
/// `ClaudeManager` calls `Command::new(&self.claude_command)` directly, so the
/// command string is the only seam. No network, no real CLI.
struct FakeCli {
    _dir: TempDir,
    script: PathBuf,
}

impl FakeCli {
    fn build(stdout: &str, stderr: &[&str], stdin_first: bool, keep_alive: bool) -> Self {
        let dir = tempfile::tempdir().expect("tempdir for the fake CLI");
        let payload = dir.path().join("payload.ndjson");
        std::fs::write(&payload, stdout).expect("write the fake CLI payload");
        let body = if cfg!(windows) {
            let mut body = String::from("@echo off\r\n");
            if stdin_first {
                // `sort` reads stdin to EOF; the transcript below is printed
                // only once the gateway lets go of the write end.
                body.push_str("sort >nul 2>nul\r\n");
            }
            body.push_str(&format!("type \"{}\"\r\n", payload.display()));
            for line in stderr {
                if line.is_empty() {
                    body.push_str("echo. 1>&2\r\n");
                } else {
                    body.push_str(&format!("echo {line} 1>&2\r\n"));
                }
            }
            if keep_alive {
                // `sort` reads stdin to EOF; while the manager holds the write
                // end open the process stays alive.
                body.push_str("sort >nul 2>nul\r\n");
            }
            body.push_str("exit /b 0\r\n");
            body
        } else {
            let mut body = String::from("#!/bin/sh\n");
            if stdin_first {
                body.push_str("cat > /dev/null\n");
            }
            body.push_str(&format!("cat '{}'\n", payload.display()));
            for line in stderr {
                body.push_str(&format!("echo '{line}' 1>&2\n"));
            }
            if keep_alive {
                body.push_str("cat > /dev/null\n");
            }
            body.push_str("exit 0\n");
            body
        };

        let script = fake_exec::plant_fake_cli(dir.path(), &body);

        Self { _dir: dir, script }
    }

    fn emitting(stdout: &str) -> Self {
        Self::build(stdout, &[], false, false)
    }

    fn noisy(stdout: &str, stderr: &[&str]) -> Self {
        Self::build(stdout, stderr, false, false)
    }

    /// Stays alive until its stdin is closed — an interactive CLI's behaviour.
    fn blocking() -> Self {
        Self::build("", &[], false, true)
    }

    /// Waits for its stdin to close, **then** prints `stdout`.
    ///
    /// The transcript is a witness: it can only reach the output channel if the
    /// process was released rather than killed.
    fn blocking_then_emitting(stdout: &str) -> Self {
        Self::build(stdout, &[], true, false)
    }

    fn command(&self) -> String {
        self.script.to_string_lossy().into_owned()
    }
}

fn manager(cli: &FakeCli, mcp: MCPConfig) -> ClaudeManager {
    ClaudeManager::new(cli.command(), FileAccessConfig::default(), mcp)
}

fn mcp_inline(config_json: &str) -> MCPConfig {
    MCPConfig {
        enabled: true,
        config_file: None,
        config_json: Some(config_json.to_string()),
        strict: true,
        debug: false,
    }
}

fn result_line() -> String {
    format!("{}\n", json!({"type": "result", "subtype": "success"}))
}

fn assistant_line(text: &str) -> String {
    format!(
        "{}\n",
        json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}
        })
    )
}

/// Drains `rx` to its close, failing rather than hanging.
async fn drain(mut rx: mpsc::Receiver<ClaudeCodeOutput>) -> usize {
    let mut seen = 0;
    loop {
        match tokio::time::timeout(PATIENCE, rx.recv()).await {
            Ok(Some(_)) => seen += 1,
            Ok(None) => return seen,
            Err(_) => panic!("the output channel never closed ({seen} message(s) seen)"),
        }
    }
}

// ──────────────────────────── S2: the spawn lines ────────────────────────────

/// The regression test for S2 at its real call site.
///
/// Replay it against the previous code — `info!("… with command: {:?}", cmd)` —
/// and it fails on the first assertion: `Debug for Command` prints every
/// argument, the MCP JSON included.
#[tokio::test]
#[serial_test::serial]
async fn the_inline_mcp_configuration_never_reaches_the_log() {
    let logs = logs();
    let payload = mcp_payload();
    let cli = FakeCli::emitting(&result_line());
    let manager = manager(&cli, mcp_inline(&payload));

    let (_, rx) = manager
        .create_session_with_message(Some("log-oneshot".to_string()), None, None, "bonjour")
        .await
        .expect("the fake CLI must spawn");
    drain(rx).await;

    let seen = logs
        .wait_for("Starting Claude process for session log-oneshot")
        .await;
    assert!(
        !seen.contains(SECRET),
        "the MCP configuration reached the log:\n{seen}"
    );
    assert!(
        seen.contains(&format!("<redacted {} bytes>", payload.len())),
        "the value should be reported as redacted with its size, not dropped \
         in silence:\n{seen}"
    );
    // What a person debugging a launch still needs survives.
    for kept in [
        "--mcp-config",
        "--strict-mcp-config",
        "--print",
        "stream-json",
    ] {
        assert!(
            seen.contains(kept),
            "{kept} must survive redaction:\n{seen}"
        );
    }
}

/// The control that keeps the test above from being vacuous: the sentinel is
/// something `Debug` really does print, so its absence above means something.
#[test]
fn debug_formatting_is_what_leaked_and_still_would() {
    let mut cmd = std::process::Command::new("claude");
    cmd.arg("--mcp-config").arg(mcp_payload());
    assert!(
        format!("{cmd:?}").contains(SECRET),
        "if `Debug` no longer printed arguments, the assertions above would pass \
         for the wrong reason"
    );
}

/// The interactive constructor logs the same way. It passes no MCP argument at
/// all today, so there is no secret to hide here — the assertion is that the
/// line is produced by the redacting description, not by `Debug`, so the site stays
/// correct if it ever starts forwarding the configuration.
#[tokio::test]
#[serial_test::serial]
async fn the_interactive_spawn_line_uses_the_redacted_description_too() {
    let logs = logs();
    let cli = FakeCli::emitting(&result_line());
    let manager = manager(&cli, mcp_inline(&mcp_payload()));

    let (_, rx) = manager
        .create_interactive_session(
            Some("log-inter".to_string()),
            None,
            Some("opus".to_string()),
        )
        .await
        .expect("the fake CLI must spawn");
    drain(rx).await;

    let seen = logs
        .wait_for("Starting interactive Claude session log-inter")
        .await;
    assert!(!seen.contains(SECRET), "{seen}");
    let line = logs
        .lines_with("Starting interactive Claude session log-inter")
        .pop()
        .expect("the line was just waited for");
    for shape in ["program=", "cwd=", "args=", "env_keys="] {
        assert!(
            line.contains(shape),
            "{shape} is part of the redacted description; a raw `{{:?}}` would \
             not have it: {line}"
        );
    }
    assert!(
        line.contains("--model") && line.contains("opus"),
        "the model is not a secret and belongs in the line: {line}"
    );
}

// ───────────────────── the one-shot monitoring statements ─────────────────────

#[tokio::test]
#[serial_test::serial]
async fn the_one_shot_path_logs_the_real_pid_of_the_process_it_started() {
    let logs = logs();
    let cli = FakeCli::emitting(&result_line());
    let manager = manager(&cli, MCPConfig::default());

    let (_, rx) = manager
        .create_session_with_message(Some("log-pid".to_string()), None, None, "bonjour")
        .await
        .expect("the fake CLI must spawn");
    drain(rx).await;

    logs.wait_for("for session log-pid").await;
    let line = logs
        .lines_with("Monitoring Claude process")
        .pop()
        .expect("the spawn must be announced with its pid");
    let pid: u32 = line
        .split("Monitoring Claude process ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no pid in {line:?}"));
    assert_ne!(
        pid, 0,
        "`child.id().unwrap_or(0)` logs 0 for a process it could not identify; \
         a freshly spawned child always has an id: {line}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn every_cli_line_is_logged_and_the_end_of_each_stream_announced() {
    let logs = logs();
    let cli = FakeCli::emitting(&format!("{}{}", assistant_line("Bonjour"), result_line()));
    let manager = manager(&cli, MCPConfig::default());

    let (_, rx) = manager
        .create_session_with_message(Some("log-stream".to_string()), None, None, "bonjour")
        .await
        .expect("the fake CLI must spawn");
    assert_eq!(drain(rx).await, 2);

    let seen = logs.wait_for("Claude output stream ended").await;
    let raw = logs.lines_with("Claude output line:");
    assert_eq!(raw.len(), 2, "every non-blank line is logged raw: {raw:?}");
    assert!(
        raw[0].contains(r#""text":"Bonjour""#),
        "the raw line is logged verbatim before parsing, assistant text \
         included: {raw:?}"
    );
    assert!(
        seen.contains("Parsed Claude output: type=assistant, subtype=None"),
        "and again after parsing, with the fields the handlers switch on:\n{seen}"
    );
    assert!(
        seen.contains(r#"Parsed Claude output: type=result, subtype=Some("success")"#),
        "{seen}"
    );
    logs.wait_for("Claude stderr stream ended").await;
}

#[tokio::test]
#[serial_test::serial]
async fn a_line_the_parser_rejects_is_logged_with_its_text() {
    let logs = logs();
    let cli = FakeCli::emitting(&format!("not json at all\n{}", result_line()));
    let manager = manager(&cli, MCPConfig::default());

    let (_, rx) = manager
        .create_session_with_message(Some("log-garbage".to_string()), None, None, "bonjour")
        .await
        .expect("the fake CLI must spawn");
    assert_eq!(drain(rx).await, 1, "the bad line is dropped, not forwarded");

    let seen = logs.wait_for("Failed to parse Claude output").await;
    assert!(
        seen.contains("Line: not json at all"),
        "the offending text must be in the log, or the operator cannot tell \
         what the CLI said:\n{seen}"
    );
}

// ──────────────────────────── stderr: two policies ────────────────────────────

/// The one-shot path logs **every** stderr line at `error!`, blank lines
/// included and whatever the exit status — so a CLI that merely chats on stderr
/// fills the error log of a successful request.
#[tokio::test]
#[serial_test::serial]
async fn the_one_shot_path_logs_every_stderr_line_as_an_error_blank_ones_too() {
    let logs = logs();
    let cli = FakeCli::noisy(&result_line(), &["", "harmless-progress-note"]);
    let manager = manager(&cli, MCPConfig::default());

    let (_, rx) = manager
        .create_session_with_message(Some("log-stderr-one".to_string()), None, None, "bonjour")
        .await
        .expect("the fake CLI must spawn");
    drain(rx).await;

    // The "stream ended" line is written after the loop, so once it is there
    // every stderr line has been through the log.
    logs.wait_for("Claude stderr stream ended").await;
    let lines = logs.lines_with("Claude stderr:");
    assert_eq!(
        lines.len(),
        2,
        "the blank line is logged too, giving two entries, not one: {lines:?}"
    );
    assert!(
        lines.iter().all(|l| l.contains("ERROR")),
        "the CLI's chatter is recorded at ERROR level even though the call \
         succeeded: {lines:?}"
    );
    assert!(lines[1].contains("harmless-progress-note"), "{lines:?}");
}

/// The interactive path applies the opposite policy to the same stream: blank
/// lines are skipped and the rest is `warn!`, not `error!`. Two constructors in
/// one file, two answers.
#[tokio::test]
#[serial_test::serial]
async fn the_interactive_path_skips_blank_stderr_lines_and_only_warns() {
    let logs = logs();
    let cli = FakeCli::noisy(&result_line(), &["", "interactive-progress-note"]);
    let manager = manager(&cli, MCPConfig::default());

    let (_, rx) = manager
        .create_interactive_session(Some("log-stderr-inter".to_string()), None, None)
        .await
        .expect("the fake CLI must spawn");
    drain(rx).await;

    // The blank line is written by the CLI *before* the note, so once the note
    // is in the log the blank one has had its chance.
    logs.wait_for("interactive-progress-note").await;
    let lines = logs.lines_with("Claude stderr:");
    assert_eq!(
        lines.len(),
        1,
        "the blank line must have been dropped here: {lines:?}"
    );
    assert!(
        lines[0].contains("WARN"),
        "the same text is a warning on this path and an error on the other: {lines:?}"
    );
    assert!(
        !logs.contents().contains("Claude stderr stream ended"),
        "and the end of the stream is not announced at all on this path"
    );
}

// ───────────────────── a prompt that never reaches the CLI ─────────────────────

/// The prompt is written to stdin by a **detached** task, so a failed write is
/// logged and nothing else: `create_session_with_message` has already returned
/// `Ok`, and the caller sees a CLI that simply had nothing to say. A request
/// whose prompt never arrived is indistinguishable from one the CLI chose not to
/// answer.
///
/// The setup is deterministic rather than lucky: four megabytes cannot sit in a
/// pipe buffer on any platform, so the write must wait for a reader, and this
/// CLI exits without ever reading.
#[tokio::test]
#[serial_test::serial]
async fn a_prompt_that_cannot_be_written_is_logged_and_then_forgotten() {
    let logs = logs();
    let cli = FakeCli::emitting("");
    let manager = manager(&cli, MCPConfig::default());

    let prompt = "x".repeat(4 * 1024 * 1024);
    let (_, rx) = manager
        .create_session_with_message(Some("log-stdin-fail".to_string()), None, None, &prompt)
        .await
        .expect("the spawn itself succeeds");

    assert_eq!(
        drain(rx).await,
        0,
        "the caller gets an empty, cleanly closed channel — no error at all"
    );

    let seen = logs.wait_for("Failed to write to stdin").await;
    assert!(
        seen.contains("Broken pipe") || seen.contains("os error"),
        "the underlying io error should be named:\n{seen}"
    );
}

// ──────────────────────── send_message and close_session ────────────────────────

/// `send_message` logs the size of what it writes and never the text. The
/// prompt is the user's content; the byte count is what an operator needs.
#[tokio::test]
#[serial_test::serial]
async fn send_message_logs_a_byte_count_and_not_the_message() {
    let logs = logs();
    let cli = FakeCli::blocking();
    let manager = manager(&cli, MCPConfig::default());
    let (id, _rx) = manager
        .create_interactive_session(Some("log-send".to_string()), None, None)
        .await
        .expect("the fake CLI must spawn");

    manager
        .send_message(&id, "mon-secret-de-fabrication")
        .await
        .expect("the write must succeed on a live CLI");

    let seen = logs.wait_for("Message sent successfully").await;
    assert!(
        seen.contains("Writing message to stdin: 25 bytes"),
        "the size is logged:\n{seen}"
    );
    assert!(
        !seen.contains("mon-secret-de-fabrication"),
        "the prompt itself must not be:\n{seen}"
    );

    manager.close_session(&id).await.expect("close the session");
    logs.wait_for(&format!("Closed session {id}")).await;
}

// ────────────────────────────────── Drop ──────────────────────────────────

/// `Drop` names every process still registered — and does nothing else.
///
/// Killing a child is `async`, so the impl cannot do it from `drop`. It only
/// logs, and a `tokio::process::Child` without `kill_on_drop` is not killed when
/// it is dropped either: the CLI is *released*, not stopped. The only thing that
/// ends it is the closing of its stdin pipe, which happens as a side effect of
/// the `Child` being dropped — so a CLI that does not read stdin, or that
/// survives EOF, keeps running after the gateway has forgotten it. `cleanup()`
/// exists for precisely this and is called from nowhere in the crate, which is
/// why the line below says "Warning" and means "leaked".
#[tokio::test]
#[serial_test::serial]
async fn dropping_a_manager_names_every_process_it_is_abandoning() {
    let logs = logs();
    let cli = FakeCli::blocking_then_emitting(&assistant_line("spoke-after-being-abandoned"));
    let manager = manager(&cli, MCPConfig::default());
    let (_id, mut rx) = manager
        .create_interactive_session(Some("log-leaked".to_string()), None, None)
        .await
        .expect("the fake CLI must spawn");

    // Nothing yet: the CLI is blocked on a stdin the manager holds open.
    assert!(
        tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .is_err(),
        "the CLI must still be waiting when the manager goes away"
    );

    drop(manager);

    let seen = logs
        .wait_for("Claude process log-leaked still running at shutdown")
        .await;
    assert!(
        logs.lines_with("still running at shutdown")
            .iter()
            .all(|l| l.contains("ERROR")),
        "the warning is emitted at ERROR level:\n{seen}"
    );

    // And the warning is all that happens: no `kill`, so the abandoned CLI runs
    // on and its output still arrives. Only the closing of the stdin pipe — a
    // side effect of dropping the `Child`, not a decision — ever ends it.
    let witness = tokio::time::timeout(PATIENCE, rx.recv())
        .await
        .expect("the abandoned CLI must still be able to speak")
        .expect("and its line must reach the channel");
    assert_eq!(
        witness.data.pointer("/message/content/0/text"),
        Some(&json!("spoke-after-being-abandoned")),
        "a killed process could not have printed this"
    );
}

/// A manager that was cleaned up says nothing on the way out.
#[tokio::test]
#[serial_test::serial]
async fn a_cleaned_up_manager_drops_in_silence() {
    let logs = logs();
    let cli = FakeCli::blocking();
    let manager = manager(&cli, MCPConfig::default());
    manager
        .create_interactive_session(Some("log-cleaned".to_string()), None, None)
        .await
        .expect("the fake CLI must spawn");

    manager.cleanup().await;
    drop(manager);

    assert!(
        !logs.contents().contains("still running at shutdown"),
        "`cleanup()` is the supported way out, and it leaves nothing to warn \
         about:\n{}",
        logs.contents()
    );
}
