//! [`ScriptedProvider`]: an in-memory provider that plays a [`Script`].
//!
//! The provider has no process and no network. What it emits is what the script
//! says; what it **enforces** is the contract: one turn at a time, one destination
//! per event, an out-of-band stream taken once, an idempotent `close`, and the
//! capability fallbacks of `docs/agent-contract.md` §5 computed from
//! [`Script::capabilities`].
//!
//! A [`Step::Emit`] plays its event verbatim. A script can therefore emit a
//! malformed turn (an orphan `tool_result`, a turn without `done`): this is what
//! the negative controls of the conformance suite rely on. The events the session
//! emits **by itself** (`done interrupted`, `tool_result` of a cancelled tool,
//! `error closed`, the default turn) always follow the contract.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;

use super::full_capabilities;
use super::transcript::Transcript;
use crate::agent::{
    AgentEvent, AgentProvider, AgentSession, BackgroundTask, BackgroundTaskStatus, CancelOutcome,
    CancelScope, Capabilities, CompactionInfo, CompactionPhase, CompactionTrigger, Cost, CostBasis,
    EventStream, HealthStatus, HookSupport, InterruptOutcome, InterruptScope, ModelInfo,
    PermissionDecision, PolicyMode, ProviderError, ProviderHealth, ProviderKind, QuestionAnswer,
    QuestionReply, ResumeToken, SessionHooks, SessionSpec, StopReason, ToolCallInfo, ToolOutput,
    ToolPolicy, ToolResultInfo, TurnInput, Usage,
};

/// Capacity of the out-of-band buffer (contract §9).
const OUT_OF_BAND_CAPACITY: usize = 1024;

/// What a [`ScriptedProvider`] plays.
///
/// Serialisable: a script is a JSON document, so a host can keep its scenarios as
/// fixtures. The `i`-th `send_turn` of a session plays `turns[i]`; once the list
/// is exhausted a default turn is played (a `text` echoing the input, then
/// `done completed`). Every session of the provider starts at `turns[0]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Script {
    /// Capabilities the provider declares and the sessions enforce.
    pub capabilities: Capabilities,
    /// One list of steps per turn, in order.
    #[serde(default)]
    pub turns: Vec<Vec<Step>>,
    /// Steps played on the out-of-band stream as soon as a session opens.
    #[serde(default)]
    pub out_of_band: Vec<Step>,
    /// Resume token of the sessions. `None` with `capabilities.resume`: a default
    /// token naming the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_token: Option<ResumeToken>,
    /// Catalogue answered by `catalog()`.
    #[serde(default)]
    pub models: Vec<ModelInfo>,
    /// Health answered by `health()`. `None`: healthy. An `unavailable` health
    /// also makes `open` and `resume` fail with its error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<ProviderHealth>,
}

/// One step of a scripted turn (or of the out-of-band script).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
// A script is a handful of steps of test data: boxing the event would only make
// `Step::Emit(event)` awkward to write and to match.
#[allow(clippy::large_enum_variant)]
pub enum Step {
    /// Emits the event, verbatim. A terminal event ends the turn: the steps
    /// after it are not played.
    Emit(AgentEvent),
    /// Waits. An interruption or a `close` cuts the wait.
    Sleep {
        /// Duration in milliseconds.
        ms: u64,
    },
    /// Blocks until `answer_permission(request_id, …)`, then plays `on_allow` or
    /// `on_deny` and goes on with the script. A denial with `interrupt: true`
    /// ends the turn with `done interrupted` after `on_deny`.
    ///
    /// The `permission_ask` event itself is a previous [`Step::Emit`].
    AwaitPermission {
        /// Identifier awaited.
        request_id: String,
        /// Steps played when the answer allows.
        #[serde(default)]
        on_allow: Vec<Step>,
        /// Steps played when the answer denies.
        #[serde(default)]
        on_deny: Vec<Step>,
    },
    /// Blocks until `answer_question(question_id, …)`, then goes on.
    AwaitQuestion {
        /// Identifier awaited.
        question_id: String,
    },
    /// Blocks until `interrupt`; the session then ends the turn with
    /// `done { stop_reason: interrupted }`.
    AwaitInterrupt,
    /// Blocks until `cancel_tools(all)`, then **goes on** with the script: the
    /// turn is preserved.
    AwaitCancel,
    /// Emits `error` with this error. Terminal. A `process_exited` error also
    /// kills the session: every later call answers `closed`.
    Fail(ProviderError),
    /// Closes the turn stream without any terminal event. A contract violation,
    /// for negative controls only.
    EndWithoutTerminal,
}

/// Fluent construction of a [`Script`]; see [`Script::builder`].
#[derive(Debug, Clone)]
pub struct ScriptBuilder {
    script: Script,
}

impl ScriptBuilder {
    /// Replaces the capabilities.
    pub fn capabilities(mut self, capabilities: Capabilities) -> Self {
        self.script.capabilities = capabilities;
        self
    }

    /// Adds a turn made of these steps.
    pub fn turn(mut self, steps: Vec<Step>) -> Self {
        self.script.turns.push(steps);
        self
    }

    /// Adds a turn answering `text` then `done completed`.
    pub fn text_turn(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        let done = steps::done_with_text(&self.script.capabilities, &text);
        self.script.turns.push(vec![steps::text(text), done]);
        self
    }

    /// Adds steps to the out-of-band script.
    pub fn out_of_band(mut self, steps: Vec<Step>) -> Self {
        self.script.out_of_band.extend(steps);
        self
    }

    /// Sets the resume token of the sessions.
    pub fn resume_token(mut self, token: ResumeToken) -> Self {
        self.script.resume_token = Some(token);
        self
    }

    /// Adds a model to the catalogue.
    pub fn model(mut self, model: ModelInfo) -> Self {
        self.script.models.push(model);
        self
    }

    /// Sets the health of the provider.
    pub fn health(mut self, health: ProviderHealth) -> Self {
        self.script.health = Some(health);
        self
    }

    /// The script.
    pub fn build(self) -> Script {
        self.script
    }
}

impl Script {
    /// An empty script with these capabilities: every turn is the default turn.
    pub fn new(capabilities: Capabilities) -> Self {
        Self {
            capabilities,
            turns: Vec::new(),
            out_of_band: Vec::new(),
            resume_token: None,
            models: Vec::new(),
            health: None,
        }
    }

