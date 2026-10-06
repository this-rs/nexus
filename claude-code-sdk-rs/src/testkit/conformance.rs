//! Conformance suite of the agent contract, parameterised by provider.
//!
//! The same [`Scenario`]s run against every provider. A provider is plugged in
//! through a [`ConformanceTarget`]: for each scenario the target hands back a
//! provider and a spec **staged** so that the scenario plays out as its
//! documentation says (a scripted provider gets a script, an adapter gets a fake
//! executable or a fake server). The suite then **drives** the session itself —
//! it sends the turn, answers the permission, interrupts, cancels — and checks:
//!
//! - the generic invariants of `docs/agent-contract.md` §4 on every stream
//!   ([`check_stream_invariants`]);
//! - what the scenario expects;
//! - the capability fallbacks of §5.
//!
//! **An absent capability never skips a scenario.** When the capability a
//! scenario depends on is absent, the suite verifies the written fallback
//! instead (`Unsupported { capability }`, or the absence of the corresponding
//! events) and reports [`ScenarioOutcome::FallbackVerified`]. Conversely a
//! capability that is **declared** must be proven: [`ConformanceReport::assert_conformant`]
//! refuses a scenario left unstaged unless its capability is absent.
//!
//! Every wait is bounded: a provider that hangs fails the scenario, it does not
//! hang the test.

use std::any::Any;
use std::collections::HashSet;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::scripted::{Script, ScriptedProvider, Step, steps};
use crate::agent::{
    AgentEvent, AgentProvider, AgentSession, BackgroundTask, BackgroundTaskKind,
    BackgroundTaskStatus, CancelScope, Capabilities, CompactionPhase, CompactionTrigger, CostBasis,
    DeltaKind, EventStream, HookSupport, HookVerdict, InputBlock, InterruptScope, McpServerSpec,
    ModelInfo, PermissionDecision, PermissionScope, PolicyMode, ProviderError, ProviderKind,
    QuestionAnswer, QuestionAnswerItem, QuestionReply, ResumeToken, SandboxLevel, SessionHooks,
    SessionSpec, StopReason, SubagentSupport, ToolCallInfo, ToolCategory, ToolPolicy,
    ToolResultInfo, TurnInput,
};

/// Longest wait for one step of a scenario: an answer to a call, the next event
/// of a turn.
const STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest wait for a stream to close after its terminal event.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// Window during which an event that must **not** come is watched for.
const QUIET_WINDOW: Duration = Duration::from_millis(150);
/// Longest a whole scenario may take.
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(60);

/// A provider and a spec staged for one scenario.
pub struct Prepared {
    /// Provider to open the session on.
    pub provider: Arc<dyn AgentProvider>,
    /// Spec to open (or resume) with. The suite may set `hooks` on it and derives
    /// variants from it to check refusals at opening.
    pub spec: SessionSpec,
    /// Token to resume with, for [`Scenario::Reprise`].
    pub resume: Option<ResumeToken>,
    /// Whatever must stay alive while the scenario runs (a fake server, a
    /// temporary directory). Dropped when the scenario ends.
    pub guard: Option<Box<dyn Any + Send>>,
}

impl Prepared {
    /// A staging with no resume token and nothing to keep alive.
    pub fn new(provider: Arc<dyn AgentProvider>, spec: SessionSpec) -> Self {
        Self {
            provider,
            spec,
            resume: None,
            guard: None,
        }
    }
}

/// A provider under conformance test.
#[async_trait]
pub trait ConformanceTarget: Send + Sync {
    /// Name shown in the report.
    fn name(&self) -> &str;

    /// Provider whose capabilities are checked (default model).
    fn provider(&self) -> Arc<dyn AgentProvider>;

    /// A provider and a spec wired so that `scenario` plays out as described in
    /// its documentation; `None` when the target cannot stage it.
    ///
    /// When the capability of a scenario is absent the suite still asks for it:
    /// a target may then return any session able to run a plain turn, or `None`,
    /// in which case the suite falls back to the staging of
    /// [`Scenario::TourTexteSimple`].
    async fn prepare(&self, scenario: Scenario) -> Option<Prepared>;
}

/// One scenario of the suite.
///
/// The documentation of each variant is what the **target** must stage; what the
/// suite does and checks follows. The suite always sends
/// `TurnInput::text(scenario.prompt())` as the first turn, so a fake provider can
/// dispatch on the prompt. Unless said otherwise, a `permission_ask` met on the
/// way is allowed once by the suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Scenario {
    /// Stage: one turn answering with assistant text.
    ///
    /// Checked: a non-empty top-level `text`, then `done completed`; the session's
    /// capabilities are the provider's for that model and stay frozen (this
    /// declaration is what the registry reads for `secret_isolation`).
    TourTexteSimple,
    /// Stage: one turn streaming text `delta`s, then the complete `text`.
    ///
    /// Checked: at least one text delta, and a complete `text` after the last one.
    FluxDeltas,
    /// Stage: one turn with reasoning, then text.
    ///
    /// Checked: a non-empty `thinking`. Fallback (`thinking: false`): a plain turn
    /// carries no `thinking`.
    Raisonnement,
    /// Stage: one turn where the model calls one tool that runs and succeeds,
    /// then answers.
    ///
    /// Checked: a complete `tool_call`, its successful `tool_result`, `done
    /// completed`; with `hooks: in_protocol`, `before_tool` and `after_tool` of
    /// the spec's hooks were called. Fallback (`tools: false`): `open` with an MCP
    /// server answers `model_no_tools`, and no `tool_call` is emitted; with
    /// `per_session_mcp: false`, it answers `Unsupported { per_session_mcp }`.
    AppelOutilResultat,
    /// Stage: one turn where two tool calls are emitted before either result,
    /// then both results.
    ///
    /// Checked: two complete calls pending at the same time, a result for each.
    /// Fallback: as [`Scenario::AppelOutilResultat`].
    OutilsParalleles,
    /// Stage: one turn where a tool needs approval (`permission_ask`), runs once
    /// allowed, and the turn completes.
    ///
    /// Checked: the request's scopes are declared; an unknown identifier answers
    /// `invalid_request`; an undeclared scope answers `Unsupported {
    /// permission_scope }`; after the approval the tool result is a success and
    /// the turn is `done completed`; a second answer is `invalid_request`.
    /// Fallback (`interactive_permissions: false`): no `permission_ask`,
    /// `answer_permission` answers `Unsupported`.
    PermissionAccordee,
    /// Stage: as [`Scenario::PermissionAccordee`]; once denied, the tool does not
    /// run and the turn still ends with `done`.
    ///
    /// Checked: no successful result for the denied call, a `done` terminal.
    /// Fallback: as [`Scenario::PermissionAccordee`].
    PermissionRefusee,
    /// Stage: one turn where the provider asks the user a `question`. Reply mode
    /// `call`: the turn goes on once answered. Reply mode `turn`: the turn ends by
    /// itself and a second turn (the answer) completes.
    ///
    /// Checked: mode `call`: `answer_question` succeeds once, then
    /// `invalid_request`; mode `turn`: `answer_question` answers `Unsupported {
    /// answer_question }`. Fallback (`native_question: false`): no `question`,
    /// `answer_question` answers `Unsupported`.
    QuestionUtilisateur,
    /// Stage: one turn that emits at least one event, then stays busy until it is
    /// interrupted.
    ///
    /// Checked: `interrupt(turn_only)` reports `turn_interrupted`, the turn ends
    /// with `done interrupted`, and an interruption outside a turn is `Ok` with
    /// `turn_interrupted: false`.
    InterruptionEnFlux,
    /// Stage: one turn where a tool is running (`tool_call` emitted, no result)
    /// until the turn is interrupted.
    ///
    /// Checked: `interrupt(turn_and_tools)` ends the turn with `done interrupted`;
    /// a result for the cut call, if any, is an error. Fallback (`tools: false`):
    /// as [`Scenario::AppelOutilResultat`].
    InterruptionOutil,
    /// Stage: one turn where a tool runs until it is cancelled, after which the
    /// turn goes on to its normal end.
    ///
    /// Checked: `cancel_tools(all)` stops at least one tool, its `tool_result` is
    /// an error, and the turn ends with `done completed` — not interrupted.
    /// Fallback (`tool_cancel: false`): `cancel_tools` answers `Unsupported {
    /// tool_cancel }`.
    AnnulationTourPreserve,
    /// Stage: one turn that leaves a background task running (a
    /// `background_tasks` snapshot with a `running` task, in the turn or out of
    /// band) and completes.
    ///
    /// Checked: `cancel_tools(task)` succeeds and a later snapshot shows the task
    /// gone or `killed`. Fallback (`background_tasks: false`): no
    /// `background_tasks` nor `task_update`, `cancel_tools(task)` answers
    /// `Unsupported`.
    AnnulationTache,
    /// Stage: a session that runs one plain turn after its model was changed; the
    /// catalogue names a second model.
    ///
    /// Checked: `set_model` succeeds, the capabilities stay frozen, the turn
    /// completes. Fallback (`set_model_live: false`): `set_model` answers
    /// `Unsupported { set_model_live }` and the session still works.
    ChangementModele,
    /// Stage: a session that runs one plain turn after `set_policy_mode`.
    ///
    /// Checked: `set_policy_mode(auto_edits)` is `Ok` or `Unsupported`, never
    /// another error; the turn completes; `open` refuses a policy above its
    /// ceiling (`Unsupported { policy_ceiling }`); `open` in `trust` mode is
    /// never refused for lack of a sandbox (`Unsupported { sandbox }` is a
    /// failure: the sandbox level is information, not a gate).
    ChangementPolitique,
    /// Stage: [`Prepared::resume`] holds a token the provider accepts, and the
    /// resumed session runs one plain turn.
    ///
    /// Checked: a token of another provider family is `invalid_request`; the
    /// resumed turn completes; `resume_token()` is then a token of the provider's
    /// family. Fallback (`resume: false`): `resume()` answers `Unsupported {
    /// resume }` and `resume_token()` stays `None`.
    Reprise,
    /// Stage: one turn whose input carries an image, completing normally.
    ///
    /// Checked: the turn completes. Fallback (`images: false`): `send_turn`
    /// answers `Unsupported { images }`, no turn is left running, and a text turn
    /// then works.
    MessageImages,
    /// Stage: one turn where a sub-agent runs: events carrying `parent`.
    ///
    /// Checked: at least one event with `parent`; with `subagents: nested`, every
    /// `parent` is a `tool_call` emitted before. Fallback (`subagents: none`): no
    /// event carries `parent`.
    SousAgent,
    /// Stage: one turn during which the context is compacted (`compaction
    /// completed`, in the turn or out of band).
    ///
    /// Checked: a `compaction completed`; a `started`, if any, comes first.
    /// Fallback (`compaction_signal: false`): no `compaction`.
    Compaction,
    /// Stage: the session emits at least one event with no turn running, then
    /// runs one plain turn.
    ///
    /// Checked: `out_of_band()` is `Some` once, then `None`; the event arrives on
    /// it; no `done` travels out of band. The suite opens with hooks: unless
    /// `hooks: in_protocol`, the first out-of-band event is `provider_notice {
    /// hooks_not_supported }`.
    HorsTour,
    /// Stage: a `permission_ask` arrives out of band, with no turn running.
    ///
    /// Checked: it can be answered outside a turn; a second answer is
    /// `invalid_request`. Fallback (`interactive_permissions: false`): no
    /// out-of-band `permission_ask`, `answer_permission` answers `Unsupported`.
    PermissionHorsTour,
    /// Stage: a first turn that emits one event then stays busy until
    /// interrupted, and a second, plain turn.
    ///
    /// Checked: `send_turn` during the first turn answers `turn_in_progress` and
    /// does not disturb it; once its terminal event is emitted, the next
    /// `send_turn` is accepted.
    TourConcurrent,
    /// Stage: a first turn failing with a retryable error (rate limit, overload,
    /// unreachable endpoint, timeout), and a second, plain turn.
    ///
    /// Checked: the turn ends with a typed, retryable failure — a terminal `error`,
    /// or a `done` in error carrying its classified `error` — and the session
    /// survives it.
    ErreurRetryable,
    /// Stage: one plain turn whose `done` carries token usage and cost.
    ///
    /// Checked: input and output tokens are known; the cost matches the declared
    /// basis; a declared context window is not zero. Fallback (`cost: unknown`):
    /// `done.cost.usd` is `None` with basis `unknown`.
    FinUsageCout,
    /// Stage: the provider process dies during the turn. For a provider without
    /// a process: a terminal, non-retryable error after which the session is
    /// unusable.
    ///
    /// Checked: a terminal, non-retryable `error`; `send_turn` then fails.
    ProcessusMort,
    /// Stage: one turn that emits one event then stays busy.
    ///
    /// Checked: `close()` during the turn gives the stream `error { closed }`;
    /// `close()` again is `Ok`; every other method then answers `closed`.
    FermetureIdempotente,
}

