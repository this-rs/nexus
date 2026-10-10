//! [`ClaudeCodeProvider`] and its session: Claude Code behind
//! [`AgentProvider`] / [`AgentSession`].
//!
//! The session is a façade over [`InteractiveClient`]. What it writes to the CLI
//! is what the orchestrator wrote before the contract existed — the same input
//! message, the same control JSON ([`super::control`]).
//!
//! # One pump per session
//!
//! A single task reads **every** message of the CLI (`subscribe_messages`) and
//! its control channel (`take_sdk_control_receiver`), projects them
//! ([`map_message`], [`permission_event`]) and routes each event to exactly one
//! destination: the stream of the running turn, or the out-of-band buffer when no
//! turn is running (contract §9). A turn stops being "running" when its terminal
//! event is **emitted**. Messages are read before control requests, so a
//! `tool_call` the CLI printed before asking a permission is emitted before the
//! `permission_ask` that names it.
//!
//! Control writes go through a clone of the CLI's stdin sender: the client is
//! never locked while a turn runs, only for the time of `send_message`.
//!
//! # Cancellation and background tasks
//!
//! The CLI's tools are its child processes. `cancel_tools(all)` and
//! `interrupt(turn_and_tools)` send `SIGINT` to the CLI's descendants and never
//! to the CLI ([`super::cancel`]); `interrupt(turn_only)` only writes the
//! interrupt request. A complete `tool_call` of `Bash { run_in_background }` or
//! `Monitor` opens a background task ([`super::tasks`]); its process is claimed a
//! second later from the descendants that appeared since, and every change to
//! the table is followed by a complete `background_tasks` snapshot.

use std::collections::{HashMap, HashSet, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Notify, RwLock, mpsc};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, warn};

use super::cancel;
use super::control::{self, InboundControl, PermissionRequest};
use super::input;
use super::map_events::{MapState, compaction_trigger, map_message, permission_event};
use super::options::{ClaudeCodeConfig, build_options, ignored_extension_keys};
use super::policy_map::{canonical_name, neutral_to_native, tool_category};
use super::tasks::{TaskTable, task_from_tool_call};
use crate::agent::{
    AgentEvent, AgentProvider, AgentSession, BackgroundTaskStatus, CancelOutcome, CancelScope,
    Capabilities, CompactionInfo, CompactionPhase, CostBasis, EventStream, HealthStatus,
    HookVerdict, InterruptOutcome, InterruptScope, ModelInfo, PermissionDecision, PermissionScope,
    PolicyMode, ProcessDiagnostic, ProviderError, ProviderHealth, ProviderKind, QuestionAnswer,
    ResumeToken, SessionHooks, SessionSpec, TaskPhase, ToolCallInfo, ToolResultInfo, TurnInput,
    now_ms,
};
use crate::errors::SdkError;
use crate::interactive::{InteractiveClient, dispatch_hook_from_registry};
use crate::transport::remote::{RemoteProbe, probe_version};
use crate::transport::subprocess::{find_claude_cli, get_cli_version_with_policy, min_cli_version};
use crate::types::{
    ClaudeCodeOptions, HookCallback, HookContext, HookInput, HookJSONOutput, HookMatcher,
    HookSpecificOutput, Message, PostToolUseHookSpecificOutput, PreToolUseHookSpecificOutput,
    SyncHookJSONOutput,
};

/// Capacity of the out-of-band buffer (contract §9).
const OUT_OF_BAND_CAPACITY: usize = 1024;

/// Delay between the two readings of the CLI's descendants that attribute a
/// process to a background task (what the orchestrator's `async_pid_claim` did).
const PID_CLAIM_DELAY: Duration = Duration::from_secs(1);

/// How long `cancel_tools(task)` waits for a claim still in flight before it
/// decides the task has no known process.
const PID_CLAIM_WAIT: Duration = Duration::from_millis(2_500);

/// Builds the client of a session from its options. The default spawns the CLI;
/// a test kit substitutes an in-memory transport.
pub type ClientFactory =
    Arc<dyn Fn(ClaudeCodeOptions) -> Result<InteractiveClient, SdkError> + Send + Sync>;

type HookRegistry = Arc<RwLock<HashMap<String, Arc<dyn HookCallback>>>>;
type MessageStream = Pin<Box<dyn Stream<Item = Result<Message, SdkError>> + Send + 'static>>;

/// The Claude Code CLI as an [`AgentProvider`].
#[derive(Clone)]
pub struct ClaudeCodeProvider {
    config: ClaudeCodeConfig,
    factory: Option<ClientFactory>,
}

impl std::fmt::Debug for ClaudeCodeProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCodeProvider")
            .field("config", &self.config)
            .field("custom_client", &self.factory.is_some())
            .finish()
    }
}

impl Default for ClaudeCodeProvider {
    fn default() -> Self {
        Self::new(ClaudeCodeConfig::default())
    }
}

impl ClaudeCodeProvider {
    /// A provider that spawns the Claude Code CLI.
    pub fn new(config: ClaudeCodeConfig) -> Self {
        Self {
            config,
            factory: None,
        }
    }

    /// A provider whose sessions run on a client built by `factory` instead of a
    /// spawned CLI: a replay transport in tests, or a transport of the host's own.
    /// The factory receives the options [`build_options`] produced, hooks included.
    pub fn with_client_factory(config: ClaudeCodeConfig, factory: ClientFactory) -> Self {
        Self {
            config,
            factory: Some(factory),
        }
    }

    /// Configuration of the instance.
    pub fn config(&self) -> &ClaudeCodeConfig {
        &self.config
    }

