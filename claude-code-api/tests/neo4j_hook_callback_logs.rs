//! What [`claude_code_api::core::hooks::Neo4jHookCallback`] says in the log, for
//! the paths whose only observable effect *is* the log.
//!
//! Three things this callback does are invisible to its caller — it returns
//! `Ok(SyncHookJSONOutput::default())` whatever happens — and invisible to Neo4j,
//! because nothing is written:
//!
//! * `init_schema` swallows every statement error with a `debug!`, so a schema
//!   that was rejected is only knowable from the log;
//! * a write Neo4j refuses is reported at `warn!` and then forgotten;
//! * `HookInput::SubagentStop` and `HookInput::PreCompact` are dropped by the
//!   catch-all arm of `execute`, with nothing but a `debug!` naming them.
//!
//! Asserting on those lines needs a **global** subscriber, and a global
//! subscriber needs its own process, which is why this is a separate test binary
//! rather than more tests in `neo4j_hook_callback_bolt.rs`.
//!
//! # Why not a thread-local subscriber
//!
//! `tracing::subscriber::with_default` cannot do this reliably. `tracing` caches
//! each callsite's interest process-wide: if any other thread reaches the same
//! `debug!` while no subscriber is installed, the callsite is cached as "never
//! enabled" and a later thread-local subscriber sees nothing.
//! `rebuild_interest_cache()` does not undo it. The symptom is a test that is
//! green alone and red under `cargo llvm-cov` or under load.
//!
//! So: one global subscriber, installed by whichever test runs first, before any
//! of these callsites has been reached; and `#[serial]` on every test, because
//! they all read the same buffer.

mod hook_bolt_fake;

use claude_code_api::core::hooks::{Neo4jHookCallback, Neo4jHookCallbackConfig};
use hook_bolt_fake::{
    FakeBolt, SESSION, callback, fire, post_tool_use, pre_tool_use, stop, support, user_prompt,
};
use nexus_claude::{HookInput, PreCompactHookInput};
use serde_json::json;
use serial_test::serial;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};
use tracing_subscriber::fmt::MakeWriter;

/// The in-memory sink the global subscriber writes to.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    /// Everything logged since the last [`Captured::reset`].
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("log buffer").clone()).expect("log lines are utf-8")
    }

    fn reset(&self) {
        self.0.lock().expect("log buffer").clear();
    }

    /// The single captured line containing `needle`, or a panic showing the whole
    /// buffer — a missing log line is otherwise indistinguishable from a wrong one.
    fn line_containing(&self, needle: &str) -> String {
        let text = self.text();
        match text.lines().find(|l| l.contains(needle)) {
            Some(line) => line.to_string(),
            None => panic!("no log line contained {needle:?}; the buffer held:\n{text}"),
        }
    }

    fn has(&self, needle: &str) -> bool {
        self.text().contains(needle)
    }
}

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Captured;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Install the process-wide subscriber (once) and hand back an empty buffer.
///
/// Must be the first statement of every test in this file: the point of a global
/// subscriber is that it is in place before any callsite in this process has had
/// its interest cached.
fn capture() -> &'static Captured {
    static SINK: OnceLock<Captured> = OnceLock::new();
    let sink = SINK.get_or_init(|| {
        let sink = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_target(true)
            .without_time()
            .with_writer(sink.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("this binary installs the only global subscriber");
        sink
    });
    sink.reset();
    sink
}

/// The guard every test in this file depends on: the subscriber really is live at
/// DEBUG, so a *missing* line below means the code did not log, not that the
/// harness swallowed it. Without this, every other assertion here could pass
/// vacuously if `capture()` silently failed.
#[tokio::test]
#[serial]
async fn the_global_subscriber_really_is_live_at_debug() {
    let log = capture();

    tracing::debug!("canary at debug");
    tracing::warn!("canary at warn");

    assert!(log.has("canary at debug"), "buffer:\n{}", log.text());
    assert!(log.has("canary at warn"));
    log.reset();
    assert_eq!(log.text(), "", "reset empties the buffer between tests");
}

