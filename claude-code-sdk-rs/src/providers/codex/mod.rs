//! Codex behind the agent contract: [`CodexProvider`] drives `codex app-server`
//! (JSON-RPC 2.0 without the `jsonrpc` header, JSONL on stdio), **stable surface
//! only** — `capabilities.experimentalApi` is never sent.
//!
//! Compiled with the cargo feature `provider-codex`.
//!
//! | File | Role |
//! |---|---|
//! | `mod.rs` | [`CodexProvider`], [`CodexConfig`], capabilities, `health`, `open` / `resume`, MCP configuration |
//! | `wire.rs` | the JSON-RPC types and the `validate_kind` check the versioned schema is tested with |
//! | `transport.rs` | the process (single launcher), id correlation, killing the tree |
//! | `map.rs` | pure projection of notifications and server requests onto `AgentEvent`s |
//! | `session.rs` | [`CodexSession`]: pump, routing, permissions, turn lifecycle |
//!
//! # What is **not** established
//!
//! No real `codex app-server` was ever run: the `codex` installed where this was
//! written is 0.38.0, which has no `app-server`. Everything comes from the README of
//! `codex-rs/app-server` at tag `rust-v0.130.0` and from `docs/…/review-5`, and is
//! exercised against `fake_codex`, a fake executable replaying JSONL transcripts
//! (`tests/transcripts/codex/<version>/`). Marked **NOT VERIFIED** where they occur
//! (source comments, schema, fixtures) and listed here:
//!
//! - the exact spelling of `approvalPolicy` (`on-request`) and `sandbox`
//!   (`workspaceWrite`) accepted by `thread/start` (the README shows both camelCase
//!   and the task brief says kebab-case for the policy);
//! - the fields of `thread/tokenUsage/updated` (`total`, `last`, `modelContextWindow`);
//! - `thread/start.baseInstructions` / `developerInstructions` (system prompt);
//! - the `_meta` of an MCP approval elicitation (`tool_name`/`tool_title`,
//!   `tool_params`, `persist`) and how a persistent approval is answered
//!   (`_meta.persist` in the response);
//! - that `-c key=value` placed before the `app-server` subcommand is honoured;
//! - that the server accepts `CODEX_API_KEY` from its environment;
//! - that the events of a sub-agent thread reach this client (`collabToolCall`);
//! - that `<CODEX_HOME>/auth.json` is where the login lives (used by `health`).
//!
//! # Capabilities (per model, contract §5)
//!
//! | Field | Value |
//! |---|---|
//! | `interactive_permissions` | yes; scopes `once`, `session`, `always` (a request offers the ones the server advertises: command/file approvals `once` + `session`, MCP elicitations `persist`) |
//! | `sandbox` | `workspace` (`workspaceWrite`; `plan_only` is `readOnly`; `dangerFullAccess` is never sent) |
//! | `secret_isolation` | yes: allowlisted environment, MCP credentials by variable *name* (`env_vars`, `bearer_token_env_var`, `env_http_headers`), never on argv |
//! | `per_session_mcp` | yes: one process per session, servers given by `-c mcp_servers.<name>.…` |
//! | `hooks` | `none`: Codex hooks are command files, v1 has no relay executable (A40); `SessionSpec::hooks` is ignored with `provider_notice { hooks_not_supported }` first on `out_of_band()` |
//! | `subagents` | `separate_thread`: the README documents `collabToolCall` and events keyed by `threadId`; the adapter gives `parent` = the child thread id. Delivery of the child's events is NOT VERIFIED |
//! | `compaction_signal` | yes (`contextCompaction` item), trigger `auto` |
//! | `thinking` | yes (`reasoning` items and deltas); a model that emits none just emits no `thinking` |
//! | `images` | no (A12) |
//! | `tools` | yes (Codex's own tools and the session's MCP servers) |
//! | `context_window` | the configured one (`configured`); never a default. The server's `modelContextWindow` is not read into the frozen snapshot |
//! | `set_model_live`, `resume` | yes: the model is a sticky `turn/start` override; the token is `{"thread_id"}` |
//! | `native_question` | no: the stable surface has no user-input request (`item/tool/requestUserInput` is experimental); no `question` is emitted and the backend synthesises it (§5) |
//! | `tool_cancel`, `background_tasks` | no: `interrupt` is the only cut |
//! | `cost` | from the configuration: `unknown` (default), `free`, `priced`; never `reported` (tokens only, A40) |
//!
//! # MCP servers of a session
//!
//! `SessionSpec::mcp_servers` reach the session's own process as `-c
//! mcp_servers.<name>.<key>=<TOML>` arguments, **not** as a `config.toml`: the
//! `CODEX_HOME` of an instance is shared and persistent (login, history), sessions
//! run concurrently, and an ephemeral file there would race and could outlive a
//! crash. Nothing secret is on argv: a stdio server's `env` values are put in the
//! process environment and named in `env_vars`; an `Authorization: Bearer …` header
//! goes to a variable named by `bearer_token_env_var`, any other header to one named
//! in `env_http_headers`. A server `args` entry or URL that looks like a credential
//! is refused (`invalid_request`), a legacy SSE server is `Unsupported { mcp_sse }`.
//!
//! # Limits and policy
//!
//! `limits.turn_timeout_ms` is honoured (`done { error: timeout }`). The other
//! limits (`max_turns`, `max_tokens`, `max_cost_usd`, `max_tool_iterations`) have no
//! counterpart in Codex and are refused (`Unsupported { limits }`). The `allow` and
//! `deny` patterns of the policy apply to *approval requests* only: a command Codex
//! runs inside its sandbox without asking never reaches them (a
//! `provider_notice { policy_patterns_partial }` says so when patterns are given).
//!
//! # Deviations from the written contract
//!
//! Listed in `docs/agent-contract.md` §5 (Codex paragraph).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::{
    AgentEvent, AgentProvider, AgentSession, Capabilities, ContextWindow, ContextWindowSource,
    CostBasis, CredentialRef, CredentialResolver, EnvCredentialResolver, McpServerSpec,
    McpServerStatus, ModelInfo, PermissionScope, ProviderError, ProviderHealth, ProviderKind,
    ResumeToken, SandboxLevel, SessionSpec, SubagentSupport, SystemPromptMode, redact,
};
use crate::model::PriceTable;
use crate::transport::spawn::EnvPolicy;

