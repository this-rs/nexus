//! Any agent that speaks the **Agent Client Protocol** (ACP) behind the agent
//! contract: [`AcpProvider`] is a generic ACP *client* (JSON-RPC 2.0, one JSON object
//! per line on the agent's stdio). The agent is whatever the instance's `command`
//! launches: `opencode acp`, a Gemini CLI configured for ACP, a test fake.
//!
//! Compiled with the cargo feature `provider-acp`. Decision A38 (replaces an opencode
//! HTTP adapter), A40-like cost (never `reported`).
//!
//! | File | Role |
//! |---|---|
//! | `mod.rs` | [`AcpProvider`], [`AcpConfig`], capabilities, `health`, `open` / `resume`, MCP servers of `session/new` |
//! | `wire.rs` | the JSON-RPC types and the `validate_kind` check the versioned schema is tested with |
//! | `transport.rs` | the process (single launcher), id correlation, killing the tree |
//! | `map.rs` | pure projection of `session/update` and requests onto `AgentEvent`s |
//! | `session.rs` | [`AcpSession`]: pump, routing, permissions, turn lifecycle |
//!
//! # What is **not** established
//!
//! No real agent (opencode, Gemini CLI) was ever run. Everything comes from the public
//! specification (`https://agentclientprotocol.com/protocol/`, protocol version 1, read
//! 2026-10-05) and from the provider review, and is exercised against `fake_acp`, a fake
//! executable replaying JSONL transcripts (`tests/transcripts/acp/<version>/`). Marked
//! **NOT VERIFIED** where they occur and listed in `schema/PROVENANCE.md`:
//!
//! - `usage_update` and the `usage` of a prompt result (unstable in the specification);
//! - `session/set_model` (unstable: not used);
//! - the mode ids an agent publishes (`ask`, `plan`, `acceptEdits`… are conventions);
//! - that `-32000` is what an agent answers `session/new` with when it needs
//!   `authenticate`;
//! - that `opencode acp` / a Gemini CLI start with the arguments an instance gives;
//! - that an agent honours the `env` / `headers` arrays of an MCP server as written;
//! - opencode's `server_tool` spelling of MCP tool titles (the `canonical` rule);
//! - the reconnection of a `session/load` against a real agent (history replay is
//!   drained and dropped, `provider_notice { history_replayed }`).
//!
//! # Capabilities (per what the agent really offers, contract §5)
//!
//! | Field | Value |
//! |---|---|
//! | `resume` | `agentCapabilities.loadSession` as learned by `health()` / the last `open`; `false` until learned |
//! | `images` | no (A12), whatever `promptCapabilities.image` says |
//! | `thinking` | `AcpConfig::thinking` (ACP has no flag for it), or a thought chunk already seen by this provider |
//! | `per_session_mcp`, `tools` | yes: `mcpServers` of `session/new`; an HTTP / SSE server needs `mcpCapabilities.http` / `.sse` (else `Unsupported { mcp_http | mcp_sse }`) |
//! | `interactive_permissions` | yes; scopes `once`, `always`; a request offers those it has an `allow_once` / `allow_always` option for |
//! | `hooks`, `subagents`, `compaction_signal`, `background_tasks`, `tool_cancel`, `native_question` | none: ACP carries none of them |
//! | `sandbox` | `none`: information for the user, not a gate: `trust` opens, and is applied live when the agent publishes a matching mode |
//! | `secret_isolation` | yes: allowlisted environment, no secret on argv (an argument that looks like one is refused), MCP credentials only in the `session/new` JSON on the pipe |
//! | `context_window` | `AcpConfig::context_window` (`configured`), else `None`: never implicit |
//! | `set_model_live` | no (`session/set_model` is unstable): `Unsupported { set_model_live }`; `SessionSpec::model` is a label of `done.model` and of the price, announced by `provider_notice { model_not_applied }` |
//! | `cost` | from the configuration: `unknown` (default), `free`, `priced`; never `reported` (tokens only, when the agent gives them) |
//!
//! `set_policy_mode` is `session/set_mode` when the agent published modes and one
//! matches (`policy.native_mode` exactly, else the conventional ids of
//! [`map::neutral_of_mode`]), otherwise `Unsupported { set_policy_mode }`. The neutral
//! policy also applies locally to every permission request.
//!
//! # Refused at opening
//!
//! `limits.max_*` and `max_turns` (`Unsupported { limits }`; `turn_timeout_ms` is
//! honoured), `system_prompt` (`Unsupported { system_prompt }`: ACP has none),
//! `extra_dirs` (`Unsupported { extra_dirs }`), `trust` (`Unsupported { sandbox }`), a
//! command whose argument looks like a credential (`invalid_request`).
//!
//! # Authentication
//!
//! `authMethods` is read, never used: PO never runs `authenticate` nor a login. A
//! `session/new` refused with `-32000` is `AuthRequired { login_hint }`, the hint being
//! [`AcpConfig::login_hint`] or a sentence naming the published methods. `health()`
//! probes `session/new` only when `authMethods` is not empty (it creates, then drops,
//! one empty session of the agent).
//!
//! # Deviations from the written contract
//!
//! Listed in `docs/agent-contract.md` §5 (ACP paragraph).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::{
    AgentEvent, AgentProvider, AgentSession, Capabilities, ContextWindow, ContextWindowSource,
    CostBasis, McpServerSpec, McpServerStatus, ModelInfo, PermissionScope, ProviderError,
    ProviderHealth, ProviderKind, ResumeToken, SessionSpec, redact,
};
use crate::model::PriceTable;
use crate::transport::spawn::EnvPolicy;

