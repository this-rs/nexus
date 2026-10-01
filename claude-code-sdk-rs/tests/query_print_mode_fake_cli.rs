//! `query()` — the one-shot `--print` entry point — driven against the
//! `fake_claude` test double.
//!
//! Everything here goes through the public `query()` function, so each test
//! exercises the whole path: `QueryInput` conversion, the command `query_print_mode`
//! builds, the child process, the stdout reader and the stream handed back.
//!
//! No test may be pointed at a real `claude`: every one sets `options.cli_path`
//! (through [`FakeCli::options_with`]) to the `fake_claude` binary cargo built.

mod support;

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::Stream;
use nexus_claude::{
    ClaudeCodeOptions, McpServerConfig, Message, PermissionMode, SdkError, SystemPrompt, query,
};
use support::*;

type Item = nexus_claude::Result<Message>;

/// Drain the stream `query()` returned to its end.
///
/// The end matters as much as the items: a stream that never reports
/// end-of-stream is the bug `the_stream_ends_once_the_cli_has_exited` guards.
async fn drain(stream: impl Stream<Item = Item>) -> Vec<Item> {
    let mut stream = Box::pin(stream);
    let mut out = Vec::new();
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(item) = stream.next().await {
            out.push(item);
        }
    })
    .await;
    assert!(
        ended.is_ok(),
        "query()'s stream never reported end-of-stream (got {} items)",
        out.len()
    );
    out
}

/// `query()` plus [`drain`], for options already pointed at a fake.
async fn run(prompt: &str, options: ClaudeCodeOptions) -> Vec<Item> {
    let stream = query(prompt, Some(options))
        .await
        .expect("query() should have spawned the fake CLI");
    drain(stream).await
}

fn expect_all_ok(items: Vec<Item>) -> Vec<Message> {
    items
        .into_iter()
        .map(|item| item.expect("no error was scripted in this transcript"))
        .collect()
}

fn result_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::Result { result, .. } => Some(result.clone().unwrap_or_default()),
            _ => None,
        })
        .collect()
}

/// Run `query()` against a fake that records its command line and exits at once,
/// and hand back that recording. Nothing reaches the stream, by design.
async fn argv_for(prompt: &str, options: ClaudeCodeOptions) -> Invocation {
    let fake = Transcript::new().exit_with(0).build();
    let items = run(prompt, fake.options_with(options)).await;
    assert!(
        items.is_empty(),
        "a fake that exits 0 without printing must yield no message and no error"
    );
    fake.invocation()
}

// ---------------------------------------------------------------------------
// the stream contract
// ---------------------------------------------------------------------------

/// Regression: the stream used to hang for ever instead of ending.
///
/// The cleanup task held a `Sender` clone of the message channel while awaiting
/// `Sender::closed()`, i.e. the receiver's drop; the receiver only reports
/// end-of-stream once every sender is gone. A caller writing the `while let
/// Some(msg) = messages.next().await` loop from `query()`'s own doc example never
/// left it, and the CLI child stayed alive behind it.
#[tokio::test]
async fn the_stream_ends_once_the_cli_has_exited() {
    let fake = Transcript::new()
        .init("sess-print")
        .assistant_text("bonjour")
        .result_ok("bonjour")
        .build();

    let messages = expect_all_ok(run("salut", fake.options()).await);

    assert_eq!(messages.len(), 3, "init + assistant + result, then the end");
    assert!(matches!(
        messages[0],
        Message::System { ref subtype, .. } if subtype == "init"
    ));
    assert_eq!(assistant_texts(&messages), vec!["bonjour".to_string()]);
    assert_eq!(result_texts(&messages), vec!["bonjour".to_string()]);
}

/// Blank stdout lines are skipped rather than parsed (and rather than logged as
/// a JSON failure).
#[tokio::test]
async fn blank_stdout_lines_are_skipped() {
    let fake = Transcript::new()
        .raw("")
        .init("sess-blank")
        .raw("   ")
        .result_ok("fini")
        .raw("\t")
        .build();

    let messages = expect_all_ok(run("salut", fake.options()).await);

    assert_eq!(
        messages.len(),
        2,
        "only the two real messages: {messages:?}"
    );
    assert_eq!(result_texts(&messages), vec!["fini".to_string()]);
}

