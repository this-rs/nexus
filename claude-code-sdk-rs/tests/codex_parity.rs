//! Per-function parity fixtures for `fake_codex`: each test exercises one
//! transcript from `tests/transcripts/codex/0.130.0/sessions/parity/` through
//! the full adapter stack (open → send_turn → events → done), proving the
//! fixture produces the events the backend parity matrix needs.
//!
//! What this proves: the JSONL transcripts written for the parity matrix are
//! valid transcripts that `fake_codex` plays and that the Codex adapter maps to
//! the agent contract's event stream correctly.

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use nexus_claude::agent::{
    AgentEvent, AgentProvider, AgentSession, CostBasis, EventStream, InterruptScope, ModelPrice,
    PermissionDecision, SessionSpec, TurnInput,
};
use nexus_claude::providers::codex::{CodexConfig, CodexProvider, MIN_APP_SERVER_VERSION};
use std::sync::Arc;
use tempfile::TempDir;

const FAKE: &str = env!("CARGO_BIN_EXE_fake_codex");
const MODEL: &str = "gpt-fake";

fn parity_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/transcripts/codex")
        .join(MIN_APP_SERVER_VERSION)
        .join("sessions/parity")
}

fn config(home: &std::path::Path) -> CodexConfig {
    let mut config = CodexConfig::new("codex-parity");
    config.program = PathBuf::from(FAKE);
    config.codex_home = home.to_path_buf();
    config.default_model = Some(MODEL.to_owned());
    config.cost_basis = CostBasis::Priced;
    config.prices = nexus_claude::model::PriceTable::new().with(
        MODEL,
        ModelPrice {
            input_per_mtok: 1.0,
            output_per_mtok: 2.0,
            cache_read_per_mtok: Some(0.5),
            cache_write_per_mtok: None,
        },
    );
    config
}

struct Stage {
    _cwd: TempDir,
    home: TempDir,
    record: PathBuf,
}

impl Stage {
    fn new() -> Self {
        let cwd = tempfile::tempdir().expect("a temp dir");
        let home = tempfile::tempdir().expect("a temp dir");
        let record = cwd.path().join("record.jsonl");
        Self {
            _cwd: cwd,
            home,
            record,
        }
    }

    /// Build a composite transcript: prelude + per-turn fixture, written to a
    /// temp file via two `include` directives.
    fn spec(&self, fixture: &str) -> SessionSpec {
        let dir = parity_dir();
        let composite = self._cwd.path().join("composite.jsonl");
        let contents = if fixture == "resume" {
            // resume.jsonl has its own handshake, no prelude needed.
            format!(
                "{{\"op\":\"include\",\"file\":\"{}\"}}\n",
                dir.join(format!("{fixture}.jsonl")).display()
            )
        } else {
            format!(
                "{{\"op\":\"include\",\"file\":\"{}\"}}\n{{\"op\":\"include\",\"file\":\"{}\"}}\n",
                dir.join("prelude.jsonl").display(),
                dir.join(format!("{fixture}.jsonl")).display(),
            )
        };
        std::fs::write(&composite, contents).expect("write composite transcript");

        let mut spec = SessionSpec::new(self._cwd.path());
        spec.model = Some(MODEL.to_owned());
        spec.env.set.insert(
            "FAKE_CODEX_TRANSCRIPT".to_owned(),
            composite.display().to_string(),
        );
        spec.env.set.insert(
            "FAKE_CODEX_RECORD".to_owned(),
            self.record.display().to_string(),
        );
        spec
    }

    fn provider(&self) -> CodexProvider {
        CodexProvider::new(config(&self.home.path().join("codex-home")))
    }

    async fn open(&self, fixture: &str) -> Arc<dyn AgentSession> {
        self.provider()
            .open(self.spec(fixture))
            .await
            .unwrap_or_else(|e| panic!("open({fixture}): {e:#}"))
    }
}

async fn collect(mut stream: EventStream) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("the turn stream makes progress")
    {
        events.push(event);
    }
    events
}

async fn turn(session: &dyn AgentSession) -> Vec<AgentEvent> {
    collect(
        session
            .send_turn(TurnInput::text("go"))
            .await
            .expect("send_turn"),
    )
    .await
}

fn has_text(events: &[AgentEvent], needle: &str) -> bool {
    events.iter().any(|e| match e {
        AgentEvent::Text { text, .. } => text.contains(needle),
        AgentEvent::Delta { text, .. } => text.contains(needle),
        _ => false,
    })
}

fn is_done(events: &[AgentEvent]) -> bool {
    events
        .last()
        .is_some_and(|e| matches!(e, AgentEvent::Done { .. }))
}

fn has_tool_call(events: &[AgentEvent], tool: &str) -> bool {
    events.iter().any(|e| match e {
        AgentEvent::ToolCall { name, .. } => name == tool,
        _ => false,
    })
}

fn has_tool_result(events: &[AgentEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolResult { .. }))
}

fn has_compaction(events: &[AgentEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, AgentEvent::Compaction { .. }))
}

// ============================================================================
// Parity fixture tests
// ============================================================================

/// A prelude + text turn: the simplest scenario, proving the adapter maps a
/// basic turn to the contract's event stream.
#[tokio::test]
async fn parity_text_turn() {
    let stage = Stage::new();
    let session = stage.open("turn_text").await;
    let events = turn(&*session).await;
    assert!(
        has_text(&events, "parity text"),
        "the text arrives: {events:?}"
    );
    assert!(is_done(&events), "the turn ends: {events:?}");
    // Token usage is in the Done event's `usage` field.
    if let Some(AgentEvent::Done { usage, .. }) = events.last() {
        assert!(
            usage.output_tokens.unwrap_or(0) > 0,
            "token usage is reported: {usage:?}"
        );
    }
}