pub mod map;
pub mod session;
pub mod transport;
pub mod wire;

pub use session::AcpSession;

use session::Shared;
use transport::{Inbound, Launch, Process};
use wire::{
    HttpMcpServer, InitializeParams, InitializeResult, LoadSessionParams, LoadSessionResult,
    McpServer, ModeState, NameValue, NewSessionParams, NewSessionResult, SetModeParams,
    StdioMcpServer,
};

/// Protocol version this adapter speaks (and the directory of the schema).
pub const SUPPORTED_PROTOCOL_VERSION: u32 = wire::PROTOCOL_VERSION;

/// Timeout of the handshake requests (`initialize`, `session/new`, `session/load`).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Configuration of one ACP instance.
#[derive(Clone)]
pub struct AcpConfig {
    /// Identifier of the instance (registry key).
    pub instance_id: String,
    /// Program and arguments that start the agent (`["opencode", "acp"]`). No secret:
    /// an argument that looks like one is refused.
    pub command: Vec<String>,
    /// Host variables the process inherits on top of the base allowlist (names).
    pub env_inherit: Vec<String>,
    /// Variables set explicitly on every process of the instance. Not for secrets.
    pub env: BTreeMap<String, String>,
    /// Where `done.cost` comes from: `Unknown` (default), `Free` or `Priced`; anything
    /// else is read as `Unknown`.
    pub cost_basis: CostBasis,
    /// Prices by model, for `Priced`.
    pub prices: PriceTable,
    /// Model label of a session that names none.
    pub default_model: Option<String>,
    /// Models `catalog()` lists besides the default one.
    pub models: Vec<String>,
    /// The agent emits reasoning (`agent_thought_chunk`).
    pub thinking: bool,
    /// Context window of the agent's model, when the operator knows it.
    pub context_window: Option<u64>,
    /// What a human runs to log in to the agent (never run by PO).
    pub login_hint: Option<String>,
    /// The dedicated `HOME` of the instance (decision A33): the agent keeps its own
    /// configuration and login there, and cannot read the host user's. Created
    /// `0700` when the agent starts. A human logs in with
    /// `HOME=<this dir> <agent> login`.
    pub home: std::path::PathBuf,
}

impl std::fmt::Debug for AcpConfig {
    /// `env` is documented "not for secrets", but an operator can still put one
    /// there: only the NAMES and the length of each value are printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpConfig")
            .field("instance_id", &self.instance_id)
            .field("command", &self.command)
            .field("env_inherit", &self.env_inherit)
            .field("env", &crate::agent::spec::redacted_map(&self.env))
            .field("cost_basis", &self.cost_basis)
            .field("prices", &self.prices)
            .field("default_model", &self.default_model)
            .field("models", &self.models)
            .field("thinking", &self.thinking)
            .field("context_window", &self.context_window)
            .field("login_hint", &self.login_hint)
            .field("home", &self.home)
            .finish()
    }
}

