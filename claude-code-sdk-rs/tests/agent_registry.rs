//! `ProviderRegistry` over a real native provider: an instance created from a
//! configuration, built lazily, talking to the `fake_openai` server. No network
//! beyond 127.0.0.1.
//!
//! What this file proves, beyond the unit tests of `agent/registry.rs`: the key a
//! resolver hands out reaches the endpoint (an `Authorization` header is present)
//! and no error or printed form holds it; a locked vault is reported as such with
//! no request sent; `test_connection` of an address that does not answer is an
//! `Unavailable` health and leaves the registry as it was.

#[path = "support/fake_openai.rs"]
mod fake_openai;
#[path = "support/native.rs"]
mod native;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use fake_openai::FakeOpenAi;
use native::{models_route, probe_route};
use nexus_claude::agent::{
    CredentialRef, CredentialResolver, HealthStatus, ModelPrice, ProviderError,
    ProviderInstanceConfig, ProviderKind, ProviderRegistry, Secret, SecurityGate,
};
use serde_json::json;

const KEY: &str = "tok_Zq81mLpWx39vNbR2";

/// Answers `KEY` for `vault:` references and counts the requests.
struct VaultResolver(AtomicUsize);

#[async_trait]
impl CredentialResolver for VaultResolver {
    async fn resolve(
        &self,
        instance: &str,
        reference: &CredentialRef,
    ) -> Result<Option<Secret>, ProviderError> {
        match reference {
            CredentialRef::None => Ok(None),
            CredentialRef::Vault(_) => {
                assert_eq!(instance, "deepseek-prod", "the instance id is the asker's");
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Some(Secret::new(KEY)))
            },
            _ => Err(ProviderError::AuthRequired { login_hint: None }),
        }
    }
}

struct LockedVault;

#[async_trait]
impl CredentialResolver for LockedVault {
    async fn resolve(
        &self,
        _instance: &str,
        _reference: &CredentialRef,
    ) -> Result<Option<Secret>, ProviderError> {
        Err(ProviderError::CredentialsLocked)
    }
}

fn open_registry(resolver: Arc<dyn CredentialResolver>) -> ProviderRegistry {
    let registry = ProviderRegistry::new(resolver);
    registry.activate_security_gate(SecurityGate::attest("registry-test"));
    registry
}

fn instance(server: &FakeOpenAi) -> ProviderInstanceConfig {
    ProviderInstanceConfig::native("deepseek-prod", server.base_url())
        .with_preset("deepseek")
        .with_credential(CredentialRef::Vault("deepseek-key".to_owned()))
        .with_default_model("m")
        .with_price(
            "m",
            ModelPrice {
                input_per_mtok: 1.0,
                output_per_mtok: 2.0,
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
        )
}

fn assert_no_key(text: &str) {
    assert!(!text.contains(KEY), "credential leaked: {text}");
}

#[tokio::test]
async fn a_native_instance_created_from_config_serves_its_catalog_and_health() {
    let server = FakeOpenAi::start(json!([
        models_route(128_000),
        models_route(128_000),
        probe_route(true)
    ]));
    let resolver = Arc::new(VaultResolver(AtomicUsize::new(0)));
    let registry = open_registry(resolver.clone());
    registry
        .upsert(instance(&server))
        .expect("a native instance");

    let provider = registry.get("deepseek-prod").expect("built on first use");
    assert_eq!(provider.kind(), ProviderKind::Native);
    assert_eq!(provider.id(), "deepseek-prod");
    assert_eq!(
        resolver.0.load(Ordering::SeqCst),
        0,
        "nothing is resolved at construction"
    );

    let catalog = provider
        .catalog()
        .await
        .expect("catalog through the endpoint");
    let ids: Vec<&str> = catalog.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, ["m", "other"]);
    assert!(catalog[0].is_default, "the instance's default model");
    assert_eq!(
        catalog[0].pricing.map(|p| p.output_per_mtok),
        Some(2.0),
        "the instance's price"
    );
    assert_eq!(catalog[1].pricing, None, "no price for the other one");

    let health = provider.health().await;
    assert_eq!(health.status, HealthStatus::Ok, "{health:?}");
    assert_eq!(
        resolver.0.load(Ordering::SeqCst),
        2,
        "the key is asked for per request"
    );

    // The resolved key was sent as an Authorization header (presence only: the fake
    // never records the value), and is nowhere else.
    let sent = server.requests_to("GET", "/v1/models");
    assert_eq!(sent.len(), 2);
    assert!(
        sent.iter()
            .all(|request| request["authorization_present"] == true)
    );
    assert_no_key(&server.raw_log());
    assert_no_key(&format!("{registry:?} {:?}", registry.list()));

    // Capabilities: nothing claimed before the probe, then what the probe saw.
    assert!(
        !registry
            .capabilities_for("deepseek-prod", Some("m"))
            .unwrap()
            .tools
    );
    let probed = registry
        .refresh_capabilities("deepseek-prod", "m")
        .await
        .expect("probe");
    assert!(probed.tools && probed.thinking);
    assert_eq!(
        registry
            .capabilities_for("deepseek-prod", Some("m"))
            .unwrap(),
        probed
    );
}

