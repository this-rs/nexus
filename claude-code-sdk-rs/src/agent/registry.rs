//! The provider registry (contract §13, decisions A1, A24, A26, A32, A38).
//!
//! One registry, in this crate; the backend keeps only a resolver (which provider
//! and model for which request). It holds **instances**: a [`ProviderInstanceConfig`]
//! is plain configuration, with at most a *reference* to a credential
//! ([`CredentialRef`]), never the credential. The key is asked from the host's
//! [`CredentialResolver`] at the moment a request needs it, by the provider the
//! registry built; the registry stores nothing secret and its `Debug` prints none.
//!
//! # The security gate (A32)
//!
//! Until [`ProviderRegistry::activate_security_gate`] is called, the registry
//! refuses every instance whose kind is neither `claude_code` nor `scripted` (the
//! test kit): [`ProviderRegistry::upsert`], [`ProviderRegistry::get`] and
//! [`ProviderRegistry::test_connection`] answer
//! `Unsupported { capability: "security_gate" }`. [`ProviderRegistry::list`] shows
//! every instance regardless. The gate takes a [`SecurityGate`] proof, which can
//! only be built with [`SecurityGate::attest`]: the backend calls it when its
//! security batch (secret isolation, MCP grants per instance, audit) is active.
//! Activation is **irreversible for the life of the registry**, and a registry is
//! per process in practice: there is no way to close the gate again.
//!
//! # The built-in instance
//!
//! `claude-code` is present from [`ProviderRegistry::new`], cannot be removed, and
//! can only be reconfigured with kind `claude_code` (for example another
//! `cli_path`). Another instance of kind `claude_code` under another id is allowed.
//!
//! # Kinds (A38)
//!
//! | Kind | Built by | Needs |
//! |---|---|---|
//! | `claude_code` | `ClaudeCodeProvider` | nothing |
//! | `native` | `NativeProvider` on an `OpenAiEndpoint` | `endpoint`; cargo feature `provider-native` (else `Unsupported { provider_native }`) |
//! | `codex` | `CodexProvider` driving `codex app-server`; cargo feature `provider-codex` (else `Unsupported { provider_codex }`) | optional `command` (the program, nothing else), `cost_source`, `prices`, `context_window`, `default_model`, `env_inherit`, `credential` (`CODEX_API_KEY`), extension `codex_home` |
//! | `acp` | `AcpProvider` driving any ACP agent; cargo feature `provider-acp` (else `Unsupported { provider_acp }`) | `command` (program + arguments, no secret), `cost_source`, `prices`, `context_window`, `default_model`, `env_inherit`, extensions `env` (object of non-secret variables), `thinking` (bool), `login_hint` (string) |
//! | `scripted` | no built-in constructor: register a factory (tests) | |
//!
//! **Extension point**: [`ProviderRegistry::register_kind_factory`] installs the
//! constructor of a kind and wins over the built-in one. The Codex and ACP slices
//! register theirs (or complete the `match` in `ProviderRegistry::construct`); a
//! test registers a replay client for `claude_code` or a stub for `scripted`. A
//! factory is called with no registry lock held but must not block for long.
//!
//! # Keys of `extensions` the built-in constructors read
//!
//! `allow_private_network` (bool, native: accept RFC 1918 / CGNAT endpoints),
//! `cli_path` (string, claude_code: path of the CLI), `compaction_keep_recent`
//! (unsigned integer, native), `codex_home` (string, codex: the instance's persistent
//! `CODEX_HOME`), `env` (object of strings, acp), `thinking` (bool, acp), `login_hint`
//! (string, acp), `acp_home` (string, acp: the instance's dedicated `HOME`, created `0700`). Their types are checked by
//! [`ProviderInstanceConfig::validate`]; other keys are kept untouched for the
//! factory of the kind to read. A key whose name looks like a credential is refused.
//!
//! # Models and aliases
//!
//! [`ProviderRegistry::resolve_alias`] is synchronous and reads the configuration
//! only. The rule: an alias key maps to its target; otherwise the name passes as is
//! when it is a model the configuration knows (alias target, `default_model`, a
//! priced model) **or when the instance declares no alias at all** (nothing can
//! contradict it, the catalogue is the judge); otherwise it is an unknown alias and
//! the answer is `invalid_request`. Aliases do not chain.
//!
//! # Prices (A1, A21)
//!
//! Each instance carries the prices of *its* models. [`ProviderRegistry::price_table`]
//! aggregates them into a [`PriceBook`] keyed by instance: two instances serving a
//! model of the same name keep their own price, and a model with no price costs
//! `None` (never zero).

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::credentials::{CredentialRef, CredentialResolver, is_sensitive_name, redact};
use super::model_provider::ModelProviderConfig;
use super::{
    AgentProvider, AgentSession, Capabilities, ContextWindow, ContextWindowSource, CostBasis,
    HealthStatus, ModelInfo, ModelPrice, ProviderError, ProviderHealth, ProviderKind, SessionSpec,
    Usage,
};
use crate::model::{EndpointQuirks, PriceTable};
use crate::providers::claude_code::{ClaudeCodeConfig, ClaudeCodeProvider};
use crate::transport::EnvPolicy;

/// Id of the built-in Claude Code instance.
pub const BUILTIN_CLAUDE_CODE_ID: &str = "claude-code";

/// Capability named by the refusal of the security gate.
pub const SECURITY_GATE_CAPABILITY: &str = "security_gate";

/// Longest instance id, in bytes.
const MAX_ID_LEN: usize = 64;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration of one provider instance. Holds no secret: `credential` is a
/// reference (`vault:<name>`, `env:<VAR>` or `none`).
///
/// Serialised (and deserialised) as JSON by the backend's instance store.
/// Unknown fields are refused, so a pasted `api_key` field fails instead of being
/// dropped silently. `#[non_exhaustive]`: build it with [`ProviderInstanceConfig::new`]
/// and the `with_*` methods; the backend reads the fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ProviderInstanceConfig {
    /// Registry key: `[a-z0-9_-]`, at most 64 bytes. `claude-code` is the built-in.
    pub id: String,
    /// Family of adapter.
    pub kind: ProviderKind,
    /// Base URL (`https://api.deepseek.com/v1`); required for `native`. No user-info.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Where the key lives. Never the key.
    #[serde(default)]
    pub credential: CredentialRef,
    /// Dialect preset of a `native` endpoint (`deepseek`, `vllm`, `ollama`,
    /// `llama_server`, `nim`, `generic`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Dialect flags added to the preset's (see [`ProviderInstanceConfig::effective_quirks`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quirks: Option<EndpointQuirks>,
    /// Model of a session that names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Alias → model id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_aliases: BTreeMap<String, String>,
    /// Context window of every model of the instance, when the operator knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Where `done.cost` comes from: `reported`, `priced`, `free`, `subscription`
    /// (`unknown` when nothing is declared: the amount is then withheld).
    #[serde(default)]
    pub cost_source: CostBasis,
    /// Prices of the instance's models, by model id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prices: BTreeMap<String, ModelPrice>,
    /// Command line of an `acp` (or `codex`) agent. No secret in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Names of host environment variables the provider process inherits
    /// (Bedrock/Vertex variables of a Claude Code instance).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_inherit: Vec<String>,
    /// Kind-specific keys (see the module documentation). No secret.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

impl ProviderInstanceConfig {
    /// A configuration with no endpoint, no credential and nothing else.
    pub fn new(id: impl Into<String>, kind: ProviderKind) -> Self {
        Self {
            id: id.into(),
            kind,
            endpoint: None,
            credential: CredentialRef::None,
            preset: None,
            quirks: None,
            default_model: None,
            model_aliases: BTreeMap::new(),
            context_window: None,
            cost_source: CostBasis::Unknown,
            prices: BTreeMap::new(),
            command: None,
            env_inherit: Vec::new(),
            extensions: BTreeMap::new(),
        }
    }

    /// A Claude Code instance (cost reported by the CLI).
    pub fn claude_code(id: impl Into<String>) -> Self {
        Self::new(id, ProviderKind::ClaudeCode).with_cost_source(CostBasis::Reported)
    }

    /// A native instance on an OpenAI-compatible endpoint.
    pub fn native(id: impl Into<String>, endpoint: impl Into<String>) -> Self {
        let mut config = Self::new(id, ProviderKind::Native);
        config.endpoint = Some(endpoint.into());
        config
    }

    /// Parses a configuration from JSON, with errors that never echo a value.
    ///
    /// A `credential` that is not a reference (a pasted key), an unknown `kind` and
    /// any other malformation answer `invalid_request` with a fixed message.
    pub fn from_json(value: &Value) -> Result<Self, ProviderError> {
        if let Some(credential) = value.get("credential") {
            let text = credential.as_str().ok_or_else(|| {
                ProviderError::invalid(
                    "credential must be a reference: vault:<name>, env:<VAR> or none",
                )
            })?;
            text.parse::<CredentialRef>()?;
        }
        if let Some(kind) = value.get("kind")
            && serde_json::from_value::<ProviderKind>(kind.clone()).is_err()
        {
            return Err(ProviderError::invalid("unknown provider kind"));
        }
        serde_json::from_value(value.clone())
            .map_err(|_| ProviderError::invalid("malformed provider instance configuration"))
    }

    /// Sets the endpoint.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Sets the credential reference.
    pub fn with_credential(mut self, credential: CredentialRef) -> Self {
        self.credential = credential;
        self
    }

    /// Sets the credential reference from its text form; a value that is not a
    /// reference is refused without being echoed.
    pub fn with_credential_ref(mut self, reference: &str) -> Result<Self, ProviderError> {
        self.credential = reference.parse()?;
        Ok(self)
    }

