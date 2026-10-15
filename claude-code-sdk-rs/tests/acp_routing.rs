//! Routing non-regression for the ACP provider: the multi-provider router's entry
//! point `ProviderRegistry::open_session` drives `fake_acp`, survives a provider move,
//! and the wire the adapter writes stays conformant across the transition.
//!
//! The existing `acp_registry.rs` drives `registry.get(...).open(...)` directly and
//! never moves an instance; this file drives the router (`open_session`, the path a
//! host actually takes) and proves the wire shape does not change when the instance is
//! removed and re-added under another id.
//!
//! It reads the committed wire golden (`tests/snapshots/acp_wire_v1.json`, written by
//! `acp_wire_snapshots.rs`): the permission answer is compared byte for byte, and the
//! `session/new` / `session/prompt` lines are compared by shape after the values that
//! vary per run (cwd, session id, prompt text, the empty MCP-server list) are aligned.
//! Like the rest of the ACP tests this runs against `fake_acp`; no real ACP agent was
//! involved.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use nexus_claude::agent::{
    AgentEvent, AgentSession, CostBasis, EnvCredentialResolver, EventStream, ModelBinding,
    ModelPrice, ModelProtocol, ModelProviderConfig, PermissionDecision, ProviderError,
    ProviderInstanceConfig, ProviderKind, ProviderRegistry, SecurityGate, SessionSpec, StopReason,
    TurnInput,
};
use nexus_claude::providers::acp::SUPPORTED_PROTOCOL_VERSION;
use nexus_claude::providers::acp::wire;
use serde_json::{Value, json};
use tempfile::TempDir;

const FAKE: &str = env!("CARGO_BIN_EXE_fake_acp");
const MODEL: &str = "fake-model";

fn transcript(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/transcripts/acp")
        .join(SUPPORTED_PROTOCOL_VERSION.to_string())
        .join("sessions")
        .join(format!("{name}.jsonl"))
        .display()
        .to_string()
}

fn price() -> ModelPrice {
    ModelPrice {
        input_per_mtok: 1.0,
        output_per_mtok: 2.0,
        cache_read_per_mtok: None,
        cache_write_per_mtok: None,
    }
}

/// One staged session: the instance the registry routes, and where the fake records
/// what it saw. Mirrors `acp_conformance.rs::Staging`, but through the router.
struct Staging {
    cwd: TempDir,
    record: PathBuf,
}

impl Staging {
    fn new() -> Self {
        let cwd = tempfile::tempdir().expect("a temp dir");
        let record = cwd.path().join("record.jsonl");
        Self { cwd, record }
    }

    /// A `ProviderInstanceConfig` for `fake_acp` playing `health` (the transcript the
    /// handshake runs), with the agent's dedicated HOME under this staging's temp dir.
    fn instance(&self, id: &str) -> ProviderInstanceConfig {
        ProviderInstanceConfig::new(id, ProviderKind::Acp)
            .with_command([FAKE, "--stdio"])
            .with_default_model(MODEL)
            .with_cost_source(CostBasis::Priced)
            .with_price(MODEL, price())
            .with_extension(
                "env",
                json!({ "FAKE_ACP_TRANSCRIPT": transcript("health") }),
            )
            .with_extension("thinking", Value::Bool(true))
            .with_extension(
                "acp_home",
                json!(self.cwd.path().join("acp-home").display().to_string()),
            )
    }

    /// A session playing `name`, recorded to this staging's record path.
    fn spec(&self, name: &str) -> SessionSpec {
        let mut spec = SessionSpec::new(self.cwd.path());
        spec.model = Some(MODEL.to_owned());
        spec.env
            .set
            .insert("FAKE_ACP_TRANSCRIPT".to_owned(), transcript(name));
        spec.env.set.insert(
            "FAKE_ACP_RECORD".to_owned(),
            self.record.display().to_string(),
        );
        spec
    }

    /// Every line the fake recorded, parsed.
    fn recorded(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.record)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// The recorded inbound requests of `method`.
    fn requests(&self, method: &str) -> Vec<Value> {
        self.recorded()
            .into_iter()
            .filter(|entry| entry["kind"] == "in" && entry["method"] == method)
            .collect()
    }

