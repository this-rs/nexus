//! Per-function parity fixtures for `fake_acp`: each test exercises one
//! transcript from `tests/transcripts/acp/1/sessions/parity/` through the full
//! adapter stack (health → open → send_turn → events → done), proving the
//! fixture produces the events the backend parity matrix needs.
//!
//! What this proves: the JSONL transcripts written for the parity matrix are
//! valid transcripts that `fake_acp` plays and that the ACP adapter maps to
//! the agent contract's event stream correctly.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use nexus_claude::agent::{
    AgentEvent, AgentProvider, AgentSession, CostBasis, EventStream, HealthStatus, InterruptScope,
    ModelPrice, PermissionDecision, PermissionScope, ProviderKind, ResumeToken, SessionSpec,
    TurnInput,
};
use nexus_claude::providers::acp::{AcpConfig, AcpProvider, SUPPORTED_PROTOCOL_VERSION};
use serde_json::json;
use tempfile::TempDir;

const FAKE: &str = env!("CARGO_BIN_EXE_fake_acp");
const MODEL: &str = "fake-model";

fn parity_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/transcripts/acp")
        .join(SUPPORTED_PROTOCOL_VERSION.to_string())
        .join("sessions/parity")
}

fn health_transcript() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/transcripts/acp")
        .join(SUPPORTED_PROTOCOL_VERSION.to_string())
        .join("sessions/health.jsonl")
}

struct Stage {
    cwd: TempDir,
    record: PathBuf,
}

impl Stage {
    fn new() -> Self {
        let cwd = tempfile::tempdir().expect("a temp dir");
        let record = cwd.path().join("record.jsonl");
        Self { cwd, record }
    }

    fn config(&self) -> AcpConfig {
        let mut config = AcpConfig::new("acp-parity", vec![FAKE.to_owned()]);
        config.home = self.cwd.path().join("acp-home");
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
        config.env.insert(
            "FAKE_ACP_TRANSCRIPT".to_owned(),
            health_transcript().display().to_string(),
        );
        config.env.insert(
            "FAKE_ACP_RECORD".to_owned(),
            self.record.display().to_string(),
        );
        config
    }