    /// Sets the dialect preset.
    pub fn with_preset(mut self, preset: impl Into<String>) -> Self {
        self.preset = Some(preset.into());
        self
    }

    /// Sets the dialect flags added to the preset's.
    pub fn with_quirks(mut self, quirks: EndpointQuirks) -> Self {
        self.quirks = Some(quirks);
        self
    }

    /// Sets the default model.
    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Adds a model alias.
    pub fn with_alias(mut self, alias: impl Into<String>, model: impl Into<String>) -> Self {
        self.model_aliases.insert(alias.into(), model.into());
        self
    }

    /// Sets the context window of every model.
    pub fn with_context_window(mut self, tokens: u64) -> Self {
        self.context_window = Some(tokens);
        self
    }

    /// Sets where the cost comes from.
    pub fn with_cost_source(mut self, source: CostBasis) -> Self {
        self.cost_source = source;
        self
    }

    /// Sets the price of a model of this instance.
    pub fn with_price(mut self, model: impl Into<String>, price: ModelPrice) -> Self {
        self.prices.insert(model.into(), price);
        self
    }

    /// Sets the command line of an agent process.
    pub fn with_command<I, S>(mut self, command: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.command = Some(command.into_iter().map(Into::into).collect());
        self
    }

    /// Lists host environment variables the provider process inherits.
    pub fn with_env_inherit<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.env_inherit = names.into_iter().map(Into::into).collect();
        self
    }

    /// Sets a kind-specific key.
    pub fn with_extension(mut self, key: impl Into<String>, value: Value) -> Self {
        self.extensions.insert(key.into(), value);
        self
    }

    /// The quirks a `native` instance runs with: the preset's, plus the instance's
    /// own. Booleans are OR-ed (an override can add a flag, not remove the preset's:
    /// leave `preset` out to control every flag), `explicit_parallel_tool_calls`,
    /// `vision` and `reasoning_field` take the override when it sets one (`Some`,
    /// non-default).
    pub fn effective_quirks(&self) -> Result<EndpointQuirks, ProviderError> {
        let base = match &self.preset {
            Some(name) => EndpointQuirks::preset(name)
                .ok_or_else(|| ProviderError::invalid("unknown endpoint preset"))?,
            None => EndpointQuirks::generic(),
        };
        let Some(over) = &self.quirks else {
            return Ok(base);
        };
        Ok(EndpointQuirks {
            echo_reasoning_with_tools: base.echo_reasoning_with_tools
                || over.echo_reasoning_with_tools,
            omit_tool_choice: base.omit_tool_choice || over.omit_tool_choice,
            no_forced_tool_choice: base.no_forced_tool_choice || over.no_forced_tool_choice,
            explicit_parallel_tool_calls: over
                .explicit_parallel_tool_calls
                .or(base.explicit_parallel_tool_calls),
            reasoning_field: if over.reasoning_field == EndpointQuirks::generic().reasoning_field {
                base.reasoning_field
            } else {
                over.reasoning_field
            },
            fold_late_system: base.fold_late_system || over.fold_late_system,
            tool_args_as_object: base.tool_args_as_object || over.tool_args_as_object,
            vision_probe: base.vision_probe || over.vision_probe,
            vision: over.vision.or(base.vision),
        })
    }

    /// Checks the configuration without building anything or touching the network.
    ///
    /// Every refusal is `invalid_request` and none echoes a configuration value
    /// (a value may be a credential pasted in the wrong field).
    pub fn validate(&self) -> Result<(), ProviderError> {
        let bad = |detail: &str| Err(ProviderError::invalid(detail));
        if !valid_instance_id(&self.id) {
            return bad("instance id must be 1-64 characters of [a-z0-9_-]");
        }
        if self.id == BUILTIN_CLAUDE_CODE_ID && self.kind != ProviderKind::ClaudeCode {
            return bad("the id claude-code is reserved for the built-in claude_code instance");
        }
        match self.kind {
            ProviderKind::ClaudeCode => {
                if self.endpoint.is_some() {
                    return bad("claude_code does not take an endpoint");
                }
            },
            ProviderKind::Native => {
                if self.endpoint.is_none() {
                    return bad("native requires an endpoint");
                }
            },
            ProviderKind::Codex => {},
            ProviderKind::Acp => {
                if self.command.is_none() {
                    return bad("acp requires a command");
                }
            },
            ProviderKind::Scripted => {},
            #[allow(unreachable_patterns)]
            _ => return bad("unknown provider kind"),
        }
        if let Some(endpoint) = &self.endpoint {
            check_endpoint(endpoint)?;
        }
        if self.kind != ProviderKind::Native && (self.preset.is_some() || self.quirks.is_some()) {
            return bad("preset and quirks apply to native instances only");
        }
        if let Some(preset) = &self.preset
            && EndpointQuirks::preset(preset).is_none()
        {
            return bad("unknown endpoint preset");
        }
        if let Some(command) = &self.command {
            if !matches!(self.kind, ProviderKind::Acp | ProviderKind::Codex) {
                return bad("command applies to acp and codex instances only");
            }
            if command
                .first()
                .is_none_or(|program| program.trim().is_empty())
            {
                return bad("command must start with a program");
            }
            if command.iter().any(|part| redact(part) != *part) {
                return bad("command must not carry a credential: use a credential reference");
            }
        }
        if self.default_model.as_deref().is_some_and(str::is_empty) {
            return bad("default_model must not be empty");
        }
        if self
            .model_aliases
            .iter()
            .any(|(alias, model)| !valid_model_name(alias) || !valid_model_name(model))
        {
            return bad("model aliases must map a name to a model id, both non-empty");
        }
        if self.context_window == Some(0) {
            return bad("context_window must be positive");
        }
        let valid_price = |price: &ModelPrice| {
            [Some(price.input_per_mtok), Some(price.output_per_mtok)]
                .into_iter()
                .chain([price.cache_read_per_mtok, price.cache_write_per_mtok])
                .flatten()
                .all(|figure| figure.is_finite() && figure >= 0.0)
        };
        if self
            .prices
            .iter()
            .any(|(model, price)| model.is_empty() || !valid_price(price))
        {
            return bad("prices must be finite and not negative");
        }
        if self.env_inherit.iter().any(|name| {
            name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }) {
            return bad("env_inherit holds variable names");
        }
        self.validate_extensions()
    }

    fn validate_extensions(&self) -> Result<(), ProviderError> {
        fn has_sensitive_key(value: &Value) -> bool {
            match value {
                Value::Object(map) => map
                    .iter()
                    .any(|(key, inner)| is_sensitive_name(key) || has_sensitive_key(inner)),
                Value::Array(items) => items.iter().any(has_sensitive_key),
                _ => false,
            }
        }
        for (key, value) in &self.extensions {
            if is_sensitive_name(key) || has_sensitive_key(value) {
                return Err(ProviderError::invalid(
                    "extensions must not carry credentials: use a credential reference",
                ));
            }
            let well_typed = match key.as_str() {
                "allow_private_network" => value.is_boolean(),
                "cli_path" => value.as_str().is_some_and(|path| !path.is_empty()),
                "compaction_keep_recent" => value.is_u64(),
                "codex_home" => value.as_str().is_some_and(|path| !path.is_empty()),
                "acp_home" => value.as_str().is_some_and(|path| !path.is_empty()),
                "env" => value
                    .as_object()
                    .is_some_and(|env| env.values().all(Value::is_string)),
                "thinking" => value.is_boolean(),
                "login_hint" => value.as_str().is_some_and(|hint| !hint.is_empty()),
                _ => true,
            };
            if !well_typed {
                return Err(ProviderError::invalid(
                    "an extension known to the registry has the wrong type",
                ));
            }
        }
        Ok(())
    }

    #[cfg(feature = "provider-native")]
    fn extension_bool(&self, key: &str) -> bool {
        self.extensions
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

/// Syntax of an instance id (and of a model provider id): 1-64 of `[a-z0-9_-]`.
pub(crate) fn valid_instance_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
}

fn valid_model_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Syntax of an endpoint URL, without a URL parser (the parser lives behind
/// `provider-native`). The request-time guard (A36) does the rest.
pub(crate) fn check_endpoint(raw: &str) -> Result<(), ProviderError> {
    let bad = |detail: &str| Err(ProviderError::invalid(detail));
    let text = raw.trim();
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return bad("endpoint URL is not valid");
    }
    let rest = if let Some(rest) = strip_scheme(text, "https://") {
        rest
    } else if let Some(rest) = strip_scheme(text, "http://") {
        rest
    } else {
        return bad("endpoint URL must use http or https");
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.contains('@') {
        return bad("endpoint URL must not carry credentials: use a credential reference");
    }
    if authority.is_empty() || authority.starts_with(':') {
        return bad("endpoint URL has no host");
    }
    if let Some(query) = rest[authority_end..]
        .split_once('?')
        .map(|(_, query)| query.split('#').next().unwrap_or_default())
        && query
            .split('&')
            .any(|pair| is_sensitive_name(pair.split('=').next().unwrap_or_default()))
    {
        return bad("endpoint URL must not carry credentials: use a credential reference");
    }
    Ok(())
}

fn strip_scheme<'a>(text: &'a str, scheme: &str) -> Option<&'a str> {
    text.get(..scheme.len())
        .filter(|head| head.eq_ignore_ascii_case(scheme))
        .map(|_| &text[scheme.len()..])
}

// ---------------------------------------------------------------------------
// Security gate
// ---------------------------------------------------------------------------