/// A line that is not JSON is dropped: it is neither a message nor a stream
/// error, so one noisy line from the CLI cannot abort a query.
#[tokio::test]
async fn non_json_output_is_dropped_without_failing_the_query() {
    let fake = Transcript::new()
        .garbage()
        .malformed_json()
        .init("sess-noise")
        .result_ok("fini")
        .build();

    let messages = expect_all_ok(run("salut", fake.options()).await);

    assert_eq!(messages.len(), 2, "the two noise lines were dropped");
    assert_eq!(result_texts(&messages), vec!["fini".to_string()]);
}

/// Valid JSON of a type the parser does not model (here a `control_request`,
/// which print mode has no way to answer) is dropped silently — `parse_message`
/// returns `Ok(None)` and the reader simply continues.
#[tokio::test]
async fn an_unmodelled_message_type_is_dropped_silently() {
    let fake = Transcript::new()
        .init("sess-unmodelled")
        .permission_request("req-1", "Bash", serde_json::json!({"command": "ls"}))
        .result_ok("fini")
        .build();

    let messages = expect_all_ok(run("salut", fake.options()).await);

    assert_eq!(messages.len(), 2, "the control_request is not a Message");
    assert_eq!(result_texts(&messages), vec!["fini".to_string()]);
}

/// JSON the parser rejects arrives as an `Err` *inside* the stream, and the
/// stream carries on: the error is per-line, not fatal.
#[tokio::test]
async fn an_unparseable_message_becomes_an_error_item_and_the_stream_continues() {
    let fake = Transcript::new()
        .init("sess-bad")
        .unparseable_message()
        .result_ok("fini")
        .build();

    let items = run("salut", fake.options()).await;

    assert_eq!(items.len(), 3, "init, the parse error, then the result");
    assert!(
        matches!(items[1], Err(SdkError::MessageParseError { .. })),
        "expected a MessageParseError as the second item, got {:?}",
        items[1]
    );
    assert!(matches!(items[2], Ok(Message::Result { .. })));
}

