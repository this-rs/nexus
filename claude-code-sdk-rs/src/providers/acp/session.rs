//! The ACP session: one agent process, a pump that turns what it says into events,
//! permissions and the turn lifecycle (contract §2, §9, §10).
//!
//! # Event routing (§9)
//!
//! Every event has exactly one destination. While a turn runs, events go to the
//! turn's stream; a turn stream that was dropped does not stop the turn, and the rest
//! of it goes out of band. Outside a turn they go to the out-of-band buffer (1024
//! events; the oldest are dropped and a `provider_notice { lagged }` says how many).
//! A turn lasts from the acceptance of `send_turn` (the `session/prompt` request) to
//! the emission of its terminal event: `done` for every answer of the agent, `error`
//! only for a dead process (or `closed`).
//!
//! # Permissions
//!
//! Each `session/request_permission` becomes a `permission_ask`, answered with
//! [`AgentSession::answer_permission`] (see [`super::map::answer_for`]). The neutral
//! policy is applied first, locally: a request the session's [`ToolPolicy`] denies is
//! rejected without asking anyone, one it allows is accepted.
//!
//! # Interruption (§10)
//!
//! ACP has `session/cancel` only: `interrupt` answers every pending permission request
//! `cancelled` (the protocol requires it), sends the notification, and the turn ends
//! with `done interrupted` when the agent answers `session/prompt` with `cancelled`.
//! `cancel_tools` is `Unsupported { tool_cancel }`.
//!
//! # Requests of the agent this client does not serve
//!
//! The client announces no `fs` and no `terminal` capability. A `fs/*`, `terminal/*`
//! (or any unknown) request is answered `-32601` (method not found) and noted once per
//! method by `provider_notice { unsupported_agent_request }`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};

use super::AcpConfig;
use super::map::{Ask, MapConfig, MapState, answer_for, auto_answer, mode_id_for};
use super::transport::{Inbound, Process};
use super::wire::{
    CancelParams, ModeInfo, ModeState, PermissionOption, PermissionRequest, PromptParams,
    SessionNotification, SetModeParams, TextBlock,
};
use crate::agent::{
    AgentEvent, AgentSession, CancelOutcome, CancelScope, Capabilities, EventStream,
    InterruptOutcome, InterruptScope, PermissionDecision, PolicyDecision, PolicyMode,
    ProcessDiagnostic, ProviderError, ProviderKind, QuestionAnswer, ResumeToken, SessionLimits,
    StopReason, ToolPolicy, TurnInput,
};

/// Capacity of the out-of-band buffer.
pub(crate) const OUT_OF_BAND_CAPACITY: usize = 1024;

/// How long `session/set_mode` may take to be answered.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// What a provider shares with its sessions.
#[derive(Debug, Default)]
pub struct Shared {
    /// A thought chunk was received by some session.
    pub thinking_seen: AtomicBool,
}

/// Why a turn was stopped from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopCause {
    Interrupted,
    TimedOut(u64),
}

struct PendingAsk {
    rpc_id: Value,
    options: Vec<PermissionOption>,
}

pub(crate) struct State {
    closed: bool,
    dead: Option<ProviderError>,
    turn: Option<mpsc::UnboundedSender<AgentEvent>>,
    prompt_id: Option<i64>,
    turn_seq: u64,
    stop: Option<StopCause>,
    out_of_band: VecDeque<AgentEvent>,
    out_of_band_dropped: u64,
    out_of_band_taken: bool,
    pending: HashMap<String, PendingAsk>,
    policy: ToolPolicy,
    modes: Vec<ModeInfo>,
    noticed: HashSet<String>,
    map: MapState,
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
            self.prompt_id = None;
            self.stop = None;
            self.pending.clear();
        }
        out_of_band
    }
}

/// What a session shares with its pump.
pub(crate) struct Core {
    pub(crate) capabilities: Capabilities,
    process: Arc<Process>,
    session_id: String,
    turn_timeout_ms: Option<u64>,
    ceiling: Option<ToolPolicy>,
    shared: Arc<Shared>,
    state: Mutex<State>,
    wake: Notify,
    ask_seq: AtomicU64,
}