/// Proof that the security batch is active (A32).
///
/// There is no public field and no `Default`: the only way to get one is
/// [`SecurityGate::attest`], a call a reviewer can find by searching for it. The
/// backend makes it once, where its security batch (secret isolation, MCP grants
/// per instance, audit of third-party sessions) is switched on; nothing in this
/// crate calls it outside tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityGate {
    batch: &'static str,
}

impl SecurityGate {
    /// Attests that the security batch named `batch` is active. The name is kept
    /// for the logs of the process (`ProviderRegistry::security_gate_batch`).
    ///
    /// Calling this without the batch being active defeats the gate: it is the
    /// host's declaration, and the reason the registry refuses third-party
    /// providers before it.
    pub fn attest(batch: &'static str) -> Self {
        Self { batch }
    }

    /// Name of the batch that attested.
    pub fn batch(&self) -> &'static str {
        self.batch
    }
}

/// Whether the gate guards `kind`: everything but Claude Code and the test kit.
fn gated(kind: ProviderKind) -> bool {
    !matches!(kind, ProviderKind::ClaudeCode | ProviderKind::Scripted)
}

// ---------------------------------------------------------------------------
// Built providers and factories
// ---------------------------------------------------------------------------

/// Probes the capabilities of a model by talking to the endpoint (native).
#[async_trait]
pub trait CapabilityRefresher: Send + Sync {
    /// Probes `model` and returns the capabilities that result.
    async fn refresh(&self, model: &str) -> Result<Capabilities, ProviderError>;
}

/// What a kind factory returns.
#[derive(Clone)]
pub struct BuiltProvider {
    provider: Arc<dyn AgentProvider>,
    refresher: Option<Arc<dyn CapabilityRefresher>>,
}

impl BuiltProvider {
    /// A provider with nothing to probe.
    pub fn new(provider: Arc<dyn AgentProvider>) -> Self {
        Self {
            provider,
            refresher: None,
        }
    }

    /// Adds the probe that fills `capabilities(model)` (see
    /// [`ProviderRegistry::refresh_capabilities`]).
    pub fn with_refresher(mut self, refresher: Arc<dyn CapabilityRefresher>) -> Self {
        self.refresher = Some(refresher);
        self
    }
}

/// Constructor of a kind: builds the provider of an instance. It receives the
/// registry's credential resolver and must hand it to the provider, which asks it
/// per request; nothing is resolved at construction.
pub type KindFactory = Arc<
    dyn Fn(
            &ProviderInstanceConfig,
            Arc<dyn CredentialResolver>,
        ) -> Result<BuiltProvider, ProviderError>
        + Send
        + Sync,
>;

#[cfg(feature = "provider-native")]
#[async_trait]
impl CapabilityRefresher for crate::providers::native::NativeProvider {
    async fn refresh(&self, model: &str) -> Result<Capabilities, ProviderError> {
        self.refresh_capabilities(model).await
    }
}

// ---------------------------------------------------------------------------
// Price book
// ---------------------------------------------------------------------------

/// The prices of every instance, aggregated: one [`PriceTable`] per instance.
///
/// A cost is always computed with the price of the instance that served the model,
/// so two instances serving a model of the same name never share a price.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PriceBook {
    tables: BTreeMap<String, PriceTable>,
}

impl PriceBook {
    /// The price table of an instance.
    pub fn table(&self, instance: &str) -> Option<&PriceTable> {
        self.tables.get(instance)
    }

    /// The price of `model` on `instance`.
    pub fn get(&self, instance: &str, model: &str) -> Option<&ModelPrice> {
        self.tables.get(instance)?.get(model)
    }

    /// Number of instances in the book.
    pub fn instances(&self) -> usize {
        self.tables.len()
    }

    /// Cost in USD of `usage` on `model` of `instance`; `None` without a price.
    pub fn cost_usd(&self, instance: &str, model: &str, usage: &Usage) -> Option<f64> {
        self.table(instance)?.cost_usd(model, usage)
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

struct Entry {
    config: Arc<ProviderInstanceConfig>,
    built: Option<BuiltProvider>,
}

/// The provider registry. See the module documentation.
pub struct ProviderRegistry {
    resolver: Arc<dyn CredentialResolver>,
    gate: OnceLock<&'static str>,
    entries: Mutex<BTreeMap<String, Entry>>,
    factories: Mutex<HashMap<ProviderKind, KindFactory>>,
    model_providers: Mutex<BTreeMap<String, Arc<ModelProviderConfig>>>,
    composed: Mutex<BTreeMap<(String, String), BuiltProvider>>,
    #[cfg(feature = "provider-native")]
    transcripts: Mutex<Option<Arc<dyn crate::providers::native::TranscriptStore>>>,
}

impl fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let instances: Vec<String> = entries
            .values()
            .map(|entry| format!("{}:{}", entry.config.id, entry.config.kind.as_str()))
            .collect();
        f.debug_struct("ProviderRegistry")
            .field("instances", &instances)
            .field("security_gate", &self.gate.get())
            .finish_non_exhaustive()
    }
}

impl ProviderRegistry {
    /// A registry holding the built-in `claude-code` instance. `resolver` is the
    /// host's: it answers every credential request of every provider built here.
    pub fn new(resolver: Arc<dyn CredentialResolver>) -> Self {
        let mut entries = BTreeMap::new();
        entries.insert(
            BUILTIN_CLAUDE_CODE_ID.to_owned(),
            Entry {
                config: Arc::new(ProviderInstanceConfig::claude_code(BUILTIN_CLAUDE_CODE_ID)),
                built: None,
            },
        );
        Self {
            resolver,
            gate: OnceLock::new(),
            entries: Mutex::new(entries),
            factories: Mutex::new(HashMap::new()),
            model_providers: Mutex::new(BTreeMap::from([(
                BUILTIN_ANTHROPIC_PROVIDER_ID.to_owned(),
                Arc::new(ModelProviderConfig::new(
                    BUILTIN_ANTHROPIC_PROVIDER_ID,
                    super::model_provider::ModelProtocol::AnthropicMessages,
                )),
            )])),
            composed: Mutex::new(BTreeMap::new()),
            #[cfg(feature = "provider-native")]
            transcripts: Mutex::new(None),
        }
    }

    /// Opens the security gate (A32). Irreversible: see the module documentation.
    /// A second call keeps the first proof.
    pub fn activate_security_gate(&self, proof: SecurityGate) {
        let _ = self.gate.set(proof.batch);
    }

    /// Whether the security gate is open.
    pub fn security_gate_active(&self) -> bool {
        self.gate.get().is_some()
    }