/// `<user data dir>/nexus/acp/<instance>/home`, or under the temp dir without a data dir.
pub fn default_acp_home(instance_id: &str) -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("nexus")
        .join("acp")
        .join(instance_id)
        .join("home")
}

impl AcpConfig {
    /// A configuration with the defaults: cost unknown, nothing inherited.
    pub fn new(instance_id: impl Into<String>, command: Vec<String>) -> Self {
        let instance_id = instance_id.into();
        Self {
            home: default_acp_home(&instance_id),
            instance_id,
            command,
            env_inherit: Vec::new(),
            env: BTreeMap::new(),
            cost_basis: CostBasis::Unknown,
            prices: PriceTable::new(),
            default_model: None,
            models: Vec::new(),
            thinking: false,
            context_window: None,
            login_hint: None,
        }
    }

    /// Refuses a configuration that cannot launch an agent safely: no program, an
    /// argument that looks like a secret (nothing of it is echoed), a bad variable name.
    pub fn validate(&self) -> Result<(), ProviderError> {
        if self
            .command
            .first()
            .is_none_or(|program| program.trim().is_empty())
        {
            return Err(ProviderError::invalid(
                "an ACP instance needs a command that starts with a program",
            ));
        }
        if self.command.iter().any(|part| redact(part) != *part) {
            return Err(ProviderError::invalid(
                "a command argument looks like a credential (or is too long): secrets never go on argv",
            ));
        }
        let bad_name = |name: &String| {
            name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        };
        if self.env_inherit.iter().any(bad_name) || self.env.keys().any(bad_name) {
            return Err(ProviderError::invalid(
                "an environment variable name is made of [A-Za-z0-9_]",
            ));
        }
        Ok(())
    }

    fn effective_cost_basis(&self, model: Option<&str>) -> CostBasis {
        match self.cost_basis {
            CostBasis::Free => CostBasis::Free,
            CostBasis::Priced => {
                if model.is_some_and(|model| self.prices.get(model).is_some()) {
                    CostBasis::Priced
                } else {
                    CostBasis::Unknown
                }
            },
            _ => CostBasis::Unknown,
        }
    }
}

/// What `initialize` taught about the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Learned {
    load_session: bool,
    mcp_http: bool,
    mcp_sse: bool,
}

/// What a handshake gives a session.
struct Handshake {
    session_id: String,
    modes: Option<ModeState>,
    replayed: u64,
}

/// The ACP provider: one instance of an ACP agent, one process per session.
pub struct AcpProvider {
    config: Arc<AcpConfig>,
    learned: Mutex<Option<Learned>>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for AcpProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpProvider")
            .field("instance_id", &self.config.instance_id)
            .finish_non_exhaustive()
    }
}

impl AcpProvider {
    /// A provider for `config`. Nothing is started, nothing is validated yet:
    /// `health`, `open` and `resume` refuse an invalid configuration.
    pub fn new(config: AcpConfig) -> Self {
        Self {
            config: Arc::new(config),
            learned: Mutex::new(None),
            shared: Arc::new(Shared::default()),
        }
    }

    /// The configuration of the instance.
    pub fn config(&self) -> &AcpConfig {
        &self.config
    }

