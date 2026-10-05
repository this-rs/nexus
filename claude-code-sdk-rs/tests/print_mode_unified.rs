//! `query()` (print mode) shares the streaming transport's command builder (N6).
//!
//! Until then the one-shot path had its own copy of the flag logic, and every
//! option added to the streaming builder after the fork silently did not apply
//! to it: `--settings`, `--add-dir`, `--fork-session`, the native permission
//! mode of N5b, and — worst — `mcp_config_via_file`, so a caller who asked for
//! MCP secrets to stay off the command line still had them on `argv` in print
//! mode.

mod support;

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use nexus_claude::{ClaudeCodeOptions, McpServerConfig, query};
use support::*;

const WAIT: Duration = Duration::from_secs(10);
const MCP_SECRET: &str = "s3cr3t-print-mode-7c1d";

async fn print_invocation(options: ClaudeCodeOptions) -> Invocation {
    let fake = Transcript::new().result_ok("ok").build();
    let options = fake.options_with(options);
    let mut stream = Box::pin(query("the prompt", Some(options)).await.expect("query"));
    let _ = tokio::time::timeout(WAIT, async { while stream.next().await.is_some() {} }).await;
    fake.wait_for_invocation(WAIT).await
}

fn with_mcp(mut options: ClaudeCodeOptions) -> ClaudeCodeOptions {
    options.mcp_servers.insert(
        "po".into(),
        McpServerConfig::Stdio {
            command: "po-mcp".into(),
            args: None,
            env: Some(HashMap::from([(
                "DB_PASSWORD".to_string(),
                MCP_SECRET.to_string(),
            )])),
        },
    );
    options
}

#[tokio::test]
async fn print_mode_honours_mcp_config_via_file() {
    let mut options = with_mcp(ClaudeCodeOptions::default());
    options.mcp_config_via_file = true;
    let invocation = print_invocation(options).await;

    assert!(
        !invocation.args().join(" ").contains(MCP_SECRET),
        "the MCP secret is on argv in print mode: {:?}",
        invocation.args()
    );
    assert!(invocation.flag_value("--mcp-config").is_some());
}

#[tokio::test]
async fn print_mode_keeps_inline_config_when_not_asked() {
    let invocation = print_invocation(with_mcp(ClaudeCodeOptions::default())).await;
    assert!(
        invocation.args().join(" ").contains(MCP_SECRET),
        "control: inline stays the default"
    );
}

#[tokio::test]
async fn print_mode_honours_options_it_used_to_drop() {
    let options = ClaudeCodeOptions::builder()
        .settings(r#"{"model":"x"}"#)
        .add_dir("/work/extra")
        .fork_session(true)
        .include_partial_messages(true)
        .permission_mode_native("dontAsk")
        .build();
    let invocation = print_invocation(options).await;
    let argv = invocation.args();
    for flag in [
        "--settings",
        "--add-dir",
        "--fork-session",
        "--include-partial-messages",
    ] {
        assert!(argv.iter().any(|a| a == flag), "{flag} missing: {argv:?}");
    }
    assert_eq!(
        invocation.flag_value("--permission-mode").as_deref(),
        Some("dontAsk"),
        "the native permission mode must replace the enum in print mode too"
    );
}

#[tokio::test]
async fn print_mode_line_keeps_its_shape() {
    let invocation = print_invocation(ClaudeCodeOptions::default()).await;
    let argv = invocation.args();
    assert!(
        !argv.iter().any(|a| a == "--input-format"),
        "print mode has no stdin protocol: {argv:?}"
    );
    assert!(
        !argv.iter().any(|a| a == "--setting-sources"),
        "an empty --setting-sources would switch every user/project setting off: {argv:?}"
    );
    assert_eq!(
        &argv[argv.len() - 3..],
        ["--print", "--", "the prompt"],
        "the prompt closes the line"
    );
}
