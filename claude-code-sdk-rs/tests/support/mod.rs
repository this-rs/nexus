//! Test support for driving `SubprocessTransport` against the `fake_claude`
//! binary — no real `claude` install, no network, no shell scripts (so it works
//! on windows-latest too).
//!
//! # How to use it
//!
//! `tests/support/mod.rs` is **not** a cargo test target: cargo auto-discovers
//! `tests/*.rs` and `tests/*/main.rs` only, so a `mod.rs` inside a subdirectory is
//! compiled solely as a module of whichever sibling test declares it. Declare it
//! with a plain `mod support;` at the top of your `tests/<your_test>.rs`:
//!
//! ```ignore
//! mod support;
//!
//! use std::time::Duration;
//! use support::*;              // brings in the `Transport` trait too
//!
//! #[tokio::test]
//! async fn scripted_session() {
//!     let fake = Transcript::new()
//!         .await_stdin()                         // wait for the prompt before printing
//!         .init("sess-1")
//!         .assistant_text("bonjour")
//!         .result_ok("bonjour")
//!         .build();
//!     let mut transport = fake.transport();      // a real SubprocessTransport
//!     transport.connect().await.unwrap();
//!     let stream = start_turn(&mut transport, "salut").await;   // subscribe, then send
//!     let messages = collect_until_result(stream, Duration::from_secs(5)).await;
//!     transport.disconnect().await.unwrap();
//!     assert_eq!(assistant_texts(&messages), vec!["bonjour".to_string()]);
//! }
//! ```
//!
//! Each test file that declares `mod support;` gets its own copy of this module,
//! so the whole module is `allow(dead_code)`: otherwise a file that uses two
//! helpers would fail `-D warnings` on the other thirty.
//!
//! # Using it from an inline `#[cfg(test)]` module in `src/`
//!
//! Works, and is verified, but it is not free: it needs two lines in `src/lib.rs`,
//! so it is opt-in rather than shipped.
//!
//! ```ignore
//! // src/lib.rs, right after the crate-level `#![...]` attributes
//! extern crate self as nexus_claude;   // makes the `nexus_claude::` paths below resolve
//!
//! #[cfg(test)]
//! #[path = "../tests/support/mod.rs"]
//! mod support;
//! ```
//!
//! Two caveats. `CARGO_BIN_EXE_fake_claude` does not exist for the lib's unit-test
//! target, so [`fake_cli_path`] falls back to deriving the path from
//! `current_exe()` — which means the bins must have been built (`cargo test` builds
//! them; `cargo test --lib` alone in a clean target dir does not). And the module is
//! then compiled into every `cargo test --lib` run. Prefer an integration test under
//! `tests/` unless you specifically need access to private SDK internals.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::stream::{Stream, StreamExt};
use nexus_claude::{ClaudeCodeOptions, Message, Result as SdkResult};

/// Re-exported so a test only needs `use support::*;` to call `connect()` and
/// friends — those live on the `Transport` trait, which must be in scope.
pub use nexus_claude::transport::{InputMessage, SubprocessTransport, Transport};
use serde_json::{Value, json};
use tempfile::TempDir;

/// Diagnostic exit codes `fake_claude` uses; surfaced here so an assertion
/// failure message can name the cause instead of "the stream was empty".
pub mod exit_code {
    /// A transcript line was not a usable directive.
    pub const BAD_DIRECTIVE: i32 = 94;
    /// The fake was waiting for a stdin line and stdin closed.
    pub const STDIN_CLOSED: i32 = 95;
    /// The fake was waiting for a stdin line and timed out.
    pub const STDIN_TIMEOUT: i32 = 96;
    /// `FAKE_CLAUDE_TRANSCRIPT` could not be read.
    pub const NO_TRANSCRIPT: i32 = 97;
    /// The fake outlived `FAKE_CLAUDE_MAX_RUNTIME_MS`.
    pub const WATCHDOG: i32 = 98;
}