pub mod map;
pub mod session;
pub mod transport;
pub mod wire;

pub use session::{CodexSession, policy_axes};

use transport::{Launch, Process};
use wire::{
    InitializeParams, InitializeResult, ThreadResult, ThreadResumeParams, ThreadStartParams,
};

/// Oldest `codex` this adapter drives. **0.130.0** is the tag whose
/// `app-server` README (stable surface, no "experimental" label on the server
/// itself) the wire types and the versioned schema were written from; anything
/// older is untested and unverified, and `codex 0.38.0` — the version found
/// installed — has no `app-server` at all. Raise it when a real session shows a
/// later floor.
pub const MIN_APP_SERVER_VERSION: &str = "0.130.0";

/// Environment variable naming the Codex home of a process.
pub const CODEX_HOME_ENV: &str = "CODEX_HOME";

/// Environment variable carrying an API key to `codex` (resolved per `open`).
pub const CODEX_API_KEY_ENV: &str = "CODEX_API_KEY";

/// Timeout of the handshake requests (`initialize`, `thread/start`).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout of `codex --version`.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Configuration of one Codex instance.
#[derive(Clone)]
pub struct CodexConfig {
    /// Identifier of the instance (registry key).
    pub instance_id: String,
    /// The `codex` program: a name looked up on `PATH`, or a path.
    pub program: PathBuf,
    /// `CODEX_HOME` of the instance: login, history and rollouts live there, so it
    /// is persistent and **per instance**, never per session. Created `0700`.
    pub codex_home: PathBuf,
    /// Model of a session that names none (`None`: Codex's own configured one).
    pub default_model: Option<String>,
    /// Context window of every model, when the operator knows it.
    pub context_window: Option<u64>,
    /// Where `done.cost` comes from: `Unknown` (default), `Free` or `Priced`.
    /// Anything else (notably `Reported`) is read as `Unknown`: Codex reports tokens only.
    pub cost_basis: CostBasis,
    /// Prices by model, for `Priced`.
    pub prices: PriceTable,
    /// Host variables the process inherits on top of the base allowlist.
    pub env_inherit: Vec<String>,
    /// Variables set explicitly on every process of the instance, `--version`
    /// included. Not for secrets.
    pub env_set: BTreeMap<String, String>,
    /// Where the API key lives, when the instance uses one (`CODEX_API_KEY`).
    pub credential: CredentialRef,
    /// Models `catalog()` lists besides the default one (Codex's own `model/list`
    /// needs a process and a login).
    pub models: Vec<String>,
}

