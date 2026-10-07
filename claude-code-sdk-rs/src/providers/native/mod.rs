//! The native harness: an [`AgentProvider`] composed on a [`ModelEndpoint`]
//! (decision A2) that runs the tool loop itself, with **MCP tools only**
//! (decision A35) — no shell, no file edit of its own.
//!
//! Compiled with the cargo feature `provider-native`.
//!
//! | File | Role |
//! |---|---|
//! | `mod.rs` | [`NativeProvider`], [`NativeConfig`], per-model capabilities, `open` / `resume` |
//! | `session.rs` | [`NativeSession`]: shared core, event routing, permissions, cancellation |
//! | `loop.rs` | the tool loop of a turn, budgets, terminal events |
//! | `mcp.rs` | the MCP client (stdio through `isolated_command`, streamable HTTP) |
//! | `tools.rs` | `mcp__<server>__<tool>` names and the exposure rule under a policy |
//! | `transcript.rs` | [`TranscriptStore`] (memory, `0600` files), reasoning kept (A39) |
//! | `compaction.rs` | summary of the old history by the same endpoint, recent messages intact |
//! | `cancel.rs` | the cancellation token |
//!
//! # Capabilities (per model, contract §5)
//!
//! | Field | Value |
//! |---|---|
//! | `tools` | the endpoint probe called a tool (`ModelNoTools` otherwise) |
//! | `thinking` | the probe saw a reasoning field |
//! | `context_window` | configured, else the probe's (`probed`), else the catalogue's (`catalog`, once `catalog()` was read); `None` if unknown: no automatic compaction |
//! | `cost` | `free` if configured so, `priced` when the model has a price, else `unknown` |
//! | `interactive_permissions` | yes; scopes `once` and `session` (`always` has nowhere to be kept) |
//! | `resume`, `set_model_live`, `per_session_mcp`, `compaction_signal`, `tool_cancel` | yes |
//! | `secret_isolation` | yes: stdio servers get an allowlisted environment, credentials are resolved per request and never stored |
//! | `sandbox` | none: information for the user, not a gate: `trust` opens like on every provider |
//! | `hooks` | `in_protocol`: `before_tool` (after the exposure check, before the policy, so a replaced input is judged too), `after_tool` (only for a call that ran; its text reaches the model, not the `tool_result` event) and `before_compaction` (its text joins the summary instructions). A hook that does not answer within `NativeConfig::hook_timeout` is skipped with `provider_notice { hook_timeout }` |
//! | `subagents`, `background_tasks`, `native_question`, `images` | no |
//!
//! `capabilities()` is synchronous: it reads what was probed. Call
//! [`NativeProvider::refresh_capabilities`] (or `catalog` then `open`) to fill it;
//! until a model is probed `tools` is `false` and nothing is claimed about it.
//! `open` probes a model that has not been.
//!
//! # Limits and budgets
//!
//! `SessionSpec::max_turns` and `limits.max_tool_iterations` bound the model round
//! trips of a turn (`done max_turns`). `limits.max_tokens` is a **session** budget
//! counted from the usage the endpoint reports (estimated at four characters per
//! token when it reports none) and works without a known context window.
//! `limits.max_cost_usd` needs a price: for a model without one `open` answers
//! `Unsupported { cost }` (decision A21: no budget on an invented price). Both end
//! the turn with `done budget_exceeded`, checked before every request.
//!
//! # Deviations from the written contract
//!
//! Listed in `docs/agent-contract.md` §5 (native paragraph).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;

use crate::agent::{
    AgentEvent, AgentProvider, AgentSession, Capabilities, ContextWindow, ContextWindowSource,
    CostBasis, HookSupport, McpServerSpec, McpServerStatus, ModelInfo, PermissionScope,
    ProviderError, ProviderHealth, ProviderKind, ResumeToken, SandboxLevel, SessionLimits,
    SessionSpec, SystemPromptSpec,
};
use crate::model::{ChatMessage, EndpointProbe, ModelEndpoint, PriceTable};

pub mod browser;
pub mod cancel;
pub mod compaction;
pub mod r#loop;
pub mod mcp;
pub(crate) mod policy_args;
pub mod session;
pub mod tools;
pub mod transcript;