    /// A builder starting from [`full_capabilities`]. Set the capabilities first:
    /// [`ScriptBuilder::text_turn`] reads them to fill the cost.
    pub fn builder() -> ScriptBuilder {
        ScriptBuilder {
            script: Self::new(full_capabilities()),
        }
    }

    /// A script of one turn answering `text` then `done completed`, with usage
    /// and cost filled, under [`full_capabilities`].
    pub fn text_turn(text: impl Into<String>) -> Self {
        Self::builder().text_turn(text).build()
    }

    /// Adds a turn made of these steps.
    pub fn with_turn(mut self, steps: Vec<Step>) -> Self {
        self.turns.push(steps);
        self
    }

    /// A script replaying a transcript under [`full_capabilities`].
    ///
    /// Every event becomes a [`Step::Emit`], with three exceptions: a
    /// `permission_ask` is followed by a [`Step::AwaitPermission`] (the rest of
    /// the turn plays once it is answered, whatever the answer), a `question`
    /// whose reply mode is `call` is followed by a [`Step::AwaitQuestion`], and a
    /// terminal `error` becomes a [`Step::Fail`]. A recorded turn without a
    /// terminal event ends with [`Step::EndWithoutTerminal`], so a faulty
    /// recording replays as faulty.
    pub fn from_transcript(transcript: &Transcript) -> Self {
        let mut script = Self::new(full_capabilities());
        script.turns = transcript
            .turns
            .iter()
            .map(|events| {
                let mut steps = replay_steps(events);
                if !events.iter().any(AgentEvent::is_terminal) {
                    steps.push(Step::EndWithoutTerminal);
                }
                steps
            })
            .collect();
        script.out_of_band = replay_steps(&transcript.out_of_band);
        script
    }
}

fn replay_steps(events: &[AgentEvent]) -> Vec<Step> {
    let mut steps = Vec::with_capacity(events.len());
    for event in events {
        match event {
            AgentEvent::PermissionAsk { request_id, .. } => {
                steps.push(Step::Emit(event.clone()));
                steps.push(Step::AwaitPermission {
                    request_id: request_id.clone(),
                    on_allow: Vec::new(),
                    on_deny: Vec::new(),
                });
            },
            AgentEvent::Question {
                question_id,
                reply: QuestionReply::Call,
                ..
            } => {
                steps.push(Step::Emit(event.clone()));
                steps.push(Step::AwaitQuestion {
                    question_id: question_id.clone(),
                });
            },
            AgentEvent::Error { error } => steps.push(Step::Fail(error.clone())),
            other => steps.push(Step::Emit(other.clone())),
        }
    }
    steps
}

/// Shorthands for the steps a script is usually made of.
pub mod steps {
    use super::*;
    use crate::agent::{DeltaKind, PermissionScope, QuestionSpec, ToolCategory};

    /// Emits a complete assistant `text`.
    pub fn text(text: impl Into<String>) -> Step {
        Step::Emit(AgentEvent::Text {
            text: text.into(),
            seq: None,
            parent: None,
        })
    }

    /// Emits a complete `thinking`.
    pub fn thinking(text: impl Into<String>) -> Step {
        Step::Emit(AgentEvent::Thinking {
            text: text.into(),
            signature: None,
            seq: None,
            parent: None,
        })
    }

    /// Emits a text `delta`.
    pub fn delta(text: impl Into<String>) -> Step {
        Step::Emit(AgentEvent::Delta {
            kind: DeltaKind::Text,
            text: text.into(),
            index: Some(0),
            tool_call_id: None,
            parent: None,
        })
    }

    /// Emits a complete `tool_call` of category `other`.
    pub fn tool_call(id: impl Into<String>, name: impl Into<String>, input: Value) -> Step {
        Step::Emit(AgentEvent::ToolCall {
            id: id.into(),
            name: name.into(),
            input,
            category: ToolCategory::Other,
            canonical: None,
            input_complete: true,
            seq: None,
            parent: None,
        })
    }

    /// Emits the start of a `tool_call` (`input_complete: false`, empty input).
    pub fn tool_call_start(id: impl Into<String>, name: impl Into<String>) -> Step {
        Step::Emit(AgentEvent::ToolCall {
            id: id.into(),
            name: name.into(),
            input: json!({}),
            category: ToolCategory::Other,
            canonical: None,
            input_complete: false,
            seq: None,
            parent: None,
        })
    }

    /// Emits a successful `tool_result` with a text output.
    pub fn tool_result(id: impl Into<String>, output: impl Into<String>) -> Step {
        Step::Emit(tool_result_event(id.into(), output.into(), false))
    }

    /// Emits a failed `tool_result` with a text output.
    pub fn tool_error(id: impl Into<String>, output: impl Into<String>) -> Step {
        Step::Emit(tool_result_event(id.into(), output.into(), true))
    }

    /// Emits a `permission_ask`. Follow it with [`await_permission`].
    pub fn permission_ask(
        request_id: impl Into<String>,
        tool_name: impl Into<String>,
        tool_call_id: Option<&str>,
        scopes: &[PermissionScope],
    ) -> Step {
        Step::Emit(AgentEvent::PermissionAsk {
            request_id: request_id.into(),
            tool_name: tool_name.into(),
            input: json!({}),
            category: ToolCategory::Other,
            canonical: None,
            tool_call_id: tool_call_id.map(str::to_owned),
            scopes: scopes.to_vec(),
            parent: None,
        })
    }

    /// Blocks until the permission is answered.
    pub fn await_permission(
        request_id: impl Into<String>,
        on_allow: Vec<Step>,
        on_deny: Vec<Step>,
    ) -> Step {
        Step::AwaitPermission {
            request_id: request_id.into(),
            on_allow,
            on_deny,
        }
    }

    /// Emits a `question` with one question, answered by `answer_question`.
    /// Follow it with [`Step::AwaitQuestion`].
    pub fn question(question_id: impl Into<String>, question: impl Into<String>) -> Step {
        let question = question.into();
        Step::Emit(AgentEvent::Question {
            question_id: question_id.into(),
            tool_call_id: None,
            reply: QuestionReply::Call,
            questions: vec![QuestionSpec {
                question: question.clone(),
                header: None,
                options: Vec::new(),
                multi_select: false,
            }],
            input: json!({ "questions": [{ "question": question }] }),
            parent: None,
        })
    }

    /// Emits a `provider_notice`.
    pub fn notice(kind: impl Into<String>, data: Value) -> Step {
        Step::Emit(AgentEvent::ProviderNotice {
            kind: kind.into(),
            data,
        })
    }

