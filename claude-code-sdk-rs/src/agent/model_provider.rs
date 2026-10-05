//! The model side of a session, apart from the harness (N16, decision `130fa134`).
//!
//! A **harness** is the agent loop that runs the session: Claude Code, Codex, an ACP
//! agent, the native loop. A **model provider** is what serves the model: Anthropic,
//! OpenAI, DeepSeek, a local Ollama or vLLM. Until N16 the two were fused in one
//! instance; here they are separate, so one harness can run over several model
//! providers and one model provider can serve several harnesses.
//!
//! They meet through a **protocol**: the wire format a harness speaks to its model.
//! A harness declares the protocols it can consume ([`ProviderKind::model_protocols`]);
//! a model provider declares the one it serves ([`ModelProviderConfig::protocol`]);
//! a pair whose protocols do not meet is refused **when the session opens**, by
//! [`ProviderError::ModelProtocolMismatch`], instead of failing late and obscurely.
//!
//! Credentials and the consent that goes with an origin follow the **model provider**,
//! never the harness: the grant the [`CredentialResolver`](super::CredentialResolver)
//! checks names the model provider, so a harness cannot read a key meant for another.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::credentials::CredentialRef;
use super::error::ProviderError;
use super::{CostBasis, ModelPrice, ProviderKind};
use crate::model::EndpointQuirks;

/// The wire format between a harness and its model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ModelProtocol {
    /// The Anthropic Messages API, and gateways that speak it. What Claude Code consumes.
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
    /// OpenAI-compatible `chat/completions` with SSE: OpenAI, DeepSeek, vLLM, Ollama,
    /// llama.cpp, NVIDIA NIM. What the native harness consumes.
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    /// The OpenAI Responses API. What Codex consumes for a custom provider.
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
}

impl ModelProtocol {
    /// Every protocol, in declaration order.
    pub const ALL: [ModelProtocol; 3] = [
        ModelProtocol::AnthropicMessages,
        ModelProtocol::OpenAiChat,
        ModelProtocol::OpenAiResponses,
    ];

    /// The serialised name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic_messages",
            Self::OpenAiChat => "openai_chat",
            Self::OpenAiResponses => "openai_responses",
        }
    }
}

impl fmt::Display for ModelProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ModelProtocol {
    type Err = ProviderError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|protocol| protocol.as_str() == text)
            .ok_or_else(|| ProviderError::invalid("unknown model protocol"))
    }
}

/// How a harness gets its model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolSupport {
    /// The harness consumes exactly these protocols.
    Fixed(&'static [ModelProtocol]),
    /// The agent picks its own model through its own configuration (an ACP agent such
    /// as opencode): nothing outside can bind one, and a binding is refused.
    AgentChosen,
}

impl ProviderKind {
    /// The protocols this harness can consume (the table of the contract §13.1).
    pub fn model_protocols(self) -> ProtocolSupport {
        match self {
            Self::ClaudeCode => ProtocolSupport::Fixed(&[ModelProtocol::AnthropicMessages]),
            Self::Native => ProtocolSupport::Fixed(&[ModelProtocol::OpenAiChat]),
            Self::Codex => ProtocolSupport::Fixed(&[ModelProtocol::OpenAiResponses]),
            Self::Acp | Self::Scripted => ProtocolSupport::AgentChosen,
        }
    }

    /// The protocol an instance of this kind already speaks when nothing else is said:
    /// what an instance written before N16 is read as. `None` for an agent that picks
    /// its own model.
    pub fn default_model_protocol(self) -> Option<ModelProtocol> {
        match self.model_protocols() {
            ProtocolSupport::Fixed(protocols) => protocols.first().copied(),
            ProtocolSupport::AgentChosen => None,
        }
    }
}

/// What a session asks of the model side: which model provider, and optionally which
/// model of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBinding {
    /// Id of a model provider registered in the [`ProviderRegistry`](super::ProviderRegistry).
    pub provider: String,
    /// Model of that provider; its default when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl ModelBinding {
    /// A binding to a model provider's default model.
    pub fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: None,
        }
    }

    /// The same binding, naming the model.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