    fn learned(&self) -> Option<Learned> {
        *self.learned.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn learn(&self, init: &InitializeResult) {
        *self.learned.lock().unwrap_or_else(PoisonError::into_inner) = Some(Learned {
            load_session: init.agent_capabilities.load_session,
            mcp_http: init.agent_capabilities.mcp_capabilities.http,
            mcp_sse: init.agent_capabilities.mcp_capabilities.sse,
        });
    }

    fn env_policy(&self, extra: &[String]) -> EnvPolicy {
        EnvPolicy::allowlist()
            .with_inherited(self.config.env_inherit.iter().cloned())
            .with_inherited(extra.iter().cloned())
            .with_home(&self.config.home)
    }

    fn launch(&self, spec: Option<&SessionSpec>, cwd: std::path::PathBuf) -> Launch {
        let mut env: Vec<(String, String)> = self
            .config
            .env
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        let mut extra = Vec::new();
        if let Some(spec) = spec {
            env.extend(spec.env.set.iter().map(|(n, v)| (n.clone(), v.clone())));
            extra.clone_from(&spec.env.inherit);
        }
        Launch {
            program: std::path::PathBuf::from(&self.config.command[0]),
            args: self.config.command[1..].to_vec(),
            env_policy: self.env_policy(&extra),
            env,
            cwd,
        }
    }

    fn login_hint(&self, init: Option<&InitializeResult>) -> Option<String> {
        if let Some(hint) = &self.config.login_hint {
            return Some(redact(hint));
        }
        let methods: Vec<&str> = init?
            .auth_methods
            .iter()
            .map(|method| method.id.as_str())
            .collect();
        (!methods.is_empty()).then(|| {
            redact(&format!(
                "log in to the agent with its own command (authentication methods: {})",
                methods.join(", ")
            ))
        })
    }

    /// Starts the agent and runs `initialize`.
    async fn connect(
        &self,
        launch: &Launch,
    ) -> Result<
        (
            Arc<Process>,
            tokio::sync::mpsc::UnboundedReceiver<Inbound>,
            InitializeResult,
        ),
        ProviderError,
    > {
        // The agent runs with a HOME of its own; it must exist before the process does.
        crate::transport::spawn::ensure_private_dir(&self.config.home).map_err(|error| {
            ProviderError::protocol(format!(
                "the instance HOME could not be created: {:?}",
                error.kind()
            ))
        })?;
        let (process, inbound) = Process::spawn(launch)?;
        let init = async {
            let value = process
                .request(
                    "initialize",
                    to_value(&InitializeParams::for_adapter()),
                    HANDSHAKE_TIMEOUT,
                )
                .await?;
            let init = serde_json::from_value::<InitializeResult>(value)
                .map_err(|_| ProviderError::protocol("malformed `initialize` result"))?;
            if init.protocol_version != SUPPORTED_PROTOCOL_VERSION {
                return Err(ProviderError::protocol(format!(
                    "the agent speaks ACP version {}, this client speaks {SUPPORTED_PROTOCOL_VERSION}",
                    init.protocol_version
                )));
            }
            Ok(init)
        }
        .await;
        match init {
            Ok(init) => {
                self.learn(&init);
                Ok((process, inbound, init))
            },
            Err(error) => {
                process.shutdown().await;
                Err(error)
            },
        }
    }

    async fn build(
        &self,
        spec: SessionSpec,
        resume: Option<String>,
    ) -> Result<Arc<dyn AgentSession>, ProviderError> {
        // Refusals that need nothing from the disk, the network or a process.
        self.config.validate()?;
        spec.validate()?;
        if spec.max_turns.is_some()
            || spec.limits.max_tokens.is_some()
            || spec.limits.max_cost_usd.is_some()
            || spec.limits.max_tool_iterations.is_some()
        {
            return Err(ProviderError::unsupported("limits"));
        }
        if spec.system_prompt.is_some() {
            return Err(ProviderError::unsupported("system_prompt"));
        }
        if !spec.extra_dirs.is_empty() {
            return Err(ProviderError::unsupported("extra_dirs"));
        }
        if resume.is_some() && self.learned().is_some_and(|learned| !learned.load_session) {
            return Err(ProviderError::unsupported("resume"));
        }
        let mcp = mcp_servers_wire(&spec.mcp_servers)?;
        if spec.env.set.keys().any(|name| {
            name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }) {
            return Err(ProviderError::invalid(
                "an environment variable name is made of [A-Za-z0-9_]",
            ));
        }
        if !spec.cwd.is_dir() {
            return Err(ProviderError::invalid("cwd is not a directory"));
        }
        let launch = self.launch(Some(&spec), spec.cwd.clone());
        let (process, mut inbound, init) = self.connect(&launch).await?;
        match self
            .handshake(&process, &mut inbound, &init, &spec, &mcp, resume.clone())
            .await
        {
            Ok(handshake) => {
                let model = spec
                    .model
                    .clone()
                    .or_else(|| self.config.default_model.clone());
                let mut initial_events = Vec::new();
                if spec.hooks.is_some() {
                    initial_events.push(AgentEvent::ProviderNotice {
                        kind: "hooks_not_supported".to_owned(),
                        data: json!({ "provider": "acp" }),
                    });
                }
                if !spec.policy.allow.is_empty() || !spec.policy.deny.is_empty() {
                    initial_events.push(AgentEvent::ProviderNotice {
                        kind: "policy_patterns_partial".to_owned(),
                        data: json!({ "applies_to": "permission requests" }),
                    });
                }
                if spec.model.is_some() {
                    initial_events.push(AgentEvent::ProviderNotice {
                        kind: "model_not_applied".to_owned(),
                        data: json!({ "detail": "ACP gives no stable way to choose the model: the name is a label" }),
                    });
                }
                if handshake.replayed > 0 {
                    initial_events.push(AgentEvent::ProviderNotice {
                        kind: "history_replayed".to_owned(),
                        data: json!({ "updates": handshake.replayed }),
                    });
                }
                initial_events.push(AgentEvent::SessionStarted {
                    provider_session_id: Some(handshake.session_id.clone()),
                    model: model.clone(),
                    policy_mode: Some(spec.policy.mode),
                    native_mode: handshake
                        .modes
                        .as_ref()
                        .map(|modes| modes.current_mode_id.clone()),
                    tools: Vec::new(),
                    mcp_servers: spec
                        .mcp_servers
                        .keys()
                        .map(|name| McpServerStatus {
                            name: name.clone(),
                            status: "configured".to_owned(),
                        })
                        .collect(),
                    cwd: Some(spec.cwd.display().to_string()),
                });
                let capabilities = self.capabilities(model.as_deref());
                let core = session::Core::start(
                    session::CoreParts {
                        capabilities,
                        process,
                        settings: Arc::clone(&self.config),
                        session_id: handshake.session_id,
                        limits: spec.limits,
                        ceiling: spec.policy_ceiling.clone(),
                        policy: spec.policy.clone(),
                        model,
                        deltas: spec.deltas,
                        login_hint: self.login_hint(Some(&init)),
                        mcp_servers: spec.mcp_servers.keys().cloned().collect(),
                        modes: handshake.modes,
                        shared: Arc::clone(&self.shared),
                        initial_events,
                    },
                    inbound,
                );
                Ok(Arc::new(AcpSession::new(core)))
            },
            Err(error) => {
                process.shutdown().await;
                Err(error)
            },
        }
    }

    async fn handshake(
        &self,
        process: &Arc<Process>,
        inbound: &mut tokio::sync::mpsc::UnboundedReceiver<Inbound>,
        init: &InitializeResult,
        spec: &SessionSpec,
        mcp: &[McpServer],
        resume: Option<String>,
    ) -> Result<Handshake, ProviderError> {
        let capabilities = &init.agent_capabilities;
        if resume.is_some() && !capabilities.load_session {
            return Err(ProviderError::unsupported("resume"));
        }
        for server in mcp {
            if let McpServer::Http(http) = server {
                let (allowed, name) = if http.r#type == "sse" {
                    (capabilities.mcp_capabilities.sse, "mcp_sse")
                } else {
                    (capabilities.mcp_capabilities.http, "mcp_http")
                };
                if !allowed {
                    return Err(ProviderError::unsupported(name));
                }
            }
        }
        let hint = self.login_hint(Some(init));
        let cwd = spec.cwd.display().to_string();
        let refine = |error: ProviderError| map::with_login_hint(error, hint.as_deref());
        let (session_id, modes, replayed) = match resume {
            Some(session_id) => {
                let value = process
                    .request(
                        "session/load",
                        to_value(&LoadSessionParams {
                            session_id: session_id.clone(),
                            cwd,
                            mcp_servers: mcp.to_vec(),
                        }),
                        HANDSHAKE_TIMEOUT,
                    )
                    .await
                    .map_err(refine)?;
                let loaded = if value.is_null() {
                    LoadSessionResult::default()
                } else {
                    serde_json::from_value::<LoadSessionResult>(value)
                        .map_err(|_| ProviderError::protocol("malformed `session/load` result"))?
                };
                // The agent replays the history as `session/update`s before it
                // answers: they are not events of this session.
                let mut replayed = 0u64;
                while let Ok(message) = inbound.try_recv() {
                    if matches!(&message, Inbound::Notification { method, .. } if method == "session/update")
                    {
                        replayed += 1;
                    }
                }
                (session_id, loaded.modes, replayed)
            },
            None => {
                let value = process
                    .request(
                        "session/new",
                        to_value(&NewSessionParams {
                            cwd,
                            mcp_servers: mcp.to_vec(),
                        }),
                        HANDSHAKE_TIMEOUT,
                    )
                    .await
                    .map_err(refine)?;
                let created = serde_json::from_value::<NewSessionResult>(value)
                    .map_err(|_| ProviderError::protocol("malformed `session/new` result"))?;
                (created.session_id, created.modes, 0)
            },
        };
        if let Some(native) = spec.policy.native_mode.as_deref()
            && let Some(state) = &modes
            && state.available_modes.iter().any(|mode| mode.id == native)
            && state.current_mode_id != native
        {
            process
                .request(
                    "session/set_mode",
                    to_value(&SetModeParams {
                        session_id: session_id.clone(),
                        mode_id: native.to_owned(),
                    }),
                    HANDSHAKE_TIMEOUT,
                )
                .await?;
        }
        Ok(Handshake {
            session_id,
            modes,
            replayed,
        })
    }
}

