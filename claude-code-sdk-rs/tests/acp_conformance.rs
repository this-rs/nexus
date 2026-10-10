//! The ACP adapter (`providers::acp`) against the conformance suite of the agent
//! contract, and the tests of what is specific to it.
//!
//! **What this proves and what it does not.** `AcpProvider` launches `fake_acp`, a fake
//! executable replaying a JSONL transcript (one per scenario) written from the public
//! specification of the Agent Client Protocol (version 1). Nothing here talked to a real
//! ACP agent: opencode and the Gemini CLI were never executed. A pass says the adapter
//! implements the protocol *as the specification describes it*, and the contract on top
//! of it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use nexus_claude::agent::{
    AgentEvent, AgentProvider, AgentSession, CancelScope, CostBasis, EventStream, HealthStatus,
    HookSupport, InterruptScope, McpServerSpec, ModelPrice, PermissionDecision, PermissionScope,
    PolicyMode, ProviderError, ProviderKind, ResumeToken, SandboxLevel, SessionSpec, StopReason,
    SubagentSupport, ToolCategory, ToolPolicy, TurnInput,
};
use nexus_claude::providers::acp::wire;
use nexus_claude::providers::acp::{AcpConfig, AcpProvider, SUPPORTED_PROTOCOL_VERSION};
use nexus_claude::testkit::conformance::{
    ConformanceTarget, Prepared, Scenario, ScenarioOutcome, run_all,
};
use serde_json::{Value, json};
use tempfile::TempDir;

const FAKE: &str = env!("CARGO_BIN_EXE_fake_acp");
const MODEL: &str = "fake-model";
/// A key-shaped value that must never reach argv, a log, an event or an error.
const CANARY: &str = "sk-canary-Zq81mLpWx39vNbR2kT";

fn sessions_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/transcripts/acp")
        .join(SUPPORTED_PROTOCOL_VERSION.to_string())
        .join("sessions")
}

fn transcript(name: &str) -> PathBuf {
    sessions_dir().join(format!("{name}.jsonl"))
}

fn price() -> ModelPrice {
    ModelPrice {
        input_per_mtok: 1.0,
        output_per_mtok: 2.0,
        cache_read_per_mtok: Some(0.5),
        cache_write_per_mtok: None,
    }
}

/// One staged session: a spec for `fake_acp` playing a transcript, and where it records
/// what it saw.
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

    /// A configuration whose `health()` plays `health_transcript`.
    fn config_with(&self, health_transcript: &str) -> AcpConfig {
        let mut config = AcpConfig::new("acp-test", vec![FAKE.to_owned()]);
        // The agent's dedicated HOME lives in the test's temp dir, not the data dir.
        config.home = self.cwd.path().join("acp-home");
        config.default_model = Some(MODEL.to_owned());
        config.cost_basis = CostBasis::Priced;
        config.prices = nexus_claude::model::PriceTable::new().with(MODEL, price());
        config.thinking = true;
        config.env.insert(
            "FAKE_ACP_TRANSCRIPT".to_owned(),
            transcript(health_transcript).display().to_string(),
        );
        config.env.insert(
            "FAKE_ACP_RECORD".to_owned(),
            self.record.display().to_string(),
        );
        // What the test knows as a secret: the fake records whether it *saw* it.
        config
            .env
            .insert("FAKE_ACP_CANARY".to_owned(), CANARY.to_owned());
        // Under `cargo llvm-cov`, the fake agent writes its profile where this variable
        // says; the allowlist would drop it and every line of `fake_acp` the suite runs
        // would be counted as never run. Absent outside coverage: inheriting it is a no-op.
        config.env_inherit.push("LLVM_PROFILE_FILE".to_owned());
        config
    }

    fn config(&self) -> AcpConfig {
        self.config_with("health")
    }

    fn spec(&self, name: &str) -> SessionSpec {
        let mut spec = SessionSpec::new(self.cwd.path());
        spec.model = Some(MODEL.to_owned());
        spec.env.set.insert(
            "FAKE_ACP_TRANSCRIPT".to_owned(),
            transcript(name).display().to_string(),
        );
        spec.env.set.insert(
            "FAKE_ACP_RECORD".to_owned(),
            self.record.display().to_string(),
        );
        spec
    }

    /// A provider that has learned what the agent offers (through `health`).
    async fn learned_provider(&self) -> AcpProvider {
        let provider = AcpProvider::new(self.config());
        let health = provider.health().await;
        assert_eq!(health.status, HealthStatus::Ok, "{health:?}");
        provider
    }

    async fn open(&self, name: &str) -> Arc<dyn AgentSession> {
        self.learned_provider()
            .await
            .open(self.spec(name))
            .await
            .expect("the session opens")
    }

    /// Every line the fake recorded, parsed.
    fn recorded(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.record)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn requests(&self, method: &str) -> Vec<Value> {
        self.recorded()
            .into_iter()
            .filter(|entry| entry["kind"] == "in" && entry["method"] == method)
            .collect()
    }

    /// The `start` record of the session process (the last one: `health` starts one too).
    fn start(&self) -> Value {
        self.recorded()
            .into_iter()
            .rfind(|entry| entry["kind"] == "start")
            .expect("the fake recorded its start")
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

// ---------------------------------------------------------------------------
// The conformance suite
// ---------------------------------------------------------------------------

fn transcript_of(scenario: Scenario) -> &'static str {
    match scenario {
        Scenario::Raisonnement => "raisonnement",
        Scenario::AppelOutilResultat => "appel_outil_resultat",
        Scenario::OutilsParalleles => "outils_paralleles",
        Scenario::PermissionAccordee => "permission_accordee",
        Scenario::PermissionRefusee => "permission_refusee",
        Scenario::PermissionHorsTour => "permission_hors_tour",
        Scenario::InterruptionEnFlux => "interruption_en_flux",
        Scenario::InterruptionOutil => "interruption_outil",
        Scenario::ChangementPolitique => "changement_politique",
        Scenario::Reprise => "reprise",
        Scenario::TourConcurrent => "tour_concurrent",
        Scenario::ErreurRetryable => "erreur_retryable",
        Scenario::ProcessusMort => "processus_mort",
        Scenario::FermetureIdempotente => "fermeture_idempotente",
        // A plain turn: the text turns, and the fallbacks of absent capabilities.
        _ => "plain",
    }
}

struct AcpTarget {
    base: Arc<AcpProvider>,
    stagings: Mutex<Vec<Staging>>,
}