#[tokio::test]
async fn the_key_never_appears_in_an_error_even_when_the_endpoint_echoes_it() {
    let server = FakeOpenAi::start(json!([{
        "method": "GET", "path": "/v1/models", "status": 401,
        "body": {"error": {"message": format!("invalid api key {KEY} for this account")}}
    }]));
    let registry = open_registry(Arc::new(VaultResolver(AtomicUsize::new(0))));
    registry.upsert(instance(&server)).unwrap();
    let provider = registry.get("deepseek-prod").unwrap();
    let error = provider.catalog().await.expect_err("401");
    assert_no_key(&format!(
        "{error} {error:?} {}",
        serde_json::to_string(&error).unwrap()
    ));
    let health = provider.health().await;
    assert_eq!(health.status, HealthStatus::Unavailable);
    assert_no_key(&format!(
        "{health:?} {}",
        serde_json::to_string(&health).unwrap()
    ));
    assert_no_key(&format!(
        "{:?}",
        registry.test_connection(&instance(&server)).await
    ));
}

#[tokio::test]
async fn a_locked_vault_is_reported_and_no_request_is_sent() {
    let server = FakeOpenAi::start(json!([models_route(1000)]));
    let registry = open_registry(Arc::new(LockedVault));
    registry.upsert(instance(&server)).unwrap();
    let provider = registry.get("deepseek-prod").unwrap();
    assert_eq!(
        provider.catalog().await.err(),
        Some(ProviderError::CredentialsLocked)
    );
    let health = provider.health().await;
    assert_eq!(health.status, HealthStatus::Unavailable);
    assert_eq!(health.error, Some(ProviderError::CredentialsLocked));
    let tested = registry.test_connection(&instance(&server)).await.unwrap();
    assert_eq!(tested.error, Some(ProviderError::CredentialsLocked));
    assert!(
        server.requests().is_empty(),
        "an unauthenticated request must not replace the authenticated one"
    );
}

#[tokio::test]
async fn test_connection_of_an_address_that_does_not_answer_is_unavailable_and_registers_nothing() {
    let registry = open_registry(Arc::new(VaultResolver(AtomicUsize::new(0))));
    // Nothing listens on port 1.
    let dead = ProviderInstanceConfig::native("deepseek-prod", "http://127.0.0.1:1/v1")
        .with_credential(CredentialRef::Vault("deepseek-key".to_owned()));
    let health = registry
        .test_connection(&dead)
        .await
        .expect("an answer, not an error");
    assert_eq!(health.status, HealthStatus::Unavailable);
    assert!(
        matches!(
            health.error,
            Some(ProviderError::EndpointUnreachable { .. })
        ),
        "{health:?}"
    );
    assert!(registry.config("deepseek-prod").is_none());
    assert_eq!(registry.list().len(), 1, "only the built-in claude-code");
    assert!(registry.get("deepseek-prod").is_err());
}

#[tokio::test]
async fn test_connection_of_a_working_endpoint_probes_the_default_model_and_registers_nothing() {
    let server = FakeOpenAi::start(json!([models_route(128_000), probe_route(true)]));
    let registry = open_registry(Arc::new(VaultResolver(AtomicUsize::new(0))));
    let health = registry.test_connection(&instance(&server)).await.unwrap();
    assert_eq!(health.status, HealthStatus::Ok, "{health:?}");
    assert_eq!(
        server.requests_to("POST", "/v1/chat/completions").len(),
        1,
        "the tool-call probe ran"
    );
    assert!(registry.config("deepseek-prod").is_none());
    assert_eq!(registry.list().len(), 1);

    // A model that cannot call tools is a degraded instance, not a healthy one.
    let no_tools = FakeOpenAi::start(json!([
        models_route(1000),
        {"method": "POST", "path": "/v1/chat/completions", "status": 200,
         "sse": [native::delta(json!({"content": "no tool for you"})), native::finish("stop"), json!("[DONE]")]}
    ]));
    let health = registry
        .test_connection(&instance(&no_tools))
        .await
        .unwrap();
    assert_eq!(health.status, HealthStatus::Degraded, "{health:?}");
}

#[tokio::test]
async fn a_hot_reloaded_instance_points_the_next_get_at_the_new_endpoint() {
    let first = FakeOpenAi::start(json!([models_route(1000)]));
    let second = FakeOpenAi::start(json!([models_route(1000)]));
    let registry = open_registry(Arc::new(VaultResolver(AtomicUsize::new(0))));
    registry.upsert(instance(&first)).unwrap();
    let old = registry.get("deepseek-prod").unwrap();
    registry.upsert(instance(&second)).unwrap();
    let new = registry.get("deepseek-prod").unwrap();
    old.catalog().await.unwrap();
    new.catalog().await.unwrap();
    assert_eq!(
        first.requests_to("GET", "/v1/models").len(),
        1,
        "the provider already handed out keeps its endpoint"
    );
    assert_eq!(second.requests_to("GET", "/v1/models").len(), 1);
}

#[tokio::test]
async fn the_gate_keeps_a_native_instance_out_until_it_is_activated() {
    let server = FakeOpenAi::start(json!([models_route(1000)]));
    let registry = ProviderRegistry::new(Arc::new(VaultResolver(AtomicUsize::new(0))));
    let refused = registry
        .upsert(instance(&server))
        .expect_err("gate is shut");
    assert_eq!(refused, ProviderError::unsupported("security_gate"));
    registry.activate_security_gate(SecurityGate::attest("registry-test"));
    registry
        .upsert(instance(&server))
        .expect("accepted after activation");
    assert!(registry.get("deepseek-prod").is_ok());
}