    /// The raw answers the fake received to its requests.
    fn responses(&self) -> Vec<String> {
        self.recorded()
            .into_iter()
            .filter(|entry| entry["kind"] == "response")
            .map(|entry| entry["raw"].as_str().unwrap_or_default().to_owned())
            .collect()
    }
}

/// The committed ACP wire golden, keyed by the supported protocol version.
fn golden() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(format!("acp_wire_v{SUPPORTED_PROTOCOL_VERSION}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "the ACP wire golden is missing at {}: {error} (regenerate with \
             UPDATE_ACP_WIRE_SNAPSHOTS=1 cargo test -p nexus-claude --features provider-acp \
             --test acp_wire_snapshots)",
            path.display()
        )
    });
    serde_json::from_str(&text).expect("the golden is JSON")
}

/// A recorded `params` keeps the exact wire shape of the golden `outbound` line: same
/// field names and nesting (key order is enforced only by the golden byte test, not
/// here — the comparison is an order-insensitive `serde_json::Value`), with only the
/// values that vary per run (the `cwd`, the `sessionId`, the prompt `text`, and the
/// empty `mcpServers` of a server-less session) aligned first. The golden locks the
/// shape; this test locks that the shape survived a provider move.
fn params_match_golden(golden_line: &str, recorded: &Value) -> bool {
    let golden: Value = serde_json::from_str(golden_line).expect("the golden line is JSON");
    let mut expected = golden["params"].clone();
    for pointer in ["/cwd", "/sessionId", "/mcpServers", "/prompt/0/text"] {
        if let Some(value) = recorded.pointer(pointer)
            && let Some(slot) = expected.pointer_mut(pointer)
        {
            *slot = value.clone();
        }
    }
    expected == *recorded
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

fn done(events: &[AgentEvent]) -> &AgentEvent {
    events
        .last()
        .filter(|event| event.is_terminal())
        .expect("a terminal event")
}

fn stop_of(events: &[AgentEvent]) -> StopReason {
    match done(events) {
        AgentEvent::Done { stop_reason, .. } => *stop_reason,
        other => panic!("not a done: {other:?}"),
    }
}

/// Waits (bounded) for the stream's next event matching `wanted`.
async fn next_matching(
    stream: &mut EventStream,
    wanted: impl Fn(&AgentEvent) -> bool,
) -> AgentEvent {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("an event in time")
            .expect("the stream is open");
        if wanted(&event) {
            return event;
        }
    }
}

/// Runs a turn, answers the one `permission_ask` with `allow_once`, and returns the ask
/// and the rest of the turn.
async fn answer_allow_once(session: &dyn AgentSession) -> (AgentEvent, Vec<AgentEvent>) {
    let mut stream = session
        .send_turn(TurnInput::text("go"))
        .await
        .expect("send_turn");
    let ask = next_matching(&mut stream, |event| {
        matches!(event, AgentEvent::PermissionAsk { .. })
    })
    .await;
    let AgentEvent::PermissionAsk { request_id, .. } = &ask else {
        unreachable!()
    };
    session
        .answer_permission(request_id, PermissionDecision::allow_once())
        .await
        .expect("answered");
    let events = collect(stream).await;
    (ask, events)
}

/// Every request the adapter wrote is one the versioned schema describes, and the two
/// request lines whose scalars vary per run still match the golden's shape.
fn assert_wire_conformant(staging: &Staging, gold: &Value) {
    let mut checked = 0;
    for entry in staging.recorded() {
        if entry["kind"] != "in" {
            continue;
        }
        let method = entry["method"].as_str().unwrap_or_default();
        let kind = format!("{method}.params");
        if let Err(reason) = wire::validate_kind(&kind, &entry["params"]) {
            panic!("the adapter wrote a `{method}` the schema refuses: {reason}\n{entry}");
        }
        checked += 1;
    }
    assert!(checked >= 3, "only {checked} requests were checked");

    assert_eq!(
        staging.requests("session/new").len(),
        1,
        "one session/new per session"
    );
    assert_eq!(
        staging.requests("session/prompt").len(),
        1,
        "one session/prompt per turn"
    );
    for entry in staging.requests("session/new") {
        let golden_line = gold["outbound"]["session/new.params"]
            .as_str()
            .expect("the golden holds session/new.params");
        assert!(
            params_match_golden(golden_line, &entry["params"]),
            "session/new drifted from the golden across the move:\n{entry}"
        );
    }
    for entry in staging.requests("session/prompt") {
        let golden_line = gold["outbound"]["session/prompt.params"]
            .as_str()
            .expect("the golden holds session/prompt.params");
        assert!(
            params_match_golden(golden_line, &entry["params"]),
            "session/prompt drifted from the golden across the move:\n{entry}"
        );
    }
}