impl AcpTarget {
    async fn new() -> Self {
        let staging = Staging::new();
        let base = Arc::new(AcpProvider::new(staging.config()));
        // What the agent offers is learned, never assumed.
        assert_eq!(base.health().await.status, HealthStatus::Ok);
        Self {
            base,
            stagings: Mutex::new(vec![staging]),
        }
    }

    /// What every `fake_acp` of the run recorded.
    fn records(&self) -> Vec<Value> {
        self.stagings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .flat_map(Staging::recorded)
            .collect()
    }
}

#[async_trait]
impl ConformanceTarget for AcpTarget {
    fn name(&self) -> &str {
        "acp (AcpProvider on fake_acp, JSONL transcripts from the ACP specification)"
    }

    fn provider(&self) -> Arc<dyn AgentProvider> {
        self.base.clone()
    }

    async fn prepare(&self, scenario: Scenario) -> Option<Prepared> {
        let staging = Staging::new();
        let provider = AcpProvider::new(staging.config());
        assert_eq!(provider.health().await.status, HealthStatus::Ok);
        let mut spec = staging.spec(transcript_of(scenario));
        spec.deltas = true;
        let mut prepared = Prepared::new(Arc::new(provider), spec);
        if scenario == Scenario::Reprise {
            prepared.resume = Some(ResumeToken::new(
                ProviderKind::Acp,
                1,
                json!({ "session_id": "sess_prev" }),
            ));
        }
        self.stagings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(staging);
        Some(prepared)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_acp_adapter_passes_the_conformance_suite() {
    let target = AcpTarget::new().await;
    let report = run_all(&target).await;
    eprintln!("{}", report.summary());
    report.assert_conformant();

    // What ACP does not have is proven through its written fallback (§5).
    for absent in [
        Scenario::QuestionUtilisateur,
        Scenario::AnnulationTourPreserve,
        Scenario::AnnulationTache,
        Scenario::ChangementModele,
        Scenario::MessageImages,
        Scenario::SousAgent,
        Scenario::Compaction,
    ] {
        assert_eq!(
            report.outcome(absent),
            Some(&ScenarioOutcome::FallbackVerified),
            "{absent} must verify its fallback\n{}",
            report.summary()
        );
    }
    // What it has is played, not skipped.
    for present in [
        Scenario::TourTexteSimple,
        Scenario::FluxDeltas,
        Scenario::Raisonnement,
        Scenario::AppelOutilResultat,
        Scenario::OutilsParalleles,
        Scenario::PermissionAccordee,
        Scenario::PermissionRefusee,
        Scenario::PermissionHorsTour,
        Scenario::InterruptionEnFlux,
        Scenario::InterruptionOutil,
        Scenario::ChangementPolitique,
        Scenario::Reprise,
        Scenario::HorsTour,
        Scenario::TourConcurrent,
        Scenario::ErreurRetryable,
        Scenario::FinUsageCout,
        Scenario::ProcessusMort,
        Scenario::FermetureIdempotente,
    ] {
        assert_eq!(
            report.outcome(present),
            Some(&ScenarioOutcome::Passed),
            "{present} must pass for real\n{}",
            report.summary()
        );
    }
    let caps = &report.capabilities;
    assert!(caps.resume && caps.interactive_permissions && caps.per_session_mcp && caps.tools);
    assert!(caps.thinking, "declared by the configuration of the target");
    assert!(!caps.tool_cancel && !caps.images && !caps.native_question && !caps.background_tasks);
    assert!(!caps.set_model_live && !caps.compaction_signal);
    assert_eq!(caps.hooks, HookSupport::None);
    assert_eq!(caps.subagents, SubagentSupport::None);
    assert_eq!(caps.sandbox, SandboxLevel::None);
    assert_eq!(
        caps.permission_scopes,
        vec![PermissionScope::Once, PermissionScope::Always]
    );
    assert_eq!(caps.cost, CostBasis::Priced);
    assert_eq!(caps.context_window, None, "no window is invented");

    // Every request the adapter wrote during the run is one the versioned schema
    // describes.
    let mut checked = 0;
    for entry in target.records() {
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
    assert!(checked >= 40, "only {checked} requests were checked");
}

// ---------------------------------------------------------------------------
// health(): missing binary, authentication, protocol version
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_binary_is_cli_not_found() {
    let staging = Staging::new();
    let mut config = staging.config();
    config.command = vec!["/nonexistent/dir/agent-that-does-not-exist".to_owned()];
    let provider = AcpProvider::new(config);
    let health = provider.health().await;
    assert_eq!(health.status, HealthStatus::Unavailable);
    assert!(
        matches!(health.error, Some(ProviderError::CliNotFound { .. })),
        "{health:?}"
    );
    let error = provider
        .open(staging.spec("plain"))
        .await
        .err()
        .expect("open fails too");
    assert!(
        matches!(error, ProviderError::CliNotFound { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn an_agent_that_demands_authentication_is_auth_required_and_nothing_is_run() {
    let staging = Staging::new();
    let provider = AcpProvider::new(staging.config_with("auth_required"));
    let health = provider.health().await;
    assert_eq!(health.status, HealthStatus::Unavailable);
    let Some(ProviderError::AuthRequired { login_hint }) = &health.error else {
        panic!("{health:?}");
    };
    let hint = login_hint.as_deref().expect("a hint to show a human");
    assert!(hint.contains("fake-login"), "{hint}");
    assert_eq!(health.login_hint.as_deref(), Some(hint));

    // `open` says the same, and an operator's hint wins.
    let mut config = staging.config_with("auth_required");
    config.login_hint = Some("fake-agent login".to_owned());
    let error = AcpProvider::new(config)
        .open(staging.spec("auth_required"))
        .await
        .err()
        .expect("no session without a login");
    assert_eq!(
        error,
        ProviderError::AuthRequired {
            login_hint: Some("fake-agent login".to_owned())
        }
    );
    // PO never authenticates: the agent never received `authenticate`.
    assert!(staging.requests("authenticate").is_empty());
    let methods: Vec<String> = staging
        .recorded()
        .iter()
        .filter(|entry| entry["kind"] == "in")
        .map(|entry| entry["method"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        methods
            .iter()
            .all(|m| m == "initialize" || m == "session/new"),
        "{methods:?}"
    );
}

#[tokio::test]
async fn an_agent_of_another_protocol_version_is_refused() {
    let staging = Staging::new();
    let provider = AcpProvider::new(staging.config_with("old_protocol"));
    let health = provider.health().await;
    assert_eq!(health.status, HealthStatus::Unavailable);
    assert_eq!(
        health.error.as_ref().map(ProviderError::kind),
        Some("protocol")
    );
}

// ---------------------------------------------------------------------------
// argv and environment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_argument_that_looks_like_a_secret_is_refused_and_nothing_starts() {
    let staging = Staging::new();
    let mut config = staging.config();
    config.command = vec![FAKE.to_owned(), format!("--api-key={CANARY}")];
    let provider = AcpProvider::new(config);
    let health = provider.health().await;
    assert_eq!(health.status, HealthStatus::Unavailable);
    let error = provider
        .open(staging.spec("plain"))
        .await
        .err()
        .expect("refused");
    assert_eq!(error.kind(), "invalid_request");
    assert!(!error.to_string().contains(CANARY));
    assert!(!format!("{health:?}").contains(CANARY));
    assert!(
        staging.recorded().is_empty(),
        "no process may have been started"
    );
}

#[tokio::test]
async fn the_environment_is_an_allowlist_plus_what_is_given_explicitly() {
    let staging = Staging::new();
    // Cargo sets this variable for the test process: it is in the host environment.
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
    let mut config = staging.config();
    config
        .env
        .insert("ACP_INSTANCE_VAR".to_owned(), "instance".to_owned());
    let mut spec = staging.spec("plain");
    spec.env
        .set
        .insert("ACP_SESSION_SECRET".to_owned(), CANARY.to_owned());
    config.command = vec![FAKE.to_owned(), "--stdio".to_owned()];
    let provider = AcpProvider::new(config);
    let session = provider.open(spec.clone()).await.expect("opens");
    let start = staging.start();
    let names: Vec<&str> = start["env_names"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        !names.contains(&"CARGO_MANIFEST_DIR"),
        "a host variable leaked: {names:?}"
    );
    assert!(names.contains(&"ACP_INSTANCE_VAR") && names.contains(&"ACP_SESSION_SECRET"));
    assert!(names.contains(&"PATH"), "the base allowlist is kept");
    // The explicit secret reaches the process through its variable, and only there.
    assert_eq!(start["canary_in_env_of"], json!(["ACP_SESSION_SECRET"]));
    assert_eq!(start["canary_in_argv"], json!(false));
    assert_eq!(start["argv"], json!(["--stdio"]));
    session.close().await.unwrap();

    // `env_inherit` is the opt-in for host variables.
    let staging = Staging::new();
    let mut config = staging.config();
    config.env_inherit = vec!["CARGO_MANIFEST_DIR".to_owned()];
    let session = AcpProvider::new(config)
        .open(staging.spec("plain"))
        .await
        .expect("opens");
    let names = staging.start()["env_names"].clone();
    assert!(
        names
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "CARGO_MANIFEST_DIR")
    );
    session.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Permissions, byte for byte
// ---------------------------------------------------------------------------

async fn ask_and_answer(
    staging: &Staging,
    name: &str,
    decision: PermissionDecision,
) -> (AgentEvent, Vec<AgentEvent>) {
    let session = staging.open(name).await;
    let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    let ask = next_matching(&mut stream, |event| {
        matches!(event, AgentEvent::PermissionAsk { .. })
    })
    .await;
    let AgentEvent::PermissionAsk {
        request_id, scopes, ..
    } = ask.clone()
    else {
        unreachable!()
    };
    assert!(scopes.contains(&PermissionScope::Once));
    session
        .answer_permission(&request_id, decision)
        .await
        .expect("answered");
    let events = collect(stream).await;
    session.close().await.unwrap();
    (ask, events)
}

#[tokio::test]
async fn a_granted_permission_selects_the_allow_once_option_octet_for_octet() {
    let staging = Staging::new();
    let (ask, events) = ask_and_answer(
        &staging,
        "permission_accordee",
        PermissionDecision::allow_once(),
    )
    .await;
    assert_eq!(stop_of(&events), StopReason::Completed);
    assert_eq!(
        staging.responses(),
        [
            r#"{"id":"perm_1","jsonrpc":"2.0","result":{"outcome":{"optionId":"allow-once","outcome":"selected"}}}"#
        ]
    );
    // A request carries the tool's identity.
    let ask = std::iter::once(&ask)
        .find_map(|event| match event {
            AgentEvent::PermissionAsk {
                tool_name,
                category,
                canonical,
                tool_call_id,
                input,
                ..
            } => Some((tool_name, category, canonical, tool_call_id, input)),
            _ => None,
        })
        .expect("a permission_ask");
    assert_eq!(ask.0, "Run rm -rf build");
    assert_eq!(*ask.1, ToolCategory::Command);
    assert_eq!(ask.2.as_deref(), Some("Bash"));
    assert_eq!(ask.3.as_deref(), Some("call_1"));
    assert_eq!(ask.4, &json!({"command": "rm -rf build"}));
}

#[tokio::test]
async fn a_refused_permission_selects_the_reject_once_option_octet_for_octet() {
    let staging = Staging::new();
    let (_, events) =
        ask_and_answer(&staging, "permission_refusee", PermissionDecision::deny()).await;
    assert_eq!(
        staging.responses(),
        [
            r#"{"id":"perm_1","jsonrpc":"2.0","result":{"outcome":{"optionId":"reject-once","outcome":"selected"}}}"#
        ]
    );
    // The call that was refused ended in error, and the turn still ended with `done`.
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolResult { is_error: true, .. }))
    );
    assert!(matches!(done(&events), AgentEvent::Done { .. }));
}

#[tokio::test]
async fn an_always_approval_selects_allow_always_and_a_scope_not_offered_is_refused() {
    let staging = Staging::new();
    let decision = PermissionDecision::Allow {
        scope: PermissionScope::Always,
        updated_input: None,
    };
    ask_and_answer(&staging, "permission_always", decision.clone()).await;
    assert_eq!(
        staging.responses(),
        [
            r#"{"id":"perm_1","jsonrpc":"2.0","result":{"outcome":{"optionId":"allow-always","outcome":"selected"}}}"#
        ]
    );

    // An agent that offers no `allow_always`: the ask offers `once` only and `always`
    // answers `Unsupported { permission_scope }`; the request stays answerable.
    let staging = Staging::new();
    let session = staging.open("permission_once_only").await;
    let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    let AgentEvent::PermissionAsk {
        request_id, scopes, ..
    } = next_matching(&mut stream, |e| {
        matches!(e, AgentEvent::PermissionAsk { .. })
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(scopes, vec![PermissionScope::Once]);
    assert_eq!(
        session.answer_permission(&request_id, decision).await,
        Err(ProviderError::unsupported("permission_scope"))
    );
    let rewritten = PermissionDecision::Allow {
        scope: PermissionScope::Once,
        updated_input: Some(json!({"command": "other"})),
    };
    assert_eq!(
        session.answer_permission(&request_id, rewritten).await,
        Err(ProviderError::unsupported("permission_updated_input"))
    );
    session
        .answer_permission(&request_id, PermissionDecision::allow_once())
        .await
        .unwrap();
    assert_eq!(stop_of(&collect(stream).await), StopReason::Completed);
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_cancelled_turn_answers_the_pending_permission_cancelled_then_sends_session_cancel() {
    let staging = Staging::new();
    let session = staging.open("permission_cancel").await;
    let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    next_matching(&mut stream, |e| {
        matches!(e, AgentEvent::PermissionAsk { .. })
    })
    .await;
    let outcome = session.interrupt(InterruptScope::TurnOnly).await.unwrap();
    assert!(outcome.turn_interrupted);
    let events = collect(stream).await;
    assert_eq!(stop_of(&events), StopReason::Interrupted);
    assert_eq!(
        staging.responses(),
        [r#"{"id":"perm_1","jsonrpc":"2.0","result":{"outcome":{"outcome":"cancelled"}}}"#]
    );
    assert_eq!(staging.requests("session/cancel").len(), 1);
    session.close().await.unwrap();
}

#[tokio::test]
async fn the_neutral_policy_answers_before_anyone_is_asked() {
    // `auto_edits` accepts an edit without a `permission_ask`.
    let staging = Staging::new();
    let provider = staging.learned_provider().await;
    let mut spec = staging.spec("permission_edit");
    spec.policy = ToolPolicy::new(PolicyMode::AutoEdits);
    let session = provider.open(spec).await.unwrap();
    let events = turn(&*session).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::PermissionAsk { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ProviderNotice { kind, .. } if kind == "permission_allowed_by_policy"
    )));
    assert_eq!(
        staging.responses(),
        [
            r#"{"id":"perm_1","jsonrpc":"2.0","result":{"outcome":{"optionId":"allow-once","outcome":"selected"}}}"#
        ]
    );
    session.close().await.unwrap();

    // `plan_only` denies a command without asking.
    let staging = Staging::new();
    let provider = staging.learned_provider().await;
    let mut spec = staging.spec("permission_refusee");
    spec.policy = ToolPolicy::new(PolicyMode::PlanOnly);
    let session = provider.open(spec).await.unwrap();
    let events = turn(&*session).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::PermissionAsk { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ProviderNotice { kind, .. } if kind == "permission_denied_by_policy"
    )));
    assert_eq!(
        staging.responses(),
        [
            r#"{"id":"perm_1","jsonrpc":"2.0","result":{"outcome":{"optionId":"reject-once","outcome":"selected"}}}"#
        ]
    );
    session.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Interruption, one turn at a time
// ---------------------------------------------------------------------------

#[tokio::test]
async fn interrupt_sends_session_cancel_and_the_turn_ends_when_the_agent_says_cancelled() {
    let staging = Staging::new();
    let session = staging.open("interruption_en_flux").await;
    // Outside a turn: nothing to cancel, nothing sent.
    let idle = session.interrupt(InterruptScope::TurnOnly).await.unwrap();
    assert!(!idle.turn_interrupted);
    assert!(staging.requests("session/cancel").is_empty());

    let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    next_matching(&mut stream, |e| {
        matches!(e, AgentEvent::Text { .. } | AgentEvent::Delta { .. })
    })
    .await;
    // One turn at a time.
    assert_eq!(
        session.send_turn(TurnInput::text("again")).await.err(),
        Some(ProviderError::TurnInProgress)
    );
    let outcome = session
        .interrupt(InterruptScope::TurnAndTools)
        .await
        .unwrap();
    assert!(outcome.turn_interrupted);
    assert_eq!(stop_of(&collect(stream).await), StopReason::Interrupted);
    let cancels = staging.requests("session/cancel");
    assert_eq!(cancels.len(), 1);
    assert_eq!(cancels[0]["has_id"], json!(false), "a notification");
    assert_eq!(cancels[0]["params"], json!({"sessionId": "sess_fake1"}));
    session.close().await.unwrap();
}

#[tokio::test]
async fn what_acp_cannot_do_is_unsupported_by_name() {
    let staging = Staging::new();
    let session = staging.open("plain").await;
    assert_eq!(
        session.cancel_tools(CancelScope::All).await.err(),
        Some(ProviderError::unsupported("tool_cancel"))
    );
    assert_eq!(
        session.set_model("other-model").await.err(),
        Some(ProviderError::unsupported("set_model_live"))
    );
    assert_eq!(
        session.set_policy_mode(PolicyMode::Trust, None).await.err(),
        // The agent publishes no mode that matches `trust`: the neutral policy cannot reach it.
        // The sandbox level is not what refuses it (it is information, not a gate).
        Some(ProviderError::unsupported("set_policy_mode"))
    );
    let images = TurnInput {
        blocks: vec![nexus_claude::agent::InputBlock::Image {
            media_type: "image/png".to_owned(),
            data_base64: "AAAA".to_owned(),
        }],
    };
    assert_eq!(
        session.send_turn(images).await.err(),
        Some(ProviderError::unsupported("images"))
    );
    session.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_policy_mode_is_session_set_mode_when_the_agent_published_modes() {
    let staging = Staging::new();
    let session = staging.open("changement_politique").await;
    session
        .set_policy_mode(PolicyMode::AutoEdits, None)
        .await
        .expect("acceptEdits is published");
    let requests = staging.requests("session/set_mode");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]["params"],
        json!({"sessionId": "sess_fake1", "modeId": "acceptEdits"})
    );
    // No published mode matches `trust`; `plan_only` is published and has no transcript
    // step: only the lookup is checked here.
    assert_eq!(
        session.set_policy_mode(PolicyMode::Trust, None).await.err(),
        // The agent publishes no mode that matches `trust`: the neutral policy cannot reach it.
        // The sandbox level is not what refuses it (it is information, not a gate).
        Some(ProviderError::unsupported("set_policy_mode"))
    );
    assert_eq!(stop_of(&turn(&*session).await), StopReason::Completed);
    session.close().await.unwrap();
}

#[tokio::test]
async fn set_policy_mode_without_published_modes_is_unsupported() {
    let staging = Staging::new();
    let session = staging.open("nomodes").await;
    assert_eq!(
        session
            .set_policy_mode(PolicyMode::AutoEdits, Some("whatever"))
            .await
            .err(),
        Some(ProviderError::unsupported("set_policy_mode"))
    );
    assert!(staging.requests("session/set_mode").is_empty());
    session.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// session/load
// ---------------------------------------------------------------------------

fn token(id: &str) -> ResumeToken {
    ResumeToken::new(ProviderKind::Acp, 1, json!({ "session_id": id }))
}

#[tokio::test]
async fn load_session_unavailable_makes_resume_unsupported_with_a_typed_error() {
    // Learned through `health`: the capability is false and `resume()` refuses before
    // starting anything.
    let staging = Staging::new();
    let provider = AcpProvider::new(staging.config_with("no_load"));
    assert_eq!(provider.health().await.status, HealthStatus::Ok);
    assert!(!provider.capabilities(None).resume);
    let error = provider
        .resume(staging.spec("no_load"), token("sess_prev"))
        .await
        .err()
        .expect("no loadSession");
    assert_eq!(error, ProviderError::unsupported("resume"));
    assert!(error.to_string().contains("resume"));
    let session = provider.open(staging.spec("no_load")).await.unwrap();
    assert!(session.resume_token().is_none());
    session.close().await.unwrap();

    // Not learned yet: the handshake finds out and the process is shut down.
    let staging = Staging::new();
    let provider = AcpProvider::new(staging.config());
    let error = provider
        .resume(staging.spec("no_load"), token("sess_prev"))
        .await
        .err()
        .expect("no loadSession");
    assert_eq!(error, ProviderError::unsupported("resume"));
    assert!(staging.requests("session/load").is_empty());
    assert!(!provider.capabilities(None).resume);

    // A token that carries no session id is `invalid_request`.
    let error = provider
        .resume(
            staging.spec("reprise"),
            ResumeToken::new(ProviderKind::Acp, 1, json!({})),
        )
        .await
        .err()
        .expect("refused");
    assert_eq!(error.kind(), "invalid_request");
}

#[tokio::test]
async fn a_loaded_session_drops_the_replayed_history_and_keeps_its_token() {
    let staging = Staging::new();
    let provider = staging.learned_provider().await;
    assert!(provider.capabilities(None).resume);
    let session = provider
        .resume(staging.spec("reprise"), token("sess_prev"))
        .await
        .expect("loads");
    let mut oob = session.out_of_band().expect("out of band");
    let events = turn(&*session).await;
    assert_eq!(stop_of(&events), StopReason::Completed);
    // The replay is not an event of the turn.
    let texts: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["resumed"]);
    let replayed = next_matching(&mut oob, |event| {
        matches!(event, AgentEvent::ProviderNotice { kind, .. } if kind == "history_replayed")
    })
    .await;
    assert!(matches!(
        replayed,
        AgentEvent::ProviderNotice { data, .. } if data["updates"] == json!(2)
    ));
    assert_eq!(
        session.resume_token().expect("a token").data(),
        &json!({"session_id": "sess_prev"})
    );
    assert_eq!(
        staging.requests("session/load")[0]["params"]["sessionId"],
        "sess_prev"
    );
    session.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// What the client does not serve
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_client_announces_no_fs_or_terminal_and_answers_method_not_found() {
    let staging = Staging::new();
    let session = staging.open("fs_requests").await;
    let events = turn(&*session).await;
    // The fake exits 99 when an answer is not -32601; the turn completing proves it.
    assert_eq!(stop_of(&events), StopReason::Completed);
    let initialize = &staging.requests("initialize")[0]["params"];
    assert_eq!(
        initialize["clientCapabilities"],
        json!({"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false})
    );
    let responses = staging.responses();
    assert_eq!(responses.len(), 3);
    assert!(
        responses
            .iter()
            .all(|line| line.contains(r#""code":-32601"#)),
        "{responses:?}"
    );
    let noticed: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ProviderNotice { kind, data } if kind == "unsupported_agent_request" => {
                data["method"].as_str().map(str::to_owned)
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        noticed,
        ["fs/read_text_file", "fs/write_text_file", "terminal/create"]
    );
    session.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Process tree
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn alive(pid: u64) -> bool {
    let pid = i32::try_from(pid).expect("a pid");
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(unix)]
async fn wait_dead(pids: &[u64]) -> bool {
    for _ in 0..100 {
        if pids.iter().all(|pid| !alive(*pid)) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[cfg(unix)]
#[tokio::test]
async fn close_kills_the_process_and_all_its_descendants() {
    let staging = Staging::new();
    let session = staging.open("descendants").await;
    // The fake records its children as it starts them.
    let mut children: Vec<u64> = Vec::new();
    for _ in 0..100 {
        children = staging
            .recorded()
            .iter()
            .filter(|entry| entry["kind"] == "child")
            .filter_map(|entry| entry["pid"].as_u64())
            .collect();
        if children.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(children.len(), 2, "the fake started two children");
    let root = u64::from(staging.start()["pid"].as_u64().unwrap() as u32);
    assert!(alive(root) && children.iter().all(|pid| alive(*pid)));
    session.close().await.unwrap();
    session.close().await.expect("close is idempotent");
    let mut all = children.clone();
    all.push(root);
    let dead = wait_dead(&all).await;
    // Whatever survived must not outlive the test.
    for pid in all.iter().filter(|pid| alive(**pid)) {
        // SAFETY: killing a process this test started.
        unsafe { libc::kill(i32::try_from(*pid).unwrap(), libc::SIGKILL) };
    }
    assert!(dead, "the agent or one of its descendants survived close()");
}

// ---------------------------------------------------------------------------
// Projection, stop reasons, MCP servers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn updates_are_projected_as_documented() {
    let staging = Staging::new();
    let provider = staging.learned_provider().await;
    let mut spec = staging.spec("updates");
    spec.mcp_servers
        .insert("fake".to_owned(), McpServerSpec::stdio("fake-mcp"));
    let session = provider.open(spec).await.unwrap();
    let events = turn(&*session).await;
    assert_eq!(stop_of(&events), StopReason::Completed);
    let notice = |wanted: &str| {
        events.iter().find_map(|event| match event {
            AgentEvent::ProviderNotice { kind, data } if kind == wanted => Some(data.clone()),
            _ => None,
        })
    };
    assert_eq!(
        notice("plan").expect("a plan notice")["entries"][0]["content"],
        "Check for syntax errors"
    );
    assert_eq!(
        notice("available_commands").expect("a commands notice")["commands"][0]["name"],
        "web"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::PolicyModeChanged { mode: PolicyMode::PlanOnly, native_mode: Some(native) } if native == "plan"
    )));
    let calls: BTreeMap<&str, (ToolCategory, Option<&str>)> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolCall {
                id,
                category,
                canonical,
                input_complete: true,
                ..
            } => Some((id.as_str(), (*category, canonical.as_deref()))),
            _ => None,
        })
        .collect();
    assert_eq!(calls["m1"], (ToolCategory::Mcp, Some("mcp__fake__write")));
    assert_eq!(calls["e1"], (ToolCategory::Edit, Some("Edit")));
    assert_eq!(calls["s1"], (ToolCategory::Search, Some("Grep")));
    // Results: a raw output as text, a diff as its path; the search never finished and
    // has no result, but its input was completed at the end of the turn.
    let results: BTreeMap<&str, Option<String>> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolResult { id, output, .. } => Some((
                id.as_str(),
                output
                    .as_ref()
                    .map(|output| serde_json::to_string(output).unwrap()),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(results["m1"].as_deref(), Some(r#""{\"ok\":true}""#));
    assert_eq!(results["e1"].as_deref(), Some(r#""diff of a.txt""#));
    assert!(!results.contains_key("s1"));
    // Unknown updates are dropped, usage_update feeds the context size, the cost of an
    // unpriced model is unknown (no amount), never zero.
    let AgentEvent::Done {
        usage,
        cost,
        provider_session_id,
        ..
    } = done(&events)
    else {
        unreachable!()
    };
    assert_eq!(usage.context_tokens, Some(5000));
    assert_eq!(usage.input_tokens, None);
    assert_eq!(provider_session_id.as_deref(), Some("sess_fake1"));
    assert_eq!(cost.basis, CostBasis::Priced);
    assert_eq!(
        cost.usd, None,
        "no usage, so no amount: never an invented zero"
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn every_stop_reason_has_its_done() {
    for (name, stop, is_error) in [
        ("stop_refusal", StopReason::Refusal, false),
        ("stop_max_tokens", StopReason::MaxTokens, false),
        ("stop_max_turn_requests", StopReason::MaxTurns, false),
    ] {
        let staging = Staging::new();
        let session = staging.open(name).await;
        let events = turn(&*session).await;
        let AgentEvent::Done {
            stop_reason,
            is_error: failed,
            subtype,
            ..
        } = done(&events)
        else {
            panic!("{name}: {events:?}");
        };
        assert_eq!((*stop_reason, *failed), (stop, is_error), "{name}");
        assert!(subtype.is_some(), "the native label is kept");
        session.close().await.unwrap();
    }
}

#[tokio::test]
async fn a_json_rpc_error_ends_the_turn_with_a_classified_done_and_the_session_survives() {
    let staging = Staging::new();
    let session = staging.open("erreur_retryable").await;
    let events = turn(&*session).await;
    let AgentEvent::Done {
        stop_reason,
        is_error,
        error,
        ..
    } = done(&events)
    else {
        panic!("{events:?}");
    };
    assert_eq!(*stop_reason, StopReason::Error);
    assert!(*is_error);
    assert_eq!(
        error.as_ref(),
        Some(&ProviderError::RateLimited {
            retry_after_ms: None
        })
    );
    assert_eq!(stop_of(&turn(&*session).await), StopReason::Completed);
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_malformed_prompt_result_ends_the_turn_with_a_protocol_error_and_the_session_survives() {
    // The agent answers the prompt with a result that is not a `PromptResult` (its
    // `stopReason` is a number). Written outside `sessions/`: every transcript there must
    // be accepted by the types (`acp_schema_drift`), and this one is wrong on purpose.
    let staging = Staging::new();
    let path = staging.cwd.path().join("prompt_malformed.jsonl");
    let include = |name: &str| json!({"op": "include", "file": transcript(name)}).to_string();
    let lines = [
        include("prelude"),
        include("turn1_start"),
        json!({"op": "reply", "to": "p1", "result": {"stopReason": 42}}).to_string(),
        include("turn2_text"),
    ];
    std::fs::write(&path, lines.join("\n")).expect("the transcript is written");
    let mut spec = staging.spec("plain");
    spec.env
        .set
        .insert("FAKE_ACP_TRANSCRIPT".to_owned(), path.display().to_string());
    let session = staging
        .learned_provider()
        .await
        .open(spec)
        .await
        .expect("the session opens");
    let events = turn(&*session).await;
    let AgentEvent::Done {
        stop_reason,
        is_error,
        error,
        ..
    } = done(&events)
    else {
        panic!("{events:?}");
    };
    assert_eq!(*stop_reason, StopReason::Error);
    assert!(*is_error);
    let shown = error.as_ref().expect("a typed error").to_string();
    assert!(shown.contains("malformed session/prompt result"), "{shown}");
    assert_eq!(stop_of(&turn(&*session).await), StopReason::Completed);
    session.close().await.unwrap();
}

#[tokio::test]
async fn mcp_servers_reach_session_new_and_their_secrets_stay_off_argv_events_and_errors() {
    let staging = Staging::new();
    let provider = staging.learned_provider().await;
    let mut spec = staging.spec("plain");
    spec.mcp_servers.insert(
        "po".to_owned(),
        McpServerSpec::Stdio {
            command: "/bin/po-mcp".to_owned(),
            args: vec!["--stdio".to_owned()],
            env: BTreeMap::from([("NEO4J_PASSWORD".to_owned(), CANARY.to_owned())]),
        },
    );
    spec.mcp_servers.insert(
        "remote".to_owned(),
        McpServerSpec::Http {
            url: "https://mcp.example/api".to_owned(),
            headers: BTreeMap::from([("Authorization".to_owned(), format!("Bearer {CANARY}"))]),
        },
    );
    let session = provider.open(spec.clone()).await.expect("opens");
    let events = turn(&*session).await;
    assert_eq!(stop_of(&events), StopReason::Completed);
    let new = &staging.requests("session/new")[0];
    // The secrets reached the agent (as the spec asked)…
    assert_eq!(new["canary"], json!(true));
    // (the fake masks the whole line when it holds the canary, so the structure is
    // checked below with values that are not secrets)
    assert_eq!(new["params"], json!("<masked: canary>"));
    // …and nowhere else: not on argv, not in the environment, not in an event, not in
    // what the fake recorded (it masks credential-named values).
    let start = staging.start();
    assert_eq!(start["canary_in_argv"], json!(false));
    assert_eq!(start["canary_in_env_of"], json!([]));
    assert!(!format!("{events:?}").contains(CANARY));
    let recorded = serde_json::to_string(&staging.recorded()).unwrap();
    assert!(!recorded.contains(CANARY), "the recording holds the secret");
    session.close().await.unwrap();

    // An agent without `mcpCapabilities.http` refuses an HTTP server by name, and SSE
    // is refused unless announced; neither leaves a process behind.
    let staging = Staging::new();
    let provider = AcpProvider::new(staging.config_with("no_load"));
    provider.health().await;
    let mut http_only = staging.spec("no_load");
    http_only.mcp_servers.insert(
        "remote".to_owned(),
        McpServerSpec::Http {
            url: "https://mcp.example/api".to_owned(),
            headers: BTreeMap::new(),
        },
    );
    assert_eq!(
        provider.open(http_only).await.err(),
        Some(ProviderError::unsupported("mcp_http"))
    );
    let staging = Staging::new();
    let provider = staging.learned_provider().await;
    let mut sse = staging.spec("plain");
    sse.mcp_servers.insert(
        "legacy".to_owned(),
        McpServerSpec::Sse {
            url: "https://mcp.example/sse".to_owned(),
            headers: BTreeMap::new(),
        },
    );
    assert_eq!(
        provider.open(sse).await.err(),
        Some(ProviderError::unsupported("mcp_sse"))
    );
    // A URL with user-info is refused without echoing it.
    let mut leaky = staging.spec("plain");
    leaky.mcp_servers.insert(
        "bad".to_owned(),
        McpServerSpec::Http {
            url: format!("https://user:{CANARY}@h/mcp"),
            headers: BTreeMap::new(),
        },
    );
    let error = provider.open(leaky).await.err().expect("refused");
    assert_eq!(error.kind(), "invalid_request");
    assert!(!error.to_string().contains(CANARY));
}

// ---------------------------------------------------------------------------
// Refusals at opening, thinking, cost
// ---------------------------------------------------------------------------

#[tokio::test]
async fn what_the_protocol_cannot_honour_is_refused_before_anything_starts() {
    let staging = Staging::new();
    let provider = AcpProvider::new(staging.config());
    let refuse = |capability: &str| Some(ProviderError::unsupported(capability));
    let mut spec = staging.spec("plain");
    spec.limits.max_tokens = Some(10);
    assert_eq!(provider.open(spec).await.err(), refuse("limits"));
    let mut spec = staging.spec("plain");
    spec.max_turns = Some(2);
    assert_eq!(provider.open(spec).await.err(), refuse("limits"));
    let mut spec = staging.spec("plain");
    spec.system_prompt = Some(nexus_claude::agent::SystemPromptSpec {
        text: "be brief".to_owned(),
        mode: nexus_claude::agent::SystemPromptMode::Append,
    });
    assert_eq!(provider.open(spec).await.err(), refuse("system_prompt"));
    let mut spec = staging.spec("plain");
    spec.extra_dirs.push(PathBuf::from("/tmp"));
    assert_eq!(provider.open(spec).await.err(), refuse("extra_dirs"));
    assert!(staging.recorded().is_empty(), "nothing was started");
}

#[tokio::test]
async fn reasoning_not_declared_is_dropped_once_noticed_and_learned() {
    let staging = Staging::new();
    let mut config = staging.config();
    config.thinking = false;
    let provider = AcpProvider::new(config);
    provider.health().await;
    assert!(!provider.capabilities(None).thinking);
    let session = provider.open(staging.spec("raisonnement")).await.unwrap();
    let events = turn(&*session).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Thinking { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ProviderNotice { kind, .. } if kind == "thinking_not_declared"
    )));
    assert_eq!(stop_of(&events), StopReason::Completed);
    // The provider has now seen it: the next session declares `thinking`.
    assert!(provider.capabilities(None).thinking);
    session.close().await.unwrap();
}

#[tokio::test]
async fn the_cost_follows_the_configuration_and_is_never_reported() {
    for (basis, usd, expected) in [
        (CostBasis::Unknown, None, CostBasis::Unknown),
        (CostBasis::Free, Some(0.0), CostBasis::Free),
        // 120 in × $1 + 60 out × $2, per million tokens, and 20 cached × $0.5.
        (CostBasis::Priced, Some(0.00025), CostBasis::Priced),
        (CostBasis::Reported, None, CostBasis::Unknown),
    ] {
        let staging = Staging::new();
        let mut config = staging.config();
        config.cost_basis = basis;
        let provider = AcpProvider::new(config);
        provider.health().await;
        assert_eq!(provider.capabilities(Some(MODEL)).cost, expected);
        let session = provider.open(staging.spec("plain")).await.unwrap();
        let events = turn(&*session).await;
        let AgentEvent::Done { cost, usage, .. } = done(&events) else {
            unreachable!()
        };
        assert_eq!(cost.basis, expected);
        match (usd, cost.usd) {
            (None, None) => {},
            (Some(want), Some(got)) => assert!((want - got).abs() < 1e-9, "{basis:?}: {got}"),
            other => panic!("{basis:?}: {other:?}"),
        }
        assert_eq!(usage.input_tokens, Some(120));
        assert_eq!(usage.output_tokens, Some(60));
        session.close().await.unwrap();
    }
}

#[allow(dead_code)]
fn _unused(_: &Path) {}

#[tokio::test]
async fn mcp_servers_are_given_to_session_new_in_the_arrays_of_the_protocol() {
    let staging = Staging::new();
    let provider = staging.learned_provider().await;
    let mut spec = staging.spec("plain");
    spec.mcp_servers.insert(
        "po".to_owned(),
        McpServerSpec::Stdio {
            command: "/bin/po-mcp".to_owned(),
            args: vec!["--stdio".to_owned()],
            env: BTreeMap::from([
                ("LEVEL".to_owned(), "1".to_owned()),
                ("NEO4J_PASSWORD".to_owned(), "db-pass-value".to_owned()),
            ]),
        },
    );
    spec.mcp_servers.insert(
        "remote".to_owned(),
        McpServerSpec::Http {
            url: "https://mcp.example/api".to_owned(),
            headers: BTreeMap::from([("X-Tenant".to_owned(), "acme".to_owned())]),
        },
    );
    let session = provider.open(spec).await.expect("opens");
    let params = staging.requests("session/new")[0]["params"].clone();
    assert_eq!(
        params["mcpServers"],
        json!([
            {"name": "po", "command": "/bin/po-mcp", "args": ["--stdio"],
             "env": [{"name": "LEVEL", "value": "1"},
                     {"name": "NEO4J_PASSWORD", "value": "<redacted>"}]},
            {"type": "http", "name": "remote", "url": "https://mcp.example/api",
             "headers": [{"name": "X-Tenant", "value": "acme"}]}
        ])
    );
    assert_eq!(
        params["cwd"],
        json!(staging.cwd.path().display().to_string())
    );
    session.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// An agent that refuses per-session MCP servers (`openclaw acp`)
// ---------------------------------------------------------------------------

/// The PO server a host gives a session.
fn with_po_server(mut spec: SessionSpec) -> SessionSpec {
    spec.mcp_servers.insert(
        "project-orchestrator".to_owned(),
        McpServerSpec::Stdio {
            command: "/bin/po-mcp".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
        },
    );
    spec
}

/// `openclaw acp` answers a `session/new` that carries `mcpServers` with an error (it
/// used to ignore them). The session opens without them, says so, and the provider
/// keeps it learned: its capabilities and the next opening follow.
#[tokio::test]
async fn an_agent_that_refuses_mcp_servers_opens_without_them_and_says_so() {
    let staging = Staging::new();
    let provider = staging.learned_provider().await;
    assert!(provider.capabilities(None).per_session_mcp);
    let session = provider
        .open(with_po_server(staging.spec("mcp_refused")))
        .await
        .expect("the session opens without its MCP servers");
    let mut oob = session.out_of_band().expect("out of band");
    // Asked twice: with the server, refused; then without.
    let asked: Vec<Value> = staging
        .requests("session/new")
        .into_iter()
        .map(|entry| entry["params"]["mcpServers"].clone())
        .collect();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert_eq!(asked[0][0]["name"], "project-orchestrator");
    assert_eq!(asked[1], json!([]));
    assert!(
        staging
            .recorded()
            .iter()
            .any(|entry| entry["kind"] == "refused_mcp")
    );
    // Said three ways: the capabilities, a notice, the server's status.
    assert!(!session.capabilities().per_session_mcp);
    assert!(!provider.capabilities(None).per_session_mcp);
    let notice = next_matching(&mut oob, |event| {
        matches!(event, AgentEvent::ProviderNotice { kind, .. } if kind == "mcp_servers_refused")
    })
    .await;
    assert!(matches!(
        notice,
        AgentEvent::ProviderNotice { data, .. } if data["servers"] == json!(["project-orchestrator"])
    ));
    let started = next_matching(&mut oob, |event| {
        matches!(event, AgentEvent::SessionStarted { .. })
    })
    .await;
    match started {
        AgentEvent::SessionStarted { mcp_servers, .. } => {
            assert_eq!(mcp_servers.len(), 1);
            assert_eq!(mcp_servers[0].status, "refused");
        },
        other => panic!("{other:?}"),
    }
    // The session works.
    let events = turn(&*session).await;
    assert_eq!(stop_of(&events), StopReason::Completed);
    session.close().await.unwrap();
    // Learned: a next session given a server is refused before anything starts.
    let before = staging.requests("session/new").len();
    assert_eq!(
        provider
            .open(with_po_server(staging.spec("mcp_refused")))
            .await
            .err(),
        Some(ProviderError::unsupported("per_session_mcp"))
    );
    assert_eq!(staging.requests("session/new").len(), before);
}

/// The same refusal on `session/load` (a resume): retried without the servers.
#[tokio::test]
async fn an_agent_that_refuses_mcp_servers_on_load_resumes_without_them() {
    let staging = Staging::new();
    let provider = staging.learned_provider().await;
    let session = provider
        .resume(
            with_po_server(staging.spec("mcp_refused_load")),
            token("sess_prev"),
        )
        .await
        .expect("loads without its MCP servers");
    let loads = staging.requests("session/load");
    assert_eq!(loads.len(), 2, "{loads:?}");
    assert_eq!(loads[1]["params"]["mcpServers"], json!([]));
    assert!(!session.capabilities().per_session_mcp);
    assert_eq!(
        session.resume_token().expect("a token").data(),
        &json!({"session_id": "sess_prev"})
    );
    session.close().await.unwrap();
}

/// An instance configured without per-session MCP (`openclaw acp`) says so up front,
/// refuses a server before anything starts, and opens a session that has none.
#[tokio::test]
async fn an_instance_configured_without_per_session_mcp_says_so_and_refuses_a_server() {
    let staging = Staging::new();
    let mut config = staging.config();
    config.per_session_mcp = false;
    let provider = AcpProvider::new(config);
    assert!(!provider.capabilities(None).per_session_mcp);
    assert!(
        provider.capabilities(None).tools,
        "the agent keeps its own tools"
    );
    assert_eq!(
        provider
            .open(with_po_server(staging.spec("plain")))
            .await
            .err(),
        Some(ProviderError::unsupported("per_session_mcp"))
    );
    assert!(staging.recorded().is_empty(), "nothing was started");
    let session = provider
        .open(staging.spec("plain"))
        .await
        .expect("opens without a server");
    assert!(!session.capabilities().per_session_mcp);
    assert_eq!(
        staging.requests("session/new")[0]["params"]["mcpServers"],
        json!([])
    );
    session.close().await.unwrap();
}

/// Any other refusal of `session/new` is not taken for a refusal of the servers: no
/// second request, the error as before.
#[tokio::test]
async fn another_refusal_of_session_new_is_not_retried() {
    let staging = Staging::new();
    let provider = staging.learned_provider().await;
    let error = provider
        .open(with_po_server(staging.spec("auth_required")))
        .await
        .err()
        .expect("refused");
    assert_eq!(error.kind(), "auth_required");
    assert_eq!(staging.requests("session/new").len(), 1);
    assert!(provider.capabilities(None).per_session_mcp);
}