pub use browser::{BROWSER_SERVER, BrowserTools};
pub use cancel::CancelToken;
pub use compaction::CompactionConfig;
pub use mcp::{McpClient, McpConfig, McpError, McpLaunch, McpTool};
pub use session::NativeSession;
pub use tools::{
    NEXUS_TOOLS_CATALOG, NEXUS_TOOLS_SERVER, ToolEntry, ToolRegistry, exposed_name,
    nexus_tools_bound,
};
pub use transcript::{
    FileTranscriptStore, MemoryTranscriptStore, TranscriptStore, new_transcript_id,
};

/// How the harness attaches `nexus-tools` to a session that names no `nexus` server (N24).
///
/// The server is the harness's own child, spoken to over a private pipe: `--trust-harness` tells
/// it so (it does not ask for a signed profile, the harness is the only client and enforces the
/// session's policy before any call). It gets the session's working directory and extra
/// directories as its scope and nothing else from the host: the MCP launch applies the empty
/// environment plus the allow-list (decision A33).
#[derive(Debug, Clone)]
pub struct DefaultTools {
    /// The `nexus-tools` executable.
    pub program: PathBuf,
    /// Extra arguments (for example `--search-engine searxng:URL`).
    pub args: Vec<String>,
    /// Environment of the server (for example the variable that holds a search key).
    pub env: BTreeMap<String, String>,
}

impl DefaultTools {
    /// `nexus-tools` at `program`.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    /// Looks for `nexus-tools` next to the running executable, then in the `PATH`.
    pub fn locate() -> Option<Self> {
        let name = format!("nexus-tools{}", std::env::consts::EXE_SUFFIX);
        let beside = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join(&name)))
            .filter(|path| path.is_file());
        let on_path = || {
            std::env::var_os("PATH").and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|dir| dir.join(&name))
                    .find(|path| path.is_file())
            })
        };
        beside.or_else(on_path).map(Self::new)
    }

    /// The MCP server entry for a session: one process per session, bounded at launch (N27).
    ///
    /// Its scope is the session's directories and its `--tools` the `nexus-tools` tools the
    /// session's policy can expose ([`tools::nexus_tools_bound`], `strict` being the instance's
    /// `strict_tool_exposure`): a tool the harness would never offer is not in the process at
    /// all, so a call that slipped past the harness still finds nothing to run. A `--tools` in
    /// [`DefaultTools::args`] narrows it further (the server intersects them).
    pub fn server_for(&self, spec: &SessionSpec, strict: bool) -> McpServerSpec {
        let bound = tools::nexus_tools_bound(&spec.policy, spec.policy_ceiling.as_ref(), strict);
        let mut args = vec![
            "--trust-harness".to_owned(),
            "--cwd".to_owned(),
            spec.cwd.display().to_string(),
        ];
        for dir in &spec.extra_dirs {
            args.push("--add-dir".to_owned());
            args.push(dir.display().to_string());
        }
        args.push("--tools".to_owned());
        args.push(bound.join(","));
        args.extend(self.args.iter().cloned());
        McpServerSpec::Stdio {
            command: self.program.display().to_string(),
            args,
            env: self.env.clone(),
        }
    }
}

/// Configuration of one native instance.
#[derive(Debug, Clone)]
pub struct NativeConfig {
    /// Identifier of the instance (registry key).
    pub instance_id: String,
    /// Model used when `SessionSpec::model` is unset.
    pub default_model: Option<String>,
    /// Context window to use for every model, when the operator knows it.
    pub context_window: Option<u64>,
    /// Prices, by model. A model absent from it has no cost (never zero).
    pub prices: PriceTable,
    /// `Free` for a local model; anything else lets the price table decide.
    pub cost_basis: CostBasis,
    /// Limits applied when the session's own are unset. **Not unlimited by
    /// default**: [`NativeConfig::new`] sets `turn_timeout_ms` to
    /// [`NativeConfig::DEFAULT_TURN_TIMEOUT_MS`] and `max_tokens` to
    /// [`NativeConfig::DEFAULT_MAX_TOKENS`]; set a field to `None` to lift it
    /// (an explicit choice of the operator, not an omission).
    pub limits: SessionLimits,
    /// Round trips to the model per turn, when `SessionSpec::max_turns` is unset;
    /// [`NativeConfig::DEFAULT_MAX_TURNS`] by default, `None` lifts it.
    pub max_turns: Option<u32>,
    /// Output cap of every request, in tokens.
    pub max_tokens: Option<u32>,
    /// Value of `parallel_tool_calls` sent with tools; `None` sends nothing.
    pub parallel_tool_calls: Option<bool>,
    /// An `allow` list is an exposure list (see [`tools`]). On by default.
    pub strict_tool_exposure: bool,
    /// Automatic compaction.
    pub compaction: CompactionConfig,
    /// MCP clients.
    pub mcp: McpConfig,
    /// The tools every session has unless it names its own `nexus` server. `None`: none.
    pub default_tools: Option<DefaultTools>,
    /// The optional browser (N23). Configuring it is the operator's authorisation; without the
    /// executable installed there is no `browser_*` tool and a `browser_unavailable` notice.
    pub browser: Option<BrowserTools>,
    /// How long a host hook may take before the harness goes on without it.
    pub hook_timeout: std::time::Duration,
}

