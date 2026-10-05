//! The Codex session: one `codex app-server` process, a pump that turns what it
//! says into events, permissions and the turn lifecycle (contract §2, §9, §10).
//!
//! # Event routing (§9)
//!
//! Every event has exactly one destination. While a turn runs, events go to the
//! turn's stream; a turn stream that was dropped does not stop the turn, and the
//! rest of it goes out of band. Outside a turn they go to the out-of-band buffer
//! (1024 events; the oldest are dropped and a `provider_notice { lagged }` says how
//! many). The turn is *active* from the acceptance of `send_turn` to the emission
//! of its terminal event, which is `done` for everything the server reports and
//! `error` only for a dead process (or `closed`).
//!
//! # Permissions
//!
//! Each server request that asks something becomes a `permission_ask`, answered
//! with [`AgentSession::answer_permission`] (see [`super::map::answer_for`]). The
//! neutral policy is applied first, locally: a request that the session's
//! [`ToolPolicy`] denies is declined without asking anyone, one it allows (an
//! `allow` pattern, `auto_edits` for a file edit) is accepted. A request the
//! adapter cannot turn into a permission (an MCP elicitation that is not a tool
//! approval, a server method it does not serve) is declined / refused and noted by
//! a `provider_notice`; it never hangs the turn.
//!
//! # Interruption (§10)
//!
//! Codex has `turn/interrupt` only: `interrupt` sends it and the turn ends with
//! `done interrupted` when the server says `turn/completed { interrupted }`.
//! `cancel_tools` is `Unsupported { tool_cancel }`.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};

use super::CodexConfig;
use super::map::{Ask, AskKind, MapConfig, MapState, answer_for};
use super::transport::{Inbound, Process};
use super::wire::{
    ApprovalPolicy, Notification, SandboxMode, SandboxPolicy, ServerRequest, TurnInterruptParams,
    TurnStartParams, TurnStartResult, UserInput,
};
use crate::agent::{
    AgentEvent, AgentSession, CancelOutcome, CancelScope, Capabilities, EventStream,
    InterruptOutcome, InterruptScope, PermissionDecision, PermissionScope, PolicyDecision,
    PolicyMode, ProcessDiagnostic, ProviderError, ProviderKind, QuestionAnswer, ResumeToken,
    SessionLimits, StopReason, ToolPolicy, TurnInput,
};

/// Capacity of the out-of-band buffer.
pub(crate) const OUT_OF_BAND_CAPACITY: usize = 1024;

/// How long `turn/start` and `turn/interrupt` may take to be answered.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maps a neutral policy mode to the two Codex axes (contract §6): `ask` and
/// `auto_edits` → `on-request` + `workspace-write`; `plan_only` → `on-request` +
/// `read-only`; `trust` → `never` + `workspace-write` (never `danger-full-access`).
pub fn policy_axes(mode: PolicyMode) -> (ApprovalPolicy, SandboxMode) {
    match mode {
        PolicyMode::PlanOnly => (ApprovalPolicy::OnRequest, SandboxMode::ReadOnly),
        PolicyMode::Trust => (ApprovalPolicy::Never, SandboxMode::WorkspaceWrite),
        _ => (ApprovalPolicy::OnRequest, SandboxMode::WorkspaceWrite),
    }
}

/// Why a turn was stopped from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopCause {
    Interrupted,
    TimedOut(u64),
}

struct PendingAsk {
    rpc_id: Value,
    kind: AskKind,
    scopes: Vec<PermissionScope>,
}

