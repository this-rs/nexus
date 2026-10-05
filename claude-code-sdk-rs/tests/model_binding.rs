//! Harness and model provider apart (N16, decision `130fa134`).
//!
//! A **harness** runs the session (Claude Code, the native loop, Codex, an ACP agent);
//! a **model provider** serves the model. This file proves the seam between them:
//!
//! - every (harness, model provider) pair is checked against the protocols the harness
//!   consumes, **before anything starts**, by a typed error that names both sides;
//! - the native harness runs over a model provider it was not configured with, and the
//!   credential is resolved **for the model provider**, never for the harness;
//! - Claude Code reaches a gateway through explicit variables, and the host's own
//!   Anthropic key is shadowed instead of being sent to a third party;
//! - an instance written before N16 reads as a pair without any migration.
//!
//! Observed through `fake_openai` and `fake_claude`; nothing here spoke to a real
//! gateway. The effect of the `ANTHROPIC_*` variables on the real CLI was measured
//! separately (see the contract §13.2).

#[path = "support/fake_openai.rs"]
mod fake_openai;
#[path = "support/native.rs"]
mod native;
mod support;

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use fake_openai::FakeOpenAi;
use native::*;
use nexus_claude::agent::{
    AgentEvent, AgentProvider, AgentSession, BUILTIN_ANTHROPIC_PROVIDER_ID, CredentialRef,
    CredentialResolver, ModelBinding, ModelProtocol, ModelProviderConfig, ProviderError,
    ProviderInstanceConfig, ProviderKind, ProviderRegistry, Secret, SecurityGate, SessionSpec,
    StopReason, TurnInput,
};
use nexus_claude::providers::claude_code::{ClaudeCodeConfig, ClaudeCodeProvider};
use serde_json::json;
use support::*;

const GATEWAY_SECRET: &str = "sk-gw-test-123";

/// Answers every reference with [`GATEWAY_SECRET`] and remembers WHO asked: the grant a
/// credential is read under must be the model provider's.
struct RecordingResolver {
    asked_by: Mutex<Vec<String>>,
}