/// A final line the CLI never terminated with a newline is lost: `lines()` only
/// yields complete lines, and the process dies mid-write. Documented here so the
/// silence is a decision and not a surprise.
#[tokio::test]
async fn a_truncated_final_line_never_reaches_the_stream() {
    let fake = Transcript::new()
        .init("sess-cut")
        .partial(r#"{"type":"result","subtype":"success""#)
        .exit_with(0)
        .build();

    let items = run("salut", fake.options()).await;

    assert_eq!(items.len(), 1, "only the complete init line: {items:?}");
    assert!(matches!(items[0], Ok(Message::System { .. })));
}

/// A CLI that exits non-zero ends the stream with `ProcessExited`, carrying the
/// code, after whatever it managed to print.
#[tokio::test]
async fn a_nonzero_exit_is_reported_as_process_exited() {
    let fake = Transcript::new()
        .init("sess-doomed")
        .stderr("fake_claude: scripted failure")
        .exit_with(7)
        .build();

    let items = run("salut", fake.options()).await;

    assert_eq!(items.len(), 2, "the init message, then the exit error");
    assert!(matches!(items[0], Ok(Message::System { .. })));
    match &items[1] {
        Err(SdkError::ProcessExited { code }) => assert_eq!(*code, Some(7)),
        other => panic!("expected ProcessExited, got {other:?}"),
    }
}

/// `fake_claude`'s own diagnostic exit codes travel the same way, which is what
/// makes a mis-scripted transcript a test failure instead of a hang.
#[tokio::test]
async fn a_transcript_the_fake_cannot_read_surfaces_its_diagnostic_code() {
    let fake = Transcript::new().build();
    let mut options = fake.options();
    options.env.insert(
        "FAKE_CLAUDE_TRANSCRIPT".to_string(),
        fake.dir()
            .join("no-such-transcript.jsonl")
            .display()
            .to_string(),
    );

    let items = run("salut", options).await;

    match items.last() {
        Some(Err(SdkError::ProcessExited { code })) => {
            assert_eq!(*code, Some(exit_code::NO_TRANSCRIPT));
        },
        other => panic!("expected the fake's NO_TRANSCRIPT exit, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// the command line
// ---------------------------------------------------------------------------

/// Print mode's fixed shape: stream-json, verbose, an always-present
/// `--system-prompt`, and the prompt last — behind `--` so it can never be read
/// as a flag. It also never passes `--input-format`, unlike the interactive
/// transport: print mode writes nothing to stdin.
#[tokio::test]
async fn the_prompt_is_passed_last_behind_a_double_dash() {
    let inv = argv_for("Quelle heure est-il ?", ClaudeCodeOptions::default()).await;
    let args = inv.args();

    assert_eq!(
        inv.flag_value("--output-format").as_deref(),
        Some("stream-json")
    );
    assert!(inv.has_flag("--verbose"));
    assert!(inv.has_flag("--print"));
    assert!(
        !inv.has_flag("--input-format"),
        "print mode never writes to stdin, so it must not announce an input format: {args:?}"
    );
    assert_eq!(
        &args[args.len() - 3..],
        ["--print", "--", "Quelle heure est-il ?"],
        "the prompt must come last, after the -- terminator: {args:?}"
    );
}

/// A prompt that looks exactly like a CLI flag stays a prompt. This is the whole
/// point of the `--` terminator, and the only test that proves it.
#[tokio::test]
async fn a_prompt_shaped_like_a_flag_is_still_a_prompt() {
    let inv = argv_for(
        "--dangerously-skip-permissions",
        ClaudeCodeOptions::default(),
    )
    .await;
    let args = inv.args();

    let terminator = args
        .iter()
        .position(|a| a == "--")
        .expect("the -- terminator must be present");
    assert_eq!(args[terminator + 1], "--dangerously-skip-permissions");
    assert_eq!(
        terminator + 2,
        args.len(),
        "nothing may follow the prompt: {args:?}"
    );
}

/// With no system prompt at all, print mode still passes an empty one — matching
/// the Python SDK, as the comment in `query_print_mode` claims.
#[tokio::test]
async fn an_absent_system_prompt_is_passed_as_an_empty_one() {
    let inv = argv_for("salut", ClaudeCodeOptions::default()).await;

    assert_eq!(inv.flag_value("--system-prompt").as_deref(), Some(""));
    assert!(!inv.has_flag("--append-system-prompt"));
}

/// `system_prompt_v2` takes precedence: the deprecated pair is not consulted at
/// all when it is set, not even `append_system_prompt`.
#[tokio::test]
async fn system_prompt_v2_shuts_out_the_deprecated_pair() {
    let options = ClaudeCodeOptions::builder()
        .system_prompt("vieux prompt")
        .append_system_prompt("vieil ajout")
        .build();
    let options = ClaudeCodeOptions {
        system_prompt_v2: Some(SystemPrompt::String("nouveau prompt".into())),
        ..options
    };

    let inv = argv_for("salut", options).await;

    assert_eq!(
        inv.flag_value("--system-prompt").as_deref(),
        Some("nouveau prompt")
    );
    assert!(
        !inv.has_flag("--append-system-prompt"),
        "the deprecated append must be ignored once v2 is set: {:?}",
        inv.args()
    );
}

/// A preset system prompt contributes only its `append`: no preset selector flag
/// reaches the CLI, and no `--system-prompt` either — so a preset with nothing to
/// append leaves the command without any system prompt flag at all.
#[tokio::test]
async fn a_preset_system_prompt_only_contributes_its_append() {
    let with_append = ClaudeCodeOptions {
        system_prompt_v2: Some(SystemPrompt::Preset {
            preset_type: "preset".into(),
            preset: "claude_code".into(),
            append: Some("et sois bref".into()),
        }),
        ..ClaudeCodeOptions::default()
    };
    let inv = argv_for("salut", with_append).await;
    assert_eq!(
        inv.flag_value("--append-system-prompt").as_deref(),
        Some("et sois bref")
    );
    assert!(!inv.has_flag("--system-prompt"));
    assert!(!inv.has_flag("--preset"));

    let bare = ClaudeCodeOptions {
        system_prompt_v2: Some(SystemPrompt::Preset {
            preset_type: "preset".into(),
            preset: "claude_code".into(),
            append: None,
        }),
        ..ClaudeCodeOptions::default()
    };
    let inv = argv_for("salut", bare).await;
    assert!(
        !inv.has_flag("--system-prompt") && !inv.has_flag("--append-system-prompt"),
        "a preset with no append must add nothing: {:?}",
        inv.args()
    );
}

/// The deprecated pair still works on its own, both halves of it.
#[tokio::test]
async fn the_deprecated_system_prompt_pair_is_still_honoured() {
    let options = ClaudeCodeOptions::builder()
        .system_prompt("tu es concis")
        .append_system_prompt("en français")
        .build();

    let inv = argv_for("salut", options).await;

    assert_eq!(
        inv.flag_value("--system-prompt").as_deref(),
        Some("tu es concis")
    );
    assert_eq!(
        inv.flag_value("--append-system-prompt").as_deref(),
        Some("en français")
    );
}

/// Tool lists become one comma-joined flag each, and an empty list adds nothing
/// (an empty `--allowedTools` would read as "allow nothing", a different thing).
#[tokio::test]
async fn tool_lists_are_comma_joined_and_omitted_when_empty() {
    let options = ClaudeCodeOptions::builder()
        .allowed_tools(vec!["Bash".into(), "Read(src/**)".into()])
        .disallowed_tools(vec!["WebFetch".into()])
        .build();
    let inv = argv_for("salut", options).await;
    assert_eq!(
        inv.flag_value("--allowedTools").as_deref(),
        Some("Bash,Read(src/**)")
    );
    assert_eq!(
        inv.flag_value("--disallowedTools").as_deref(),
        Some("WebFetch")
    );

    let inv = argv_for("salut", ClaudeCodeOptions::default()).await;
    assert!(!inv.has_flag("--allowedTools"));
    assert!(!inv.has_flag("--disallowedTools"));
}

/// `max_turns` and `max_thinking_tokens` are both opt-in; a zero thinking budget
/// means "unset" and must not be passed as `0`.
#[tokio::test]
async fn turn_and_thinking_budgets_are_only_passed_when_asked_for() {
    let inv = argv_for("salut", ClaudeCodeOptions::default()).await;
    assert!(!inv.has_flag("--max-turns"));
    assert!(
        !inv.has_flag("--max-thinking-tokens"),
        "the default budget is 0, which means unset: {:?}",
        inv.args()
    );

    let options = ClaudeCodeOptions::builder()
        .max_turns(3)
        .max_thinking_tokens(2048)
        .build();
    let inv = argv_for("salut", options).await;
    assert_eq!(inv.flag_value("--max-turns").as_deref(), Some("3"));
    assert_eq!(
        inv.flag_value("--max-thinking-tokens").as_deref(),
        Some("2048")
    );
}

/// The model and the permission-prompt tool are passed through verbatim.
#[tokio::test]
async fn the_model_and_permission_prompt_tool_are_passed_through() {
    let options = ClaudeCodeOptions::builder()
        .model("claude-sonnet-4-5")
        .permission_prompt_tool_name("mcp__approve__ask")
        .build();

    let inv = argv_for("salut", options).await;

    assert_eq!(
        inv.flag_value("--model").as_deref(),
        Some("claude-sonnet-4-5")
    );
    assert_eq!(
        inv.flag_value("--permission-prompt-tool").as_deref(),
        Some("mcp__approve__ask")
    );
}

/// Every `PermissionMode` has exactly one CLI spelling, and one is always sent —
/// print mode has no "leave it to the CLI default" state.
#[tokio::test]
async fn every_permission_mode_maps_to_its_cli_spelling() {
    let cases = [
        (PermissionMode::Default, "default"),
        (PermissionMode::AcceptEdits, "acceptEdits"),
        (PermissionMode::Plan, "plan"),
        (PermissionMode::BypassPermissions, "bypassPermissions"),
    ];

    for (mode, spelling) in cases {
        let options = ClaudeCodeOptions::builder().permission_mode(mode).build();
        let inv = argv_for("salut", options).await;
        assert_eq!(
            inv.flag_value("--permission-mode").as_deref(),
            Some(spelling),
            "wrong spelling for {mode:?}"
        );
    }
}

/// `--continue` is a bare flag; `--resume` carries the session id. Neither
/// appears unless asked for.
#[tokio::test]
async fn continue_and_resume_reach_the_cli() {
    let inv = argv_for("salut", ClaudeCodeOptions::default()).await;
    assert!(!inv.has_flag("--continue"));
    assert!(!inv.has_flag("--resume"));

    let options = ClaudeCodeOptions::builder()
        .continue_conversation(true)
        .resume("sess-abc")
        .build();
    let inv = argv_for("salut", options).await;
    assert!(inv.has_flag("--continue"));
    assert_eq!(inv.flag_value("--resume").as_deref(), Some("sess-abc"));
}

/// MCP servers are collapsed into a single `--mcp-config` JSON document under a
/// `mcpServers` key — one argument, however many servers.
#[tokio::test]
async fn mcp_servers_become_one_json_config_argument() {
    let mut servers = HashMap::new();
    servers.insert(
        "orchestrator".to_string(),
        McpServerConfig::Stdio {
            command: "node".into(),
            args: Some(vec!["server.js".into()]),
            env: None,
        },
    );
    let options = ClaudeCodeOptions::builder().mcp_servers(servers).build();

    let inv = argv_for("salut", options).await;

    let config = inv
        .flag_value("--mcp-config")
        .expect("--mcp-config must be present");
    let parsed: serde_json::Value =
        serde_json::from_str(&config).expect("--mcp-config must be valid JSON");
    assert_eq!(
        parsed["mcpServers"]["orchestrator"]["command"],
        serde_json::json!("node")
    );

    let inv = argv_for("salut", ClaudeCodeOptions::default()).await;
    assert!(
        !inv.has_flag("--mcp-config"),
        "no servers configured means no --mcp-config at all"
    );
}

/// `extra_args` keys get `--` prepended only when they do not already start with
/// a dash, and a `None` value means a bare flag.
#[tokio::test]
async fn extra_args_are_dashed_only_when_they_need_it() {
    let mut extra = HashMap::new();
    extra.insert("custom-flag".to_string(), Some("valeur".to_string()));
    extra.insert("--already-dashed".to_string(), None);
    extra.insert("-s".to_string(), Some("court".to_string()));
    let options = ClaudeCodeOptions {
        extra_args: extra,
        ..ClaudeCodeOptions::default()
    };

    let inv = argv_for("salut", options).await;
    let args = inv.args();

    assert_eq!(inv.flag_value("--custom-flag").as_deref(), Some("valeur"));
    assert!(
        inv.has_flag("--already-dashed"),
        "an already-dashed key must not be dashed twice: {args:?}"
    );
    assert!(
        !args.iter().any(|a| a == "----already-dashed"),
        "double prefixing: {args:?}"
    );
    assert_eq!(
        inv.flag_value("-s").as_deref(),
        Some("court"),
        "a single-dash short flag must be left alone: {args:?}"
    );
}

// ---------------------------------------------------------------------------
// process environment and working directory
// ---------------------------------------------------------------------------

/// `options.env` used to be dropped by print mode while every other entry point
/// honoured it. It now reaches the child — which is also what lets this whole
/// file script the fake.
#[tokio::test]
async fn options_env_reaches_the_child() {
    let options = ClaudeCodeOptions::builder()
        .env("QUERY_PRINT_MODE_MARKER", "present")
        .env("FAKE_CLAUDE_ARGS_ENV_ALLOW", "QUERY_PRINT_MODE_MARKER")
        .build();

    let inv = argv_for("salut", options).await;

    assert_eq!(
        inv.env("QUERY_PRINT_MODE_MARKER").as_deref(),
        Some("present")
    );
}

/// `options.env` is applied after the `max_output_tokens` handling, so a caller
/// who sets the variable by hand wins over the capped option. Pinned because the
/// ordering is the only thing that decides it.
#[tokio::test]
async fn options_env_overrides_the_capped_max_output_tokens() {
    let options = ClaudeCodeOptions::builder()
        .max_output_tokens(99_999)
        .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "4096")
        .build();

    let inv = argv_for("salut", options).await;

    assert_eq!(
        inv.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS").as_deref(),
        Some("4096")
    );
}

/// `max_output_tokens` is clamped into `1..=32000`, both ends.
#[tokio::test]
async fn max_output_tokens_is_clamped_at_both_ends() {
    let options = ClaudeCodeOptions::builder()
        .max_output_tokens(99_999)
        .build();
    let inv = argv_for("salut", options).await;
    assert_eq!(
        inv.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS").as_deref(),
        Some("32000")
    );

    let options = ClaudeCodeOptions::builder().max_output_tokens(0).build();
    let inv = argv_for("salut", options).await;
    assert_eq!(
        inv.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS").as_deref(),
        Some("1"),
        "0 is clamped up to 1, not passed through as 0"
    );
}

/// `options.cwd` is where the CLI runs. Dropped by print mode until now, like
/// `options.env`.
#[tokio::test]
async fn options_cwd_is_where_the_cli_runs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let expected = dir.path().canonicalize().expect("canonicalize temp dir");
    let options = ClaudeCodeOptions::builder().cwd(dir.path()).build();

    let inv = argv_for("salut", options).await;

    let actual = std::path::PathBuf::from(inv.cwd())
        .canonicalize()
        .expect("canonicalize the recorded cwd");
    assert_eq!(actual, expected);
}

/// `query()` marks the child as coming from the Rust SDK.
///
/// Note how it does it: `query()` calls `std::env::set_var` on the *calling*
/// process and lets the child inherit, instead of setting the variable on the
/// `Command` the way `SubprocessTransport::build_command` does. The child sees the
/// right thing either way, which is what this asserts; the caller's own
/// environment being modified as a side effect is reported, not pinned.
#[tokio::test]
async fn the_child_is_marked_as_the_rust_sdk_entrypoint() {
    let inv = argv_for("salut", ClaudeCodeOptions::default()).await;

    assert_eq!(
        inv.env("CLAUDE_CODE_ENTRYPOINT").as_deref(),
        Some("sdk-rust")
    );
}

// ---------------------------------------------------------------------------
// refusals before anything is spawned
// ---------------------------------------------------------------------------

/// A blank `options.user` is refused, and refused *before* the CLI is spawned —
/// a query that cannot honour its privilege drop must not run at all.
#[tokio::test]
async fn a_blank_process_user_is_refused_before_the_cli_is_spawned() {
    let fake = Transcript::new().exit_with(0).build();
    let options = fake.options_with(ClaudeCodeOptions::builder().user("   ").build());

    let error = query("salut", Some(options))
        .await
        .err()
        .expect("a blank options.user must be refused");

    match error {
        SdkError::ConfigError(message) => assert!(
            message.contains("non-empty"),
            "unhelpful message: {message}"
        ),
        other => panic!("expected ConfigError, got {other:?}"),
    }
    assert!(
        !fake.dir().join("invocation.json").exists(),
        "the fake CLI must never have been spawned"
    );
}

/// An `options.cli_path` that does not exist fails as a process error, with the
/// OS message attached — not as `CliNotFound`, which would wrongly suggest the
/// usual locations were searched.
#[tokio::test]
async fn a_cli_path_that_does_not_exist_fails_as_a_process_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = dir
        .path()
        .join(format!("not-claude{}", std::env::consts::EXE_SUFFIX));
    let options = ClaudeCodeOptions::builder().cli_path(&missing).build();

    let error = query("salut", Some(options))
        .await
        .err()
        .expect("spawning a missing binary must fail");

    assert!(
        matches!(error, SdkError::ProcessError(_)),
        "expected ProcessError, got {error:?}"
    );
}