/// Absolute path to the `fake_claude` test double.
///
/// Uses `CARGO_BIN_EXE_fake_claude`, which cargo defines for integration tests
/// and guarantees is built before they run. The `current_exe()` fallback exists
/// for the lib unit-test target, where that variable does not exist — there,
/// `cargo test` must have built the bins (plain `cargo test --lib` does not).
pub fn fake_cli_path() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_fake_claude") {
        return PathBuf::from(path);
    }
    let exe = std::env::current_exe().expect("current_exe");
    let mut dir = exe
        .parent()
        .expect("test binary has a parent")
        .to_path_buf();
    if dir.file_name().and_then(|n| n.to_str()) == Some("deps") {
        dir = dir.parent().expect("deps has a parent").to_path_buf();
    }
    dir.join(format!("fake_claude{}", std::env::consts::EXE_SUFFIX))
}

// ---------------------------------------------------------------------------
// Transcript builder
// ---------------------------------------------------------------------------

/// Builder for a `fake_claude` directive file.
///
/// Every method appends one directive; `build()` writes the file into a
/// [`TempDir`] owned by the returned [`FakeCli`], so the transcript lives exactly
/// as long as the handle you keep in the test.
#[derive(Debug, Default, Clone)]
pub struct Transcript {
    lines: Vec<String>,
}

impl Transcript {
    /// Start an empty transcript.
    pub fn new() -> Self {
        Self::default()
    }

    fn push(mut self, directive: Value) -> Self {
        self.lines.push(directive.to_string());
        self
    }

    /// An arbitrary directive of the fake, for the ones no helper spells.
    pub fn directive(self, directive: Value) -> Self {
        self.push(directive)
    }

    // ----- raw directives -------------------------------------------------

    /// Emit `line` on stdout verbatim. Use it for noise and malformed JSON.
    pub fn raw(self, line: impl Into<String>) -> Self {
        self.push(json!({"op": "emit", "line": line.into()}))
    }

    /// Emit a JSON value as one compact stdout line.
    pub fn json(self, value: Value) -> Self {
        self.push(json!({"op": "emit_json", "json": value}))
    }

    /// Emit a line of text that is valid JSON prefix but never terminated: no
    /// trailing newline. Pair with [`Transcript::exit_with`] to die mid-message.
    pub fn partial(self, text: impl Into<String>) -> Self {
        self.push(json!({"op": "emit_partial", "text": text.into()}))
    }

    /// Emit a line that is not JSON at all.
    pub fn garbage(self) -> Self {
        self.raw("this is not json at all")
    }

