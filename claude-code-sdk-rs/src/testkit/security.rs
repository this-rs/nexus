//! Mandatory security scenarios of the agent contract (decisions A32, A33, A35).
//!
//! The conformance suite of [`super::conformance`] skips nothing for an absent
//! *capability*: it verifies the written fallback instead. These scenarios go one
//! step further. They are **not conditioned by any capability**: every provider
//! that ships runs every one of them against its own fake executable, and a
//! provider that cannot be staged is a failure of the target, not a pass.
//!
//! | Scenario | Rule |
//! |---|---|
//! | [`SecurityScenario::TrustWithoutSandbox`] | `trust` asked of a third-party provider with no sandbox is refused when the session opens, with a typed error (A35); never downgraded silently |
//! | [`SecurityScenario::UnknownPolicyRefused`] | an unknown mode, a malformed pattern and a policy above its ceiling are refused, never read as "allow" (A35) |
//! | [`SecurityScenario::EnvironmentIsolation`] | the child starts from an empty environment: no host variable reaches it, and a third-party provider gets a `HOME` of its own (A33) |
//! | [`SecurityScenario::NoSecretInArgv`] | the secret appears nowhere on the command line (A33) |
//! | [`SecurityScenario::NoSecretInErrors`] | the secret appears in no `Debug`, no error, no event (A10, A33) |
//! | [`SecurityScenario::OutOfProfileToolRefused`] | the native harness never runs a tool outside its profile (A35) |
//!
//! Each scenario is **first proven red** against a deliberately faulty provider
//! (`tests/agent_security.rs`): one that inherits the host environment, writes the
//! secret on argv, leaks it in an error, and opens `trust` without a sandbox. A
//! check that has never failed proves nothing.
//!
//! The only "not applicable" verdict is [`SecurityScenario::OutOfProfileToolRefused`]
//! for a provider that runs no tool of its own (the CLI decides). Every other
//! scenario returns [`SecurityVerdict::Passed`] or a [`Violation`].

use std::any::Any;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;

use crate::agent::{
    AgentEvent, AgentProvider, AgentSession, PermissionDecision, PolicyMode, ProviderError,
    ProviderKind, SandboxLevel, SessionSpec, ToolPolicy, TurnInput,
};

/// The secret every target is asked to wire in. It is key-shaped on purpose so a
/// redaction by shape cannot hide a leak from the checks.
pub const SECRET_SENTINEL: &str = "sk-sentinel-n15-Zq81mLpWx39vNbR2kT";

/// A variable cargo puts in every test process: it is in the host environment, so
/// it must **not** reach a child. No `set_var`, hence no race between tests.
pub const HOST_VARIABLE: &str = "CARGO_MANIFEST_DIR";

/// Longest wait for a turn.
const TURN_TIMEOUT: Duration = Duration::from_secs(20);

/// One scenario of the security suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SecurityScenario {
    /// `trust` + third-party provider + no sandbox → refused when opening.
    TrustWithoutSandbox,
    /// Unknown mode, malformed pattern, policy above its ceiling → refused.
    UnknownPolicyRefused,
    /// Empty environment plus an allowlist; a dedicated `HOME` for a third party.
    EnvironmentIsolation,
    /// The secret is nowhere on the command line.
    NoSecretInArgv,
    /// The secret is in no `Debug`, no error and no event.
    NoSecretInErrors,
    /// The native harness never runs a tool outside the session's profile.
    OutOfProfileToolRefused,
}

impl SecurityScenario {
    /// Every scenario, in the order the suite runs them.
    pub const ALL: [SecurityScenario; 6] = [
        SecurityScenario::TrustWithoutSandbox,
        SecurityScenario::UnknownPolicyRefused,
        SecurityScenario::EnvironmentIsolation,
        SecurityScenario::NoSecretInArgv,
        SecurityScenario::NoSecretInErrors,
        SecurityScenario::OutOfProfileToolRefused,
    ];