    /// Name of the batch that opened the gate.
    pub fn security_gate_batch(&self) -> Option<&'static str> {
        self.gate.get().copied()
    }

    /// Keeps the transcripts of every native instance in `store` (default: memory,
    /// lost with the process). Applies to providers built afterwards; instances
    /// already built are rebuilt on their next `get` only if upserted again.
    #[cfg(feature = "provider-native")]
    pub fn set_transcript_store(&self, store: Arc<dyn crate::providers::native::TranscriptStore>) {
        *self
            .transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(store);
        self.invalidate_kind(ProviderKind::Native);
    }

    /// Installs the constructor of `kind`, replacing the built-in one. Instances of
    /// that kind already built are rebuilt on their next `get`.
    pub fn register_kind_factory(&self, kind: ProviderKind, factory: KindFactory) {
        self.factories
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(kind, factory);
        self.invalidate_kind(kind);
    }

    fn invalidate_kind(&self, kind: ProviderKind) {
        for entry in self
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values_mut()
        {
            if entry.config.kind == kind {
                entry.built = None;
            }
        }
    }

    fn check_gate(&self, kind: ProviderKind) -> Result<(), ProviderError> {
        if gated(kind) && !self.security_gate_active() {
            return Err(ProviderError::unsupported(SECURITY_GATE_CAPABILITY));
        }
        Ok(())
    }

    /// Adds an instance or replaces the one with the same id (hot reload).
    ///
    /// Sessions already open keep the provider they were opened on (an
    /// `Arc<dyn AgentProvider>` handed out earlier is not touched); the next `get`
    /// builds the new one. Nothing is built or contacted here.
    ///
    /// Refused: a third-party kind before the security gate is open
    /// (`Unsupported { security_gate }`), an invalid configuration
    /// (`invalid_request`, see [`ProviderInstanceConfig::validate`]).
    pub fn upsert(&self, config: ProviderInstanceConfig) -> Result<(), ProviderError> {
        self.check_gate(config.kind)?;
        config.validate()?;
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                config.id.clone(),
                Entry {
                    config: Arc::new(config),
                    built: None,
                },
            );
        self.clear_composed();
        Ok(())
    }

    /// Removes an instance. `false` when there is none, and for the built-in
    /// `claude-code`, which is always present.
    pub fn remove(&self, id: &str) -> bool {
        if id == BUILTIN_CLAUDE_CODE_ID {
            return false;
        }
        let removed = self
            .entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
            .is_some();
        self.clear_composed();
        removed
    }

    /// The provider of an instance, built on first use and cached until the
    /// instance is upserted again.
    ///
    /// Answers `invalid_request` for an unknown id, `Unsupported { security_gate }`
    /// for a third-party kind while the gate is shut, and
    /// `Unsupported { provider_codex | provider_acp | provider_native }` for a kind
    /// with no constructor yet.
    pub fn get(&self, id: &str) -> Result<Arc<dyn AgentProvider>, ProviderError> {
        Ok(self.get_built(id)?.provider)
    }

    fn get_built(&self, id: &str) -> Result<BuiltProvider, ProviderError> {
        let (config, cached) = {
            let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            let entry = entries
                .get(id)
                .ok_or_else(|| ProviderError::invalid("unknown provider instance"))?;
            (Arc::clone(&entry.config), entry.built.clone())
        };
        self.check_gate(config.kind)?;
        if let Some(built) = cached {
            return Ok(built);
        }
        // Built with no lock held: a factory may be slow, or call back.
        let built = self.construct(&config)?;
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        match entries.get_mut(id) {
            // Still the configuration we built from: cache (first builder wins).
            Some(entry) if Arc::ptr_eq(&entry.config, &config) => {
                Ok(entry.built.get_or_insert(built).clone())
            },
            // Upserted or removed meanwhile: the caller still gets what it asked for.
            _ => Ok(built),
        }
    }

    /// Configurations of every instance, built-in included, sorted by id. Never
    /// gated: the instances a shut gate refuses are listed all the same.
    pub fn list(&self) -> Vec<ProviderInstanceConfig> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|entry| (*entry.config).clone())
            .collect()
    }

    /// Configuration of one instance.
    pub fn config(&self, id: &str) -> Option<ProviderInstanceConfig> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .map(|entry| (*entry.config).clone())
    }

    /// Resolves a model alias of an instance to a model id (rule in the module
    /// documentation). An unknown alias or instance is `invalid_request`.
    pub fn resolve_alias(&self, id: &str, alias: &str) -> Result<String, ProviderError> {
        let config = self
            .config(id)
            .ok_or_else(|| ProviderError::invalid("unknown provider instance"))?;
        if let Some(model) = config.model_aliases.get(alias) {
            return Ok(model.clone());
        }
        if !valid_model_name(alias) {
            return Err(ProviderError::invalid("model name is not valid"));
        }
        let known = config.model_aliases.values().any(|model| model == alias)
            || config.default_model.as_deref() == Some(alias)
            || config.prices.contains_key(alias);
        if known || config.model_aliases.is_empty() {
            Ok(alias.to_owned())
        } else {
            Err(ProviderError::invalid("unknown model alias"))
        }
    }

    /// Capabilities of an instance for a model (`None`: its default model), as the
    /// provider declares them now. A native instance declares `tools: false` until
    /// the model is probed: see [`ProviderRegistry::refresh_capabilities`].
    pub fn capabilities_for(
        &self,
        id: &str,
        model: Option<&str>,
    ) -> Result<Capabilities, ProviderError> {
        Ok(self.get(id)?.capabilities(model))
    }

    /// Probes `model` on the instance when its kind can be probed (native: one
    /// tool-call request), then returns [`ProviderRegistry::capabilities_for`].
    pub async fn refresh_capabilities(
        &self,
        id: &str,
        model: &str,
    ) -> Result<Capabilities, ProviderError> {
        let built = self.get_built(id)?;
        match &built.refresher {
            Some(refresher) => refresher.refresh(model).await,
            None => Ok(built.provider.capabilities(Some(model))),
        }
    }

    /// The prices of every instance, aggregated (see [`PriceBook`]).
    pub fn price_table(&self) -> PriceBook {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        PriceBook {
            tables: entries
                .values()
                .map(|entry| (entry.config.id.clone(), price_table_of(&entry.config)))
                .collect(),
        }
    }

    /// Connection test before registration (A30): builds an ephemeral instance
    /// from `config`, asks it `health()` and, for a native instance with a default
    /// model, probes that model's tool calls. **Registers nothing** and caches
    /// nothing; the ephemeral provider is dropped.
    ///
    /// A configuration the registry refuses (gate, validation, no constructor) is
    /// an `Err`; an endpoint that does not answer is `Ok` with `health.status`
    /// `Unavailable`. A model that cannot call tools gives `Degraded`.
    pub async fn test_connection(
        &self,
        config: &ProviderInstanceConfig,
    ) -> Result<ProviderHealth, ProviderError> {
        self.check_gate(config.kind)?;
        config.validate()?;
        let built = self.construct(config)?;
        let mut health = built.provider.health().await;
        if health.status != HealthStatus::Ok {
            return Ok(health);
        }
        if let (Some(refresher), Some(model)) = (&built.refresher, &config.default_model) {
            match refresher.refresh(model).await {
                Ok(capabilities) if !capabilities.tools => {
                    health.status = HealthStatus::Degraded;
                    health.detail = Some("the default model does not call tools".to_owned());
                },
                Ok(_) => {},
                Err(error) => {
                    health.status = HealthStatus::Degraded;
                    health.detail = Some(format!("tool-call probe failed: {error}"));
                },
            }
        }
        Ok(health)
    }

    fn factory(&self, kind: ProviderKind) -> Option<KindFactory> {
        self.factories
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&kind)
            .cloned()
    }

    /// Builds the provider of an instance. The place to complete when a kind gets
    /// its module (or register a factory instead).
    fn construct(&self, config: &ProviderInstanceConfig) -> Result<BuiltProvider, ProviderError> {
        if let Some(factory) = self.factory(config.kind) {
            return factory(config, Arc::clone(&self.resolver));
        }
        match config.kind {
            ProviderKind::ClaudeCode => Ok(build_claude_code(config)),
            ProviderKind::Native => self.build_native(config),
            ProviderKind::Codex => self.build_codex(config),
            ProviderKind::Acp => self.build_acp(config),
            ProviderKind::Scripted => Err(ProviderError::unsupported("provider_scripted")),
            #[allow(unreachable_patterns)]
            _ => Err(ProviderError::unsupported("provider_kind")),
        }
    }

    #[cfg(feature = "provider-codex")]
    fn build_codex(&self, config: &ProviderInstanceConfig) -> Result<BuiltProvider, ProviderError> {
        use crate::providers::codex::{CodexConfig, CodexProvider};

        let mut codex = CodexConfig::new(&config.id);
        if let Some(command) = &config.command {
            if command.len() > 1 {
                return Err(ProviderError::invalid(
                    "a codex command is the program only: the adapter owns the arguments",
                ));
            }
            if let Some(program) = command.first() {
                codex.program = PathBuf::from(program);
            }
        }
        if let Some(home) = config.extensions.get("codex_home").and_then(Value::as_str) {
            codex.codex_home = PathBuf::from(home);
        }
        codex.default_model = config.default_model.clone();
        codex.context_window = config.context_window;
        codex.cost_basis = config.cost_source;
        codex.prices = price_table_of(config);
        codex.env_inherit = config.env_inherit.clone();
        codex.credential = config.credential.clone();
        let mut models: Vec<String> = config
            .prices
            .keys()
            .chain(config.model_aliases.values())
            .cloned()
            .collect();
        models.sort();
        models.dedup();
        codex.models = models;
        Ok(BuiltProvider::new(Arc::new(CodexProvider::with_resolver(
            codex,
            Arc::clone(&self.resolver),
        ))))
    }

    #[cfg(not(feature = "provider-codex"))]
    fn build_codex(
        &self,
        _config: &ProviderInstanceConfig,
    ) -> Result<BuiltProvider, ProviderError> {
        Err(ProviderError::unsupported("provider_codex"))
    }

    #[cfg(feature = "provider-acp")]
    fn build_acp(&self, config: &ProviderInstanceConfig) -> Result<BuiltProvider, ProviderError> {
        use crate::providers::acp::{AcpConfig, AcpProvider};

        let command = config
            .command
            .clone()
            .ok_or_else(|| ProviderError::invalid("acp requires a command"))?;
        let mut acp = AcpConfig::new(&config.id, command);
        acp.env_inherit = config.env_inherit.clone();
        if let Some(Value::Object(env)) = config.extensions.get("env") {
            acp.env = env
                .iter()
                .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_owned())))
                .collect();
        }
        acp.thinking = config
            .extensions
            .get("thinking")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(home) = config.extensions.get("acp_home").and_then(Value::as_str) {
            acp.home = PathBuf::from(home);
        }
        acp.login_hint = config
            .extensions
            .get("login_hint")
            .and_then(Value::as_str)
            .map(str::to_owned);
        acp.default_model = config.default_model.clone();
        acp.context_window = config.context_window;
        acp.cost_basis = config.cost_source;
        acp.prices = price_table_of(config);
        let mut models: Vec<String> = config
            .prices
            .keys()
            .chain(config.model_aliases.values())
            .cloned()
            .collect();
        models.sort();
        models.dedup();
        acp.models = models;
        acp.validate()?;
        Ok(BuiltProvider::new(Arc::new(AcpProvider::new(acp))))
    }

    #[cfg(not(feature = "provider-acp"))]
    fn build_acp(&self, _config: &ProviderInstanceConfig) -> Result<BuiltProvider, ProviderError> {
        Err(ProviderError::unsupported("provider_acp"))
    }

    fn build_native(
        &self,
        config: &ProviderInstanceConfig,
    ) -> Result<BuiltProvider, ProviderError> {
        self.build_native_for(config, &config.id)
    }

    /// `credential_owner` is the id the credential grant names: the instance itself for
    /// a native instance of its own, the model provider for a composed one (N16).
    #[cfg(feature = "provider-native")]
    fn build_native_for(
        &self,
        config: &ProviderInstanceConfig,
        credential_owner: &str,
    ) -> Result<BuiltProvider, ProviderError> {
        use crate::model::{OpenAiEndpoint, OpenAiEndpointConfig};
        use crate::providers::native::{NativeConfig, NativeProvider};

        let base_url = config
            .endpoint
            .clone()
            .ok_or_else(|| ProviderError::invalid("native requires an endpoint"))?;
        let mut endpoint = OpenAiEndpointConfig::new(credential_owner, base_url);
        endpoint.credential = config.credential.clone();
        endpoint.quirks = config.effective_quirks()?;
        endpoint.allow_private_network = config.extension_bool("allow_private_network");
        let endpoint = Arc::new(OpenAiEndpoint::new(endpoint, Arc::clone(&self.resolver)));

        let mut native = NativeConfig::new(&config.id);
        native.default_model = config.default_model.clone();
        native.context_window = config.context_window;
        native.prices = price_table_of(config);
        native.cost_basis = config.cost_source;
        if let Some(keep) = config
            .extensions
            .get("compaction_keep_recent")
            .and_then(Value::as_u64)
        {
            native.compaction.keep_recent = usize::try_from(keep).unwrap_or(usize::MAX);
        }
        native.default_tools = default_tools_of(config)?;
        native.browser = browser_of(config)?;
        let mut provider = NativeProvider::new(native, endpoint);
        if let Some(store) = self
            .transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            provider = provider.with_transcript_store(store);
        }
        let provider = Arc::new(provider);
        Ok(BuiltProvider::new(provider.clone()).with_refresher(provider))
    }

    #[cfg(not(feature = "provider-native"))]
    fn build_native_for(
        &self,
        _config: &ProviderInstanceConfig,
        _credential_owner: &str,
    ) -> Result<BuiltProvider, ProviderError> {
        Err(ProviderError::unsupported("provider_native"))
    }
}

