//! What `query()`'s `--print` mode writes to `tracing` — including the one line
//! that must *not* say what it knows.
//!
//! This lives in its own integration test file on purpose. A thread-local
//! subscriber (`tracing::subscriber::with_default`) cannot reliably capture these
//! events: `tracing` caches each callsite's interest process-wide, so a single
//! `debug!` reached from another thread with no global subscriber installed is
//! cached as "never enabled" and no local subscriber sees it again. One file is
//! one test binary is one process, so a global subscriber installed here cannot be
//! contaminated by, or contaminate, any other test. Every test is `#[serial]`
//! because they share that one capture buffer — and three of them move process-wide
//! environment variables.

mod support;

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::Stream;
use nexus_claude::{ClaudeCodeOptions, McpServerConfig, Message, query};
use serial_test::serial;
use support::*;

type Item = nexus_claude::Result<Message>;

// ---------------------------------------------------------------------------
// capture plumbing
// ---------------------------------------------------------------------------

/// An in-memory `MakeWriter` the tests read back.
#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl CapturedLog {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log mutex")).into_owned()
    }

    /// Read and clear, so one test cannot read another's lines.
    fn drain(&self) -> String {
        let mut buffer = self.0.lock().expect("log mutex");
        let text = String::from_utf8_lossy(&buffer).into_owned();
        buffer.clear();
        text
    }
}

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log mutex").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for CapturedLog {
    type Writer = CapturedLog;
    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

static CAPTURE: OnceLock<CapturedLog> = OnceLock::new();

/// The process-wide capture buffer, installing the global subscriber on first use.
fn captured_log() -> &'static CapturedLog {
    CAPTURE.get_or_init(|| {
        let log = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(log.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("this test binary installs exactly one global subscriber");
        log
    })
}

/// A fresh capture buffer for the test about to run.
fn fresh_log() -> &'static CapturedLog {
    let log = captured_log();
    log.drain();
    log
}

/// Poll the capture until `needle` shows up, then return everything captured.
/// Needed for the lines a background task writes after the test's last `await`.
async fn log_containing(log: &CapturedLog, needle: &str, within: Duration) -> String {
    let found = poll_until(within, || log.contents().contains(needle)).await;
    let text = log.contents();
    assert!(found, "never logged {needle:?}; captured:\n{text}");
    text
}

/// Restores every variable it touched when dropped.
struct EnvGuard {
    saved: Vec<(String, Option<String>)>,
}

impl EnvGuard {
    fn set(key: &str, value: &str) -> Self {
        let saved = vec![(key.to_string(), std::env::var(key).ok())];
        // SAFETY: every test in this file is `#[serial]`, and the previous value
        // is put back on drop.
        unsafe { std::env::set_var(key, value) };
        Self { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..).rev() {
            match value {
                // SAFETY: as in `set`.
                Some(value) => unsafe { std::env::set_var(&key, value) },
                None => unsafe { std::env::remove_var(&key) },
            }
        }
    }
}

async fn drain_stream(stream: impl Stream<Item = Item>) -> Vec<Item> {
    let mut stream = Box::pin(stream);
    let mut out = Vec::new();
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(item) = stream.next().await {
            out.push(item);
        }
    })
    .await;
    assert!(ended.is_ok(), "query()'s stream never ended");
    out
}

async fn run(prompt: &str, options: ClaudeCodeOptions) -> Vec<Item> {
    let stream = query(prompt, Some(options))
        .await
        .expect("query() should have spawned the fake CLI");
    drain_stream(stream).await
}

// ---------------------------------------------------------------------------
// the redaction that matters
// ---------------------------------------------------------------------------

