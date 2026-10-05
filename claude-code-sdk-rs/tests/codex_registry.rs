//! The `codex` kind of `ProviderRegistry` (cargo feature `provider-codex`): an
//! instance described as configuration becomes a `CodexProvider`, the security gate
//! (A32) still refuses it while shut, and the credential reference reaches the
//! process as `CODEX_API_KEY` and nowhere else.
//!
//! Like the rest of the Codex tests this runs against `fake_codex`; no real
//! `codex app-server` was involved.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use nexus_claude::agent::{
    AgentEvent, CostBasis, CredentialRef, CredentialResolver, EnvCredentialResolver, HealthStatus,
    ModelPrice, ProviderError, ProviderInstanceConfig, ProviderKind, ProviderRegistry, Secret,
    SecurityGate, SessionSpec, TurnInput,
};
use nexus_claude::providers::codex::MIN_APP_SERVER_VERSION;
use serde_json::Value;

const FAKE: &str = env!("CARGO_BIN_EXE_fake_codex");
const KEY: &str = "sk-registry-Zq81mLpWx39vNbR2";

struct Vault;

#[async_trait]
impl CredentialResolver for Vault {
    async fn resolve(
        &self,
        instance: &str,
        reference: &CredentialRef,
    ) -> Result<Option<Secret>, ProviderError> {
        assert_eq!(instance, "codex-prod");
        match reference {
            CredentialRef::Vault(_) => Ok(Some(Secret::new(KEY))),
            _ => Ok(None),
        }
    }
}

fn instance(home: &std::path::Path) -> ProviderInstanceConfig {
    ProviderInstanceConfig::new("codex-prod", ProviderKind::Codex)
        .with_command([FAKE])
        .with_default_model("gpt-fake")
        .with_cost_source(CostBasis::Priced)
        .with_price(
            "gpt-fake",
            ModelPrice {
                input_per_mtok: 1.0,
                output_per_mtok: 2.0,
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
        )
        .with_extension("codex_home", Value::String(home.display().to_string()))
}

fn plain_transcript() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/transcripts/codex")
        .join(MIN_APP_SERVER_VERSION)
        .join("sessions/plain.jsonl")
}

#[test]
fn the_gate_still_refuses_a_codex_instance_until_the_security_batch_is_active() {
    let registry = ProviderRegistry::new(Arc::new(EnvCredentialResolver));
    let home = tempfile::tempdir().unwrap();
    let error = registry
        .upsert(instance(home.path()))
        .expect_err("gate is shut");
    assert_eq!(error, ProviderError::unsupported("security_gate"));
    assert!(registry.config("codex-prod").is_none());
    registry.activate_security_gate(SecurityGate::attest("codex-test"));
    registry
        .upsert(instance(home.path()))
        .expect("the gate is open");
    let provider = registry
        .get("codex-prod")
        .expect("a constructor exists for codex");
    assert_eq!(provider.kind(), ProviderKind::Codex);
    assert_eq!(provider.id(), "codex-prod");
}

#[test]
fn a_codex_command_is_the_program_only_and_a_credential_shaped_extension_is_refused() {
    let registry = ProviderRegistry::new(Arc::new(EnvCredentialResolver));
    registry.activate_security_gate(SecurityGate::attest("codex-test"));
    let home = tempfile::tempdir().unwrap();
    registry
        .upsert(instance(home.path()).with_command([FAKE, "--extra"]))
        .expect("syntactically valid");
    let error = registry
        .get("codex-prod")
        .err()
        .expect("arguments are the adapter's");
    assert_eq!(error.kind(), "invalid_request");
    let leaky = instance(home.path()).with_extension("api_key", Value::String(KEY.to_owned()));
    assert_eq!(
        registry.upsert(leaky).unwrap_err().kind(),
        "invalid_request"
    );
    let typed = instance(home.path()).with_extension("codex_home", Value::Bool(true));
    assert_eq!(
        registry.upsert(typed).unwrap_err().kind(),
        "invalid_request"
    );
}

#[tokio::test]
async fn an_instance_built_by_the_registry_runs_a_turn_with_its_key_in_the_environment_only() {
    let registry = ProviderRegistry::new(Arc::new(Vault));
    registry.activate_security_gate(SecurityGate::attest("codex-test"));
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let record = cwd.path().join("record.jsonl");
    registry
        .upsert(
            instance(&home.path().join("codex-home"))
                .with_credential(CredentialRef::Vault("codex-key".into())),
        )
        .unwrap();
    let provider = registry.get("codex-prod").unwrap();
    let caps = registry.capabilities_for("codex-prod", None).unwrap();
    assert_eq!(caps.cost, CostBasis::Priced);
    assert!(caps.resume && caps.per_session_mcp && !caps.images);
    // The credential resolves (a vault reference), so the instance counts as logged in.
    let mut spec = SessionSpec::new(cwd.path());
    spec.env.set.insert(
        "FAKE_CODEX_TRANSCRIPT".into(),
        plain_transcript().display().to_string(),
    );
    spec.env
        .set
        .insert("FAKE_CODEX_RECORD".into(), record.display().to_string());
    spec.env
        .set
        .insert("FAKE_CODEX_CANARY".into(), KEY.to_owned());
    let session = provider.open(spec).await.expect("opens");
    let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    let mut last = None;
    while let Some(event) = stream.next().await {
        last = Some(event);
    }
    assert!(
        matches!(
            last,
            Some(AgentEvent::Done {
                is_error: false,
                ..
            })
        ),
        "{last:?}"
    );
    session.close().await.unwrap();
    let start: Value = std::fs::read_to_string(&record)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|entry| entry["kind"] == "start")
        .unwrap();
    assert_eq!(start["api_key_len"], KEY.len());
    assert_eq!(start["canary_in_argv"], false);
    assert_eq!(start["canary_in_env_of"], serde_json::json!([]));
    assert_eq!(
        start["codex_home"],
        home.path().join("codex-home").display().to_string()
    );
}

#[tokio::test]
async fn test_connection_reports_a_missing_login_as_a_value_and_registers_nothing() {
    let registry = ProviderRegistry::new(Arc::new(EnvCredentialResolver));
    registry.activate_security_gate(SecurityGate::attest("codex-test"));
    let home = tempfile::tempdir().unwrap();
    let health = registry
        .test_connection(&instance(&home.path().join("empty-home")))
        .await
        .expect("a configuration the registry accepts");
    assert_eq!(health.status, HealthStatus::Unavailable);
    assert!(
        matches!(
            health.error,
            Some(ProviderError::AuthRequired {
                login_hint: Some(_)
            })
        ),
        "{health:?}"
    );
    assert!(
        health
            .login_hint
            .is_some_and(|hint| hint.contains("codex login"))
    );
    assert!(registry.config("codex-prod").is_none());
}