    /// Emit a line that starts like JSON but cannot be parsed.
    pub fn malformed_json(self) -> Self {
        self.raw(r#"{"type":"assistant","message":{"content":[{"type":"text",}]}"#)
    }

    /// Emit a syntactically valid JSON line the SDK cannot turn into a `Message`.
    pub fn unparseable_message(self) -> Self {
        self.json(json!({"type": "assistant", "message": {"content": "not-an-array"}}))
    }

    /// A comment line in the transcript file: ignored by the fake, readable by
    /// whoever debugs the test.
    pub fn comment(mut self, text: impl AsRef<str>) -> Self {
        self.lines.push(format!("# {}", text.as_ref()));
        self
    }

    /// A blank line in the transcript file: ignored by the fake.
    pub fn blank(mut self) -> Self {
        self.lines.push(String::new());
        self
    }

    /// Write a line to the fake's stderr.
    pub fn stderr(self, line: impl Into<String>) -> Self {
        self.push(json!({"op": "stderr", "line": line.into()}))
    }

    /// Sleep `ms` milliseconds before the next directive.
    pub fn sleep_ms(self, ms: u64) -> Self {
        self.push(json!({"op": "sleep", "ms": ms}))
    }

    /// Block until one line has been read from stdin.
    pub fn await_stdin(self) -> Self {
        self.push(json!({"op": "await_stdin"}))
    }

    /// Block until `n` lines have been read from stdin.
    pub fn await_stdin_lines(self, n: u64) -> Self {
        self.push(json!({"op": "await_stdin", "count": n}))
    }

    /// Block until a stdin line containing `needle` arrives, discarding others.
    pub fn await_stdin_containing(self, needle: impl Into<String>) -> Self {
        self.push(json!({"op": "await_stdin", "contains": needle.into()}))
    }

    /// Block for a stdin line but carry on if none arrives within `ms`.
    pub fn await_stdin_optional(self, ms: u64) -> Self {
        self.push(json!({"op": "await_stdin", "optional": true, "timeout_ms": ms}))
    }

    /// Answer the SDK's next `control_request` with `subtype` with a success
    /// `control_response` echoing its `request_id`.
    pub fn reply_control_success(self, subtype: impl Into<String>) -> Self {
        self.push(json!({"op": "reply_control", "match_subtype": subtype.into()}))
    }

    /// Answer the SDK's next `control_request` with `subtype`, merging `payload`
    /// into the inner response object.
    pub fn reply_control_with(self, subtype: impl Into<String>, payload: Value) -> Self {
        self.push(json!({
            "op": "reply_control",
            "match_subtype": subtype.into(),
            "response": payload,
        }))
    }

    /// Answer the SDK's next `control_request` with `subtype` with an error.
    pub fn reply_control_error(self, subtype: impl Into<String>, error: impl Into<String>) -> Self {
        self.reply_control_with(subtype, json!({"subtype": "error", "error": error.into()}))
    }

    /// Answer a specific `request_id` rather than matching on subtype.
    pub fn reply_control_for_id(self, request_id: impl Into<String>) -> Self {
        self.push(json!({"op": "reply_control", "match_request_id": request_id.into()}))
    }

    /// Wait for the SDK's `initialize` request, remember the hook callback ids it
    /// registers and acknowledge it. Needed before [`Transcript::emit_hook`]: the
    /// ids are minted at run time, a transcript cannot spell them.
    pub fn capture_hooks(self) -> Self {
        self.push(json!({"op": "capture_hooks"}))
    }

    /// A `hook_callback` request for the callback the SDK registered for `event`
    /// (`PreToolUse`, `PostToolUse`, `PreCompact`), then wait for its response.
    pub fn emit_hook(
        self,
        event: &str,
        request_id: &str,
        input: Value,
        tool_use_id: Option<&str>,
    ) -> Self {
        self.push(json!({
            "op": "emit_hook",
            "event": event,
            "request_id": request_id,
            "input": input,
            "tool_use_id": tool_use_id,
            "await_response": true,
        }))
    }

    /// Stay alive until the SDK closes stdin (`end_input` / `disconnect`).
    pub fn wait_eof(self) -> Self {
        self.push(json!({"op": "wait_eof"}))
    }

    /// Stay alive until stdin closes, but give up after `ms`.
    pub fn wait_eof_for(self, ms: u64) -> Self {
        self.push(json!({"op": "wait_eof", "timeout_ms": ms, "optional": true}))
    }

    /// Start a real child process (the fake's own "tool"); default `sleep 120`.
    pub fn spawn_child(self) -> Self {
        self.push(json!({"op": "spawn_child", "program": "sleep", "args": ["120"]}))
    }

    /// Block until every spawned child has exited (been signalled), or `ms`.
    pub fn wait_children_exit(self, ms: u64) -> Self {
        self.push(json!({"op": "wait_children_exit", "timeout_ms": ms, "optional": true}))
    }

    /// Exit with `code`, immediately.
    pub fn exit_with(self, code: i32) -> Self {
        self.push(json!({"op": "exit", "code": code}))
    }

    // ----- message shapes -------------------------------------------------

    /// The `system`/`init` message the CLI opens every session with.
    pub fn init(self, session_id: &str) -> Self {
        self.json(msg::init(session_id))
    }

    /// An assistant turn carrying a single text block.
    pub fn assistant_text(self, text: &str) -> Self {
        self.json(msg::assistant_text(text))
    }

    /// An assistant turn carrying a `thinking` block.
    pub fn assistant_thinking(self, thinking: &str, signature: &str) -> Self {
        self.json(msg::assistant_thinking(thinking, signature))
    }

    /// An assistant turn carrying a `tool_use` block.
    pub fn tool_use(self, id: &str, name: &str, input: Value) -> Self {
        self.json(msg::tool_use(id, name, input))
    }

    /// The `user` message the CLI emits to report a tool result.
    pub fn tool_result(self, tool_use_id: &str, content: &str, is_error: bool) -> Self {
        self.json(msg::tool_result(tool_use_id, content, is_error))
    }

    /// A partial-message `stream_event` (needs `include_partial_messages`).
    pub fn text_delta(self, text: &str) -> Self {
        self.json(msg::text_delta(text))
    }

    /// An arbitrary `system` message (flat payload, like the real CLI).
    pub fn system(self, subtype: &str, payload: Value) -> Self {
        self.json(msg::system(subtype, payload))
    }

    /// The terminal `result` message, successful.
    pub fn result_ok(self, text: &str) -> Self {
        self.json(msg::result("success", text, false))
    }

    /// The terminal `result` message, flagged as an error.
    pub fn result_error(self, text: &str) -> Self {
        self.json(msg::result("error_during_execution", text, true))
    }

    /// A `can_use_tool` control request from the CLI, i.e. a permission prompt.
    pub fn permission_request(self, request_id: &str, tool_name: &str, input: Value) -> Self {
        self.json(msg::permission_request(request_id, tool_name, input))
    }

    /// A `hook_callback` control request from the CLI.
    pub fn hook_callback(self, request_id: &str, callback_id: &str, input: Value) -> Self {
        self.json(msg::hook_callback(request_id, callback_id, input))
    }

    /// A `control_response` the SDK is expected to correlate by `request_id`.
    pub fn control_response(self, request_id: &str, success: bool) -> Self {
        self.json(msg::control_response(request_id, success))
    }

    // ----- terminal --------------------------------------------------------

    /// Write the transcript to a temp dir and return the handle.
    pub fn build(self) -> FakeCli {
        self.build_inner(None)
    }

    /// Like [`Transcript::build`], but the fake reports `version` for `--version`.
    ///
    /// The SDK's version probe (`get_cli_version`) spawns its own `Command` and
    /// does not pass `options.env`, so the only per-test channel is a file beside
    /// the executable: this copies `fake_claude` into the temp dir and drops a
    /// `fake_claude.version` next to it.
    pub fn build_with_version(self, version: &str) -> FakeCli {
        self.build_inner(Some(version))
    }

    fn build_inner(self, version: Option<&str>) -> FakeCli {
        let dir = TempDir::new().expect("create temp dir for transcript");
        let transcript = dir.path().join("transcript.jsonl");
        let mut body = self.lines.join("\n");
        body.push('\n');
        std::fs::write(&transcript, body).expect("write transcript");

        let cli_path = match version {
            None => fake_cli_path(),
            Some(version) => {
                let copy = dir
                    .path()
                    .join(format!("fake_claude{}", std::env::consts::EXE_SUFFIX));
                // Hard-link instead of copying when possible. `fs::copy` leaves a
                // write fd open on the new executable while it is written; if another
                // test thread forks in that window the child inherits the fd, and
                // spawning the file then fails with ETXTBSY ("Text file busy") on
                // Linux. A hard link never opens the binary for writing, and
                // `current_exe()` still reports the link's own path, so the
                // `.version` sidecar next to it is found. Fall back to a copy
                // across filesystems.
                if std::fs::hard_link(fake_cli_path(), &copy).is_err() {
                    std::fs::copy(fake_cli_path(), &copy).expect("copy fake_claude");
                }
                std::fs::write(copy.with_extension("version"), version)
                    .expect("write version sidecar");
                copy
            },
        };

        FakeCli {
            cli_path,
            transcript,
            args_out: dir.path().join("invocation.json"),
            stdin_out: dir.path().join("stdin.jsonl"),
            wire_transcript: true,
            dir,
        }
    }
}

// ---------------------------------------------------------------------------
// FakeCli handle
// ---------------------------------------------------------------------------

/// A built transcript plus the paths the fake reads and writes. Keep it alive for
/// as long as the transport: dropping it deletes the temp dir.
#[derive(Debug)]
pub struct FakeCli {
    dir: TempDir,
    cli_path: PathBuf,
    transcript: PathBuf,
    args_out: PathBuf,
    stdin_out: PathBuf,
    wire_transcript: bool,
}

/// The fake with no transcript at all: it replays its built-in minimal session
/// (init, one assistant turn, one result), so a smoke test is one line.
pub fn default_session() -> FakeCli {
    let mut fake = Transcript::new().build();
    fake.wire_transcript = false;
    fake
}

impl FakeCli {
    /// Path of the executable the transport will spawn.
    pub fn cli_path(&self) -> &Path {
        &self.cli_path
    }