/// The command line is logged, and the `--mcp-config` JSON — which carries every
/// MCP server's headers and `env`, i.e. the orchestrator's credentials — is not.
///
/// This line used to be `debug!("Command: {:?}", cmd)`, whose `Debug` prints every
/// argument and every environment value verbatim. The point of the test is the
/// negative assertion: the secret-shaped value must appear nowhere in the log.
#[tokio::test]
#[serial]
async fn the_mcp_config_value_is_redacted_from_the_command_log() {
    const CANARY: &str = "valeur-sensible-de-test-ne-doit-pas-fuiter";

    let log = fresh_log();

    let mut headers = std::collections::HashMap::new();
    headers.insert("Authorization".to_string(), format!("Bearer {CANARY}"));
    let mut servers = std::collections::HashMap::new();
    servers.insert(
        "orchestrator".to_string(),
        McpServerConfig::Http {
            url: "http://127.0.0.1:1/mcp".into(),
            headers: Some(headers),
        },
    );

    let fake = Transcript::new().exit_with(0).build();
    let options = fake.options_with(
        ClaudeCodeOptions::builder()
            .mcp_servers(servers)
            .env("QUERY_LOG_SECRET", CANARY)
            .build(),
    );

    let items = run("salut", options).await;
    assert!(items.is_empty());

    let text = log.drain();
    assert!(
        text.contains("Command: program="),
        "the command was not described at all; captured:\n{text}"
    );
    assert!(
        !text.contains(CANARY),
        "the sensitive value reached the log; captured:\n{text}"
    );
    assert!(
        text.contains("\"--mcp-config\", \"<redacted "),
        "--mcp-config's value should be replaced by a byte count; captured:\n{text}"
    );
    assert!(
        text.contains("\"QUERY_LOG_SECRET\""),
        "environment variable NAMES stay visible, that is what they are for; captured:\n{text}"
    );
}