impl NativeConfig {
    /// Default bound on the model round trips of a turn (a runaway tool loop).
    pub const DEFAULT_MAX_TURNS: u32 = 50;
    /// A hook that has not answered by then is skipped.
    pub const DEFAULT_HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    /// Default longest a turn may run: 30 minutes.
    pub const DEFAULT_TURN_TIMEOUT_MS: u64 = 30 * 60 * 1000;
    /// Default token budget of a session (input + output, reported or estimated).
    pub const DEFAULT_MAX_TOKENS: u64 = 10_000_000;

    /// A configuration with the defaults: strict exposure, compaction on, no
    /// price, no default model, bounded turns and budget (see the `DEFAULT_*`
    /// constants).
    pub fn new(instance_id: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
            default_model: None,
            context_window: None,
            prices: PriceTable::new(),
            cost_basis: CostBasis::Unknown,
            limits: SessionLimits {
                turn_timeout_ms: Some(Self::DEFAULT_TURN_TIMEOUT_MS),
                max_tokens: Some(Self::DEFAULT_MAX_TOKENS),
                ..SessionLimits::default()
            },
            max_turns: Some(Self::DEFAULT_MAX_TURNS),
            max_tokens: None,
            parallel_tool_calls: None,
            strict_tool_exposure: true,
            compaction: CompactionConfig::default(),
            mcp: McpConfig::default(),
            default_tools: None,
            browser: None,
            hook_timeout: Self::DEFAULT_HOOK_TIMEOUT,
        }
    }
}

/// The native harness over a model endpoint.
pub struct NativeProvider {
    config: Arc<NativeConfig>,
    endpoint: Arc<dyn ModelEndpoint>,
    store: Arc<dyn TranscriptStore>,
    facts: ModelFacts,
}

/// What the provider knows about each model: the probes, the catalogue and the
/// configuration. Shared with every session so that a model change in a live
/// session (`set_model`, `before_turn`) reads the facts of the model now active.
#[derive(Clone)]
pub(crate) struct ModelFacts {
    config: Arc<NativeConfig>,
    probes: Arc<Mutex<HashMap<String, EndpointProbe>>>,
    catalog: Arc<Mutex<Vec<ModelInfo>>>,
}

