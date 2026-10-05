//! The mandatory security scenarios (N15: decisions A32, A33, A35), played against
//! every provider that ships and against a deliberately faulty one.
//!
//! **Order matters, and it is the point.** A check that has never failed proves
//! nothing, so the first half of this file runs each scenario against
//! [`LeakyProvider`] — a provider that inherits the host environment, writes the
//! secret on its command line, leaks it in an error, accepts `trust` without a
//! sandbox and ignores a policy ceiling — and requires a [`Violation`] for **every**
//! scenario. Only then does the second half require the real providers to pass:
//! Claude Code (`fake_claude`), Codex (`fake_codex`), ACP (`fake_acp`) and the
//! native harness (`fake_openai` + `fake_mcp`).
//!
//! What this does not prove: nothing here talked to a real Codex, a real ACP agent
//! or a real model endpoint. Each provider is exercised through its fake
//! executable, which records what the child was given.

#[path = "support/fake_openai.rs"]
mod fake_openai;
#[path = "support/native.rs"]
mod native;
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use fake_openai::FakeOpenAi;
use native::*;
use nexus_claude::agent::{
    AgentEvent, AgentProvider, AgentSession, Capabilities, McpServerSpec, ModelInfo, PolicyMode,
    ProviderError, ProviderHealth, ProviderKind, ResumeToken, SessionSpec, ToolOutput, ToolPolicy,
};
use nexus_claude::model::EndpointQuirks;
use nexus_claude::providers::acp::{AcpConfig, AcpProvider, SUPPORTED_PROTOCOL_VERSION};
use nexus_claude::providers::claude_code::{ClaudeCodeConfig, ClaudeCodeProvider};
use nexus_claude::providers::codex::{CodexConfig, CodexProvider, MIN_APP_SERVER_VERSION};
use nexus_claude::providers::native::{NativeConfig, NativeProvider};
use nexus_claude::testkit::security::{
    HOST_VARIABLE, LaunchObservation, SECRET_SENTINEL, SecurityScenario, SecurityStaging,
    SecurityTarget, SecurityVerdict, ToolProfileProbe, Violation, run_all, run_scenario,
};
use nexus_claude::testkit::{Scenario, ScriptedProvider};
use nexus_claude::testkit::{Script, scripted_target};
use serde_json::{Value, json};
use support::*;

const FAKE_CODEX: &str = env!("CARGO_BIN_EXE_fake_codex");
const FAKE_ACP: &str = env!("CARGO_BIN_EXE_fake_acp");
const FAKE_CLAUDE: &str = env!("CARGO_BIN_EXE_fake_claude");
const MODEL: &str = "fake-model";

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn codex_sessions() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/transcripts/codex")
        .join(MIN_APP_SERVER_VERSION)
        .join("sessions")
}

fn acp_sessions() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/transcripts/acp")
        .join(SUPPORTED_PROTOCOL_VERSION.to_string())
        .join("sessions")
}

/// An MCP server entry that carries the secret in its environment: the place
/// credentials of MCP servers really live.
fn secret_mcp_server() -> McpServerSpec {
    McpServerSpec::Stdio {
        command: "po-mcp".into(),
        args: Vec::new(),
        env: [("DB_PASSWORD".to_owned(), SECRET_SENTINEL.to_owned())].into(),
    }
}

fn read_jsonl(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// What `fake_codex` / `fake_acp` recorded at start. They never write the secret:
/// they record whether they *saw* it on argv, so that fact is turned back into the
/// sentinel here for the scenario to find.
fn observation_from_start(start: &Value) -> LaunchObservation {
    let mut argv = strings(&start["argv"]);
    if start["canary_in_argv"] == json!(true) {
        argv.push(format!("--secret={SECRET_SENTINEL}"));
    }
    LaunchObservation {
        argv,
        env_names: strings(&start["env_names"]),
        home: start["home"].as_str().map(str::to_owned),
    }
}

// ---------------------------------------------------------------------------
// The faulty provider
// ---------------------------------------------------------------------------

/// Everything the suite forbids, in one provider. It wraps a scripted provider for
/// the session behaviour and adds the faults around `open`.
struct LeakyProvider {
    inner: ScriptedProvider,
    record: PathBuf,
}

fn leaky_capabilities() -> Capabilities {
    // No sandbox: `trust` must be refused. Third-party kind, below. MCP servers are
    // accepted: a provider that refused them would never open the session the
    // scenarios observe, and the scenarios would be red by accident, not by the
    // fault they are meant to catch.
    let mut capabilities = Capabilities::none();
    capabilities.per_session_mcp = true;
    capabilities.tools = true;
    capabilities
}

impl LeakyProvider {
    fn new(record: PathBuf) -> Self {
        let mut script: Script =
            scripted_target(leaky_capabilities()).script_for(Scenario::TourTexteSimple);
        script.capabilities = leaky_capabilities();
        Self {
            inner: ScriptedProvider::new("leaky", script),
            record,
        }
    }
}

impl std::fmt::Debug for LeakyProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Fault: a Debug that prints the credential.
        write!(f, "LeakyProvider {{ api_key: {SECRET_SENTINEL:?} }}")
    }
}