impl Scenario {
    /// Every scenario, in the order the suite runs them.
    pub const ALL: [Scenario; 25] = [
        Self::TourTexteSimple,
        Self::FluxDeltas,
        Self::Raisonnement,
        Self::AppelOutilResultat,
        Self::OutilsParalleles,
        Self::PermissionAccordee,
        Self::PermissionRefusee,
        Self::QuestionUtilisateur,
        Self::InterruptionEnFlux,
        Self::InterruptionOutil,
        Self::AnnulationTourPreserve,
        Self::AnnulationTache,
        Self::ChangementModele,
        Self::ChangementPolitique,
        Self::Reprise,
        Self::MessageImages,
        Self::SousAgent,
        Self::Compaction,
        Self::HorsTour,
        Self::PermissionHorsTour,
        Self::TourConcurrent,
        Self::ErreurRetryable,
        Self::FinUsageCout,
        Self::ProcessusMort,
        Self::FermetureIdempotente,
    ];

    /// Stable name of the scenario.
    pub fn name(self) -> &'static str {
        match self {
            Self::TourTexteSimple => "tour_texte_simple",
            Self::FluxDeltas => "flux_deltas",
            Self::Raisonnement => "raisonnement",
            Self::AppelOutilResultat => "appel_outil_resultat",
            Self::OutilsParalleles => "outils_paralleles",
            Self::PermissionAccordee => "permission_accordee",
            Self::PermissionRefusee => "permission_refusee",
            Self::QuestionUtilisateur => "question_utilisateur",
            Self::InterruptionEnFlux => "interruption_en_flux",
            Self::InterruptionOutil => "interruption_outil",
            Self::AnnulationTourPreserve => "annulation_tour_preserve",
            Self::AnnulationTache => "annulation_tache",
            Self::ChangementModele => "changement_modele",
            Self::ChangementPolitique => "changement_politique",
            Self::Reprise => "reprise",
            Self::MessageImages => "message_images",
            Self::SousAgent => "sous_agent",
            Self::Compaction => "compaction",
            Self::HorsTour => "hors_tour",
            Self::PermissionHorsTour => "permission_hors_tour",
            Self::TourConcurrent => "tour_concurrent",
            Self::ErreurRetryable => "erreur_retryable",
            Self::FinUsageCout => "fin_usage_cout",
            Self::ProcessusMort => "processus_mort",
            Self::FermetureIdempotente => "fermeture_idempotente",
        }
    }

    /// Text of the first turn the suite sends: `conformance:<name>`.
    pub fn prompt(self) -> String {
        format!("conformance:{}", self.name())
    }

    /// The [`Capabilities`] field the scenario depends on, `None` when every
    /// provider must pass it. When that capability is absent the suite verifies
    /// its fallback instead of the scenario.
    pub fn capability(self) -> Option<&'static str> {
        match self {
            Self::Raisonnement => Some("thinking"),
            Self::AppelOutilResultat | Self::OutilsParalleles | Self::InterruptionOutil => {
                Some("tools")
            },
            Self::PermissionAccordee | Self::PermissionRefusee | Self::PermissionHorsTour => {
                Some("interactive_permissions")
            },
            Self::QuestionUtilisateur => Some("native_question"),
            Self::AnnulationTourPreserve => Some("tool_cancel"),
            Self::AnnulationTache => Some("background_tasks"),
            Self::ChangementModele => Some("set_model_live"),
            Self::Reprise => Some("resume"),
            Self::MessageImages => Some("images"),
            Self::SousAgent => Some("subagents"),
            Self::Compaction => Some("compaction_signal"),
            Self::FinUsageCout => Some("cost"),
            Self::TourTexteSimple
            | Self::FluxDeltas
            | Self::InterruptionEnFlux
            | Self::ChangementPolitique
            | Self::HorsTour
            | Self::TourConcurrent
            | Self::ErreurRetryable
            | Self::ProcessusMort
            | Self::FermetureIdempotente => None,
        }
    }

    /// Every [`Capabilities`] field this scenario checks, present or through its
    /// fallback: [`Scenario::capability`] plus the fields checked on the side.
    pub fn covers(self) -> &'static [&'static str] {
        match self {
            // `secret_isolation` has no session-level behaviour: its fallback is a
            // refusal by the registry. What a session can prove is that the
            // declaration the registry reads is the one the session carries.
            Self::TourTexteSimple => &["secret_isolation"],
            Self::Raisonnement => &["thinking"],
            Self::AppelOutilResultat => &["tools", "per_session_mcp", "hooks"],
            Self::OutilsParalleles | Self::InterruptionOutil => &["tools", "per_session_mcp"],
            Self::PermissionAccordee => &["interactive_permissions", "permission_scopes"],
            Self::PermissionRefusee | Self::PermissionHorsTour => &["interactive_permissions"],
            Self::QuestionUtilisateur => &["native_question"],
            Self::AnnulationTourPreserve => &["tool_cancel"],
            Self::AnnulationTache => &["background_tasks"],
            Self::ChangementModele => &["set_model_live"],
            Self::ChangementPolitique => &["sandbox"],
            Self::Reprise => &["resume"],
            Self::MessageImages => &["images"],
            Self::SousAgent => &["subagents"],
            Self::Compaction => &["compaction_signal"],
            Self::HorsTour => &["hooks"],
            Self::FinUsageCout => &["cost", "context_window"],
            Self::FluxDeltas
            | Self::InterruptionEnFlux
            | Self::TourConcurrent
            | Self::ErreurRetryable
            | Self::ProcessusMort
            | Self::FermetureIdempotente => &[],
        }
    }
}

impl std::fmt::Display for Scenario {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Whether the capability named by a [`Capabilities`] field is present: `true`
/// for a boolean, a non-empty list, a level other than `none`, a known context
/// window, a cost basis other than `unknown`, hooks `in_protocol`.
///
/// # Panics
///
/// Panics on a name that is not in [`Capabilities::FIELDS`]: a capability added
/// to the contract must be given a meaning here.
pub fn capability_present(capabilities: &Capabilities, field: &str) -> bool {
    match field {
        "interactive_permissions" => capabilities.interactive_permissions,
        "permission_scopes" => !capabilities.permission_scopes.is_empty(),
        "sandbox" => capabilities.sandbox != SandboxLevel::None,
        "secret_isolation" => capabilities.secret_isolation,
        "per_session_mcp" => capabilities.per_session_mcp,
        "hooks" => capabilities.hooks == HookSupport::InProtocol,
        "subagents" => capabilities.subagents != SubagentSupport::None,
        "compaction_signal" => capabilities.compaction_signal,
        "thinking" => capabilities.thinking,
        "images" => capabilities.images,
        "tools" => capabilities.tools,
        "context_window" => capabilities.context_window.is_some(),
        "set_model_live" => capabilities.set_model_live,
        "native_question" => capabilities.native_question,
        "tool_cancel" => capabilities.tool_cancel,
        "background_tasks" => capabilities.background_tasks,
        "resume" => capabilities.resume,
        "cost" => capabilities.cost != CostBasis::Unknown,
        other => panic!("capability `{other}` has no meaning in the conformance suite"),
    }
}

/// How a scenario ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScenarioOutcome {
    /// The scenario played out and every check held.
    Passed,
    /// The scenario's capability is absent and its written fallback was verified.
    FallbackVerified,
    /// The target did not stage the scenario; the reason.
    NotStaged(String),
    /// At least one check failed; what failed.
    Failed(Vec<String>),
}

/// Result of the whole suite against one target.
#[derive(Debug, Clone)]
pub struct ConformanceReport {
    /// Name of the target.
    pub target: String,
    /// Capabilities the target's provider declares for its default model.
    pub capabilities: Capabilities,
    /// Outcome of every scenario, in the order of [`Scenario::ALL`].
    pub results: Vec<(Scenario, ScenarioOutcome)>,
}

impl ConformanceReport {
    /// Outcome of one scenario.
    pub fn outcome(&self, scenario: Scenario) -> Option<&ScenarioOutcome> {
        self.results
            .iter()
            .find(|(candidate, _)| *candidate == scenario)
            .map(|(_, outcome)| outcome)
    }

    /// What keeps the target from being conformant, one line per problem: every
    /// failed check, and every unstaged scenario whose capability is declared
    /// present or that depends on no capability. An unstaged scenario whose
    /// capability is absent is tolerated.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for (scenario, outcome) in &self.results {
            match outcome {
                ScenarioOutcome::Passed | ScenarioOutcome::FallbackVerified => {},
                ScenarioOutcome::Failed(failures) => {
                    problems.extend(failures.iter().map(|failure| format!("{scenario}: {failure}")));
                },
                ScenarioOutcome::NotStaged(reason) => match scenario.capability() {
                    Some(field) if !capability_present(&self.capabilities, field) => {},
                    Some(field) => problems.push(format!(
                        "{scenario}: not staged ({reason}) although `{field}` is declared: a declared capability must be proven"
                    )),
                    None => problems.push(format!(
                        "{scenario}: not staged ({reason}) although every provider must pass it"
                    )),
                },
            }
        }
        problems
    }

    /// Whether [`ConformanceReport::problems`] is empty.
    pub fn is_conformant(&self) -> bool {
        self.problems().is_empty()
    }

    /// One line per scenario.
    pub fn summary(&self) -> String {
        let mut text = format!("conformance of `{}`\n", self.target);
        for (scenario, outcome) in &self.results {
            let line = match outcome {
                ScenarioOutcome::Passed => "passed".to_owned(),
                ScenarioOutcome::FallbackVerified => "fallback verified".to_owned(),
                ScenarioOutcome::NotStaged(reason) => format!("not staged: {reason}"),
                ScenarioOutcome::Failed(failures) => format!("FAILED ({})", failures.len()),
            };
            text.push_str(&format!("  {scenario}: {line}\n"));
        }
        text
    }

    /// Panics, listing the problems, unless the target is conformant.
    ///
    /// # Panics
    ///
    /// Panics when [`ConformanceReport::problems`] is not empty.
    pub fn assert_conformant(&self) {
        let problems = self.problems();
        assert!(
            problems.is_empty(),
            "`{}` is not conformant to the agent contract:\n  - {}\n{}",
            self.target,
            problems.join("\n  - "),
            self.summary()
        );
    }
}

/// Runs every scenario against the target, one after the other.
pub async fn run_all(target: &dyn ConformanceTarget) -> ConformanceReport {
    let capabilities = target.provider().capabilities(None);
    let mut results = Vec::with_capacity(Scenario::ALL.len());
    for scenario in Scenario::ALL {
        results.push((scenario, run_scenario(target, scenario).await));
    }
    ConformanceReport {
        target: target.name().to_owned(),
        capabilities,
        results,
    }
}

/// Runs one scenario against the target. Never hangs: a scenario that exceeds
/// its time fails.
pub async fn run_scenario(target: &dyn ConformanceTarget, scenario: Scenario) -> ScenarioOutcome {
    match timeout(SCENARIO_TIMEOUT, run_staged(target, scenario)).await {
        Ok(outcome) => outcome,
        Err(_) => ScenarioOutcome::Failed(vec![format!(
            "the scenario did not end within {} s",
            SCENARIO_TIMEOUT.as_secs()
        )]),
    }
}

