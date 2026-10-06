//! A REAL machine, a real sshd, a real `claude`: what `claude_code_remote.rs` cannot prove.
//!
//! Ignored by default. It runs `claude --version` on the machine through the exact
//! channel a session uses (pinned host key, no agent, no config file) and checks the
//! provider's health. No model is called, nothing is written on the machine.
//!
//! ```text
//! NEXUS_REAL_REMOTE_HOST=192.168.1.101 \
//! NEXUS_REAL_REMOTE_USER=me \
//! NEXUS_REAL_REMOTE_KEY="ssh-ed25519 AAAA…"   # the host key, checked by a human \
//! NEXUS_REAL_REMOTE_IDENTITY=~/.ssh/id_ed25519 \
//!   cargo test -p nexus-claude --all-features --test claude_code_remote_real -- --ignored --nocapture
//! ```
//!
//! Optional: `NEXUS_REAL_REMOTE_PORT`, `NEXUS_REAL_REMOTE_CLI` (default `claude`).

#![cfg(unix)]

use nexus_claude::agent::{AgentProvider, HealthStatus};
use nexus_claude::providers::claude_code::{ClaudeCodeConfig, ClaudeCodeProvider};
use nexus_claude::transport::RemoteHost;
use nexus_claude::transport::remote::{RemoteProbe, probe_version};

fn env(name: &str) -> Option<String> {
    std::env::var(format!("NEXUS_REAL_REMOTE_{name}"))
        .ok()
        .filter(|v| !v.is_empty())
}

fn remote() -> Option<RemoteHost> {
    let mut host = RemoteHost::new(env("HOST")?, env("KEY")?);
    host.user = env("USER");
    host.port = env("PORT").and_then(|p| p.parse().ok());
    host.identity_file = env("IDENTITY").map(|p| {
        let p = p.replacen('~', &std::env::var("HOME").unwrap_or_default(), 1);
        p.into()
    });
    if let Some(cli) = env("CLI") {
        host.cli = cli;
    }
    Some(host)
}

#[tokio::test]
#[ignore = "needs a real machine: see the module comment"]
async fn the_real_machine_answers_through_the_pinned_channel() {
    let host = remote().expect("set NEXUS_REAL_REMOTE_HOST, _KEY, and usually _USER and _IDENTITY");
    let probe = probe_version(&host).await;
    eprintln!("probe: {probe:?}");
    assert!(
        matches!(probe, RemoteProbe::Version(Some(_))),
        "the machine must answer with a version: {probe:?}"
    );
    let mut config = ClaudeCodeConfig::default();
    config.remote = Some(host);
    let health = ClaudeCodeProvider::new(config).health().await;
    eprintln!("health: {health:?}");
    assert_ne!(health.status, HealthStatus::Unavailable, "{health:?}");
}

#[tokio::test]
#[ignore = "needs a real machine: see the module comment"]
async fn a_wrong_pinned_key_is_refused_by_the_real_sshd() {
    let mut host = remote().expect("set the NEXUS_REAL_REMOTE_* variables");
    // A well-formed key that is not this machine's.
    host.host_key =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl".into();
    match probe_version(&host).await {
        RemoteProbe::Unreachable(why) => {
            assert!(why.contains("does not match the pinned key"), "{why}");
        },
        other => panic!("a wrong host key must be refused, got {other:?}"),
    }
}