/// `serde_json::to_value` of a wire type (plain structs of strings, options and
/// booleans: it always serialises).
fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// The `mcpServers` of `session/new` for the servers of a session. The secrets of
/// `env` and `headers` are the spec's and travel in this JSON only (a pipe), never on
/// argv, never in a log or an error.
///
/// Refuses (`invalid_request`, nothing echoed): an empty server name or command, a URL
/// with user-info or a credential-shaped query, an empty header or variable name.
pub fn mcp_servers_wire(
    servers: &BTreeMap<String, McpServerSpec>,
) -> Result<Vec<McpServer>, ProviderError> {
    let bad = |detail: &str| Err(ProviderError::invalid(detail));
    let pairs = |map: &BTreeMap<String, String>| -> Vec<NameValue> {
        map.iter()
            .map(|(name, value)| NameValue {
                name: name.clone(),
                value: value.clone(),
            })
            .collect()
    };
    let mut wired = Vec::with_capacity(servers.len());
    for (name, server) in servers {
        if name.is_empty() {
            return bad("an MCP server needs a name");
        }
        match server {
            McpServerSpec::Stdio { command, args, env } => {
                if command.is_empty() {
                    return bad("an MCP server needs a command");
                }
                if env.keys().any(String::is_empty) {
                    return bad("an MCP environment variable needs a name");
                }
                wired.push(McpServer::Stdio(StdioMcpServer {
                    name: name.clone(),
                    command: command.clone(),
                    args: args.clone(),
                    env: pairs(env),
                }));
            },
            McpServerSpec::Http { url, headers } | McpServerSpec::Sse { url, headers } => {
                if redact(url) != *url || url.contains('@') {
                    return bad("an MCP URL must carry no credential: use a header");
                }
                if headers.keys().any(String::is_empty) {
                    return bad("an MCP header needs a name");
                }
                wired.push(McpServer::Http(HttpMcpServer {
                    r#type: if matches!(server, McpServerSpec::Sse { .. }) {
                        "sse"
                    } else {
                        "http"
                    }
                    .to_owned(),
                    name: name.clone(),
                    url: url.clone(),
                    headers: pairs(headers),
                }));
            },
            #[allow(unreachable_patterns)]
            _ => return Err(ProviderError::unsupported("mcp_transport")),
        }
    }
    Ok(wired)
}