async fn run_staged(target: &dyn ConformanceTarget, scenario: Scenario) -> ScenarioOutcome {
    let declared = target.provider().capabilities(None);
    let declared_present = scenario
        .capability()
        .is_none_or(|field| capability_present(&declared, field));
    let mut prepared = bounded_quiet(target.prepare(scenario)).await.flatten();
    if prepared.is_none() && !declared_present {
        // The capability is absent: any session able to run a plain turn lets the
        // suite verify the fallback.
        prepared = bounded_quiet(target.prepare(Scenario::TourTexteSimple))
            .await
            .flatten();
    }
    let Some(prepared) = prepared else {
        return ScenarioOutcome::NotStaged("the target returned no staging".to_owned());
    };

    let capabilities = prepared
        .provider
        .capabilities(prepared.spec.model.as_deref());
    let present = scenario
        .capability()
        .is_none_or(|field| capability_present(&capabilities, field));
    let mut ctx = Ctx {
        scenario,
        caps: capabilities,
        failures: Vec::new(),
        tool_calls: HashSet::new(),
    };
    match scenario {
        Scenario::TourTexteSimple => tour_texte_simple(&mut ctx, &prepared).await,
        Scenario::FluxDeltas => flux_deltas(&mut ctx, &prepared).await,
        Scenario::Raisonnement => raisonnement(&mut ctx, &prepared).await,
        Scenario::AppelOutilResultat => appel_outil_resultat(&mut ctx, &prepared).await,
        Scenario::OutilsParalleles => outils_paralleles(&mut ctx, &prepared).await,
        Scenario::PermissionAccordee => permission(&mut ctx, &prepared, true).await,
        Scenario::PermissionRefusee => permission(&mut ctx, &prepared, false).await,
        Scenario::QuestionUtilisateur => question_utilisateur(&mut ctx, &prepared).await,
        Scenario::InterruptionEnFlux => interruption_en_flux(&mut ctx, &prepared).await,
        Scenario::InterruptionOutil => interruption_outil(&mut ctx, &prepared).await,
        Scenario::AnnulationTourPreserve => annulation_tour_preserve(&mut ctx, &prepared).await,
        Scenario::AnnulationTache => annulation_tache(&mut ctx, &prepared).await,
        Scenario::ChangementModele => changement_modele(&mut ctx, &prepared).await,
        Scenario::ChangementPolitique => changement_politique(&mut ctx, &prepared).await,
        Scenario::Reprise => reprise(&mut ctx, &prepared).await,
        Scenario::MessageImages => message_images(&mut ctx, &prepared).await,
        Scenario::SousAgent => sous_agent(&mut ctx, &prepared).await,
        Scenario::Compaction => compaction(&mut ctx, &prepared).await,
        Scenario::HorsTour => hors_tour(&mut ctx, &prepared).await,
        Scenario::PermissionHorsTour => permission_hors_tour(&mut ctx, &prepared).await,
        Scenario::TourConcurrent => tour_concurrent(&mut ctx, &prepared).await,
        Scenario::ErreurRetryable => erreur_retryable(&mut ctx, &prepared).await,
        Scenario::FinUsageCout => fin_usage_cout(&mut ctx, &prepared).await,
        Scenario::ProcessusMort => processus_mort(&mut ctx, &prepared).await,
        Scenario::FermetureIdempotente => fermeture_idempotente(&mut ctx, &prepared).await,
    }
    if !ctx.failures.is_empty() {
        ScenarioOutcome::Failed(ctx.failures)
    } else if present {
        ScenarioOutcome::Passed
    } else {
        ScenarioOutcome::FallbackVerified
    }
}

// ---------------------------------------------------------------------------
// Generic invariants
// ---------------------------------------------------------------------------

/// Which stream a list of events comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// The stream of a turn: must end with exactly one terminal event.
    Turn,
    /// The out-of-band stream: no terminal event is expected.
    OutOfBand,
}

fn parent_of(event: &AgentEvent) -> Option<&str> {
    match event {
        AgentEvent::UserEcho { parent, .. }
        | AgentEvent::Text { parent, .. }
        | AgentEvent::Thinking { parent, .. }
        | AgentEvent::Delta { parent, .. }
        | AgentEvent::ToolCall { parent, .. }
        | AgentEvent::ToolResult { parent, .. }
        | AgentEvent::PermissionAsk { parent, .. }
        | AgentEvent::Question { parent, .. } => parent.as_deref(),
        _ => None,
    }
}

/// Checks the generic invariants of `docs/agent-contract.md` §4 and the
/// event-level fallbacks of §5 on everything one stream carried, in order.
/// Returns one message per violation; empty when the stream is sound.
///
/// - a turn stream carries exactly one terminal event, last;
/// - every `tool_result.id` and `permission_ask.tool_call_id` names a `tool_call`
///   already emitted (`known_tool_calls` carries them from one stream of a
///   session to the next, and is updated);
/// - a `tool_call` with `input_complete: false` is followed by the same call,
///   complete, and a `delta` by its complete `text` or `thinking` (both checked
///   when the turn completed normally: an interruption may cut them short);
/// - every event survives a JSON round trip;
/// - nothing is emitted for an absent capability: no `thinking`, `compaction`,
///   `background_tasks`, `task_update`, `question`, `permission_ask`, `tool_call`
///   or `parent`, no scope outside `permission_scopes`, no cost amount when the
///   cost is `unknown`.
pub fn check_stream_invariants(
    events: &[AgentEvent],
    kind: StreamKind,
    capabilities: &Capabilities,
    known_tool_calls: &mut HashSet<String>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let terminals: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.is_terminal())
        .map(|(index, _)| index)
        .collect();
    if kind == StreamKind::Turn {
        match terminals.as_slice() {
            [] => problems.push(
                "the turn stream closed without a terminal event (`done` or `error`)".to_owned(),
            ),
            [first, rest @ ..] => {
                if !rest.is_empty() {
                    problems.push(format!(
                        "the turn stream carries {} terminal events, exactly one is allowed",
                        terminals.len()
                    ));
                }
                let trailing = events.len() - 1 - first;
                if trailing > 0 {
                    problems.push(format!(
                        "{trailing} event(s) emitted after the terminal event (first: `{}`)",
                        events[first + 1].type_name()
                    ));
                }
            },
        }
    }
    let completed = matches!(
        terminals.first().map(|index| &events[*index]),
        Some(AgentEvent::Done {
            stop_reason: StopReason::Completed,
            ..
        })
    );

    let mut incomplete: Vec<String> = Vec::new();
    let mut last_text_delta = None;
    let mut last_thinking_delta = None;
    let mut last_text = None;
    let mut last_thinking = None;
    for (index, event) in events.iter().enumerate() {
        let name = event.type_name();
        match serde_json::to_value(event).and_then(serde_json::from_value::<AgentEvent>) {
            Ok(back) if &back == event => {},
            Ok(_) => problems.push(format!(
                "event #{index} (`{name}`) changes through a JSON round trip"
            )),
            Err(error) => problems.push(format!(
                "event #{index} (`{name}`) does not survive a JSON round trip: {error}"
            )),
        }
        if parent_of(event).is_some() && capabilities.subagents == SubagentSupport::None {
            problems.push(format!(
                "event #{index} (`{name}`) carries `parent` although `subagents` is `none`"
            ));
        }
        match event {
            AgentEvent::Text { .. } => last_text = Some(index),
            AgentEvent::Thinking { .. } => {
                last_thinking = Some(index);
                if !capabilities.thinking {
                    problems.push(format!(
                        "event #{index}: `thinking` emitted although `thinking` is false"
                    ));
                }
            },
            AgentEvent::Delta { kind, .. } => match kind {
                DeltaKind::Text => last_text_delta = Some(index),
                DeltaKind::Thinking => {
                    last_thinking_delta = Some(index);
                    if !capabilities.thinking {
                        problems.push(format!(
                            "event #{index}: thinking `delta` emitted although `thinking` is false"
                        ));
                    }
                },
                DeltaKind::ToolInput => {},
            },
            AgentEvent::ToolCall {
                id, input_complete, ..
            } => {
                if id.is_empty() {
                    problems.push(format!("event #{index}: `tool_call` with an empty id"));
                }
                if !capabilities.tools {
                    problems.push(format!(
                        "event #{index}: `tool_call` emitted although `tools` is false"
                    ));
                }
                known_tool_calls.insert(id.clone());
                if *input_complete {
                    incomplete.retain(|pending| pending != id);
                } else if !incomplete.contains(id) {
                    incomplete.push(id.clone());
                }
            },
            AgentEvent::ToolResult { id, .. } => {
                if !known_tool_calls.contains(id) {
                    problems.push(format!(
                        "event #{index}: orphan `tool_result`: no `tool_call` with id `{id}` was emitted before"
                    ));
                }
            },
            AgentEvent::PermissionAsk {
                tool_call_id,
                scopes,
                ..
            } => {
                if !capabilities.interactive_permissions {
                    problems.push(format!(
                        "event #{index}: `permission_ask` emitted although `interactive_permissions` is false"
                    ));
                }
                if let Some(id) = tool_call_id
                    && !known_tool_calls.contains(id)
                {
                    problems.push(format!(
                        "event #{index}: `permission_ask.tool_call_id` `{id}` names no `tool_call` emitted before"
                    ));
                }
                for scope in scopes {
                    if !capabilities.permission_scopes.contains(scope) {
                        problems.push(format!(
                            "event #{index}: `permission_ask` offers scope {scope:?}, absent from `permission_scopes`"
                        ));
                    }
                }
            },
            AgentEvent::Question { .. } if !capabilities.native_question => {
                problems.push(format!(
                    "event #{index}: `question` emitted although `native_question` is false"
                ));
            },
            AgentEvent::Compaction { .. } if !capabilities.compaction_signal => {
                problems.push(format!(
                    "event #{index}: `compaction` emitted although `compaction_signal` is false"
                ));
            },
            AgentEvent::BackgroundTasks { .. } | AgentEvent::TaskUpdate { .. }
                if !capabilities.background_tasks =>
            {
                problems.push(format!(
                    "event #{index}: `{name}` emitted although `background_tasks` is false"
                ));
            },
            AgentEvent::Done { cost, .. }
                if capabilities.cost == CostBasis::Unknown
                    && (cost.usd.is_some() || cost.basis != CostBasis::Unknown) =>
            {
                problems.push(format!(
                        "event #{index}: `done.cost` is {cost:?} although `cost` is `unknown` (expected no amount, basis `unknown`)"
                    ));
            },
            _ => {},
        }
    }
    if completed {
        for id in incomplete {
            problems.push(format!(
                "`tool_call` `{id}` was emitted with `input_complete: false` and never completed"
            ));
        }
        if let Some(delta) = last_text_delta
            && last_text.is_none_or(|text| text < delta)
        {
            problems.push(
                "a text `delta` is not followed by the complete `text`: a consumer ignoring deltas loses it"
                    .to_owned(),
            );
        }
        if let Some(delta) = last_thinking_delta
            && last_thinking.is_none_or(|thinking| thinking < delta)
        {
            problems
                .push("a thinking `delta` is not followed by the complete `thinking`".to_owned());
        }
    }
    problems
}

// ---------------------------------------------------------------------------
// Driving a session
// ---------------------------------------------------------------------------

struct Ctx {
    scenario: Scenario,
    caps: Capabilities,
    failures: Vec<String>,
    /// Tool calls seen so far in the session.
    tool_calls: HashSet<String>,
}

impl Ctx {
    fn fail(&mut self, message: impl Into<String>) {
        self.failures.push(message.into());
    }

    fn input(&self) -> TurnInput {
        TurnInput::text(self.scenario.prompt())
    }

    /// Awaits a call on the provider, bounded. `None` (and a failure) on timeout.
    async fn bounded<T>(&mut self, what: &str, future: impl Future<Output = T>) -> Option<T> {
        match timeout(STEP_TIMEOUT, future).await {
            Ok(value) => Some(value),
            Err(_) => {
                self.fail(format!(
                    "{what} did not answer within {} s",
                    STEP_TIMEOUT.as_secs()
                ));
                None
            },
        }
    }

    /// Expects `Ok`.
    fn expect_ok<T>(&mut self, what: &str, result: Option<Result<T, ProviderError>>) -> Option<T> {
        match result? {
            Ok(value) => Some(value),
            Err(error) => {
                self.fail(format!("{what} failed: {error} (`{}`)", error.kind()));
                None
            },
        }
    }

    /// Expects `Unsupported` naming one of `accepted`.
    fn expect_unsupported<T>(
        &mut self,
        what: &str,
        accepted: &[&str],
        result: Option<Result<T, ProviderError>>,
    ) {
        match result {
            None => {},
            Some(Ok(_)) => self.fail(format!(
                "{what} succeeded silently although the capability is absent (expected `Unsupported {{ capability: {accepted:?} }}`)"
            )),
            Some(Err(ProviderError::Unsupported { capability }))
                if accepted.contains(&capability.as_str()) => {},
            Some(Err(error)) => self.fail(format!(
                "{what} answered `{}` ({error}), expected `Unsupported {{ capability: {accepted:?} }}`",
                error.kind()
            )),
        }
    }

    /// Expects an error of a given kind (the `kind` tag of [`ProviderError`]).
    fn expect_kind<T>(&mut self, what: &str, kind: &str, result: Option<Result<T, ProviderError>>) {
        match result {
            None => {},
            Some(Ok(_)) => self.fail(format!("{what} succeeded, expected `{kind}`")),
            Some(Err(error)) if error.kind() == kind => {},
            Some(Err(error)) => self.fail(format!(
                "{what} answered `{}` ({error}), expected `{kind}`",
                error.kind()
            )),
        }
    }
}