    /// Build a composite transcript: prelude + per-turn fixture, written to a
    /// temp file via two `include` directives.
    fn spec(&self, fixture: &str) -> SessionSpec {
        let dir = parity_dir();
        let composite = self.cwd.path().join("composite.jsonl");
        let contents = if fixture == "resume" {
            // resume.jsonl has its own handshake + session/load, no prelude needed.
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

        let mut spec = SessionSpec::new(self.cwd.path());
        spec.model = Some(MODEL.to_owned());
        spec.env.set.insert(
            "FAKE_ACP_TRANSCRIPT".to_owned(),
            composite.display().to_string(),
        );
        spec.env.set.insert(
            "FAKE_ACP_RECORD".to_owned(),
            self.record.display().to_string(),
        );
        spec
    }

    async fn learned_provider(&self) -> AcpProvider {
        let provider = AcpProvider::new(self.config());
        let health = provider.health().await;
        assert_eq!(health.status, HealthStatus::Ok, "{health:?}");
        provider
    }

    async fn open(&self, fixture: &str) -> Arc<dyn AgentSession> {
        self.learned_provider()
            .await
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

// ============================================================================
// Parity fixture tests
// ============================================================================

/// A prelude + text turn: the simplest scenario.
#[tokio::test]
async fn parity_acp_text_turn() {
    let stage = Stage::new();
    let session = stage.open("turn_text").await;
    let events = turn(&*session).await;
    assert!(
        has_text(&events, "parity text from acp"),
        "the text arrives: {events:?}"
    );
    assert!(is_done(&events), "the turn ends: {events:?}");
}

/// A tool call that needs permission (session/request_permission, allow-once).
#[tokio::test]
async fn parity_acp_tool_permission() {
    let stage = Stage::new();
    let session = stage.open("turn_tool_call").await;
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
    assert!(has_text(&events, "Done"), "text follows: {events:?}");
    assert!(is_done(&events), "the turn ends: {events:?}");
}

/// A tool call granted "always".
#[tokio::test]
async fn parity_acp_permission_always() {
    let stage = Stage::new();
    let session = stage.open("turn_permission_always").await;
    let mut stream = session
        .send_turn(TurnInput::text("go"))
        .await
        .expect("send_turn");

    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("an event in time")
            .expect("stream open");
        if let AgentEvent::PermissionAsk { ref request_id, .. } = event {
            // The fixture expects allow-always.
            session
                .answer_permission(
                    request_id,
                    PermissionDecision::Allow {
                        scope: PermissionScope::Always,
                        updated_input: None,
                    },
                )
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

    while let Some(event) = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("events continue")
    {
        events.push(event);
    }
    assert!(has_text(&events, "Read always"), "text follows: {events:?}");
    assert!(is_done(&events), "the turn ends: {events:?}");
}

/// Interruption: a turn cancelled by session/cancel.
#[tokio::test]
async fn parity_acp_interrupt() {
    let stage = Stage::new();
    let session = stage.open("turn_interrupt").await;
    let mut stream = session
        .send_turn(TurnInput::text("go"))
        .await
        .expect("send_turn");

    // Wait for a text event (the fake sends one before stalling).
    let mut got_text = false;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("event in time")
            .expect("stream open");
        if matches!(&event, AgentEvent::Delta { .. } | AgentEvent::Text { .. }) {
            got_text = true;
            break;
        }
        if event.is_terminal() {
            break;
        }
    }
    assert!(got_text, "a text event was sent before the stall");

    // Interrupt — the fixture expects session/cancel.
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

/// Compaction: a turn with very high token usage.
#[tokio::test]
async fn parity_acp_compaction() {
    let stage = Stage::new();
    let session = stage.open("turn_compaction").await;
    let events = turn(&*session).await;
    assert!(is_done(&events), "the turn ends: {events:?}");
    assert!(
        has_text(&events, "compacted"),
        "the text arrives: {events:?}"
    );
}

/// Resume: session/load replays history then a new turn.
#[tokio::test]
async fn parity_acp_resume() {
    let stage = Stage::new();
    let provider = stage.learned_provider().await;
    let token = ResumeToken::new(
        ProviderKind::Acp,
        1,
        json!({ "session_id": "sess_par_prev" }),
    );
    let session = provider
        .resume(stage.spec("resume"), token)
        .await
        .expect("resume");
    let events = turn(&*session).await;
    assert!(
        has_text(&events, "resumed ok"),
        "the resumed text: {events:?}"
    );
    assert!(is_done(&events), "the resumed turn ends: {events:?}");
}

/// A second text turn on the same session.
#[tokio::test]
async fn parity_acp_second_turn() {
    let stage = Stage::new();
    // This fixture needs prelude + turn_text + turn_second. Build a composite.
    let dir = parity_dir();
    let composite = stage.cwd.path().join("two_turns.jsonl");
    let contents = format!(
        "{{\"op\":\"include\",\"file\":\"{}\"}}\n{{\"op\":\"include\",\"file\":\"{}\"}}\n{{\"op\":\"include\",\"file\":\"{}\"}}\n",
        dir.join("prelude.jsonl").display(),
        dir.join("turn_text.jsonl").display(),
        dir.join("turn_second.jsonl").display(),
    );
    std::fs::write(&composite, contents).expect("write composite");

    let mut spec = SessionSpec::new(stage.cwd.path());
    spec.model = Some(MODEL.to_owned());
    spec.env.set.insert(
        "FAKE_ACP_TRANSCRIPT".to_owned(),
        composite.display().to_string(),
    );
    spec.env.set.insert(
        "FAKE_ACP_RECORD".to_owned(),
        stage.record.display().to_string(),
    );

    let session = stage
        .learned_provider()
        .await
        .open(spec)
        .await
        .expect("open");

    // First turn.
    let events1 = turn(&*session).await;
    assert!(
        has_text(&events1, "parity text"),
        "first turn text: {events1:?}"
    );
    assert!(is_done(&events1), "first turn done: {events1:?}");

    // Second turn.
    let events2 = turn(&*session).await;
    assert!(
        has_text(&events2, "answered second"),
        "second turn text: {events2:?}"
    );
    assert!(is_done(&events2), "second turn done: {events2:?}");
}