/// The `nexus_tools` extension of a native instance: `true` (find `nexus-tools` beside the
/// running executable or in the `PATH`), or `{"program": "...", "args": [...], "env": {...}}`.
/// Absent or `false`: the sessions of the instance have no default tools.
#[cfg(feature = "provider-native")]
fn default_tools_of(
    config: &ProviderInstanceConfig,
) -> Result<Option<crate::providers::native::DefaultTools>, ProviderError> {
    use crate::providers::native::DefaultTools;
    let Some(value) = config.extensions.get("nexus_tools") else {
        return Ok(None);
    };
    match value {
        Value::Bool(false) | Value::Null => Ok(None),
        Value::Bool(true) => DefaultTools::locate().map(Some).ok_or_else(|| {
            ProviderError::invalid(
                "extensions.nexus_tools is true but no `nexus-tools` executable was found beside this program or in the PATH",
            )
        }),
        Value::Object(object) => {
            let program = object
                .get("program")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| ProviderError::invalid("extensions.nexus_tools.program is required"))?;
            let mut tools = DefaultTools::new(program);
            if let Some(args) = object.get("args").and_then(Value::as_array) {
                tools.args = args.iter().filter_map(Value::as_str).map(str::to_owned).collect();
            }
            if let Some(env) = object.get("env").and_then(Value::as_object) {
                tools.env = env
                    .iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                    .collect();
            }
            Ok(Some(tools))
        },
        _ => Err(ProviderError::invalid(
            "extensions.nexus_tools must be true, false or an object",
        )),
    }
}

/// The `browser` extension of a native instance: `true` (find `obscura` in the `PATH`) or
/// `{"program": "...", "args": [...]}`; absent or `false`: no browser. A missing executable is
/// not an error here: the session says so (`browser_unavailable`).
#[cfg(feature = "provider-native")]
fn browser_of(
    config: &ProviderInstanceConfig,
) -> Result<Option<crate::providers::native::BrowserTools>, ProviderError> {
    use crate::providers::native::BrowserTools;
    let Some(value) = config.extensions.get("browser") else {
        return Ok(None);
    };
    match value {
        Value::Bool(false) | Value::Null => Ok(None),
        Value::Bool(true) => Ok(Some(
            BrowserTools::locate().unwrap_or_else(|| BrowserTools::new("obscura")),
        )),
        Value::Object(object) => {
            let program = object
                .get("program")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| ProviderError::invalid("extensions.browser.program is required"))?;
            let args: Vec<String> = object
                .get("args")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            Ok(Some(BrowserTools::new(program).with_args(args)?))
        },
        _ => Err(ProviderError::invalid(
            "extensions.browser must be true, false or an object",
        )),
    }
}

fn price_table_of(config: &ProviderInstanceConfig) -> PriceTable {
    let mut table = PriceTable::new();
    for (model, price) in &config.prices {
        table.insert(model.clone(), *price);
    }
    table
}

fn build_claude_code(config: &ProviderInstanceConfig) -> BuiltProvider {
    let window = config.context_window.map(|value| ContextWindow {
        value,
        source: ContextWindowSource::Configured,
    });
    let mut claude = ClaudeCodeConfig {
        id: config.id.clone(),
        cli_path: config
            .extensions
            .get("cli_path")
            .and_then(Value::as_str)
            .map(PathBuf::from),
        cost_basis: config.cost_source,
        default_model: config.default_model.clone(),
        context_window: window,
        ..ClaudeCodeConfig::default()
    };
    if !config.env_inherit.is_empty() {
        claude.env_policy = EnvPolicy::claude_code().with_inherited(config.env_inherit.iter());
    }
    // The catalogue: every model the configuration names.
    let mut ids: Vec<&String> = config
        .prices
        .keys()
        .chain(config.model_aliases.values())
        .chain(config.default_model.as_ref())
        .collect();
    ids.sort();
    ids.dedup();
    claude.models = ids
        .into_iter()
        .map(|id| {
            let mut info = ModelInfo::new(id.clone());
            info.is_default = config.default_model.as_ref() == Some(id);
            info.pricing = config.prices.get(id).copied();
            info.context_window = window;
            info
        })
        .collect();
    BuiltProvider::new(Arc::new(ClaudeCodeProvider::new(claude)))
}

// ---------------------------------------------------------------------------
// Model providers and the binding of a session to one (N16)
// ---------------------------------------------------------------------------

/// Id of the built-in model provider: Anthropic's own API, reached with no endpoint of
/// the operator's. The only first-party one; it cannot be removed or redefined.
pub const BUILTIN_ANTHROPIC_PROVIDER_ID: &str = "anthropic";

impl ProviderInstanceConfig {
    /// The model side of this instance, read as a model provider.
    ///
    /// An instance written before N16 fuses a harness and a model provider; this reads
    /// the second out of it **without migrating anything**: the same fields, seen as the
    /// pair `(kind, model provider)`. `None` for an agent that picks its own model
    /// (`acp`, `scripted`).
    pub fn model_provider(&self) -> Option<ModelProviderConfig> {
        let protocol = self.kind.default_model_protocol()?;
        let mut provider = ModelProviderConfig::new(self.id.clone(), protocol);
        provider.endpoint = self.endpoint.clone();
        provider.credential = self.credential.clone();
        provider.preset = self.preset.clone();
        provider.quirks = self.quirks.clone();
        provider.default_model = self.default_model.clone();
        provider.model_aliases = self.model_aliases.clone();
        provider.context_window = self.context_window;
        provider.cost_source = self.cost_source;
        provider.prices = self.prices.clone();
        if let Some(allow) = self.extensions.get("allow_private_network") {
            provider
                .extensions
                .insert("allow_private_network".to_owned(), allow.clone());
        }
        Some(provider)
    }
}

impl ProviderRegistry {
    /// Adds a model provider or replaces the one with the same id.
    ///
    /// Refused: a third-party model provider before the security gate is open
    /// (`Unsupported { security_gate }`, the A32 rule extended to the model side), an
    /// invalid configuration, and a redefinition of the built-in `anthropic`.
    pub fn upsert_model_provider(&self, config: ModelProviderConfig) -> Result<(), ProviderError> {
        if config.id == BUILTIN_ANTHROPIC_PROVIDER_ID && !config.is_first_party() {
            return Err(ProviderError::invalid(
                "the id anthropic is reserved for Anthropic's own API",
            ));
        }
        if !config.is_first_party() && !self.security_gate_active() {
            return Err(ProviderError::unsupported(SECURITY_GATE_CAPABILITY));
        }
        config.validate()?;
        self.model_providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(config.id.clone(), Arc::new(config));
        self.clear_composed();
        Ok(())
    }

    /// Removes a model provider. `false` when there is none, and for the built-in
    /// `anthropic`.
    pub fn remove_model_provider(&self, id: &str) -> bool {
        if id == BUILTIN_ANTHROPIC_PROVIDER_ID {
            return false;
        }
        let removed = self
            .model_providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
            .is_some();
        self.clear_composed();
        removed
    }

    /// Configuration of one model provider.
    pub fn model_provider(&self, id: &str) -> Option<ModelProviderConfig> {
        self.model_providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .map(|config| (**config).clone())
    }

