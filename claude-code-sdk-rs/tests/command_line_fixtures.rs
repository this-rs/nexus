//! The command line the SDK hands to the CLI, pinned as fixtures (N6).
//!
//! Two entry points build a `claude` command line today: the streaming transport
//! (`SubprocessTransport::build_command`) and the one-shot `query()` print mode.
//! N6 folds them into one builder; these fixtures are what proves the fold
//! changed nothing it was not meant to. Each fixture is the exact `argv` the
//! `fake_claude` double recorded, for a default and for a rich option set.
//!
//! Regenerate with `UPDATE_GOLDEN=1 cargo test --test command_line_fixtures`
//! and read the diff: a changed flag is a behaviour change of the CLI contract.

mod support;

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use nexus_claude::{ClaudeCodeOptions, McpServerConfig, PermissionMode, query};
use serde_json::Value;
use support::*;

const WAIT: Duration = Duration::from_secs(10);

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(format!("{name}.json"))
}

fn check(name: &str, argv: Vec<String>) {
    let actual = Value::Array(argv.into_iter().map(Value::String).collect());
    let rendered = format!("{}\n", serde_json::to_string_pretty(&actual).unwrap());
    let path = fixture_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing fixture {}; UPDATE_GOLDEN=1", path.display()));
    assert_eq!(
        expected, rendered,
        "command line `{name}` changed (UPDATE_GOLDEN=1 to re-record after review)"
    );
}

/// A deliberately wide option set: every flag family the builders handle.
fn rich_options() -> ClaudeCodeOptions {
    let mut options = ClaudeCodeOptions::builder()
        .system_prompt("be brief")
        .model("claude-sonnet-4")
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Read".into(), "Bash(git *)".into()])
        .disallowed_tools(vec!["WebFetch".into()])
        .max_turns(7)
        .max_thinking_tokens(2048)
        .resume("sess-resume")
        .build();
    options.mcp_servers.insert(
        "po".into(),
        McpServerConfig::Stdio {
            command: "po-mcp".into(),
            args: Some(vec!["--stdio".into()]),
            env: None,
        },
    );
    options
        .extra_args
        .insert("custom-flag".into(), Some("v".into()));
    options
}

/// `argv` of a streaming-transport launch.
async fn stream_argv(options: ClaudeCodeOptions) -> Vec<String> {
    let fake = Transcript::new()
        .await_stdin()
        .result_ok("ok")
        .wait_eof()
        .build();
    let mut transport = fake.transport_with(options);
    transport.connect().await.expect("connect");
    let invocation = fake.wait_for_invocation(WAIT).await;
    transport.disconnect().await.ok();
    invocation.args()
}

/// `argv` of a `query()` (print mode) launch.
async fn print_argv(options: ClaudeCodeOptions) -> Vec<String> {
    let fake = Transcript::new().result_ok("ok").build();
    let options = fake.options_with(options);
    let mut stream = Box::pin(query("the prompt", Some(options)).await.expect("query"));
    let _ = tokio::time::timeout(WAIT, async { while stream.next().await.is_some() {} }).await;
    fake.wait_for_invocation(WAIT).await.args()
}

#[tokio::test]
async fn cmdline_stream_default() {
    check(
        "cmdline_stream_default",
        stream_argv(ClaudeCodeOptions::default()).await,
    );
}

#[tokio::test]
async fn cmdline_stream_rich() {
    check("cmdline_stream_rich", stream_argv(rich_options()).await);
}

#[tokio::test]
async fn cmdline_print_default() {
    check(
        "cmdline_print_default",
        print_argv(ClaudeCodeOptions::default()).await,
    );
}

#[tokio::test]
async fn cmdline_print_rich() {
    check("cmdline_print_rich", print_argv(rich_options()).await);
}