#[async_trait]
impl AgentProvider for LeakyProvider {
    fn id(&self) -> &str {
        "leaky"
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Acp
    }

    async fn health(&self) -> ProviderHealth {
        self.inner.health().await
    }

    async fn catalog(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.inner.catalog().await
    }

    fn capabilities(&self, model: Option<&str>) -> Capabilities {
        self.inner.capabilities(model)
    }

    async fn open(&self, mut spec: SessionSpec) -> Result<Arc<dyn AgentSession>, ProviderError> {
        // Fault 1 and 2: a child that inherits the WHOLE host environment and gets
        // the secret on its command line.
        let status = std::process::Command::new(FAKE_CLAUDE)
            .arg(format!("--api-key={SECRET_SENTINEL}"))
            .env("FAKE_CLAUDE_ARGS_OUT", &self.record)
            .env("FAKE_CLAUDE_MAX_RUNTIME_MS", "5000")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status();
        let _ = status;
        // Fault 3: no refusal of `trust` without a sandbox, and the ceiling is ignored.
        spec.policy = ToolPolicy::new(PolicyMode::Ask);
        spec.policy_ceiling = None;
        self.inner.open(spec).await
    }

    async fn resume(
        &self,
        spec: SessionSpec,
        token: ResumeToken,
    ) -> Result<Arc<dyn AgentSession>, ProviderError> {
        self.inner.resume(spec, token).await
    }
}

struct LeakyTarget {
    dir: tempfile::TempDir,
    /// Whether the provider's `Debug` leaks the secret. Switched off to prove the
    /// error-message surface red on its own.
    leak_debug: bool,
}

impl LeakyTarget {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("a temp dir"),
            leak_debug: true,
        }
    }

    fn leaking_only_through_errors() -> Self {
        Self {
            leak_debug: false,
            ..Self::new()
        }
    }
}

#[async_trait]
impl SecurityTarget for LeakyTarget {
    fn name(&self) -> &str {
        "leaky (deliberately faulty)"
    }

    async fn stage(&self) -> Option<SecurityStaging> {
        let record = self.dir.path().join("leaky-invocation.json");
        let provider = Arc::new(LeakyProvider::new(record.clone()));
        let mut spec = SessionSpec::new(self.dir.path());
        spec.env
            .set
            .insert("SESSION_SECRET".into(), SECRET_SENTINEL.into());
        spec.mcp_servers.insert("po".into(), secret_mcp_server());
        let rendered = format!("{provider:?}");
        Some(SecurityStaging {
            provider,
            spec,
            observe: Box::new(move |_| {
                let raw: Value =
                    serde_json::from_str(&std::fs::read_to_string(&record).ok()?).ok()?;
                Some(LaunchObservation {
                    argv: strings(&raw["argv"]),
                    env_names: strings(&raw["env_names"]),
                    home: None,
                })
            }),
            secret_subjects: if self.leak_debug {
                vec![rendered]
            } else {
                Vec::new()
            },
            guard: None,
        })
    }

    async fn provoke_errors(&self) -> Vec<ProviderError> {
        // Fault 4: an error message that carries the credential. Built as the
        // variant itself: the `ProviderError::invalid(..)` constructors redact
        // key-shaped text (A10), so a provider that goes through them is already
        // protected — the fault is a provider that does not.
        vec![ProviderError::EndpointUnreachable {
            detail: format!("endpoint rejected key {SECRET_SENTINEL}"),
        }]
    }

    async fn out_of_profile_probe(&self) -> Option<ToolProfileProbe> {
        // Fault 5: the forbidden tool ran.
        Some(ToolProfileProbe {
            events: Vec::new(),
            executions: 1,
        })
    }
}

