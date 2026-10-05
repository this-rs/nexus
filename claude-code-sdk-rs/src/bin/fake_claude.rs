//! `fake_claude` — a dependency-free stand-in for the real `claude` CLI.
//!
//! `SubprocessTransport` can only be pointed at a different executable
//! (`SubprocessTransport::with_cli_path` / `ClaudeCodeOptions::cli_path`), so the
//! only portable way to test it without a real CLI and without network is to
//! spawn a *real executable* that speaks the same stream-json protocol. Cargo
//! auto-discovers `src/bin/*.rs` and hands integration tests the absolute path
//! through `env!("CARGO_BIN_EXE_fake_claude")`, which works identically on
//! Linux, macOS and Windows (no shell, no `#!` line, no PATH games).
//!
//! The fake is driven by environment variables because that is the only channel
//! the SDK leaves open: `build_command` adds `ClaudeCodeOptions::env` entries with
//! `Command::env` and **never** calls `env_clear`, so a test can inject the
//! variables below through `options.env` without touching the test process's own
//! environment.
//!
//! # Environment
//!
//! | Variable | Meaning |
//! |---|---|
//! | `FAKE_CLAUDE_TRANSCRIPT` | path to a newline-delimited JSON directive file to replay |
//! | `FAKE_CLAUDE_ARGS_OUT` | path to write the recorded invocation (argv / cwd / env) as JSON |
//! | `FAKE_CLAUDE_STDIN_OUT` | path to append every line read from stdin, one per line, flushed immediately |
//! | `FAKE_CLAUDE_VERSION` | version string printed for `--version` |
//! | `FAKE_CLAUDE_STDIN_TIMEOUT_MS` | default timeout for the stdin-waiting directives (default 10000) |
//! | `FAKE_CLAUDE_MAX_RUNTIME_MS` | watchdog: hard-exit after this long so a test can never hang (default 30000) |
//!
//! `--version` is answered *before* anything else and the real CLI's shape is
//! reproduced exactly (`"2.1.280 (Claude Code)"`), because
//! [`SemVer::parse`](nexus_claude::transport::subprocess::SemVer::parse) keeps only
//! the first whitespace-separated token. The version check runs in a separate
//! `Command` that does **not** receive `options.env`, so for version tests the
//! version is also read from a sidecar file next to the executable
//! (`<exe>.version`); copy the binary into a temp dir to use it.
//!
//! # Directive vocabulary
//!
//! One JSON object per line. Blank lines and lines starting with `#` or `//` are
//! ignored, so a transcript can be commented.
//!
//! * `{"op":"emit","line":"<raw text>"}` — write the text verbatim plus `\n`.
//!   Use it for valid messages, malformed JSON and plain non-JSON noise alike.
//! * `{"op":"emit_json","json":<value>}` — write `<value>` as one compact JSON line.
//! * `{"op":"emit_partial","text":"..."}` — write the text with **no** trailing
//!   newline and flush: a truncated message. Follow with `exit` to die mid-line.
//! * `{"op":"stderr","line":"..."}` — write one line to stderr.
//! * `{"op":"sleep","ms":N}` — sleep N milliseconds.
//! * `{"op":"await_stdin"}` — block until a line has been read from stdin.
//!   Optional: `"count":N` (default 1), `"contains":"substr"` (skip lines that do
//!   not match), `"timeout_ms":N`, `"optional":true` (continue instead of failing
//!   when the wait times out or stdin closes).
//! * `{"op":"reply_control","match_subtype":"can_use_tool"}` — consume stdin until
//!   an inbound `control_request` matches, then emit a `control_response` echoing
//!   its `request_id`. Optional: `"match_request_id":"..."`, `"response":{...}`
//!   (merged into the inner response object, so `{"subtype":"error","error":"no"}`
//!   scripts a failure), `"timeout_ms":N`, `"optional":true`.
//! * `{"op":"wait_eof"}` — block until stdin reaches EOF. Optional `"timeout_ms"`,
//!   `"optional":true`. Use it to keep the child alive for `disconnect()` tests.
//! * `{"op":"capture_hooks"}` — consume stdin until the SDK's `initialize`
//!   control request, remember the hook callback ids it registers (they are minted
//!   at run time, so a transcript cannot spell them), and acknowledge it like the
//!   real CLI. Optional: `"timeout_ms":N`, `"optional":true`.
//! * `{"op":"emit_hook","event":"PreToolUse","request_id":"h1","input":{...}}` —
//!   emit a `hook_callback` control request for the first callback id captured for
//!   that event. Optional: `"tool_use_id":"..."`, `"await_response":true` (then
//!   block until a stdin line containing the `request_id` arrives), `"optional":true`
//!   (do nothing, instead of failing, when no callback was captured for the event).
//! * `{"op":"spawn_child","program":"sleep","args":["600"]}` — start a real child
//!   process (the fake's "tool"), kept until it exits; default `sleep 600`.
//! * `{"op":"wait_children_exit"}` — block until every spawned child has exited
//!   (e.g. been signalled). Optional `"timeout_ms"`, `"optional":true`.
//! * `{"op":"exit","code":N}` — flush and exit with that code (default 0).
//!
//! When the transcript runs out the fake exits 0, which closes stdout and ends the
//! SDK's reader task. Diagnostic exit codes (always accompanied by a stderr line):
//! 94 unusable directive, 95 stdin closed while waiting, 96 stdin wait timed out,
//! 97 transcript unreadable, 98 watchdog.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use serde_json::{Value, json};