    /// Emits `done completed`, usage filled, cost according to `capabilities.cost`.
    pub fn done(capabilities: &Capabilities) -> Step {
        Step::Emit(done_event(capabilities, StopReason::Completed, None))
    }

    /// Like [`done`], with `result_text`.
    pub fn done_with_text(capabilities: &Capabilities, result_text: &str) -> Step {
        Step::Emit(done_event(
            capabilities,
            StopReason::Completed,
            Some(result_text.to_owned()),
        ))
    }

    /// Waits `ms` milliseconds.
    pub fn sleep(ms: u64) -> Step {
        Step::Sleep { ms }
    }
}

fn tool_result_event(id: String, output: String, is_error: bool) -> AgentEvent {
    AgentEvent::ToolResult {
        id,
        output: Some(ToolOutput::Text(output)),
        is_error,
        seq: None,
        parent: None,
    }
}

/// A `done` event that respects `capabilities.cost`: no amount when the cost is
/// `unknown` or covered by a subscription, zero when free, a small amount
/// otherwise. Usage is filled (12 input tokens, 3 output tokens).
pub fn done_event(
    capabilities: &Capabilities,
    stop_reason: StopReason,
    result_text: Option<String>,
) -> AgentEvent {
    let cost = match capabilities.cost {
        CostBasis::Unknown => Cost::unknown(),
        CostBasis::Free => Cost::free(),
        CostBasis::Subscription => Cost {
            usd: None,
            basis: CostBasis::Subscription,
        },
        basis @ (CostBasis::Reported | CostBasis::Priced) => Cost {
            usd: Some(0.0012),
            basis,
        },
    };
    let subtype = match stop_reason {
        StopReason::Completed => Some("success".to_owned()),
        StopReason::Interrupted => Some("error_during_execution".to_owned()),
        _ => None,
    };
    AgentEvent::Done {
        stop_reason,
        subtype,
        is_error: stop_reason == StopReason::Error,
        result_text,
        usage: Usage {
            input_tokens: Some(12),
            output_tokens: Some(3),
            ..Usage::default()
        },
        cost,
        duration_ms: 12,
        duration_api_ms: None,
        num_turns: 1,
        model: None,
        provider_session_id: None,
        structured_output: None,
        error: None,
    }
}

/// One call received by a [`ScriptedProvider`] or by one of its sessions.
///
/// Every call is recorded when it is **received**, before any check: a call the
/// session refuses is in the journal too.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordedCall {
    /// `AgentProvider::open`.
    Open {
        /// `spec.model`.
        model: Option<String>,
        /// `spec.cwd`.
        cwd: PathBuf,
        /// `spec.policy`.
        policy: ToolPolicy,
    },
    /// `AgentProvider::resume`.
    Resume {
        /// The token handed back.
        token: ResumeToken,
    },
    /// `AgentSession::send_turn`.
    SendTurn(TurnInput),
    /// `AgentSession::answer_permission`.
    AnswerPermission {
        /// Request answered.
        request_id: String,
        /// The decision.
        decision: PermissionDecision,
    },
    /// `AgentSession::answer_question`.
    AnswerQuestion {
        /// Question answered.
        question_id: String,
        /// The answer.
        answer: QuestionAnswer,
    },
    /// `AgentSession::interrupt`.
    Interrupt(InterruptScope),
    /// `AgentSession::cancel_tools`.
    CancelTools(CancelScope),
    /// `AgentSession::set_model`.
    SetModel(String),
    /// `AgentSession::set_policy_mode`.
    SetPolicyMode {
        /// Neutral mode.
        mode: PolicyMode,
        /// Provider mode name, when given.
        native: Option<String>,
    },
    /// `AgentSession::close`.
    Close,
}

type CallLog = Arc<Mutex<Vec<RecordedCall>>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// In-memory provider of kind [`ProviderKind::Scripted`] playing a [`Script`].
///
/// ```
/// use futures::StreamExt;
/// use nexus_claude::agent::{AgentProvider, SessionSpec, TurnInput};
/// use nexus_claude::testkit::{Script, ScriptedProvider};
///
/// # tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
/// let provider = ScriptedProvider::new("fake", Script::text_turn("hello"));
/// let session = provider.open(SessionSpec::new("/work")).await.unwrap();
/// let events: Vec<_> = session.send_turn(TurnInput::text("hi")).await.unwrap().collect().await;
/// assert!(events.last().unwrap().is_terminal());
/// assert_eq!(provider.calls().len(), 2); // open + send_turn
/// # });
/// ```
#[derive(Debug)]
pub struct ScriptedProvider {
    id: String,
    script: Arc<Script>,
    log: CallLog,
    sessions: AtomicU64,
}

impl ScriptedProvider {
    /// A provider instance named `id` playing `script`.
    pub fn new(id: impl Into<String>, script: Script) -> Self {
        Self {
            id: id.into(),
            script: Arc::new(script),
            log: Arc::new(Mutex::new(Vec::new())),
            sessions: AtomicU64::new(0),
        }
    }

    /// Every call received so far by the provider and by all its sessions, in
    /// order of arrival.
    pub fn calls(&self) -> Vec<RecordedCall> {
        lock(&self.log).clone()
    }

    /// Empties the journal of calls.
    pub fn clear_calls(&self) {
        lock(&self.log).clear();
    }

    /// The script played.
    pub fn script(&self) -> &Script {
        &self.script
    }