    /// The temp dir holding the transcript and the recordings.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Default options already pointed at the fake.
    pub fn options(&self) -> ClaudeCodeOptions {
        self.options_with(ClaudeCodeOptions::default())
    }

    /// Your own options, with the fake's `cli_path` and control variables added.
    /// Existing `options.env` entries win, so a test can override a timeout.
    pub fn options_with(&self, mut options: ClaudeCodeOptions) -> ClaudeCodeOptions {
        options.cli_path = Some(self.cli_path.clone());
        let mut wire: HashMap<String, String> = HashMap::new();
        if self.wire_transcript {
            wire.insert(
                "FAKE_CLAUDE_TRANSCRIPT".into(),
                self.transcript.display().to_string(),
            );
        }
        wire.insert(
            "FAKE_CLAUDE_ARGS_OUT".into(),
            self.args_out.display().to_string(),
        );
        wire.insert(
            "FAKE_CLAUDE_STDIN_OUT".into(),
            self.stdin_out.display().to_string(),
        );
        // Bounded by default: a mis-scripted transcript must fail a test, not hang CI.
        wire.insert("FAKE_CLAUDE_STDIN_TIMEOUT_MS".into(), "5000".into());
        wire.insert("FAKE_CLAUDE_MAX_RUNTIME_MS".into(), "15000".into());
        // Keep the fake's coverage profile out of the workspace profile set.
        //
        // Under `cargo llvm-cov`, `fake_claude` is an instrumented workspace
        // binary and inherits `LLVM_PROFILE_FILE`, so every one of the ~160
        // spawns a suite makes writes a `.profraw` next to the test binaries'.
        // Meanwhile the disconnect path exists to kill an uncooperative child
        // (SIGINT → SIGTERM → SIGKILL), and a process killed while its atexit
        // handler is flushing the profile leaves a *short* `.profraw`. One short
        // file makes `llvm-profdata merge` reject the entire set — "invalid
        // instrumentation profile data (file header is corrupt)" followed by
        // "no profile can be merged" — so the Coverage job fails after every
        // test has passed, intermittently and for reasons no test can show.
        // Pointing the fake at its own temp dir means a damaged profile lands
        // where nothing merges it. The fake is a test double, so its own
        // coverage was never the measurement anyone wanted.
        if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
            wire.insert(
                "LLVM_PROFILE_FILE".into(),
                self.dir()
                    .join("fake-claude-%p-%16m.profraw")
                    .display()
                    .to_string(),
            );
        }
        for (key, value) in wire {
            options.env.entry(key).or_insert(value);
        }
        options
    }

