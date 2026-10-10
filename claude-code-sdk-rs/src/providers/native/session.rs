//! The native session: its shared core, event routing, permissions and
//! cancellation (contract §2, §9, §10). The tool loop that drives a turn is in
//! `loop.rs`.
//!
//! # Event routing (§9)
//!
//! Every event has exactly one destination. While a turn runs, events go to the
//! turn's stream; a turn stream that was dropped does not stop the turn, and the
//! rest of it goes out of band. Outside a turn, events go to the out-of-band
//! buffer (1024 events; the oldest are dropped and a `provider_notice { lagged }`
//! says how many). The turn is *active* from the acceptance of `send_turn` to
//! the emission of its terminal event.
//!
//! # Cancellation (§10)
//!
//! A turn has one `TurnSignal` (interruption, timeout, close: the first cause
//! wins) and each running tool call has its own `CancelToken`. `interrupt`
//! stops the turn and the tools; `cancel_tools(all)` stops the tools only: each
//! gets an error `tool_result` ("cancelled by the user"), the model sees it and
//! the turn carries on to its normal end. Tokens are registered **before** the
//! `tool_call` event is emitted, so a consumer that reacts to that event never
//! races the registration.
//!
//! # Background tasks (§4, §10)
//!
//! A session whose `nexus` server serves `Bash` declares `background_tasks`. What that
//! server reports in the structured content of `Bash` (`run_in_background`), `Monitor`
//! and `TaskStop` results, and the task ends it notifies, keep the table of the
//! session's tasks (`tasks.rs`): every change routes a complete `background_tasks`
//! snapshot, every `Monitor` line a `task_update { progress }`. `cancel_tools(task { id
//! })` calls that server's `TaskStop` (the task's process group is ended); a running
//! turn goes on.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, oneshot};

use super::cancel::CancelToken;
use super::mcp::McpClient;
use super::tasks::{self as background, TaskTable};
use super::tools::{ToolEntry, ToolRegistry};
use super::transcript::TranscriptStore;
use super::{ModelFacts, NativeConfig as Settings, r#loop};
use crate::agent::BackgroundTaskStatus;
use crate::agent::{
    AgentEvent, AgentSession, CancelOutcome, CancelScope, Capabilities, CostBasis, EventStream,
    InputBlock, InterruptOutcome, InterruptScope, PermissionDecision, PermissionScope,
    PolicyDecision, PolicyMode, ProcessDiagnostic, ProviderError, ProviderKind, QuestionAnswer,
    ResumeToken, SessionHooks, SessionLimits, TaskPhase, ToolPolicy, TurnInput,
};
use crate::model::{ChatMessage, ModelEndpoint};

/// Capacity of the out-of-band buffer.
pub(crate) const OUT_OF_BAND_CAPACITY: usize = 1024;

/// Why a turn was stopped from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopCause {
    /// `interrupt`.
    Interrupted,
    /// `limits.turn_timeout_ms` elapsed.
    TimedOut(u64),
    /// `close`.
    Closed,
}

/// Interruption state of the running turn.
pub(crate) struct TurnSignal {
    pub(crate) token: CancelToken,
    cause: Mutex<Option<StopCause>>,
}