/// An explicit `options.cli_path` is announced, so a launch pointed somewhere
/// unexpected can be diagnosed from the log alone.
#[tokio::test]
#[serial]
async fn an_explicit_cli_path_is_announced() {
    let log = fresh_log();
    let fake = Transcript::new().exit_with(0).build();

    let items = run("salut", fake.options()).await;
    assert!(items.is_empty());

    let text = log.drain();
    assert!(
        text.contains("Using explicit CLI path"),
        "captured:\n{text}"
    );
    assert!(
        text.contains("Starting Claude CLI with --print mode"),
        "captured:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// what the reader tasks report
// ---------------------------------------------------------------------------

/// Everything the CLI writes to stderr is relayed at DEBUG — except blank lines,
/// which are dropped rather than logged as empty entries.
#[tokio::test]
#[serial]
async fn cli_stderr_is_relayed_except_for_blank_lines() {
    let log = fresh_log();
    let fake = Transcript::new()
        .stderr("warning: le modele est lent")
        .stderr("   ")
        .stderr("")
        .init("sess-err")
        .result_ok("fini")
        .build();

    let items = run("salut", fake.options()).await;
    assert_eq!(items.len(), 2);

    let text = log_containing(log, "Claude stderr:", Duration::from_secs(5)).await;
    assert!(text.contains("Claude stderr: warning: le modele est lent"));
    assert_eq!(
        text.matches("Claude stderr:").count(),
        1,
        "the two blank stderr lines must not be logged; captured:\n{text}"
    );
}

/// A line the JSON parser rejects is logged with both the parse error and the
/// offending line, which is the only trace it leaves — it never reaches the stream.
#[tokio::test]
#[serial]
async fn a_non_json_line_is_logged_with_the_line_that_caused_it() {
    let log = fresh_log();
    let fake = Transcript::new()
        .garbage()
        .init("sess-noise")
        .result_ok("fini")
        .build();

    let items = run("salut", fake.options()).await;
    assert_eq!(items.len(), 2, "the garbage line produced no item");

    let text = log.drain();
    assert!(text.contains("Failed to parse JSON:"), "captured:\n{text}");
    assert!(
        text.contains("this is not json at all"),
        "the offending line must be quoted; captured:\n{text}"
    );
    assert!(
        text.contains("Claude output: "),
        "every stdout line is echoed at DEBUG; captured:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// the child's fate
// ---------------------------------------------------------------------------

/// Dropping the stream early kills the CLI instead of leaving it running: the
/// cleanup task sees a live child and reaps it.
#[tokio::test]
#[serial]
async fn dropping_the_stream_early_kills_the_cli() {
    let log = fresh_log();
    let fake = Transcript::new()
        .init("sess-abandoned")
        // Long enough that the child is certainly still alive when we walk away.
        .sleep_ms(8_000)
        .result_ok("jamais lu")
        .build();

    let stream = query("salut", Some(fake.options()))
        .await
        .expect("query() should have spawned the fake CLI");
    let mut stream = Box::pin(stream);
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the init message within 5s")
        .expect("an item");
    assert!(matches!(first, Ok(Message::System { .. })));

    drop(stream);

    let text = log_containing(
        log,
        "Claude CLI process killed and cleaned up",
        Duration::from_secs(5),
    )
    .await;
    assert!(text.contains("Killing Claude CLI process on stream drop"));
}

/// When the stream was read to its end the child is already gone, and the cleanup
/// task says so instead of killing a pid it no longer owns.
#[tokio::test]
#[serial]
async fn a_stream_read_to_the_end_finds_the_cli_already_gone() {
    let log = fresh_log();
    let fake = Transcript::new()
        .init("sess-complete")
        .result_ok("fini")
        .build();

    let items = run("salut", fake.options()).await;
    assert_eq!(items.len(), 2);

    let text = log_containing(
        log,
        "Claude CLI process already exited",
        Duration::from_secs(5),
    )
    .await;
    assert!(
        !text.contains("Killing Claude CLI process on stream drop"),
        "nothing left to kill; captured:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// CLAUDE_CODE_MAX_OUTPUT_TOKENS, the inherited-environment fallback
// ---------------------------------------------------------------------------

const MAX_TOKENS: &str = "CLAUDE_CODE_MAX_OUTPUT_TOKENS";

/// Run a bare query and report what the child saw for `MAX_TOKENS`.
async fn child_max_tokens(options: ClaudeCodeOptions) -> Option<String> {
    let fake = Transcript::new().exit_with(0).build();
    let items = run("salut", fake.options_with(options)).await;
    assert!(items.is_empty());
    fake.invocation().env(MAX_TOKENS)
}

/// The option path is logged with the value actually used, i.e. after clamping.
#[tokio::test]
#[serial]
async fn the_clamped_option_value_is_the_one_logged() {
    let log = fresh_log();

    let seen = child_max_tokens(
        ClaudeCodeOptions::builder()
            .max_output_tokens(99_999)
            .build(),
    )
    .await;

    assert_eq!(seen.as_deref(), Some("32000"));
    let text = log.drain();
    assert!(
        text.contains("Setting max_output_tokens from option: 32000"),
        "captured:\n{text}"
    );
}

/// With no option set, an inherited value above the safe ceiling is overridden
/// for the child and the override is warned about.
#[tokio::test]
#[serial]
async fn an_inherited_value_above_the_ceiling_is_overridden_with_a_warning() {
    let log = fresh_log();
    let _env = EnvGuard::set(MAX_TOKENS, "99999");

    let seen = child_max_tokens(ClaudeCodeOptions::default()).await;

    assert_eq!(
        seen.as_deref(),
        Some("32000"),
        "the child must not inherit the oversized value"
    );
    let text = log.drain();
    assert!(
        text.contains("exceeds maximum safe value of 32000"),
        "captured:\n{text}"
    );
}

/// An inherited value that is not a number falls back to 8192 — a silent default
/// would hide a typo, so it is warned about too.
#[tokio::test]
#[serial]
async fn an_inherited_value_that_is_not_a_number_falls_back_to_8192() {
    let log = fresh_log();
    let _env = EnvGuard::set(MAX_TOKENS, "beaucoup");

    let seen = child_max_tokens(ClaudeCodeOptions::default()).await;

    assert_eq!(seen.as_deref(), Some("8192"));
    let text = log.drain();
    assert!(
        text.contains("Invalid CLAUDE_CODE_MAX_OUTPUT_TOKENS value: beaucoup"),
        "captured:\n{text}"
    );
}

/// An inherited value inside the safe range is left exactly as it is: no
/// override, no warning.
#[tokio::test]
#[serial]
async fn an_inherited_value_inside_the_range_is_left_untouched() {
    let log = fresh_log();
    let _env = EnvGuard::set(MAX_TOKENS, "1000");

    let seen = child_max_tokens(ClaudeCodeOptions::default()).await;

    assert_eq!(seen.as_deref(), Some("1000"));
    let text = log.drain();
    assert!(
        !text.contains("exceeds maximum safe value"),
        "captured:\n{text}"
    );
    assert!(
        !text.contains("Invalid CLAUDE_CODE_MAX_OUTPUT_TOKENS"),
        "captured:\n{text}"
    );
    assert!(
        !text.contains("Setting max_output_tokens from option"),
        "no option was set; captured:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// a caller that walks away mid-output
// ---------------------------------------------------------------------------

/// How many messages to script so the 100-slot channel certainly fills while
/// still fitting in the OS pipe buffer once the reader has taken its hundred.
const MORE_THAN_THE_CHANNEL_HOLDS: usize = 130;

/// The last stderr line of a flood transcript, used as "the fake is done".
const DONE_MARKER: &str = "transcript-complete";

/// Script more messages than the channel holds, never read one, and wait until
/// the fake has reached the end of its transcript.
fn flood(body: impl Fn(Transcript) -> Transcript) -> FakeCli {
    let mut transcript = Transcript::new().init("sess-flood");
    for _ in 0..MORE_THAN_THE_CHANNEL_HOLDS {
        transcript = body(transcript);
    }
    transcript.stderr(DONE_MARKER).build()
}

/// A caller that walks away from a *full* channel stops the reader instead of
/// leaving it parked in `send` for ever.
///
/// The channel `query()` feeds holds 100 items. Scripting 130 messages and
/// reading none leaves the reader task blocked inside `send`, which is the only
/// way to reach its "the caller is gone" branch: the pending send fails the
/// instant the stream is dropped and the reader leaves its loop without waiting
/// for EOF. The observable end state is that everything is torn down — the child
/// is reaped, not leaked.
#[tokio::test]
#[serial]
async fn abandoning_a_full_channel_of_messages_tears_everything_down() {
    let log = fresh_log();
    let fake = flood(|t| t.assistant_text("trop"));

    let stream = query("salut", Some(fake.options()))
        .await
        .expect("query() should have spawned the fake CLI");

    // Never polled, so nothing is consumed from the channel.
    log_containing(
        log,
        &format!("Claude stderr: {DONE_MARKER}"),
        Duration::from_secs(10),
    )
    .await;
    drop(stream);

    log_containing(
        log,
        "Claude CLI process already exited",
        Duration::from_secs(10),
    )
    .await;
}

/// Same, with every line a parse failure: the error items take the same full
/// channel, and the reader gives up on them the same way.
#[tokio::test]
#[serial]
async fn abandoning_a_full_channel_of_parse_errors_tears_everything_down() {
    let log = fresh_log();
    let fake = flood(Transcript::unparseable_message);

    let stream = query("salut", Some(fake.options()))
        .await
        .expect("query() should have spawned the fake CLI");

    log_containing(
        log,
        &format!("Claude stderr: {DONE_MARKER}"),
        Duration::from_secs(10),
    )
    .await;
    drop(stream);

    log_containing(
        log,
        "Claude CLI process already exited",
        Duration::from_secs(10),
    )
    .await;
}

/// KNOWN BUG, kept as a failing test on purpose.
///
/// `query_print_mode`'s stdout reader task holds the `Arc<Mutex<Child>>` across
/// `child.wait().await`, so the cleanup task cannot take that lock to kill the
/// CLI. Trigger: a caller that drops the stream while the channel is full (130
/// unread messages) **and** a CLI that then stays alive. The reader leaves its
/// loop, locks the child and blocks in `wait()`; the cleanup task blocks on the
/// mutex; nothing kills anything, and the child survives the stream for as long
/// as it pleases — here the four seconds it was told to sleep.
///
/// So the kill-on-drop guarantee only holds while the reader is *idle*, which is
/// exactly not the case when a caller gives up on a busy CLI.
///
/// Not fixed here: it needs the ownership of the child restructured — one task
/// selecting between `wait()` and the caller's departure — rather than two tasks
/// taking turns on a mutex, which is more than a test pass should change.
#[tokio::test]
#[serial]
#[ignore = "known deadlock: the stdout reader holds the child mutex across wait()"]
async fn dropping_a_full_stream_should_still_kill_a_lingering_cli() {
    let log = fresh_log();
    let mut transcript = Transcript::new().init("sess-lingering");
    for _ in 0..MORE_THAN_THE_CHANNEL_HOLDS {
        transcript = transcript.assistant_text("trop");
    }
    let fake = transcript
        .stderr(DONE_MARKER)
        .sleep_ms(4_000)
        .result_ok("jamais lu")
        .build();

    let stream = query("salut", Some(fake.options()))
        .await
        .expect("query() should have spawned the fake CLI");

    log_containing(
        log,
        &format!("Claude stderr: {DONE_MARKER}"),
        Duration::from_secs(10),
    )
    .await;
    drop(stream);

    let killed = poll_until(Duration::from_secs(2), || {
        log.contents()
            .contains("Claude CLI process killed and cleaned up")
    })
    .await;
    assert!(
        killed,
        "the lingering CLI was never killed; captured:\n{}",
        log.contents()
    );
}