pub(crate) struct State {
    closed: bool,
    dead: Option<ProviderError>,
    turn: Option<mpsc::UnboundedSender<AgentEvent>>,
    turn_id: Option<String>,
    turn_seq: u64,
    stop: Option<StopCause>,
    out_of_band: VecDeque<AgentEvent>,
    out_of_band_dropped: u64,
    out_of_band_taken: bool,
    pending: HashMap<String, PendingAsk>,
    policy: ToolPolicy,
    model: Option<String>,
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
            self.turn_id = None;
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
    thread_id: String,
    cwd: String,
    extra_dirs: Vec<String>,
    turn_timeout_ms: Option<u64>,
    ceiling: Option<ToolPolicy>,
    state: Mutex<State>,
    wake: Notify,
    turn_known: Notify,
    ask_seq: AtomicU64,
}

/// Everything `open` / `resume` hand over to build a session.
pub(crate) struct CoreParts {
    pub(crate) capabilities: Capabilities,
    pub(crate) process: Arc<Process>,
    pub(crate) settings: Arc<CodexConfig>,
    pub(crate) thread_id: String,
    pub(crate) cwd: String,
    pub(crate) extra_dirs: Vec<String>,
    pub(crate) limits: SessionLimits,
    pub(crate) ceiling: Option<ToolPolicy>,
    pub(crate) policy: ToolPolicy,
    pub(crate) model: Option<String>,
    pub(crate) deltas: bool,
    pub(crate) login_hint: Option<String>,
    pub(crate) initial_events: Vec<AgentEvent>,
}