    /// Stable name, used in reports and in the documentation.
    pub fn name(self) -> &'static str {
        match self {
            Self::TrustWithoutSandbox => "trust_without_sandbox",
            Self::UnknownPolicyRefused => "unknown_policy_refused",
            Self::EnvironmentIsolation => "isolation_env",
            Self::NoSecretInArgv => "argv_sans_secret",
            Self::NoSecretInErrors => "erreur_sans_identifiant",
            Self::OutOfProfileToolRefused => "outil_hors_profil_refuse",
        }
    }
}

impl fmt::Display for SecurityScenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What a scenario found wrong. Always a failure: there is no "warning".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation(pub String);

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a scenario ended without a violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityVerdict {
    /// The rule held.
    Passed,
    /// The rule does not apply to this provider, and why. Only
    /// [`SecurityScenario::OutOfProfileToolRefused`] may say so.
    NotApplicable(&'static str),
}

/// What the child process saw when it started.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchObservation {
    /// Its command line, without the program name.
    pub argv: Vec<String>,
    /// The **names** of its environment variables.
    pub env_names: Vec<String>,
    /// The value of its `HOME`, when it could be observed.
    pub home: Option<String>,
}

/// Looks at what the child saw, once the session has run a turn. The events of
/// that turn are given for providers whose child is only observable through a tool
/// (the native harness asks its MCP server for its own environment).
pub type Observer = Box<dyn Fn(&[AgentEvent]) -> Option<LaunchObservation> + Send + Sync>;

/// A session staged for the security scenarios.
pub struct SecurityStaging {
    /// The provider to open the session on, built for this staging alone: a fake
    /// server that consumes its routes in order cannot be shared between scenarios.
    pub provider: Arc<dyn AgentProvider>,
    /// Spec to open. It carries [`SECRET_SENTINEL`] in every place this provider
    /// accepts a secret (session environment, MCP server environment, headers).
    pub spec: SessionSpec,
    /// Reads the launch record of the child.
    pub observe: Observer,
    /// `Debug` renderings of every object that was given the secret (instance
    /// configuration, provider, spec): none may contain it.
    pub secret_subjects: Vec<String>,
    /// Whatever must outlive the scenario (a fake server, a temporary directory).
    pub guard: Option<Box<dyn Any + Send>>,
}

/// What the out-of-profile probe saw (native harness).
#[derive(Debug)]
pub struct ToolProfileProbe {
    /// Events of the turn in which the model asked for a tool outside the profile.
    pub events: Vec<AgentEvent>,
    /// How many times that tool actually ran (counter of the fake MCP server).
    pub executions: usize,
}

/// A provider under security test.
#[async_trait]
pub trait SecurityTarget: Send + Sync {
    /// Name shown in the report.
    fn name(&self) -> &str;

    /// A provider and a spec able to run one plain turn, with the secret wired in.
    /// Called once per scenario. `None` is a failure of the target: every provider
    /// launches something observable.
    async fn stage(&self) -> Option<SecurityStaging>;

    /// Errors the provider raises while the secret is in play (a refused open, a
    /// failed launch, an unreachable endpoint). The suite checks none contains it.
    async fn provoke_errors(&self) -> Vec<ProviderError>;

    /// Staging of the out-of-profile tool probe; `None` for a provider that runs no
    /// tool of its own.
    async fn out_of_profile_probe(&self) -> Option<ToolProfileProbe> {
        None
    }
}

/// Whether Trust is subject to the sandbox rule: every kind but the first-party CLI.
fn third_party(kind: ProviderKind) -> bool {
    !matches!(kind, ProviderKind::ClaudeCode)
}