#[async_trait]
impl AgentProvider for AcpProvider {
    fn id(&self) -> &str {
        &self.config.instance_id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Acp
    }

    async fn health(&self) -> ProviderHealth {
        if let Err(error) = self.config.validate() {
            return ProviderHealth::unavailable(error);
        }
        let cwd = std::env::temp_dir();
        let (process, _inbound, init) = match self.connect(&self.launch(None, cwd.clone())).await {
            Ok(connected) => connected,
            Err(error) => return ProviderHealth::unavailable(error),
        };
        let version = init
            .agent_info
            .as_ref()
            .and_then(|info| info.version.clone());
        let mut health = ProviderHealth::ok(version.clone());
        if !init.auth_methods.is_empty() {
            // The agent advertises authentication: a session is the proof it is not
            // needed (or is). The probe session is dropped with the process.
            let probe = process
                .request(
                    "session/new",
                    to_value(&NewSessionParams {
                        cwd: cwd.display().to_string(),
                        mcp_servers: Vec::new(),
                    }),
                    HANDSHAKE_TIMEOUT,
                )
                .await;
            match probe {
                Ok(_) => {
                    health.detail = Some(
                        "the agent advertises authentication methods but opened a session without one"
                            .to_owned(),
                    );
                },
                Err(error) => {
                    let error =
                        map::with_login_hint(error, self.login_hint(Some(&init)).as_deref());
                    health = ProviderHealth::unavailable(error);
                    health.version = version;
                    health.detail = Some(
                        "the agent needs authentication; a human runs the login command".to_owned(),
                    );
                },
            }
        }
        process.shutdown().await;
        health
    }