/// Awaits a future of the target, bounded, without recording a failure.
async fn bounded_quiet<T>(future: impl Future<Output = T>) -> Option<T> {
    timeout(STEP_TIMEOUT, future).await.ok()
}

async fn open_with(
    ctx: &mut Ctx,
    prepared: &Prepared,
    spec: SessionSpec,
) -> Option<Arc<dyn AgentSession>> {
    let opened = ctx.bounded("open()", prepared.provider.open(spec)).await;
    ctx.expect_ok("open()", opened)
}

async fn open(ctx: &mut Ctx, prepared: &Prepared) -> Option<Arc<dyn AgentSession>> {
    open_with(ctx, prepared, prepared.spec.clone()).await
}

/// Ends a scenario: the capabilities did not move, and `close()` works.
async fn finish(ctx: &mut Ctx, session: &dyn AgentSession) {
    if session.capabilities() != &ctx.caps {
        ctx.fail(
            "the session's capabilities differ from `provider.capabilities(model)`: the snapshot must be the provider's, frozen at opening",
        );
    }
    let closed = ctx.bounded("close()", session.close()).await;
    ctx.expect_ok("close()", closed);
}

/// What the suite does while it reads a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// Only read (and allow the permission requests).
    Passive,
    /// Allow the first permission request, after probing the refusals.
    Grant,
    /// Deny the first permission request.
    Refuse,
    /// Answer the first question.
    Question,
    /// Interrupt after the first event.
    Interrupt,
    /// Interrupt, tools included, once a tool call is complete.
    InterruptOnTool,
    /// Cancel the tools once a tool call is complete.
    CancelOnTool,
    /// Try a second turn after the first event, then interrupt.
    Concurrent,
    /// Close the session after the first event.
    Close,
}

fn pick_scope(scopes: &[PermissionScope]) -> PermissionScope {
    if scopes.is_empty() || scopes.contains(&PermissionScope::Once) {
        PermissionScope::Once
    } else {
        scopes[0]
    }
}

async fn start_turn(
    ctx: &mut Ctx,
    session: &dyn AgentSession,
    input: TurnInput,
) -> Option<EventStream> {
    let started = ctx.bounded("send_turn()", session.send_turn(input)).await;
    ctx.expect_ok("send_turn()", started)
}

/// Reads a turn stream to its end, acting on the session according to `plan`,
/// then runs the generic invariants on what was read.
async fn drive(
    ctx: &mut Ctx,
    session: &dyn AgentSession,
    mut stream: EventStream,
    plan: Plan,
) -> Vec<AgentEvent> {
    let mut events: Vec<AgentEvent> = Vec::new();
    let mut acted = false;
    let mut terminal_seen = false;
    let mut trailing = 0;
    loop {
        let wait = if terminal_seen {
            CLOSE_TIMEOUT
        } else {
            STEP_TIMEOUT
        };
        let next = match timeout(wait, stream.next()).await {
            Ok(next) => next,
            Err(_) if terminal_seen => {
                ctx.fail("the turn stream stayed open after its terminal event");
                break;
            },
            Err(_) => {
                ctx.fail(format!(
                    "the turn emitted nothing for {} s and has no terminal event: it hangs",
                    STEP_TIMEOUT.as_secs()
                ));
                break;
            },
        };
        let Some(event) = next else { break };
        events.push(event.clone());
        if terminal_seen {
            // Recorded for the invariants; a stream that never stops is cut.
            trailing += 1;
            if trailing >= 16 {
                break;
            }
            continue;
        }
        if event.is_terminal() {
            terminal_seen = true;
            continue;
        }
        let failures_before = ctx.failures.len();
        react(ctx, session, plan, &event, &mut acted).await;
        if ctx.failures.len() > failures_before {
            // The turn cannot go on as staged once an action failed.
            break;
        }
    }
    if !acted && plan != Plan::Passive {
        ctx.fail(format!(
            "the turn never reached the point where the suite acts ({plan:?}): the scenario is not staged as documented"
        ));
    }
    let problems =
        check_stream_invariants(&events, StreamKind::Turn, &ctx.caps, &mut ctx.tool_calls);
    ctx.failures.extend(problems);
    events
}

async fn react(
    ctx: &mut Ctx,
    session: &dyn AgentSession,
    plan: Plan,
    event: &AgentEvent,
    acted: &mut bool,
) {
    match event {
        AgentEvent::PermissionAsk {
            request_id, scopes, ..
        } => {
            let probe = matches!(plan, Plan::Grant | Plan::Refuse) && !*acted;
            if probe {
                *acted = true;
                answer_with_probes(ctx, session, request_id, scopes, plan == Plan::Grant).await;
            } else if ctx.caps.interactive_permissions {
                let decision = PermissionDecision::Allow {
                    scope: pick_scope(scopes),
                    updated_input: None,
                };
                let answered = ctx
                    .bounded(
                        "answer_permission()",
                        session.answer_permission(request_id, decision),
                    )
                    .await;
                ctx.expect_ok("answer_permission(allow)", answered);
            }
            return;
        },
        AgentEvent::Question {
            question_id,
            reply,
            questions,
            ..
        } if plan == Plan::Question && !*acted => {
            *acted = true;
            if question_id.is_empty() || questions.is_empty() {
                ctx.fail("`question` without an identifier or without any question");
            }
            match reply {
                QuestionReply::Call => {
                    let answer = QuestionAnswer::Answered {
                        answers: questions
                            .iter()
                            .map(|question| QuestionAnswerItem {
                                question: question.question.clone(),
                                selected: question
                                    .options
                                    .first()
                                    .map(|option| vec![option.label.clone()])
                                    .unwrap_or_default(),
                                free_text: Some("conformance".to_owned()),
                            })
                            .collect(),
                    };
                    let first = ctx
                        .bounded(
                            "answer_question()",
                            session.answer_question(question_id, answer.clone()),
                        )
                        .await;
                    ctx.expect_ok("answer_question()", first);
                    let second = ctx
                        .bounded(
                            "answer_question()",
                            session.answer_question(question_id, answer),
                        )
                        .await;
                    ctx.expect_kind(
                        "answer_question() on a question already answered",
                        "invalid_request",
                        second,
                    );
                },
                QuestionReply::Turn => {
                    let refused = ctx
                        .bounded(
                            "answer_question()",
                            session.answer_question(question_id, QuestionAnswer::Cancelled),
                        )
                        .await;
                    ctx.expect_unsupported(
                        "answer_question() on a question answered by a turn",
                        &["answer_question"],
                        refused,
                    );
                },
            }
            return;
        },
        _ => {},
    }
    if *acted {
        return;
    }
    let complete_tool_call = matches!(
        event,
        AgentEvent::ToolCall {
            input_complete: true,
            ..
        }
    );
    match plan {
        Plan::Interrupt | Plan::Concurrent => {
            *acted = true;
            if plan == Plan::Concurrent {
                let input = TurnInput::text("conformance:concurrent");
                let second = ctx.bounded("send_turn()", session.send_turn(input)).await;
                ctx.expect_kind(
                    "send_turn() while a turn is running",
                    "turn_in_progress",
                    second,
                );
            }
            interrupt_running(ctx, session, InterruptScope::TurnOnly).await;
        },
        Plan::InterruptOnTool if complete_tool_call => {
            *acted = true;
            interrupt_running(ctx, session, InterruptScope::TurnAndTools).await;
        },
        Plan::CancelOnTool if complete_tool_call => {
            *acted = true;
            let cancelled = ctx
                .bounded("cancel_tools(all)", session.cancel_tools(CancelScope::All))
                .await;
            if let Some(outcome) = ctx.expect_ok("cancel_tools(all)", cancelled)
                && outcome.tools_cancelled == 0
            {
                ctx.fail("cancel_tools(all) reports no tool cancelled while one was running");
            }
        },
        Plan::Close => {
            *acted = true;
            let closed = ctx.bounded("close()", session.close()).await;
            ctx.expect_ok("close() during a turn", closed);
        },
        _ => {},
    }
}

async fn interrupt_running(ctx: &mut Ctx, session: &dyn AgentSession, scope: InterruptScope) {
    let interrupted = ctx.bounded("interrupt()", session.interrupt(scope)).await;
    if let Some(outcome) = ctx.expect_ok("interrupt()", interrupted)
        && !outcome.turn_interrupted
    {
        ctx.fail("interrupt() during a turn reports `turn_interrupted: false`");
    }
}

/// Probes the refusals of `answer_permission`, answers, then answers again.
async fn answer_with_probes(
    ctx: &mut Ctx,
    session: &dyn AgentSession,
    request_id: &str,
    scopes: &[PermissionScope],
    grant: bool,
) {
    if scopes.is_empty() {
        ctx.fail("`permission_ask` offers no scope");
    }
    let unknown = ctx
        .bounded(
            "answer_permission()",
            session.answer_permission(
                "conformance-unknown-request",
                PermissionDecision::allow_once(),
            ),
        )
        .await;
    ctx.expect_kind(
        "answer_permission() with an unknown identifier",
        "invalid_request",
        unknown,
    );
    let undeclared = [
        PermissionScope::Once,
        PermissionScope::Session,
        PermissionScope::Always,
    ]
    .into_iter()
    .find(|scope| !ctx.caps.permission_scopes.contains(scope));
    if let Some(scope) = undeclared {
        let decision = PermissionDecision::Allow {
            scope,
            updated_input: None,
        };
        let refused = ctx
            .bounded(
                "answer_permission()",
                session.answer_permission(request_id, decision),
            )
            .await;
        ctx.expect_unsupported(
            &format!("answer_permission() with the undeclared scope {scope:?}"),
            &["permission_scope"],
            refused,
        );
    }
    let decision = if grant {
        PermissionDecision::Allow {
            scope: pick_scope(scopes),
            updated_input: None,
        }
    } else {
        PermissionDecision::Deny {
            message: Some("denied by the conformance suite".to_owned()),
            interrupt: false,
        }
    };
    let answered = ctx
        .bounded(
            "answer_permission()",
            session.answer_permission(request_id, decision.clone()),
        )
        .await;
    ctx.expect_ok("answer_permission()", answered);
    let again = ctx
        .bounded(
            "answer_permission()",
            session.answer_permission(request_id, decision),
        )
        .await;
    ctx.expect_kind(
        "answer_permission() on a request already answered",
        "invalid_request",
        again,
    );
}

fn terminal(events: &[AgentEvent]) -> Option<&AgentEvent> {
    events.iter().find(|event| event.is_terminal())
}

fn expect_stop(ctx: &mut Ctx, events: &[AgentEvent], expected: StopReason, what: &str) {
    match terminal(events) {
        Some(AgentEvent::Done { stop_reason, .. }) if *stop_reason == expected => {},
        Some(AgentEvent::Done { stop_reason, .. }) => ctx.fail(format!(
            "{what} ended with `done {{ stop_reason: {stop_reason:?} }}`, expected {expected:?}"
        )),
        Some(AgentEvent::Error { error }) => ctx.fail(format!(
            "{what} ended with `error` ({error}), expected `done {{ stop_reason: {expected:?} }}`"
        )),
        // The absence of a terminal event is reported by the invariants.
        _ => {},
    }
}

/// Sends a turn and reads it passively; `None` when it could not start.
async fn turn(
    ctx: &mut Ctx,
    session: &dyn AgentSession,
    input: TurnInput,
    plan: Plan,
) -> Option<Vec<AgentEvent>> {
    let stream = start_turn(ctx, session, input).await?;
    Some(drive(ctx, session, stream, plan).await)
}

/// A turn expected to complete normally.
async fn plain_turn(ctx: &mut Ctx, session: &dyn AgentSession, what: &str) -> Vec<AgentEvent> {
    let input = ctx.input();
    let events = turn(ctx, session, input, Plan::Passive)
        .await
        .unwrap_or_default();
    expect_stop(ctx, &events, StopReason::Completed, what);
    events
}

/// The out-of-band stream of a session, read in the background.
struct OutOfBand {
    events: Arc<Mutex<Vec<AgentEvent>>>,
    reader: JoinHandle<()>,
}

