//! The Codex adapter (`providers::codex`) against the conformance suite of the
//! agent contract, and the tests of what is specific to it.
//!
//! **What this proves and what it does not.** `CodexProvider` launches
//! `fake_codex`, a fake executable replaying a JSONL transcript (one per scenario)
//! written from the README of `codex app-server` at `rust-v0.130.0`. Nothing here
//! talked to a real `codex app-server`: the installed `codex` is 0.38.0, which has
//! no such subcommand. A pass says the adapter implements the protocol *as the
//! documentation describes it*, and the contract on top of it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use nexus_claude::agent::{
    AgentEvent, AgentProvider, AgentSession, CancelScope, CostBasis, CredentialRef,
    CredentialResolver, EventStream, HealthStatus, InterruptScope, McpServerSpec, ModelPrice,
    PermissionDecision, PermissionScope, PolicyMode, ProviderError, ProviderKind, QuestionAnswer,
    ResumeToken, Secret, SessionSpec, StopReason, SubagentSupport, ToolPolicy, TurnInput,
};
use nexus_claude::providers::codex::wire;
use nexus_claude::providers::codex::{CodexConfig, CodexProvider, MIN_APP_SERVER_VERSION};
use nexus_claude::testkit::conformance::{
    ConformanceTarget, Prepared, Scenario, ScenarioOutcome, run_all,
};
use serde_json::{Value, json};
use tempfile::TempDir;

const FAKE: &str = env!("CARGO_BIN_EXE_fake_codex");
const MODEL: &str = "gpt-fake";
/// A key-shaped value that must never leave the one variable it was given in.
const CANARY: &str = "sk-canary-Zq81mLpWx39vNbR2kT";

fn sessions_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/transcripts/codex")
        .join(MIN_APP_SERVER_VERSION)
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

fn config(home: &Path) -> CodexConfig {
    let mut config = CodexConfig::new("codex-test");
    config.program = PathBuf::from(FAKE);
    config.codex_home = home.to_path_buf();
    config.default_model = Some(MODEL.to_owned());
    config.cost_basis = CostBasis::Priced;
    config.prices = nexus_claude::model::PriceTable::new().with(MODEL, price());
    config
}

/// One staged session: a spec for `fake_codex` playing `transcript`, and where it
/// records what it saw.
struct Staging {
    _cwd: TempDir,
    home: TempDir,
    record: PathBuf,
}

impl Staging {
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

    fn home(&self) -> PathBuf {
        self.home.path().join("codex-home")
    }

    fn spec(&self, name: &str) -> SessionSpec {
        let mut spec = SessionSpec::new(self._cwd.path());
        spec.model = Some(MODEL.to_owned());
        spec.env.set.insert(
            "FAKE_CODEX_TRANSCRIPT".to_owned(),
            transcript(name).display().to_string(),
        );
        spec.env.set.insert(
            "FAKE_CODEX_RECORD".to_owned(),
            self.record.display().to_string(),
        );
        spec
    }

    fn provider(&self) -> CodexProvider {
        CodexProvider::new(config(&self.home()))
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

    fn start(&self) -> Value {
        self.recorded()
            .into_iter()
            .find(|entry| entry["kind"] == "start")
            .expect("the fake recorded its start")
    }

    /// The raw answers the fake received to its server requests.
    fn responses(&self) -> Vec<String> {
        self.recorded()
            .into_iter()
            .filter(|entry| entry["kind"] == "response")
            .map(|entry| entry["raw"].as_str().unwrap_or_default().to_owned())
            .collect()
    }
}

async fn open(staging: &Staging, name: &str) -> Arc<dyn AgentSession> {
    staging
        .provider()
        .open(staging.spec(name))
        .await
        .expect("the session opens")
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
        Scenario::Reprise => "reprise",
        Scenario::Compaction => "compaction",
        Scenario::SousAgent => "sous_agent",
        Scenario::TourConcurrent => "tour_concurrent",
        Scenario::ErreurRetryable => "erreur_retryable",
        Scenario::ProcessusMort => "processus_mort",
        Scenario::FermetureIdempotente => "fermeture_idempotente",
        // A plain turn: the text turns, and the fallbacks of absent capabilities.
        _ => "plain",
    }
}

struct CodexTarget {
    base: Arc<CodexProvider>,
    _home: TempDir,
    stagings: Mutex<Vec<Staging>>,
}

impl CodexTarget {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("a temp dir");
        Self {
            base: Arc::new(CodexProvider::new(config(&home.path().join("codex-home")))),
            _home: home,
            stagings: Mutex::new(Vec::new()),
        }
    }

    /// What every `fake_codex` of the run recorded.
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
impl ConformanceTarget for CodexTarget {
    fn name(&self) -> &str {
        "codex (CodexProvider on fake_codex, JSONL transcripts from the app-server README)"
    }

    fn provider(&self) -> Arc<dyn AgentProvider> {
        self.base.clone()
    }