/// Everything `open` / `resume` hand over to build a session.
pub(crate) struct CoreParts {
    pub(crate) capabilities: Capabilities,
    pub(crate) process: Arc<Process>,
    pub(crate) settings: Arc<AcpConfig>,
    pub(crate) session_id: String,
    pub(crate) limits: SessionLimits,
    pub(crate) ceiling: Option<ToolPolicy>,
    pub(crate) policy: ToolPolicy,
    pub(crate) model: Option<String>,
    pub(crate) deltas: bool,
    pub(crate) login_hint: Option<String>,
    pub(crate) mcp_servers: Vec<String>,
    pub(crate) modes: Option<ModeState>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) initial_events: Vec<AgentEvent>,
}

impl Core {
    pub(crate) fn start(parts: CoreParts, inbound: mpsc::UnboundedReceiver<Inbound>) -> Arc<Self> {
        let mut map = MapState::new(MapConfig {
            deltas: parts.deltas,
            cost_basis: parts.capabilities.cost,
            prices: parts.settings.prices.clone(),
            login_hint: parts.login_hint,
            mcp_servers: parts.mcp_servers,
            thinking: parts.capabilities.thinking,
        });
        map.set_model(parts.model);
        let mut state = State {
            closed: false,
            dead: None,
            turn: None,
            prompt_id: None,
            turn_seq: 0,
            stop: None,
            out_of_band: VecDeque::new(),
            out_of_band_dropped: 0,
            out_of_band_taken: false,
            pending: HashMap::new(),
            policy: parts.policy,
            modes: parts
                .modes
                .map(|modes| modes.available_modes)
                .unwrap_or_default(),
            noticed: HashSet::new(),
            map,
        };
        for event in parts.initial_events {
            state.push_out_of_band(event);
        }
        let core = Arc::new(Self {
            capabilities: parts.capabilities,
            process: parts.process,
            session_id: parts.session_id,
            turn_timeout_ms: parts.limits.turn_timeout_ms,
            ceiling: parts.ceiling,
            shared: parts.shared,
            state: Mutex::new(state),
            wake: Notify::new(),
            ask_seq: AtomicU64::new(1),
        });
        tokio::spawn(pump(Arc::clone(&core), inbound));
        core
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, event: AgentEvent) {
        let out_of_band = self.lock().route(event);
        if out_of_band {
            self.wake.notify_waiters();
        }
    }

    fn usable(&self) -> Result<(), ProviderError> {
        let state = self.lock();
        if state.closed {
            return Err(ProviderError::Closed);
        }
        if let Some(dead) = &state.dead {
            return Err(dead.clone());
        }
        Ok(())
    }

    fn is_closed(&self) -> bool {
        self.lock().closed
    }

