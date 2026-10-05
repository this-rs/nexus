//! `CodexProvider` against a REAL `codex app-server` (N10, contract §13 and the Codex
//! section).
//!
//! Every other Codex test in this crate talks to `fake_codex`, whose messages were written
//! from the documentation. This file is the one place the adapter meets the real thing:
//! the handshake (`initialize`, `initialized`), the creation of a thread, and the real
//! response shapes, parsed by our own wire types.
//!
//! Ignored by default: it needs a Codex of at least `MIN_APP_SERVER_VERSION`. Point it at
//! one with `NEXUS_REAL_CODEX=/path/to/codex` (else `codex` on the `PATH` is used) and run
//! `cargo test -p nexus-claude --features provider-codex --test codex_real -- --ignored`.
//!
//! **What it spends: nothing.** The instance has a throwaway `CODEX_HOME` with no login, so
//! no model is ever called (the real server logs a 401 on its model websocket, expected);
//! it uses none of the user's credentials. What it therefore does NOT prove is a real turn,
//! a real tool call or a real approval: those need a login in the instance's own
//! `CODEX_HOME`, which a human does (`CODEX_HOME=<dir> codex login`, decision A27).

use std::path::PathBuf;

use nexus_claude::agent::{AgentProvider, HealthStatus, ProviderError, SessionSpec};
use nexus_claude::providers::codex::{CodexConfig, CodexProvider, MIN_APP_SERVER_VERSION};

fn real_codex() -> PathBuf {
    PathBuf::from(std::env::var("NEXUS_REAL_CODEX").unwrap_or_else(|_| "codex".to_owned()))
}

fn provider(home: &std::path::Path) -> CodexProvider {
    let mut config = CodexConfig::new("codex-real");
    config.program = real_codex();
    config.codex_home = home.join("codex-home");
    CodexProvider::new(config)
}

#[tokio::test]
#[ignore = "needs a real codex >= MIN_APP_SERVER_VERSION; set NEXUS_REAL_CODEX"]
async fn health_reads_the_real_version_and_reports_the_missing_login() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let health = provider(dir.path()).health().await;
    let version = health
        .version
        .clone()
        .unwrap_or_else(|| panic!("no version read: {health:?}"));
    assert!(
        health.error != Some(ProviderError::unsupported("app_server")),
        "the codex found is older than {MIN_APP_SERVER_VERSION}: {version}. Update it, or point \
         NEXUS_REAL_CODEX at a newer one"
    );
    // A throwaway home has no login: the adapter must say so and hand the human the
    // command, rather than start a process that would fail on its first model call.
    assert_eq!(health.status, HealthStatus::Unavailable, "{health:?}");
    assert!(
        matches!(health.error, Some(ProviderError::AuthRequired { .. })),
        "{health:?}"
    );
    let hint = match health.error {
        Some(ProviderError::AuthRequired {
            login_hint: Some(hint),
        }) => hint,
        other => panic!("no login hint: {other:?}"),
    };
    assert!(hint.contains("codex login"), "{hint}");
    assert!(
        hint.contains("CODEX_HOME"),
        "the hint names the instance home: {hint}"
    );
}

#[tokio::test]
#[ignore = "needs a real codex >= MIN_APP_SERVER_VERSION; set NEXUS_REAL_CODEX"]
async fn the_real_app_server_accepts_our_handshake_and_creates_a_thread() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut spec = SessionSpec::new(&work);
    spec.model = None;

    let session = provider(dir.path())
        .open(spec)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "the real app-server refused the adapter's handshake or thread/start: {error:?}\n\
             (our wire types were written from the documentation of {MIN_APP_SERVER_VERSION})"
            )
        });

    // The thread exists: the adapter read its id out of the real response.
    let token = session
        .resume_token()
        .expect("a resume token naming the real thread");
    assert_eq!(token.kind().as_str(), "codex", "{token:?}");
    // The capabilities are frozen at opening and say what Codex declares.
    let capabilities = session.capabilities();
    assert!(capabilities.interactive_permissions, "{capabilities:?}");
    session.close().await.expect("close is clean");
    // The instance's HOME is its own, inside its CODEX_HOME (decision A33).
    let home = dir.path().join("codex-home").join("home");
    assert!(
        home.is_dir(),
        "the dedicated HOME was created at {}",
        home.display()
    );
}