    /// A `SubprocessTransport` on default options, ready to `connect()`.
    pub fn transport(&self) -> SubprocessTransport {
        self.transport_with(ClaudeCodeOptions::default())
    }

    /// A `SubprocessTransport` on your options, wired to the fake.
    pub fn transport_with(&self, options: ClaudeCodeOptions) -> SubprocessTransport {
        SubprocessTransport::with_cli_path(self.options_with(options), self.cli_path.clone())
    }

    /// The invocation `build_command` actually produced. Panics if the fake never
    /// ran; use [`FakeCli::wait_for_invocation`] when racing a fresh `connect()`.
    pub fn invocation(&self) -> Invocation {
        let text = std::fs::read_to_string(&self.args_out).unwrap_or_else(|e| {
            panic!(
                "fake_claude wrote no invocation record at {}: {e} (did connect() succeed?)",
                self.args_out.display()
            )
        });
        Invocation(serde_json::from_str(&text).expect("invocation record is JSON"))
    }

    /// Poll until the invocation record exists.
    pub async fn wait_for_invocation(&self, within: Duration) -> Invocation {
        poll_until(within, || self.args_out.exists()).await;
        self.invocation()
    }

    /// Every line the SDK wrote to the fake's stdin, in order.
    pub fn stdin_lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.stdin_out)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Every line the SDK wrote to stdin, parsed as JSON (non-JSON lines dropped).
    pub fn stdin_json(&self) -> Vec<Value> {
        self.stdin_lines()
            .iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Poll until at least `n` stdin lines have been recorded, then return them.
    pub async fn wait_for_stdin_lines(&self, n: usize, within: Duration) -> Vec<String> {
        poll_until(within, || self.stdin_lines().len() >= n).await;
        self.stdin_lines()
    }
}