    fn notice(&self, kind: &str, data: Value) {
        self.emit(AgentEvent::ProviderNotice {
            kind: kind.to_owned(),
            data,
        });
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

    // -- inbound ---------------------------------------------------------------

    fn on_notification(&self, method: &str, params: &Value) {
        if method != "session/update" {
            tracing::debug!(%method, "acp notification ignored");
            return;
        }
        let notification = match SessionNotification::parse(params) {
            Ok(notification) => notification,
            Err(reason) => {
                tracing::warn!(%reason, "acp session/update dropped");
                return;
            },
        };
        if notification
            .session_id
            .as_deref()
            .is_some_and(|id| id != self.session_id)
        {
            return;
        }
        let events = {
            let mut state = self.lock();
            let events = state.map.map_update(notification.update);
            if state.map.thinking_seen() {
                self.shared.thinking_seen.store(true, Ordering::SeqCst);
            }
            events
        };
        for event in events {
            self.emit(event);
        }
    }

    async fn on_request(&self, id: Value, method: &str, params: &Value) {
        if method != "session/request_permission" {
            let _ = self
                .process
                .respond_error(
                    &id,
                    -32601,
                    "method not found: this client serves no fs or terminal",
                )
                .await;
            let first = self.lock().noticed.insert(method.to_owned());
            if first {
                self.notice("unsupported_agent_request", json!({ "method": method }));
            }
            return;
        }
        let request: PermissionRequest = match serde_json::from_value(params.clone()) {
            Ok(request) => request,
            Err(_) => {
                let _ = self
                    .process
                    .respond_error(&id, -32602, "invalid params")
                    .await;
                return;
            },
        };
        let mut ask = self.lock().map.ask_for(&request);
        let decision = {
            let state = self.lock();
            state.policy.decide(&ask.tool, Some(&ask.arg), ask.category)
        };
        let auto = match decision {
            PolicyDecision::Allow => auto_answer(&ask.options, true).map(|answer| (answer, true)),
            PolicyDecision::Deny => auto_answer(&ask.options, false).map(|answer| (answer, false)),
            _ => None,
        };
        match auto {
            Some((answer, allowed)) => {
                let _ = self.process.respond(&id, answer).await;
                self.notice(
                    if allowed {
                        "permission_allowed_by_policy"
                    } else {
                        "permission_denied_by_policy"
                    },
                    json!({ "tool": ask.tool }),
                );
                // The synthesised `tool_call`, if any, still has to be announced.
                for event in ask.events.drain(..) {
                    if !matches!(event, AgentEvent::PermissionAsk { .. }) {
                        self.emit(event);
                    }
                }
            },
            None => self.raise(id, ask),
        }
    }

    /// Registers the request, then emits its events (the request is answerable as
    /// soon as the consumer sees the `permission_ask`).
    fn raise(&self, rpc_id: Value, ask: Ask) {
        let request_id = format!("acp-ask-{}", self.ask_seq.fetch_add(1, Ordering::SeqCst));
        self.lock().pending.insert(
            request_id.clone(),
            PendingAsk {
                rpc_id,
                options: ask.options,
            },
        );
        for mut event in ask.events {
            if let AgentEvent::PermissionAsk {
                request_id: slot, ..
            } = &mut event
            {
                slot.clone_from(&request_id);
            }
            self.emit(event);
        }
    }

    fn on_prompt_response(&self, id: i64, outcome: Result<Value, super::wire::RpcError>) {
        let events = {
            let mut state = self.lock();
            if state.prompt_id != Some(id) {
                return;
            }
            let stop = state.stop;
            let parsed = match outcome {
                Ok(value) => match serde_json::from_value::<super::wire::PromptResult>(value) {
                    Ok(result) => Ok(result),
                    Err(_) => Err(super::wire::RpcError {
                        code: -32603,
                        message: "malformed session/prompt result".to_owned(),
                    }),
                },
                Err(error) => Err(error),
            };
            let mut events = state.map.finish(parsed);
            for event in &mut events {
                if let AgentEvent::Done {
                    stop_reason,
                    is_error,
                    error,
                    provider_session_id,
                    ..
                } = event
                {
                    *provider_session_id = Some(self.session_id.clone());
                    if let Some(StopCause::TimedOut(after_ms)) = stop
                        && *stop_reason == StopReason::Interrupted
                    {
                        *stop_reason = StopReason::Error;
                        *is_error = true;
                        *error = Some(ProviderError::Timeout { after_ms });
                    }
                }
            }
            events
        };
        for event in events {
            self.emit(event);
        }
    }

    fn on_death(&self, code: Option<i32>) {
        let error = ProviderError::ProcessExited { code };
        let was_closed = {
            let mut state = self.lock();
            state.dead.get_or_insert(error.clone());
            state.pending.clear();
            state.closed
        };
        if !was_closed {
            // Terminal for the running turn; out of band (and harmless) otherwise.
            let turn_running = self.lock().turn.is_some();
            if turn_running {
                self.emit(AgentEvent::Error { error });
            }
        }
        self.wake.notify_waiters();
    }

    /// `session/cancel`: every pending permission request is answered `cancelled`
    /// first (the protocol requires it), then the notification goes out.
    async fn request_cancel(&self) -> Result<(), ProviderError> {
        let pending: Vec<PendingAsk> = {
            let mut state = self.lock();
            state.pending.drain().map(|(_, ask)| ask).collect()
        };
        for ask in pending {
            let _ = self
                .process
                .respond(&ask.rpc_id, super::wire::permission_cancelled())
                .await;
        }
        let params = serde_json::to_value(CancelParams {
            session_id: self.session_id.clone(),
        })
        .map_err(|_| ProviderError::protocol("cancel parameters"))?;
        self.process.notify("session/cancel", params).await
    }
}

enum OutOfBandNext {
    Event(Box<AgentEvent>),
    Empty,
    End,
}

async fn pump(core: Arc<Core>, mut inbound: mpsc::UnboundedReceiver<Inbound>) {
    while let Some(message) = inbound.recv().await {
        match message {
            Inbound::Notification { method, params } => core.on_notification(&method, &params),
            Inbound::Request { id, method, params } => core.on_request(id, &method, &params).await,
            Inbound::Response { id, outcome } => core.on_prompt_response(id, outcome),
            Inbound::Malformed(reason) => tracing::warn!(%reason, "acp line ignored"),
            Inbound::Closed { code } => {
                core.on_death(code);
                break;
            },
        }
    }
}

/// A live ACP session.
pub struct AcpSession {
    core: Arc<Core>,
    capabilities: Capabilities,
}

impl std::fmt::Debug for AcpSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpSession")
            .field("session_id", &self.core.session_id)
            .field("pid", &self.core.process.pid())
            .finish_non_exhaustive()
    }
}