/// The ground truth of the red half: every scenario must catch the faulty provider.
#[tokio::test]
async fn every_scenario_is_red_against_the_faulty_provider() {
    let target = LeakyTarget::new();
    for scenario in SecurityScenario::ALL {
        let outcome = run_scenario(&target, scenario).await;
        assert!(
            matches!(outcome, Err(Violation(_))),
            "{scenario} did not catch the faulty provider: {outcome:?} — a check that cannot \
             fail proves nothing"
        );
    }
}

/// Each scenario must be red for its **own** reason, not by accident of another.
#[tokio::test]
async fn each_scenario_names_the_fault_it_found() {
    let target = LeakyTarget::new();
    let reason = |scenario| {
        let target = &target;
        async move { run_scenario(target, scenario).await.unwrap_err().0 }
    };
    assert!(
        reason(SecurityScenario::TrustWithoutSandbox)
            .await
            .contains("trust")
    );
    assert!(
        reason(SecurityScenario::UnknownPolicyRefused)
            .await
            .contains("ceiling")
    );
    assert!(
        reason(SecurityScenario::EnvironmentIsolation)
            .await
            .contains(HOST_VARIABLE)
    );
    assert!(
        reason(SecurityScenario::NoSecretInArgv)
            .await
            .contains("command line")
    );
    assert!(
        reason(SecurityScenario::NoSecretInErrors)
            .await
            .contains("secret appears")
    );
    assert!(
        reason(SecurityScenario::OutOfProfileToolRefused)
            .await
            .contains("outside")
    );
}

/// The error-message surface, red on its own: the `Debug` of everything is clean,
/// only an error carries the secret.
#[tokio::test]
async fn an_error_that_carries_the_secret_is_caught_on_its_own() {
    let target = LeakyTarget::leaking_only_through_errors();
    let violation = run_scenario(&target, SecurityScenario::NoSecretInErrors)
        .await
        .unwrap_err();
    assert!(
        violation.0.contains("Display of an error"),
        "wrong surface: {violation}"
    );
}

// ---------------------------------------------------------------------------
// Claude Code — fake_claude
// ---------------------------------------------------------------------------

struct ClaudeTarget;

fn claude_config(fake: Option<&FakeCli>) -> ClaudeCodeConfig {
    let mut default_model = ModelInfo::new("fake-claude");
    default_model.is_default = true;
    let mut config = ClaudeCodeConfig::default();
    config.models = vec![default_model];
    config.cli_path = fake.map(|fake| fake.cli_path().to_path_buf());
    config
}

#[async_trait]
impl SecurityTarget for ClaudeTarget {
    fn name(&self) -> &str {
        "claude_code (fake_claude)"
    }

    async fn stage(&self) -> Option<SecurityStaging> {
        let fake = Arc::new(
            Transcript::new()
                .await_stdin()
                .init("sess-security")
                .assistant_text("ok")
                .result_ok("ok")
                .wait_eof()
                .build(),
        );
        let config = claude_config(Some(&fake));
        let rendered = format!("{config:?}");
        let provider = Arc::new(ClaudeCodeProvider::new(config));
        let mut spec = SessionSpec::new(fake.dir());
        spec.env.set = fake.options().env.into_iter().collect();
        spec.env
            .set
            .insert("SESSION_SECRET".into(), SECRET_SENTINEL.into());
        spec.mcp_servers.insert("po".into(), secret_mcp_server());
        let observed = fake.clone();
        Some(SecurityStaging {
            provider,
            spec,
            observe: Box::new(move |_| {
                let invocation = observed.invocation();
                Some(LaunchObservation {
                    argv: invocation.args(),
                    env_names: strings(&invocation.raw()["env_names"]),
                    home: None,
                })
            }),
            secret_subjects: vec![rendered],
            guard: Some(Box::new(fake)),
        })
    }