/// Version printed by `--version` when nothing overrides it. Must stay at or
/// above `nexus_claude::cli_download::MIN_CLI_VERSION`; the harness asserts it.
const DEFAULT_VERSION: &str = "2.1.280 (Claude Code)";

const EXIT_BAD_DIRECTIVE: i32 = 94;
const EXIT_STDIN_CLOSED: i32 = 95;
const EXIT_STDIN_TIMEOUT: i32 = 96;
const EXIT_NO_TRANSCRIPT: i32 = 97;
const EXIT_WATCHDOG: i32 = 98;

/// Environment variables whose *value* may be recorded. Everything else is
/// recorded by NAME only: the child inherits the whole ambient environment of
/// the test runner, which on a developer machine holds real credentials
/// (`ANTHROPIC_API_KEY`, cloud tokens, ...), and the recording file is written
/// precisely so that a test can print it.
const ENV_VALUE_ALLOWLIST: [&str; 4] = [
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_AGENT_SDK_VERSION",
    "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
    "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING",
];

/// Substrings that make a variable name credential-shaped. A value is replaced by
/// its byte length even though the test explicitly allowlisted the name through
/// `FAKE_CLAUDE_ARGS_ENV_ALLOW`, so the recording can never become the place a
/// real secret surfaces.
const SECRET_NAME_MARKERS: [&str; 9] = [
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "auth",
    "cookie",
    "session",
];

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // Version detection first: `get_cli_version` spawns `<cli> --version` with no
    // other arguments and reads stdout only.
    if argv.len() == 1 && matches!(argv[0].as_str(), "--version" | "-v" | "version") {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "{}", resolve_version());
        let _ = out.flush();
        return;
    }

    start_watchdog();
    record_invocation(&argv);

    let (rx, _reader) = spawn_stdin_reader();
    let mut fake = Fake {
        out: std::io::stdout(),
        rx,
        default_timeout: Duration::from_millis(env_u64("FAKE_CLAUDE_STDIN_TIMEOUT_MS", 10_000)),
        hooks: BTreeMap::new(),
        children: Vec::new(),
    };

    match std::env::var("FAKE_CLAUDE_TRANSCRIPT") {
        Ok(path) => {
            let path = PathBuf::from(path);
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) => die(
                    EXIT_NO_TRANSCRIPT,
                    &format!("cannot read FAKE_CLAUDE_TRANSCRIPT {}: {e}", path.display()),
                ),
            };
            fake.replay(&text);
        },
        Err(_) => fake.replay(DEFAULT_TRANSCRIPT),
    }

    for child in &mut fake.children {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = fake.out.flush();
}