/// Runs one turn to its terminal event and returns every event. Permission asks
/// are allowed once so a scripted turn can finish. Bounded: a hang is a failure.
async fn drive_turn(session: &dyn AgentSession) -> Result<Vec<AgentEvent>, Violation> {
    let mut stream = session
        .send_turn(TurnInput::text("security scenario"))
        .await
        .map_err(|error| Violation(format!("the turn did not start: {error}")))?;
    let mut events = Vec::new();
    let run = async {
        while let Some(event) = stream.next().await {
            if let AgentEvent::PermissionAsk { request_id, .. } = &event {
                let _ = session
                    .answer_permission(request_id, PermissionDecision::allow_once())
                    .await;
            }
            let terminal = matches!(event, AgentEvent::Done { .. } | AgentEvent::Error { .. });
            events.push(event);
            if terminal {
                break;
            }
        }
    };
    if tokio::time::timeout(TURN_TIMEOUT, run).await.is_err() {
        return Err(Violation("the turn never reached a terminal event".into()));
    }
    Ok(events)
}

async fn staged(target: &dyn SecurityTarget) -> Result<SecurityStaging, Violation> {
    target.stage().await.ok_or_else(|| {
        Violation(format!(
            "{} could not be staged: a provider that cannot be observed is a failure, not a pass",
            target.name()
        ))
    })
}

/// Opens the staged spec, runs a turn and returns what the child saw.
async fn observe_launch(
    target: &dyn SecurityTarget,
) -> Result<(LaunchObservation, Vec<AgentEvent>, SecurityStaging), Violation> {
    let staging = staged(target).await?;
    let session = staging
        .provider
        .open(staging.spec.clone())
        .await
        .map_err(|error| Violation(format!("the staged session did not open: {error}")))?;
    let events = drive_turn(&*session).await?;
    let _ = session.close().await;
    let observation = (staging.observe)(&events)
        .ok_or_else(|| Violation("the child left no launch record to inspect".into()))?;
    Ok((observation, events, staging))
}

async fn trust_without_sandbox(target: &dyn SecurityTarget) -> Result<SecurityVerdict, Violation> {
    let mut staging = staged(target).await?;
    let provider = staging.provider.clone();
    let capabilities = provider.capabilities(None);
    staging.spec.policy = ToolPolicy::new(PolicyMode::Trust);
    let outcome = provider.open(staging.spec.clone()).await;
    let must_refuse = third_party(provider.kind()) && capabilities.sandbox == SandboxLevel::None;
    match outcome {
        Ok(session) => {
            let _ = session.close().await;
            if must_refuse {
                return Err(Violation(format!(
                    "{:?} opened a session in `trust` mode with no sandbox: the refusal is \
                     missing",
                    provider.kind()
                )));
            }
            Ok(SecurityVerdict::Passed)
        },
        Err(error) => {
            let sandbox_refusal = matches!(
                &error,
                ProviderError::Unsupported { capability } if capability == "sandbox"
            );
            if must_refuse && !sandbox_refusal {
                return Err(Violation(format!(
                    "`trust` without a sandbox was refused with the wrong error: {error:?} \
                     (expected unsupported sandbox)"
                )));
            }
            Ok(SecurityVerdict::Passed)
        },
    }
}