    fn start(&self, spec: SessionSpec) -> Result<Arc<dyn AgentSession>, ProviderError> {
        let capabilities = &self.script.capabilities;
        if let Some(health) = &self.script.health
            && health.status == HealthStatus::Unavailable
        {
            return Err(health
                .error
                .clone()
                .unwrap_or_else(|| ProviderError::protocol("scripted provider is unavailable")));
        }
        spec.validate()?;
        if !spec.mcp_servers.is_empty() {
            if !capabilities.per_session_mcp {
                return Err(ProviderError::unsupported("per_session_mcp"));
            }
            if !capabilities.tools {
                return Err(ProviderError::ModelNoTools {
                    model: spec.model.clone().unwrap_or_else(|| "default".to_owned()),
                });
            }
        }

        let number = self.sessions.fetch_add(1, Ordering::SeqCst) + 1;
        let session_id = format!("scripted-session-{number}");
        let resume_token = capabilities.resume.then(|| {
            self.script.resume_token.clone().unwrap_or_else(|| {
                ResumeToken::new(
                    ProviderKind::Scripted,
                    1,
                    json!({ "session_id": session_id }),
                )
            })
        });
        let honoured_hooks = match capabilities.hooks {
            HookSupport::InProtocol => spec.hooks.clone(),
            HookSupport::Command | HookSupport::None => None,
        };
        let (changed, _) = watch::channel(0u64);
        let shared = Arc::new(Shared {
            capabilities: capabilities.clone(),
            script: Arc::clone(&self.script),
            log: Arc::clone(&self.log),
            hooks: honoured_hooks,
            model: Mutex::new(spec.model.clone().unwrap_or_else(|| "default".to_owned())),
            resume_token,
            changed,
            state: Mutex::new(State {
                closed: false,
                turn: None,
                buffers: HashMap::new(),
                epoch: 0,
                next_turn: 0,
                out_of_band: VecDeque::new(),
                out_of_band_dropped: 0,
                out_of_band_taken: false,
                permissions: HashMap::new(),
                questions: HashMap::new(),
                cancels_pending: 0,
                tool_calls: HashMap::new(),
                background: Vec::new(),
            }),
        });

        {
            let mut state = shared.lock();
            if spec.hooks.is_some() && shared.hooks.is_none() {
                shared.emit(
                    &mut state,
                    Origin::OutOfBand,
                    AgentEvent::ProviderNotice {
                        kind: "hooks_not_supported".to_owned(),
                        data: Value::Null,
                    },
                );
            }
        }
        // The leading emissions of the out-of-band script are in the buffer before
        // `open` returns; only what blocks runs in a task.
        let script = &self.script.out_of_band;
        let split = script
            .iter()
            .position(|step| !matches!(step, Step::Emit(_)))
            .unwrap_or(script.len());
        {
            let mut state = shared.lock();
            for step in &script[..split] {
                if let Step::Emit(event) = step {
                    shared.emit(&mut state, Origin::OutOfBand, event.clone());
                }
            }
        }
        if split < script.len() {
            let rest = script[split..].to_vec();
            let task = Arc::clone(&shared);
            tokio::spawn(async move {
                task.play(Origin::OutOfBand, &rest).await;
            });
        }
        Ok(Arc::new(ScriptedSession { shared }))
    }
}

#[async_trait]
impl AgentProvider for ScriptedProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Scripted
    }

    async fn health(&self) -> ProviderHealth {
        self.script
            .health
            .clone()
            .unwrap_or_else(|| ProviderHealth::ok(None))
    }

    async fn catalog(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(self.script.models.clone())
    }

    fn capabilities(&self, _model: Option<&str>) -> Capabilities {
        self.script.capabilities.clone()
    }

    async fn open(&self, spec: SessionSpec) -> Result<Arc<dyn AgentSession>, ProviderError> {
        lock(&self.log).push(RecordedCall::Open {
            model: spec.model.clone(),
            cwd: spec.cwd.clone(),
            policy: spec.policy.clone(),
        });
        self.start(spec)
    }

    async fn resume(
        &self,
        spec: SessionSpec,
        token: ResumeToken,
    ) -> Result<Arc<dyn AgentSession>, ProviderError> {
        lock(&self.log).push(RecordedCall::Resume {
            token: token.clone(),
        });
        if !self.script.capabilities.resume {
            return Err(ProviderError::unsupported("resume"));
        }
        token.expect_kind(ProviderKind::Scripted)?;
        self.start(spec)
    }
}

/// Where an emission comes from: the turn of a given epoch, or the out-of-band script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Turn(u64),
    OutOfBand,
}

/// Whether the script goes on after a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

struct ActiveTurn {
    epoch: u64,
    /// Complete tool calls of the turn that have no result yet.
    open_tools: Vec<String>,
}

/// State of a permission request or of a question.
enum Ask<T> {
    Pending,
    Answered(T),
    /// A question answered by a regular turn, not by `answer_question`.
    ByTurn,
}

/// Events of a turn waiting for the consumer of its stream. Lives as long as
/// that stream, which may be longer than the turn.
struct TurnBuffer {
    events: VecDeque<AgentEvent>,
    ended: bool,
}

struct State {
    closed: bool,
    turn: Option<ActiveTurn>,
    /// Buffers of the turn streams still held by a consumer, by epoch.
    buffers: HashMap<u64, TurnBuffer>,
    epoch: u64,
    next_turn: usize,
    out_of_band: VecDeque<AgentEvent>,
    out_of_band_dropped: u64,
    out_of_band_taken: bool,
    permissions: HashMap<String, Ask<PermissionDecision>>,
    questions: HashMap<String, Ask<QuestionAnswer>>,
    cancels_pending: u32,
    tool_calls: HashMap<String, ToolCallInfo>,
    background: Vec<BackgroundTask>,
}

impl State {
    fn is_current(&self, origin: Origin) -> bool {
        if self.closed {
            return false;
        }
        match origin {
            Origin::Turn(epoch) => self.turn.as_ref().is_some_and(|turn| turn.epoch == epoch),
            Origin::OutOfBand => true,
        }
    }

    /// Ends the running turn, if any: its stream closes once drained.
    fn end_turn(&mut self) {
        if let Some(turn) = self.turn.take()
            && let Some(buffer) = self.buffers.get_mut(&turn.epoch)
        {
            buffer.ended = true;
        }
    }

    fn push_out_of_band(&mut self, event: AgentEvent) {
        if self.out_of_band.len() >= OUT_OF_BAND_CAPACITY {
            self.out_of_band.pop_front();
            self.out_of_band_dropped += 1;
        }
        self.out_of_band.push_back(event);
    }

    /// Bookkeeping the session derives from what goes through it.
    fn track(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::ToolCall {
                id,
                name,
                input,
                category,
                canonical,
                input_complete: true,
                ..
            } => {
                self.tool_calls.insert(
                    id.clone(),
                    ToolCallInfo {
                        id: Some(id.clone()),
                        name: name.clone(),
                        canonical: canonical.clone(),
                        category: *category,
                        input: input.clone(),
                    },
                );
                if let Some(turn) = self.turn.as_mut()
                    && !turn.open_tools.contains(id)
                {
                    turn.open_tools.push(id.clone());
                }
            },
            AgentEvent::ToolResult { id, .. } => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.open_tools.retain(|open| open != id);
                }
            },
            AgentEvent::PermissionAsk { request_id, .. } => {
                self.permissions
                    .entry(request_id.clone())
                    .or_insert(Ask::Pending);
            },
            AgentEvent::Question {
                question_id, reply, ..
            } => {
                let ask = match reply {
                    QuestionReply::Call => Ask::Pending,
                    QuestionReply::Turn => Ask::ByTurn,
                };
                self.questions.entry(question_id.clone()).or_insert(ask);
            },
            AgentEvent::BackgroundTasks { tasks } => self.background = tasks.clone(),
            _ => {},
        }
    }
}