/// Minimal valid session used when no transcript is given: init, one short
/// assistant turn, one result. The `await_stdin` is `optional` so the fake works
/// both for a test that sends a prompt and for one that only reads.
const DEFAULT_TRANSCRIPT: &str = concat!(
    r#"{"op":"emit_json","json":{"type":"system","subtype":"init","session_id":"fake-session","model":"fake-claude","tools":[],"permissionMode":"default","apiKeySource":"none"}}"#,
    "\n",
    r#"{"op":"await_stdin","optional":true,"timeout_ms":2000}"#,
    "\n",
    r#"{"op":"emit_json","json":{"type":"assistant","message":{"id":"msg_fake_0001","type":"message","role":"assistant","model":"fake-claude","content":[{"type":"text","text":"Hello from fake_claude."}],"stop_reason":"end_turn"}}}"#,
    "\n",
    r#"{"op":"emit_json","json":{"type":"result","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"fake-session","total_cost_usd":0.0,"result":"Hello from fake_claude."}}"#,
    "\n",
);

// ---------------------------------------------------------------------------
// version
// ---------------------------------------------------------------------------

fn resolve_version() -> String {
    if let Ok(v) = std::env::var("FAKE_CLAUDE_VERSION") {
        return v;
    }
    // `get_cli_version` spawns a fresh Command that never receives
    // `options.env`, so the only per-test channel left is a file beside the
    // executable. Copy the binary into a temp dir to use it.
    if let Ok(exe) = std::env::current_exe() {
        let sidecar = exe.with_extension("version");
        if let Ok(text) = std::fs::read_to_string(&sidecar) {
            return text.trim().to_string();
        }
    }
    DEFAULT_VERSION.to_string()
}

// ---------------------------------------------------------------------------
// invocation recording
// ---------------------------------------------------------------------------