// ---------------------------------------------------------------------------
// The router and a provider move
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registry_open_session_routes_an_acp_instance_and_stays_conformant_across_a_provider_move()
{
    let first = Staging::new();
    let second = Staging::new();
    let registry = ProviderRegistry::new(Arc::new(EnvCredentialResolver));
    registry.activate_security_gate(SecurityGate::attest("acp-test"));

    // Route through the router's entry point, not `get(...).open(...)`.
    registry.upsert(first.instance("acp-prod")).unwrap();
    let session = registry
        .open_session("acp-prod", first.spec("plain"))
        .await
        .expect("the router opens the acp instance");
    assert_eq!(stop_of(&turn(&*session).await), StopReason::Completed);
    session.close().await.unwrap();

    // The move: the old id is gone, the new one routes.
    assert!(registry.remove("acp-prod"));
    registry.upsert(second.instance("acp-prod-moved")).unwrap();
    assert!(registry.get("acp-prod").is_err(), "the old id is gone");
    assert!(registry.get("acp-prod-moved").is_ok());

    // Route again and answer the permission exactly as the golden says.
    let session = registry
        .open_session("acp-prod-moved", second.spec("permission_accordee"))
        .await
        .expect("the moved instance routes");
    let (ask, events) = answer_allow_once(&*session).await;
    assert!(matches!(ask, AgentEvent::PermissionAsk { .. }));
    assert_eq!(stop_of(&events), StopReason::Completed);
    let gold = golden();
    let permission_answer = gold["outbound"]["session/request_permission.response.selected"]
        .as_str()
        .expect("the golden holds the selected permission answer");
    assert_eq!(second.responses(), [permission_answer]);
    session.close().await.unwrap();

    // Wire conformance across the transition, for both stagings.
    assert_wire_conformant(&first, &gold);
    assert_wire_conformant(&second, &gold);
}

// ---------------------------------------------------------------------------
// The routing fallback: a model binding on an agent that picks its own model
// ---------------------------------------------------------------------------

/// An ACP agent chooses its own model: a model binding is refused **before anything
/// starts**. The refusal is `ModelProtocolMismatch` (with an empty `accepts`): the
/// router's pair check (`open_session` calls `check_pair` before its per-kind
/// `Unsupported { model_binding }` arm) runs first, and an ACP harness accepts no
/// protocol at all. This is the same error `tests/model_binding.rs` already pins for
/// `acp-h x anthropic/deepseek`.
#[tokio::test]
async fn an_acp_instance_refuses_a_model_binding_and_spawns_nothing() {
    let staging = Staging::new();
    let registry = ProviderRegistry::new(Arc::new(EnvCredentialResolver));
    registry.activate_security_gate(SecurityGate::attest("acp-test"));
    registry.upsert(staging.instance("acp-prod")).unwrap();
    registry
        .upsert_model_provider(
            ModelProviderConfig::new("deepseek", ModelProtocol::OpenAiChat)
                .with_endpoint("https://api.deepseek.com/v1"),
        )
        .unwrap();

    let mut spec = staging.spec("plain");
    spec.model_binding = Some(ModelBinding::new("deepseek"));
    let error = registry
        .open_session("acp-prod", spec)
        .await
        .err()
        .expect("a binding on an acp agent is refused");
    let ProviderError::ModelProtocolMismatch(mismatch) = &error else {
        panic!("acp x model provider must be a protocol mismatch, got {error:?}");
    };
    assert_eq!(mismatch.harness, "acp-prod");
    assert_eq!(mismatch.provider, "deepseek");
    assert_eq!(mismatch.protocol, "openai_chat");
    assert!(mismatch.accepts.is_empty(), "acp accepts no protocol");
    assert_eq!(error.kind(), "model_protocol_mismatch");
    // The fallback spawns nothing: no agent process, no wire line.
    assert!(
        staging.recorded().is_empty(),
        "the refusal must not start the agent"
    );
}