struct Shared {
    capabilities: Capabilities,
    script: Arc<Script>,
    log: CallLog,
    /// Hooks, only when the capabilities say they are honoured.
    hooks: Option<Arc<dyn SessionHooks>>,
    /// Model the next turn runs on: the spec's, then what `set_model` and the
    /// `before_turn` directives asked for.
    model: Mutex<String>,
    resume_token: Option<ResumeToken>,
    /// Bumped at every state change; what the waiting steps listen to.
    changed: watch::Sender<u64>,
    state: Mutex<State>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn bump(&self) {
        self.changed.send_modify(|generation| *generation += 1);
    }

    fn record(&self, call: RecordedCall) {
        lock(&self.log).push(call);
    }

    /// Sends an event to its single destination. Answers `false`, and drops the
    /// event, when `origin` is a turn that is over or the session is closed.
    ///
    /// A turn event goes to the turn stream, or to the out-of-band buffer when
    /// the consumer dropped the stream (what it had not read went there too). A terminal turn event ends the turn in
    /// the same critical section: the turn is active until its terminal event is
    /// **emitted**, not until it is read.
    fn emit(&self, state: &mut State, origin: Origin, event: AgentEvent) -> bool {
        if !state.is_current(origin) {
            return false;
        }
        state.track(&event);
        match origin {
            Origin::Turn(epoch) => {
                let terminal = event.is_terminal();
                match state.buffers.get_mut(&epoch) {
                    Some(buffer) => buffer.events.push_back(event),
                    None => state.push_out_of_band(event),
                }
                if terminal {
                    state.end_turn();
                }
            },
            Origin::OutOfBand => state.push_out_of_band(event),
        }
        self.bump();
        true
    }

    /// Waits until `check` answers, re-running it after every state change.
    async fn wait<T>(&self, mut check: impl FnMut(&mut State) -> Option<T>) -> T {
        let mut changes = self.changed.subscribe();
        loop {
            {
                let mut state = self.lock();
                if let Some(value) = check(&mut state) {
                    return value;
                }
            }
            // The sender lives in `self`: `changed` cannot fail while we run.
            if changes.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    async fn run_hooks(&self, event: &AgentEvent) {
        let Some(hooks) = &self.hooks else { return };
        match event {
            AgentEvent::ToolCall {
                id,
                input_complete: true,
                ..
            } => {
                let call = self.lock().tool_calls.get(id).cloned();
                if let Some(call) = call {
                    // The verdict cannot change a script: it is observed, not applied.
                    let _ = hooks.before_tool(&call).await;
                }
            },
            AgentEvent::ToolResult {
                id,
                output,
                is_error,
                ..
            } => {
                let call = self.lock().tool_calls.get(id).cloned();
                if let Some(call) = call {
                    let output = serde_json::to_value(output).unwrap_or(Value::Null);
                    let result = ToolResultInfo {
                        call,
                        output,
                        is_error: *is_error,
                    };
                    let _ = hooks.after_tool(&result).await;
                }
            },
            AgentEvent::Compaction {
                phase: CompactionPhase::Started,
                trigger,
                ..
            } => {
                let info = CompactionInfo {
                    trigger: trigger
                        .unwrap_or(CompactionTrigger::Auto)
                        .as_str()
                        .to_owned(),
                    custom_instructions: None,
                };
                let _ = hooks.before_compaction(&info).await;
            },
            _ => {},
        }
    }

    /// Plays steps. Recursive (the branches of a permission), hence boxed.
    fn play<'a>(&'a self, origin: Origin, steps: &'a [Step]) -> BoxFuture<'a, Flow> {
        Box::pin(async move {
            for step in steps {
                if self.play_step(origin, step).await == Flow::Stop {
                    return Flow::Stop;
                }
            }
            Flow::Continue
        })
    }

    async fn play_step(&self, origin: Origin, step: &Step) -> Flow {
        let in_turn = matches!(origin, Origin::Turn(_));
        match step {
            Step::Emit(event) => {
                let emitted = self.emit(&mut self.lock(), origin, event.clone());
                if !emitted || (in_turn && event.is_terminal()) {
                    return Flow::Stop;
                }
                // After the emission: a `tool_call` is known to the session by then.
                self.run_hooks(event).await;
                Flow::Continue
            },
            Step::Sleep { ms } => {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(*ms)) => Flow::Continue,
                    () = self.wait(|state| (!state.is_current(origin)).then_some(())) => Flow::Stop,
                }
            },
            Step::AwaitPermission {
                request_id,
                on_allow,
                on_deny,
            } => {
                self.lock()
                    .permissions
                    .entry(request_id.clone())
                    .or_insert(Ask::Pending);
                let decision = self
                    .wait(|state| {
                        if !state.is_current(origin) {
                            return Some(None);
                        }
                        match state.permissions.get(request_id) {
                            Some(Ask::Answered(decision)) => Some(Some(decision.clone())),
                            _ => None,
                        }
                    })
                    .await;
                match decision {
                    None => Flow::Stop,
                    Some(PermissionDecision::Allow { .. }) => self.play(origin, on_allow).await,
                    Some(PermissionDecision::Deny { interrupt, .. }) => {
                        if self.play(origin, on_deny).await == Flow::Stop {
                            return Flow::Stop;
                        }
                        if interrupt && in_turn {
                            let done =
                                done_event(&self.capabilities, StopReason::Interrupted, None);
                            self.emit(&mut self.lock(), origin, done);
                            return Flow::Stop;
                        }
                        Flow::Continue
                    },
                }
            },
            Step::AwaitQuestion { question_id } => {
                self.lock()
                    .questions
                    .entry(question_id.clone())
                    .or_insert(Ask::Pending);
                self.wait(|state| {
                    if !state.is_current(origin) {
                        return Some(Flow::Stop);
                    }
                    matches!(state.questions.get(question_id), Some(Ask::Answered(_)))
                        .then_some(Flow::Continue)
                })
                .await
            },
            Step::AwaitInterrupt => {
                // `interrupt` and `close` emit the terminal event themselves.
                self.wait(|state| (!state.is_current(origin)).then_some(()))
                    .await;
                Flow::Stop
            },
            Step::AwaitCancel => {
                self.wait(|state| {
                    if !state.is_current(origin) {
                        return Some(Flow::Stop);
                    }
                    if state.cancels_pending > 0 {
                        state.cancels_pending -= 1;
                        return Some(Flow::Continue);
                    }
                    None
                })
                .await
            },
            Step::Fail(error) => {
                let mut state = self.lock();
                self.emit(
                    &mut state,
                    origin,
                    AgentEvent::Error {
                        error: error.clone(),
                    },
                );
                if matches!(error, ProviderError::ProcessExited { .. }) {
                    state.end_turn();
                    state.closed = true;
                }
                self.bump();
                Flow::Stop
            },
            Step::EndWithoutTerminal => {
                if in_turn {
                    let mut state = self.lock();
                    if state.is_current(origin) {
                        state.end_turn();
                    }
                    self.bump();
                }
                Flow::Stop
            },
        }
    }