    async fn provoke_errors(&self) -> Vec<ProviderError> {
        // The CLI is missing while the session carries the secret.
        let mut config = claude_config(None);
        config.cli_path = Some(PathBuf::from("/nonexistent/claude"));
        let mut spec = SessionSpec::new(std::env::temp_dir());
        spec.env
            .set
            .insert("SESSION_SECRET".into(), SECRET_SENTINEL.into());
        spec.mcp_servers.insert("po".into(), secret_mcp_server());
        let mut errors = Vec::new();
        if let Err(error) = ClaudeCodeProvider::new(config).open(spec).await {
            errors.push(error);
        }
        errors
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claude_code_passes_the_security_scenarios() {
    let report = run_all(&ClaudeTarget).await;
    println!("{}", report.summary());
    report.assert_secure();
}

// ---------------------------------------------------------------------------
// Codex — fake_codex
// ---------------------------------------------------------------------------

struct CodexTarget;

fn codex_config(home: &Path, program: &str) -> CodexConfig {
    let mut config = CodexConfig::new("codex-security");
    config.program = PathBuf::from(program);
    config.codex_home = home.to_path_buf();
    config.default_model = Some(MODEL.to_owned());
    // The fake records whether it *saw* this value on argv or in an environment value.
    config
        .env_set
        .insert("FAKE_CODEX_CANARY".to_owned(), SECRET_SENTINEL.to_owned());
    config
}

#[async_trait]
impl SecurityTarget for CodexTarget {
    fn name(&self) -> &str {
        "codex (fake_codex)"
    }

    async fn stage(&self) -> Option<SecurityStaging> {
        let dir = tempfile::tempdir().ok()?;
        let record = dir.path().join("record.jsonl");
        let config = codex_config(&dir.path().join("codex-home"), FAKE_CODEX);
        let rendered = format!("{config:?}");
        let provider = Arc::new(CodexProvider::new(config));
        let mut spec = SessionSpec::new(dir.path());
        spec.model = Some(MODEL.to_owned());
        spec.env.set.insert(
            "FAKE_CODEX_TRANSCRIPT".into(),
            codex_sessions().join("plain.jsonl").display().to_string(),
        );
        spec.env
            .set
            .insert("FAKE_CODEX_RECORD".into(), record.display().to_string());
        spec.env
            .set
            .insert("SESSION_SECRET".into(), SECRET_SENTINEL.into());
        spec.mcp_servers.insert("po".into(), secret_mcp_server());
        let recorded = record.clone();
        Some(SecurityStaging {
            provider,
            spec,
            observe: Box::new(move |_| {
                read_jsonl(&recorded)
                    .into_iter()
                    .find(|entry| entry["kind"] == "start")
                    .map(|start| observation_from_start(&start))
            }),
            secret_subjects: vec![rendered],
            guard: Some(Box::new(dir)),
        })
    }

    async fn provoke_errors(&self) -> Vec<ProviderError> {
        let dir = tempfile::tempdir().expect("a temp dir");
        let config = codex_config(&dir.path().join("codex-home"), "/nonexistent/codex");
        let mut spec = SessionSpec::new(dir.path());
        spec.env
            .set
            .insert("SESSION_SECRET".into(), SECRET_SENTINEL.into());
        spec.mcp_servers.insert("po".into(), secret_mcp_server());
        let mut errors = Vec::new();
        let provider = CodexProvider::new(config);
        if let Some(error) = provider.health().await.error {
            errors.push(error);
        }
        if let Err(error) = provider.open(spec).await {
            errors.push(error);
        }
        errors
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_passes_the_security_scenarios() {
    let report = run_all(&CodexTarget).await;
    println!("{}", report.summary());
    report.assert_secure();
}

// ---------------------------------------------------------------------------
// ACP — fake_acp
// ---------------------------------------------------------------------------

struct AcpTarget;

fn acp_config(command: Vec<String>, home: &Path) -> AcpConfig {
    let mut config = AcpConfig::new("acp-security", command);
    // The instance HOME the agent runs with: a temp dir, never the machine's data dir.
    config.home = home.join("acp-home");
    config.default_model = Some(MODEL.to_owned());
    config.env.insert(
        "FAKE_ACP_TRANSCRIPT".to_owned(),
        acp_sessions().join("health.jsonl").display().to_string(),
    );
    config
        .env
        .insert("FAKE_ACP_CANARY".to_owned(), SECRET_SENTINEL.to_owned());
    config
}

#[async_trait]
impl SecurityTarget for AcpTarget {
    fn name(&self) -> &str {
        "acp (fake_acp)"
    }

    async fn stage(&self) -> Option<SecurityStaging> {
        let dir = tempfile::tempdir().ok()?;
        let record = dir.path().join("record.jsonl");
        let config = acp_config(vec![FAKE_ACP.to_owned()], dir.path());
        let rendered = format!("{config:?}");
        let provider = Arc::new(AcpProvider::new(config));
        // The adapter learns what the agent offers through `health` first.
        provider.health().await;
        let mut spec = SessionSpec::new(dir.path());
        spec.model = Some(MODEL.to_owned());
        spec.env.set.insert(
            "FAKE_ACP_TRANSCRIPT".into(),
            acp_sessions().join("plain.jsonl").display().to_string(),
        );
        spec.env
            .set
            .insert("FAKE_ACP_RECORD".into(), record.display().to_string());
        spec.env
            .set
            .insert("SESSION_SECRET".into(), SECRET_SENTINEL.into());
        spec.mcp_servers.insert("po".into(), secret_mcp_server());
        let recorded = record.clone();
        Some(SecurityStaging {
            provider,
            spec,
            observe: Box::new(move |_| {
                read_jsonl(&recorded)
                    .into_iter()
                    .find(|entry| entry["kind"] == "start")
                    .map(|start| observation_from_start(&start))
            }),
            secret_subjects: vec![rendered],
            guard: Some(Box::new(dir)),
        })
    }

    async fn provoke_errors(&self) -> Vec<ProviderError> {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut spec = SessionSpec::new(dir.path());
        spec.env
            .set
            .insert("SESSION_SECRET".into(), SECRET_SENTINEL.into());
        spec.mcp_servers.insert("po".into(), secret_mcp_server());
        let mut errors = Vec::new();
        // The command itself carries a key-shaped argument: refused, nothing starts.
        let leaky = AcpProvider::new(acp_config(
            vec![FAKE_ACP.to_owned(), format!("--api-key={SECRET_SENTINEL}")],
            dir.path(),
        ));
        if let Some(error) = leaky.health().await.error {
            errors.push(error);
        }
        if let Err(error) = leaky.open(spec.clone()).await {
            errors.push(error);
        }
        // An agent that is not there.
        let missing = AcpProvider::new(acp_config(
            vec!["/nonexistent/agent".to_owned()],
            dir.path(),
        ));
        if let Err(error) = missing.open(spec).await {
            errors.push(error);
        }
        errors
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_passes_the_security_scenarios() {
    let report = run_all(&AcpTarget).await;
    println!("{}", report.summary());
    report.assert_secure();
}

// ---------------------------------------------------------------------------
// Native harness — fake_openai + fake_mcp
// ---------------------------------------------------------------------------

struct NativeTarget;

const TOOL_MESSAGE: &str = "\"role\":\"tool\"";

fn native_provider(server: &FakeOpenAi) -> Arc<NativeProvider> {
    let mut config = NativeConfig::new("native-security");
    config.default_model = Some("m".to_owned());
    Arc::new(NativeProvider::new(
        config,
        endpoint(server.base_url(), EndpointQuirks::deepseek()),
    ))
}

fn native_server(routes: Vec<Value>) -> FakeOpenAi {
    let mut all = vec![probe_route(true), models_route(128_000)];
    all.extend(routes);
    FakeOpenAi::start(json!(all))
}

fn tool_result_texts(events: &[AgentEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolResult { id, output, .. } => Some((
                id.clone(),
                match output {
                    Some(ToolOutput::Text(text)) => text.clone(),
                    other => format!("{other:?}"),
                },
            )),
            _ => None,
        })
        .collect()
}

#[async_trait]
impl SecurityTarget for NativeTarget {
    fn name(&self) -> &str {
        "native (fake_openai, fake_mcp over stdio)"
    }

    async fn stage(&self) -> Option<SecurityStaging> {
        // The harness has no process of its own; the processes it starts are its
        // MCP servers. The model is scripted to ask one of them for its environment
        // and its command line, which is what the scenarios observe.
        let server = native_server(vec![
            tool_reply(
                Some("security scenario"),
                &[
                    ("e1", "mcp__fake__env", json!({"name": "HOME"})),
                    ("a1", "mcp__fake__argv", json!({})),
                ],
                None,
                None,
            ),
            text_reply(Some(TOOL_MESSAGE), "inspected", None, None),
        ]);
        let provider = native_provider(&server);
        let dir = tempfile::tempdir().ok()?;
        let mut spec = SessionSpec::new(dir.path());
        spec.model = Some("m".to_owned());
        let mut mcp = stdio_mcp(None);
        if let McpServerSpec::Stdio { env, .. } = &mut mcp {
            env.insert("DB_PASSWORD".into(), SECRET_SENTINEL.into());
        }
        spec.mcp_servers.insert("fake".into(), mcp);
        spec.env
            .set
            .insert("SESSION_SECRET".into(), SECRET_SENTINEL.into());
        let rendered = format!("{provider:?}");
        Some(SecurityStaging {
            provider,
            spec,
            observe: Box::new(|events| {
                let results = tool_result_texts(events);
                let env = &results.iter().find(|(id, _)| id == "e1")?.1;
                let argv = &results.iter().find(|(id, _)| id == "a1")?.1;
                // First line: the names; then `HOME=<value>`.
                let mut lines = env.lines();
                let names = lines.next().unwrap_or_default();
                let home = lines
                    .find_map(|line| line.strip_prefix("HOME="))
                    .map(str::to_owned);
                Some(LaunchObservation {
                    argv: argv.split_whitespace().map(str::to_owned).collect(),
                    env_names: names
                        .split(|c: char| c == ',' || c.is_whitespace())
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned)
                        .collect(),
                    home,
                })
            }),
            secret_subjects: vec![rendered],
            guard: Some(Box::new((server, dir))),
        })
    }

    async fn provoke_errors(&self) -> Vec<ProviderError> {
        // An endpoint nobody listens on, and an MCP server over HTTP that does not
        // answer, whose header holds the secret.
        let server = native_server(vec![]);
        let provider = native_provider(&server);
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut spec = SessionSpec::new(dir.path());
        spec.model = Some("m".to_owned());
        spec.mcp_servers.insert(
            "remote".into(),
            McpServerSpec::Http {
                url: "http://127.0.0.1:9/mcp".into(),
                headers: [(
                    "Authorization".to_owned(),
                    format!("Bearer {SECRET_SENTINEL}"),
                )]
                .into(),
            },
        );
        let mut errors = Vec::new();
        if let Err(error) = provider.open(spec).await {
            errors.push(error);
        }
        errors
    }

    async fn out_of_profile_probe(&self) -> Option<ToolProfileProbe> {
        // The model asks for `write`; the session's profile allows only read-only
        // tools and denies `write` by name. The fake server counts what ran.
        let server = native_server(vec![
            tool_reply(
                Some("security scenario"),
                &[("w1", "mcp__fake__write", json!({"text": "data"}))],
                None,
                None,
            ),
            text_reply(Some(TOOL_MESSAGE), "understood", None, None),
        ]);
        let provider = native_provider(&server);
        let dir = tempfile::tempdir().ok()?;
        let log = dir.path().join("mcp.jsonl");
        let mut spec = SessionSpec::new(dir.path());
        spec.model = Some("m".to_owned());
        spec.mcp_servers
            .insert("fake".into(), stdio_mcp(Some(&log)));
        spec.policy = ToolPolicy::from_patterns(
            PolicyMode::PlanOnly,
            &["mcp__fake__readonly"],
            &["mcp__fake__write"],
        )
        .ok()?;
        let session = provider.open(spec).await.ok()?;
        let mut stream = session
            .send_turn(nexus_claude::agent::TurnInput::text("security scenario"))
            .await
            .ok()?;
        let mut events = Vec::new();
        use futures::StreamExt;
        while let Some(event) = stream.next().await {
            let terminal = matches!(event, AgentEvent::Done { .. } | AgentEvent::Error { .. });
            events.push(event);
            if terminal {
                break;
            }
        }
        let executions = log_events(&log)
            .iter()
            .filter(|e| e["event"] == "call" && e["tool"] == "write")
            .count();
        Some(ToolProfileProbe { events, executions })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_passes_the_security_scenarios() {
    let report = run_all(&NativeTarget).await;
    println!("{}", report.summary());
    report.assert_secure();
    // The native harness is the one provider the out-of-profile rule applies to.
    assert!(
        matches!(
            report
                .results
                .iter()
                .find(|(s, _)| *s == SecurityScenario::OutOfProfileToolRefused)
                .map(|(_, r)| r),
            Some(Ok(SecurityVerdict::Passed))
        ),
        "{}",
        report.summary()
    );
}

// ---------------------------------------------------------------------------
// The report itself
// ---------------------------------------------------------------------------

/// "Not applicable" may only ever be said for the out-of-profile rule.
#[tokio::test]
async fn no_scenario_but_the_tool_profile_may_be_skipped() {
    let report = run_all(&ClaudeTarget).await;
    for (scenario, result) in &report.results {
        if matches!(result, Ok(SecurityVerdict::NotApplicable(_))) {
            assert_eq!(*scenario, SecurityScenario::OutOfProfileToolRefused);
        }
    }
}