/// A model provider: an endpoint that serves models in one protocol.
///
/// Holds no secret: `credential` is a reference (`vault:<name>`, `env:<VAR>`, `none`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ModelProviderConfig {
    /// Registry key: `[a-z0-9_-]`, at most 64 bytes. The grant of the credential names it.
    pub id: String,
    /// The protocol this provider serves.
    pub protocol: ModelProtocol,
    /// Base URL. Required for the OpenAI protocols; `None` for Anthropic's own API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Where the key lives. Never the key.
    #[serde(default)]
    pub credential: CredentialRef,
    /// Dialect preset of an `openai_chat` endpoint (`deepseek`, `vllm`, `ollama`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Dialect flags added to the preset's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quirks: Option<EndpointQuirks>,
    /// Model of a session that names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Alias → model id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_aliases: BTreeMap<String, String>,
    /// Context window of every model, when the operator knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Where `done.cost` comes from.
    #[serde(default)]
    pub cost_source: CostBasis,
    /// Prices by model id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prices: BTreeMap<String, ModelPrice>,
    /// Protocol-specific keys. No secret. `anthropic_auth` (`bearer`, the default, or
    /// `api_key`) chooses how a gateway wants its key presented.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

impl ModelProviderConfig {
    /// A provider with no endpoint, no credential and nothing else.
    pub fn new(id: impl Into<String>, protocol: ModelProtocol) -> Self {
        Self {
            id: id.into(),
            protocol,
            endpoint: None,
            credential: CredentialRef::None,
            preset: None,
            quirks: None,
            default_model: None,
            model_aliases: BTreeMap::new(),
            context_window: None,
            cost_source: CostBasis::Unknown,
            prices: BTreeMap::new(),
            extensions: BTreeMap::new(),
        }
    }

    /// Sets the base URL.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Sets the credential reference.
    pub fn with_credential(mut self, credential: CredentialRef) -> Self {
        self.credential = credential;
        self
    }

    /// Sets the default model.
    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Adds a protocol-specific key.
    pub fn with_extension(mut self, key: impl Into<String>, value: Value) -> Self {
        self.extensions.insert(key.into(), value);
        self
    }

    /// Whether this is the vendor's own API (Anthropic's), reached with no endpoint of
    /// the operator's: the one model provider that is first party.
    pub fn is_first_party(&self) -> bool {
        self.protocol == ModelProtocol::AnthropicMessages && self.endpoint.is_none()
    }

    /// Refuses a configuration that cannot serve a session safely.
    pub fn validate(&self) -> Result<(), ProviderError> {
        let bad = |detail: &str| Err(ProviderError::invalid(detail));
        if !super::registry::valid_instance_id(&self.id) {
            return bad("model provider id must be 1-64 characters of [a-z0-9_-]");
        }
        match (self.protocol, &self.endpoint) {
            (ModelProtocol::OpenAiChat | ModelProtocol::OpenAiResponses, None) => {
                return bad("an OpenAI-protocol model provider requires an endpoint");
            },
            (_, Some(endpoint)) => super::registry::check_endpoint(endpoint)?,
            (ModelProtocol::AnthropicMessages, None) => {},
            #[allow(unreachable_patterns)]
            _ => return bad("unknown model protocol"),
        }
        if self.protocol != ModelProtocol::OpenAiChat
            && (self.preset.is_some() || self.quirks.is_some())
        {
            return bad("preset and quirks apply to openai_chat providers only");
        }
        if let Some(preset) = &self.preset
            && EndpointQuirks::preset(preset).is_none()
        {
            return bad("unknown endpoint preset");
        }
        if self.default_model.as_deref().is_some_and(str::is_empty) {
            return bad("default_model must not be empty");
        }
        if self
            .model_aliases
            .iter()
            .any(|(alias, model)| alias.is_empty() || model.is_empty())
        {
            return bad("model aliases must map a name to a model id, both non-empty");
        }
        if self.context_window == Some(0) {
            return bad("context_window must be positive");
        }
        let figure_ok = |figure: f64| figure.is_finite() && figure >= 0.0;
        if self.prices.iter().any(|(model, price)| {
            model.is_empty()
                || !figure_ok(price.input_per_mtok)
                || !figure_ok(price.output_per_mtok)
                || price.cache_read_per_mtok.is_some_and(|f| !figure_ok(f))
                || price.cache_write_per_mtok.is_some_and(|f| !figure_ok(f))
        }) {
            return bad("prices must be finite and not negative");
        }
        if let Some(auth) = self.extensions.get("anthropic_auth")
            && !matches!(auth.as_str(), Some("bearer" | "api_key"))
        {
            return bad("anthropic_auth is `bearer` or `api_key`");
        }
        if self
            .extensions
            .keys()
            .any(|key| super::credentials::is_sensitive_name(key))
        {
            return bad("extensions must not carry credentials: use a credential reference");
        }
        Ok(())
    }