async fn unknown_policy_refused(target: &dyn SecurityTarget) -> Result<SecurityVerdict, Violation> {
    for text in ["\"bogus\"", "\"yolo\"", "\"TRUST\"", "\"\"", "null", "3"] {
        if serde_json::from_str::<PolicyMode>(text).is_ok() {
            return Err(Violation(format!(
                "{text} was read as a policy mode: an unknown mode must be an error"
            )));
        }
    }
    for pattern in ["Bash(", "Bash()", "(git *)", "Bash(git *", "Ba sh", ""] {
        if ToolPolicy::from_patterns(PolicyMode::Ask, &[pattern], &[] as &[&str]).is_ok()
            || ToolPolicy::from_patterns(PolicyMode::Trust, &[] as &[&str], &[pattern]).is_ok()
        {
            return Err(Violation(format!(
                "the pattern {pattern:?} was accepted: a dropped `deny` widens the policy"
            )));
        }
    }

    // A policy above its ceiling is refused by the provider itself, before anything
    // starts: an unreadable ceiling must never fall back to "no ceiling".
    let mut staging = staged(target).await?;
    staging.spec.policy = ToolPolicy::new(PolicyMode::AutoEdits);
    staging.spec.policy_ceiling = Some(ToolPolicy::new(PolicyMode::PlanOnly));
    match staging.provider.open(staging.spec.clone()).await {
        Ok(session) => {
            let _ = session.close().await;
            Err(Violation(
                "a policy above its ceiling was accepted by `open`".into(),
            ))
        },
        Err(ProviderError::Unsupported { capability }) if capability == "policy_ceiling" => {
            Ok(SecurityVerdict::Passed)
        },
        Err(other) => Err(Violation(format!(
            "a policy above its ceiling was refused with the wrong error: {other:?}"
        ))),
    }
}

async fn environment_isolation(target: &dyn SecurityTarget) -> Result<SecurityVerdict, Violation> {
    if std::env::var_os(HOST_VARIABLE).is_none() {
        return Err(Violation(format!(
            "{HOST_VARIABLE} is not in the test process: this scenario would prove nothing"
        )));
    }
    let (observation, _events, staging) = observe_launch(target).await?;
    if observation
        .env_names
        .iter()
        .any(|name| name == HOST_VARIABLE)
    {
        return Err(Violation(format!(
            "the host variable {HOST_VARIABLE} reached the child: it inherits the environment \
             ({} variables seen)",
            observation.env_names.len()
        )));
    }
    if third_party(staging.provider.kind()) {
        let host_home = std::env::var("HOME").unwrap_or_default();
        match observation.home {
            Some(home) if !host_home.is_empty() && home == host_home => {
                return Err(Violation(
                    "a third-party provider runs with the host user's HOME: it can read the \
                     credentials of every other tool (A33)"
                        .into(),
                ));
            },
            Some(_) => {},
            None => {
                return Err(Violation(
                    "the HOME of a third-party child could not be observed: the target must \
                     report it"
                        .into(),
                ));
            },
        }
    }
    Ok(SecurityVerdict::Passed)
}

async fn no_secret_in_argv(target: &dyn SecurityTarget) -> Result<SecurityVerdict, Violation> {
    let (observation, _events, _staging) = observe_launch(target).await?;
    if let Some(arg) = observation
        .argv
        .iter()
        .find(|arg| arg.contains(SECRET_SENTINEL))
    {
        return Err(Violation(format!(
            "the secret is on the command line of the child: {}",
            arg.replace(SECRET_SENTINEL, "[SENTINEL]")
        )));
    }
    Ok(SecurityVerdict::Passed)
}

async fn no_secret_in_errors(target: &dyn SecurityTarget) -> Result<SecurityVerdict, Violation> {
    let staging = staged(target).await?;
    let mut surfaces: Vec<(String, String)> = Vec::new();
    surfaces.push(("Debug of the spec".into(), format!("{:?}", staging.spec)));
    for (index, subject) in staging.secret_subjects.iter().enumerate() {
        surfaces.push((format!("Debug of subject #{index}"), subject.clone()));
    }
    for error in target.provoke_errors().await {
        surfaces.push(("Display of an error".into(), error.to_string()));
        surfaces.push(("Debug of an error".into(), format!("{error:?}")));
        if let Ok(json) = serde_json::to_string(&error) {
            surfaces.push(("JSON of an error".into(), json));
        }
    }
    // The events of a real turn are a surface too.
    let (_observation, events, _staging) = observe_launch(target).await?;
    surfaces.push(("Debug of the turn's events".into(), format!("{events:?}")));
    if let Some((surface, _)) = surfaces
        .iter()
        .find(|(_, text)| text.contains(SECRET_SENTINEL))
    {
        return Err(Violation(format!("the secret appears in: {surface}")));
    }
    Ok(SecurityVerdict::Passed)
}