impl OutOfBand {
    fn take(ctx: &mut Ctx, session: &dyn AgentSession) -> Option<Self> {
        let Some(mut stream) = session.out_of_band() else {
            ctx.fail("out_of_band() answered `None` on its first call");
            return None;
        };
        if session.out_of_band().is_some() {
            ctx.fail("out_of_band() answered `Some` twice: it has a single consumer");
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let reader = tokio::spawn(async move {
            while let Some(event) = stream.next().await {
                sink.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(event);
            }
        });
        Some(Self { events, reader })
    }

    fn snapshot(&self) -> Vec<AgentEvent> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Waits for an event matching `wanted` among those received from `from` on.
    async fn wait_for(
        &self,
        from: usize,
        within: Duration,
        wanted: impl Fn(&AgentEvent) -> bool,
    ) -> Option<AgentEvent> {
        let search = async {
            loop {
                if let Some(event) = self.snapshot().into_iter().skip(from).find(&wanted) {
                    return event;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        timeout(within, search).await.ok()
    }

    /// Runs the invariants on what was received.
    fn check(&self, ctx: &mut Ctx) -> Vec<AgentEvent> {
        let events = self.snapshot();
        let problems = check_stream_invariants(
            &events,
            StreamKind::OutOfBand,
            &ctx.caps,
            &mut ctx.tool_calls,
        );
        ctx.failures.extend(
            problems
                .into_iter()
                .map(|problem| format!("out of band: {problem}")),
        );
        events
    }
}

impl Drop for OutOfBand {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Hooks that count their calls.
#[derive(Default)]
struct CountingHooks {
    before_tool: AtomicU32,
    after_tool: AtomicU32,
}

#[async_trait]
impl SessionHooks for CountingHooks {
    async fn before_tool(&self, _call: &ToolCallInfo) -> HookVerdict {
        self.before_tool.fetch_add(1, Ordering::SeqCst);
        HookVerdict::Continue
    }

    async fn after_tool(&self, _result: &ToolResultInfo) -> Option<String> {
        self.after_tool.fetch_add(1, Ordering::SeqCst);
        None
    }
}

/// With `tools` or `per_session_mcp` absent, `open` with an MCP server must be
/// refused, with the error of the absent capability.
async fn check_mcp_refusal(ctx: &mut Ctx, prepared: &Prepared) {
    if ctx.caps.tools && ctx.caps.per_session_mcp {
        return;
    }
    let mut spec = prepared.spec.clone();
    spec.mcp_servers.insert(
        "conformance".to_owned(),
        McpServerSpec::stdio("conformance-mcp-server-that-must-not-start"),
    );
    let opened = ctx.bounded("open()", prepared.provider.open(spec)).await;
    match opened {
        None => {},
        Some(Ok(session)) => {
            ctx.fail(format!(
                "open() with an MCP server succeeded silently although tools={} and per_session_mcp={}",
                ctx.caps.tools, ctx.caps.per_session_mcp
            ));
            let _ = bounded_quiet(session.close()).await;
        },
        Some(Err(ProviderError::ModelNoTools { .. })) if !ctx.caps.tools => {},
        Some(Err(ProviderError::Unsupported { capability }))
            if capability == "per_session_mcp" && !ctx.caps.per_session_mcp => {},
        Some(Err(error)) => ctx.fail(format!(
            "open() with an MCP server answered `{}` ({error}); expected `model_no_tools` (tools absent) or `Unsupported {{ per_session_mcp }}` (per_session_mcp absent)",
            error.kind()
        )),
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

async fn tour_texte_simple(ctx: &mut Ctx, prepared: &Prepared) {
    if prepared.provider.id().is_empty() {
        ctx.fail("the provider has an empty instance id");
    }
    let _ = ctx.bounded("health()", prepared.provider.health()).await;
    let _ = ctx.bounded("catalog()", prepared.provider.catalog()).await;
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    let events = plain_turn(ctx, &*session, "the plain turn").await;
    let has_text = events.iter().any(
        |event| matches!(event, AgentEvent::Text { text, parent: None, .. } if !text.is_empty()),
    );
    if !has_text {
        ctx.fail("the turn carries no non-empty top-level `text`");
    }
    finish(ctx, &*session).await;
}

async fn flux_deltas(ctx: &mut Ctx, prepared: &Prepared) {
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    let events = plain_turn(ctx, &*session, "the streamed turn").await;
    let has_delta = events.iter().any(|event| {
        matches!(
            event,
            AgentEvent::Delta {
                kind: DeltaKind::Text,
                ..
            }
        )
    });
    if !has_delta {
        ctx.fail("the turn carries no text `delta`");
    }
    finish(ctx, &*session).await;
}

async fn raisonnement(ctx: &mut Ctx, prepared: &Prepared) {
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    let events = plain_turn(ctx, &*session, "the reasoning turn").await;
    // `thinking: false`: the invariants already refuse any `thinking`.
    if ctx.caps.thinking
        && !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Thinking { text, .. } if !text.is_empty()))
    {
        ctx.fail("`thinking` is declared but the turn carries no non-empty `thinking`");
    }
    finish(ctx, &*session).await;
}

/// The complete tool calls of a turn, in order.
fn complete_calls(events: &[AgentEvent]) -> Vec<(usize, &str)> {
    events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            AgentEvent::ToolCall {
                id,
                input_complete: true,
                ..
            } => Some((index, id.as_str())),
            _ => None,
        })
        .collect()
}

/// The result of a call: its position and whether it is an error.
fn result_of(events: &[AgentEvent], call: &str) -> Option<(usize, bool)> {
    events
        .iter()
        .enumerate()
        .find_map(|(index, event)| match event {
            AgentEvent::ToolResult { id, is_error, .. } if id == call => Some((index, *is_error)),
            _ => None,
        })
}

async fn appel_outil_resultat(ctx: &mut Ctx, prepared: &Prepared) {
    check_mcp_refusal(ctx, prepared).await;
    let hooks = Arc::new(CountingHooks::default());
    let mut spec = prepared.spec.clone();
    spec.hooks = Some(hooks.clone());
    let Some(session) = open_with(ctx, prepared, spec).await else {
        return;
    };
    let events = plain_turn(ctx, &*session, "the tool turn").await;
    if ctx.caps.tools {
        match complete_calls(&events).first() {
            None => ctx.fail("`tools` is declared but the turn carries no complete `tool_call`"),
            Some((position, id)) => match result_of(&events, id) {
                None => ctx.fail(format!("`tool_call` `{id}` has no `tool_result`")),
                Some((result, _)) if result < *position => {
                    ctx.fail(format!(
                        "the `tool_result` of `{id}` precedes its `tool_call`"
                    ));
                },
                Some((_, true)) => ctx.fail(format!(
                    "the `tool_result` of `{id}` is an error, the staged tool should succeed"
                )),
                Some(_) => {},
            },
        }
        if ctx.caps.hooks == HookSupport::InProtocol {
            let called = async {
                while hooks.before_tool.load(Ordering::SeqCst) == 0
                    || hooks.after_tool.load(Ordering::SeqCst) == 0
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            };
            if timeout(CLOSE_TIMEOUT, called).await.is_err() {
                ctx.fail(format!(
                    "`hooks` is `in_protocol` but the hooks of the spec were not called around the tool (before_tool: {}, after_tool: {})",
                    hooks.before_tool.load(Ordering::SeqCst),
                    hooks.after_tool.load(Ordering::SeqCst)
                ));
            }
        }
    }
    finish(ctx, &*session).await;
}

async fn outils_paralleles(ctx: &mut Ctx, prepared: &Prepared) {
    check_mcp_refusal(ctx, prepared).await;
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    let events = plain_turn(ctx, &*session, "the parallel tools turn").await;
    if ctx.caps.tools {
        let calls = complete_calls(&events);
        let first_result = events
            .iter()
            .position(|event| matches!(event, AgentEvent::ToolResult { .. }))
            .unwrap_or(events.len());
        let mut pending: Vec<&str> = Vec::new();
        for (position, id) in &calls {
            if *position < first_result && !pending.contains(id) {
                pending.push(id);
            }
        }
        if pending.len() < 2 {
            ctx.fail(format!(
                "expected two tool calls pending at the same time, found {} before the first `tool_result`",
                pending.len()
            ));
        }
        for (_, id) in calls {
            if result_of(&events, id).is_none() {
                ctx.fail(format!("`tool_call` `{id}` has no `tool_result`"));
            }
        }
    }
    finish(ctx, &*session).await;
}

async fn permission(ctx: &mut Ctx, prepared: &Prepared, grant: bool) {
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    if !ctx.caps.interactive_permissions {
        let refused = ctx
            .bounded(
                "answer_permission()",
                session.answer_permission(
                    "conformance-unknown-request",
                    PermissionDecision::allow_once(),
                ),
            )
            .await;
        ctx.expect_unsupported("answer_permission()", &["interactive_permissions"], refused);
        // No `permission_ask` in the turn: checked by the invariants.
        plain_turn(ctx, &*session, "the turn without interactive permissions").await;
        finish(ctx, &*session).await;
        return;
    }
    let input = ctx.input();
    let plan = if grant { Plan::Grant } else { Plan::Refuse };
    let events = turn(ctx, &*session, input, plan).await.unwrap_or_default();
    let asked = events.iter().find_map(|event| match event {
        AgentEvent::PermissionAsk { tool_call_id, .. } => Some(tool_call_id.clone()),
        _ => None,
    });
    if grant {
        expect_stop(
            ctx,
            &events,
            StopReason::Completed,
            "the turn after the approval",
        );
    } else if let Some(AgentEvent::Error { error }) = terminal(&events) {
        ctx.fail(format!(
            "the turn after the denial ended with `error` ({error}), expected `done`"
        ));
    }
    if let Some(Some(call)) = asked {
        match (grant, result_of(&events, &call)) {
            (true, None) => ctx.fail(format!("the approved call `{call}` has no `tool_result`")),
            (true, Some((_, true))) => {
                ctx.fail(format!("the approved call `{call}` ended in error"))
            },
            (false, Some((_, false))) => ctx.fail(format!(
                "the denied call `{call}` has a successful `tool_result`: the tool ran"
            )),
            _ => {},
        }
    }
    finish(ctx, &*session).await;
}

async fn question_utilisateur(ctx: &mut Ctx, prepared: &Prepared) {
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    if !ctx.caps.native_question {
        let refused = ctx
            .bounded(
                "answer_question()",
                session.answer_question("conformance-unknown-question", QuestionAnswer::Cancelled),
            )
            .await;
        ctx.expect_unsupported("answer_question()", &["native_question"], refused);
        plain_turn(ctx, &*session, "the turn without native question").await;
        finish(ctx, &*session).await;
        return;
    }
    let input = ctx.input();
    let events = turn(ctx, &*session, input, Plan::Question)
        .await
        .unwrap_or_default();
    let reply = events.iter().find_map(|event| match event {
        AgentEvent::Question { reply, .. } => Some(*reply),
        _ => None,
    });
    if let Some(AgentEvent::Error { error }) = terminal(&events) {
        ctx.fail(format!("the question turn ended with `error` ({error})"));
    }
    match reply {
        Some(QuestionReply::Call) => {
            expect_stop(
                ctx,
                &events,
                StopReason::Completed,
                "the turn after the answer",
            );
        },
        Some(QuestionReply::Turn) => {
            let answer = TurnInput::text(format!("{}:answer", ctx.scenario.prompt()));
            let events = turn(ctx, &*session, answer, Plan::Passive)
                .await
                .unwrap_or_default();
            expect_stop(
                ctx,
                &events,
                StopReason::Completed,
                "the turn carrying the answer",
            );
        },
        // A missing question is already reported by `drive`.
        _ => {},
    }
    finish(ctx, &*session).await;
}

async fn interruption_en_flux(ctx: &mut Ctx, prepared: &Prepared) {
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    let input = ctx.input();
    let events = turn(ctx, &*session, input, Plan::Interrupt)
        .await
        .unwrap_or_default();
    expect_stop(
        ctx,
        &events,
        StopReason::Interrupted,
        "the interrupted turn",
    );
    let outside = ctx
        .bounded("interrupt()", session.interrupt(InterruptScope::TurnOnly))
        .await;
    if let Some(outcome) = ctx.expect_ok("interrupt() outside a turn", outside)
        && outcome.turn_interrupted
    {
        ctx.fail("interrupt() outside a turn reports `turn_interrupted: true`");
    }
    finish(ctx, &*session).await;
}

async fn interruption_outil(ctx: &mut Ctx, prepared: &Prepared) {
    check_mcp_refusal(ctx, prepared).await;
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    if !ctx.caps.tools {
        plain_turn(ctx, &*session, "the turn without tools").await;
        finish(ctx, &*session).await;
        return;
    }
    let input = ctx.input();
    let events = turn(ctx, &*session, input, Plan::InterruptOnTool)
        .await
        .unwrap_or_default();
    expect_stop(
        ctx,
        &events,
        StopReason::Interrupted,
        "the interrupted turn",
    );
    if let Some((_, call)) = complete_calls(&events).first()
        && let Some((_, false)) = result_of(&events, call)
    {
        ctx.fail(format!(
            "the call `{call}` cut by the interruption has a successful `tool_result`"
        ));
    }
    finish(ctx, &*session).await;
}

async fn annulation_tour_preserve(ctx: &mut Ctx, prepared: &Prepared) {
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    if !ctx.caps.tool_cancel {
        let refused = ctx
            .bounded("cancel_tools(all)", session.cancel_tools(CancelScope::All))
            .await;
        ctx.expect_unsupported("cancel_tools(all)", &["tool_cancel"], refused);
        plain_turn(ctx, &*session, "the turn after the refused cancellation").await;
        finish(ctx, &*session).await;
        return;
    }
    let input = ctx.input();
    let events = turn(ctx, &*session, input, Plan::CancelOnTool)
        .await
        .unwrap_or_default();
    expect_stop(
        ctx,
        &events,
        StopReason::Completed,
        "the turn whose tools were cancelled (the turn must be preserved)",
    );
    if let Some((_, call)) = complete_calls(&events).first() {
        match result_of(&events, call) {
            Some((_, true)) => {},
            Some((_, false)) => ctx.fail(format!(
                "the cancelled call `{call}` has a successful `tool_result`"
            )),
            None => ctx.fail(format!(
                "the cancelled call `{call}` has no `tool_result {{ is_error: true }}`"
            )),
        }
    }
    finish(ctx, &*session).await;
}

fn running_task(events: &[AgentEvent]) -> Option<String> {
    events.iter().rev().find_map(|event| match event {
        AgentEvent::BackgroundTasks { tasks } => tasks
            .iter()
            .find(|task| task.status == BackgroundTaskStatus::Running)
            .map(|task| task.id.clone()),
        _ => None,
    })
}

async fn annulation_tache(ctx: &mut Ctx, prepared: &Prepared) {
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    if !ctx.caps.background_tasks || !ctx.caps.tool_cancel {
        let mut accepted = Vec::new();
        if !ctx.caps.background_tasks {
            accepted.push("background_tasks");
        }
        if !ctx.caps.tool_cancel {
            accepted.push("tool_cancel");
        }
        let scope = CancelScope::Task {
            id: "conformance-unknown-task".to_owned(),
        };
        let refused = ctx
            .bounded("cancel_tools(task)", session.cancel_tools(scope))
            .await;
        ctx.expect_unsupported("cancel_tools(task)", &accepted, refused);
        // No `background_tasks` nor `task_update`: checked by the invariants.
        plain_turn(ctx, &*session, "the turn without background tasks").await;
        finish(ctx, &*session).await;
        return;
    }
    let Some(out_of_band) = OutOfBand::take(ctx, &*session) else {
        return;
    };
    let events = plain_turn(ctx, &*session, "the turn starting a background task").await;
    let mut task = running_task(&events);
    if task.is_none() {
        task = out_of_band
            .wait_for(0, STEP_TIMEOUT, |event| {
                running_task(std::slice::from_ref(event)).is_some()
            })
            .await
            .and_then(|event| running_task(&[event]));
    }
    let Some(task) = task else {
        ctx.fail(
            "`background_tasks` is declared but no `background_tasks` snapshot with a running task was emitted",
        );
        finish(ctx, &*session).await;
        return;
    };
    let seen = out_of_band.snapshot().len();
    let scope = CancelScope::Task { id: task.clone() };
    let cancelled = ctx
        .bounded("cancel_tools(task)", session.cancel_tools(scope))
        .await;
    if ctx.expect_ok("cancel_tools(task)", cancelled).is_some() {
        let gone = out_of_band
            .wait_for(seen, STEP_TIMEOUT, |event| match event {
                AgentEvent::BackgroundTasks { tasks } => tasks.iter().all(|candidate| {
                    candidate.id != task || candidate.status != BackgroundTaskStatus::Running
                }),
                _ => false,
            })
            .await;
        if gone.is_none() {
            ctx.fail(format!(
                "after cancel_tools(task), no `background_tasks` snapshot shows `{task}` gone or killed"
            ));
        }
    }
    out_of_band.check(ctx);
    finish(ctx, &*session).await;
}

/// A model to switch to: another one from the catalogue when there is one.
async fn other_model(prepared: &Prepared) -> String {
    let current = prepared.spec.model.as_deref();
    let catalog = bounded_quiet(prepared.provider.catalog())
        .await
        .and_then(Result::ok)
        .unwrap_or_default();
    catalog
        .iter()
        .find(|model| Some(model.id.as_str()) != current && !model.is_default)
        .or_else(|| catalog.first())
        .map(|model| model.id.clone())
        .or_else(|| current.map(str::to_owned))
        .unwrap_or_else(|| "conformance-model".to_owned())
}

async fn changement_modele(ctx: &mut Ctx, prepared: &Prepared) {
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    let model = other_model(prepared).await;
    let changed = ctx.bounded("set_model()", session.set_model(&model)).await;
    if ctx.caps.set_model_live {
        ctx.expect_ok("set_model()", changed);
        if session.capabilities() != &ctx.caps {
            ctx.fail(
                "set_model() changed the session's capabilities: the snapshot is frozen at opening",
            );
        }
    } else {
        ctx.expect_unsupported("set_model()", &["set_model_live"], changed);
    }
    plain_turn(ctx, &*session, "the turn after set_model()").await;
    finish(ctx, &*session).await;
}

async fn changement_politique(ctx: &mut Ctx, prepared: &Prepared) {
    // Refusals at opening, on variants of the staged spec. Nothing is spawned:
    // each must be refused before the provider starts anything.
    let mut above = prepared.spec.clone();
    above.policy = ToolPolicy::new(PolicyMode::AutoEdits);
    above.policy_ceiling = Some(ToolPolicy::new(PolicyMode::PlanOnly));
    let opened = ctx.bounded("open()", prepared.provider.open(above)).await;
    ctx.expect_unsupported(
        "open() with a policy above its ceiling",
        &["policy_ceiling"],
        opened,
    );
    // `trust` is a mode like the others, on every provider: the sandbox level is
    // information for the user (what isolates the tools), never a gate. A provider that
    // cannot honour the mode says so with its own typed refusal; it does not blame a
    // missing sandbox. (Decision of 2026-10-07: behave the same whatever the provider.)
    {
        let mut trust = prepared.spec.clone();
        trust.policy = ToolPolicy::new(PolicyMode::Trust);
        trust.policy_ceiling = None;
        match ctx.bounded("open()", prepared.provider.open(trust)).await {
            Some(Ok(session)) => {
                let _ = session.close().await;
            },
            Some(Err(ProviderError::Unsupported { capability })) if capability == "sandbox" => {
                ctx.fail(
                    "open() in `trust` mode was refused for lack of a sandbox: the sandbox is \
                     information, not a gate",
                );
            },
            Some(Err(_)) | None => {},
        }
    }

    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    let changed = ctx
        .bounded(
            "set_policy_mode()",
            session.set_policy_mode(PolicyMode::AutoEdits, None),
        )
        .await;
    match changed {
        None | Some(Ok(())) | Some(Err(ProviderError::Unsupported { .. })) => {},
        Some(Err(error)) => ctx.fail(format!(
            "set_policy_mode() answered `{}` ({error}); expected `Ok` or `Unsupported`",
            error.kind()
        )),
    }
    plain_turn(ctx, &*session, "the turn after set_policy_mode()").await;
    finish(ctx, &*session).await;
}

async fn reprise(ctx: &mut Ctx, prepared: &Prepared) {
    let kind = prepared.provider.kind();
    if !ctx.caps.resume {
        let token = prepared
            .resume
            .clone()
            .unwrap_or_else(|| ResumeToken::new(kind, 1, json!({})));
        let resumed = ctx
            .bounded(
                "resume()",
                prepared.provider.resume(prepared.spec.clone(), token),
            )
            .await;
        ctx.expect_unsupported("resume()", &["resume"], resumed);
        let Some(session) = open(ctx, prepared).await else {
            return;
        };
        plain_turn(ctx, &*session, "the turn of a session that cannot resume").await;
        if session.resume_token().is_some() {
            ctx.fail("resume_token() is `Some` although `resume` is false");
        }
        finish(ctx, &*session).await;
        return;
    }
    let Some(token) = prepared.resume.clone() else {
        ctx.fail(
            "`resume` is declared but the target supplied no resume token (`Prepared::resume`)",
        );
        return;
    };
    let foreign_kind = if kind == ProviderKind::ClaudeCode {
        ProviderKind::Codex
    } else {
        ProviderKind::ClaudeCode
    };
    let foreign = ResumeToken::new(foreign_kind, 1, json!({ "session_id": "conformance" }));
    let refused = ctx
        .bounded(
            "resume()",
            prepared.provider.resume(prepared.spec.clone(), foreign),
        )
        .await;
    ctx.expect_kind(
        "resume() with a token of another provider family",
        "invalid_request",
        refused,
    );
    let resumed = ctx
        .bounded(
            "resume()",
            prepared.provider.resume(prepared.spec.clone(), token),
        )
        .await;
    let Some(session) = ctx.expect_ok("resume()", resumed) else {
        return;
    };
    plain_turn(ctx, &*session, "the resumed turn").await;
    match session.resume_token() {
        None => ctx.fail("resume_token() is `None` after a turn although `resume` is declared"),
        Some(token) if token.kind() != kind => ctx.fail(format!(
            "resume_token() is of kind {:?}, the provider is {kind:?}",
            token.kind()
        )),
        Some(_) => {},
    }
    finish(ctx, &*session).await;
}

/// A 1×1 transparent PNG.
const PIXEL_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

async fn message_images(ctx: &mut Ctx, prepared: &Prepared) {
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    let mut input = ctx.input();
    input.blocks.push(InputBlock::Image {
        media_type: "image/png".to_owned(),
        data_base64: PIXEL_PNG_BASE64.to_owned(),
    });
    let started = ctx.bounded("send_turn()", session.send_turn(input)).await;
    if ctx.caps.images {
        if let Some(stream) = ctx.expect_ok("send_turn() with an image", started) {
            let events = drive(ctx, &*session, stream, Plan::Passive).await;
            expect_stop(
                ctx,
                &events,
                StopReason::Completed,
                "the turn with an image",
            );
        }
    } else {
        match started {
            None => {},
            Some(Ok(stream)) => {
                ctx.fail(
                    "send_turn() accepted an image block silently although `images` is false (expected `Unsupported { capability: \"images\" }`)",
                );
                // Let the accepted turn end so the session can be closed cleanly.
                let _ = timeout(STEP_TIMEOUT, stream.collect::<Vec<_>>()).await;
            },
            Some(Err(ProviderError::Unsupported { capability })) if capability == "images" => {
                // The refusal must not leave a turn running.
                plain_turn(ctx, &*session, "the text turn after the refused image").await;
            },
            Some(Err(error)) => ctx.fail(format!(
                "send_turn() with an image answered `{}` ({error}), expected `Unsupported {{ capability: \"images\" }}`",
                error.kind()
            )),
        }
    }
    finish(ctx, &*session).await;
}

async fn sous_agent(ctx: &mut Ctx, prepared: &Prepared) {
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    let events = plain_turn(ctx, &*session, "the sub-agent turn").await;
    // `subagents: none`: the invariants already refuse any `parent`.
    if ctx.caps.subagents != SubagentSupport::None {
        let mut calls: HashSet<&str> = HashSet::new();
        let mut children = 0;
        for event in &events {
            if let Some(parent) = parent_of(event) {
                children += 1;
                if ctx.caps.subagents == SubagentSupport::Nested && !calls.contains(parent) {
                    ctx.fail(format!(
                        "`{}` carries parent `{parent}`, which is no `tool_call` emitted before (`subagents: nested`)",
                        event.type_name()
                    ));
                }
            }
            if let AgentEvent::ToolCall { id, .. } = event {
                calls.insert(id);
            }
        }
        if children == 0 {
            ctx.fail("`subagents` is declared but no event of the turn carries `parent`");
        }
    }
    finish(ctx, &*session).await;
}

async fn compaction(ctx: &mut Ctx, prepared: &Prepared) {
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    let Some(out_of_band) = OutOfBand::take(ctx, &*session) else {
        return;
    };
    let events = plain_turn(ctx, &*session, "the turn with a compaction").await;
    let is_completed = |event: &AgentEvent| {
        matches!(
            event,
            AgentEvent::Compaction {
                phase: CompactionPhase::Completed,
                ..
            }
        )
    };
    if ctx.caps.compaction_signal {
        let mut stream = events.clone();
        if !stream.iter().any(is_completed) {
            out_of_band.wait_for(0, STEP_TIMEOUT, is_completed).await;
            stream = out_of_band.snapshot();
        }
        match stream.iter().position(is_completed) {
            None => ctx.fail(
                "`compaction_signal` is declared but no `compaction { phase: completed }` was emitted",
            ),
            Some(completed) => {
                let started = stream.iter().position(|event| {
                    matches!(
                        event,
                        AgentEvent::Compaction {
                            phase: CompactionPhase::Started,
                            ..
                        }
                    )
                });
                if started.is_some_and(|started| started > completed) {
                    ctx.fail("`compaction started` comes after `compaction completed`");
                }
            },
        }
    } else {
        // Leave a late signal the time to show up before judging its absence.
        tokio::time::sleep(QUIET_WINDOW).await;
    }
    out_of_band.check(ctx);
    finish(ctx, &*session).await;
}

fn is_hooks_notice(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::ProviderNotice { kind, .. } if kind == "hooks_not_supported")
}

async fn hors_tour(ctx: &mut Ctx, prepared: &Prepared) {
    let mut spec = prepared.spec.clone();
    spec.hooks = Some(Arc::new(CountingHooks::default()));
    let Some(session) = open_with(ctx, prepared, spec).await else {
        return;
    };
    let Some(out_of_band) = OutOfBand::take(ctx, &*session) else {
        return;
    };
    let staged = out_of_band
        .wait_for(0, STEP_TIMEOUT, |event| !is_hooks_notice(event))
        .await;
    if staged.is_none() {
        ctx.fail("no event arrived on out_of_band() although no turn is running");
    }
    let first = out_of_band.snapshot().into_iter().next();
    let notified = first.as_ref().is_some_and(is_hooks_notice);
    if ctx.caps.hooks == HookSupport::InProtocol {
        if out_of_band.snapshot().iter().any(is_hooks_notice) {
            ctx.fail("`provider_notice { hooks_not_supported }` emitted although `hooks` is `in_protocol`");
        }
    } else if !notified {
        ctx.fail(format!(
            "`hooks` is {:?} and the spec carries hooks: the first out-of-band event must be `provider_notice {{ kind: \"hooks_not_supported\" }}`, got {}",
            ctx.caps.hooks,
            first.map_or("nothing".to_owned(), |event| format!("`{}`", event.type_name()))
        ));
    }
    plain_turn(ctx, &*session, "the turn after the out-of-band event").await;
    let events = out_of_band.check(ctx);
    if events
        .iter()
        .any(|event| matches!(event, AgentEvent::Done { .. }))
    {
        ctx.fail("a `done` travelled out of band although the turn stream was being read");
    }
    finish(ctx, &*session).await;
}

async fn permission_hors_tour(ctx: &mut Ctx, prepared: &Prepared) {
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    let Some(out_of_band) = OutOfBand::take(ctx, &*session) else {
        return;
    };
    if !ctx.caps.interactive_permissions {
        let refused = ctx
            .bounded(
                "answer_permission()",
                session.answer_permission(
                    "conformance-unknown-request",
                    PermissionDecision::allow_once(),
                ),
            )
            .await;
        ctx.expect_unsupported(
            "answer_permission() outside a turn",
            &["interactive_permissions"],
            refused,
        );
        tokio::time::sleep(QUIET_WINDOW).await;
        // No out-of-band `permission_ask`: checked by the invariants.
        out_of_band.check(ctx);
        finish(ctx, &*session).await;
        return;
    }
    let asked = out_of_band
        .wait_for(0, STEP_TIMEOUT, |event| {
            matches!(event, AgentEvent::PermissionAsk { .. })
        })
        .await;
    match asked {
        Some(AgentEvent::PermissionAsk {
            request_id, scopes, ..
        }) => {
            let decision = PermissionDecision::Allow {
                scope: pick_scope(&scopes),
                updated_input: None,
            };
            let answered = ctx
                .bounded(
                    "answer_permission()",
                    session.answer_permission(&request_id, decision.clone()),
                )
                .await;
            ctx.expect_ok("answer_permission() outside a turn", answered);
            let again = ctx
                .bounded(
                    "answer_permission()",
                    session.answer_permission(&request_id, decision),
                )
                .await;
            ctx.expect_kind(
                "answer_permission() on a request already answered",
                "invalid_request",
                again,
            );
        },
        _ => ctx.fail("no `permission_ask` arrived on out_of_band() although no turn is running"),
    }
    out_of_band.check(ctx);
    finish(ctx, &*session).await;
}

async fn tour_concurrent(ctx: &mut Ctx, prepared: &Prepared) {
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    let input = ctx.input();
    let events = turn(ctx, &*session, input, Plan::Concurrent)
        .await
        .unwrap_or_default();
    expect_stop(ctx, &events, StopReason::Interrupted, "the first turn");
    // The first turn emitted its terminal event: the session takes a new turn.
    plain_turn(ctx, &*session, "the turn sent after the first one ended").await;
    finish(ctx, &*session).await;
}

async fn erreur_retryable(ctx: &mut Ctx, prepared: &Prepared) {
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    let input = ctx.input();
    let events = turn(ctx, &*session, input, Plan::Passive)
        .await
        .unwrap_or_default();
    match terminal(&events) {
        // Either form is a typed, retryable failure: a terminal `error`, or a
        // `done` in error that carries its classification (and keeps usage and cost).
        Some(AgentEvent::Error { error })
        | Some(AgentEvent::Done {
            is_error: true,
            error: Some(error),
            ..
        }) if error.retryable() => {},
        Some(AgentEvent::Error { error })
        | Some(AgentEvent::Done {
            error: Some(error), ..
        }) => ctx.fail(format!(
            "the failing turn ended with `{}`, which is not retryable",
            error.kind()
        )),
        Some(_) => ctx.fail(
            "the failing turn ended with a `done` carrying no classified `error`, expected a retryable failure",
        ),
        None => {},
    }
    plain_turn(ctx, &*session, "the turn after the retryable error").await;
    finish(ctx, &*session).await;
}

async fn fin_usage_cout(ctx: &mut Ctx, prepared: &Prepared) {
    if let Some(window) = ctx.caps.context_window
        && window.value == 0
    {
        ctx.fail("`context_window` is declared with a value of 0: an unknown window is `None`");
    }
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    let events = plain_turn(ctx, &*session, "the turn reporting usage and cost").await;
    if let Some(AgentEvent::Done { usage, cost, .. }) = terminal(&events) {
        if usage.input_tokens.is_none() || usage.output_tokens.is_none() {
            ctx.fail(format!(
                "`done.usage` lacks token counts (input: {:?}, output: {:?})",
                usage.input_tokens, usage.output_tokens
            ));
        }
        let declared = ctx.caps.cost;
        // `cost: unknown` is checked on every `done` by the invariants.
        if declared != CostBasis::Unknown {
            if cost.basis != declared && cost.basis != CostBasis::Subscription {
                ctx.fail(format!(
                    "`done.cost.basis` is {:?}, the declared basis is {declared:?}",
                    cost.basis
                ));
            }
            match cost.basis {
                CostBasis::Reported | CostBasis::Priced if cost.usd.is_none() => ctx.fail(format!(
                    "`done.cost.usd` is `None` with basis {:?}: a declared cost must carry an amount",
                    cost.basis
                )),
                CostBasis::Free if cost.usd != Some(0.0) => ctx.fail(format!(
                    "`done.cost.usd` is {:?} with basis `free`, expected 0",
                    cost.usd
                )),
                _ => {},
            }
        }
    }
    finish(ctx, &*session).await;
}

async fn processus_mort(ctx: &mut Ctx, prepared: &Prepared) {
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    let input = ctx.input();
    let events = turn(ctx, &*session, input, Plan::Passive)
        .await
        .unwrap_or_default();
    match terminal(&events) {
        Some(AgentEvent::Error { error }) if !error.retryable() => {},
        Some(AgentEvent::Error { error }) => ctx.fail(format!(
            "the dying turn ended with `{}`, which is retryable: a dead provider cannot be retried unchanged",
            error.kind()
        )),
        Some(_) => ctx.fail("the dying turn ended with `done`, expected a terminal `error`"),
        None => {},
    }
    let input = ctx.input();
    let after = ctx.bounded("send_turn()", session.send_turn(input)).await;
    if let Some(Ok(_)) = after {
        ctx.fail("send_turn() succeeded on a session whose provider is dead");
    }
    finish(ctx, &*session).await;
}

async fn fermeture_idempotente(ctx: &mut Ctx, prepared: &Prepared) {
    let Some(session) = open(ctx, prepared).await else {
        return;
    };
    let input = ctx.input();
    let events = turn(ctx, &*session, input, Plan::Close)
        .await
        .unwrap_or_default();
    match terminal(&events) {
        Some(AgentEvent::Error {
            error: ProviderError::Closed,
        }) => {},
        Some(other) => ctx.fail(format!(
            "the turn running during close() ended with {other:?}, expected `error {{ closed }}`"
        )),
        None => {},
    }
    let again = ctx.bounded("close()", session.close()).await;
    ctx.expect_ok("a second close()", again);

    let input = ctx.input();
    let result = ctx.bounded("send_turn()", session.send_turn(input)).await;
    ctx.expect_kind("send_turn() after close()", "closed", result);
    let result = ctx
        .bounded("interrupt()", session.interrupt(InterruptScope::TurnOnly))
        .await;
    ctx.expect_kind("interrupt() after close()", "closed", result);
    let result = ctx
        .bounded("cancel_tools()", session.cancel_tools(CancelScope::All))
        .await;
    ctx.expect_kind("cancel_tools() after close()", "closed", result);
    let result = ctx
        .bounded(
            "answer_permission()",
            session.answer_permission("conformance-unknown-request", PermissionDecision::deny()),
        )
        .await;
    ctx.expect_kind("answer_permission() after close()", "closed", result);
    let result = ctx
        .bounded(
            "answer_question()",
            session.answer_question("conformance-unknown-question", QuestionAnswer::Cancelled),
        )
        .await;
    ctx.expect_kind("answer_question() after close()", "closed", result);
    let result = ctx
        .bounded("set_model()", session.set_model("conformance-model"))
        .await;
    ctx.expect_kind("set_model() after close()", "closed", result);
    let result = ctx
        .bounded(
            "set_policy_mode()",
            session.set_policy_mode(PolicyMode::Ask, None),
        )
        .await;
    ctx.expect_kind("set_policy_mode() after close()", "closed", result);
    finish(ctx, &*session).await;
}

// ---------------------------------------------------------------------------
// Scripted target
// ---------------------------------------------------------------------------

/// The suite's target for [`ScriptedProvider`]: one script per scenario, written
/// for a given set of capabilities.
///
/// [`ScriptedTarget::with_script`] replaces the script of a scenario, which is
/// how a faulty provider is staged for a negative control.
#[derive(Debug, Clone)]
pub struct ScriptedTarget {
    capabilities: Capabilities,
    overrides: Vec<(Scenario, Script)>,
}

/// A [`ConformanceTarget`] preparing, for each scenario, a [`ScriptedProvider`]
/// whose script plays it under `capabilities`.
pub fn scripted_target(capabilities: Capabilities) -> ScriptedTarget {
    ScriptedTarget {
        capabilities,
        overrides: Vec::new(),
    }
}

impl ScriptedTarget {
    /// Replaces the script played for `scenario`.
    pub fn with_script(mut self, scenario: Scenario, script: Script) -> Self {
        self.overrides.retain(|(staged, _)| *staged != scenario);
        self.overrides.push((scenario, script));
        self
    }

    /// The script played for `scenario`: the override if there is one, else the
    /// suite's own.
    pub fn script_for(&self, scenario: Scenario) -> Script {
        self.overrides
            .iter()
            .find(|(staged, _)| *staged == scenario)
            .map(|(_, script)| script.clone())
            .unwrap_or_else(|| scenario_script(&self.capabilities, scenario))
    }
}

/// Model the scripted target opens its sessions with.
const SCRIPTED_MODEL: &str = "scripted-model-a";

#[async_trait]
impl ConformanceTarget for ScriptedTarget {
    fn name(&self) -> &str {
        "scripted"
    }

    fn provider(&self) -> Arc<dyn AgentProvider> {
        Arc::new(ScriptedProvider::new(
            "scripted",
            Script::new(self.capabilities.clone()),
        ))
    }

    async fn prepare(&self, scenario: Scenario) -> Option<Prepared> {
        let script = self.script_for(scenario);
        let resume = script.resume_token.clone();
        let provider = Arc::new(ScriptedProvider::new("scripted", script));
        let mut spec = SessionSpec::new("/conformance/scripted");
        spec.model = Some(SCRIPTED_MODEL.to_owned());
        let mut prepared = Prepared::new(provider, spec);
        prepared.resume = resume;
        Some(prepared)
    }
}

fn busy_turn() -> Vec<Step> {
    vec![steps::text("working"), Step::AwaitInterrupt]
}

/// The script the scripted target plays for a scenario. When the capability the
/// scenario depends on is absent, the script is empty: the default turn of the
/// provider is what the fallback is verified on.
fn scenario_script(caps: &Capabilities, scenario: Scenario) -> Script {
    let mut script = Script::new(caps.clone());
    let done = || steps::done(caps);
    let answer = || steps::text("done");
    match scenario {
        Scenario::TourTexteSimple | Scenario::ChangementPolitique | Scenario::MessageImages => {},
        Scenario::FinUsageCout => script.turns.push(vec![answer(), done()]),
        Scenario::FluxDeltas => script.turns.push(vec![
            steps::delta("Hel"),
            steps::delta("lo"),
            steps::text("Hello"),
            done(),
        ]),
        Scenario::Raisonnement if caps.thinking => {
            script
                .turns
                .push(vec![steps::thinking("let me think"), answer(), done()]);
        },
        Scenario::AppelOutilResultat if caps.tools => script.turns.push(vec![
            steps::tool_call_start("tool-1", "Read"),
            steps::tool_call("tool-1", "Read", json!({ "file_path": "README.md" })),
            steps::tool_result("tool-1", "contents"),
            answer(),
            done(),
        ]),
        Scenario::OutilsParalleles if caps.tools => script.turns.push(vec![
            steps::tool_call("tool-1", "Read", json!({ "file_path": "a" })),
            steps::tool_call("tool-2", "Read", json!({ "file_path": "b" })),
            steps::tool_result("tool-2", "b"),
            steps::tool_result("tool-1", "a"),
            answer(),
            done(),
        ]),
        Scenario::PermissionAccordee | Scenario::PermissionRefusee
            if caps.interactive_permissions =>
        {
            let call = caps.tools.then_some("tool-1");
            let mut turn = Vec::new();
            if let Some(call) = call {
                turn.push(steps::tool_call(call, "Bash", json!({ "command": "ls" })));
            }
            turn.push(steps::permission_ask(
                "request-1",
                "Bash",
                call,
                &caps.permission_scopes,
            ));
            turn.push(steps::await_permission(
                "request-1",
                call.map(|call| steps::tool_result(call, "listing"))
                    .into_iter()
                    .collect(),
                call.map(|call| steps::tool_error(call, "permission denied"))
                    .into_iter()
                    .collect(),
            ));
            turn.extend([answer(), done()]);
            script.turns.push(turn);
        },
        Scenario::QuestionUtilisateur if caps.native_question => script.turns.push(vec![
            steps::question("question-1", "Which branch?"),
            Step::AwaitQuestion {
                question_id: "question-1".to_owned(),
            },
            answer(),
            done(),
        ]),
        Scenario::InterruptionEnFlux | Scenario::FermetureIdempotente => {
            script.turns.push(busy_turn());
        },
        Scenario::InterruptionOutil if caps.tools => script.turns.push(vec![
            steps::tool_call("tool-1", "Bash", json!({ "command": "sleep 600" })),
            Step::AwaitInterrupt,
        ]),
        Scenario::AnnulationTourPreserve if caps.tool_cancel && caps.tools => {
            script.turns.push(vec![
                steps::tool_call("tool-1", "Bash", json!({ "command": "sleep 600" })),
                Step::AwaitCancel,
                steps::text("the tool was cancelled, going on"),
                done(),
            ]);
        },
        Scenario::AnnulationTache if caps.background_tasks && caps.tool_cancel => {
            let call = caps.tools.then_some("tool-1");
            let mut turn = Vec::new();
            if let Some(call) = call {
                turn.push(steps::tool_call(
                    call,
                    "Bash",
                    json!({ "command": "serve &" }),
                ));
                turn.push(steps::tool_result(call, "started in the background"));
            }
            turn.push(Step::Emit(AgentEvent::BackgroundTasks {
                tasks: vec![BackgroundTask {
                    id: "task-1".to_owned(),
                    kind: BackgroundTaskKind::Shell,
                    description: "serve".to_owned(),
                    status: BackgroundTaskStatus::Running,
                    started_at_ms: None,
                    tool_call_id: call.map(str::to_owned),
                    parent: None,
                    pid: None,
                }],
            }));
            turn.extend([answer(), done()]);
            script.turns.push(turn);
        },
        Scenario::ChangementModele => {
            let mut default = ModelInfo::new(SCRIPTED_MODEL);
            default.is_default = true;
            script.models = vec![default, ModelInfo::new("scripted-model-b")];
        },
        Scenario::Reprise if caps.resume => {
            script.resume_token = Some(ResumeToken::new(
                ProviderKind::Scripted,
                1,
                json!({ "session_id": "scripted-resumed" }),
            ));
        },
        Scenario::SousAgent if caps.subagents != SubagentSupport::None => {
            let child = |text: &str, parent: &str| {
                Step::Emit(AgentEvent::Text {
                    text: text.to_owned(),
                    seq: None,
                    parent: Some(parent.to_owned()),
                })
            };
            let mut turn = Vec::new();
            if caps.subagents == SubagentSupport::Nested && caps.tools {
                turn.push(Step::Emit(AgentEvent::ToolCall {
                    id: "tool-agent".to_owned(),
                    name: "Task".to_owned(),
                    input: json!({ "prompt": "explore" }),
                    category: ToolCategory::Agent,
                    canonical: None,
                    input_complete: true,
                    seq: None,
                    parent: None,
                }));
                turn.push(child("exploring", "tool-agent"));
                turn.push(steps::tool_result("tool-agent", "explored"));
            } else {
                turn.push(child("exploring", "thread-1"));
            }
            turn.extend([answer(), done()]);
            script.turns.push(turn);
        },
        Scenario::Compaction if caps.compaction_signal => script.turns.push(vec![
            Step::Emit(AgentEvent::Compaction {
                phase: CompactionPhase::Started,
                trigger: Some(CompactionTrigger::Auto),
                pre_tokens: Some(180_000),
            }),
            Step::Emit(AgentEvent::Compaction {
                phase: CompactionPhase::Completed,
                trigger: Some(CompactionTrigger::Auto),
                pre_tokens: Some(180_000),
            }),
            answer(),
            done(),
        ]),
        Scenario::HorsTour => {
            script.out_of_band = vec![steps::notice("status", json!({ "status": "idle" }))];
        },
        Scenario::PermissionHorsTour if caps.interactive_permissions => {
            script.out_of_band = vec![
                steps::permission_ask("request-1", "Bash", None, &caps.permission_scopes),
                steps::await_permission(
                    "request-1",
                    vec![steps::notice(
                        "permission_resolved",
                        json!({ "allowed": true }),
                    )],
                    vec![steps::notice(
                        "permission_resolved",
                        json!({ "allowed": false }),
                    )],
                ),
            ];
        },
        Scenario::TourConcurrent => {
            script.turns.push(busy_turn());
            script.turns.push(vec![answer(), done()]);
        },
        Scenario::ErreurRetryable => {
            script
                .turns
                .push(vec![Step::Fail(ProviderError::Overloaded)]);
            script.turns.push(vec![answer(), done()]);
        },
        Scenario::ProcessusMort => script.turns.push(vec![
            steps::text("about to die"),
            Step::Fail(ProviderError::ProcessExited { code: Some(1) }),
        ]),
        // The capability is absent: the default turn is what the fallback runs on.
        Scenario::Raisonnement
        | Scenario::AppelOutilResultat
        | Scenario::OutilsParalleles
        | Scenario::PermissionAccordee
        | Scenario::PermissionRefusee
        | Scenario::QuestionUtilisateur
        | Scenario::InterruptionOutil
        | Scenario::AnnulationTourPreserve
        | Scenario::AnnulationTache
        | Scenario::Reprise
        | Scenario::SousAgent
        | Scenario::Compaction
        | Scenario::PermissionHorsTour => {},
    }
    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::full_capabilities;
    use crate::testkit::scripted::done_event;

    #[test]
    fn every_capability_field_has_a_scenario() {
        for field in Capabilities::FIELDS {
            let scenarios: Vec<Scenario> = Scenario::ALL
                .into_iter()
                .filter(|scenario| scenario.covers().contains(&field))
                .collect();
            assert!(
                !scenarios.is_empty(),
                "capability `{field}` is covered by no conformance scenario: add it to `Scenario::covers` with the check of its fallback"
            );
            // Also proves `capability_present` knows the field (it panics otherwise).
            assert!(!capability_present(&Capabilities::none(), field));
            assert!(capability_present(&full_capabilities(), field));
        }
    }

    #[test]
    fn scenarios_only_name_real_capability_fields() {
        for scenario in Scenario::ALL {
            for field in scenario.covers() {
                assert!(Capabilities::FIELDS.contains(field), "{scenario}: {field}");
            }
            if let Some(field) = scenario.capability() {
                assert!(
                    scenario.covers().contains(&field),
                    "{scenario}: its capability `{field}` is not in `covers()`"
                );
            }
        }
    }

    #[test]
    fn scenario_names_are_unique_snake_case() {
        let mut names = HashSet::new();
        for scenario in Scenario::ALL {
            let name = scenario.name();
            assert!(names.insert(name), "duplicate name {name}");
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{name}"
            );
            assert_eq!(scenario.prompt(), format!("conformance:{name}"));
        }
        assert_eq!(names.len(), Scenario::ALL.len());
    }

    fn text(text: &str) -> AgentEvent {
        AgentEvent::Text {
            text: text.into(),
            seq: None,
            parent: None,
        }
    }

    #[test]
    fn invariants_accept_a_sound_turn_and_name_each_violation() {
        let caps = full_capabilities();
        let done = done_event(&caps, StopReason::Completed, None);
        let check = |events: &[AgentEvent]| {
            check_stream_invariants(events, StreamKind::Turn, &caps, &mut HashSet::new())
        };
        assert_eq!(check(&[text("hi"), done.clone()]), Vec::<String>::new());

        let no_terminal = check(&[text("hi")]);
        assert!(
            no_terminal[0].contains("without a terminal event"),
            "{no_terminal:?}"
        );

        let after = check(&[done.clone(), text("late")]);
        assert!(after[0].contains("after the terminal event"), "{after:?}");

        let two = check(&[done.clone(), done.clone()]);
        assert!(two[0].contains("2 terminal events"), "{two:?}");

        let Step::Emit(orphan) = steps::tool_result("nope", "x") else {
            unreachable!()
        };
        let orphaned = check(&[orphan, done.clone()]);
        assert!(orphaned[0].contains("orphan `tool_result`"), "{orphaned:?}");

        let Step::Emit(start) = steps::tool_call_start("t1", "Read") else {
            unreachable!()
        };
        let never_completed = check(&[start, done.clone()]);
        assert!(
            never_completed[0].contains("never completed"),
            "{never_completed:?}"
        );

        let Step::Emit(delta) = steps::delta("Hel") else {
            unreachable!()
        };
        let lost = check(&[text("before"), delta, done.clone()]);
        assert!(
            lost[0].contains("not followed by the complete `text`"),
            "{lost:?}"
        );
    }

    #[test]
    fn invariants_refuse_events_of_an_absent_capability() {
        let caps = Capabilities::none();
        let done = done_event(&caps, StopReason::Completed, None);
        let Step::Emit(thinking) = steps::thinking("hm") else {
            unreachable!()
        };
        let priced = done_event(&full_capabilities(), StopReason::Completed, None);
        let child = AgentEvent::Text {
            text: "child".into(),
            seq: None,
            parent: Some("thread".into()),
        };
        for (events, expected) in [
            (vec![thinking, done.clone()], "`thinking` is false"),
            (vec![child, done.clone()], "`subagents` is `none`"),
            (vec![priced], "`cost` is `unknown`"),
        ] {
            let problems =
                check_stream_invariants(&events, StreamKind::Turn, &caps, &mut HashSet::new());
            assert!(
                problems.iter().any(|problem| problem.contains(expected)),
                "{expected}: {problems:?}"
            );
        }
    }
}