impl ModelFacts {
    fn new(config: Arc<NativeConfig>) -> Self {
        Self {
            config,
            probes: Arc::new(Mutex::new(HashMap::new())),
            catalog: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn record_probe(&self, model: &str, probe: EndpointProbe) {
        self.probes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(model.to_owned(), probe);
    }

    fn record_catalog(&self, models: Vec<ModelInfo>) {
        *self.catalog.lock().unwrap_or_else(PoisonError::into_inner) = models;
    }

    pub(crate) fn probed(&self, model: &str) -> Option<EndpointProbe> {
        self.probes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(model)
            .cloned()
    }

    pub(crate) fn context_window(
        &self,
        model: &str,
        probe: Option<&EndpointProbe>,
    ) -> Option<ContextWindow> {
        if let Some(value) = self.config.context_window {
            return Some(ContextWindow {
                value,
                source: ContextWindowSource::Configured,
            });
        }
        // The probe wins over the catalogue: reading the catalogue (`catalog()`)
        // must not change what `capabilities(model)` says once the model is probed.
        let probed = probe
            .and_then(|probe| probe.context_window)
            .map(|value| ContextWindow {
                value,
                source: ContextWindowSource::Probed,
            });
        probed.or_else(|| {
            self.catalog
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .find(|info| info.id == model)
                .and_then(|info| info.context_window)
        })
    }

    pub(crate) fn cost_basis(&self, model: &str) -> CostBasis {
        if self.config.cost_basis == CostBasis::Free {
            CostBasis::Free
        } else if self.config.prices.get(model).is_some() {
            CostBasis::Priced
        } else {
            CostBasis::Unknown
        }
    }

    /// The window and the cost basis that govern a turn on `model`: what was
    /// probed or catalogued. A model never seen is probed once, best effort
    /// (the endpoint caches it); a probe that fails leaves the window unknown,
    /// which means no automatic compaction, never an error for the caller.
    pub(crate) async fn active(
        &self,
        endpoint: &dyn ModelEndpoint,
        model: &str,
    ) -> (Option<u64>, CostBasis) {
        let mut probe = self.probed(model);
        if probe.is_none() && self.config.context_window.is_none() {
            match endpoint.probe(model).await {
                Ok(fresh) => {
                    self.record_probe(model, fresh.clone());
                    probe = Some(fresh);
                },
                Err(ProviderError::ModelNoTools { .. }) => {
                    let fresh = EndpointProbe {
                        tools: false,
                        parallel_tools: None,
                        reasoning_field: None,
                        context_window: None,
                        checked_at_ms: crate::agent::now_ms(),
                    };
                    self.record_probe(model, fresh.clone());
                    probe = Some(fresh);
                },
                Err(_) => {},
            }
        }
        (
            self.context_window(model, probe.as_ref()).map(|w| w.value),
            self.cost_basis(model),
        )
    }
}

impl std::fmt::Debug for NativeProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeProvider")
            .field("instance_id", &self.config.instance_id)
            .finish_non_exhaustive()
    }
}

impl NativeProvider {
    /// A provider on `endpoint`, with transcripts kept in memory.
    pub fn new(config: NativeConfig, endpoint: Arc<dyn ModelEndpoint>) -> Self {
        let config = Arc::new(config);
        Self {
            facts: ModelFacts::new(Arc::clone(&config)),
            config,
            endpoint,
            store: Arc::new(MemoryTranscriptStore::new()),
        }
    }

    /// Keeps transcripts in `store` instead of memory.
    pub fn with_transcript_store(mut self, store: Arc<dyn TranscriptStore>) -> Self {
        self.store = store;
        self
    }

    /// The configuration of the instance.
    pub fn config(&self) -> &NativeConfig {
        &self.config
    }

    /// Probes `model` (a tool call against the endpoint, cached by the endpoint)
    /// and returns the capabilities that result. A model that refuses tools is
    /// recorded as such; any other failure is returned and records nothing.
    pub async fn refresh_capabilities(&self, model: &str) -> Result<Capabilities, ProviderError> {
        let probe = match self.endpoint.probe(model).await {
            Ok(probe) => probe,
            Err(ProviderError::ModelNoTools { .. }) => EndpointProbe {
                tools: false,
                parallel_tools: None,
                reasoning_field: None,
                context_window: None,
                checked_at_ms: crate::agent::now_ms(),
            },
            Err(error) => return Err(error),
        };
        self.facts.record_probe(model, probe);
        Ok(self.capabilities(Some(model)))
    }

    fn probed(&self, model: &str) -> Option<EndpointProbe> {
        self.facts.probed(model)
    }

    fn context_window(&self, model: &str, probe: Option<&EndpointProbe>) -> Option<ContextWindow> {
        self.facts.context_window(model, probe)
    }

    fn cost_basis(&self, model: &str) -> CostBasis {
        self.facts.cost_basis(model)
    }

