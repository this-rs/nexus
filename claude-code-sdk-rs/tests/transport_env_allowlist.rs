//! Process isolation of the CLI child (decision A33).
//!
//! `fake_claude` records the names of every variable it was started with and its
//! whole command line, so these tests observe what a real child would see:
//! nothing in-process can tell whether `env_clear` actually happened.
//!
//! The canary is a variable cargo itself puts in every test process
//! (`CARGO_MANIFEST_DIR`): no `set_var`, hence no race with the other tests of
//! this binary.

mod support;

use std::collections::HashMap;
use std::time::Duration;

use nexus_claude::transport::Transport;
use nexus_claude::transport::spawn::EnvPolicy;
use nexus_claude::{ClaudeCodeOptions, McpServerConfig};
use support::*;

const WAIT: Duration = Duration::from_secs(5);
const CANARY: &str = "CARGO_MANIFEST_DIR";
const MCP_SECRET: &str = "s3cr3t-db-password-0123456789";

/// Names the operating system, the loader or the coverage runtime inject into a child no
/// matter what the parent passes. `__LLVM_PROFILE_RT_INIT_ONCE` is set by the LLVM profiling
/// runtime of every instrumented process (the fake CLI under `cargo llvm-cov`) in its own
/// environment: it is not a variable of the host that leaked.
const OS_INJECTED: &[&str] = &[
    "__CF_USER_TEXT_ENCODING",
    "__CFBundleIdentifier",
    "__LLVM_PROFILE_RT_INIT_ONCE",
];

fn idle_session() -> FakeCli {
    Transcript::new().wait_eof().build()
}

fn mcp_options() -> ClaudeCodeOptions {
    let mut options = ClaudeCodeOptions::default();
    options.mcp_servers.insert(
        "project-orchestrator".into(),
        McpServerConfig::Stdio {
            command: "/opt/po/mcp_server".into(),
            args: None,
            env: Some(HashMap::from([(
                "NEO4J_PASSWORD".to_string(),
                MCP_SECRET.to_string(),
            )])),
        },
    );
    options
}

#[tokio::test]
async fn default_policy_still_inherits_the_host_environment() {
    assert!(
        std::env::var_os(CANARY).is_some(),
        "the canary must exist in the test process for this file to prove anything"
    );
    let fake = idle_session();
    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let invocation = fake.wait_for_invocation(WAIT).await;
    transport.disconnect().await.unwrap();

    assert!(
        invocation.has_env(CANARY),
        "ClaudeCodeOptions::default() keeps the historical full inheritance"
    );
}

#[tokio::test]
async fn allowlist_policy_hides_everything_that_is_not_listed() {
    assert!(std::env::var_os(CANARY).is_some());
    let fake = idle_session();
    let policy = EnvPolicy::claude_code();
    let mut options = ClaudeCodeOptions {
        env_policy: policy.clone(),
        ..Default::default()
    };
    options.env.insert("NEXUS_EXPLICIT".into(), "1".into());
    let options = fake.options_with(options);
    let explicit: Vec<String> = options.env.keys().cloned().collect();

    let mut transport = nexus_claude::SubprocessTransport::new(options).unwrap();
    transport.connect().await.unwrap();
    let invocation = fake.wait_for_invocation(WAIT).await;
    transport.disconnect().await.unwrap();

    assert!(
        !invocation.has_env(CANARY),
        "{CANARY} leaked into the child"
    );
    assert!(
        invocation.has_env("PATH"),
        "the base allowlist still applies"
    );
    assert!(
        invocation.has_env("NEXUS_EXPLICIT"),
        "explicit variables are added on top"
    );
    assert_eq!(
        invocation.env("CLAUDE_CODE_ENTRYPOINT").as_deref(),
        Some("sdk-rust"),
        "variables the transport sets itself survive the clean environment"
    );

    // The strong form: no variable of this process reaches the child unless the
    // policy lists it or the options set it.
    let leaked: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|name| !policy.allows(name))
        .filter(|name| !explicit.contains(name))
        .filter(|name| !OS_INJECTED.contains(&name.as_str()))
        .filter(|name| invocation.has_env(name))
        .collect();
    assert!(
        leaked.is_empty(),
        "host variables leaked into the child: {leaked:?}"
    );
}

#[tokio::test]
async fn mcp_config_is_inline_by_default() {
    // Control for the next test: the secret IS on the command line unless asked otherwise.
    let fake = idle_session();
    let mut transport = fake.transport_with(mcp_options());
    transport.connect().await.unwrap();
    let invocation = fake.wait_for_invocation(WAIT).await;
    transport.disconnect().await.unwrap();

    assert!(invocation.args().join(" ").contains(MCP_SECRET));
}

#[tokio::test]
async fn mcp_config_via_file_keeps_secrets_off_the_command_line() {
    let fake = idle_session();
    let mut options = mcp_options();
    options.mcp_config_via_file = true;
    let mut transport = fake.transport_with(options);
    transport.connect().await.unwrap();
    let invocation = fake.wait_for_invocation(WAIT).await;

    assert!(
        !invocation.args().join(" ").contains(MCP_SECRET),
        "the MCP secret is on the command line"
    );
    let path = std::path::PathBuf::from(
        invocation
            .flag_value("--mcp-config")
            .expect("--mcp-config is still passed"),
    );
    let content = std::fs::read_to_string(&path).expect("--mcp-config points at a readable file");
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(
        parsed["mcpServers"]["project-orchestrator"]["env"]["NEO4J_PASSWORD"],
        MCP_SECRET
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the MCP config file must be owner-only");
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    transport.disconnect().await.unwrap();
    assert!(
        !path.exists(),
        "the MCP config file must be deleted on disconnect"
    );
}

#[tokio::test]
async fn mcp_config_file_is_deleted_when_the_transport_is_dropped() {
    let fake = idle_session();
    let mut options = mcp_options();
    options.mcp_config_via_file = true;
    let mut transport = fake.transport_with(options);
    transport.connect().await.unwrap();
    let invocation = fake.wait_for_invocation(WAIT).await;
    let path = std::path::PathBuf::from(invocation.flag_value("--mcp-config").unwrap());
    assert!(path.exists());

    drop(transport);
    assert!(
        poll_until(WAIT, || !path.exists()).await,
        "dropping the transport without disconnect() left the secret file behind"
    );
}