impl std::fmt::Debug for CodexConfig {
    /// The instance's explicit environment may hold an API key an operator put there:
    /// only the NAMES and the length of each value are printed, never a value.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexConfig")
            .field("instance_id", &self.instance_id)
            .field("program", &self.program)
            .field("codex_home", &self.codex_home)
            .field("default_model", &self.default_model)
            .field("context_window", &self.context_window)
            .field("cost_basis", &self.cost_basis)
            .field("prices", &self.prices)
            .field("env_inherit", &self.env_inherit)
            .field("env_set", &crate::agent::spec::redacted_map(&self.env_set))
            .field("credential", &self.credential)
            .field("models", &self.models)
            .finish()
    }
}

impl CodexConfig {
    /// A configuration with the defaults: `codex` on `PATH`, a home under the
    /// user's data directory named after the instance, cost unknown.
    pub fn new(instance_id: impl Into<String>) -> Self {
        let instance_id = instance_id.into();
        Self {
            codex_home: default_codex_home(&instance_id),
            instance_id,
            program: PathBuf::from("codex"),
            default_model: None,
            context_window: None,
            cost_basis: CostBasis::Unknown,
            prices: PriceTable::new(),
            env_inherit: Vec::new(),
            env_set: BTreeMap::new(),
            credential: CredentialRef::None,
            models: Vec::new(),
        }
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

/// `<user data dir>/nexus/codex/<instance>`, or under the temp dir without a data dir.
pub fn default_codex_home(instance_id: &str) -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("nexus")
        .join("codex")
        .join(instance_id)
}

/// A numeric `major.minor.patch`, pre-release suffix ignored.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    text.split_whitespace().find_map(|word| {
        let core = word.trim_start_matches('v').split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        parts.next().is_none().then_some((major, minor, patch))
    })
}

/// `serde_json::to_value` of a wire type (plain structs of strings, options and unit
/// enums: it always serialises).
fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn quote_for_shell(path: &Path) -> String {
    let text = path.display().to_string();
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.' | ':'))
    {
        text
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// The Codex provider: one instance of `codex app-server`, one process per session.
pub struct CodexProvider {
    config: Arc<CodexConfig>,
    resolver: Arc<dyn CredentialResolver>,
}

impl std::fmt::Debug for CodexProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexProvider")
            .field("instance_id", &self.config.instance_id)
            .finish_non_exhaustive()
    }
}

impl CodexProvider {
    /// A provider that reads `env:` credentials from the process environment.
    pub fn new(config: CodexConfig) -> Self {
        Self::with_resolver(config, Arc::new(EnvCredentialResolver))
    }

    /// A provider that asks `resolver` for its credential, per `open`.
    pub fn with_resolver(config: CodexConfig, resolver: Arc<dyn CredentialResolver>) -> Self {
        Self {
            config: Arc::new(config),
            resolver,
        }
    }

    /// The configuration of the instance.
    pub fn config(&self) -> &CodexConfig {
        &self.config
    }

    fn login_command(&self) -> String {
        format!(
            "{}={} codex login",
            CODEX_HOME_ENV,
            quote_for_shell(&self.config.codex_home)
        )
    }