    async fn build(
        &self,
        spec: SessionSpec,
        resumed: Option<(String, Vec<ChatMessage>)>,
    ) -> Result<Arc<dyn AgentSession>, ProviderError> {
        // Refusals that need nothing from the network or the disk.
        spec.validate()?;
        if spec
            .mcp_servers
            .values()
            .any(|server| matches!(server, McpServerSpec::Sse { .. }))
        {
            return Err(ProviderError::unsupported("mcp_sse"));
        }
        let model = spec
            .model
            .clone()
            .or_else(|| self.config.default_model.clone())
            .ok_or_else(|| {
                ProviderError::invalid("no model: set `SessionSpec::model` or a default")
            })?;
        if self.probed(&model).is_none() {
            // A probe that fails for another reason than "no tools" matters only
            // when the session needs tools.
            if let Err(error) = self.refresh_capabilities(&model).await
                && !spec.mcp_servers.is_empty()
            {
                return Err(error);
            }
        }
        let capabilities = self.capabilities(Some(&model));
        if !spec.mcp_servers.is_empty() && !capabilities.tools {
            return Err(ProviderError::ModelNoTools { model });
        }
        // The default tools (N24): attached unless the session brought its own `nexus` server,
        // and only to a model that can call tools: a chat-only model keeps working as chat.
        let mut servers = spec.mcp_servers.clone();
        let mut notices = Vec::new();
        if let Some(browser) = &self.config.browser
            && !servers.contains_key(BROWSER_SERVER)
        {
            if !browser.is_installed() {
                notices.push(AgentEvent::ProviderNotice {
                    kind: "browser_unavailable".to_owned(),
                    data: serde_json::json!({ "reason": "executable_not_found" }),
                });
            } else if capabilities.tools {
                servers.insert(BROWSER_SERVER.to_owned(), browser.server());
            } else {
                notices.push(AgentEvent::ProviderNotice {
                    kind: "browser_unavailable".to_owned(),
                    data: serde_json::json!({ "reason": "model_no_tools" }),
                });
            }
        }
        if let Some(default) = &self.config.default_tools
            && !servers.contains_key(NEXUS_TOOLS_SERVER)
        {
            if capabilities.tools {
                servers.insert(
                    NEXUS_TOOLS_SERVER.to_owned(),
                    default.server_for(&spec, self.config.strict_tool_exposure),
                );
            } else {
                notices.push(AgentEvent::ProviderNotice {
                    kind: "default_tools_skipped".to_owned(),
                    data: serde_json::json!({ "reason": "model_no_tools", "model": model }),
                });
            }
        }
        let mut limits = spec.limits;
        limits.max_cost_usd = limits.max_cost_usd.or(self.config.limits.max_cost_usd);
        limits.max_tokens = limits.max_tokens.or(self.config.limits.max_tokens);
        limits.turn_timeout_ms = limits
            .turn_timeout_ms
            .or(self.config.limits.turn_timeout_ms);
        limits.max_tool_iterations = limits
            .max_tool_iterations
            .or(self.config.limits.max_tool_iterations);
        if limits.max_cost_usd.is_some() && capabilities.cost == CostBasis::Unknown {
            return Err(ProviderError::unsupported("cost"));
        }

        let home = (self.config.mcp.isolated_home
            && servers
                .values()
                .any(|server| matches!(server, McpServerSpec::Stdio { .. })))
        .then(|| mcp::isolated_home(&self.config.instance_id))
        .flatten();
        let launch = McpLaunch {
            cwd: spec.cwd.clone(),
            env: spec.env.clone(),
            home,
        };
        let mut clients: HashMap<String, McpClient> = HashMap::new();
        let mut registry = ToolRegistry::new();
        let mut statuses = Vec::new();
        let connected = async {
            for (name, server) in &servers {
                let client = McpClient::connect(name, server, &launch, &self.config.mcp).await?;
                let listed = client.list_tools().await;
                clients.insert(name.clone(), client);
                for tool in listed? {
                    registry.insert(ToolEntry::mcp(
                        name,
                        &tool.name,
                        tool.description,
                        tool.input_schema,
                        tool.read_only,
                    ))?;
                }
                statuses.push(McpServerStatus {
                    name: name.clone(),
                    status: "connected".to_owned(),
                });
            }
            Ok::<(), ProviderError>(())
        }
        .await;
        if let Err(error) = connected {
            for client in clients.values() {
                client.close().await;
            }
            return Err(error);
        }

        let (transcript_id, messages) = match resumed {
            Some(found) => found,
            None => (new_transcript_id(), Vec::new()),
        };
        let mut initial_events = notices;
        initial_events.push(AgentEvent::SessionStarted {
            provider_session_id: Some(transcript_id.clone()),
            model: Some(model.clone()),
            policy_mode: Some(spec.policy.mode),
            native_mode: spec.policy.native_mode.clone(),
            tools: registry
                .exposed(&spec.policy, self.config.strict_tool_exposure)
                .into_iter()
                .map(|entry| entry.name.clone())
                .collect(),
            mcp_servers: statuses,
            cwd: Some(spec.cwd.display().to_string()),
        });
        let SessionSpec {
            cwd,
            system_prompt,
            policy,
            policy_ceiling,
            max_turns,
            deltas,
            hooks,
            ..
        } = spec;
        let max_turns = max_turns.or(self.config.max_turns);
        let core = session::Core::new(session::CoreParts {
            capabilities,
            facts: self.facts.clone(),
            endpoint: Arc::clone(&self.endpoint),
            settings: Arc::clone(&self.config),
            registry,
            mcp: clients,
            system_prompt: system_prompt.map(|SystemPromptSpec { text, .. }| text),
            deltas,
            max_turns,
            limits,
            ceiling: policy_ceiling,
            cwd,
            hooks,
            transcript_id,
            store: Arc::clone(&self.store),
            policy,
            model,
            messages,
            initial_events,
        });
        Ok(Arc::new(NativeSession::new(core)))
    }
}