    /// Every model provider, built-in included, sorted by id. Never gated.
    pub fn list_model_providers(&self) -> Vec<ModelProviderConfig> {
        self.model_providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|config| (**config).clone())
            .collect()
    }

    fn clear_composed(&self) {
        self.composed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Opens a session on a harness instance, over the model provider the spec names.
    ///
    /// Without a [`ModelBinding`](super::ModelBinding) this is `get(harness).open(spec)`, unchanged. With one,
    /// **before anything starts**: the harness and the model provider are looked up, the
    /// pair is checked against the protocols the harness consumes (a mismatch is
    /// [`ProviderError::ModelProtocolMismatch`], naming both), then the binding is
    /// applied the way that harness takes it:
    ///
    /// - `native`: a provider composed from the harness and the model provider's endpoint;
    ///   the credential is resolved **for the model provider**, never for the harness.
    /// - `claude_code`: `ANTHROPIC_BASE_URL` and the key reach the child as explicit
    ///   variables, and the other authentication variable is set empty: the CLI sends
    ///   both headers when both exist, which would hand the host's own Anthropic key to
    ///   a third-party gateway (measured on the real CLI, 2.1.287).
    /// - `codex`, `acp`, `scripted`: `Unsupported { model_binding }`. Codex consumes the
    ///   Responses protocol, so a compatible pair passes the check, but writing its custom
    ///   provider configuration is not verified against a real `app-server`.
    ///
    /// The session's own `model` wins; else the binding's; else the model provider's
    /// default (aliases resolved).
    pub async fn open_session(
        &self,
        harness_id: &str,
        mut spec: SessionSpec,
    ) -> Result<Arc<dyn AgentSession>, ProviderError> {
        let Some(binding) = spec.model_binding.take() else {
            return self.get(harness_id)?.open(spec).await;
        };
        let harness = self
            .config(harness_id)
            .ok_or_else(|| ProviderError::invalid("unknown provider instance"))?;
        let provider = self
            .model_provider(&binding.provider)
            .ok_or_else(|| ProviderError::invalid("unknown model provider"))?;
        super::model_provider::check_pair(harness_id, harness.kind, &provider)?;
        if spec.model.is_none() {
            spec.model = provider.resolve_model(&binding);
        }
        match harness.kind {
            ProviderKind::Native => {
                self.check_gate(harness.kind)?;
                self.composed_native(&harness, &provider)?
                    .provider
                    .open(spec)
                    .await
            },
            ProviderKind::ClaudeCode => {
                self.bind_claude_code(&provider, &mut spec).await?;
                self.get(harness_id)?.open(spec).await
            },
            _ => Err(ProviderError::unsupported("model_binding")),
        }
    }

    /// The model provider's key, resolved for the model provider, into the variables a
    /// Claude Code child takes.
    async fn bind_claude_code(
        &self,
        provider: &ModelProviderConfig,
        spec: &mut SessionSpec,
    ) -> Result<(), ProviderError> {
        if provider.is_first_party() {
            return Ok(());
        }
        let Some(endpoint) = &provider.endpoint else {
            return Ok(());
        };
        let api_key = provider
            .extensions
            .get("anthropic_auth")
            .and_then(Value::as_str)
            == Some("api_key");
        let (used, shadowed) = if api_key {
            ("ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN")
        } else {
            ("ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY")
        };
        // The grant names the model provider: a harness cannot read a key meant for another.
        let secret = self
            .resolver
            .resolve(&provider.id, &provider.credential)
            .await?;
        spec.env
            .set
            .insert("ANTHROPIC_BASE_URL".to_owned(), endpoint.clone());
        spec.env.set.insert(
            used.to_owned(),
            secret.map(|s| s.expose().to_owned()).unwrap_or_default(),
        );
        spec.env.set.insert(shadowed.to_owned(), String::new());
        Ok(())
    }

    #[cfg(feature = "provider-native")]
    fn composed_native(
        &self,
        harness: &ProviderInstanceConfig,
        provider: &ModelProviderConfig,
    ) -> Result<BuiltProvider, ProviderError> {
        let key = (harness.id.clone(), provider.id.clone());
        if let Some(built) = self
            .composed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return Ok(built.clone());
        }
        let mut config = ProviderInstanceConfig::new(
            format!("{}--{}", harness.id, provider.id),
            ProviderKind::Native,
        );
        config.endpoint = provider.endpoint.clone();
        config.credential = provider.credential.clone();
        config.preset = provider.preset.clone();
        config.quirks = provider.quirks.clone();
        config.default_model = provider.default_model.clone();
        config.model_aliases = provider.model_aliases.clone();
        config.context_window = provider.context_window.or(harness.context_window);
        config.cost_source = provider.cost_source;
        config.prices = provider.prices.clone();
        config.extensions = harness.extensions.clone();
        config.extensions.extend(provider.extensions.clone());
        // The key is resolved for the model provider, not for the composed instance.
        let built = self.build_native_for(&config, &provider.id)?;
        Ok(self
            .composed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(key)
            .or_insert(built)
            .clone())
    }

    #[cfg(not(feature = "provider-native"))]
    fn composed_native(
        &self,
        _harness: &ProviderInstanceConfig,
        _provider: &ModelProviderConfig,
    ) -> Result<BuiltProvider, ProviderError> {
        Err(ProviderError::unsupported("provider_native"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;
    use crate::agent::{AgentSession, EnvCredentialResolver, ResumeToken, Secret, SessionSpec};

    /// Real-looking key value that must never appear anywhere the registry prints.
    const KEY: &str = "tok_Zq81mLpWx39vNbR2";

    struct Stub {
        id: String,
        kind: ProviderKind,
        marker: Option<String>,
        resolver: Arc<dyn CredentialResolver>,
        credential: CredentialRef,
    }

    #[async_trait]
    impl AgentProvider for Stub {
        fn id(&self) -> &str {
            &self.id
        }

        fn kind(&self) -> ProviderKind {
            self.kind
        }

        async fn health(&self) -> ProviderHealth {
            // Asks the resolver like a real provider does, per call.
            match self.resolver.resolve(&self.id, &self.credential).await {
                Ok(_) => ProviderHealth::ok(self.marker.clone()),
                Err(error) => ProviderHealth::unavailable(error),
            }
        }

        async fn catalog(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }

        fn capabilities(&self, _model: Option<&str>) -> Capabilities {
            Capabilities::none()
        }

        async fn open(&self, _spec: SessionSpec) -> Result<Arc<dyn AgentSession>, ProviderError> {
            Err(ProviderError::unsupported("open"))
        }

        async fn resume(
            &self,
            _spec: SessionSpec,
            _token: ResumeToken,
        ) -> Result<Arc<dyn AgentSession>, ProviderError> {
            Err(ProviderError::unsupported("resume"))
        }
    }

    fn stub_factory(builds: Arc<AtomicUsize>) -> KindFactory {
        Arc::new(move |config, resolver| {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok(BuiltProvider::new(Arc::new(Stub {
                id: config.id.clone(),
                kind: config.kind,
                marker: config.default_model.clone(),
                resolver,
                credential: config.credential.clone(),
            })))
        })
    }

    fn registry() -> ProviderRegistry {
        ProviderRegistry::new(Arc::new(EnvCredentialResolver))
    }

    fn open_gate(registry: &ProviderRegistry) {
        registry.activate_security_gate(SecurityGate::attest("test-batch"));
    }

    fn native(id: &str) -> ProviderInstanceConfig {
        ProviderInstanceConfig::native(id, "https://api.example.com/v1")
    }

    fn price(input: f64, output: f64) -> ModelPrice {
        ModelPrice {
            input_per_mtok: input,
            output_per_mtok: output,
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
        }
    }

    fn is_invalid<T>(result: Result<T, ProviderError>) -> bool {
        matches!(result, Err(ProviderError::InvalidRequest { .. }))
    }

    fn unsupported(error: &ProviderError, capability: &str) -> bool {
        matches!(error, ProviderError::Unsupported { capability: c } if c == capability)
    }

    // -- the gate ----------------------------------------------------------

    #[test]
    fn registry_refuses_third_party_instance_without_security_gate() {
        let registry = registry();
        for config in [
            native("deepseek"),
            ProviderInstanceConfig::new("codex-1", ProviderKind::Codex),
            ProviderInstanceConfig::new("agent", ProviderKind::Acp)
                .with_command(["agent", "--stdio"]),
        ] {
            let error = registry.upsert(config.clone()).expect_err("gate is shut");
            assert!(unsupported(&error, "security_gate"), "{error:?}");
            assert!(registry.config(&config.id).is_none(), "nothing was stored");
        }
        // Claude Code stays usable, and so does the test kit.
        registry
            .upsert(ProviderInstanceConfig::claude_code("claude-bedrock"))
            .expect("claude_code is not gated");
        registry
            .upsert(ProviderInstanceConfig::new(
                "scripted-1",
                ProviderKind::Scripted,
            ))
            .expect("scripted is not gated");
        assert!(registry.get("claude-code").is_ok());
        assert!(!registry.security_gate_active());
    }

    #[test]
    fn get_and_test_connection_refuse_a_third_party_instance_while_the_gate_is_shut() {
        let other = registry();
        // Directly: an entry present with the gate shut cannot be reached through `get`.
        other.entries.lock().unwrap().insert(
            "deepseek".to_owned(),
            Entry {
                config: Arc::new(native("deepseek")),
                built: None,
            },
        );
        let error = other.get("deepseek").err().expect("gate is shut");
        assert!(unsupported(&error, "security_gate"), "{error:?}");
        assert!(
            other.list().iter().any(|config| config.id == "deepseek"),
            "list shows it"
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let error = runtime
            .block_on(other.test_connection(&native("probe")))
            .expect_err("gate is shut");
        assert!(unsupported(&error, "security_gate"), "{error:?}");
    }

    #[test]
    fn activating_the_gate_lets_a_native_instance_in_and_it_stays_open() {
        let registry = registry();
        open_gate(&registry);
        registry
            .upsert(native("deepseek"))
            .expect("accepted once the gate is open");
        assert_eq!(registry.security_gate_batch(), Some("test-batch"));
        registry.activate_security_gate(SecurityGate::attest("another"));
        assert_eq!(
            registry.security_gate_batch(),
            Some("test-batch"),
            "first proof kept"
        );
        // The provider of the instance is built (no feature: no constructor, a clear refusal).
        match registry.get("deepseek") {
            Ok(provider) => assert_eq!(provider.kind(), ProviderKind::Native),
            Err(error) => assert!(unsupported(&error, "provider_native"), "{error:?}"),
        }
    }

    // -- built-in instance --------------------------------------------------

    #[test]
    fn claude_code_is_always_present_and_cannot_be_removed() {
        let registry = registry();
        assert!(registry.list().iter().any(|c| c.id == "claude-code"));
        assert!(!registry.remove("claude-code"));
        assert!(registry.list().iter().any(|c| c.id == "claude-code"));
        let provider = registry.get("claude-code").expect("built without the gate");
        assert_eq!(provider.id(), "claude-code");
        assert_eq!(provider.kind(), ProviderKind::ClaudeCode);
        // Reconfigurable as a claude_code, never as another kind.
        registry
            .upsert(
                ProviderInstanceConfig::claude_code("claude-code")
                    .with_extension("cli_path", json!("/opt/claude")),
            )
            .expect("same kind");
        open_gate(&registry);
        assert!(is_invalid(registry.upsert(native("claude-code"))));
        assert!(!registry.remove("unknown"));
    }

    // -- validation ---------------------------------------------------------

    #[test]
    fn validation_refuses_what_the_contract_refuses() {
        let registry = registry();
        open_gate(&registry);
        let cases = [
            ("empty id", native("")),
            ("upper case id", native("DeepSeek")),
            ("id with a slash", native("a/b")),
            ("long id", native(&"a".repeat(65))),
            (
                "native without endpoint",
                ProviderInstanceConfig::new("n", ProviderKind::Native),
            ),
            (
                "acp without command",
                ProviderInstanceConfig::new("a", ProviderKind::Acp),
            ),
            (
                "acp with an empty program",
                ProviderInstanceConfig::new("a", ProviderKind::Acp).with_command([" "]),
            ),
            (
                "acp with a key in its arguments",
                ProviderInstanceConfig::new("a", ProviderKind::Acp)
                    .with_command(["agent".to_owned(), format!("api_key={KEY}")]),
            ),
            (
                "ftp endpoint",
                ProviderInstanceConfig::native("n", "ftp://example.com"),
            ),
            (
                "endpoint without host",
                ProviderInstanceConfig::native("n", "https:///v1"),
            ),
            (
                "endpoint with user-info",
                ProviderInstanceConfig::native("n", format!("https://user:{KEY}@example.com/v1")),
            ),
            (
                "endpoint with a key in the query",
                ProviderInstanceConfig::native(
                    "n",
                    format!("https://example.com/v1?api_key={KEY}"),
                ),
            ),
            ("unknown preset", native("n").with_preset("mystery")),
            (
                "preset on claude_code",
                ProviderInstanceConfig::claude_code("c").with_preset("vllm"),
            ),
            (
                "endpoint on claude_code",
                ProviderInstanceConfig::claude_code("c").with_endpoint("https://example.com"),
            ),
            ("zero window", native("n").with_context_window(0)),
            (
                "negative price",
                native("n").with_price("m", price(-1.0, 1.0)),
            ),
            (
                "nan price",
                native("n").with_price("m", price(f64::NAN, 1.0)),
            ),
            ("empty alias", native("n").with_alias("", "m")),
            (
                "extension holding a credential",
                native("n").with_extension("api_key", json!(KEY)),
            ),
            (
                "nested credential in an extension",
                native("n").with_extension("proxy", json!({"auth": {"password": KEY}})),
            ),
            (
                "mistyped known extension",
                native("n").with_extension("allow_private_network", json!("yes")),
            ),
            ("bad env name", native("n").with_env_inherit(["A=B"])),
        ];
        for (name, config) in cases {
            let error = registry.upsert(config).expect_err(name);
            let text = format!("{error} {error:?}");
            assert!(
                matches!(error, ProviderError::InvalidRequest { .. }),
                "{name}: {error:?}"
            );
            assert!(!text.contains(KEY), "{name} echoed a value: {text}");
        }
        assert_eq!(registry.list().len(), 1, "nothing refused was stored");
        registry
            .upsert(
                native("good")
                    .with_preset("deepseek")
                    .with_extension("allow_private_network", json!(true)),
            )
            .expect("a valid configuration");
    }

    #[test]
    fn a_raw_key_is_never_accepted_as_a_credential_and_is_not_echoed() {
        for raw in [
            KEY,
            "sk-live-123456",
            "vault",
            "env:",
            "vault:has space",
            "Bearer abc",
        ] {
            let error = ProviderInstanceConfig::native("n", "https://example.com/v1")
                .with_credential_ref(raw)
                .expect_err(raw);
            assert!(matches!(error, ProviderError::InvalidRequest { .. }));
            let text = format!("{error} {error:?}");
            assert!(!text.contains(raw) || raw.len() < 6, "echoed {raw}: {text}");
            assert!(!text.contains(KEY));

            let json = json!({"id": "n", "kind": "native", "endpoint": "https://example.com/v1", "credential": raw});
            let error = ProviderInstanceConfig::from_json(&json).expect_err(raw);
            assert!(matches!(error, ProviderError::InvalidRequest { .. }));
            let text = format!("{error} {error:?}");
            assert!(!text.contains(KEY) && !text.contains("sk-live"), "{text}");
        }
        // A number or an object in the credential field is refused too.
        for bad in [json!(42), json!({"key": KEY})] {
            let json = json!({"id": "n", "kind": "native", "endpoint": "https://e.com", "credential": bad});
            let error = ProviderInstanceConfig::from_json(&json).expect_err("not a reference");
            assert!(!format!("{error:?}").contains(KEY));
        }
        for reference in ["vault:deepseek-key", "env:DEEPSEEK_API_KEY", "none"] {
            let json = json!({"id": "n", "kind": "native", "endpoint": "https://e.com", "credential": reference});
            let config = ProviderInstanceConfig::from_json(&json).expect(reference);
            assert_eq!(config.credential.to_string(), reference);
        }
    }

    #[test]
    fn json_with_an_unknown_kind_or_field_is_invalid_request() {
        let unknown_kind = json!({"id": "n", "kind": "telepathy"});
        assert!(is_invalid(ProviderInstanceConfig::from_json(&unknown_kind)));
        // A pasted key under a field name that does not exist is refused, not dropped.
        let pasted =
            json!({"id": "n", "kind": "native", "endpoint": "https://e.com", "api_key": KEY});
        let error = ProviderInstanceConfig::from_json(&pasted).expect_err("unknown field");
        assert!(!format!("{error} {error:?}").contains(KEY));
        let minimal = json!({"id": "n", "kind": "claude_code"});
        let config = ProviderInstanceConfig::from_json(&minimal).expect("defaults fill the rest");
        assert_eq!(config.credential, CredentialRef::None);
    }

    // -- no secret anywhere ---------------------------------------------------

    struct KeyResolver;

    #[async_trait]
    impl CredentialResolver for KeyResolver {
        async fn resolve(
            &self,
            _instance: &str,
            reference: &CredentialRef,
        ) -> Result<Option<Secret>, ProviderError> {
            match reference {
                CredentialRef::None => Ok(None),
                _ => Ok(Some(Secret::new(KEY))),
            }
        }
    }

    #[test]
    fn no_secret_value_in_debug_nor_in_json() {
        let registry = ProviderRegistry::new(Arc::new(KeyResolver));
        open_gate(&registry);
        let builds = Arc::new(AtomicUsize::new(0));
        registry.register_kind_factory(ProviderKind::Native, stub_factory(builds));
        let config = native("deepseek")
            .with_credential(CredentialRef::Vault("deepseek-key".to_owned()))
            .with_price("m", price(1.0, 2.0));
        registry.upsert(config.clone()).unwrap();
        let provider = registry.get("deepseek").unwrap();
        // The resolver holds the key and the provider asked for it: it still is nowhere.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(provider.health());
        let json = serde_json::to_string(&config).unwrap();
        let listed = serde_json::to_string(&registry.list()).unwrap();
        let printed = format!(
            "{config:?} {:?} {registry:?} {:?}",
            registry.list(),
            registry.price_table()
        );
        for text in [&json, &listed, &printed] {
            assert!(!text.contains(KEY), "secret leaked: {text}");
        }
        assert!(
            json.contains("vault:deepseek-key"),
            "the reference is serialised"
        );
        let back: ProviderInstanceConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, config, "round trip");
    }

    // -- locked vault -----------------------------------------------------------

    struct Locked;

    #[async_trait]
    impl CredentialResolver for Locked {
        async fn resolve(
            &self,
            _instance: &str,
            _reference: &CredentialRef,
        ) -> Result<Option<Secret>, ProviderError> {
            Err(ProviderError::CredentialsLocked)
        }
    }

    #[test]
    fn a_locked_vault_surfaces_as_is_and_nothing_falls_back() {
        let registry = ProviderRegistry::new(Arc::new(Locked));
        open_gate(&registry);
        registry.register_kind_factory(
            ProviderKind::Native,
            stub_factory(Arc::new(AtomicUsize::new(0))),
        );
        registry
            .upsert(native("deepseek").with_credential(CredentialRef::Vault("k".to_owned())))
            .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let health = runtime.block_on(registry.get("deepseek").unwrap().health());
        assert_eq!(health.status, HealthStatus::Unavailable);
        assert_eq!(health.error, Some(ProviderError::CredentialsLocked));
        // The registry never answers with another instance in its place.
        assert_eq!(registry.get("deepseek").unwrap().id(), "deepseek");
        let tested = runtime
            .block_on(registry.test_connection(
                &native("candidate").with_credential(CredentialRef::Vault("k".to_owned())),
            ))
            .unwrap();
        assert_eq!(tested.error, Some(ProviderError::CredentialsLocked));
    }

    // -- hot reload, cache --------------------------------------------------------

    #[test]
    fn get_caches_and_upsert_replaces_without_touching_open_providers() {
        let registry = registry();
        open_gate(&registry);
        let builds = Arc::new(AtomicUsize::new(0));
        registry.register_kind_factory(ProviderKind::Native, stub_factory(builds.clone()));
        registry
            .upsert(native("a").with_default_model("v1"))
            .unwrap();
        let first = registry.get("a").unwrap();
        let again = registry.get("a").unwrap();
        assert!(Arc::ptr_eq(&first, &again), "cached");
        assert_eq!(builds.load(Ordering::SeqCst), 1);

        registry
            .upsert(native("a").with_default_model("v2"))
            .unwrap();
        let second = registry.get("a").unwrap();
        assert!(!Arc::ptr_eq(&first, &second), "replaced");
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        // The provider a session was opened on keeps its configuration.
        assert_eq!(
            runtime.block_on(first.health()).version.as_deref(),
            Some("v1")
        );
        assert_eq!(
            runtime.block_on(second.health()).version.as_deref(),
            Some("v2")
        );
        assert_eq!(registry.list().iter().filter(|c| c.id == "a").count(), 1);

        assert!(registry.remove("a"));
        assert!(is_invalid(registry.get("a")));
        // The removed provider is still usable by whoever holds it.
        assert_eq!(
            runtime.block_on(second.health()).version.as_deref(),
            Some("v2")
        );
    }

    #[test]
    fn registering_a_factory_invalidates_built_instances_of_the_kind() {
        let registry = registry();
        let before = registry.get("claude-code").unwrap();
        registry.register_kind_factory(
            ProviderKind::ClaudeCode,
            stub_factory(Arc::new(AtomicUsize::new(0))),
        );
        let after = registry.get("claude-code").unwrap();
        assert!(!Arc::ptr_eq(&before, &after));
    }

    #[test]
    fn kinds_without_a_module_say_which_capability_is_missing() {
        let registry = registry();
        open_gate(&registry);
        registry
            .upsert(ProviderInstanceConfig::new("codex-1", ProviderKind::Codex))
            .unwrap();
        registry
            .upsert(ProviderInstanceConfig::new("agent", ProviderKind::Acp).with_command(["agent"]))
            .unwrap();
        // `codex` has a constructor behind the cargo feature `provider-codex`.
        let built = registry.get("codex-1");
        if cfg!(feature = "provider-codex") {
            assert!(built.is_ok(), "{:?}", built.err());
        } else {
            let error = built.err().expect("no constructor");
            assert!(unsupported(&error, "provider_codex"), "{error:?}");
        }
        let built = registry.get("agent");
        if cfg!(feature = "provider-acp") {
            assert!(built.is_ok(), "{:?}", built.err());
        } else {
            let error = built.err().expect("no constructor");
            assert!(unsupported(&error, "provider_acp"), "{error:?}");
        }
    }

    // -- aliases --------------------------------------------------------------------

    #[test]
    fn aliases_resolve_per_instance_and_unknown_ones_are_refused() {
        let registry = registry();
        open_gate(&registry);
        registry
            .upsert(
                native("a")
                    .with_alias("fast", "model-small")
                    .with_default_model("model-big")
                    .with_price("model-priced", price(1.0, 2.0)),
            )
            .unwrap();
        registry
            .upsert(native("b").with_alias("fast", "other-small"))
            .unwrap();
        registry.upsert(native("plain")).unwrap();
        assert_eq!(registry.resolve_alias("a", "fast").unwrap(), "model-small");
        assert_eq!(registry.resolve_alias("b", "fast").unwrap(), "other-small");
        // Direct ids the configuration knows pass as they are.
        for direct in ["model-small", "model-big", "model-priced"] {
            assert_eq!(registry.resolve_alias("a", direct).unwrap(), direct);
        }
        // An instance with aliases refuses a name it neither maps nor knows.
        assert!(is_invalid(registry.resolve_alias("a", "slow")));
        // An instance with no alias cannot contradict a direct id.
        assert_eq!(
            registry.resolve_alias("plain", "anything-v2").unwrap(),
            "anything-v2"
        );
        assert!(is_invalid(registry.resolve_alias("plain", "has space")));
        assert!(is_invalid(registry.resolve_alias("nobody", "fast")));
    }

    // -- prices -------------------------------------------------------------------------

    #[test]
    fn two_instances_with_the_same_model_name_keep_their_own_price() {
        let registry = registry();
        open_gate(&registry);
        registry
            .upsert(native("cheap").with_price("m", price(1.0, 2.0)))
            .unwrap();
        registry
            .upsert(native("dear").with_price("m", price(10.0, 20.0)))
            .unwrap();
        registry.upsert(native("unpriced")).unwrap();
        let book = registry.price_table();
        assert_eq!(book.get("cheap", "m"), Some(&price(1.0, 2.0)));
        assert_eq!(book.get("dear", "m"), Some(&price(10.0, 20.0)));
        assert_eq!(book.get("unpriced", "m"), None);
        let usage = Usage {
            input_tokens: Some(1_000_000),
            output_tokens: Some(1_000_000),
            ..Usage::default()
        };
        assert_eq!(book.cost_usd("cheap", "m", &usage), Some(3.0));
        assert_eq!(book.cost_usd("dear", "m", &usage), Some(30.0));
        assert_eq!(
            book.cost_usd("unpriced", "m", &usage),
            None,
            "no price, no cost: never zero"
        );
        assert_eq!(book.cost_usd("nobody", "m", &usage), None);
        assert_eq!(book.instances(), 4, "claude-code plus the three");
        // A change of price reaches the book on upsert.
        registry
            .upsert(native("cheap").with_price("m", price(5.0, 5.0)))
            .unwrap();
        assert_eq!(
            registry.price_table().get("cheap", "m"),
            Some(&price(5.0, 5.0))
        );
    }

    // -- quirks --------------------------------------------------------------------------

    #[test]
    fn quirks_are_the_preset_plus_the_override() {
        let config = native("n").with_preset("deepseek");
        assert!(config.effective_quirks().unwrap().echo_reasoning_with_tools);
        let mut over = EndpointQuirks::generic();
        over.omit_tool_choice = true;
        over.explicit_parallel_tool_calls = Some(false);
        let merged = config.with_quirks(over).effective_quirks().unwrap();
        assert!(merged.echo_reasoning_with_tools, "preset flag kept");
        assert!(merged.omit_tool_choice, "override flag added");
        assert_eq!(merged.explicit_parallel_tool_calls, Some(false));
        let vllm = native("n")
            .with_preset("vllm")
            .with_quirks(EndpointQuirks::generic());
        assert_eq!(vllm.effective_quirks().unwrap(), EndpointQuirks::vllm());
        assert_eq!(
            native("n").effective_quirks().unwrap(),
            EndpointQuirks::generic()
        );
    }

    // -- test_connection -------------------------------------------------------------------

    #[test]
    fn test_connection_registers_nothing_and_caches_nothing() {
        let registry = registry();
        open_gate(&registry);
        let builds = Arc::new(AtomicUsize::new(0));
        registry.register_kind_factory(ProviderKind::Native, stub_factory(builds.clone()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let health = runtime
            .block_on(registry.test_connection(&native("candidate")))
            .unwrap();
        assert_eq!(health.status, HealthStatus::Ok);
        assert_eq!(
            builds.load(Ordering::SeqCst),
            1,
            "an ephemeral instance was built"
        );
        assert!(registry.config("candidate").is_none());
        assert_eq!(registry.list().len(), 1);
        assert!(is_invalid(registry.get("candidate")));
        // An invalid configuration is an error, not an unavailable health.
        let error = runtime.block_on(
            registry.test_connection(&ProviderInstanceConfig::new("n", ProviderKind::Native)),
        );
        assert!(is_invalid(error));
        assert_eq!(builds.load(Ordering::SeqCst), 1, "nothing was built for it");
    }

    #[test]
    fn capabilities_for_goes_through_the_provider() {
        let registry = registry();
        let capabilities = registry.capabilities_for("claude-code", None).unwrap();
        assert!(capabilities.interactive_permissions);
        assert!(is_invalid(registry.capabilities_for("nobody", None)));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        // Nothing to probe on a Claude Code instance: the declared capabilities.
        let refreshed = runtime
            .block_on(registry.refresh_capabilities("claude-code", "m"))
            .unwrap();
        assert_eq!(
            refreshed,
            registry.capabilities_for("claude-code", Some("m")).unwrap()
        );
    }

    #[test]
    fn the_claude_code_instance_reads_its_extension_and_cost_source() {
        let config = ProviderInstanceConfig::claude_code("claude-sub")
            .with_cost_source(CostBasis::Subscription)
            .with_extension("cli_path", json!("/opt/claude"))
            .with_default_model("sonnet")
            .with_context_window(200_000)
            .with_price("sonnet", price(3.0, 15.0));
        config.validate().unwrap();
        let built = build_claude_code(&config);
        let capabilities = built.provider.capabilities(None);
        assert_eq!(capabilities.cost, CostBasis::Subscription);
        assert_eq!(capabilities.context_window.map(|w| w.value), Some(200_000));
        assert_eq!(built.provider.id(), "claude-sub");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let catalog = runtime.block_on(built.provider.catalog()).unwrap();
        assert_eq!(catalog.len(), 1);
        assert!(catalog[0].is_default);
        assert_eq!(catalog[0].pricing, Some(price(3.0, 15.0)));
    }
}