    async fn prepare(&self, scenario: Scenario) -> Option<Prepared> {
        let staging = Staging::new();
        let mut spec = staging.spec(transcript_of(scenario));
        // The suite opens and asks nothing else of the spec.
        spec.deltas = true;
        let provider: Arc<dyn AgentProvider> = Arc::new(staging.provider());
        let mut prepared = Prepared::new(provider, spec);
        if scenario == Scenario::Reprise {
            prepared.resume = Some(ResumeToken::new(
                ProviderKind::Codex,
                1,
                json!({ "thread_id": "thr_prev" }),
            ));
        }
        let record_home = staging.home();
        self.stagings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(staging);
        prepared.guard = Some(Box::new(record_home));
        Some(prepared)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_codex_adapter_passes_the_conformance_suite() {
    let target = CodexTarget::new();
    let report = run_all(&target).await;
    eprintln!("{}", report.summary());
    report.assert_conformant();

    // What Codex does not have is proven through its written fallback (§5).
    for absent in [
        Scenario::QuestionUtilisateur,
        Scenario::AnnulationTourPreserve,
        Scenario::AnnulationTache,
        Scenario::MessageImages,
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
        Scenario::ChangementModele,
        Scenario::ChangementPolitique,
        Scenario::Reprise,
        Scenario::SousAgent,
        Scenario::Compaction,
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
    assert!(caps.resume && caps.interactive_permissions && caps.set_model_live);
    assert!(!caps.tool_cancel && !caps.images && !caps.native_question && !caps.background_tasks);
    assert_eq!(caps.cost, CostBasis::Priced);
    assert_eq!(caps.subagents, SubagentSupport::SeparateThread);
    assert_eq!(caps.context_window, None, "no window is invented");

    // Every request the adapter wrote during the run is one the versioned schema
    // describes, and none carried a canary or a credential-shaped value.
    let mut checked = 0;
    for entry in target.records() {
        if entry["kind"] != "in" || entry["has_id"] != true {
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
// health()
// ---------------------------------------------------------------------------

fn health_provider(version: &str, home: &Path) -> CodexProvider {
    let mut config = config(home);
    config
        .env_set
        .insert("FAKE_CODEX_VERSION".to_owned(), version.to_owned());
    CodexProvider::new(config)
}

#[tokio::test]
async fn a_codex_older_than_the_minimum_is_refused_by_health() {
    let home = tempfile::tempdir().unwrap();
    // The version that was actually installed where this was written.
    let health = health_provider("0.38.0", home.path()).health().await;
    assert_eq!(health.status, HealthStatus::Unavailable);
    assert_eq!(health.version.as_deref(), Some("0.38.0"));
    assert_eq!(health.error, Some(ProviderError::unsupported("app_server")));
    let detail = health.detail.expect("a typed explanation");
    assert!(detail.contains(MIN_APP_SERVER_VERSION) && detail.contains("update codex"));
    // Just below the minimum is still too old; the minimum itself is not.
    let health = health_provider("0.129.9", home.path()).health().await;
    assert_eq!(health.error, Some(ProviderError::unsupported("app_server")));
    std::fs::write(home.path().join("auth.json"), "{}").unwrap();
    let health = health_provider(MIN_APP_SERVER_VERSION, home.path())
        .health()
        .await;
    assert_eq!(health.status, HealthStatus::Ok, "{health:?}");
    assert_eq!(health.version.as_deref(), Some(MIN_APP_SERVER_VERSION));
}

#[tokio::test]
async fn a_missing_binary_is_cli_not_found() {
    let home = tempfile::tempdir().unwrap();
    let mut config = config(home.path());
    config.program = PathBuf::from("/nonexistent/dir/codex-that-does-not-exist");
    let provider = CodexProvider::new(config);
    let health = provider.health().await;
    assert_eq!(health.status, HealthStatus::Unavailable);
    assert!(
        matches!(health.error, Some(ProviderError::CliNotFound { .. })),
        "{health:?}"
    );
    let error = provider
        .open(SessionSpec::new(home.path()))
        .await
        .err()
        .expect("open fails too");
    assert!(
        matches!(error, ProviderError::CliNotFound { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn nobody_logged_in_is_auth_required_with_the_command_to_run_and_nothing_is_run() {
    let home = tempfile::tempdir().unwrap();
    let codex_home = home.path().join("codex-home");
    let mut config = config(&codex_home);
    config.env_set.insert(
        "FAKE_CODEX_VERSION".to_owned(),
        MIN_APP_SERVER_VERSION.to_owned(),
    );
    let health = CodexProvider::new(config).health().await;
    assert_eq!(health.status, HealthStatus::Unavailable);
    let Some(ProviderError::AuthRequired {
        login_hint: Some(hint),
    }) = &health.error
    else {
        panic!("expected auth_required with a hint: {health:?}");
    };
    assert!(hint.contains("codex login"), "{hint}");
    assert!(
        hint.contains(&codex_home.display().to_string()),
        "the login targets this instance's home: {hint}"
    );
    assert_eq!(health.login_hint.as_deref(), Some(hint.as_str()));
    // `health` neither logs in nor creates the home.
    assert!(!codex_home.exists());
}

// ---------------------------------------------------------------------------
// CODEX_HOME
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
#[tokio::test]
async fn codex_home_is_created_private_and_is_one_per_instance() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut homes = Vec::new();
    for instance in ["codex-a", "codex-b"] {
        let home = root.path().join(instance).join("home");
        let mut config = config(&home);
        config.instance_id = instance.to_owned();
        let provider = CodexProvider::new(config);
        let record = root.path().join(format!("{instance}.jsonl"));
        let mut spec = SessionSpec::new(cwd.path());
        spec.env.set.insert(
            "FAKE_CODEX_TRANSCRIPT".into(),
            transcript("plain").display().to_string(),
        );
        spec.env
            .set
            .insert("FAKE_CODEX_RECORD".into(), record.display().to_string());
        let session = provider.open(spec).await.expect("opens");
        assert_eq!(mode_of(&home), 0o700, "{} is private", home.display());
        let start: Value = std::fs::read_to_string(&record)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|entry| entry["kind"] == "start")
            .unwrap();
        assert_eq!(
            start["codex_home"],
            home.display().to_string(),
            "the process runs with this CODEX_HOME"
        );
        homes.push(home);
        session.close().await.unwrap();
    }
    assert_ne!(homes[0], homes[1]);
    // Two sessions of one instance share the home; a wider existing one is tightened.
    let shared = root.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let provider = CodexProvider::new(config(&shared));
    let mut spec = SessionSpec::new(cwd.path());
    spec.env.set.insert(
        "FAKE_CODEX_TRANSCRIPT".into(),
        transcript("plain").display().to_string(),
    );
    provider
        .open(spec)
        .await
        .expect("opens")
        .close()
        .await
        .unwrap();
    assert_eq!(mode_of(&shared), 0o700);
    assert_ne!(
        nexus_claude::providers::codex::default_codex_home("one"),
        nexus_claude::providers::codex::default_codex_home("two")
    );
}

#[tokio::test]
async fn the_session_cannot_override_the_instances_home_or_key() {
    let staging = Staging::new();
    for name in ["CODEX_HOME", "CODEX_API_KEY"] {
        let mut spec = staging.spec("plain");
        spec.env
            .set
            .insert(name.to_owned(), "/elsewhere".to_owned());
        let error = staging.provider().open(spec).await.err().expect("refused");
        assert_eq!(error.kind(), "invalid_request", "{name}");
    }
    assert!(staging.recorded().is_empty(), "nothing was started");
}

// ---------------------------------------------------------------------------
// Secrets and environment
// ---------------------------------------------------------------------------

struct CanaryVault;

#[async_trait]
impl CredentialResolver for CanaryVault {
    async fn resolve(
        &self,
        instance: &str,
        reference: &CredentialRef,
    ) -> Result<Option<Secret>, ProviderError> {
        assert_eq!(instance, "codex-test", "the instance id is the asker's");
        match reference {
            CredentialRef::Vault(_) => Ok(Some(Secret::new(CANARY))),
            _ => Ok(None),
        }
    }
}

#[tokio::test]
async fn the_api_key_reaches_the_process_through_its_environment_only() {
    let staging = Staging::new();
    let mut config = config(&staging.home());
    config.credential = CredentialRef::Vault("codex-key".to_owned());
    let provider = CodexProvider::with_resolver(config, Arc::new(CanaryVault));
    let mut spec = staging.spec("plain");
    spec.env
        .set
        .insert("FAKE_CODEX_CANARY".to_owned(), CANARY.to_owned());
    spec.mcp_servers.insert(
        "fake".to_owned(),
        McpServerSpec::Stdio {
            command: "/bin/fake-mcp".to_owned(),
            args: vec!["--stdio".to_owned()],
            env: BTreeMap::from([(
                "NEO4J_PASSWORD".to_owned(),
                "mcp-env-secret-value".to_owned(),
            )]),
        },
    );
    spec.mcp_servers.insert(
        "remote".to_owned(),
        McpServerSpec::Http {
            url: "https://mcp.example/api".to_owned(),
            headers: BTreeMap::from([(
                "Authorization".to_owned(),
                "Bearer mcp-header-secret-value".to_owned(),
            )]),
        },
    );
    let session = provider.open(spec).await.expect("opens");
    let events = turn(&*session).await;
    session.close().await.unwrap();

    let start = staging.start();
    assert_eq!(start["api_key_len"], CANARY.len(), "CODEX_API_KEY was set");
    assert_eq!(start["canary_in_argv"], false);
    assert_eq!(
        start["canary_in_env_of"],
        json!([]),
        "the key is in no other variable"
    );
    let argv = start["argv"].as_array().unwrap();
    assert_eq!(argv.last().and_then(Value::as_str), Some("app-server"));
    let argv_text = argv
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        argv_text.contains("mcp_servers.fake.command="),
        "{argv_text}"
    );
    for secret in [CANARY, "mcp-env-secret-value", "mcp-header-secret-value"] {
        assert!(
            !argv_text.contains(secret),
            "{secret} is on the command line"
        );
    }
    let names: Vec<&str> = start["env_names"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        names.contains(&"NEO4J_PASSWORD") && names.contains(&"NEXUS_MCP_REMOTE_BEARER"),
        "MCP secrets travel by variable name: {names:?}"
    );
    assert!(
        staging
            .recorded()
            .iter()
            .all(|entry| entry["canary"] != true),
        "the key was written to the process"
    );
    // Nothing the adapter produced holds it either.
    let printed = format!(
        "{events:?}{:?}{:?}",
        staging.provider(),
        session.capabilities()
    );
    assert!(!printed.contains(CANARY));
    // And a refusal does not echo what it refused.
    let mut bad = staging.spec("plain");
    bad.mcp_servers.insert(
        "x".to_owned(),
        McpServerSpec::Stdio {
            command: "x".to_owned(),
            args: vec![format!("--token={CANARY}")],
            env: BTreeMap::new(),
        },
    );
    let error = staging.provider().open(bad).await.err().expect("refused");
    assert_eq!(error.kind(), "invalid_request");
    assert!(!error.to_string().contains(CANARY) && !format!("{error:?}").contains(CANARY));
}

#[tokio::test]
async fn the_process_starts_from_an_empty_environment_plus_the_allowlist() {
    let staging = Staging::new();
    let session = open(&staging, "plain").await;
    session.close().await.unwrap();
    let start = staging.start();
    let names: Vec<&str> = start["env_names"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    // The test runs under cargo, whose own variables (and the runner's secrets, on
    // a developer machine) must not reach the child.
    let leaked: Vec<&&str> = names
        .iter()
        .filter(|name| {
            name.starts_with("CARGO")
                || name.starts_with("RUST")
                || name.contains("TOKEN")
                || name.contains("SECRET")
        })
        .collect();
    assert!(
        leaked.is_empty(),
        "the host's environment leaked: {leaked:?}"
    );
    assert!(names.contains(&"CODEX_HOME") && names.contains(&"PATH"));
    assert!(
        !names.contains(&"CODEX_API_KEY"),
        "no key is set when no credential is configured"
    );
}

// ---------------------------------------------------------------------------
// MCP approvals, byte for byte
// ---------------------------------------------------------------------------

async fn permission_ask(
    stream: &mut EventStream,
) -> (String, Vec<PermissionScope>, Option<String>) {
    match next_matching(stream, |event| {
        matches!(event, AgentEvent::PermissionAsk { .. })
    })
    .await
    {
        AgentEvent::PermissionAsk {
            request_id,
            scopes,
            tool_call_id,
            ..
        } => (request_id, scopes, tool_call_id),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn an_mcp_approval_accepted_is_answered_exactly() {
    let staging = Staging::new();
    let session = open(&staging, "permission_accordee").await;
    let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    let (request_id, scopes, call) = permission_ask(&mut stream).await;
    assert_eq!(
        scopes,
        [
            PermissionScope::Once,
            PermissionScope::Session,
            PermissionScope::Always
        ]
    );
    assert_eq!(
        call.as_deref(),
        Some("call_1"),
        "the ask names the call it is about"
    );
    session
        .answer_permission(&request_id, PermissionDecision::allow_once())
        .await
        .unwrap();
    let events = collect(stream).await;
    assert!(matches!(
        done(&events),
        AgentEvent::Done {
            stop_reason: StopReason::Completed,
            ..
        }
    ));
    session.close().await.unwrap();
    assert_eq!(
        staging.responses(),
        [r#"{"id":"srv_7","result":{"action":"accept","content":null}}"#],
        "the id comes back untouched"
    );
}

#[tokio::test]
async fn an_mcp_approval_declined_is_answered_exactly() {
    let staging = Staging::new();
    let session = open(&staging, "permission_refusee").await;
    let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    let (request_id, _, _) = permission_ask(&mut stream).await;
    session
        .answer_permission(&request_id, PermissionDecision::deny())
        .await
        .unwrap();
    let events = collect(stream).await;
    // The denied call did not succeed, and the turn still ends with `done`.
    assert!(events.iter().any(
        |event| matches!(event, AgentEvent::ToolResult { id, is_error: true, .. } if id == "call_1")
    ));
    assert!(matches!(
        done(&events),
        AgentEvent::Done {
            is_error: false,
            ..
        }
    ));
    session.close().await.unwrap();
    assert_eq!(
        staging.responses(),
        [r#"{"id":42,"result":{"action":"decline","content":null}}"#],
        "an integer id stays an integer"
    );
}

#[tokio::test]
async fn an_mcp_approval_cancelled_is_answered_exactly_and_interrupts_the_turn() {
    let staging = Staging::new();
    let session = open(&staging, "permission_cancel").await;
    let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    let (request_id, _, _) = permission_ask(&mut stream).await;
    session
        .answer_permission(
            &request_id,
            PermissionDecision::Deny {
                message: None,
                interrupt: true,
            },
        )
        .await
        .unwrap();
    let events = collect(stream).await;
    assert!(matches!(
        done(&events),
        AgentEvent::Done {
            stop_reason: StopReason::Interrupted,
            ..
        }
    ));
    session.close().await.unwrap();
    assert_eq!(
        staging.responses(),
        [r#"{"id":"srv_8","result":{"action":"cancel","content":null}}"#]
    );
    assert_eq!(staging.requests("turn/interrupt").len(), 1);
}

#[tokio::test]
async fn a_persistent_approval_carries_its_scope() {
    let staging = Staging::new();
    let session = open(&staging, "permission_persist").await;
    let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    let (request_id, _, _) = permission_ask(&mut stream).await;
    let decision = PermissionDecision::Allow {
        scope: PermissionScope::Always,
        updated_input: Some(json!({"x": 1})),
    };
    // Codex cannot rewrite the input of an approved call: refused, still pending.
    assert_eq!(
        session
            .answer_permission(&request_id, decision)
            .await
            .unwrap_err(),
        ProviderError::unsupported("permission_updated_input")
    );
    session
        .answer_permission(
            &request_id,
            PermissionDecision::Allow {
                scope: PermissionScope::Always,
                updated_input: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        done(&collect(stream).await),
        AgentEvent::Done { .. }
    ));
    session.close().await.unwrap();
    assert_eq!(
        staging.responses(),
        [
            r#"{"id":"srv_10","result":{"_meta":{"persist":"always"},"action":"accept","content":null}}"#
        ]
    );
}

#[tokio::test]
async fn a_command_approval_is_answered_with_a_decision_and_offers_what_the_server_offers() {
    let staging = Staging::new();
    let session = open(&staging, "command_approval").await;
    let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    let (request_id, scopes, call) = permission_ask(&mut stream).await;
    assert_eq!(
        scopes,
        [PermissionScope::Once, PermissionScope::Session],
        "Always is not offered by a command approval"
    );
    assert_eq!(call.as_deref(), Some("cmd_1"));
    let always = PermissionDecision::Allow {
        scope: PermissionScope::Always,
        updated_input: None,
    };
    assert_eq!(
        session
            .answer_permission(&request_id, always)
            .await
            .unwrap_err(),
        ProviderError::unsupported("permission_scope")
    );
    session
        .answer_permission(
            &request_id,
            PermissionDecision::Allow {
                scope: PermissionScope::Session,
                updated_input: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        done(&collect(stream).await),
        AgentEvent::Done { .. }
    ));
    session.close().await.unwrap();
    assert_eq!(
        staging.responses(),
        [r#"{"id":11,"result":{"decision":"acceptForSession"}}"#]
    );
}

#[tokio::test]
async fn what_the_adapter_cannot_serve_is_refused_and_never_hangs_the_turn() {
    let staging = Staging::new();
    let session = open(&staging, "unknown_requests").await;
    let mut oob = session.out_of_band().expect("the out-of-band stream");
    let events = turn(&*session).await;
    assert!(matches!(done(&events), AgentEvent::Done { .. }));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::PermissionAsk { .. })),
        "no permission is raised for what nobody can fill in"
    );
    // The notices come while the turn runs, so they travel on its stream.
    let _ = &mut oob;
    let notices: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ProviderNotice { kind, .. } => Some(kind.clone()),
            _ => None,
        })
        .collect();
    assert!(
        notices.contains(&"mcp_elicitation_declined".to_owned()),
        "{notices:?}"
    );
    assert!(
        notices.contains(&"unsupported_server_request".to_owned()),
        "{notices:?}"
    );
    session.close().await.unwrap();
    let responses = staging.responses();
    assert_eq!(
        responses[0],
        r#"{"id":"srv_20","result":{"action":"decline","content":null}}"#
    );
    assert!(
        responses[1].contains(r#""id":"srv_21""#) && responses[1].contains("-32601"),
        "{}",
        responses[1]
    );
}

#[tokio::test]
async fn an_elicitation_outside_a_turn_goes_out_of_band_and_can_be_answered() {
    let staging = Staging::new();
    let session = open(&staging, "permission_hors_tour").await;
    let mut oob = session.out_of_band().expect("first call");
    assert!(session.out_of_band().is_none(), "a single consumer");
    let ask = next_matching(&mut oob, |event| {
        matches!(event, AgentEvent::PermissionAsk { .. })
    })
    .await;
    let AgentEvent::PermissionAsk {
        request_id,
        scopes,
        tool_name,
        tool_call_id,
        ..
    } = ask
    else {
        unreachable!()
    };
    assert_eq!(tool_name, "mcp__fake__write");
    assert_eq!(scopes, [PermissionScope::Once, PermissionScope::Session]);
    assert_eq!(tool_call_id, None, "no call is running");
    session
        .answer_permission(&request_id, PermissionDecision::allow_once())
        .await
        .unwrap();
    assert_eq!(
        session
            .answer_permission(&request_id, PermissionDecision::allow_once())
            .await
            .unwrap_err()
            .kind(),
        "invalid_request"
    );
    session.close().await.unwrap();
    assert_eq!(
        staging.responses(),
        [r#"{"id":"srv_9","result":{"action":"accept","content":null}}"#]
    );
}

#[tokio::test]
async fn the_local_policy_answers_for_the_requests_it_decides() {
    // plan_only: a tool that is not a read is declined without asking anybody.
    let staging = Staging::new();
    let mut spec = staging.spec("permission_refusee");
    spec.policy = ToolPolicy::new(PolicyMode::PlanOnly);
    let session = staging.provider().open(spec).await.unwrap();
    let events = turn(&*session).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::PermissionAsk { .. }))
    );
    assert!(matches!(
        done(&events),
        AgentEvent::Done {
            is_error: false,
            ..
        }
    ));
    session.close().await.unwrap();
    assert_eq!(
        staging.responses(),
        [r#"{"id":42,"result":{"action":"decline","content":null}}"#]
    );

    // An `allow` pattern accepts it.
    let staging = Staging::new();
    let mut spec = staging.spec("permission_accordee");
    spec.policy = ToolPolicy::from_patterns(PolicyMode::Ask, &["mcp__fake__write"], &[]).unwrap();
    let session = staging.provider().open(spec).await.unwrap();
    let events = turn(&*session).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::PermissionAsk { .. }))
    );
    session.close().await.unwrap();
    assert_eq!(
        staging.responses(),
        [r#"{"id":"srv_7","result":{"action":"accept","content":null}}"#]
    );
}

// ---------------------------------------------------------------------------
// Policy, model, resume, limits
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_policy_modes_map_to_the_two_codex_axes_and_never_to_full_access() {
    for (mode, approval, sandbox, policy_type) in [
        (
            PolicyMode::Ask,
            "on-request",
            "workspaceWrite",
            "workspaceWrite",
        ),
        (
            PolicyMode::AutoEdits,
            "on-request",
            "workspaceWrite",
            "workspaceWrite",
        ),
        (PolicyMode::PlanOnly, "on-request", "readOnly", "readOnly"),
        (
            PolicyMode::Trust,
            "never",
            "workspaceWrite",
            "workspaceWrite",
        ),
    ] {
        let staging = Staging::new();
        let mut spec = staging.spec("plain");
        spec.policy = ToolPolicy::new(mode);
        let session = staging.provider().open(spec).await.expect("opens");
        turn(&*session).await;
        session.close().await.unwrap();
        let thread = &staging.requests("thread/start")[0]["params"];
        assert_eq!(
            (
                thread["approvalPolicy"].as_str(),
                thread["sandbox"].as_str()
            ),
            (Some(approval), Some(sandbox)),
            "{mode:?}"
        );
        let turn_params = &staging.requests("turn/start")[0]["params"];
        assert_eq!(turn_params["approvalPolicy"], approval, "{mode:?}");
        assert_eq!(
            turn_params["sandboxPolicy"]["type"], policy_type,
            "{mode:?}"
        );
        let text = std::fs::read_to_string(&staging.record).unwrap();
        assert!(
            !text.contains("dangerFullAccess") && !text.contains("externalSandbox"),
            "{mode:?}"
        );
    }
}

#[tokio::test]
async fn model_and_policy_changes_apply_from_the_next_turn() {
    let staging = Staging::new();
    let session = open(&staging, "tour_concurrent").await;
    session.set_model("gpt-other").await.unwrap();
    session
        .set_policy_mode(PolicyMode::PlanOnly, None)
        .await
        .unwrap();
    let stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    let mut stream = stream;
    next_matching(&mut stream, |event| {
        matches!(event, AgentEvent::Delta { .. })
    })
    .await;
    session.interrupt(InterruptScope::TurnOnly).await.unwrap();
    collect(stream).await;
    let params = &staging.requests("turn/start")[0]["params"];
    assert_eq!(params["model"], "gpt-other");
    assert_eq!(params["sandboxPolicy"]["type"], "readOnly");
    assert!(session.set_model("  ").await.is_err());
    session.close().await.unwrap();

    // A ceiling holds for the live change too.
    let capped = Staging::new();
    let mut spec = capped.spec("plain");
    spec.policy = ToolPolicy::new(PolicyMode::Ask);
    spec.policy_ceiling = Some(ToolPolicy::new(PolicyMode::Ask));
    let session = capped.provider().open(spec).await.unwrap();
    assert_eq!(
        session
            .set_policy_mode(PolicyMode::Trust, None)
            .await
            .unwrap_err(),
        ProviderError::unsupported("policy_ceiling")
    );
    session
        .set_policy_mode(PolicyMode::PlanOnly, None)
        .await
        .unwrap();
    session.close().await.unwrap();
}

#[tokio::test]
async fn resume_reopens_the_thread_of_the_token() {
    let staging = Staging::new();
    let token = ResumeToken::new(ProviderKind::Codex, 1, json!({ "thread_id": "thr_prev" }));
    let session = staging
        .provider()
        .resume(staging.spec("reprise"), token)
        .await
        .expect("resumes");
    assert!(
        staging.requests("thread/start").is_empty(),
        "a resume starts no new thread"
    );
    let resume = &staging.requests("thread/resume")[0]["params"];
    assert_eq!(resume["threadId"], "thr_prev");
    let events = turn(&*session).await;
    assert!(
        matches!(done(&events), AgentEvent::Done { provider_session_id: Some(id), .. } if id == "thr_prev")
    );
    assert_eq!(
        session.resume_token().unwrap().to_wire(),
        r#"{"k":"codex","v":1,"d":{"thread_id":"thr_prev"}}"#
    );
    // The replayed usage of the thread is not charged to the new turn.
    let AgentEvent::Done { usage, .. } = done(&events) else {
        unreachable!()
    };
    assert_eq!(
        usage.output_tokens,
        Some(60),
        "260 - 200 cumulative output tokens: {usage:?}"
    );
    session.close().await.unwrap();

    // A token that carries no usable thread id never starts a process.
    let fresh = Staging::new();
    for data in [
        json!({}),
        json!({"thread_id": ""}),
        json!({"thread_id": "a b"}),
        json!({"thread_id": 5}),
    ] {
        let error = fresh
            .provider()
            .resume(
                fresh.spec("reprise"),
                ResumeToken::new(ProviderKind::Codex, 1, data),
            )
            .await
            .err()
            .expect("refused");
        assert_eq!(error.kind(), "invalid_request");
    }
    assert!(fresh.recorded().is_empty());
}

#[tokio::test]
async fn limits_codex_cannot_keep_are_refused_before_anything_starts() {
    let staging = Staging::new();
    for tweak in [
        (|spec: &mut SessionSpec| spec.max_turns = Some(3)) as fn(&mut SessionSpec),
        |spec| spec.limits.max_tokens = Some(10),
        |spec| spec.limits.max_cost_usd = Some(1.0),
        |spec| spec.limits.max_tool_iterations = Some(2),
    ] {
        let mut spec = staging.spec("plain");
        tweak(&mut spec);
        let error = staging.provider().open(spec).await.err().expect("refused");
        assert_eq!(error, ProviderError::unsupported("limits"));
    }
    assert!(staging.recorded().is_empty());
}

#[tokio::test]
async fn a_turn_timeout_ends_the_turn_with_a_typed_timeout() {
    let staging = Staging::new();
    let mut spec = staging.spec("interruption_en_flux");
    spec.limits.turn_timeout_ms = Some(300);
    let session = staging.provider().open(spec).await.unwrap();
    let events = turn(&*session).await;
    let AgentEvent::Done {
        stop_reason,
        is_error,
        error,
        ..
    } = done(&events)
    else {
        panic!("{events:?}")
    };
    assert_eq!((*stop_reason, *is_error), (StopReason::Error, true));
    assert_eq!(*error, Some(ProviderError::Timeout { after_ms: 300 }));
    session.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Turns, errors, the process
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_turn_at_a_time_and_nothing_else_is_sent_for_the_refused_one() {
    let staging = Staging::new();
    let session = open(&staging, "interruption_en_flux").await;
    let mut stream = session.send_turn(TurnInput::text("one")).await.unwrap();
    next_matching(&mut stream, |event| {
        matches!(event, AgentEvent::Delta { .. })
    })
    .await;
    assert_eq!(
        session.send_turn(TurnInput::text("two")).await.err(),
        Some(ProviderError::TurnInProgress)
    );
    let outcome = session
        .interrupt(InterruptScope::TurnAndTools)
        .await
        .unwrap();
    assert!(outcome.turn_interrupted);
    let events = collect(stream).await;
    assert!(matches!(
        done(&events),
        AgentEvent::Done {
            stop_reason: StopReason::Interrupted,
            ..
        }
    ));
    assert_eq!(staging.requests("turn/start").len(), 1);
    assert_eq!(staging.requests("turn/interrupt").len(), 1);
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_failed_turn_is_a_done_carrying_the_classified_error() {
    let staging = Staging::new();
    let session = open(&staging, "erreur_retryable").await;
    let events = turn(&*session).await;
    let AgentEvent::Done {
        stop_reason,
        is_error,
        error,
        ..
    } = done(&events)
    else {
        panic!("{events:?}")
    };
    assert_eq!((*stop_reason, *is_error), (StopReason::Error, true));
    let error = error.clone().expect("classified");
    assert_eq!(
        error,
        ProviderError::RateLimited {
            retry_after_ms: None
        }
    );
    assert!(error.retryable());
    // The session survives and the second turn counts its own usage.
    let events = turn(&*session).await;
    assert!(matches!(
        done(&events),
        AgentEvent::Done {
            is_error: false,
            ..
        }
    ));
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_dead_process_is_a_terminal_non_retryable_error_and_the_session_stays_dead() {
    let staging = Staging::new();
    let session = open(&staging, "processus_mort").await;
    let events = turn(&*session).await;
    let AgentEvent::Error { error } = done(&events) else {
        panic!("{events:?}")
    };
    assert_eq!(*error, ProviderError::ProcessExited { code: Some(3) });
    assert!(!error.retryable());
    assert_eq!(
        session.send_turn(TurnInput::text("again")).await.err(),
        Some(ProviderError::ProcessExited { code: Some(3) })
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_started_session_says_what_it_is_and_what_it_ignores() {
    let staging = Staging::new();
    let mut spec = staging.spec("plain");
    spec.policy = ToolPolicy::from_patterns(PolicyMode::Ask, &["Bash(git *)"], &[]).unwrap();
    let provider = staging.provider();
    let session = provider.open(spec).await.unwrap();
    let mut oob = session.out_of_band().unwrap();
    let started = next_matching(&mut oob, |event| {
        matches!(event, AgentEvent::SessionStarted { .. })
    })
    .await;
    let AgentEvent::SessionStarted {
        provider_session_id,
        model,
        policy_mode,
        native_mode,
        ..
    } = started
    else {
        unreachable!()
    };
    assert_eq!(provider_session_id.as_deref(), Some("thr_fake1"));
    assert_eq!(model.as_deref(), Some(MODEL));
    assert_eq!(policy_mode, Some(PolicyMode::Ask));
    assert_eq!(native_mode.as_deref(), Some("on-request/workspaceWrite"));
    let caps = session.capabilities();
    assert_eq!(caps, &provider.capabilities(Some(MODEL)));
    assert!(
        caps.secret_isolation && caps.per_session_mcp && caps.compaction_signal && caps.thinking
    );
    assert_eq!(caps.hooks, nexus_claude::agent::HookSupport::None);
    assert_eq!(caps.sandbox, nexus_claude::agent::SandboxLevel::Workspace);
    // The capabilities of an unconfigured instance never invent a window or a cost.
    let bare = CodexProvider::new(CodexConfig::new("bare")).capabilities(None);
    assert_eq!((bare.context_window, bare.cost), (None, CostBasis::Unknown));
    session.close().await.unwrap();
}

#[tokio::test]
async fn unsupported_calls_name_their_capability() {
    let staging = Staging::new();
    let session = open(&staging, "plain").await;
    assert_eq!(
        session.cancel_tools(CancelScope::All).await.unwrap_err(),
        ProviderError::unsupported("tool_cancel")
    );
    assert_eq!(
        session
            .answer_question("q", QuestionAnswer::Cancelled)
            .await
            .unwrap_err(),
        ProviderError::unsupported("native_question")
    );
    let mut image = TurnInput::text("look");
    image.blocks.push(nexus_claude::agent::InputBlock::Image {
        media_type: "image/png".to_owned(),
        data_base64: "AAAA".to_owned(),
    });
    assert_eq!(
        session.send_turn(image).await.err(),
        Some(ProviderError::unsupported("images"))
    );
    session.close().await.unwrap();
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(i32::try_from(pid).unwrap(), 0) == 0 }
}

#[cfg(unix)]
#[tokio::test]
async fn close_kills_the_process_and_every_descendant() {
    let staging = Staging::new();
    let session = open(&staging, "descendants").await;
    // The fake starts two children (one in its own process group) once the
    // handshake is over.
    let mut pids = Vec::new();
    for _ in 0..200 {
        pids = staging
            .recorded()
            .iter()
            .filter(|entry| entry["kind"] == "child")
            .map(|entry| entry["pid"].as_u64().unwrap() as u32)
            .collect();
        if pids.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(pids.len(), 2, "the fake started its children");
    let fake_pid = staging.start()["pid"].as_u64().unwrap() as u32;
    assert!(alive(fake_pid) && pids.iter().all(|pid| alive(*pid)));
    session.close().await.unwrap();
    session.close().await.expect("close is idempotent");
    for _ in 0..200 {
        if !alive(fake_pid) && pids.iter().all(|pid| !alive(*pid)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    for pid in pids.iter().chain([&fake_pid]) {
        if alive(*pid) {
            // Do not leave it behind a failing test.
            // SAFETY: SIGKILL to a process we started.
            unsafe { libc::kill(i32::try_from(*pid).unwrap(), libc::SIGKILL) };
        }
    }
    panic!("close left a process of the tree alive");
}

#[tokio::test]
async fn an_old_codex_without_app_server_fails_open_with_a_typed_error() {
    // fake_codex without the transcript variable behaves like a codex that cannot
    // serve: here, one that exits at once.
    let staging = Staging::new();
    let mut spec = SessionSpec::new(staging._cwd.path());
    spec.model = Some(MODEL.to_owned());
    let error = staging
        .provider()
        .open(spec)
        .await
        .err()
        .expect("open fails");
    assert!(
        matches!(
            error,
            ProviderError::ProcessExited { .. } | ProviderError::Protocol { .. }
        ),
        "{error:?}"
    );
}