impl TurnSignal {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            token: CancelToken::new(),
            cause: Mutex::new(None),
        })
    }

    /// Stops the turn. The first cause stays.
    pub(crate) fn stop(&self, cause: StopCause) {
        self.cause
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(cause);
        self.token.cancel();
    }

    pub(crate) fn cause(&self) -> Option<StopCause> {
        *self.cause.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A permission request waiting for its answer.
pub(crate) struct PendingAsk {
    pub(crate) tool: String,
    pub(crate) answer: oneshot::Sender<PermissionDecision>,
}

pub(crate) struct State {
    pub(crate) closed: bool,
    /// The session cannot go on (an MCP server died): the error `send_turn` answers.
    pub(crate) dead: Option<ProviderError>,
    pub(crate) turn: Option<mpsc::UnboundedSender<AgentEvent>>,
    pub(crate) signal: Option<Arc<TurnSignal>>,
    pub(crate) out_of_band: VecDeque<AgentEvent>,
    out_of_band_dropped: u64,
    out_of_band_taken: bool,
    pub(crate) pending: HashMap<String, PendingAsk>,
    /// Running tool calls, by tool call id.
    pub(crate) inflight: HashMap<String, CancelToken>,
    /// Tools approved for the rest of the session (scope `session`).
    pub(crate) approved: HashSet<String>,
    pub(crate) policy: ToolPolicy,
    pub(crate) model: String,
    /// Window of the model now active: what compaction reads. Starts as the
    /// snapshot's, follows every model change (`set_model`, `before_turn`).
    pub(crate) active_window: Option<u64>,
    /// Cost basis of the model now active: what the cost of a turn reads.
    pub(crate) active_cost: CostBasis,
    /// Vision of the model now active: whether an image may reach it (a turn's
    /// input, a tool's image, the images of the history).
    pub(crate) active_images: bool,
    /// Turns started so far (the index of the next one).
    pub(crate) turn_index: u32,
    /// The committed conversation.
    pub(crate) messages: Vec<ChatMessage>,
    /// Tokens and USD spent by the session so far (budgets).
    pub(crate) tokens_spent: u64,
    pub(crate) usd_spent: f64,
    /// Size of the last prompt the endpoint reported, since the last compaction.
    pub(crate) last_prompt_tokens: Option<u64>,
    /// The background tasks `nexus-tools` runs for the session (§4 `background_tasks`).
    pub(crate) tasks: TaskTable,
}

impl State {
    fn push_out_of_band(&mut self, event: AgentEvent) {
        if self.out_of_band.len() >= OUT_OF_BAND_CAPACITY {
            self.out_of_band.pop_front();
            self.out_of_band_dropped += 1;
        }
        self.out_of_band.push_back(event);
    }

    /// Sends the event to its one destination. Returns whether it went out of band.
    fn route(&mut self, event: AgentEvent) -> bool {
        if self.closed && !event.is_terminal() {
            return false;
        }
        let Some(turn) = &self.turn else {
            self.push_out_of_band(event);
            return true;
        };
        let terminal = event.is_terminal();
        let mut out_of_band = false;
        // A dropped turn stream does not stop the turn: the rest goes out of band.
        if let Err(mpsc::error::SendError(event)) = turn.send(event) {
            self.push_out_of_band(event);
            out_of_band = true;
        }
        if terminal {
            self.turn = None;
            self.signal = None;
        }
        out_of_band
    }
}

/// What a session shares with its turn task.
pub(crate) struct Core {
    pub(crate) capabilities: Capabilities,
    /// Facts per model, shared with the provider: what a model change reads.
    pub(crate) facts: ModelFacts,
    pub(crate) endpoint: Arc<dyn ModelEndpoint>,
    pub(crate) settings: Arc<Settings>,
    pub(crate) registry: ToolRegistry,
    pub(crate) mcp: HashMap<String, McpClient>,
    pub(crate) system_prompt: Option<String>,
    pub(crate) deltas: bool,
    pub(crate) max_turns: Option<u32>,
    pub(crate) limits: SessionLimits,
    pub(crate) ceiling: Option<ToolPolicy>,
    /// The session directory: paths in policy patterns are relative to it.
    pub(crate) cwd: std::path::PathBuf,
    /// Host callbacks around tools and compaction (`SessionSpec::hooks`).
    pub(crate) hooks: Option<Arc<dyn SessionHooks>>,
    pub(crate) transcript_id: String,
    pub(crate) store: Arc<dyn TranscriptStore>,
    pub(crate) state: Mutex<State>,
    pub(crate) wake: Notify,
}

/// Everything `open`/`resume` hand over to build a session.
pub(crate) struct CoreParts {
    pub(crate) capabilities: Capabilities,
    pub(crate) facts: ModelFacts,
    pub(crate) endpoint: Arc<dyn ModelEndpoint>,
    pub(crate) settings: Arc<Settings>,
    pub(crate) registry: ToolRegistry,
    pub(crate) mcp: HashMap<String, McpClient>,
    pub(crate) system_prompt: Option<String>,
    pub(crate) deltas: bool,
    pub(crate) max_turns: Option<u32>,
    pub(crate) limits: SessionLimits,
    pub(crate) ceiling: Option<ToolPolicy>,
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) hooks: Option<Arc<dyn SessionHooks>>,
    pub(crate) transcript_id: String,
    pub(crate) store: Arc<dyn TranscriptStore>,
    pub(crate) policy: ToolPolicy,
    pub(crate) model: String,
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) initial_events: Vec<AgentEvent>,
}