fn record_invocation(argv: &[String]) {
    let Ok(path) = std::env::var("FAKE_CLAUDE_ARGS_OUT") else {
        return;
    };

    let extra_allowed: Vec<String> = std::env::var("FAKE_CLAUDE_ARGS_ENV_ALLOW")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut names: Vec<String> = Vec::new();
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in std::env::vars() {
        names.push(name.clone());
        // The curated list is reviewed source; its values are recorded as-is even
        // though `CLAUDE_CODE_MAX_OUTPUT_TOKENS` is credential-shaped by name.
        if ENV_VALUE_ALLOWLIST.contains(&name.as_str()) || name.starts_with("FAKE_CLAUDE_") {
            values.insert(name, value);
            continue;
        }
        // Names a test asked for at runtime get the heuristic.
        if !extra_allowed.contains(&name) {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        if SECRET_NAME_MARKERS.iter().any(|m| lower.contains(m)) {
            values.insert(name, format!("<redacted {} bytes>", value.len()));
        } else {
            values.insert(name, value);
        }
    }
    names.sort();

    let record = json!({
        "program": std::env::args().next().unwrap_or_default(),
        "argv": argv,
        "cwd": std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default(),
        "env": values,
        "env_names": names,
    });

    // Write the whole record beside the target, then rename it into place. The test
    // harness polls for the file's existence and parses it at once; `File::create`
    // truncates first, so a reader arriving between the truncate and the write saw
    // an empty file ("EOF while parsing a value"), which flaked on slow runners.
    // A rename is atomic: the reader sees either no file or the complete record.
    let tmp = format!("{path}.tmp");
    let mut body = serde_json::to_string_pretty(&record).unwrap_or_default();
    body.push('\n');
    if std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

// ---------------------------------------------------------------------------
// stdin
// ---------------------------------------------------------------------------

/// Read stdin on a background thread so the directive loop can wait for a line
/// with a timeout. Each line is appended to `FAKE_CLAUDE_STDIN_OUT` and flushed
/// as it arrives, so the recording survives an abrupt `exit` directive.
fn spawn_stdin_reader() -> (Receiver<String>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<String>();
    let sink = std::env::var("FAKE_CLAUDE_STDIN_OUT").ok();
    let handle = std::thread::spawn(move || {
        let mut sink = sink.and_then(|p| OpenOptions::new().create(true).append(true).open(p).ok());
        let stdin = std::io::stdin();
        for line in BufReader::new(stdin.lock()).lines() {
            let Ok(line) = line else { break };
            if let Some(file) = sink.as_mut() {
                let _ = writeln!(file, "{line}");
                let _ = file.flush();
            }
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (rx, handle)
}

// ---------------------------------------------------------------------------
// replay
// ---------------------------------------------------------------------------

struct Fake {
    out: std::io::Stdout,
    rx: Receiver<String>,
    default_timeout: Duration,
    /// Hook callback ids registered by the SDK's `initialize` request, by event
    /// name. Filled by the `capture_hooks` directive.
    hooks: BTreeMap<String, Vec<String>>,
    /// Processes started by `spawn_child`: the fake's own "tools", so a test can
    /// signal them the way it signals a real CLI's.
    children: Vec<std::process::Child>,
}

impl Fake {
    fn replay(&mut self, text: &str) {
        for (lineno, raw) in text.lines().enumerate() {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("//") {
                continue;
            }
            let directive: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => die(
                    EXIT_BAD_DIRECTIVE,
                    &format!("transcript line {}: not JSON: {e}", lineno + 1),
                ),
            };
            self.run(&directive, lineno + 1);
        }
    }

    fn run(&mut self, d: &Value, lineno: usize) {
        let op = d.get("op").and_then(Value::as_str).unwrap_or_default();
        match op {
            "emit" => {
                let line = d.get("line").and_then(Value::as_str).unwrap_or_default();
                self.write_line(line);
            },
            "emit_json" => {
                let value = d.get("json").cloned().unwrap_or(Value::Null);
                self.write_line(&value.to_string());
            },
            "emit_partial" => {
                let text = d.get("text").and_then(Value::as_str).unwrap_or_default();
                let _ = write!(self.out, "{text}");
                let _ = self.out.flush();
            },
            "stderr" => {
                let line = d.get("line").and_then(Value::as_str).unwrap_or_default();
                let mut err = std::io::stderr();
                let _ = writeln!(err, "{line}");
                let _ = err.flush();
            },
            "sleep" => {
                std::thread::sleep(Duration::from_millis(
                    d.get("ms").and_then(Value::as_u64).unwrap_or(0),
                ));
            },
            "await_stdin" => {
                let count = d.get("count").and_then(Value::as_u64).unwrap_or(1);
                let contains = d.get("contains").and_then(Value::as_str);
                for _ in 0..count {
                    if self.next_line(d, contains, lineno).is_none() {
                        return;
                    }
                }
            },
            "reply_control" => self.reply_control(d, lineno),
            "capture_hooks" => self.capture_hooks(d, lineno),
            "emit_hook" => self.emit_hook(d, lineno),
            "wait_eof" => {
                let deadline = std::time::Instant::now() + self.timeout(d);
                loop {
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    match self.rx.recv_timeout(left) {
                        Ok(_) => continue,
                        Err(RecvTimeoutError::Disconnected) => return,
                        Err(RecvTimeoutError::Timeout) => {
                            if optional(d) {
                                return;
                            }
                            die(
                                EXIT_STDIN_TIMEOUT,
                                &format!("transcript line {lineno}: wait_eof timed out"),
                            );
                        },
                    }
                }
            },
            "spawn_child" => {
                let program = d.get("program").and_then(Value::as_str).unwrap_or("sleep");
                let args: Vec<String> = d
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_else(|| vec!["600".to_owned()]);
                match std::process::Command::new(program)
                    .args(&args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .spawn()
                {
                    Ok(child) => self.children.push(child),
                    Err(e) => die(
                        EXIT_BAD_DIRECTIVE,
                        &format!("transcript line {lineno}: spawn_child failed: {e}"),
                    ),
                }
            },
            "wait_children_exit" => {
                let deadline = std::time::Instant::now() + self.timeout(d);
                loop {
                    self.children
                        .retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
                    if self.children.is_empty() {
                        return;
                    }
                    if std::time::Instant::now() >= deadline {
                        if optional(d) {
                            return;
                        }
                        die(
                            EXIT_STDIN_TIMEOUT,
                            &format!("transcript line {lineno}: wait_children_exit timed out"),
                        );
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            },
            "exit" => {
                let code = d.get("code").and_then(Value::as_i64).unwrap_or(0) as i32;
                let _ = self.out.flush();
                std::process::exit(code);
            },
            other => die(
                EXIT_BAD_DIRECTIVE,
                &format!("transcript line {lineno}: unknown op {other:?}"),
            ),
        }
    }

    fn reply_control(&mut self, d: &Value, lineno: usize) {
        let want_subtype = d.get("match_subtype").and_then(Value::as_str);
        let want_id = d.get("match_request_id").and_then(Value::as_str);

        loop {
            let Some(line) = self.next_line(d, None, lineno) else {
                return;
            };
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if msg.get("type").and_then(Value::as_str) != Some("control_request") {
                continue;
            }
            let request = msg.get("request").cloned().unwrap_or(Value::Null);
            // The SDK writes the subtype under `request.subtype` for the control
            // protocol and under `request.type` for the legacy interrupt.
            let subtype = request
                .get("subtype")
                .and_then(Value::as_str)
                .or_else(|| request.get("type").and_then(Value::as_str));
            if let Some(want) = want_subtype
                && subtype != Some(want)
            {
                continue;
            }
            // Likewise the request id is top-level for the control protocol and
            // nested inside `request` for the legacy interrupt.
            let request_id = msg
                .get("request_id")
                .or_else(|| msg.get("requestId"))
                .or_else(|| request.get("request_id"))
                .or_else(|| request.get("requestId"))
                .cloned()
                .unwrap_or(Value::Null);
            if let Some(want) = want_id
                && request_id.as_str() != Some(want)
            {
                continue;
            }

            let mut inner = serde_json::Map::new();
            inner.insert("subtype".to_string(), json!("success"));
            inner.insert("request_id".to_string(), request_id);
            if let Some(Value::Object(extra)) = d.get("response") {
                for (k, v) in extra {
                    inner.insert(k.clone(), v.clone());
                }
            }
            let response = json!({"type": "control_response", "response": Value::Object(inner)});
            self.write_line(&response.to_string());
            return;
        }
    }

    /// Consume stdin until the SDK's `initialize` request, keep the callback ids
    /// of its `hooks` map, and acknowledge the request.
    fn capture_hooks(&mut self, d: &Value, lineno: usize) {
        loop {
            let Some(line) = self.next_line(d, None, lineno) else {
                return;
            };
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let request = msg.get("request").cloned().unwrap_or(Value::Null);
            if msg.get("type").and_then(Value::as_str) != Some("control_request")
                || request.get("subtype").and_then(Value::as_str) != Some("initialize")
            {
                continue;
            }
            if let Some(Value::Object(hooks)) = request.get("hooks") {
                for (event, matchers) in hooks {
                    let ids: Vec<String> = matchers
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|matcher| matcher.get("hookCallbackIds"))
                        .filter_map(Value::as_array)
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect();
                    self.hooks.insert(event.clone(), ids);
                }
            }
            let request_id = msg.get("request_id").cloned().unwrap_or(Value::Null);
            let response = json!({
                "type": "control_response",
                "response": {"subtype": "success", "request_id": request_id},
            });
            self.write_line(&response.to_string());
            return;
        }
    }

    /// Emit a `hook_callback` request for a callback id captured by
    /// `capture_hooks`, and optionally wait for its response.
    fn emit_hook(&mut self, d: &Value, lineno: usize) {
        let event = d.get("event").and_then(Value::as_str).unwrap_or_default();
        let Some(callback_id) = self.hooks.get(event).and_then(|ids| ids.first()).cloned() else {
            if optional(d) {
                return;
            }
            die(
                EXIT_BAD_DIRECTIVE,
                &format!(
                    "transcript line {lineno}: emit_hook for {event:?}, but no callback id was captured for it (missing capture_hooks?)"
                ),
            );
        };
        let request_id = d
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or("hook-request")
            .to_string();
        let mut request = serde_json::Map::new();
        request.insert("subtype".to_string(), json!("hook_callback"));
        request.insert("callback_id".to_string(), json!(callback_id));
        request.insert(
            "input".to_string(),
            d.get("input").cloned().unwrap_or_else(|| json!({})),
        );
        if let Some(tool_use_id) = d.get("tool_use_id").filter(|v| !v.is_null()) {
            request.insert("tool_use_id".to_string(), tool_use_id.clone());
        }
        let message = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": Value::Object(request),
        });
        self.write_line(&message.to_string());
        if d.get("await_response").and_then(Value::as_bool) == Some(true) {
            let _ = self.next_line(d, Some(&request_id), lineno);
        }
    }

    /// Pop the next stdin line, honouring `contains`, `timeout_ms` and `optional`.
    /// Returns `None` only when the directive was `optional` and gave up.
    fn next_line(&mut self, d: &Value, contains: Option<&str>, lineno: usize) -> Option<String> {
        let timeout = self.timeout(d);
        loop {
            match self.rx.recv_timeout(timeout) {
                Ok(line) => match contains {
                    Some(needle) if !line.contains(needle) => continue,
                    _ => return Some(line),
                },
                Err(RecvTimeoutError::Timeout) => {
                    if optional(d) {
                        return None;
                    }
                    die(
                        EXIT_STDIN_TIMEOUT,
                        &format!(
                            "transcript line {lineno}: no stdin line within {} ms (op={}, contains={contains:?})",
                            timeout.as_millis(),
                            d.get("op").and_then(Value::as_str).unwrap_or("?"),
                        ),
                    );
                },
                Err(RecvTimeoutError::Disconnected) => {
                    if optional(d) {
                        return None;
                    }
                    die(
                        EXIT_STDIN_CLOSED,
                        &format!("transcript line {lineno}: stdin closed while waiting"),
                    );
                },
            }
        }
    }

    fn timeout(&self, d: &Value) -> Duration {
        d.get("timeout_ms")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(self.default_timeout)
    }

    fn write_line(&mut self, line: &str) {
        let _ = writeln!(self.out, "{line}");
        let _ = self.out.flush();
    }
}

fn optional(d: &Value) -> bool {
    d.get("optional").and_then(Value::as_bool).unwrap_or(false)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Hard upper bound on the fake's life so a mis-scripted transcript can never
/// leave a child process (or a test) hanging.
fn start_watchdog() {
    let ms = env_u64("FAKE_CLAUDE_MAX_RUNTIME_MS", 30_000);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ms));
        let mut err = std::io::stderr();
        let _ = writeln!(err, "fake_claude: watchdog fired after {ms} ms, exiting");
        let _ = err.flush();
        std::process::exit(EXIT_WATCHDOG);
    });
}

fn die(code: i32, message: &str) -> ! {
    let mut err = std::io::stderr();
    let _ = writeln!(err, "fake_claude: {message}");
    let _ = err.flush();
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