    async fn run_turn(&self, epoch: u64, steps: Vec<Step>) {
        let origin = Origin::Turn(epoch);
        if self.play(origin, &steps).await == Flow::Continue {
            // A script that forgot its terminal event still ends its turn.
            let done = done_event(&self.capabilities, StopReason::Completed, None);
            self.emit(&mut self.lock(), origin, done);
        }
    }

    /// Marks the session closed; a running turn receives `error { closed }`.
    fn close(&self) {
        let mut state = self.lock();
        if state.closed {
            return;
        }
        if let Some(epoch) = state.turn.as_ref().map(|turn| turn.epoch) {
            self.emit(
                &mut state,
                Origin::Turn(epoch),
                AgentEvent::Error {
                    error: ProviderError::Closed,
                },
            );
        }
        state.end_turn();
        state.closed = true;
        self.bump();
    }
}

/// Moves what the consumer of a turn stream did not read to the out-of-band
/// buffer when the stream goes away, and detaches the turn from it.
struct TurnStreamGuard {
    shared: Arc<Shared>,
    epoch: u64,
}

impl Drop for TurnStreamGuard {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        if let Some(buffer) = state.buffers.remove(&self.epoch) {
            for event in buffer.events {
                state.push_out_of_band(event);
            }
        }
        self.shared.bump();
    }
}

fn turn_stream(shared: Arc<Shared>, epoch: u64) -> EventStream {
    let guard = TurnStreamGuard { shared, epoch };
    Box::pin(async_stream::stream! {
        loop {
            let next = guard
                .shared
                .wait(|state| {
                    let Some(buffer) = state.buffers.get_mut(&epoch) else {
                        return Some(None);
                    };
                    match buffer.events.pop_front() {
                        Some(event) => Some(Some(event)),
                        None => buffer.ended.then_some(None),
                    }
                })
                .await;
            match next {
                Some(event) => yield event,
                None => break,
            }
        }
    })
}

/// A session of a [`ScriptedProvider`]. Dropping it closes it.
struct ScriptedSession {
    shared: Arc<Shared>,
}

impl Drop for ScriptedSession {
    fn drop(&mut self) {
        // Wakes the tasks still waiting on a step, which would otherwise keep the
        // session state alive for ever.
        self.shared.close();
    }
}

#[async_trait]
impl AgentSession for ScriptedSession {
    fn capabilities(&self) -> &Capabilities {
        &self.shared.capabilities
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        self.shared.resume_token.clone()
    }

    async fn send_turn(&self, input: TurnInput) -> Result<EventStream, ProviderError> {
        let shared = &self.shared;
        shared.record(RecordedCall::SendTurn(input.clone()));
        {
            let state = shared.lock();
            if state.closed {
                return Err(ProviderError::Closed);
            }
            if input.has_images() && !shared.capabilities.images {
                return Err(ProviderError::unsupported("images"));
            }
            if state.turn.is_some() {
                return Err(ProviderError::TurnInProgress);
            }
        }
        // `before_turn` (contract §3): the host may name the model of this turn.
        // Honoured only with `set_model_live`; otherwise said, never silently dropped.
        let mut opening = Vec::new();
        if let Some(hooks) = &shared.hooks {
            let index = shared.lock().next_turn as u32;
            let current = lock(&shared.model).clone();
            let mut ctx = crate::agent::TurnContext::new(index, current.clone());
            ctx.input_chars = input.joined_text().len();
            let directive = hooks.before_turn(&ctx).await;
            if let Some(model) = directive
                .model
                .filter(|m| !m.trim().is_empty() && *m != current)
            {
                if shared.capabilities.set_model_live {
                    *lock(&shared.model) = model.clone();
                    opening.push(AgentEvent::ModelChanged { model });
                } else {
                    opening.push(AgentEvent::ProviderNotice {
                        kind: "model_directive_ignored".to_owned(),
                        data: json!({ "model": model, "capability": "set_model_live" }),
                    });
                }
            }
        }
        let mut state = shared.lock();
        if state.closed {
            return Err(ProviderError::Closed);
        }
        if state.turn.is_some() {
            return Err(ProviderError::TurnInProgress);
        }
        let steps = match shared.script.turns.get(state.next_turn) {
            Some(steps) => steps.clone(),
            None => {
                let echo = format!("echo: {}", input.joined_text());
                vec![
                    steps::text(echo.clone()),
                    steps::done_with_text(&shared.capabilities, &echo),
                ]
            },
        };
        state.next_turn += 1;
        state.epoch += 1;
        state.cancels_pending = 0;
        let epoch = state.epoch;
        state.turn = Some(ActiveTurn {
            epoch,
            open_tools: Vec::new(),
        });
        state.buffers.insert(
            epoch,
            TurnBuffer {
                events: VecDeque::new(),
                ended: false,
            },
        );
        for event in opening {
            shared.emit(&mut state, Origin::Turn(epoch), event);
        }
        drop(state);
        let task = Arc::clone(shared);
        tokio::spawn(async move { task.run_turn(epoch, steps).await });
        Ok(turn_stream(Arc::clone(shared), epoch))
    }