#[async_trait]
impl AgentProvider for NativeProvider {
    fn id(&self) -> &str {
        &self.config.instance_id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Native
    }

    async fn health(&self) -> ProviderHealth {
        self.endpoint.health().await
    }

    async fn catalog(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let mut models = self.endpoint.models().await?;
        for info in &mut models {
            info.is_default = self.config.default_model.as_deref() == Some(info.id.as_str());
            info.pricing = self.config.prices.get(&info.id).copied();
        }
        self.facts.record_catalog(models.clone());
        Ok(models)
    }

    fn capabilities(&self, model: Option<&str>) -> Capabilities {
        let mut capabilities = Capabilities::none();
        capabilities.interactive_permissions = true;
        capabilities.permission_scopes = vec![PermissionScope::Once, PermissionScope::Session];
        capabilities.sandbox = SandboxLevel::None;
        capabilities.secret_isolation = true;
        capabilities.per_session_mcp = true;
        capabilities.compaction_signal = true;
        capabilities.set_model_live = true;
        capabilities.tool_cancel = true;
        capabilities.resume = true;
        capabilities.hooks = HookSupport::InProtocol;
        let Some(model) = model.or(self.config.default_model.as_deref()) else {
            return capabilities;
        };
        let probe = self.probed(model);
        capabilities.tools = probe.as_ref().is_some_and(|probe| probe.tools);
        capabilities.thinking = probe
            .as_ref()
            .is_some_and(|probe| probe.reasoning_field.is_some());
        capabilities.context_window = self.context_window(model, probe.as_ref());
        capabilities.cost = self.cost_basis(model);
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
        let data = token.expect_kind(ProviderKind::Native)?;
        let id = data
            .get("transcript_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| transcript::valid_transcript_id(id))
            .ok_or_else(|| ProviderError::invalid("the resume token carries no transcript id"))?
            .to_owned();
        let messages = self
            .store
            .load(&id)?
            .ok_or_else(|| ProviderError::invalid("unknown transcript: nothing to resume"))?;
        self.build(spec, Some((id, messages))).await
    }
}

#[cfg(test)]
mod default_tools_tests {
    use super::*;
    use crate::agent::{PolicyMode, ToolPolicy};

    fn args(spec: &SessionSpec, extra: &[&str]) -> Vec<String> {
        let mut tools = DefaultTools::new("nexus-tools");
        tools.args = extra.iter().map(|a| (*a).to_owned()).collect();
        match tools.server_for(spec, true) {
            McpServerSpec::Stdio { args, .. } => args,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_server_of_a_session_is_launched_with_the_tools_its_policy_exposes() {
        let mut spec = SessionSpec::new(std::env::temp_dir());
        spec.policy = ToolPolicy::from_patterns(PolicyMode::Ask, &["Read", "Grep"], &[]).unwrap();
        spec.extra_dirs = vec![std::env::temp_dir().join("extra")];
        let args = args(&spec, &["--tools", "Read"]);
        let at = args.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(args[at + 1], "Read,Grep", "{args:?}");
        assert_eq!(args[0], "--trust-harness");
        assert!(args.iter().any(|a| a == "--add-dir"), "{args:?}");
        // The operator's own `--tools` comes after: the server narrows by both.
        assert_eq!(args.last().map(String::as_str), Some("Read"));
        // A policy that exposes none of them launches a server with none.
        spec.policy = ToolPolicy::from_patterns(PolicyMode::Ask, &["mcp__other__x"], &[]).unwrap();
        let args = self::args(&spec, &[]);
        let at = args.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(args[at + 1], "");
    }
}