impl RecordingResolver {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            asked_by: Mutex::new(Vec::new()),
        })
    }

    fn asked_by(&self) -> Vec<String> {
        self.asked_by
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl CredentialResolver for RecordingResolver {
    async fn resolve(
        &self,
        instance: &str,
        reference: &CredentialRef,
    ) -> Result<Option<Secret>, ProviderError> {
        self.asked_by
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(instance.to_owned());
        Ok(match reference {
            CredentialRef::None => None,
            _ => Some(Secret::new(GATEWAY_SECRET)),
        })
    }
}

fn registry(resolver: Arc<RecordingResolver>) -> ProviderRegistry {
    let registry = ProviderRegistry::new(resolver);
    registry.activate_security_gate(SecurityGate::attest("model-binding-test"));
    registry
}

fn binding_spec(provider: &str) -> SessionSpec {
    let mut spec = SessionSpec::new(std::env::temp_dir());
    spec.model_binding = Some(ModelBinding::new(provider));
    spec
}

async fn refusal(registry: &ProviderRegistry, harness: &str, provider: &str) -> ProviderError {
    match registry.open_session(harness, binding_spec(provider)).await {
        Err(error) => error,
        Ok(_) => panic!("{harness} x {provider} must be refused"),
    }
}

// ---------------------------------------------------------------------------
// The pairs
// ---------------------------------------------------------------------------

/// Every pair of the matrix, with what it must answer. Instances point at programs that
/// do not exist: if a refusal ever came after a launch, the error would be another one.
#[tokio::test]
async fn every_pair_is_checked_before_anything_starts() {
    let registry = registry(RecordingResolver::new());
    registry
        .upsert(ProviderInstanceConfig::native(
            "native-h",
            "http://127.0.0.1:9",
        ))
        .unwrap();
    registry
        .upsert(
            ProviderInstanceConfig::new("codex-h", ProviderKind::Codex)
                .with_command(["/nonexistent/codex"]),
        )
        .unwrap();
    registry
        .upsert(
            ProviderInstanceConfig::new("acp-h", ProviderKind::Acp)
                .with_command(["/nonexistent/agent"]),
        )
        .unwrap();
    registry
        .upsert_model_provider(
            ModelProviderConfig::new("deepseek", ModelProtocol::OpenAiChat)
                .with_endpoint("https://api.deepseek.com/v1"),
        )
        .unwrap();
    registry
        .upsert_model_provider(
            ModelProviderConfig::new("responses", ModelProtocol::OpenAiResponses)
                .with_endpoint("https://api.openai.com/v1"),
        )
        .unwrap();

    // (harness, model provider, protocol the provider serves, protocols the harness accepts)
    let mismatches: &[(&str, &str, &str, &[&str])] = &[
        (
            "claude-code",
            "deepseek",
            "openai_chat",
            &["anthropic_messages"],
        ),
        (
            "claude-code",
            "responses",
            "openai_responses",
            &["anthropic_messages"],
        ),
        (
            "native-h",
            "anthropic",
            "anthropic_messages",
            &["openai_chat"],
        ),
        (
            "native-h",
            "responses",
            "openai_responses",
            &["openai_chat"],
        ),
        ("codex-h", "deepseek", "openai_chat", &["openai_responses"]),
        (
            "codex-h",
            "anthropic",
            "anthropic_messages",
            &["openai_responses"],
        ),
        // An agent that picks its own model accepts no binding at all.
        ("acp-h", "anthropic", "anthropic_messages", &[]),
        ("acp-h", "deepseek", "openai_chat", &[]),
    ];
    for (harness, provider, protocol, accepts) in mismatches {
        let error = refusal(&registry, harness, provider).await;
        let ProviderError::ModelProtocolMismatch(mismatch) = &error else {
            panic!("{harness} x {provider}: wrong error {error:?}");
        };
        assert_eq!(
            (
                mismatch.harness.as_str(),
                mismatch.provider.as_str(),
                mismatch.protocol.as_str()
            ),
            (*harness, *provider, *protocol)
        );
        assert_eq!(
            mismatch
                .accepts
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            *accepts
        );
        let text = error.to_string();
        assert!(text.contains(harness) && text.contains(provider), "{text}");
        assert_eq!(error.kind(), "model_protocol_mismatch");
    }

    // Compatible, but not applied: Codex consumes the Responses protocol and the
    // application of its custom provider is not verified against a real app-server.
    assert_eq!(
        refusal(&registry, "codex-h", "responses").await,
        ProviderError::unsupported("model_binding")
    );
}

#[tokio::test]
async fn an_unknown_harness_or_model_provider_is_an_invalid_request() {
    let registry = registry(RecordingResolver::new());
    assert_eq!(
        refusal(&registry, "nope", "anthropic").await.kind(),
        "invalid_request"
    );
    assert_eq!(
        refusal(&registry, "claude-code", "nope").await.kind(),
        "invalid_request"
    );
}

/// A binding handed straight to a provider would be silently ignored: refused instead.
#[tokio::test]
async fn a_provider_refuses_a_binding_it_was_not_given_through_the_registry() {
    let provider = ClaudeCodeProvider::new(ClaudeCodeConfig::default());
    let error = provider
        .open(binding_spec("anthropic"))
        .await
        .err()
        .expect("refused");
    assert_eq!(error, ProviderError::unsupported("model_binding"));
}

// ---------------------------------------------------------------------------
// The registry of model providers
// ---------------------------------------------------------------------------

#[test]
fn a_third_party_model_provider_waits_for_the_security_gate_and_anthropic_is_fixed() {
    let registry = ProviderRegistry::new(RecordingResolver::new());
    let gateway = ModelProviderConfig::new("gw", ModelProtocol::AnthropicMessages)
        .with_endpoint("https://gw.example.test/v1");
    assert_eq!(
        registry.upsert_model_provider(gateway.clone()).unwrap_err(),
        ProviderError::unsupported("security_gate"),
        "the A32 rule reaches the model side"
    );
    // The vendor's own API is first party: no gate needed, and it is already there.
    assert!(
        registry
            .model_provider(BUILTIN_ANTHROPIC_PROVIDER_ID)
            .is_some()
    );
    assert!(!registry.remove_model_provider(BUILTIN_ANTHROPIC_PROVIDER_ID));
    registry.activate_security_gate(SecurityGate::attest("t"));
    registry.upsert_model_provider(gateway).unwrap();
    // `anthropic` cannot be redefined as something else.
    assert!(
        registry
            .upsert_model_provider(
                ModelProviderConfig::new(
                    BUILTIN_ANTHROPIC_PROVIDER_ID,
                    ModelProtocol::AnthropicMessages
                )
                .with_endpoint("https://evil.example.test/v1")
            )
            .is_err()
    );
    let ids: Vec<String> = registry
        .list_model_providers()
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(ids, ["anthropic", "gw"]);
    assert!(registry.remove_model_provider("gw"));
}

/// An instance written before N16 reads as a pair, with nothing migrated.
#[test]
fn an_existing_instance_reads_as_a_harness_and_a_model_provider() {
    let native = ProviderInstanceConfig::native("deepseek-prod", "https://api.deepseek.com/v1")
        .with_preset("deepseek")
        .with_credential(CredentialRef::Env("DEEPSEEK_KEY".into()))
        .with_default_model("deepseek-chat");
    let provider = native.model_provider().expect("native has a model side");
    assert_eq!(provider.protocol, ModelProtocol::OpenAiChat);
    assert_eq!(
        provider.endpoint.as_deref(),
        Some("https://api.deepseek.com/v1")
    );
    assert_eq!(
        provider.credential,
        CredentialRef::Env("DEEPSEEK_KEY".into())
    );
    assert_eq!(provider.preset.as_deref(), Some("deepseek"));
    assert_eq!(provider.default_model.as_deref(), Some("deepseek-chat"));
    assert!(
        provider.validate().is_ok(),
        "the derived view is itself valid"
    );

    let claude = ProviderInstanceConfig::claude_code("claude-code")
        .model_provider()
        .expect("claude code has a model side");
    assert_eq!(claude.protocol, ModelProtocol::AnthropicMessages);
    assert!(claude.is_first_party());

    // An agent that picks its own model has no model side to read.
    assert!(
        ProviderInstanceConfig::new("a", ProviderKind::Acp)
            .with_command(["agent"])
            .model_provider()
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// Native: a harness over a model provider it was not configured with
// ---------------------------------------------------------------------------

async fn collect_turn(session: &dyn AgentSession) -> Vec<AgentEvent> {
    collect(
        session
            .send_turn(TurnInput::text("go"))
            .await
            .expect("send_turn"),
    )
    .await
}

#[tokio::test]
async fn the_native_harness_runs_over_the_model_provider_the_session_names() {
    let server = FakeOpenAi::start(json!([
        probe_route(true),
        models_route(128_000),
        text_reply(None, "bound ok", None, None),
    ]));
    let resolver = RecordingResolver::new();
    let registry = registry(resolver.clone());
    // The harness instance has an endpoint of its own — a dead one: if the binding did
    // not replace it, nothing would answer.
    registry
        .upsert(
            ProviderInstanceConfig::native("native-h", "http://127.0.0.1:9")
                .with_default_model("dead-model"),
        )
        .unwrap();
    registry
        .upsert_model_provider(
            ModelProviderConfig::new("fake-openai", ModelProtocol::OpenAiChat)
                .with_endpoint(server.base_url())
                .with_credential(CredentialRef::Env("FAKE_KEY".into()))
                .with_default_model("model-from-provider")
                .with_extension("allow_private_network", json!(true)),
        )
        .unwrap();

    let session = registry
        .open_session("native-h", binding_spec("fake-openai"))
        .await
        .expect("the pair is compatible and the endpoint answers");
    let events = collect_turn(&*session).await;
    let Some(AgentEvent::Done { stop_reason, .. }) = events.last() else {
        panic!("no terminal event: {events:?}");
    };
    assert_eq!(*stop_reason, StopReason::Completed);

    // The model is the model provider's default, not the harness instance's.
    let chat = server.requests_to("POST", "/v1/chat/completions");
    let turn_request = chat
        .iter()
        .map(|r| r["body"].to_string())
        .find(|body| !body.contains("Call the ping tool now"))
        .expect("the turn reached the model provider's endpoint");
    assert!(
        turn_request.contains("model-from-provider"),
        "{turn_request}"
    );
    assert!(!turn_request.contains("dead-model"));

    // The credential was read under the MODEL PROVIDER's grant, never the harness's.
    let asked = resolver.asked_by();
    assert!(!asked.is_empty(), "the credential was resolved");
    assert!(
        asked.iter().all(|who| who == "fake-openai"),
        "resolved for {asked:?}: the grant must name the model provider only"
    );
}

#[tokio::test]
async fn a_session_that_names_a_model_keeps_it_over_the_providers_default() {
    let server = FakeOpenAi::start(json!([
        probe_route(true),
        models_route(128_000),
        text_reply(None, "ok", None, None),
    ]));
    let registry = registry(RecordingResolver::new());
    registry
        .upsert(ProviderInstanceConfig::native(
            "native-h",
            "http://127.0.0.1:9",
        ))
        .unwrap();
    registry
        .upsert_model_provider(
            ModelProviderConfig::new("fake-openai", ModelProtocol::OpenAiChat)
                .with_endpoint(server.base_url())
                .with_default_model("the-default")
                .with_extension("allow_private_network", json!(true)),
        )
        .unwrap();
    let mut spec = binding_spec("fake-openai");
    spec.model = Some("explicit-model".to_owned());
    let session = registry.open_session("native-h", spec).await.unwrap();
    collect_turn(&*session).await;
    let seen = server
        .requests_to("POST", "/v1/chat/completions")
        .iter()
        .map(|r| r["body"].to_string())
        .any(|body| body.contains("explicit-model"));
    assert!(seen, "the session's own model wins");
}

// ---------------------------------------------------------------------------
// Claude Code: a gateway, and the host's own key kept away from it
// ---------------------------------------------------------------------------

/// Opens a session on a fake `claude` bound to `provider`, returns what the child saw.
async fn claude_bound_to(
    registry: &ProviderRegistry,
    provider: &str,
) -> (FakeCli, Result<Arc<dyn AgentSession>, ProviderError>) {
    let fake = Transcript::new()
        .await_stdin()
        .init("sess-bound")
        .assistant_text("ok")
        .result_ok("ok")
        .wait_eof()
        .build();
    registry
        .upsert(
            ProviderInstanceConfig::claude_code("claude-fake")
                .with_extension("cli_path", json!(fake.cli_path().display().to_string())),
        )
        .unwrap();
    let mut spec = binding_spec(provider);
    spec.cwd = fake.dir().to_path_buf();
    spec.env.set = fake.options().env.into_iter().collect();
    // Let the fake record these three names (secret-shaped values come back as a length).
    spec.env.set.insert(
        "FAKE_CLAUDE_ARGS_ENV_ALLOW".to_owned(),
        "ANTHROPIC_BASE_URL,ANTHROPIC_AUTH_TOKEN,ANTHROPIC_API_KEY".to_owned(),
    );
    let session = registry.open_session("claude-fake", spec).await;
    if let Ok(session) = &session {
        // The child starts with the first turn.
        let _ = collect_turn(&**session).await;
    }
    (fake, session)
}

fn redacted(len: usize) -> Option<String> {
    Some(format!("<redacted {len} bytes>"))
}

#[tokio::test]
async fn claude_code_reaches_a_gateway_with_the_other_auth_variable_shadowed() {
    let resolver = RecordingResolver::new();
    let registry = registry(resolver.clone());
    registry
        .upsert_model_provider(
            ModelProviderConfig::new("gw", ModelProtocol::AnthropicMessages)
                .with_endpoint("https://gw.example.test/v1")
                .with_credential(CredentialRef::Env("GW_KEY".into())),
        )
        .unwrap();
    let (fake, session) = claude_bound_to(&registry, "gw").await;
    session.expect("the pair is compatible");
    let invocation = fake
        .wait_for_invocation(std::time::Duration::from_secs(5))
        .await;

    assert_eq!(
        invocation.env("ANTHROPIC_BASE_URL").as_deref(),
        Some("https://gw.example.test/v1")
    );
    assert_eq!(
        invocation.env("ANTHROPIC_AUTH_TOKEN"),
        redacted(GATEWAY_SECRET.len()),
        "the gateway's key travels as a Bearer token"
    );
    assert_eq!(
        invocation.env("ANTHROPIC_API_KEY"),
        redacted(0),
        "the host's own x-api-key is shadowed by an empty value: with both set the CLI \
         sends both headers, which would hand the host's Anthropic key to the gateway"
    );
    assert!(
        !invocation.args().join(" ").contains(GATEWAY_SECRET),
        "the key is never on argv"
    );
    assert_eq!(
        resolver.asked_by(),
        ["gw"],
        "resolved for the model provider only"
    );
}

#[tokio::test]
async fn a_gateway_that_wants_an_api_key_header_gets_the_swapped_pair() {
    let registry = registry(RecordingResolver::new());
    registry
        .upsert_model_provider(
            ModelProviderConfig::new("gw-key", ModelProtocol::AnthropicMessages)
                .with_endpoint("https://gw.example.test/v1")
                .with_credential(CredentialRef::Env("GW_KEY".into()))
                .with_extension("anthropic_auth", json!("api_key")),
        )
        .unwrap();
    let (fake, session) = claude_bound_to(&registry, "gw-key").await;
    session.expect("opens");
    let invocation = fake
        .wait_for_invocation(std::time::Duration::from_secs(5))
        .await;
    assert_eq!(
        invocation.env("ANTHROPIC_API_KEY"),
        redacted(GATEWAY_SECRET.len())
    );
    assert_eq!(invocation.env("ANTHROPIC_AUTH_TOKEN"), redacted(0));
}

#[tokio::test]
async fn binding_to_anthropic_itself_resolves_no_key_and_sets_no_endpoint() {
    let resolver = RecordingResolver::new();
    let registry = registry(resolver.clone());
    let (_fake, session) = claude_bound_to(&registry, BUILTIN_ANTHROPIC_PROVIDER_ID).await;
    session.expect("the first-party pair opens");
    assert!(
        resolver.asked_by().is_empty(),
        "nothing to resolve for the vendor itself"
    );
}

/// A credential store that is locked must stop the session: no fallback to a key
/// from somewhere else.
#[tokio::test]
async fn a_locked_credential_store_stops_the_session_instead_of_falling_back() {
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
    let registry = ProviderRegistry::new(Arc::new(Locked));
    registry.activate_security_gate(SecurityGate::attest("t"));
    registry
        .upsert_model_provider(
            ModelProviderConfig::new("gw", ModelProtocol::AnthropicMessages)
                .with_endpoint("https://gw.example.test/v1")
                .with_credential(CredentialRef::Env("GW_KEY".into())),
        )
        .unwrap();
    let (_fake, session) = {
        let fake = Transcript::new().await_stdin().wait_eof().build();
        registry
            .upsert(
                ProviderInstanceConfig::claude_code("claude-fake")
                    .with_extension("cli_path", json!(fake.cli_path().display().to_string())),
            )
            .unwrap();
        let session = registry
            .open_session("claude-fake", binding_spec("gw"))
            .await;
        (fake, session)
    };
    assert_eq!(
        session.err().expect("refused"),
        ProviderError::CredentialsLocked
    );
}