impl Core {
    pub(crate) fn start(parts: CoreParts, inbound: mpsc::UnboundedReceiver<Inbound>) -> Arc<Self> {
        let mut map = MapState::new(MapConfig {
            deltas: parts.deltas,
            cost_basis: parts.settings.cost_basis,
            prices: parts.settings.prices.clone(),
            login_hint: parts.login_hint,
        });
        map.set_main_thread(parts.thread_id.clone());
        map.set_model(parts.model.clone());
        let mut state = State {
            closed: false,
            dead: None,
            turn: None,
            turn_id: None,
            turn_seq: 0,
            stop: None,
            out_of_band: VecDeque::new(),
            out_of_band_dropped: 0,
            out_of_band_taken: false,
            pending: HashMap::new(),
            policy: parts.policy,
            model: parts.model,
            map,
        };
        for event in parts.initial_events {
            state.push_out_of_band(event);
        }
        let core = Arc::new(Self {
            capabilities: parts.capabilities,
            process: parts.process,
            thread_id: parts.thread_id,
            cwd: parts.cwd,
            extra_dirs: parts.extra_dirs,
            turn_timeout_ms: parts.limits.turn_timeout_ms,
            ceiling: parts.ceiling,
            state: Mutex::new(state),
            wake: Notify::new(),
            turn_known: Notify::new(),
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
        let notification = match Notification::parse(method, params) {
            Ok(notification) => notification,
            Err(reason) => {
                tracing::warn!(%reason, "codex notification dropped");
                return;
            },
        };
        let mut events = Vec::new();
        {
            let mut state = self.lock();
            match &notification {
                Notification::TurnStarted(envelope) => {
                    let main = envelope
                        .thread_id
                        .as_deref()
                        .is_none_or(|thread| thread == self.thread_id);
                    if main && state.turn.is_some() && state.turn_id.is_none() {
                        state.turn_id = Some(envelope.turn.id.clone());
                        self.turn_known.notify_waiters();
                    }
                },
                Notification::RequestResolved(resolved) => {
                    state
                        .pending
                        .retain(|_, ask| ask.rpc_id != resolved.request_id);
                },
                Notification::TurnCompleted(_) => {
                    // Settle the end of the turn below, with the cause in hand.
                },
                _ => {},
            }
            let stop = state.stop;
            for mut event in state.map.map(notification) {
                if let (
                    Some(StopCause::TimedOut(after_ms)),
                    AgentEvent::Done {
                        stop_reason,
                        is_error,
                        error,
                        ..
                    },
                ) = (stop, &mut event)
                    && *stop_reason == StopReason::Interrupted
                {
                    *stop_reason = StopReason::Error;
                    *is_error = true;
                    *error = Some(ProviderError::Timeout { after_ms });
                }
                events.push(event);
            }
        }
        for event in events {
            self.emit(event);
        }
    }

    async fn on_request(&self, id: Value, method: &str, params: &Value) {
        let request = match ServerRequest::parse(method, params) {
            Ok(request) => request,
            Err(reason) => {
                tracing::warn!(%reason, "codex server request refused");
                let _ = self
                    .process
                    .respond_error(&id, -32602, "invalid params")
                    .await;
                return;
            },
        };
        if let ServerRequest::Unknown(method) = &request {
            let _ = self
                .process
                .respond_error(&id, -32601, "method not supported by this client")
                .await;
            self.notice("unsupported_server_request", json!({ "method": method }));
            return;
        }
        let ask = self.lock().map.ask_for(&request);
        let Some(mut ask) = ask else {
            // An elicitation that is not an MCP tool approval: nobody can fill it in.
            let server = match &request {
                ServerRequest::Elicitation(elicitation) => elicitation.server_name.clone(),
                _ => String::new(),
            };
            let _ = self
                .process
                .respond(&id, super::wire::elicitation_response("decline", None))
                .await;
            self.notice("mcp_elicitation_declined", json!({ "server": server }));
            return;
        };
        let decision = {
            let state = self.lock();
            state.policy.decide(&ask.tool, Some(&ask.arg), ask.category)
        };
        match decision {
            PolicyDecision::Allow | PolicyDecision::Deny => {
                let allow = decision == PolicyDecision::Allow;
                let verdict = if allow {
                    PermissionDecision::allow_once()
                } else {
                    PermissionDecision::deny()
                };
                if let Ok((answer, _)) = answer_for(&ask.kind, &ask.scopes, &verdict) {
                    let _ = self.process.respond(&id, answer).await;
                }
                self.notice(
                    if allow {
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
            PolicyDecision::Ask => self.raise(id, ask),
        }
    }

    /// Registers the request, then emits its events (the request is answerable as
    /// soon as the consumer sees the `permission_ask`).
    fn raise(&self, rpc_id: Value, ask: Ask) {
        let request_id = format!("codex-ask-{}", self.ask_seq.fetch_add(1, Ordering::SeqCst));
        self.lock().pending.insert(
            request_id.clone(),
            PendingAsk {
                rpc_id,
                kind: ask.kind,
                scopes: ask.scopes,
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

    fn turn_params(&self, input: &TurnInput) -> TurnStartParams {
        let state = self.lock();
        let (approval, sandbox) = policy_axes(state.policy.mode);
        let sandbox_policy = match sandbox {
            SandboxMode::ReadOnly => SandboxPolicy::ReadOnly,
            SandboxMode::WorkspaceWrite => SandboxPolicy::WorkspaceWrite {
                writable_roots: if self.extra_dirs.is_empty() {
                    Vec::new()
                } else {
                    std::iter::once(self.cwd.clone())
                        .chain(self.extra_dirs.iter().cloned())
                        .collect()
                },
            },
        };
        TurnStartParams {
            thread_id: self.thread_id.clone(),
            input: vec![UserInput::Text {
                text: input.joined_text(),
            }],
            model: state.model.clone(),
            approval_policy: Some(approval),
            sandbox_policy: Some(sandbox_policy),
        }
    }

    async fn request_interrupt(&self) -> Result<(), ProviderError> {
        // The turn id is known from the `turn/start` answer; wait briefly for the
        // `turn/started` notification in the rare case the stream was read first.
        let turn_id = {
            let waiting = self.turn_known.notified();
            tokio::pin!(waiting);
            waiting.as_mut().enable();
            if let Some(id) = self.lock().turn_id.clone() {
                id
            } else if tokio::time::timeout(Duration::from_secs(5), waiting)
                .await
                .is_ok()
            {
                self.lock().turn_id.clone().ok_or(ProviderError::Closed)?
            } else {
                return Err(ProviderError::Timeout { after_ms: 5000 });
            }
        };
        let params = serde_json::to_value(TurnInterruptParams {
            thread_id: self.thread_id.clone(),
            turn_id,
        })
        .map_err(|_| ProviderError::protocol("interrupt parameters"))?;
        self.process
            .request("turn/interrupt", params, REQUEST_TIMEOUT)
            .await
            .map(|_| ())
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
            Inbound::Malformed(reason) => tracing::warn!(%reason, "codex line ignored"),
            Inbound::Closed { code } => {
                core.on_death(code);
                break;
            },
        }
    }
}

/// A live Codex session.
pub struct CodexSession {
    core: Arc<Core>,
    capabilities: Capabilities,
}

impl std::fmt::Debug for CodexSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexSession")
            .field("thread_id", &self.core.thread_id)
            .field("pid", &self.core.process.pid())
            .finish_non_exhaustive()
    }
}

impl CodexSession {
    pub(crate) fn new(core: Arc<Core>) -> Self {
        let capabilities = core.capabilities.clone();
        Self { core, capabilities }
    }
}

#[async_trait]
impl AgentSession for CodexSession {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        self.capabilities.resume.then(|| {
            ResumeToken::new(
                ProviderKind::Codex,
                1,
                json!({ "thread_id": self.core.thread_id }),
            )
        })
    }

    async fn send_turn(&self, input: TurnInput) -> Result<EventStream, ProviderError> {
        self.core.usable()?;
        if input.has_images() {
            return Err(ProviderError::unsupported("images"));
        }
        if input.joined_text().trim().is_empty() {
            return Err(ProviderError::invalid("a turn needs some text"));
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        let seq = {
            let mut state = self.core.lock();
            if state.closed {
                return Err(ProviderError::Closed);
            }
            if state.turn.is_some() {
                return Err(ProviderError::TurnInProgress);
            }
            state.turn = Some(sender);
            state.turn_id = None;
            state.stop = None;
            state.turn_seq += 1;
            state.map.begin_turn();
            state.turn_seq
        };
        let params = serde_json::to_value(self.core.turn_params(&input))
            .map_err(|_| ProviderError::protocol("turn parameters"))?;
        let started = self
            .core
            .process
            .request("turn/start", params, REQUEST_TIMEOUT)
            .await;
        let turn_id = match started.and_then(|result| {
            serde_json::from_value::<TurnStartResult>(result)
                .map_err(|_| ProviderError::protocol("malformed `turn/start` result"))
        }) {
            Ok(result) => result.turn.id,
            Err(error) => {
                // No stream was handed out: the refusal is the answer of `send_turn`.
                let mut state = self.core.lock();
                if state.turn_seq == seq {
                    state.turn = None;
                }
                return Err(error);
            },
        };
        {
            let mut state = self.core.lock();
            if state.turn_seq == seq && state.turn.is_some() && state.turn_id.is_none() {
                state.turn_id = Some(turn_id);
                self.core.turn_known.notify_waiters();
            }
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
                    let _ = core.request_interrupt().await;
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
        let (rpc_id, answer, interrupt) = {
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
            let (answer, interrupt) = answer_for(&pending.kind, &pending.scopes, &decision)?;
            let rpc_id = pending.rpc_id.clone();
            state.pending.remove(request_id);
            (rpc_id, answer, interrupt)
        };
        self.core.process.respond(&rpc_id, answer).await?;
        if interrupt {
            // A denial that stops the turn: best effort, the answer is already sent.
            let _ = self.core.request_interrupt().await;
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
        self.core.request_interrupt().await?;
        // `turn_only` and `turn_and_tools` stop the same things here: Codex has one
        // interruption, and no tool of its own to cut.
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
        let mut state = self.core.lock();
        state.model = Some(model.to_owned());
        state.map.set_model(Some(model.to_owned()));
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

impl Drop for CodexSession {
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
