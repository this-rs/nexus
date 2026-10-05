//! The `acp` kind of `ProviderRegistry` (cargo feature `provider-acp`): an instance
//! described as configuration becomes an `AcpProvider`, the security gate (A32) still
//! refuses it while shut, and the instance's environment reaches the agent as names
//! given in configuration, never as a credential.
//!
//! Like the rest of the ACP tests this runs against `fake_acp`; no real ACP agent was
//! involved.

use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use nexus_claude::agent::{
    AgentEvent, CostBasis, EnvCredentialResolver, HealthStatus, ModelPrice, ProviderError,
    ProviderInstanceConfig, ProviderKind, ProviderRegistry, SecurityGate, SessionSpec, TurnInput,
};
use nexus_claude::providers::acp::SUPPORTED_PROTOCOL_VERSION;
use serde_json::{Value, json};

const FAKE: &str = env!("CARGO_BIN_EXE_fake_acp");

fn transcript(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/transcripts/acp")
        .join(SUPPORTED_PROTOCOL_VERSION.to_string())
        .join("sessions")
        .join(format!("{name}.jsonl"))
        .display()
        .to_string()
}

fn instance(health: &str) -> ProviderInstanceConfig {
    ProviderInstanceConfig::new("acp-prod", ProviderKind::Acp)
        .with_command([FAKE, "--stdio"])
        .with_default_model("fake-model")
        .with_cost_source(CostBasis::Priced)
        .with_price(
            "fake-model",
            ModelPrice {
                input_per_mtok: 1.0,
                output_per_mtok: 2.0,
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
        )
        .with_extension("env", json!({ "FAKE_ACP_TRANSCRIPT": transcript(health) }))
        .with_extension("thinking", Value::Bool(true))
}

#[test]
fn the_gate_still_refuses_an_acp_instance_until_the_security_batch_is_active() {
    let registry = ProviderRegistry::new(Arc::new(EnvCredentialResolver));
    let error = registry
        .upsert(instance("health"))
        .expect_err("gate is shut");
    assert_eq!(error, ProviderError::unsupported("security_gate"));
    assert!(registry.config("acp-prod").is_none());
    registry.activate_security_gate(SecurityGate::attest("acp-test"));
    registry
        .upsert(instance("health"))
        .expect("the gate is open");
    let provider = registry
        .get("acp-prod")
        .expect("a constructor exists for acp");
    assert_eq!(provider.kind(), ProviderKind::Acp);
    assert_eq!(provider.id(), "acp-prod");
    assert!(provider.capabilities(None).thinking);
    assert_eq!(provider.capabilities(None).cost, CostBasis::Priced);
}

#[test]
fn an_acp_instance_needs_a_command_without_a_secret_and_well_typed_extensions() {
    let registry = ProviderRegistry::new(Arc::new(EnvCredentialResolver));
    registry.activate_security_gate(SecurityGate::attest("acp-test"));
    let no_command = ProviderInstanceConfig::new("acp-prod", ProviderKind::Acp);
    assert_eq!(
        registry.upsert(no_command).unwrap_err().kind(),
        "invalid_request"
    );
    let leaky = instance("health").with_command([FAKE, "--api-key=sk-registry-Zq81mLpWx39vNbR2"]);
    assert_eq!(
        registry.upsert(leaky).unwrap_err().kind(),
        "invalid_request"
    );
    // A credential-named variable in `env` is refused: secrets are not configuration.
    let secret_env = instance("health").with_extension(
        "env",
        json!({ "OPENCODE_API_KEY": "sk-registry-Zq81mLpWx39vNbR2" }),
    );
    assert_eq!(
        registry.upsert(secret_env).unwrap_err().kind(),
        "invalid_request"
    );
    let typed = instance("health").with_extension("env", json!({ "A": 1 }));
    assert_eq!(
        registry.upsert(typed).unwrap_err().kind(),
        "invalid_request"
    );
    let typed = instance("health").with_extension("thinking", json!("yes"));
    assert_eq!(
        registry.upsert(typed).unwrap_err().kind(),
        "invalid_request"
    );
}

#[tokio::test]
async fn an_instance_built_by_the_registry_learns_the_agent_and_runs_a_turn() {
    let registry = ProviderRegistry::new(Arc::new(EnvCredentialResolver));
    registry.activate_security_gate(SecurityGate::attest("acp-test"));
    let cwd = tempfile::tempdir().unwrap();
    registry.upsert(instance("health")).unwrap();
    let provider = registry.get("acp-prod").unwrap();
    assert!(!provider.capabilities(None).resume, "not learned yet");
    let health = provider.health().await;
    assert_eq!(health.status, HealthStatus::Ok, "{health:?}");
    assert!(
        provider.capabilities(None).resume,
        "loadSession was announced"
    );
    let mut spec = SessionSpec::new(cwd.path());
    spec.model = Some("fake-model".to_owned());
    spec.env
        .set
        .insert("FAKE_ACP_TRANSCRIPT".to_owned(), transcript("plain"));
    let session = provider.open(spec).await.expect("opens");
    let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    let mut done = None;
    while let Some(event) = stream.next().await {
        if let AgentEvent::Done {
            stop_reason, cost, ..
        } = &event
        {
            done = Some((*stop_reason, cost.usd.is_some()));
        }
    }
    assert_eq!(
        done,
        Some((nexus_claude::agent::StopReason::Completed, true))
    );
    session.close().await.unwrap();
}