/// An MCP tool call: the adapter maps mcpToolCall items to ToolCall + ToolResult.
#[tokio::test]
async fn parity_mcp_tool() {
    let stage = Stage::new();
    let session = stage.open("turn_mcp_tool").await;
    let events = turn(&*session).await;
    assert!(
        has_tool_call(&events, "echo") || has_tool_result(&events),
        "the tool call is mapped: {events:?}"
    );
    assert!(
        has_text(&events, "tool done"),
        "the message follows: {events:?}"
    );
    assert!(is_done(&events), "the turn ends: {events:?}");
}

/// A tool call that needs permission: the adapter maps the elicitation/request
/// to a PermissionAsk event, and the answer unblocks the tool.
#[tokio::test]
async fn parity_permission() {
    let stage = Stage::new();
    let session = stage.open("turn_permission").await;
    let mut stream = session
        .send_turn(TurnInput::text("go"))
        .await
        .expect("send_turn");

    // Collect until we see a permission ask.
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("an event in time")
            .expect("stream open");
        if let AgentEvent::PermissionAsk { ref request_id, .. } = event {
            // Answer the permission — the fixture expects {action:"accept"}.
            session
                .answer_permission(request_id, PermissionDecision::allow_once())
                .await
                .expect("answer_permission");
            events.push(event);
            break;
        }
        assert!(
            !event.is_terminal(),
            "turn ended before permission ask: {events:?}"
        );
        events.push(event);
    }

    // Collect the rest.
    while let Some(event) = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("events continue")
    {
        events.push(event);
    }
    assert!(
        has_text(&events, "Permission granted"),
        "text follows: {events:?}"
    );
    assert!(is_done(&events), "the turn ends: {events:?}");
}

/// Interruption: a stalling turn is interrupted by the client.
#[tokio::test]
async fn parity_interrupt() {
    let stage = Stage::new();
    let session = stage.open("turn_interrupt").await;
    let mut stream = session
        .send_turn(TurnInput::text("go"))
        .await
        .expect("send_turn");

    // Wait for a delta (the fake sends one before stalling).
    let mut got_delta = false;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("event in time")
            .expect("stream open");
        if matches!(&event, AgentEvent::Delta { .. } | AgentEvent::Text { .. }) {
            got_delta = true;
            break;
        }
        if event.is_terminal() {
            break;
        }
    }
    assert!(got_delta, "a delta was sent before the stall");

    // Interrupt — the fixture expects turn/interrupt.
    let outcome = session.interrupt(InterruptScope::TurnOnly).await;
    assert!(outcome.is_ok(), "interrupt accepted: {outcome:?}");

    // Collect the rest until done.
    let mut done = false;
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
        if event.is_terminal() {
            done = true;
            break;
        }
    }
    assert!(done, "the turn ended after interruption");
}

/// Compaction: a turn whose token usage triggers contextCompaction items.
#[tokio::test]
async fn parity_compaction() {
    let stage = Stage::new();
    let session = stage.open("turn_compaction").await;
    let events = turn(&*session).await;
    assert!(
        is_done(&events),
        "the turn with compaction ends: {events:?}"
    );
    assert!(
        has_text(&events, "compacted"),
        "the text arrives: {events:?}"
    );
    assert!(
        has_compaction(&events),
        "a compaction event was emitted: {events:?}"
    );
}

/// Sub-agent (collabToolCall): the adapter maps the collab item to a sub-agent
/// event with child text and a terminal result.
#[tokio::test]
async fn parity_collab_tool() {
    let stage = Stage::new();
    let session = stage.open("turn_collab").await;
    let events = turn(&*session).await;
    assert!(is_done(&events), "the turn with collab ends: {events:?}");
    // The collab tool call arrives as a ToolCall or TaskUpdate.
    assert!(
        has_text(&events, "sub-agent")
            || has_text(&events, "The sub-agent reported")
            || has_text(&events, "child found the spec"),
        "the sub-agent result arrives: {events:?}"
    );
}

/// Auto-continue: a turn that ends on `max_output_tokens`, and the caller
/// (the backend) sends a second turn to continue. The fixture expects two
/// `turn/start` requests.
#[tokio::test]
async fn parity_auto_continue() {
    let stage = Stage::new();
    let session = stage.open("turn_auto_continue").await;

    // First turn: tool calls, ends on max_output_tokens.
    let events1 = turn(&*session).await;
    assert!(is_done(&events1), "the first turn ends: {events1:?}");

    // Second turn: the continuation.
    let events2 = turn(&*session).await;
    assert!(is_done(&events2), "the continued turn ends: {events2:?}");
    assert!(
        has_text(&events2, "continued"),
        "the continuation text: {events2:?}"
    );
}

/// Large tool result: a turn with a tool call returning a very large result.
#[tokio::test]
async fn parity_large_result() {
    let stage = Stage::new();
    let session = stage.open("turn_large_result").await;
    let events = turn(&*session).await;
    assert!(
        is_done(&events),
        "the turn with large result ends: {events:?}"
    );
    assert!(
        has_text(&events, "answered huge"),
        "the message after huge result: {events:?}"
    );
}

/// Resume: a new session on a resumed transcript (re-initialize + thread/start).
#[tokio::test]
async fn parity_resume() {
    let stage = Stage::new();
    let session = stage.open("resume").await;
    let events = turn(&*session).await;
    assert!(
        has_text(&events, "resumed ok"),
        "the resumed text: {events:?}"
    );
    assert!(is_done(&events), "the resumed turn ends: {events:?}");
}