impl Core {
    pub(crate) fn new(parts: CoreParts) -> Arc<Self> {
        let mut state = State {
            closed: false,
            dead: None,
            turn: None,
            signal: None,
            out_of_band: VecDeque::new(),
            out_of_band_dropped: 0,
            out_of_band_taken: false,
            pending: HashMap::new(),
            inflight: HashMap::new(),
            approved: HashSet::new(),
            policy: parts.policy,
            model: parts.model,
            active_window: parts.capabilities.context_window.map(|w| w.value),
            active_cost: parts.capabilities.cost,
            active_images: parts.capabilities.images,
            turn_index: 0,
            messages: parts.messages,
            tokens_spent: 0,
            usd_spent: 0.0,
            last_prompt_tokens: None,
            tasks: TaskTable::default(),
        };
        for event in parts.initial_events {
            state.push_out_of_band(event);
        }
        Arc::new(Self {
            capabilities: parts.capabilities,
            facts: parts.facts,
            endpoint: parts.endpoint,
            settings: parts.settings,
            registry: parts.registry,
            mcp: parts.mcp,
            system_prompt: parts.system_prompt,
            deltas: parts.deltas,
            max_turns: parts.max_turns,
            limits: parts.limits,
            ceiling: parts.ceiling,
            cwd: parts.cwd,
            hooks: parts.hooks,
            transcript_id: parts.transcript_id,
            store: parts.store,
            state: Mutex::new(state),
            wake: Notify::new(),
        })
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Routes one event to its destination.
    pub(crate) fn emit(&self, event: AgentEvent) {
        let out_of_band = self.lock().route(event);
        if out_of_band {
            self.wake.notify_waiters();
        }
    }

    /// `Err` when the session was closed or its provider died.
    pub(crate) fn usable(&self) -> Result<(), ProviderError> {
        let state = self.lock();
        if state.closed {
            return Err(ProviderError::Closed);
        }
        if let Some(dead) = &state.dead {
            return Err(dead.clone());
        }
        Ok(())
    }

    /// Makes `model` the active model: the next turn runs on it, and the window,
    /// cost basis and vision that govern compaction, cost and images become its own. The
    /// capabilities snapshot of the session does not move (A4). Answers whether
    /// the model changed.
    pub(crate) async fn apply_model(&self, model: &str) -> bool {
        if self.lock().model == model {
            return false;
        }
        let facts = self.facts.active(self.endpoint.as_ref(), model).await;
        let mut state = self.lock();
        state.model = model.to_owned();
        state.active_window = facts.window;
        state.active_cost = facts.cost;
        state.active_images = facts.images;
        true
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// The local policy decision for a call (contract §6). A read-only tool
    /// counts as a read, so plan mode lets it through; a tool approved for the
    /// session is not asked about again, unless it is denied.
    ///
    /// A `nexus-tools` tool is judged under both its names and by what the call really does
    /// (the parts of a command line, the normal form of a path): see `policy_args`.
    pub(crate) fn decide(&self, entry: &ToolEntry, input: &Value) -> PolicyDecision {
        let state = self.lock();
        super::policy_args::decide_call(&state.policy, &state.approved, entry, input, &self.cwd)
    }

    /// Registers a tool call as running and returns its token.
    pub(crate) fn begin_tool(&self, call_id: &str) -> CancelToken {
        let token = CancelToken::new();
        self.lock()
            .inflight
            .insert(call_id.to_owned(), token.clone());
        token
    }

    pub(crate) fn end_tool(&self, call_id: &str) {
        self.lock().inflight.remove(call_id);
    }

    fn next_out_of_band(&self) -> OutOfBandNext {
        let mut state = self.lock();
        if state.out_of_band_dropped > 0 {
            let dropped = std::mem::take(&mut state.out_of_band_dropped);
            return OutOfBandNext::Event(Box::new(AgentEvent::ProviderNotice {
                kind: "lagged".to_owned(),
                data: json!({ "dropped": dropped }),
            }));
        }
        match state.out_of_band.pop_front() {
            Some(event) => OutOfBandNext::Event(Box::new(event)),
            None if state.closed => OutOfBandNext::End,
            None => OutOfBandNext::Empty,
        }
    }

    /// Marks the provider dead: nothing but `close` works afterwards.
    pub(crate) fn mark_dead(&self, error: ProviderError) {
        self.lock().dead.get_or_insert(error);
    }

    /// Routes the complete table of background tasks.
    fn route_tasks(&self, mut state: MutexGuard<'_, State>) {
        let tasks = state.tasks.snapshot();
        let out_of_band = state.route(AgentEvent::BackgroundTasks { tasks });
        drop(state);
        if out_of_band {
            self.wake.notify_waiters();
        }
    }

    /// Records what the session's `nexus` server said of a background task in a result
    /// (`structuredContent.background_task` of `Bash`, `Monitor` or `TaskStop`), the task being
    /// started by `tool_call_id` when it is new, and routes the table when it changed. Returns
    /// the report. Nothing without `background_tasks`.
    pub(crate) fn track_task(
        &self,
        structured: &Value,
        tool_call_id: &str,
    ) -> Option<background::Reported> {
        if !self.capabilities.background_tasks {
            return None;
        }
        let reported = background::reported(structured)?;
        let mut state = self.lock();
        if state.tasks.record(reported.clone(), tool_call_id, now_ms()) {
            self.route_tasks(state);
        }
        Some(reported)
    }

    /// A notification the session's `nexus` server sent on its own: the end of a background
    /// task updates the table; a `Monitor` line becomes a `task_update { progress }`.
    pub(crate) fn server_notification(&self, notification: &Value) {
        if !self.capabilities.background_tasks {
            return;
        }
        if let Some((id, status)) = background::ended(notification) {
            let mut state = self.lock();
            if state.tasks.end(&id, status) {
                self.route_tasks(state);
            }
        } else if let Some((id, line)) = background::monitor_line(notification) {
            let tool_call_id = self
                .lock()
                .tasks
                .get(&id)
                .and_then(|task| task.tool_call_id.clone());
            self.emit(AgentEvent::TaskUpdate {
                phase: TaskPhase::Progress,
                task_id: Some(id.clone()),
                tool_call_id,
                description: None,
                status: None,
                summary: Some(line.clone()),
                event_id: None,
                data: json!({ "task_id": id, "line": line }),
            });
        }
    }

    /// Stops a running background task through the `TaskStop` of the session's `nexus` server
    /// (which ends the task's whole process group) and records what it answers.
    async fn stop_task(&self, id: &str) -> Result<CancelOutcome, ProviderError> {
        let client = self
            .mcp
            .get(super::tools::NEXUS_TOOLS_SERVER)
            .ok_or_else(|| ProviderError::protocol("the tools server is not connected"))?;
        let result = client
            .call_tool("TaskStop", json!({ "task_id": id }), std::future::pending())
            .await
            .map_err(super::mcp::McpError::into_provider)?;
        if result.is_error {
            return Err(ProviderError::protocol(format!(
                "TaskStop failed: {}",
                crate::agent::redact(&result.text)
            )));
        }
        let reported = result
            .structured
            .as_ref()
            .and_then(|structured| self.track_task(structured, ""));
        let killed = reported
            .as_ref()
            .is_some_and(|task| task.status == BackgroundTaskStatus::Killed);
        Ok(CancelOutcome {
            tools_cancelled: u32::from(killed),
            diagnostic: killed.then(|| ProcessDiagnostic {
                pid: None,
                killed_pids: reported.and_then(|task| task.pid).into_iter().collect(),
            }),
        })
    }
}

/// Milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

enum OutOfBandNext {
    Event(Box<AgentEvent>),
    Empty,
    End,
}

/// A live native session.
pub struct NativeSession {
    core: Arc<Core>,
    capabilities: Capabilities,
}

impl std::fmt::Debug for NativeSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSession")
            .field("transcript_id", &self.core.transcript_id)
            .finish_non_exhaustive()
    }
}