/// What the fake saw on its command line and in its environment.
#[derive(Debug, Clone)]
pub struct Invocation(Value);

impl Invocation {
    /// Arguments, excluding `argv[0]`.
    pub fn args(&self) -> Vec<String> {
        self.0
            .get("argv")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether `flag` was passed.
    pub fn has_flag(&self, flag: &str) -> bool {
        self.args().iter().any(|a| a == flag)
    }

    /// The argument right after `flag`, if any.
    pub fn flag_value(&self, flag: &str) -> Option<String> {
        let args = self.args();
        let index = args.iter().position(|a| a == flag)?;
        args.get(index + 1).cloned()
    }

    /// Every value passed for a repeatable flag.
    pub fn flag_values(&self, flag: &str) -> Vec<String> {
        let args = self.args();
        args.iter()
            .enumerate()
            .filter(|(_, a)| a.as_str() == flag)
            .filter_map(|(i, _)| args.get(i + 1).cloned())
            .collect()
    }

    /// The working directory the fake was started in.
    pub fn cwd(&self) -> String {
        self.0
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    /// Value of an environment variable, for the names the fake is allowed to
    /// record (see `fake_claude`'s allowlist). Credential-shaped names come back
    /// as `<redacted N bytes>` on purpose.
    ///
    /// Matches the name with [`env_names_match`], so that asking for `PATH` finds
    /// the `Path` a Windows recording holds. Without that this returned `None`
    /// there even for an allowlisted variable, and any `is_none()` assertion about
    /// an inherited name passed for the wrong reason.
    pub fn env(&self, name: &str) -> Option<String> {
        self.0
            .get("env")
            .and_then(Value::as_object)
            .and_then(|env| {
                env.iter()
                    .find(|(recorded, _)| env_names_match(recorded, name))
                    .and_then(|(_, value)| value.as_str())
            })
            .map(str::to_string)
    }

    /// Whether the variable was present in the child's environment at all.
    ///
    /// Matches the name with [`env_names_match`].
    pub fn has_env(&self, name: &str) -> bool {
        self.0
            .get("env_names")
            .and_then(Value::as_array)
            .is_some_and(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|recorded| env_names_match(recorded, name))
            })
    }

    /// The raw record, for assertions the helpers do not cover.
    pub fn raw(&self) -> &Value {
        &self.0
    }
}