    /// The model a session asks for: the binding's, else this provider's default, with
    /// aliases resolved. `None` when nothing names one.
    pub fn resolve_model(&self, binding: &ModelBinding) -> Option<String> {
        let wanted = binding.model.as_ref().or(self.default_model.as_ref())?;
        Some(
            self.model_aliases
                .get(wanted)
                .cloned()
                .unwrap_or_else(|| wanted.clone()),
        )
    }
}

/// The error of a pair whose protocols do not meet.
pub(crate) fn mismatch(
    harness: &str,
    kind: ProviderKind,
    provider: &ModelProviderConfig,
) -> ProviderError {
    let accepts = match kind.model_protocols() {
        ProtocolSupport::Fixed(protocols) => protocols.iter().map(|p| p.to_string()).collect(),
        ProtocolSupport::AgentChosen => Vec::new(),
    };
    ProviderError::ModelProtocolMismatch(Box::new(super::error::ProtocolMismatch {
        harness: harness.to_owned(),
        provider: provider.id.clone(),
        protocol: provider.protocol.to_string(),
        accepts,
    }))
}

/// Checks that `kind` can consume what `provider` serves.
pub(crate) fn check_pair(
    harness: &str,
    kind: ProviderKind,
    provider: &ModelProviderConfig,
) -> Result<(), ProviderError> {
    match kind.model_protocols() {
        ProtocolSupport::Fixed(protocols) if protocols.contains(&provider.protocol) => Ok(()),
        _ => Err(mismatch(harness, kind, provider)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table of the contract §13.1: which harness consumes which protocol. A new
    /// harness or protocol must change this test on purpose.
    #[test]
    fn each_harness_declares_the_protocols_it_consumes() {
        let table = [
            (
                ProviderKind::ClaudeCode,
                Some(ModelProtocol::AnthropicMessages),
            ),
            (ProviderKind::Native, Some(ModelProtocol::OpenAiChat)),
            (ProviderKind::Codex, Some(ModelProtocol::OpenAiResponses)),
            (ProviderKind::Acp, None),
            (ProviderKind::Scripted, None),
        ];
        for (kind, expected) in table {
            assert_eq!(kind.default_model_protocol(), expected, "{kind:?}");
            match (kind.model_protocols(), expected) {
                (ProtocolSupport::Fixed(protocols), Some(protocol)) => {
                    assert_eq!(
                        protocols,
                        [protocol],
                        "{kind:?} consumes exactly one protocol"
                    )
                },
                (ProtocolSupport::AgentChosen, None) => {},
                other => panic!("{kind:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_protocol_round_trips_through_its_name() {
        for protocol in ModelProtocol::ALL {
            assert_eq!(
                protocol.as_str().parse::<ModelProtocol>().unwrap(),
                protocol
            );
            assert_eq!(
                serde_json::to_string(&protocol).unwrap(),
                format!("\"{}\"", protocol.as_str())
            );
        }
        assert!("openai".parse::<ModelProtocol>().is_err());
    }

    #[test]
    fn every_pair_is_checked_against_the_declared_table() {
        for kind in [
            ProviderKind::ClaudeCode,
            ProviderKind::Native,
            ProviderKind::Codex,
            ProviderKind::Acp,
            ProviderKind::Scripted,
        ] {
            for protocol in ModelProtocol::ALL {
                let provider = ModelProviderConfig::new("p", protocol);
                let accepted = matches!(
                    kind.model_protocols(),
                    ProtocolSupport::Fixed(protocols) if protocols.contains(&protocol)
                );
                assert_eq!(
                    check_pair("h", kind, &provider).is_ok(),
                    accepted,
                    "{kind:?} x {protocol}"
                );
            }
        }
    }

    #[test]
    fn a_mismatch_names_both_sides_and_what_the_harness_accepts() {
        let provider = ModelProviderConfig::new("deepseek", ModelProtocol::OpenAiChat);
        let error = check_pair("claude-code", ProviderKind::ClaudeCode, &provider).unwrap_err();
        let text = error.to_string();
        for needle in [
            "claude-code",
            "deepseek",
            "openai_chat",
            "anthropic_messages",
        ] {
            assert!(text.contains(needle), "{needle} missing from: {text}");
        }
        assert_eq!(error.kind(), "model_protocol_mismatch");
        assert_eq!(error.http_status_hint(), 422);
    }

    #[test]
    fn the_validation_refuses_what_cannot_serve_safely() {
        let ok = ModelProviderConfig::new("deepseek", ModelProtocol::OpenAiChat)
            .with_endpoint("https://api.deepseek.com/v1");
        assert!(ok.validate().is_ok());
        // An OpenAI protocol needs an endpoint; Anthropic's own API does not.
        assert!(
            ModelProviderConfig::new("o", ModelProtocol::OpenAiChat)
                .validate()
                .is_err()
        );
        assert!(
            ModelProviderConfig::new("anthropic", ModelProtocol::AnthropicMessages)
                .validate()
                .is_ok()
        );
        // A bad id, a preset on the wrong protocol, a wrong auth style, a credential-shaped key.
        assert!(
            ModelProviderConfig::new("Bad Id", ModelProtocol::AnthropicMessages)
                .validate()
                .is_err()
        );
        let mut preset = ModelProviderConfig::new("g", ModelProtocol::AnthropicMessages);
        preset.preset = Some("deepseek".into());
        assert!(preset.validate().is_err());
        assert!(
            ModelProviderConfig::new("g", ModelProtocol::AnthropicMessages)
                .with_extension("anthropic_auth", Value::from("basic"))
                .validate()
                .is_err()
        );
        assert!(
            ModelProviderConfig::new("g", ModelProtocol::AnthropicMessages)
                .with_extension("api_key", Value::from("sk-x"))
                .validate()
                .is_err()
        );
        // An unknown field is refused, so a pasted `api_key` cannot be dropped silently.
        assert!(
            serde_json::from_value::<ModelProviderConfig>(
                serde_json::json!({"id": "x", "protocol": "openai_chat", "api_key": "k"})
            )
            .is_err()
        );
    }

    #[test]
    fn only_the_vendors_own_anthropic_api_is_first_party() {
        let anthropic = ModelProviderConfig::new("anthropic", ModelProtocol::AnthropicMessages);
        assert!(anthropic.is_first_party());
        assert!(
            !anthropic
                .clone()
                .with_endpoint("https://gw.example/v1")
                .is_first_party()
        );
        assert!(!ModelProviderConfig::new("o", ModelProtocol::OpenAiChat).is_first_party());
    }

    #[test]
    fn the_model_of_a_session_is_the_bindings_then_the_default_with_aliases_resolved() {
        let mut provider = ModelProviderConfig::new("p", ModelProtocol::OpenAiChat)
            .with_endpoint("https://x.example/v1")
            .with_default_model("fast");
        provider
            .model_aliases
            .insert("fast".into(), "model-small".into());
        assert_eq!(
            provider.resolve_model(&ModelBinding::new("p")).as_deref(),
            Some("model-small")
        );
        assert_eq!(
            provider
                .resolve_model(&ModelBinding::new("p").with_model("model-big"))
                .as_deref(),
            Some("model-big")
        );
        provider.default_model = None;
        assert_eq!(provider.resolve_model(&ModelBinding::new("p")), None);
    }
}