impl AcpSession {
    pub(crate) fn new(core: Arc<Core>) -> Self {
        let capabilities = core.capabilities.clone();
        Self { core, capabilities }
    }

    /// Process id of the agent, for diagnostics and tests.
    pub fn pid(&self) -> Option<u32> {
        self.core.process.pid()
    }
}

#[async_trait]
impl AgentSession for AcpSession {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        self.capabilities.resume.then(|| {
            ResumeToken::new(
                ProviderKind::Acp,
                1,
                json!({ "session_id": self.core.session_id }),
            )
        })
    }

    async fn send_turn(&self, input: TurnInput) -> Result<EventStream, ProviderError> {
        self.core.usable()?;
        if input.has_images() {
            return Err(ProviderError::unsupported("images"));
        }
        let text = input.joined_text();
        if text.trim().is_empty() {
            return Err(ProviderError::invalid("a turn needs some text"));
        }
        let params = serde_json::to_value(PromptParams {
            session_id: self.core.session_id.clone(),
            prompt: vec![TextBlock {
                r#type: "text".to_owned(),
                text,
            }],
        })
        .map_err(|_| ProviderError::protocol("prompt parameters"))?;
        let (sender, receiver) = mpsc::unbounded_channel();
        let id = self.core.process.next_id();
        let seq = {
            let mut state = self.core.lock();
            if state.closed {
                return Err(ProviderError::Closed);
            }
            if state.turn.is_some() {
                return Err(ProviderError::TurnInProgress);
            }
            state.turn = Some(sender);
            // Recorded before the request leaves: the answer may beat us back.
            state.prompt_id = Some(id);
            state.stop = None;
            state.turn_seq += 1;
            state.map.begin_turn();
            state.turn_seq
        };
        if let Err(error) = self
            .core
            .process
            .send_ordered(id, "session/prompt", params)
            .await
        {
            // No stream was handed out: the refusal is the answer of `send_turn`.
            let mut state = self.core.lock();
            if state.turn_seq == seq {
                state.turn = None;
                state.prompt_id = None;
            }
            return Err(error);
        }
        if let Some(after_ms) = self.core.turn_timeout_ms {
            let core = Arc::clone(&self.core);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(after_ms)).await;
                let active = {
                    let mut state = core.lock();
                    let active = state.turn_seq == seq && state.turn.is_some();
                    if active {
                        state.stop.get_or_insert(StopCause::TimedOut(after_ms));
                    }
                    active
                };
                if active {
                    let _ = core.request_cancel().await;
                }
            });
        }
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
        let (rpc_id, answer, cancel) = {
            let mut state = self.core.lock();
            let Some(pending) = state.pending.get(request_id) else {
                return Err(ProviderError::invalid(
                    "unknown or already answered permission request",
                ));
            };
            if let PermissionDecision::Allow { scope, .. } = &decision
                && !self.capabilities.permission_scopes.contains(scope)
            {
                return Err(ProviderError::unsupported("permission_scope"));
            }
            let (answer, cancel) = answer_for(&pending.options, &decision)?;
            let rpc_id = pending.rpc_id.clone();
            state.pending.remove(request_id);
            (rpc_id, answer, cancel)
        };
        self.core.process.respond(&rpc_id, answer).await?;
        if cancel {
            // A denial that stops the turn: best effort, the answer is already sent.
            let _ = self.core.request_cancel().await;
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
        let running = {
            let mut state = self.core.lock();
            let running = state.turn.is_some();
            if running {
                state.stop.get_or_insert(StopCause::Interrupted);
            }
            running
        };
        if !running {
            return Ok(InterruptOutcome::default());
        }
        self.core.request_cancel().await?;
        // `turn_only` and `turn_and_tools` stop the same things here: ACP has one
        // cancellation, and no tool of its own to cut.
        Ok(InterruptOutcome {
            turn_interrupted: true,
            tools_cancelled: 0,
            diagnostic: Some(ProcessDiagnostic {
                pid: self.core.process.pid(),
                killed_pids: Vec::new(),
            }),
        })
    }

    async fn cancel_tools(&self, _scope: CancelScope) -> Result<CancelOutcome, ProviderError> {
        if self.core.is_closed() {
            return Err(ProviderError::Closed);
        }
        Err(ProviderError::unsupported("tool_cancel"))
    }

    async fn set_model(&self, model: &str) -> Result<(), ProviderError> {
        self.core.usable()?;
        if model.trim().is_empty() || model.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(ProviderError::invalid("empty or malformed model name"));
        }
        // `session/set_model` is unstable in the protocol: not used.
        Err(ProviderError::unsupported("set_model_live"))
    }

    async fn set_policy_mode(
        &self,
        mode: PolicyMode,
        native: Option<&str>,
    ) -> Result<(), ProviderError> {
        self.core.usable()?;
        if let Some(ceiling) = &self.core.ceiling
            && mode > ceiling.mode
        {
            return Err(ProviderError::unsupported("policy_ceiling"));
        }
        let mode_id = {
            let state = self.core.lock();
            mode_id_for(mode, native, &state.modes)
        };
        let Some(mode_id) = mode_id else {
            // The agent publishes no mode that matches: the neutral policy cannot
            // reach it (`session/set_mode` only changes modes the agent published).
            return Err(ProviderError::unsupported("set_policy_mode"));
        };
        let params = serde_json::to_value(SetModeParams {
            session_id: self.core.session_id.clone(),
            mode_id,
        })
        .map_err(|_| ProviderError::protocol("set_mode parameters"))?;
        self.core
            .process
            .request("session/set_mode", params, REQUEST_TIMEOUT)
            .await?;
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
        let turn = {
            let mut state = self.core.lock();
            if state.closed {
                return Ok(());
            }
            state.closed = true;
            state.pending.clear();
            state.turn.is_some()
        };
        if turn {
            self.core.emit(AgentEvent::Error {
                error: ProviderError::Closed,
            });
        }
        self.core.wake.notify_waiters();
        self.core.process.shutdown().await;
        Ok(())
    }
}

impl Drop for AcpSession {
    fn drop(&mut self) {
        // A session dropped without `close`: the child is `kill_on_drop`, and the
        // group is killed here so nothing it started outlives it.
        let process = Arc::clone(&self.core.process);
        if self.core.is_closed() {
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move { process.shutdown().await });
        }
    }
}