    async fn catalog(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        // ACP publishes no model list in its stable surface: the models the
        // configuration names.
        let mut ids: Vec<&String> = self
            .config
            .default_model
            .iter()
            .chain(self.config.models.iter())
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids
            .into_iter()
            .map(|id| {
                let mut info = ModelInfo::new(id.clone());
                info.is_default = self.config.default_model.as_ref() == Some(id);
                info.pricing = self.config.prices.get(id).copied();
                info.context_window = self.config.context_window.map(|value| ContextWindow {
                    value,
                    source: ContextWindowSource::Configured,
                });
                info.supports_tools = Some(true);
                info.supports_images = Some(false);
                info.supports_thinking = Some(self.config.thinking);
                info
            })
            .collect())
    }

    fn capabilities(&self, model: Option<&str>) -> Capabilities {
        let model = model.or(self.config.default_model.as_deref());
        let mut capabilities = Capabilities::none();
        capabilities.interactive_permissions = true;
        capabilities.permission_scopes = vec![PermissionScope::Once, PermissionScope::Always];
        capabilities.secret_isolation = true;
        capabilities.per_session_mcp = true;
        capabilities.thinking =
            self.config.thinking || self.shared.thinking_seen.load(Ordering::SeqCst);
        capabilities.tools = true;
        capabilities.resume = self.learned().is_some_and(|learned| learned.load_session);
        capabilities.context_window = self.config.context_window.map(|value| ContextWindow {
            value,
            source: ContextWindowSource::Configured,
        });
        capabilities.cost = self.config.effective_cost_basis(model);
        capabilities
    }

    async fn open(&self, spec: SessionSpec) -> Result<Arc<dyn AgentSession>, ProviderError> {
        self.build(spec, None).await
    }

    async fn resume(
        &self,
        spec: SessionSpec,
        token: ResumeToken,
    ) -> Result<Arc<dyn AgentSession>, ProviderError> {
        let data = token.expect_kind(ProviderKind::Acp)?;
        let session_id = data
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
            .ok_or_else(|| ProviderError::invalid("the resume token carries no session id"))?
            .to_owned();
        self.build(spec, Some(session_id)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `env` is documented "not for secrets", but an operator can still put one there:
    /// a log line that prints the configuration must not repeat it.
    #[test]
    fn the_debug_of_a_config_never_prints_an_environment_value() {
        let mut config = config(&["agent"]);
        config.env.insert(
            "AGENT_API_KEY".to_owned(),
            "sk-live-0123456789abcdef".to_owned(),
        );
        let shown = format!("{config:?}");
        assert!(!shown.contains("sk-live-0123456789abcdef"), "{shown}");
        assert!(shown.contains("AGENT_API_KEY"), "the name stays: {shown}");
    }

    /// Decision A33: the agent runs with a HOME of its own, never the host user's.
    #[test]
    fn the_agent_runs_with_a_dedicated_home() {
        let mut config = config(&["agent"]);
        config.home = std::path::PathBuf::from("/srv/nexus/acp-home");
        let provider = AcpProvider::new(config);
        let policy = provider.env_policy(&[]);
        let command = crate::transport::spawn::isolated_command("agent", &policy);
        let home = command
            .as_std()
            .get_envs()
            .find(|(name, _)| *name == "HOME")
            .and_then(|(_, value)| value);
        assert_eq!(home, Some(std::ffi::OsStr::new("/srv/nexus/acp-home")));
    }

    fn config(command: &[&str]) -> AcpConfig {
        AcpConfig::new(
            "acp-test",
            command.iter().map(|part| (*part).to_owned()).collect(),
        )
    }

    #[test]
    fn a_command_needs_a_program_and_no_secret_on_argv() {
        assert!(config(&["opencode", "acp"]).validate().is_ok());
        assert_eq!(
            config(&[]).validate().unwrap_err().kind(),
            "invalid_request"
        );
        assert_eq!(
            config(&[" "]).validate().unwrap_err().kind(),
            "invalid_request"
        );
        let error = config(&["agent", "--api-key=sk-leak-leak-leak-leak"])
            .validate()
            .unwrap_err();
        assert_eq!(error.kind(), "invalid_request");
        assert!(!error.to_string().contains("leak"));
        let error = config(&["agent", "--token", "Bearer abcdefghijklmnop"])
            .validate()
            .unwrap_err();
        assert!(!error.to_string().contains("abcdefghijklmnop"));
    }

    #[test]
    fn mcp_servers_keep_their_secrets_in_the_wire_json_only() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "po".to_owned(),
            McpServerSpec::Stdio {
                command: "/bin/po-mcp".to_owned(),
                args: vec!["--stdio".to_owned()],
                env: BTreeMap::from([("NEO4J_PASSWORD".to_owned(), "db-pass-value".to_owned())]),
            },
        );
        servers.insert(
            "remote".to_owned(),
            McpServerSpec::Http {
                url: "https://mcp.example/api".to_owned(),
                headers: BTreeMap::from([(
                    "Authorization".to_owned(),
                    "Bearer tok-value".to_owned(),
                )]),
            },
        );
        let wired = mcp_servers_wire(&servers).unwrap();
        let json = serde_json::to_value(&wired).unwrap();
        assert_eq!(
            json[0]["env"],
            json!([{"name":"NEO4J_PASSWORD","value":"db-pass-value"}])
        );
        assert_eq!(json[1]["type"], "http");
        assert_eq!(
            json[1]["headers"],
            json!([{"name":"Authorization","value":"Bearer tok-value"}])
        );
        // Neither the debug of the servers nor an error carries a value.
        let debug = format!("{wired:?}");
        assert!(!debug.contains("db-pass-value") && !debug.contains("tok-value"));
        let leaky = BTreeMap::from([(
            "s".to_owned(),
            McpServerSpec::Http {
                url: "https://user:pw-leak-value@h/mcp".to_owned(),
                headers: BTreeMap::new(),
            },
        )]);
        let error = mcp_servers_wire(&leaky).unwrap_err();
        assert_eq!(error.kind(), "invalid_request");
        assert!(!error.to_string().contains("leak"));
    }

    #[test]
    fn capabilities_say_only_what_is_known() {
        let provider = AcpProvider::new(config(&["agent"]));
        let capabilities = provider.capabilities(None);
        assert!(!capabilities.resume && !capabilities.thinking && !capabilities.images);
        assert!(capabilities.interactive_permissions && capabilities.per_session_mcp);
        assert_eq!(capabilities.context_window, None);
        assert_eq!(capabilities.cost, CostBasis::Unknown);
        assert!(!capabilities.tool_cancel && !capabilities.background_tasks);
    }
}