async fn out_of_profile_tool_refused(
    target: &dyn SecurityTarget,
) -> Result<SecurityVerdict, Violation> {
    let Some(probe) = target.out_of_profile_probe().await else {
        return Ok(SecurityVerdict::NotApplicable(
            "the provider runs no tool of its own: the CLI it drives decides",
        ));
    };
    if probe.executions != 0 {
        return Err(Violation(format!(
            "a tool outside the session's profile ran {} time(s)",
            probe.executions
        )));
    }
    let answered = probe.events.iter().any(|event| {
        matches!(
            event,
            AgentEvent::ToolResult { is_error: true, .. } | AgentEvent::Done { .. }
        )
    });
    if !answered {
        return Err(Violation(
            "the turn that asked for the forbidden tool left no result and no end".into(),
        ));
    }
    Ok(SecurityVerdict::Passed)
}

/// Runs one scenario against a target.
pub async fn run_scenario(
    target: &dyn SecurityTarget,
    scenario: SecurityScenario,
) -> Result<SecurityVerdict, Violation> {
    match scenario {
        SecurityScenario::TrustWithoutSandbox => trust_without_sandbox(target).await,
        SecurityScenario::UnknownPolicyRefused => unknown_policy_refused(target).await,
        SecurityScenario::EnvironmentIsolation => environment_isolation(target).await,
        SecurityScenario::NoSecretInArgv => no_secret_in_argv(target).await,
        SecurityScenario::NoSecretInErrors => no_secret_in_errors(target).await,
        SecurityScenario::OutOfProfileToolRefused => out_of_profile_tool_refused(target).await,
    }
}

/// The outcome of every scenario against one target.
#[derive(Debug)]
pub struct SecurityReport {
    /// Name of the target.
    pub target: String,
    /// One result per scenario, in [`SecurityScenario::ALL`] order.
    pub results: Vec<(SecurityScenario, Result<SecurityVerdict, Violation>)>,
}

impl SecurityReport {
    /// One line per scenario.
    pub fn summary(&self) -> String {
        let mut out = format!("security of {}:\n", self.target);
        for (scenario, result) in &self.results {
            let line = match result {
                Ok(SecurityVerdict::Passed) => "passed".to_owned(),
                Ok(SecurityVerdict::NotApplicable(why)) => format!("not applicable: {why}"),
                Err(violation) => format!("VIOLATION: {violation}"),
            };
            out.push_str(&format!("  {scenario}: {line}\n"));
        }
        out
    }

    /// The scenarios that found a violation.
    pub fn violations(&self) -> Vec<(SecurityScenario, &Violation)> {
        self.results
            .iter()
            .filter_map(|(scenario, result)| result.as_ref().err().map(|v| (*scenario, v)))
            .collect()
    }

    /// Panics unless every scenario passed. Only
    /// [`SecurityScenario::OutOfProfileToolRefused`] may be "not applicable".
    pub fn assert_secure(&self) {
        let violations = self.violations();
        assert!(violations.is_empty(), "{}", self.summary());
        for (scenario, result) in &self.results {
            if matches!(result, Ok(SecurityVerdict::NotApplicable(_)))
                && *scenario != SecurityScenario::OutOfProfileToolRefused
            {
                panic!(
                    "{scenario} may not be skipped as not applicable:\n{}",
                    self.summary()
                );
            }
        }
    }
}

/// Runs every scenario against a target.
pub async fn run_all(target: &dyn SecurityTarget) -> SecurityReport {
    let mut results = Vec::new();
    for scenario in SecurityScenario::ALL {
        results.push((scenario, run_scenario(target, scenario).await));
    }
    SecurityReport {
        target: target.name().to_owned(),
        results,
    }
}