    /// The dedicated `HOME` of the instance (decision A33): Codex keeps its state in
    /// `CODEX_HOME`, and nothing else of the host user's dotfiles or credentials is
    /// reachable through `HOME`.
    fn instance_home(&self) -> std::path::PathBuf {
        self.config.codex_home.join("home")
    }

    fn base_policy(&self, extra: &[String]) -> EnvPolicy {
        EnvPolicy::allowlist()
            .with_inherited(self.config.env_inherit.iter().cloned())
            .with_inherited(extra.iter().cloned())
            .with_home(self.instance_home())
    }

    /// The instance's explicit variables: its own, then `CODEX_HOME` (which nothing
    /// may override).
    fn instance_env(&self) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = self
            .config
            .env_set
            .iter()
            .filter(|(name, _)| {
                name.as_str() != CODEX_HOME_ENV && name.as_str() != CODEX_API_KEY_ENV
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        env.push((
            CODEX_HOME_ENV.to_owned(),
            self.config.codex_home.display().to_string(),
        ));
        env
    }

    /// Creates the instance's `CODEX_HOME` `0700` (and tightens an existing one).
    fn ensure_home(&self) -> Result<(), ProviderError> {
        let home = &self.config.codex_home;
        create_private_dir(home).map_err(|error| {
            ProviderError::protocol(format!(
                "CODEX_HOME could not be created: {:?}",
                error.kind()
            ))
        })?;
        create_private_dir(&self.instance_home()).map_err(|error| {
            ProviderError::protocol(format!(
                "the instance HOME could not be created: {:?}",
                error.kind()
            ))
        })
    }

    async fn run_version(&self) -> Result<String, ProviderError> {
        let launch = Launch {
            program: self.config.program.clone(),
            args: vec!["--version".to_owned()],
            env_policy: self.base_policy(&[]),
            env: self.instance_env(),
            cwd: std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()),
        };
        let mut command = transport::command(&launch);
        command
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = match tokio::time::timeout(VERSION_TIMEOUT, command.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => return Err(transport::spawn_error(&error, &self.config.program)),
            Err(_) => {
                return Err(ProviderError::Timeout {
                    after_ms: u64::try_from(VERSION_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
                });
            },
        };
        if !output.status.success() {
            return Err(ProviderError::ProcessExited {
                code: output.status.code(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// Whether somebody is logged in: a configured credential that resolves, or an
    /// `auth.json` in the instance's home (NOT VERIFIED as the login's location).
    async fn logged_in(&self) -> Result<bool, ProviderError> {
        match self
            .resolver
            .resolve(&self.config.instance_id, &self.config.credential)
            .await
        {
            Ok(Some(_)) => return Ok(true),
            Ok(None) => {},
            Err(ProviderError::CredentialsLocked) => return Err(ProviderError::CredentialsLocked),
            Err(_) => {},
        }
        Ok(self.config.codex_home.join("auth.json").is_file())
    }

    async fn build(
        &self,
        spec: SessionSpec,
        resume: Option<String>,
    ) -> Result<Arc<dyn AgentSession>, ProviderError> {
        // Refusals that need nothing from the disk, the network or a process.
        spec.validate()?;
        if spec.max_turns.is_some()
            || spec.limits.max_tokens.is_some()
            || spec.limits.max_cost_usd.is_some()
            || spec.limits.max_tool_iterations.is_some()
        {
            return Err(ProviderError::unsupported("limits"));
        }
        let mcp = mcp_launch(&spec.mcp_servers)?;
        for name in spec.env.set.keys() {
            if name == CODEX_HOME_ENV || name == CODEX_API_KEY_ENV {
                return Err(ProviderError::invalid(
                    "SessionSpec::env must not set CODEX_HOME or CODEX_API_KEY",
                ));
            }
        }
        if !spec.cwd.is_dir() {
            return Err(ProviderError::invalid("cwd is not a directory"));
        }
        let model = spec
            .model
            .clone()
            .or_else(|| self.config.default_model.clone());
        let secret = self
            .resolver
            .resolve(&self.config.instance_id, &self.config.credential)
            .await?;
        self.ensure_home()?;

        // Environment: the allowlist, then explicit variables only.
        let mut env = self.instance_env();
        env.extend(spec.env.set.iter().map(|(n, v)| (n.clone(), v.clone())));
        env.extend(mcp.env.iter().map(|(n, v)| (n.clone(), v.clone())));
        if let Some(secret) = &secret {
            env.push((CODEX_API_KEY_ENV.to_owned(), secret.expose().to_owned()));
        }
        let mut args = Vec::new();
        for override_ in &mcp.overrides {
            args.push("-c".to_owned());
            args.push(override_.clone());
        }
        args.push("app-server".to_owned());
        let launch = Launch {
            program: self.config.program.clone(),
            args,
            env_policy: self.base_policy(&spec.env.inherit),
            env,
            cwd: spec.cwd.clone(),
        };
        let (process, inbound) = Process::spawn(&launch)?;
        match self.handshake(&process, &spec, model.clone(), resume).await {
            Ok(thread) => {
                let (approval, sandbox) = session::policy_axes(spec.policy.mode);
                let mut initial_events = Vec::new();
                if spec.hooks.is_some() {
                    initial_events.push(AgentEvent::ProviderNotice {
                        kind: "hooks_not_supported".to_owned(),
                        data: json!({ "provider": "codex" }),
                    });
                }
                if !spec.policy.allow.is_empty() || !spec.policy.deny.is_empty() {
                    initial_events.push(AgentEvent::ProviderNotice {
                        kind: "policy_patterns_partial".to_owned(),
                        data: json!({ "applies_to": "approval requests" }),
                    });
                }
                let effective_model = thread.model.clone().or_else(|| model.clone());
                initial_events.push(AgentEvent::SessionStarted {
                    provider_session_id: Some(thread.thread.id.clone()),
                    model: effective_model.clone(),
                    policy_mode: Some(spec.policy.mode),
                    native_mode: Some(format!(
                        "{}/{}",
                        serde_json::to_value(approval)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_default(),
                        serde_json::to_value(sandbox)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_default()
                    )),
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
                        thread_id: thread.thread.id,
                        cwd: spec.cwd.display().to_string(),
                        extra_dirs: spec
                            .extra_dirs
                            .iter()
                            .map(|dir| dir.display().to_string())
                            .collect(),
                        limits: spec.limits,
                        ceiling: spec.policy_ceiling.clone(),
                        policy: spec.policy.clone(),
                        model: effective_model,
                        deltas: spec.deltas,
                        login_hint: Some(self.login_command()),
                        initial_events,
                    },
                    inbound,
                );
                Ok(Arc::new(CodexSession::new(core)))
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
        spec: &SessionSpec,
        model: Option<String>,
        resume: Option<String>,
    ) -> Result<ThreadResult, ProviderError> {
        let initialize = process
            .request(
                "initialize",
                to_value(&InitializeParams::for_adapter()),
                HANDSHAKE_TIMEOUT,
            )
            .await?;
        serde_json::from_value::<InitializeResult>(initialize)
            .map_err(|_| ProviderError::protocol("malformed `initialize` result"))?;
        process.notify("initialized", json!({})).await?;
        let (approval, sandbox) = session::policy_axes(spec.policy.mode);
        let cwd = spec.cwd.display().to_string();
        let (method, params) = match resume {
            Some(thread_id) => (
                "thread/resume",
                to_value(&ThreadResumeParams {
                    thread_id,
                    cwd: Some(cwd),
                    model,
                    approval_policy: Some(approval),
                    sandbox: Some(sandbox),
                }),
            ),
            None => {
                let (base, developer) = match &spec.system_prompt {
                    Some(prompt) if prompt.mode == SystemPromptMode::Append => {
                        (None, Some(prompt.text.clone()))
                    },
                    Some(prompt) => (Some(prompt.text.clone()), None),
                    None => (None, None),
                };
                (
                    "thread/start",
                    to_value(&ThreadStartParams {
                        cwd,
                        model,
                        approval_policy: approval,
                        sandbox,
                        base_instructions: base,
                        developer_instructions: developer,
                    }),
                )
            },
        };
        let result = process.request(method, params, HANDSHAKE_TIMEOUT).await?;
        serde_json::from_value::<ThreadResult>(result)
            .map_err(|_| ProviderError::protocol(format!("malformed `{method}` result")))
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // A directory that already existed keeps its mode through `create`: tighten it.
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[async_trait]
impl AgentProvider for CodexProvider {
    fn id(&self) -> &str {
        &self.config.instance_id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    async fn health(&self) -> ProviderHealth {
        let output = match self.run_version().await {
            Ok(output) => output,
            Err(error) => return ProviderHealth::unavailable(error),
        };
        let Some(found) = parse_version(&output) else {
            return ProviderHealth::unavailable(ProviderError::protocol(
                "`codex --version` printed no version",
            ));
        };
        let version = format!("{}.{}.{}", found.0, found.1, found.2);
        let minimum = parse_version(MIN_APP_SERVER_VERSION).unwrap_or((0, 0, 0));
        if found < minimum {
            let mut health = ProviderHealth::unavailable(ProviderError::unsupported("app_server"));
            health.version = Some(version.clone());
            health.detail = Some(format!(
                "codex {version} has no stable `app-server`: {MIN_APP_SERVER_VERSION} or later is needed; update codex"
            ));
            return health;
        }
        match self.logged_in().await {
            Ok(true) => ProviderHealth::ok(Some(version)),
            Ok(false) => {
                let mut health = ProviderHealth::unavailable(ProviderError::AuthRequired {
                    login_hint: Some(self.login_command()),
                });
                health.version = Some(version);
                health.detail =
                    Some("codex is not logged in; a human runs the login command".to_owned());
                health
            },
            Err(error) => {
                let mut health = ProviderHealth::unavailable(error);
                health.version = Some(version);
                health
            },
        }
    }

    async fn catalog(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        // The models the configuration names; `model/list` needs a process and a
        // login, and returns no context window anyway.
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
                info.supports_thinking = Some(true);
                info
            })
            .collect())
    }

    fn capabilities(&self, model: Option<&str>) -> Capabilities {
        let model = model.or(self.config.default_model.as_deref());
        let mut capabilities = Capabilities::none();
        capabilities.interactive_permissions = true;
        capabilities.permission_scopes = vec![
            PermissionScope::Once,
            PermissionScope::Session,
            PermissionScope::Always,
        ];
        capabilities.sandbox = SandboxLevel::Workspace;
        capabilities.secret_isolation = true;
        capabilities.per_session_mcp = true;
        capabilities.subagents = SubagentSupport::SeparateThread;
        capabilities.compaction_signal = true;
        capabilities.thinking = true;
        capabilities.tools = true;
        capabilities.set_model_live = true;
        capabilities.resume = true;
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
        let data = token.expect_kind(ProviderKind::Codex)?;
        let thread_id = data
            .get("thread_id")
            .and_then(Value::as_str)
            .filter(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.'))
            })
            .ok_or_else(|| ProviderError::invalid("the resume token carries no thread id"))?
            .to_owned();
        self.build(spec, Some(thread_id)).await
    }
}

// ---------------------------------------------------------------------------
// MCP servers → `-c` overrides
// ---------------------------------------------------------------------------

/// What the MCP servers of a session add to the launch.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct McpLaunch {
    /// `key=value` arguments of `-c`, no secret in any of them.
    pub overrides: Vec<String>,
    /// Variables to set in the process environment (the secrets), by name.
    pub env: BTreeMap<String, String>,
}

fn toml_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn toml_array(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|item| toml_string(item)).collect();
    format!("[{}]", inner.join(","))
}

fn env_name(parts: &[&str]) -> String {
    parts
        .join("_")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Builds the `-c` overrides and the secret environment of the MCP servers.
///
/// Refuses (`invalid_request`, nothing echoed): a server name that is not
/// `[A-Za-z0-9_-]`, an `args` entry or a URL that looks like a credential, a URL
/// with user-info, a header name that is not a token, two stdio servers giving the
/// same variable different values. A legacy SSE server is `Unsupported { mcp_sse }`.
pub fn mcp_launch(servers: &BTreeMap<String, McpServerSpec>) -> Result<McpLaunch, ProviderError> {
    let bad = |detail: &str| Err(ProviderError::invalid(detail));
    let mut launch = McpLaunch::default();
    for (name, server) in servers {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        {
            return bad("an MCP server name is made of [A-Za-z0-9_-]");
        }
        let key = |field: &str| format!("mcp_servers.{name}.{field}");
        match server {
            McpServerSpec::Stdio { command, args, env } => {
                if command.is_empty() {
                    return bad("an MCP server needs a command");
                }
                if args.iter().any(|arg| redact(arg) != *arg) {
                    return bad(
                        "an MCP server argument looks like a credential (or is too long): pass secrets in the environment",
                    );
                }
                launch
                    .overrides
                    .push(format!("{}={}", key("command"), toml_string(command)));
                if !args.is_empty() {
                    launch
                        .overrides
                        .push(format!("{}={}", key("args"), toml_array(args)));
                }
                if !env.is_empty() {
                    let names: Vec<String> = env.keys().cloned().collect();
                    for name in &names {
                        if name.is_empty()
                            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                        {
                            return bad("an MCP environment variable name is made of [A-Za-z0-9_]");
                        }
                    }
                    for (variable, value) in env {
                        match launch.env.get(variable) {
                            Some(existing) if existing != value => {
                                return bad("two MCP servers give one variable different values");
                            },
                            _ => {
                                launch.env.insert(variable.clone(), value.clone());
                            },
                        }
                    }
                    launch
                        .overrides
                        .push(format!("{}={}", key("env_vars"), toml_array(&names)));
                }
            },
            McpServerSpec::Http { url, headers } => {
                if redact(url) != *url || url.contains('@') {
                    return bad("an MCP URL must carry no credential: use a header");
                }
                launch
                    .overrides
                    .push(format!("{}={}", key("url"), toml_string(url)));
                for (index, (header, value)) in headers.iter().enumerate() {
                    if header.is_empty()
                        || !header
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-')
                    {
                        return bad("an MCP header name is made of [A-Za-z0-9-]");
                    }
                    let bearer = header
                        .eq_ignore_ascii_case("authorization")
                        .then(|| {
                            value
                                .strip_prefix("Bearer ")
                                .or_else(|| value.strip_prefix("bearer "))
                        })
                        .flatten();
                    if let Some(token) = bearer {
                        let variable = env_name(&["NEXUS_MCP", name, "BEARER"]);
                        launch.overrides.push(format!(
                            "{}={}",
                            key("bearer_token_env_var"),
                            toml_string(&variable)
                        ));
                        launch.env.insert(variable, token.to_owned());
                    } else {
                        let variable = env_name(&["NEXUS_MCP", name, "HEADER", &index.to_string()]);
                        launch.overrides.push(format!(
                            "{}={}",
                            key(&format!("env_http_headers.{header}")),
                            toml_string(&variable)
                        ));
                        launch.env.insert(variable, value.clone());
                    }
                }
            },
            McpServerSpec::Sse { .. } => return Err(ProviderError::unsupported("mcp_sse")),
            #[allow(unreachable_patterns)]
            _ => return Err(ProviderError::unsupported("mcp_transport")),
        }
    }
    Ok(launch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_debug_of_a_config_never_prints_an_environment_value() {
        let mut config = CodexConfig::new("codex-debug");
        config.env_set.insert(
            "SOME_API_KEY".to_owned(),
            "sk-live-0123456789abcdef".to_owned(),
        );
        let shown = format!("{config:?}");
        assert!(!shown.contains("sk-live-0123456789abcdef"), "{shown}");
        assert!(shown.contains("SOME_API_KEY"), "the name stays: {shown}");
    }

    /// Decision A33: Codex runs with a HOME of its own inside the instance's state.
    #[test]
    fn codex_runs_with_a_dedicated_home_inside_codex_home() {
        let mut config = CodexConfig::new("codex-home");
        config.codex_home = std::path::PathBuf::from("/srv/nexus/codex-state");
        let provider = CodexProvider::new(config);
        let policy = provider.base_policy(&[]);
        let command = crate::transport::spawn::isolated_command("codex", &policy);
        let home = command
            .as_std()
            .get_envs()
            .find(|(name, _)| *name == "HOME")
            .and_then(|(_, value)| value);
        assert_eq!(
            home,
            Some(std::ffi::OsStr::new("/srv/nexus/codex-state/home"))
        );
    }

    #[test]
    fn versions_are_read_from_the_cli_banner() {
        assert_eq!(parse_version("codex-cli 0.38.0"), Some((0, 38, 0)));
        assert_eq!(
            parse_version("codex-cli 0.130.0-alpha.3"),
            Some((0, 130, 0))
        );
        assert_eq!(parse_version("0.160.0"), Some((0, 160, 0)));
        assert_eq!(parse_version("codex"), None);
        assert_eq!(parse_version("1.2"), None);
        assert!(parse_version("codex-cli 0.38.0") < parse_version(MIN_APP_SERVER_VERSION));
    }

    #[test]
    fn mcp_secrets_stay_off_the_command_line() {
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
                headers: BTreeMap::from([
                    (
                        "Authorization".to_owned(),
                        "Bearer tok-bearer-value".to_owned(),
                    ),
                    ("X-Tenant".to_owned(), "tenant-secret-value".to_owned()),
                ]),
            },
        );
        let launch = mcp_launch(&servers).unwrap();
        let argv = launch.overrides.join("\n");
        for secret in ["db-pass-value", "tok-bearer-value", "tenant-secret-value"] {
            assert!(!argv.contains(secret), "{secret} on argv");
            assert!(
                launch.env.values().any(|v| v == secret),
                "{secret} not in the env"
            );
        }
        assert!(argv.contains(r#"mcp_servers.po.env_vars=["NEO4J_PASSWORD"]"#));
        assert!(
            argv.contains(r#"mcp_servers.remote.bearer_token_env_var="NEXUS_MCP_REMOTE_BEARER""#)
        );
        assert!(argv.contains("mcp_servers.remote.env_http_headers.X-Tenant="));
    }

    #[test]
    fn mcp_refusals_do_not_echo_what_was_refused() {
        let stdio = |args: Vec<&str>| {
            BTreeMap::from([(
                "s".to_owned(),
                McpServerSpec::Stdio {
                    command: "x".to_owned(),
                    args: args.into_iter().map(str::to_owned).collect(),
                    env: BTreeMap::new(),
                },
            )])
        };
        let error = mcp_launch(&stdio(vec!["--token=sk-leak-leak-leak-leak"])).unwrap_err();
        assert_eq!(error.kind(), "invalid_request");
        assert!(!error.to_string().contains("leak"));
        let url = BTreeMap::from([(
            "s".to_owned(),
            McpServerSpec::Http {
                url: "https://user:pw-leak-value@h/mcp".to_owned(),
                headers: BTreeMap::new(),
            },
        )]);
        assert_eq!(mcp_launch(&url).unwrap_err().kind(), "invalid_request");
        let sse = BTreeMap::from([(
            "s".to_owned(),
            McpServerSpec::Sse {
                url: "https://h/sse".to_owned(),
                headers: BTreeMap::new(),
            },
        )]);
        assert_eq!(
            mcp_launch(&sse).unwrap_err(),
            ProviderError::unsupported("mcp_sse")
        );
        let named = BTreeMap::from([("a.b".to_owned(), McpServerSpec::stdio("x"))]);
        assert_eq!(mcp_launch(&named).unwrap_err().kind(), "invalid_request");
    }

    #[test]
    fn toml_strings_escape_what_breaks_a_value() {
        assert_eq!(toml_string(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(toml_string("l1\nl2"), r#""l1\nl2""#);
    }
}