// ---------------------------------------------------------------------------
// init_schema — the swallowed errors
// ---------------------------------------------------------------------------

/// `init_schema` returns `Ok(())` even when Neo4j rejects all four statements.
/// The only trace is a `debug!` per statement, and it is the error *Debug*, which
/// does not say which statement failed — so a log read at INFO, the gateway's
/// default, shows "schema initialized" and nothing else.
#[tokio::test]
#[serial]
async fn init_schema_only_whispers_the_errors_it_swallows_then_claims_success() {
    let log = capture();
    let bolt = FakeBolt::start().await;
    bolt.failing("constraints are not supported here");
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    cb.init_schema().await.expect("init_schema never fails");

    let constraint = log.line_containing("Constraint creation result");
    assert!(
        constraint.contains("DEBUG"),
        "a rejected constraint is DEBUG, not WARN: {constraint}"
    );
    assert!(
        constraint.contains("constraints are not supported here"),
        "{constraint}"
    );
    assert!(log.has("Index creation result"), "buffer:\n{}", log.text());
    let success = log.line_containing("Neo4j hook schema initialized");
    assert!(
        success.contains("INFO"),
        "and the claim of success is INFO, above the errors: {success}"
    );
    assert!(
        !log.text().contains("WARN") && !log.text().contains("ERROR"),
        "nothing above DEBUG reports the failure; see the report:\n{}",
        log.text()
    );
}

#[tokio::test]
#[serial]
async fn init_schema_announces_success_once_when_every_statement_is_accepted() {
    let log = capture();
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    cb.init_schema().await.expect("schema");

    assert!(log.has("Neo4j hook schema initialized"));
    assert!(
        !log.has("Constraint creation result"),
        "an accepted statement logs nothing:\n{}",
        log.text()
    );
    assert!(!log.has("Index creation result"));
}

// ---------------------------------------------------------------------------
// init_meilisearch_index — a log line is the whole implementation
// ---------------------------------------------------------------------------

/// The body of `init_meilisearch_index` is two comments and a `debug!`. The line
/// it emits says the index is "ready", which is the misleading part: no index was
/// created, and the one named by `INDEX_TOOL_USAGE` never is. See the report.
#[tokio::test]
#[serial]
async fn init_meilisearch_index_logs_that_the_index_is_ready_without_creating_one() {
    let log = capture();
    let bolt = FakeBolt::start().await;
    let meili = support::http_mocks::meilisearch(Vec::new(), Vec::new()).await;
    let client = support::http_mocks::meilisearch_client(&meili)
        .await
        .expect("a client over the mock");
    let before = meili.received_requests().await.unwrap_or_default().len();

    let cb = Neo4jHookCallback::new(
        bolt.graph().await,
        Some(Arc::new(client)),
        Neo4jHookCallbackConfig::default(),
    );
    cb.init_meilisearch_index().await.expect("the no-op");

    assert!(
        log.has("Meilisearch tool usage index ready"),
        "buffer:\n{}",
        log.text()
    );
    assert_eq!(
        meili.received_requests().await.unwrap_or_default().len(),
        before,
        "\"ready\" is a claim, not an action"
    );
}

/// Without a client the `if let` is skipped, so not even the claim is logged.
#[tokio::test]
#[serial]
async fn init_meilisearch_index_says_nothing_at_all_without_a_client() {
    let log = capture();
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    cb.init_meilisearch_index().await.expect("the no-op");

    assert!(
        !log.has("Meilisearch"),
        "buffer should be empty:\n{}",
        log.text()
    );
}