    async fn start(
        &self,
        spec: SessionSpec,
        resume: Option<String>,
    ) -> Result<Arc<dyn AgentSession>, ProviderError> {
        spec.validate()?;
        let capabilities = self.config.capabilities(spec.model.as_deref());
        let mut options = build_options(&self.config, &spec, resume.as_deref())?;
        let core = Arc::new(Core::new(capabilities, self.config.cost_basis));
        if let Some(session_id) = resume {
            core.lock().session_id = Some(session_id);
        }
        if let Some(hooks) = &spec.hooks {
            options.hooks = Some(neutral_hooks(Arc::clone(hooks), &core));
        }
        let ignored = ignored_extension_keys(&spec);
        if !ignored.is_empty() {
            core.emit(AgentEvent::ProviderNotice {
                kind: "extension_ignored".to_owned(),
                data: json!({ "keys": ignored }),
            });
        }

        let mut client = match &self.factory {
            Some(factory) => factory(options)?,
            None => InteractiveClient::new(options)?,
        };
        client.connect().await?;
        // Everything the CLI says from here on is read by the pump.
        let wired = wire(&client, &core).await;
        let (messages, control_rx) = match wired {
            Ok(wired) => wired,
            Err(error) => {
                let _ = client.disconnect().await;
                return Err(error);
            },
        };
        let registry = client.hook_callbacks();
        let pid = client.child_pid().await;
        *core.cli_pid.lock().unwrap_or_else(PoisonError::into_inner) = pid;
        let pump = tokio::spawn(pump(Arc::clone(&core), messages, control_rx, registry));
        Ok(Arc::new(ClaudeCodeSession {
            core,
            client: tokio::sync::Mutex::new(client),
            hooks: spec.hooks.clone(),
            model: Mutex::new(
                spec.model
                    .clone()
                    .or_else(|| self.config.default_model.clone()),
            ),
            turns: std::sync::atomic::AtomicU32::new(0),
            pid,
            machine: self
                .config
                .remote
                .as_ref()
                .map(crate::transport::remote::RemoteHost::machine),
            pump,
        }))
    }
}

/// Subscribes to the CLI's messages, takes its stdin and its control channel,
/// and registers the hooks.
async fn wire(
    client: &InteractiveClient,
    core: &Arc<Core>,
) -> Result<(MessageStream, Option<mpsc::Receiver<Value>>), ProviderError> {
    let messages = client
        .subscribe_messages()
        .await
        .ok_or_else(|| ProviderError::protocol("the transport exposes no message stream"))?;
    let stdin = client
        .clone_stdin_sender()
        .await
        .ok_or_else(|| ProviderError::protocol("the transport exposes no stdin"))?;
    *core.stdin.lock().unwrap_or_else(PoisonError::into_inner) = Some(stdin);
    client.initialize_hooks().await?;
    Ok((messages, client.take_sdk_control_receiver().await))
}

#[async_trait]
impl AgentProvider for ClaudeCodeProvider {
    fn id(&self) -> &str {
        &self.config.id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::ClaudeCode
    }

    /// Whether the CLI is there, and its version. Never runs a login (A27).
    ///
    /// `cli_not_found` when the executable is missing; `degraded` with a
    /// `detail` when its version is below the SDK's minimum (recent models are
    /// gated on it); `ok` with the probed version otherwise, or with no version
    /// when the probe says nothing.
    async fn health(&self) -> ProviderHealth {
        if self.factory.is_some() {
            // No executable behind a custom client.
            return ProviderHealth::ok(None);
        }
        let version = if let Some(remote) = &self.config.remote {
            // The machine is asked through the same pinned channel a session uses.
            // Never a fallback to the local CLI: that would run somewhere else than
            // where the user chose.
            match probe_version(remote).await {
                RemoteProbe::Version(version) => version,
                RemoteProbe::CliMissing => {
                    return ProviderHealth::unavailable(ProviderError::CliNotFound {
                        program: format!("{} on {}", remote.cli, remote.machine()),
                    });
                },
                RemoteProbe::Unreachable(why) => {
                    return ProviderHealth::unavailable(ProviderError::unreachable(format!(
                        "{}: {why}",
                        remote.machine()
                    )));
                },
            }
        } else {
            let path = match &self.config.cli_path {
                Some(path) => path.clone(),
                None => match find_claude_cli() {
                    Ok(path) => path,
                    Err(error) => return ProviderHealth::unavailable(error.into()),
                },
            };
            if !path.exists() {
                return ProviderHealth::unavailable(ProviderError::CliNotFound {
                    program: path.display().to_string(),
                });
            }
            get_cli_version_with_policy(&path, &self.config.env_policy).await
        };
        let minimum = min_cli_version();
        match version {
            Some(version) if version < minimum => ProviderHealth {
                status: HealthStatus::Degraded,
                version: Some(version.to_string()),
                detail: Some(format!(
                    "Claude CLI {version} is below the minimum {minimum}: recent models may be \
                     refused; upgrade with `claude update`"
                )),
                error: None,
                login_hint: None,
                checked_at_ms: now_ms(),
            },
            version => ProviderHealth::ok(version.map(|version| version.to_string())),
        }
    }

    async fn catalog(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(self.config.models.clone())
    }

    fn capabilities(&self, model: Option<&str>) -> Capabilities {
        self.config.capabilities(model)
    }

    async fn open(&self, spec: SessionSpec) -> Result<Arc<dyn AgentSession>, ProviderError> {
        self.start(spec, None).await
    }

    async fn resume(
        &self,
        spec: SessionSpec,
        token: ResumeToken,
    ) -> Result<Arc<dyn AgentSession>, ProviderError> {
        let data = token.expect_kind(ProviderKind::ClaudeCode)?;
        // A CLI session lives in one user's home on one machine: resuming it
        // anywhere else would silently start a different conversation.
        let issued_on = data.get("machine").and_then(Value::as_str);
        let here = self
            .config
            .remote
            .as_ref()
            .map(crate::transport::remote::RemoteHost::machine);
        if issued_on != here.as_deref() {
            return Err(ProviderError::invalid(format!(
                "this session belongs to {}, not to {}",
                issued_on.unwrap_or("the local machine"),
                here.as_deref().unwrap_or("the local machine")
            )));
        }
        let session_id = data
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ProviderError::invalid("resume token carries no session_id"))?
            .to_owned();
        self.start(spec, Some(session_id)).await
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// A permission request waiting for its answer.
struct PendingPermission {
    /// Input of the tool, replayed when the answer carries none.
    input: Value,
    /// The CLI's suggestions for a lasting approval.
    suggestions: Option<Value>,
}

struct State {
    closed: bool,
    /// The CLI's message stream ended: the process is gone.
    dead: bool,
    /// Sink of the running turn; `Some` exactly while a turn is running.
    turn: Option<mpsc::UnboundedSender<AgentEvent>>,
    out_of_band: VecDeque<AgentEvent>,
    out_of_band_dropped: u64,
    out_of_band_taken: bool,
    map: MapState,
    pending: HashMap<String, PendingPermission>,
    /// Identifiers of the `tool_call`s emitted so far.
    tool_calls: HashSet<String>,
    session_started: bool,
    session_id: Option<String>,
    /// The background tasks of the session (§4 `background_tasks`).
    tasks: TaskTable,
    /// Tasks whose process is being looked for (a claim in flight).
    claims: HashSet<String>,
}

impl State {
    /// Routes the complete table of background tasks.
    fn route_tasks(&mut self) {
        let tasks = self.tasks.snapshot();
        self.route(AgentEvent::BackgroundTasks { tasks });
    }