    async fn answer_permission(
        &self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), ProviderError> {
        let shared = &self.shared;
        shared.record(RecordedCall::AnswerPermission {
            request_id: request_id.to_owned(),
            decision: decision.clone(),
        });
        let mut state = shared.lock();
        if state.closed {
            return Err(ProviderError::Closed);
        }
        if !shared.capabilities.interactive_permissions {
            return Err(ProviderError::unsupported("interactive_permissions"));
        }
        match state.permissions.get(request_id) {
            Some(Ask::Pending) => {},
            Some(_) => {
                return Err(ProviderError::invalid(
                    "permission request already answered",
                ));
            },
            None => return Err(ProviderError::invalid("unknown permission request")),
        }
        if let PermissionDecision::Allow { scope, .. } = &decision
            && !shared.capabilities.permission_scopes.contains(scope)
        {
            return Err(ProviderError::unsupported("permission_scope"));
        }
        state
            .permissions
            .insert(request_id.to_owned(), Ask::Answered(decision));
        shared.bump();
        Ok(())
    }

    async fn answer_question(
        &self,
        question_id: &str,
        answer: QuestionAnswer,
    ) -> Result<(), ProviderError> {
        let shared = &self.shared;
        shared.record(RecordedCall::AnswerQuestion {
            question_id: question_id.to_owned(),
            answer: answer.clone(),
        });
        let mut state = shared.lock();
        if state.closed {
            return Err(ProviderError::Closed);
        }
        if !shared.capabilities.native_question {
            return Err(ProviderError::unsupported("native_question"));
        }
        match state.questions.get(question_id) {
            Some(Ask::Pending) => {},
            Some(Ask::ByTurn) => return Err(ProviderError::unsupported("answer_question")),
            Some(Ask::Answered(_)) => {
                return Err(ProviderError::invalid("question already answered"));
            },
            None => return Err(ProviderError::invalid("unknown question")),
        }
        state
            .questions
            .insert(question_id.to_owned(), Ask::Answered(answer));
        shared.bump();
        Ok(())
    }

    async fn interrupt(&self, scope: InterruptScope) -> Result<InterruptOutcome, ProviderError> {
        let shared = &self.shared;
        shared.record(RecordedCall::Interrupt(scope));
        let mut state = shared.lock();
        if state.closed {
            return Err(ProviderError::Closed);
        }
        let Some(turn) = state.turn.as_ref() else {
            return Ok(InterruptOutcome::default());
        };
        let origin = Origin::Turn(turn.epoch);
        let cut = match scope {
            InterruptScope::TurnAndTools => turn.open_tools.clone(),
            InterruptScope::TurnOnly => Vec::new(),
        };
        for id in &cut {
            let result = tool_result_event(id.clone(), "interrupted".to_owned(), true);
            shared.emit(&mut state, origin, result);
        }
        let done = done_event(&shared.capabilities, StopReason::Interrupted, None);
        shared.emit(&mut state, origin, done);
        Ok(InterruptOutcome {
            turn_interrupted: true,
            tools_cancelled: cut.len() as u32,
            diagnostic: None,
        })
    }

    async fn cancel_tools(&self, scope: CancelScope) -> Result<CancelOutcome, ProviderError> {
        let shared = &self.shared;
        shared.record(RecordedCall::CancelTools(scope.clone()));
        let mut state = shared.lock();
        if state.closed {
            return Err(ProviderError::Closed);
        }
        if matches!(scope, CancelScope::Task { .. }) && !shared.capabilities.background_tasks {
            return Err(ProviderError::unsupported("background_tasks"));
        }
        if !shared.capabilities.tool_cancel {
            return Err(ProviderError::unsupported("tool_cancel"));
        }
        let origin = match state.turn.as_ref() {
            Some(turn) => Origin::Turn(turn.epoch),
            None => Origin::OutOfBand,
        };
        match scope {
            CancelScope::All => {
                let Some(turn) = state.turn.as_ref() else {
                    return Ok(CancelOutcome::default());
                };
                let cut = turn.open_tools.clone();
                for id in &cut {
                    let result = tool_result_event(id.clone(), "cancelled".to_owned(), true);
                    shared.emit(&mut state, origin, result);
                }
                state.cancels_pending += 1;
                shared.bump();
                Ok(CancelOutcome {
                    tools_cancelled: cut.len() as u32,
                    diagnostic: None,
                })
            },
            CancelScope::Task { id } => {
                let mut tasks = state.background.clone();
                let Some(task) = tasks
                    .iter_mut()
                    .find(|task| task.id == id && task.status == BackgroundTaskStatus::Running)
                else {
                    return Err(ProviderError::invalid(
                        "unknown or finished background task",
                    ));
                };
                task.status = BackgroundTaskStatus::Killed;
                shared.emit(&mut state, origin, AgentEvent::BackgroundTasks { tasks });
                Ok(CancelOutcome {
                    tools_cancelled: 1,
                    diagnostic: None,
                })
            },
        }
    }

    async fn set_model(&self, model: &str) -> Result<(), ProviderError> {
        let shared = &self.shared;
        shared.record(RecordedCall::SetModel(model.to_owned()));
        let state = shared.lock();
        if state.closed {
            return Err(ProviderError::Closed);
        }
        if !shared.capabilities.set_model_live {
            return Err(ProviderError::unsupported("set_model_live"));
        }
        drop(state);
        *lock(&shared.model) = model.to_owned();
        Ok(())
    }

    async fn set_policy_mode(
        &self,
        mode: PolicyMode,
        native: Option<&str>,
    ) -> Result<(), ProviderError> {
        let shared = &self.shared;
        shared.record(RecordedCall::SetPolicyMode {
            mode,
            native: native.map(str::to_owned),
        });
        let state = shared.lock();
        if state.closed {
            return Err(ProviderError::Closed);
        }
        // What `open` refuses cannot be obtained afterwards.
        Ok(())
    }

    fn out_of_band(&self) -> Option<EventStream> {
        {
            let mut state = self.shared.lock();
            if state.out_of_band_taken {
                return None;
            }
            state.out_of_band_taken = true;
        }
        let shared = Arc::clone(&self.shared);
        Some(Box::pin(async_stream::stream! {
            loop {
                let next = shared
                    .wait(|state| {
                        if state.out_of_band_dropped > 0 {
                            let dropped = std::mem::take(&mut state.out_of_band_dropped);
                            return Some(Some(AgentEvent::ProviderNotice {
                                kind: "lagged".to_owned(),
                                data: json!({ "dropped": dropped }),
                            }));
                        }
                        if let Some(event) = state.out_of_band.pop_front() {
                            return Some(Some(event));
                        }
                        state.closed.then_some(None)
                    })
                    .await;
                match next {
                    Some(event) => yield event,
                    None => break,
                }
            }
        }))
    }