impl NativeSession {
    pub(crate) fn new(core: Arc<Core>) -> Self {
        let capabilities = core.capabilities.clone();
        Self { core, capabilities }
    }

    fn require_interactive(&self) -> Result<(), ProviderError> {
        if self.capabilities.interactive_permissions {
            Ok(())
        } else {
            Err(ProviderError::unsupported("interactive_permissions"))
        }
    }
}

#[async_trait]
impl AgentSession for NativeSession {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        self.capabilities.resume.then(|| {
            ResumeToken::new(
                ProviderKind::Native,
                1,
                json!({ "transcript_id": self.core.transcript_id }),
            )
        })
    }

    async fn send_turn(&self, input: TurnInput) -> Result<EventStream, ProviderError> {
        self.core.usable()?;
        // The ACTIVE model has no vision (the snapshot's, or the one `set_model`
        // made active): its limit, stated as such.
        if input.has_images() && !self.core.lock().active_images {
            return Err(ProviderError::unsupported("images"));
        }
        for block in &input.blocks {
            if let InputBlock::Image {
                media_type,
                data_base64,
            } = block
                && (!media_type.starts_with("image/") || data_base64.trim().is_empty())
            {
                return Err(ProviderError::invalid(
                    "an image block needs an image/* media type and a base64 payload",
                ));
            }
        }
        let signal = TurnSignal::new();
        let (sender, receiver) = mpsc::unbounded_channel();
        {
            let mut state = self.core.lock();
            if state.closed {
                return Err(ProviderError::Closed);
            }
            if state.turn.is_some() {
                return Err(ProviderError::TurnInProgress);
            }
            state.turn = Some(sender);
            state.signal = Some(Arc::clone(&signal));
        }
        tokio::spawn(r#loop::run_turn(Arc::clone(&self.core), input, signal));
        Ok(Box::pin(
            tokio_stream::wrappers::UnboundedReceiverStream::new(receiver),
        ))
    }

    async fn answer_permission(
        &self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), ProviderError> {
        self.core.usable()?;
        self.require_interactive()?;
        let mut state = self.core.lock();
        if !state.pending.contains_key(request_id) {
            return Err(ProviderError::invalid(
                "unknown or already answered permission request",
            ));
        }
        if let PermissionDecision::Allow { scope, .. } = &decision {
            if !self.capabilities.permission_scopes.contains(scope) {
                return Err(ProviderError::unsupported("permission_scope"));
            }
            if *scope == PermissionScope::Session
                && let Some(pending) = state.pending.get(request_id)
            {
                let tool = pending.tool.clone();
                state.approved.insert(tool);
            }
        }
        if let Some(pending) = state.pending.remove(request_id) {
            let _ = pending.answer.send(decision);
        }
        Ok(())
    }

    async fn answer_question(
        &self,
        _question_id: &str,
        _answer: QuestionAnswer,
    ) -> Result<(), ProviderError> {
        self.core.usable()?;
        Err(ProviderError::unsupported("native_question"))
    }

    async fn interrupt(&self, _scope: InterruptScope) -> Result<InterruptOutcome, ProviderError> {
        if self.core.is_closed() {
            return Err(ProviderError::Closed);
        }
        // Background tasks are not tools of the turn: both scopes spare them (`cancel_tools(task)`
        // stops one), so `turn_only` and `turn_and_tools` stop the same things.
        let (signal, tools) = {
            let state = self.core.lock();
            (state.signal.clone(), state.inflight.len() as u32)
        };
        match signal {
            Some(signal) => {
                signal.stop(StopCause::Interrupted);
                Ok(InterruptOutcome {
                    turn_interrupted: true,
                    tools_cancelled: tools,
                    diagnostic: None,
                })
            },
            None => Ok(InterruptOutcome::default()),
        }
    }

    async fn cancel_tools(&self, scope: CancelScope) -> Result<CancelOutcome, ProviderError> {
        if self.core.is_closed() {
            return Err(ProviderError::Closed);
        }
        match scope {
            CancelScope::All => {
                let tokens: Vec<CancelToken> =
                    self.core.lock().inflight.values().cloned().collect();
                let fresh = tokens.iter().filter(|t| !t.is_cancelled()).count() as u32;
                for token in &tokens {
                    token.cancel();
                }
                Ok(CancelOutcome {
                    tools_cancelled: fresh,
                    diagnostic: None,
                })
            },
            CancelScope::Task { id } => {
                if !self.capabilities.background_tasks {
                    return Err(ProviderError::unsupported("background_tasks"));
                }
                let status = self.core.lock().tasks.get(&id).map(|task| task.status);
                match status {
                    None => Err(ProviderError::invalid(format!(
                        "unknown background task `{id}`"
                    ))),
                    // Already over: nothing to stop, nothing changes.
                    Some(status) if status != BackgroundTaskStatus::Running => {
                        Ok(CancelOutcome::default())
                    },
                    Some(_) => self.core.stop_task(&id).await,
                }
            },
            // `CancelScope` is `#[non_exhaustive]`.
            #[allow(unreachable_patterns)]
            _ => Err(ProviderError::unsupported("cancel_scope")),
        }
    }

    async fn set_model(&self, model: &str) -> Result<(), ProviderError> {
        self.core.usable()?;
        if model.trim().is_empty() {
            return Err(ProviderError::invalid("empty model name"));
        }
        self.core.apply_model(model).await;
        Ok(())
    }

    async fn set_policy_mode(
        &self,
        mode: PolicyMode,
        _native: Option<&str>,
    ) -> Result<(), ProviderError> {
        self.core.usable()?;
        if let Some(ceiling) = &self.core.ceiling
            && mode > ceiling.mode
        {
            return Err(ProviderError::unsupported("policy_ceiling"));
        }
        self.core.lock().policy.mode = mode;
        Ok(())
    }

    fn out_of_band(&self) -> Option<EventStream> {
        {
            let mut state = self.core.lock();
            if state.out_of_band_taken {
                return None;
            }
            state.out_of_band_taken = true;
        }
        let core = Arc::clone(&self.core);
        Some(Box::pin(async_stream::stream! {
            loop {
                let woken = core.wake.notified();
                tokio::pin!(woken);
                // Registered before the buffer is read: an event pushed in
                // between cannot be missed.
                woken.as_mut().enable();
                match core.next_out_of_band() {
                    OutOfBandNext::Event(event) => yield *event,
                    OutOfBandNext::End => break,
                    OutOfBandNext::Empty => woken.await,
                }
            }
        }))
    }

    async fn close(&self) -> Result<(), ProviderError> {
        let (signal, tokens) = {
            let mut state = self.core.lock();
            if state.closed {
                return Ok(());
            }
            state.closed = true;
            state.pending.clear();
            (
                state.signal.clone(),
                state.inflight.values().cloned().collect::<Vec<_>>(),
            )
        };
        if let Some(signal) = signal {
            signal.stop(StopCause::Closed);
        }
        for token in tokens {
            token.cancel();
        }
        self.core.wake.notify_waiters();
        for client in self.core.mcp.values() {
            client.close().await;
        }
        Ok(())
    }
}