// ---------------------------------------------------------------------------
// PreToolUse — the verbose flag
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn pre_tool_use_names_the_tool_and_the_session_when_verbose_logging_is_on() {
    let log = capture();
    let bolt = FakeBolt::start().await;
    let cb = callback(
        bolt.graph().await,
        Neo4jHookCallbackConfig {
            log_pre_tool_use: true,
            ..Default::default()
        },
    );

    fire(
        &cb,
        &pre_tool_use("Bash", json!({"command": "ls"})),
        Some("t-1"),
    )
    .await;

    let line = log.line_containing("PreToolUse:");
    assert!(line.contains("PreToolUse: Bash"), "{line}");
    assert!(line.contains(&format!("session: {SESSION}")), "{line}");
    // The tool *input* is never logged, which is what keeps a `Bash` command out
    // of the gateway log on this path.
    assert!(!line.contains("ls"), "the input is not logged: {line}");
}

/// `log_pre_tool_use` defaults to `false`, and then a `PreToolUse` leaves no
/// trace anywhere: no node, no log line.
#[tokio::test]
#[serial]
async fn pre_tool_use_is_completely_silent_by_default() {
    let log = capture();
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    fire(&cb, &pre_tool_use("Bash", json!({})), Some("t-1")).await;

    assert!(!log.has("PreToolUse"), "buffer:\n{}", log.text());
    assert!(bolt.runs().is_empty());
}

// ---------------------------------------------------------------------------
// PostToolUse — the duration in the log
// ---------------------------------------------------------------------------

/// The companion of the `duration_ms` fix on the Neo4j side: the log used to read
/// `Stored tool usage: Bash (-1ms)` for a tool call that was never timed, a
/// sentinel an operator cannot tell from a measurement. It now says `untimed`.
#[tokio::test]
#[serial]
async fn post_tool_use_logs_an_unmeasured_call_as_untimed_not_as_minus_one() {
    let log = capture();
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    fire(&cb, &post_tool_use("Bash", json!({}), json!("ok")), None).await;

    let line = log.line_containing("Stored tool usage");
    assert!(line.contains("Stored tool usage: Bash (untimed)"), "{line}");
    assert!(
        !line.contains("-1"),
        "no sentinel in the log either: {line}"
    );
}

#[tokio::test]
#[serial]
async fn post_tool_use_logs_a_measured_call_in_milliseconds() {
    let log = capture();
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    fire(&cb, &pre_tool_use("Read", json!({})), Some("t-1")).await;
    fire(
        &cb,
        &post_tool_use("Read", json!({}), json!("ok")),
        Some("t-1"),
    )
    .await;

    let line = log.line_containing("Stored tool usage");
    let ms = line
        .split_once("Stored tool usage: Read (")
        .and_then(|(_, rest)| rest.split_once("ms)"))
        .map(|(ms, _)| ms.to_string())
        .unwrap_or_else(|| panic!("expected `Read (<n>ms)`, got {line}"));
    assert!(
        ms.parse::<u64>().is_ok(),
        "the duration is a plain integer, got {ms:?}"
    );
}

/// A refused write is the one path reported above DEBUG — and it is the only
/// trace, because `execute` still answers "continue".
#[tokio::test]
#[serial]
async fn a_refused_tool_usage_write_is_reported_at_warn_and_nowhere_else() {
    let log = capture();
    let bolt = FakeBolt::start().await;
    bolt.failing("database is read only");
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    fire(&cb, &post_tool_use("Bash", json!({}), json!("ok")), None).await;

    let line = log.line_containing("Failed to store tool usage in Neo4j");
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("database is read only"), "{line}");
    assert!(
        !log.has("Stored tool usage"),
        "and it does not also claim success:\n{}",
        log.text()
    );
}

/// The Meilisearch branch logs what it *would* do. The wording is the clearest
/// evidence that the indexing is unimplemented rather than merely untested.
#[tokio::test]
#[serial]
async fn the_meilisearch_branch_logs_what_it_would_index() {
    let log = capture();
    let bolt = FakeBolt::start().await;
    let meili = support::http_mocks::meilisearch(Vec::new(), Vec::new()).await;
    let client = support::http_mocks::meilisearch_client(&meili)
        .await
        .expect("a client over the mock");

    let cb = Neo4jHookCallback::new(
        bolt.graph().await,
        Some(Arc::new(client)),
        Neo4jHookCallbackConfig::default(),
    );
    fire(&cb, &post_tool_use("Grep", json!({}), json!("ok")), None).await;

    // Not just "Meilisearch": `MeilisearchClient::new` logs "Connecting to
    // Meilisearch at …" into the same buffer.
    let line = log.line_containing("Would index");
    assert!(
        line.contains("Would index tool usage in Meilisearch: Grep"),
        "{line}"
    );
    assert!(line.contains("DEBUG"), "{line}");
}