/// Compares two environment-variable names the way the host platform does.
///
/// Windows environment names are case-insensitive, and the inherited search path
/// is spelled `Path` there, not `PATH`. An ASCII-case-insensitive comparison is
/// therefore the *correct* comparison on Windows, not a loosened one: a helper
/// that answers "was this variable handed to the child?" has to answer the
/// question the operating system would. Exact matching answered "no" about a
/// variable the child demonstrably inherited.
///
/// Deliberately **not** relaxed on Unix, where names are case-sensitive. A test
/// double more permissive than the system it stands in for would one day
/// green-light a test asking for `path`, and prove nothing.
///
/// ASCII-only: these tests use ASCII names, and `eq_ignore_ascii_case` avoids
/// pretending to reproduce Windows' full locale-independent uppercase mapping.
#[cfg(windows)]
fn env_names_match(recorded: &str, wanted: &str) -> bool {
    recorded.eq_ignore_ascii_case(wanted)
}

/// See the `cfg(windows)` twin above: on Unix the names are case-sensitive.
#[cfg(not(windows))]
fn env_names_match(recorded: &str, wanted: &str) -> bool {
    recorded == wanted
}

// ---------------------------------------------------------------------------
// one-liner entry points
// ---------------------------------------------------------------------------

/// Build the transcript and a transport in one step.
pub fn fake_transport(transcript: Transcript) -> (FakeCli, SubprocessTransport) {
    let fake = transcript.build();
    let transport = fake.transport();
    (fake, transport)
}

/// Build the transcript and the options pointing at it, in one step.
pub fn fake_options(transcript: Transcript) -> (FakeCli, ClaudeCodeOptions) {
    let fake = transcript.build();
    let options = fake.options();
    (fake, options)
}

// ---------------------------------------------------------------------------
// message shapes
// ---------------------------------------------------------------------------

/// The exact JSON the real CLI puts on stdout, as `serde_json::Value`s.
pub mod msg {
    use serde_json::{Value, json};

    /// `system`/`init`: the session handshake.
    pub fn init(session_id: &str) -> Value {
        json!({
            "type": "system",
            "subtype": "init",
            "session_id": session_id,
            "model": "fake-claude",
            "cwd": ".",
            "tools": ["Bash", "Read"],
            "permissionMode": "default",
            "apiKeySource": "none",
        })
    }

    /// Any other `system` message; the payload is flat, as the current CLI sends it.
    pub fn system(subtype: &str, payload: Value) -> Value {
        let mut obj = match payload {
            Value::Object(map) => map,
            other => {
                let mut map = serde_json::Map::new();
                map.insert("payload".to_string(), other);
                map
            },
        };
        obj.insert("type".to_string(), json!("system"));
        obj.insert("subtype".to_string(), json!(subtype));
        Value::Object(obj)
    }

    fn assistant(content: Value) -> Value {
        json!({
            "type": "assistant",
            "message": {
                "id": "msg_fake",
                "type": "message",
                "role": "assistant",
                "model": "fake-claude",
                "content": content,
                "stop_reason": "end_turn",
            },
        })
    }

    /// One text block.
    pub fn assistant_text(text: &str) -> Value {
        assistant(json!([{"type": "text", "text": text}]))
    }

    /// One thinking block (both fields are mandatory for the SDK's parser).
    pub fn assistant_thinking(thinking: &str, signature: &str) -> Value {
        assistant(json!([{"type": "thinking", "thinking": thinking, "signature": signature}]))
    }

    /// One `tool_use` block.
    pub fn tool_use(id: &str, name: &str, input: Value) -> Value {
        assistant(json!([{"type": "tool_use", "id": id, "name": name, "input": input}]))
    }