    async fn close(&self) -> Result<(), ProviderError> {
        self.shared.record(RecordedCall::Close);
        self.shared.close();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;
    use crate::agent::{Capabilities, PermissionScope};

    async fn collect(stream: EventStream) -> Vec<AgentEvent> {
        tokio::time::timeout(Duration::from_secs(5), stream.collect())
            .await
            .expect("the turn stream ends")
    }

    #[test]
    fn a_script_round_trips_through_json() {
        let caps = full_capabilities();
        let script = Script::builder()
            .turn(vec![
                steps::tool_call("t1", "Bash", json!({ "command": "ls" })),
                steps::permission_ask("r1", "Bash", Some("t1"), &[PermissionScope::Once]),
                steps::await_permission(
                    "r1",
                    vec![steps::tool_result("t1", "ok")],
                    vec![steps::tool_error("t1", "denied")],
                ),
                Step::Sleep { ms: 1 },
                Step::AwaitQuestion {
                    question_id: "q1".into(),
                },
                Step::AwaitCancel,
                Step::AwaitInterrupt,
                Step::Fail(ProviderError::Closed),
                Step::Fail(ProviderError::RateLimited {
                    retry_after_ms: Some(10),
                }),
                Step::EndWithoutTerminal,
                steps::done(&caps),
            ])
            .out_of_band(vec![steps::notice("status", json!({ "n": 1.5 }))])
            .model(ModelInfo::new("model-a"))
            .build();
        let json = serde_json::to_string_pretty(&script).unwrap();
        assert!(json.contains(r#""step": "await_interrupt""#), "{json}");
        assert!(json.contains(r#""step": "emit""#), "{json}");
        let back: Script = serde_json::from_str(&json).unwrap();
        assert_eq!(back, script);
    }

    #[tokio::test]
    async fn the_default_turn_echoes_and_completes() {
        let provider = ScriptedProvider::new("fake", Script::new(Capabilities::none()));
        let session = provider.open(SessionSpec::new("/work")).await.unwrap();
        let events = collect(session.send_turn(TurnInput::text("ping")).await.unwrap()).await;
        assert!(matches!(&events[0], AgentEvent::Text { text, .. } if text == "echo: ping"));
        assert!(matches!(
            &events[1],
            AgentEvent::Done {
                stop_reason: StopReason::Completed,
                cost: Cost {
                    usd: None,
                    basis: CostBasis::Unknown
                },
                ..
            }
        ));
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn a_dropped_turn_stream_redirects_the_rest_out_of_band() {
        let script = Script::builder()
            .turn(vec![
                steps::text("one"),
                Step::AwaitCancel,
                steps::text("two"),
            ])
            .build();
        let provider = ScriptedProvider::new("fake", script);
        let session = provider.open(SessionSpec::new("/work")).await.unwrap();
        let mut out_of_band = session.out_of_band().unwrap();
        drop(session.send_turn(TurnInput::text("go")).await.unwrap());
        // The turn is still running: dropping the stream does not interrupt it.
        assert_eq!(
            session.send_turn(TurnInput::text("again")).await.err(),
            Some(ProviderError::TurnInProgress)
        );
        session.cancel_tools(CancelScope::All).await.unwrap();
        let mut seen = Vec::new();
        while !seen.iter().any(AgentEvent::is_terminal) {
            let event = tokio::time::timeout(Duration::from_secs(5), out_of_band.next())
                .await
                .expect("an out-of-band event arrives")
                .expect("the out-of-band stream is open");
            seen.push(event);
        }
        assert!(
            seen.iter()
                .any(|event| matches!(event, AgentEvent::Text { text, .. } if text == "two"))
        );
    }

    #[tokio::test]
    async fn an_overflowing_out_of_band_buffer_says_what_it_dropped() {
        let notices = (0..OUT_OF_BAND_CAPACITY + 6)
            .map(|n| steps::notice("status", json!({ "n": n })))
            .collect();
        let script = Script::builder().out_of_band(notices).build();
        let provider = ScriptedProvider::new("fake", script);
        let session = provider.open(SessionSpec::new("/work")).await.unwrap();
        let mut out_of_band = session.out_of_band().unwrap();
        let first = out_of_band.next().await.unwrap();
        assert_eq!(
            first,
            AgentEvent::ProviderNotice {
                kind: "lagged".into(),
                data: json!({ "dropped": 6 }),
            }
        );
        let second = out_of_band.next().await.unwrap();
        assert_eq!(
            second,
            AgentEvent::ProviderNotice {
                kind: "status".into(),
                data: json!({ "n": 6 }),
            }
        );
    }

    #[tokio::test]
    async fn a_denial_with_interrupt_ends_the_turn_interrupted() {
        let script = Script::builder()
            .turn(vec![
                steps::permission_ask("r1", "Bash", None, &[PermissionScope::Once]),
                steps::await_permission("r1", vec![], vec![steps::text("denied")]),
                steps::text("never played"),
            ])
            .build();
        let provider = ScriptedProvider::new("fake", script);
        let session = provider.open(SessionSpec::new("/work")).await.unwrap();
        let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
        // A request can only be answered once it has been asked.
        let asked = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("the request arrives");
        assert!(matches!(asked, Some(AgentEvent::PermissionAsk { .. })));
        session
            .answer_permission(
                "r1",
                PermissionDecision::Deny {
                    message: None,
                    interrupt: true,
                },
            )
            .await
            .unwrap();
        let events = collect(stream).await;
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(
            events.last(),
            Some(AgentEvent::Done {
                stop_reason: StopReason::Interrupted,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_question_answered_by_a_turn_refuses_answer_question() {
        let mut ask = steps::question("q1", "Which one?");
        if let Step::Emit(AgentEvent::Question { reply, .. }) = &mut ask {
            *reply = QuestionReply::Turn;
        }
        let script = Script::builder().turn(vec![ask]).build();
        let provider = ScriptedProvider::new("fake", script);
        let session = provider.open(SessionSpec::new("/work")).await.unwrap();
        collect(session.send_turn(TurnInput::text("go")).await.unwrap()).await;
        assert_eq!(
            session
                .answer_question("q1", QuestionAnswer::Cancelled)
                .await,
            Err(ProviderError::unsupported("answer_question"))
        );
    }
}