// ---------------------------------------------------------------------------
// UserPromptSubmit and Stop
// ---------------------------------------------------------------------------

/// The prompt itself is **not** logged, only the session id — the prompt text
/// goes to Neo4j and stays out of the gateway log.
#[tokio::test]
#[serial]
async fn user_prompt_submit_logs_the_session_but_never_the_prompt() {
    let log = capture();
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    fire(
        &cb,
        &user_prompt("mon mot de passe est dans le fichier"),
        None,
    )
    .await;

    let line = log.line_containing("Stored user prompt");
    assert!(line.contains(&format!("Stored user prompt for session: {SESSION}")));
    assert!(
        !log.has("mot de passe"),
        "the prompt must not reach the log:\n{}",
        log.text()
    );
}

#[tokio::test]
#[serial]
async fn a_refused_prompt_write_is_reported_at_warn() {
    let log = capture();
    let bolt = FakeBolt::start().await;
    bolt.failing("database is read only");
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    fire(&cb, &user_prompt("bonjour"), None).await;

    let line = log.line_containing("Failed to store user prompt in Neo4j");
    assert!(line.contains("WARN"), "{line}");
    assert!(!log.has("Stored user prompt"), "{}", log.text());
}

/// A session stop is the one success this callback reports at INFO.
#[tokio::test]
#[serial]
async fn a_stored_stop_event_is_announced_at_info() {
    let log = capture();
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    fire(&cb, &stop(true), None).await;

    let line = log.line_containing("Session stopped");
    assert!(line.contains("INFO"), "{line}");
    assert!(
        line.contains(&format!("Session stopped: {SESSION}")),
        "{line}"
    );
}

#[tokio::test]
#[serial]
async fn a_refused_stop_write_is_reported_at_warn() {
    let log = capture();
    let bolt = FakeBolt::start().await;
    bolt.failing("database is read only");
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    fire(&cb, &stop(false), None).await;

    let line = log.line_containing("Failed to store session stop event in Neo4j");
    assert!(line.contains("WARN"), "{line}");
    assert!(
        !log.has("Session stopped"),
        "a lost stop event is not announced as one:\n{}",
        log.text()
    );
}

// ---------------------------------------------------------------------------
// execute — the events the match drops
// ---------------------------------------------------------------------------

/// The catch-all arm of `execute` used to discard `SubagentStop` and `PreCompact`
/// with no trace at all. It now names the dropped event, which is the only way a
/// reader of the gateway log can tell that a `PreCompact` happened and was not
/// recorded — the module's own schema advertises `event_type: "compact"` on
/// `NexusSessionEvent`, and no code path can write it.
#[tokio::test]
#[serial]
async fn a_dropped_pre_compact_names_itself_in_the_log() {
    let log = capture();
    let bolt = FakeBolt::start().await;
    let cb = callback(bolt.graph().await, Neo4jHookCallbackConfig::default());

    let input = HookInput::PreCompact(PreCompactHookInput {
        session_id: SESSION.to_string(),
        transcript_path: "transcript.jsonl".to_string(),
        cwd: "workdir".to_string(),
        permission_mode: None,
        trigger: "auto".to_string(),
        custom_instructions: None,
    });
    fire(&cb, &input, None).await;

    let line = log.line_containing("not persisted");
    assert!(
        line.contains("Hook event not persisted by Neo4jHookCallback"),
        "{line}"
    );
    assert!(
        line.contains("PreCompact"),
        "the line must name the variant it dropped: {line}"
    );
    assert!(bolt.runs().is_empty(), "and nothing was written");
}