    /// Sends the event to its one destination.
    fn route(&mut self, event: AgentEvent) {
        if self.closed {
            return;
        }
        if let AgentEvent::ToolCall { id, .. } = &event {
            self.tool_calls.insert(id.clone());
        }
        let Some(turn) = &self.turn else {
            self.push_out_of_band(event);
            return;
        };
        let terminal = event.is_terminal();
        // A dropped turn stream does not interrupt the turn: what is left of it
        // goes out of band.
        if let Err(mpsc::error::SendError(event)) = turn.send(event) {
            self.push_out_of_band(event);
        }
        if terminal {
            self.turn = None;
        }
    }

    fn push_out_of_band(&mut self, event: AgentEvent) {
        if self.out_of_band.len() >= OUT_OF_BAND_CAPACITY {
            self.out_of_band.pop_front();
            self.out_of_band_dropped += 1;
        }
        self.out_of_band.push_back(event);
    }
}

/// What the session, its pump and its hooks share. Holds no client, so a hook
/// callback (owned by the client's registry) can hold it without a cycle.
struct Core {
    capabilities: Capabilities,
    state: Mutex<State>,
    /// Wakes the out-of-band reader.
    wake: Notify,
    /// Stops the pump.
    shutdown: Notify,
    /// The CLI's stdin; `None` once the session is closed.
    stdin: Mutex<Option<mpsc::Sender<String>>>,
    /// Process identifier of the CLI; `None` behind a transport without a process.
    cli_pid: Mutex<Option<u32>>,
    /// The tasks answering `hook_callback` requests: tracked so `close` cancels
    /// a hook that never answers instead of leaking it.
    hook_tasks: Mutex<tokio::task::JoinSet<()>>,
}

enum OutOfBandNext {
    Event(Box<AgentEvent>),
    Empty,
    End,
}

impl Core {
    fn new(capabilities: Capabilities, cost_basis: CostBasis) -> Self {
        Self {
            capabilities,
            state: Mutex::new(State {
                closed: false,
                dead: false,
                turn: None,
                out_of_band: VecDeque::new(),
                out_of_band_dropped: 0,
                out_of_band_taken: false,
                map: MapState::with_cost_basis(cost_basis),
                pending: HashMap::new(),
                tool_calls: HashSet::new(),
                session_started: false,
                session_id: None,
                tasks: TaskTable::new(),
                claims: HashSet::new(),
            }),
            wake: Notify::new(),
            shutdown: Notify::new(),
            stdin: Mutex::new(None),
            cli_pid: Mutex::new(None),
            hook_tasks: Mutex::new(tokio::task::JoinSet::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn cli_pid(&self) -> Option<u32> {
        *self.cli_pid.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Marks `killed` the tasks whose process is among `pids`, with a snapshot
    /// when something changed.
    fn mark_killed(&self, pids: &[u32]) {
        if pids.is_empty() {
            return;
        }
        let mut state = self.lock();
        if state.tasks.mark_killed(pids) > 0 {
            state.route_tasks();
            drop(state);
            self.wake.notify_waiters();
        }
    }

    /// Looks for the process of a background task started by `task_id`: the
    /// CLI's descendants are read now and again after [`PID_CLAIM_DELAY`]; the
    /// youngest newcomer is the task's. Runs on its own task so the pump never
    /// waits; needs a runtime, without one the pid simply stays unknown.
    fn schedule_pid_claim(self: &Arc<Self>, task_id: String, cli_pid: u32) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        self.lock().claims.insert(task_id.clone());
        let core = Arc::clone(self);
        handle.spawn(async move {
            let before = cancel::descendant_pids(cli_pid).await;
            tokio::time::sleep(PID_CLAIM_DELAY).await;
            let after = cancel::descendant_rows(cli_pid).await;
            let claimed = cancel::claim_pid(&before, &after);
            let mut state = core.lock();
            state.claims.remove(&task_id);
            match claimed {
                Some(pid) if state.tasks.set_pid(&task_id, pid) => state.route_tasks(),
                Some(_) => {},
                None => debug!(%task_id, "no new descendant of the CLI to attribute to the task"),
            }
            drop(state);
            core.wake.notify_waiters();
        });
    }

    /// Waits for a claim in flight on `task_id`, at most [`PID_CLAIM_WAIT`].
    async fn await_pid_claim(&self, task_id: &str) {
        let deadline = tokio::time::Instant::now() + PID_CLAIM_WAIT;
        while self.lock().claims.contains(task_id) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn emit(&self, event: AgentEvent) {
        self.lock().route(event);
        self.wake.notify_waiters();
    }

    /// `Closed` after `close()`, `ProcessExited` once the CLI is gone.
    fn check_usable(state: &State) -> Result<(), ProviderError> {
        if state.closed {
            Err(ProviderError::Closed)
        } else if state.dead {
            Err(ProviderError::ProcessExited { code: None })
        } else {
            Ok(())
        }
    }

    /// Writes one line to the CLI's stdin.
    async fn write(&self, line: String) -> Result<(), ProviderError> {
        let stdin = self
            .stdin
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(stdin) = stdin else {
            return Err(ProviderError::Closed);
        };
        stdin
            .send(line)
            .await
            .map_err(|_| ProviderError::ProcessExited { code: None })
    }

    /// One message of the CLI: projected, post-processed, routed.
    fn on_message(self: &Arc<Self>, message: &Message) {
        let mut state = self.lock();
        let mut events = map_message(message, &mut state.map);
        if !self.capabilities.background_tasks
            && let Message::System { subtype, data } = message
            && matches!(
                subtype.as_str(),
                "task_started"
                    | "task_progress"
                    | "task_updated"
                    | "task_notification"
                    | "background_tasks_changed"
            )
        {
            // Capability absent (§5): no `task_update` nor `background_tasks`.
            // The payload is not lost, it travels as a notice.
            events = vec![AgentEvent::ProviderNotice {
                kind: subtype.clone(),
                data: data.clone(),
            }];
        }
        let tasks = self.capabilities.background_tasks;
        let mut claims: Vec<String> = Vec::new();
        for event in events {
            // What the event does to the table of background tasks (A5); the
            // snapshot that follows the event is routed after it.
            let mut snapshot_after = false;
            let event = match event {
                AgentEvent::ToolCall {
                    ref id,
                    ref name,
                    ref input,
                    input_complete: true,
                    ref parent,
                    ..
                } if tasks => {
                    if let Some(task) =
                        task_from_tool_call(id, name, input, parent.as_deref(), now_ms())
                    {
                        state.tasks.start(task);
                        snapshot_after = true;
                        claims.push(id.clone());
                    }
                    event
                },
                AgentEvent::BackgroundTasks { tasks: reported } if tasks => {
                    // The CLI's own report, merged: the snapshot IS the event.
                    state.tasks.merge(reported);
                    AgentEvent::BackgroundTasks {
                        tasks: state.tasks.snapshot(),
                    }
                },
                AgentEvent::TaskUpdate {
                    phase: TaskPhase::Notification,
                    ref task_id,
                    ref tool_call_id,
                    ref status,
                    ..
                } if tasks => {
                    snapshot_after = state.tasks.apply_notification(
                        task_id.as_deref(),
                        tool_call_id.as_deref(),
                        status.as_deref(),
                    );
                    event
                },
                AgentEvent::SessionStarted {
                    provider_session_id,
                    ..
                } if state.session_started => {
                    // At most one `session_started` per process (§4, invariant 5):
                    // the CLI repeats `init`, the repeats travel as notices.
                    if let Some(id) = provider_session_id.filter(|id| !id.is_empty()) {
                        state.session_id = Some(id);
                    }
                    let Message::System { subtype, data } = message else {
                        continue;
                    };
                    AgentEvent::ProviderNotice {
                        kind: subtype.clone(),
                        data: data.clone(),
                    }
                },
                AgentEvent::SessionStarted {
                    ref provider_session_id,
                    ..
                } => {
                    state.session_started = true;
                    if let Some(id) = provider_session_id.as_ref().filter(|id| !id.is_empty()) {
                        state.session_id = Some(id.clone());
                    }
                    event
                },
                AgentEvent::Done {
                    ref provider_session_id,
                    ..
                } => {
                    if let Some(id) = provider_session_id.as_ref().filter(|id| !id.is_empty()) {
                        state.session_id = Some(id.clone());
                    }
                    event
                },
                other => other,
            };
            state.route(event);
            if snapshot_after {
                state.route_tasks();
            }
        }
        drop(state);
        self.wake.notify_waiters();
        if let Some(cli_pid) = self.cli_pid() {
            for task_id in claims {
                self.schedule_pid_claim(task_id, cli_pid);
            }
        }
    }

    /// The CLI's message stream ended without `close()`: the process is gone.
    fn on_process_exit(&self) {
        let mut state = self.lock();
        if state.closed || state.dead {
            return;
        }
        state.route(AgentEvent::Error {
            error: ProviderError::ProcessExited { code: None },
        });
        state.dead = true;
        state.pending.clear();
        drop(state);
        self.wake.notify_waiters();
    }

    /// A `can_use_tool` request: a question the adapter unblocks itself, or a
    /// permission to ask.
    async fn on_permission_request(&self, mut request: PermissionRequest) {
        let question = request.is_question();
        {
            let mut state = self.lock();
            if !question {
                // §4, invariant 2: a `tool_call_id` names a call already emitted.
                // The CLI prints the call before it asks; if it did not, the link
                // is dropped rather than left dangling.
                if request
                    .tool_use_id
                    .as_ref()
                    .is_some_and(|id| !state.tool_calls.contains(id))
                {
                    request.tool_use_id = None;
                }
                state.pending.insert(
                    request.request_id.clone(),
                    PendingPermission {
                        input: request.input.clone(),
                        suggestions: request.permission_suggestions.clone(),
                    },
                );
            }
            let event = permission_event(&request, &state.map);
            state.route(event);
        }
        self.wake.notify_waiters();
        if question {
            // §15.6: the tool is unblocked at once; the user's answer comes as
            // a regular turn.
            let line = control::permission_allow(&request.request_id, &request.input);
            if let Err(error) = self.write(line).await {
                warn!(%error, "AskUserQuestion could not be unblocked");
            }
        }
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
}

// ---------------------------------------------------------------------------
// Pump
// ---------------------------------------------------------------------------

async fn next_control(control: &mut Option<mpsc::Receiver<Value>>) -> Option<Value> {
    match control {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// The one task of a session that reads the CLI.
async fn pump(
    core: Arc<Core>,
    mut messages: MessageStream,
    mut control_rx: Option<mpsc::Receiver<Value>>,
    registry: HookRegistry,
) {
    loop {
        tokio::select! {
            biased;
            () = core.shutdown.notified() => break,
            // Messages first: what the CLI printed before a control request is
            // emitted before what that request becomes.
            next = messages.next() => match next {
                Some(Ok(message)) => core.on_message(&message),
                Some(Err(error)) => core.emit(AgentEvent::Error { error: error.into() }),
                None => {
                    core.on_process_exit();
                    break;
                },
            },
            request = next_control(&mut control_rx) => match request {
                Some(request) => on_control(&core, request, &registry).await,
                None => control_rx = None,
            },
        }
    }
    debug!("Claude Code session pump ended");
}

async fn on_control(core: &Arc<Core>, message: Value, registry: &HookRegistry) {
    match control::parse_inbound(&message) {
        InboundControl::CanUseTool(request) => core.on_permission_request(request).await,
        InboundControl::HookCallback { request_id } => {
            // A hook may take its time (it calls the host): it must not hold
            // back the events of the turn.
            let registry = Arc::clone(registry);
            let mut tasks = core
                .hook_tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            // Reap the finished ones so the set stays as small as the hooks in flight.
            while tasks.try_join_next().is_some() {}
            let core = Arc::clone(core);
            tasks.spawn(async move {
                let Some(result) = dispatch_hook_from_registry(&message, &registry).await else {
                    // The CLI and the registry disagree on what was registered:
                    // worth a warning, not an invented answer.
                    warn!(%request_id, "hook callback for an unknown callback id: not answered");
                    return;
                };
                if let Err(error) = core
                    .write(control::hook_response(&request_id, &result))
                    .await
                {
                    warn!(%request_id, %error, "hook response could not be written");
                }
            });
        },
        // The acknowledgement of one of our own requests, or a subtype this
        // adapter does not handle.
        InboundControl::Other => {},
    }
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// The one callback registered for `PreToolUse`, `PostToolUse` and `PreCompact`:
/// it calls the host's neutral [`SessionHooks`] and translates the verdict.
struct NeutralHook {
    hooks: Arc<dyn SessionHooks>,
    core: Arc<Core>,
}

fn tool_call_info(id: Option<&str>, name: &str, input: &Value) -> ToolCallInfo {
    ToolCallInfo {
        id: id.map(str::to_owned),
        name: name.to_owned(),
        canonical: canonical_name(name),
        category: tool_category(name),
        input: input.clone(),
    }
}

fn sync_output(specific: Option<HookSpecificOutput>) -> HookJSONOutput {
    HookJSONOutput::Sync(SyncHookJSONOutput {
        hook_specific_output: specific,
        ..SyncHookJSONOutput::default()
    })
}

#[async_trait]
impl HookCallback for NeutralHook {
    async fn execute(
        &self,
        input: &HookInput,
        tool_use_id: Option<&str>,
        _context: &HookContext,
    ) -> Result<HookJSONOutput, SdkError> {
        match input {
            HookInput::PreToolUse(pre) => {
                let call = tool_call_info(tool_use_id, &pre.tool_name, &pre.tool_input);
                let decided = |decision: Option<&str>, reason, updated_input, context| {
                    sync_output(Some(HookSpecificOutput::PreToolUse(
                        PreToolUseHookSpecificOutput {
                            permission_decision: decision.map(str::to_owned),
                            permission_decision_reason: reason,
                            updated_input,
                            additional_context: context,
                        },
                    )))
                };
                Ok(match self.hooks.before_tool(&call).await {
                    HookVerdict::Deny { reason } => decided(Some("deny"), Some(reason), None, None),
                    HookVerdict::ReplaceInput(input) => decided(None, None, Some(input), None),
                    HookVerdict::AddContext(context) => decided(None, None, None, Some(context)),
                    // `Continue`, and any verdict a later contract adds.
                    _ => sync_output(None),
                })
            },
            HookInput::PostToolUse(post) => {
                let result = ToolResultInfo {
                    call: tool_call_info(tool_use_id, &post.tool_name, &post.tool_input),
                    output: post.tool_response.clone(),
                    is_error: post
                        .tool_response
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                };
                Ok(match self.hooks.after_tool(&result).await {
                    Some(context) => sync_output(Some(HookSpecificOutput::PostToolUse(
                        PostToolUseHookSpecificOutput {
                            additional_context: Some(context),
                        },
                    ))),
                    None => sync_output(None),
                })
            },
            HookInput::PreCompact(compact) => {
                self.core.emit(AgentEvent::Compaction {
                    phase: CompactionPhase::Started,
                    trigger: Some(compaction_trigger(Some(&compact.trigger))),
                    pre_tokens: None,
                });
                let info = CompactionInfo {
                    trigger: compact.trigger.clone(),
                    custom_instructions: compact.custom_instructions.clone(),
                };
                if self.hooks.before_compaction(&info).await.is_some() {
                    // The CLI's hook protocol has no slot for instructions
                    // returned by a `PreCompact` callback: say so instead of
                    // pretending they were applied.
                    self.core.emit(AgentEvent::ProviderNotice {
                        kind: "compaction_instructions_unsupported".to_owned(),
                        data: Value::Null,
                    });
                }
                Ok(sync_output(None))
            },
            // Not registered by this adapter.
            _ => Ok(sync_output(None)),
        }
    }
}

/// The hook map of a session that carries neutral hooks: one matcher without
/// criteria for each of the three events.
fn neutral_hooks(
    hooks: Arc<dyn SessionHooks>,
    core: &Arc<Core>,
) -> HashMap<String, Vec<HookMatcher>> {
    let callback: Arc<dyn HookCallback> = Arc::new(NeutralHook {
        hooks,
        core: Arc::clone(core),
    });
    ["PreToolUse", "PostToolUse", "PreCompact"]
        .into_iter()
        .map(|event| {
            (
                event.to_owned(),
                vec![HookMatcher {
                    matcher: None,
                    hooks: vec![Arc::clone(&callback)],
                }],
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// A live Claude Code session.
struct ClaudeCodeSession {
    core: Arc<Core>,
    /// Locked only to send the input of a turn and to disconnect.
    client: tokio::sync::Mutex<InteractiveClient>,
    /// The host's hooks, for `before_turn` (the tool and compaction hooks go
    /// through the CLI's hook protocol).
    hooks: Option<Arc<dyn SessionHooks>>,
    /// Model the next turn runs on, as far as this side knows: the spec's, then
    /// what `set_model` and the directives asked for.
    model: Mutex<Option<String>>,
    /// Turns started so far.
    turns: std::sync::atomic::AtomicU32,
    /// Process identifier of the CLI. Diagnostic only.
    pid: Option<u32>,
    /// The machine the CLI runs on (`user@host:port`), `None` when local. A session
    /// identifier only means something on the machine that produced it.
    machine: Option<String>,
    pump: JoinHandle<()>,
}

impl Drop for ClaudeCodeSession {
    fn drop(&mut self) {
        // The pump holds the shared state; the transport kills the CLI when the
        // client is dropped with the session.
        self.pump.abort();
    }
}

impl ClaudeCodeSession {
    fn usable(&self) -> Result<(), ProviderError> {
        Core::check_usable(&self.core.lock())
    }

    /// Asks the host which model the turn runs on (`SessionHooks::before_turn`).
    /// Another model: `set_model` is written to the CLI before the input, and a
    /// `model_changed` opens the turn. Nothing to ask, nothing said, the current
    /// model, or a hook that does not answer in time: nothing happens.
    async fn before_turn(&self, input_chars: usize) -> Result<(), ProviderError> {
        let Some(hooks) = &self.hooks else {
            return Ok(());
        };
        let index = self.turns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let current = self
            .model
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut ctx = crate::agent::TurnContext::new(index, current.clone().unwrap_or_default());
        ctx.input_chars = input_chars;
        let directive = match tokio::time::timeout(HOOK_TIMEOUT, hooks.before_turn(&ctx)).await {
            Ok(directive) => directive,
            Err(_) => {
                self.core.emit(AgentEvent::ProviderNotice {
                    kind: "hook_timeout".to_owned(),
                    data: json!({ "provider": "claude_code", "hook": "before_turn" }),
                });
                return Ok(());
            },
        };
        let Some(model) = directive.model else {
            return Ok(());
        };
        if model.trim().is_empty() || current.as_deref() == Some(model.as_str()) {
            return Ok(());
        }
        self.core.write(control::set_model(&model)).await?;
        *self.model.lock().unwrap_or_else(PoisonError::into_inner) = Some(model.clone());
        self.core.emit(AgentEvent::ModelChanged { model });
        Ok(())
    }
}

/// How long a `before_turn` hook may take before the turn goes on without it.
const HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[async_trait]
impl AgentSession for ClaudeCodeSession {
    fn capabilities(&self) -> &Capabilities {
        &self.core.capabilities
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        let session_id = self.core.lock().session_id.clone()?;
        Some(match &self.machine {
            None => ResumeToken::claude_code_session(session_id),
            Some(machine) => ResumeToken::new(
                ProviderKind::ClaudeCode,
                1,
                json!({ "session_id": session_id, "machine": machine }),
            ),
        })
    }

    async fn send_turn(&self, input: TurnInput) -> Result<EventStream, ProviderError> {
        let (receiver, blocks) = {
            let mut state = self.core.lock();
            Core::check_usable(&state)?;
            if state.turn.is_some() {
                return Err(ProviderError::TurnInProgress);
            }
            if input.has_images() && !self.core.capabilities.images {
                return Err(ProviderError::unsupported("images"));
            }
            // A turn with an image is checked before it opens: refused, nothing
            // is written and no turn is running.
            let blocks = if input.has_images() {
                Some(input::content_blocks(&input)?)
            } else {
                None
            };
            let (sender, receiver) = mpsc::unbounded_channel();
            state.turn = Some(sender);
            state.map.set_interrupt_requested(false);
            (receiver, blocks)
        };
        let text = input.joined_text();
        if let Err(error) = self.before_turn(text.len()).await {
            self.core.lock().turn = None;
            return Err(error);
        }
        // Text only: the string message, unchanged. With an image: the blocks,
        // in the user's order.
        let sent = match blocks {
            None => self.client.lock().await.send_message(text).await,
            Some(blocks) => self.client.lock().await.send_message_blocks(blocks).await,
        };
        if let Err(error) = sent {
            // Nothing reached the CLI: no turn is running.
            self.core.lock().turn = None;
            return Err(error.into());
        }
        Ok(Box::pin(UnboundedReceiverStream::new(receiver)))
    }

    async fn answer_permission(
        &self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), ProviderError> {
        let core = &self.core;
        let pending = {
            let mut state = core.lock();
            Core::check_usable(&state)?;
            if let PermissionDecision::Allow { scope, .. } = &decision
                && !core.capabilities.permission_scopes.contains(scope)
            {
                return Err(ProviderError::unsupported("permission_scope"));
            }
            state.pending.remove(request_id).ok_or_else(|| {
                ProviderError::invalid("unknown permission request, or one already answered")
            })?
        };
        let (line, interrupt) = match &decision {
            PermissionDecision::Allow {
                scope,
                updated_input,
            } => {
                // No replacement: the original input is replayed (§15.1).
                let input = updated_input.as_ref().unwrap_or(&pending.input);
                let destination = match scope {
                    PermissionScope::Session => Some("session"),
                    PermissionScope::Always => Some("localSettings"),
                    _ => None,
                };
                let updates = destination.and_then(|destination| {
                    control::scoped_permission_updates(pending.suggestions.as_ref(), destination)
                });
                let line = match updates {
                    Some(updates) => {
                        control::permission_allow_with_updates(request_id, input, &updates)
                    },
                    None => control::permission_allow(request_id, input),
                };
                (line, false)
            },
            PermissionDecision::Deny { message, interrupt } => (
                control::permission_deny(request_id, message.as_deref()),
                *interrupt,
            ),
            // `PermissionDecision` is `#[non_exhaustive]`.
            #[allow(unreachable_patterns)]
            _ => return Err(ProviderError::unsupported("permission_decision")),
        };
        if let Err(error) = core.write(line).await {
            // Not answered: the request can be answered again.
            core.lock().pending.insert(request_id.to_owned(), pending);
            return Err(error);
        }
        if interrupt {
            self.interrupt(InterruptScope::TurnAndTools).await?;
        }
        Ok(())
    }

    /// Always `Unsupported { answer_question }`: Claude Code's question is
    /// answered by a regular turn (`question.reply == turn`).
    async fn answer_question(
        &self,
        _question_id: &str,
        _answer: QuestionAnswer,
    ) -> Result<(), ProviderError> {
        self.usable()?;
        Err(ProviderError::unsupported("answer_question"))
    }

    /// Writes the interrupt request (§15.5) on the CLI's stdin; with
    /// `turn_and_tools`, also sends `SIGINT` to the CLI's descendants (the
    /// tools it is running), never to the CLI. `turn_only` leaves every process
    /// alone: background work survives the end of the turn.
    async fn interrupt(&self, scope: InterruptScope) -> Result<InterruptOutcome, ProviderError> {
        {
            let mut state = self.core.lock();
            Core::check_usable(&state)?;
            if state.turn.is_none() {
                return Ok(InterruptOutcome::default());
            }
            state.map.set_interrupt_requested(true);
        }
        if let Err(error) = self.core.write(control::interrupt()).await {
            self.core.lock().map.set_interrupt_requested(false);
            return Err(error);
        }
        let killed = match (scope, self.pid) {
            (InterruptScope::TurnAndTools, Some(pid)) => {
                cancel::signal_descendants(pid, cancel::SIGINT).await
            },
            _ => Vec::new(),
        };
        self.core.mark_killed(&killed);
        Ok(InterruptOutcome {
            turn_interrupted: true,
            tools_cancelled: u32::try_from(killed.len()).unwrap_or(u32::MAX),
            diagnostic: self.pid.map(|pid| ProcessDiagnostic {
                pid: Some(pid),
                killed_pids: killed,
            }),
        })
    }

    /// Stops tools without ending the turn (§10, A6).
    ///
    /// `all`: `SIGINT` to every descendant of the CLI, the CLI untouched; the
    /// turn goes on to its own `done` once the CLI has reported the cut tools.
    /// `task { id }`: `SIGINT` to the subtree of that background task's process,
    /// then the task is `killed` and a snapshot follows. An unknown task is
    /// `invalid_request`; a task whose process was never found (the claim found
    /// no newcomer, or there is no process behind this transport) is left as it
    /// is and `tools_cancelled` is 0 — never a `killed` nothing was sent to.
    async fn cancel_tools(&self, scope: CancelScope) -> Result<CancelOutcome, ProviderError> {
        self.usable()?;
        // Capability absent (over SSH, off Unix): the written fallback (§5), never a
        // cancellation that reaches nothing.
        if !self.core.capabilities.tool_cancel {
            return Err(ProviderError::unsupported("tool_cancel"));
        }
        let diagnostic = |killed: Vec<u32>| {
            Some(ProcessDiagnostic {
                pid: self.pid,
                killed_pids: killed,
            })
        };
        match scope {
            CancelScope::All => {
                let killed = match self.pid {
                    Some(pid) => cancel::signal_descendants(pid, cancel::SIGINT).await,
                    None => Vec::new(),
                };
                self.core.mark_killed(&killed);
                Ok(CancelOutcome {
                    tools_cancelled: u32::try_from(killed.len()).unwrap_or(u32::MAX),
                    diagnostic: diagnostic(killed),
                })
            },
            CancelScope::Task { id } => {
                if self.core.lock().tasks.get(&id).is_none() {
                    return Err(ProviderError::invalid(format!(
                        "unknown background task `{id}`"
                    )));
                }
                self.core.await_pid_claim(&id).await;
                let target = {
                    let state = self.core.lock();
                    Core::check_usable(&state)?;
                    state.tasks.get(&id).and_then(|task| task.pid)
                };
                let Some(target) = target else {
                    debug!(task = %id, "background task without a known process: nothing signalled");
                    return Ok(CancelOutcome {
                        tools_cancelled: 0,
                        diagnostic: diagnostic(Vec::new()),
                    });
                };
                let killed = cancel::signal_subtree(target).await;
                {
                    let mut state = self.core.lock();
                    if state.tasks.set_status(&id, BackgroundTaskStatus::Killed) {
                        state.route_tasks();
                    }
                }
                self.core.wake.notify_waiters();
                Ok(CancelOutcome {
                    tools_cancelled: 1,
                    diagnostic: diagnostic(killed),
                })
            },
            // `CancelScope` is `#[non_exhaustive]`.
            #[allow(unreachable_patterns)]
            _ => Err(ProviderError::unsupported("cancel_scope")),
        }
    }

    async fn set_model(&self, model: &str) -> Result<(), ProviderError> {
        self.usable()?;
        self.core.write(control::set_model(model)).await?;
        *self.model.lock().unwrap_or_else(PoisonError::into_inner) = Some(model.to_owned());
        Ok(())
    }

    async fn set_policy_mode(
        &self,
        mode: PolicyMode,
        native: Option<&str>,
    ) -> Result<(), ProviderError> {
        self.usable()?;
        let native = neutral_to_native(mode, native);
        self.core.write(control::set_permission_mode(&native)).await
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
        {
            let mut state = self.core.lock();
            if state.closed {
                return Ok(());
            }
            if let Some(turn) = state.turn.take() {
                let _ = turn.send(AgentEvent::Error {
                    error: ProviderError::Closed,
                });
            }
            state.closed = true;
            state.pending.clear();
        }
        self.core.wake.notify_waiters();
        self.core.shutdown.notify_one();
        // A hook still waiting on the host dies with the session.
        self.core
            .hook_tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .abort_all();
        // Our clone of the CLI's stdin must go, or its input would stay open.
        self.core
            .stdin
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Err(error) = self.client.lock().await.disconnect().await {
            warn!(%error, "disconnecting the Claude Code CLI failed");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::PolicyMode;
    use crate::agent::StopReason;

    fn core() -> Arc<Core> {
        Arc::new(Core::new(
            ClaudeCodeConfig::default().capabilities(None),
            CostBasis::Reported,
        ))
    }

    fn notice(n: usize) -> AgentEvent {
        AgentEvent::ProviderNotice {
            kind: "status".to_owned(),
            data: json!({ "n": n }),
        }
    }

    fn done() -> AgentEvent {
        AgentEvent::Done {
            stop_reason: StopReason::Completed,
            subtype: None,
            is_error: false,
            result_text: None,
            usage: Default::default(),
            cost: Default::default(),
            duration_ms: 0,
            duration_api_ms: None,
            num_turns: 0,
            model: None,
            provider_session_id: None,
            structured_output: None,
            error: None,
        }
    }

    #[test]
    fn an_event_has_one_destination_and_the_terminal_event_ends_the_turn() {
        let core = core();
        core.emit(notice(0));
        let (sender, mut receiver) = mpsc::unbounded_channel();
        core.lock().turn = Some(sender);
        core.emit(notice(1));
        core.emit(done());
        core.emit(notice(2));
        assert_eq!(receiver.try_recv().unwrap(), notice(1));
        assert!(receiver.try_recv().unwrap().is_terminal());
        assert!(
            receiver.try_recv().is_err(),
            "the stream closes after its terminal event"
        );
        let state = core.lock();
        assert!(state.turn.is_none());
        assert_eq!(
            state.out_of_band.iter().cloned().collect::<Vec<_>>(),
            [notice(0), notice(2)]
        );
    }

    #[test]
    fn a_dropped_turn_stream_redirects_the_rest_out_of_band() {
        let core = core();
        let (sender, receiver) = mpsc::unbounded_channel();
        core.lock().turn = Some(sender);
        drop(receiver);
        core.emit(notice(1));
        assert!(
            core.lock().turn.is_some(),
            "dropping the stream does not end the turn"
        );
        core.emit(done());
        let state = core.lock();
        assert!(state.turn.is_none());
        assert_eq!(state.out_of_band.len(), 2);
    }

    #[test]
    fn an_overflowing_out_of_band_buffer_says_what_it_dropped() {
        let core = core();
        for n in 0..OUT_OF_BAND_CAPACITY + 6 {
            core.emit(notice(n));
        }
        match core.next_out_of_band() {
            OutOfBandNext::Event(event) => assert_eq!(
                *event,
                AgentEvent::ProviderNotice {
                    kind: "lagged".to_owned(),
                    data: json!({ "dropped": 6 }),
                }
            ),
            _ => panic!("the loss is reported first"),
        }
        match core.next_out_of_band() {
            OutOfBandNext::Event(event) => assert_eq!(*event, notice(6)),
            _ => panic!("the oldest kept event follows"),
        }
    }

    #[test]
    fn a_repeated_init_is_a_notice_and_a_classified_failure_stays_a_done_with_its_error() {
        let core = core();
        let init = Message::System {
            subtype: "init".to_owned(),
            data: json!({"session_id": "s1", "permissionMode": "default"}),
        };
        core.on_message(&init);
        core.on_message(&init);
        let failure = |text: &str| Message::Result {
            subtype: "error_during_execution".to_owned(),
            duration_ms: 1,
            duration_api_ms: 1,
            is_error: true,
            num_turns: 1,
            session_id: "s2".to_owned(),
            total_cost_usd: None,
            usage: None,
            result: Some(text.to_owned()),
            structured_output: None,
        };
        core.on_message(&failure("API Error: 529 overloaded_error"));
        core.on_message(&failure("the tool crashed"));
        let state = core.lock();
        let events: Vec<_> = state.out_of_band.iter().collect();
        assert!(matches!(
            events[0],
            AgentEvent::SessionStarted {
                policy_mode: Some(PolicyMode::Ask),
                ..
            }
        ));
        assert!(
            matches!(events[1], AgentEvent::ProviderNotice { kind, .. } if kind == "init"),
            "at most one session_started per process"
        );
        // The classified failure stays a `done` (usage and cost are kept) and
        // carries its typed error for the host's retry decision.
        assert!(matches!(
            events[2],
            AgentEvent::Done {
                stop_reason: StopReason::Error,
                is_error: true,
                error: Some(ProviderError::Overloaded),
                ..
            }
        ));
        // A failure nobody can classify carries no error.
        assert!(matches!(
            events[3],
            AgentEvent::Done {
                stop_reason: StopReason::Error,
                is_error: true,
                error: None,
                ..
            }
        ));
        assert_eq!(state.session_id.as_deref(), Some("s2"));
    }

    #[test]
    fn task_messages_travel_as_notices_while_background_tasks_is_absent() {
        // A core declaring the capability absent (§5 fallback).
        let mut capabilities = ClaudeCodeConfig::default().capabilities(None);
        capabilities.background_tasks = false;
        let core = Arc::new(Core::new(capabilities, CostBasis::Reported));
        assert!(!core.capabilities.background_tasks);
        for subtype in ["task_started", "background_tasks_changed"] {
            core.on_message(&Message::System {
                subtype: subtype.to_owned(),
                data: json!({"task_id": "t1"}),
            });
        }
        let state = core.lock();
        for (event, subtype) in state
            .out_of_band
            .iter()
            .zip(["task_started", "background_tasks_changed"])
        {
            assert_eq!(
                event,
                &AgentEvent::ProviderNotice {
                    kind: subtype.to_owned(),
                    data: json!({"task_id": "t1"}),
                }
            );
        }
        assert_eq!(state.map.seq(), 2, "the messages are still numbered");
    }

    fn tool_use(id: &str, name: &str, input: Value) -> Message {
        Message::Assistant {
            message: crate::types::AssistantMessage {
                content: vec![crate::types::ContentBlock::ToolUse(
                    crate::types::ToolUseContent {
                        id: id.to_owned(),
                        name: name.to_owned(),
                        input,
                    },
                )],
            },
            parent_tool_use_id: None,
        }
    }

    fn snapshots(events: &[AgentEvent]) -> Vec<Vec<(String, BackgroundTaskStatus)>> {
        events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::BackgroundTasks { tasks } => Some(
                    tasks
                        .iter()
                        .map(|task| (task.id.clone(), task.status))
                        .collect(),
                ),
                _ => None,
            })
            .collect()
    }

    /// A5: the table is fed by the tool calls, the CLI's report and the terminal
    /// notifications; every change is followed by the COMPLETE table.
    #[test]
    fn background_tasks_are_a_complete_snapshot_after_every_change() {
        let core = core();
        assert!(core.capabilities.background_tasks);
        core.on_message(&tool_use("toolu_fg", "Bash", json!({"command": "ls"})));
        core.on_message(&tool_use(
            "toolu_bg",
            "Bash",
            json!({"command": "sleep 30", "run_in_background": true, "description": "nap"}),
        ));
        core.on_message(&tool_use(
            "toolu_mon",
            "Monitor",
            json!({"description": "watch"}),
        ));
        core.on_message(&Message::System {
            subtype: "background_tasks_changed".to_owned(),
            data: json!({"tasks": [{"id": "agent-1", "type": "agent", "description": "explore", "status": "running"}]}),
        });
        core.on_message(&Message::System {
            subtype: "task_progress".to_owned(),
            data: json!({"task_id": "toolu_bg", "status": "running"}),
        });
        core.on_message(&Message::System {
            subtype: "task_notification".to_owned(),
            data: json!({"task_id": "toolu_bg", "status": "completed", "summary": "slept"}),
        });
        let state = core.lock();
        let events: Vec<AgentEvent> = state.out_of_band.iter().cloned().collect();
        let names: Vec<&str> = events.iter().map(AgentEvent::type_name).collect();
        assert_eq!(
            names,
            [
                "tool_call",        // foreground: no task
                "tool_call",        // background Bash
                "background_tasks", // … followed by the table
                "tool_call",        // Monitor
                "background_tasks",
                "background_tasks", // the CLI's report, merged
                "task_update",      // progress: typed, no change to the table
                "task_update",      // notification …
                "background_tasks", // … completed
            ]
        );
        use BackgroundTaskStatus::{Completed, Running};
        assert_eq!(
            snapshots(&events),
            [
                vec![("toolu_bg".to_owned(), Running)],
                vec![
                    ("toolu_bg".to_owned(), Running),
                    ("toolu_mon".to_owned(), Running)
                ],
                vec![
                    ("toolu_bg".to_owned(), Running),
                    ("toolu_mon".to_owned(), Running),
                    ("agent-1".to_owned(), Running)
                ],
                vec![
                    ("toolu_bg".to_owned(), Completed),
                    ("toolu_mon".to_owned(), Running),
                    ("agent-1".to_owned(), Running)
                ],
            ]
        );
        assert!(matches!(
            &events[6],
            AgentEvent::TaskUpdate { phase: TaskPhase::Progress, task_id: Some(id), .. } if id == "toolu_bg"
        ));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::ProviderNotice { .. })),
            "no task message travels as a notice when the capability is declared"
        );
        // Without a runtime no claim was scheduled: the pids stay unknown.
        assert!(state.claims.is_empty());
        assert!(state.tasks.snapshot().iter().all(|task| task.pid.is_none()));
    }

    #[tokio::test]
    async fn a_permission_names_its_tool_call_only_once_that_call_was_emitted() {
        let core = core();
        let request = |id: &str| PermissionRequest {
            request_id: id.to_owned(),
            tool_name: "Bash".to_owned(),
            input: json!({"command": "ls"}),
            tool_use_id: Some("toolu_1".to_owned()),
            permission_suggestions: None,
        };
        core.on_permission_request(request("r1")).await;
        core.on_message(&Message::Assistant {
            message: crate::types::AssistantMessage {
                content: vec![crate::types::ContentBlock::ToolUse(
                    crate::types::ToolUseContent {
                        id: "toolu_1".to_owned(),
                        name: "Bash".to_owned(),
                        input: json!({"command": "ls"}),
                    },
                )],
            },
            parent_tool_use_id: None,
        });
        core.on_permission_request(request("r2")).await;
        let state = core.lock();
        let links: Vec<_> = state
            .out_of_band
            .iter()
            .filter_map(|event| match event {
                AgentEvent::PermissionAsk { tool_call_id, .. } => Some(tool_call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(links, [None, Some("toolu_1".to_owned())]);
        assert!(state.pending.contains_key("r1") && state.pending.contains_key("r2"));
    }
}