    /// The `user` message the CLI emits to carry a tool result back.
    pub fn tool_result(tool_use_id: &str, content: &str, is_error: bool) -> Value {
        json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                    "is_error": is_error,
                }],
            },
            "parent_tool_use_id": tool_use_id,
        })
    }

    /// A `stream_event` text delta (requires `--include-partial-messages`).
    pub fn text_delta(text: &str) -> Value {
        json!({
            "type": "stream_event",
            "session_id": "fake-session",
            "event": {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text},
            },
        })
    }

    /// The terminal `result` message.
    pub fn result(subtype: &str, text: &str, is_error: bool) -> Value {
        json!({
            "type": "result",
            "subtype": subtype,
            "duration_ms": 12,
            "duration_api_ms": 7,
            "is_error": is_error,
            "num_turns": 1,
            "session_id": "fake-session",
            "total_cost_usd": 0.0001,
            "usage": {"input_tokens": 3, "output_tokens": 5},
            "result": text,
        })
    }

    /// A `can_use_tool` control request from the CLI (the permission prompt).
    pub fn permission_request(request_id: &str, tool_name: &str, input: Value) -> Value {
        json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "can_use_tool",
                "tool_name": tool_name,
                "input": input,
            },
        })
    }

    /// A `hook_callback` control request from the CLI.
    pub fn hook_callback(request_id: &str, callback_id: &str, input: Value) -> Value {
        json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "hook_callback",
                "callback_id": callback_id,
                "input": input,
            },
        })
    }

    /// A `control_response`, the shape the SDK correlates by `request_id`.
    pub fn control_response(request_id: &str, success: bool) -> Value {
        json!({
            "type": "control_response",
            "response": {
                "subtype": if success { "success" } else { "error" },
                "request_id": request_id,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// stream helpers
// ---------------------------------------------------------------------------

/// A plain user message, the way a caller drives a turn.
pub fn user(text: &str) -> InputMessage {
    InputMessage::user(text.to_string(), "fake-session".to_string())
}

/// What `Transport::receive_messages` hands back.
pub type MessageStream = std::pin::Pin<Box<dyn Stream<Item = SdkResult<Message>> + Send + 'static>>;

/// Subscribe **then** send, which is the only ordering that cannot lose output.
///
/// `receive_messages()` calls `broadcast::Sender::subscribe`, and a tokio
/// broadcast delivers nothing retroactively: anything the CLI printed before the
/// test subscribed is gone for good (see `messages_emitted_before_subscribe_are_lost`
/// in `tests/fake_cli_harness.rs`). So the deterministic shape of a scripted turn
/// is: `connect()` -> subscribe -> send the prompt -> the transcript's first
/// `await_stdin` unblocks -> output starts. Begin such transcripts with
/// [`Transcript::await_stdin`].
pub async fn start_turn(transport: &mut SubprocessTransport, prompt: &str) -> MessageStream {
    let stream = transport.receive_messages();
    transport
        .send_message(user(prompt))
        .await
        .expect("send_message to the fake CLI");
    stream
}

/// Pull messages until the terminal `result` arrives (inclusive) or the stream
/// ends. Bounded by `within` so a stuck test fails instead of hanging.
pub async fn collect_until_result(mut stream: MessageStream, within: Duration) -> Vec<Message> {
    let mut out = Vec::new();
    let _ = tokio::time::timeout(within, async {
        while let Some(item) = stream.next().await {
            match item {
                Ok(message) => {
                    let done = matches!(message, Message::Result { .. });
                    out.push(message);
                    if done {
                        break;
                    }
                },
                Err(_) => break,
            }
        }
    })
    .await;
    out
}

/// Pull at most `n` messages, or fewer if `within` elapses first.
pub async fn collect_n(mut stream: MessageStream, n: usize, within: Duration) -> Vec<Message> {
    let mut out = Vec::new();
    let _ = tokio::time::timeout(within, async {
        while out.len() < n {
            match stream.next().await {
                Some(Ok(message)) => out.push(message),
                Some(Err(_)) | None => break,
            }
        }
    })
    .await;
    out
}

/// The text of the first text block of an assistant message.
pub fn assistant_text_of(message: &Message) -> Option<String> {
    use nexus_claude::ContentBlock;
    match message {
        Message::Assistant { message, .. } => message.content.iter().find_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        }),
        _ => None,
    }
}

/// Every assistant text block in a collected stream, concatenated per message.
pub fn assistant_texts(messages: &[Message]) -> Vec<String> {
    messages.iter().filter_map(assistant_text_of).collect()
}

/// Await `condition` becoming true, polling every 10 ms, up to `within`.
pub async fn poll_until(within: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if condition() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
