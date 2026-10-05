//! Executable documentation for the `fake_claude` harness.
//!
//! Every test here drives a **real** `SubprocessTransport` against the
//! `fake_claude` binary: real spawn, real pipes, real line framing, real
//! shutdown — no network, no `claude` install, no shell script (so it also runs
//! on windows-latest). Copy the shape of whichever test is closest to what you
//! need; `tests/support/mod.rs` documents the directive vocabulary.
//!
//! The one ordering rule: `receive_messages()` subscribes to a tokio broadcast,
//! which never replays. Subscribe *before* the CLI prints anything — in practice
//! start the transcript with `await_stdin()` and use [`support::start_turn`].

mod support;

use std::time::Duration;

use nexus_claude::transport::subprocess::{SemVer, get_cli_version};
use nexus_claude::{
    ClaudeCodeOptions, ContentBlock, ControlRequest, ControlResponse, Message, PermissionMode,
    SdkError,
};
use serde_json::json;
use support::*;

/// Generous enough for a process spawn on a loaded CI runner, short enough that a
/// genuinely stuck test fails instead of eating the job's time budget.
const WAIT: Duration = Duration::from_secs(5);

// ===========================================================================
// 1. Version detection — `get_cli_version` / `SemVer::parse` / `check_cli_version`
// ===========================================================================

/// The fake must look recent enough that `check_cli_version` stays quiet,
/// otherwise every harness test would emit a misleading "below the recommended
/// version" warning. This fails loudly the day `MIN_CLI_VERSION` is bumped.
#[tokio::test]
async fn fake_cli_reports_a_version_the_sdk_accepts() {
    let version = get_cli_version(&fake_cli_path())
        .await
        .expect("fake_claude --version must parse");

    let min = SemVer::parse(nexus_claude::cli_download::MIN_CLI_VERSION)
        .expect("MIN_CLI_VERSION is semver");

    assert!(
        version >= min,
        "fake_claude reports {version} but the SDK floor is {min}: bump DEFAULT_VERSION in \
         src/bin/fake_claude.rs"
    );
}

/// An out-of-date CLI is advisory only: `check_cli_version` warns and `connect`
/// still succeeds. This pins that contract — if it ever becomes a hard error,
/// this test is where you will find out.
#[tokio::test]
async fn an_outdated_cli_version_is_a_warning_not_a_connect_failure() {
    let fake = Transcript::new()
        .await_stdin()
        .assistant_text("still works")
        .result_ok("still works")
        .build_with_version("2.0.1 (Claude Code)");

    let reported = get_cli_version(fake.cli_path())
        .await
        .expect("sidecar version parses");
    assert_eq!(reported, SemVer::new(2, 0, 1));
    assert!(reported < SemVer::parse(nexus_claude::cli_download::MIN_CLI_VERSION).unwrap());

    let mut transport = fake.transport();
    transport.connect().await.expect("connect despite old CLI");
    let stream = start_turn(&mut transport, "hello").await;
    let messages = collect_until_result(stream, WAIT).await;
    assert_eq!(assistant_texts(&messages), vec!["still works".to_string()]);
    transport.disconnect().await.unwrap();
}

/// `claude --version` output that is not semver at all must come back as `None`
/// rather than as `0.0.0` (which would look like a catastrophically old CLI).
#[tokio::test]
async fn unparseable_version_output_yields_none() {
    let fake = Transcript::new()
        .exit_with(0)
        .build_with_version("nightly-build");
    assert!(get_cli_version(fake.cli_path()).await.is_none());

    let mut transport = fake.transport();
    transport
        .connect()
        .await
        .expect("connect with unknown version");
    transport.disconnect().await.unwrap();
}

// ===========================================================================
// 2. The happy path
// ===========================================================================

/// The one-line smoke test: no transcript at all, the fake replays its built-in
/// minimal session.
#[tokio::test]
async fn default_session_smoke_test() {
    let fake = default_session();
    let mut transport = fake.transport();
    transport.connect().await.unwrap();

    let stream = start_turn(&mut transport, "ping").await;
    let messages = collect_until_result(stream, WAIT).await;

    assert!(
        matches!(
            messages.last(),
            Some(Message::Result {
                is_error: false,
                ..
            })
        ),
        "the built-in session must end on a successful result, got {messages:?}"
    );
    assert_eq!(
        assistant_texts(&messages),
        vec!["Hello from fake_claude.".to_string()]
    );
    transport.disconnect().await.unwrap();
}

/// connect -> subscribe -> send -> receive the scripted stream -> end_input ->
/// disconnect, i.e. the whole lifecycle in one test.
#[tokio::test]
async fn full_lifecycle_connect_send_receive_end_input_disconnect() {
    let fake = Transcript::new()
        .comment("wait for the SDK's prompt so nothing is printed before we subscribe")
        .await_stdin()
        .init("sess-lifecycle")
        .assistant_text("bonjour")
        .result_ok("bonjour")
        .blank()
        .comment("stay alive so disconnect() has a live child to shut down")
        .wait_eof()
        .build();

    let mut transport = fake.transport();
    assert!(!transport.is_connected());
    assert!(
        transport.subscribe_messages().is_none(),
        "no broadcast exists before connect"
    );

    transport.connect().await.unwrap();
    assert!(transport.is_connected());
    assert!(transport.subscribe_messages().is_some());
    let pid = transport.child_pid().expect("a spawned child has a pid");

    let stream = start_turn(&mut transport, "salut").await;
    let messages = collect_until_result(stream, WAIT).await;

    assert_eq!(
        messages.len(),
        3,
        "init + assistant + result, got {messages:?}"
    );
    match &messages[0] {
        Message::System { subtype, data } => {
            assert_eq!(subtype, "init");
            assert_eq!(data["session_id"], "sess-lifecycle");
        },
        other => panic!("expected the init system message first, got {other:?}"),
    }
    assert_eq!(assistant_texts(&messages), vec!["bonjour".to_string()]);

    // The prompt really travelled down the child's stdin, in the wire shape
    // `InputMessage::user` produces.
    let sent = fake.wait_for_stdin_lines(1, WAIT).await;
    assert_eq!(sent.len(), 1, "exactly one line was written to the CLI");
    let sent: serde_json::Value = serde_json::from_str(&sent[0]).unwrap();
    assert_eq!(sent["type"], "user");
    assert_eq!(sent["message"]["content"], "salut");
    assert_eq!(sent["session_id"], "fake-session");

    transport.end_input().await.unwrap();
    transport.disconnect().await.unwrap();
    assert!(!transport.is_connected());
    assert!(
        transport.child_pid().is_none(),
        "disconnect must drop the child handle"
    );
    assert_no_process(pid);
}

/// `tool_use` on the way out, `tool_result` on the way back: both content-block
/// shapes the SDK's parser has to handle.
#[tokio::test]
async fn tool_use_and_tool_result_blocks_are_parsed() {
    let fake = Transcript::new()
        .await_stdin()
        .tool_use("toolu_1", "Bash", json!({"command": "echo hi"}))
        .tool_result("toolu_1", "hi\n", false)
        .result_ok("done")
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let stream = start_turn(&mut transport, "run it").await;
    let messages = collect_until_result(stream, WAIT).await;
    transport.disconnect().await.unwrap();

    assert_eq!(messages.len(), 3, "got {messages:?}");
    match &messages[0] {
        Message::Assistant { message, .. } => match &message.content[0] {
            ContentBlock::ToolUse(tool) => {
                assert_eq!(tool.id, "toolu_1");
                assert_eq!(tool.name, "Bash");
                assert_eq!(tool.input["command"], "echo hi");
            },
            other => panic!("expected a tool_use block, got {other:?}"),
        },
        other => panic!("expected an assistant message, got {other:?}"),
    }
    match &messages[1] {
        Message::User {
            message,
            parent_tool_use_id,
        } => {
            assert_eq!(parent_tool_use_id.as_deref(), Some("toolu_1"));
            let blocks = message
                .content_blocks
                .as_ref()
                .expect("a tool_result user message carries content blocks");
            assert!(
                matches!(&blocks[0], ContentBlock::ToolResult(r) if r.tool_use_id == "toolu_1")
            );
        },
        other => panic!("expected a user message carrying the tool result, got {other:?}"),
    }
    assert!(
        messages[1].is_sidechain(),
        "a tool result is a sidechain message"
    );
}

/// `--include-partial-messages` plus a `stream_event` line: the token-streaming
/// path.
#[tokio::test]
async fn stream_events_are_parsed_when_partial_messages_are_enabled() {
    let fake = Transcript::new()
        .await_stdin()
        .text_delta("bon")
        .text_delta("jour")
        .result_ok("bonjour")
        .build();

    let options = ClaudeCodeOptions::builder()
        .include_partial_messages(true)
        .build();
    let mut transport = fake.transport_with(options);
    transport.connect().await.unwrap();
    let stream = start_turn(&mut transport, "stream please").await;
    let messages = collect_until_result(stream, WAIT).await;
    transport.disconnect().await.unwrap();

    let deltas = messages
        .iter()
        .filter(|m| matches!(m, Message::StreamEvent { .. }))
        .count();
    assert_eq!(
        deltas, 2,
        "both stream_event lines became StreamEvent messages"
    );
    assert!(
        fake.invocation().has_flag("--include-partial-messages"),
        "the option must reach the CLI as a flag"
    );
}

/// A `thinking` block needs `thinking`; its `signature` may be absent (an
/// Anthropic-compatible endpoint does not always sign its reasoning) and is then
/// empty. This test used to pin the opposite — the signature-less message was
/// dropped whole — which was the defect.
#[tokio::test]
async fn thinking_blocks_need_a_signature() {
    let fake = Transcript::new()
        .await_stdin()
        .json(json!({
            "type": "assistant",
            "message": {"content": [{"type": "thinking", "thinking": "hmm"}]},
        }))
        .assistant_thinking("hmm", "sig-abc")
        .result_ok("ok")
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let stream = start_turn(&mut transport, "think").await;
    let messages = collect_until_result(stream, WAIT).await;
    transport.disconnect().await.unwrap();

    assert_eq!(
        messages.len(),
        3,
        "the signature-less thinking message is kept, got {messages:?}"
    );
    let signatures: Vec<&str> = messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant { message, .. } => match &message.content[0] {
                ContentBlock::Thinking(thinking) => Some(thinking.signature.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(signatures, ["", "sig-abc"]);
}

// ===========================================================================
// 3. `build_command` — what the SDK actually puts on the command line
// ===========================================================================

/// The fake records its own argv and environment, so a test can assert on the
/// command line `build_command` produced without reaching into private code.
#[tokio::test]
async fn build_command_arguments_are_observable() {
    let fake = Transcript::new().exit_with(0).build();

    let cwd = fake.dir().to_path_buf();
    let options = ClaudeCodeOptions::builder()
        .model("fake-opus")
        .allowed_tools(vec!["Bash".into(), "Read".into()])
        .disallowed_tools(vec!["WebFetch".into()])
        .permission_mode(PermissionMode::AcceptEdits)
        .max_turns(7)
        .cwd(cwd.clone())
        .add_extra_arg("dangerously-skip-permissions", None)
        .add_extra_arg("session-note", Some("hello".into()))
        .build();

    let mut transport = fake.transport_with(options);
    transport.connect().await.unwrap();
    let invocation = fake.wait_for_invocation(WAIT).await;
    transport.disconnect().await.unwrap();

    // Always-on protocol flags.
    assert_eq!(
        invocation.flag_value("--output-format").as_deref(),
        Some("stream-json")
    );
    assert_eq!(
        invocation.flag_value("--input-format").as_deref(),
        Some("stream-json")
    );
    assert!(invocation.has_flag("--verbose"));
    // Python-parity: an empty --system-prompt is passed even when unset, and
    // --setting-sources always carries a value (empty by default).
    assert_eq!(
        invocation.flag_value("--system-prompt").as_deref(),
        Some("")
    );
    assert_eq!(
        invocation.flag_value("--setting-sources").as_deref(),
        Some("")
    );
    // Options.
    assert_eq!(
        invocation.flag_value("--model").as_deref(),
        Some("fake-opus")
    );
    assert_eq!(
        invocation.flag_value("--allowedTools").as_deref(),
        Some("Bash,Read")
    );
    assert_eq!(
        invocation.flag_value("--disallowedTools").as_deref(),
        Some("WebFetch")
    );
    assert_eq!(
        invocation.flag_value("--permission-mode").as_deref(),
        Some("acceptEdits")
    );
    assert_eq!(invocation.flag_value("--max-turns").as_deref(), Some("7"));
    // extra_args: bare flags get no value, keys are prefixed with `--`.
    assert!(invocation.has_flag("--dangerously-skip-permissions"));
    assert_eq!(
        invocation.flag_value("--session-note").as_deref(),
        Some("hello")
    );
    // cwd is applied to the process, not passed as a flag.
    assert!(
        std::path::Path::new(&invocation.cwd())
            .canonicalize()
            .ok()
            .zip(cwd.canonicalize().ok())
            .is_some_and(|(a, b)| a == b),
        "the child runs in options.cwd, got {}",
        invocation.cwd()
    );
    // SDK identification.
    assert_eq!(
        invocation.env("CLAUDE_CODE_ENTRYPOINT").as_deref(),
        Some("sdk-rust")
    );
    assert!(invocation.env("CLAUDE_AGENT_SDK_VERSION").is_some());
}

/// `max_output_tokens` is clamped to 32000 and passed through the environment,
/// not the command line.
#[tokio::test]
async fn max_output_tokens_is_clamped_and_passed_through_the_environment() {
    let fake = Transcript::new().exit_with(0).build();
    let options = ClaudeCodeOptions::builder()
        .max_output_tokens(999_999)
        .build();
    let mut transport = fake.transport_with(options);
    transport.connect().await.unwrap();
    let invocation = fake.wait_for_invocation(WAIT).await;
    transport.disconnect().await.unwrap();

    assert_eq!(
        invocation.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS").as_deref(),
        Some("32000")
    );
    assert!(!invocation.has_flag("--max-output-tokens"));
}

/// The recording is a file a failing test prints, so it must never be the place a
/// credential surfaces: names are recorded, values are not — and a
/// credential-shaped name is redacted even when the test allowlisted it.
#[tokio::test]
async fn the_invocation_recording_never_carries_a_credential_value() {
    let fake = Transcript::new().exit_with(0).build();
    let options = ClaudeCodeOptions::builder()
        .env("PO_AUTH_TOKEN", "hunter2-super-secret")
        .env("HARMLESS_SETTING", "visible")
        .env(
            "FAKE_CLAUDE_ARGS_ENV_ALLOW",
            "PO_AUTH_TOKEN,HARMLESS_SETTING",
        )
        .build();
    let mut transport = fake.transport_with(options);
    transport.connect().await.unwrap();
    let invocation = fake.wait_for_invocation(WAIT).await;
    transport.disconnect().await.unwrap();

    let dumped = invocation.raw().to_string();
    assert!(
        !dumped.contains("hunter2-super-secret"),
        "the credential leaked into the recording: {dumped}"
    );
    assert!(
        invocation.has_env("PO_AUTH_TOKEN"),
        "the NAME stays visible so a test can still assert the variable was set"
    );
    assert_eq!(
        invocation.env("PO_AUTH_TOKEN").as_deref(),
        Some("<redacted 20 bytes>")
    );
    assert_eq!(
        invocation.env("HARMLESS_SETTING").as_deref(),
        Some("visible")
    );
    // Not allowlisted at all: the NAME is recorded, the value is not, even though
    // the child inherits the variable. `PATH` is the interesting one because it is
    // the only name here the OS supplies rather than the test: Windows spells it
    // `Path`, and `Invocation`'s lookups compare names with the platform's own case
    // rules, so both halves below mean the same thing on Unix and on Windows.
    assert!(
        invocation.env("PATH").is_none(),
        "a variable outside the allowlist must have no value recorded, got {:?}",
        invocation.env("PATH")
    );
    assert!(
        invocation.has_env("PATH"),
        "its name is still recorded, so a test can assert the child inherited it; \
         recorded names were {:?}",
        invocation.raw().get("env_names")
    );
}

// ===========================================================================
// 4. Control protocol
// ===========================================================================

/// A `control_request` printed by the CLI must reach `take_sdk_control_receiver`
/// verbatim, and the SDK's answer must land on the child's stdin wrapped in a
/// `control_response` envelope.
#[tokio::test]
async fn permission_request_round_trip() {
    let fake = Transcript::new()
        .await_stdin()
        .permission_request("perm-1", "Bash", json!({"command": "rm -rf /"}))
        .await_stdin_containing("control_response")
        .assistant_text("refused")
        .result_ok("refused")
        .wait_eof()
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let mut control_rx = transport
        .take_sdk_control_receiver()
        .expect("the subprocess transport exposes the inbound control channel");
    assert!(
        transport.take_sdk_control_receiver().is_none(),
        "the receiver can only be taken once"
    );

    let stream = start_turn(&mut transport, "delete everything").await;

    let request = tokio::time::timeout(WAIT, control_rx.recv())
        .await
        .expect("a control request must arrive")
        .expect("the control channel stays open");
    assert_eq!(request["type"], "control_request");
    assert_eq!(request["request_id"], "perm-1");
    assert_eq!(request["request"]["subtype"], "can_use_tool");
    assert_eq!(request["request"]["tool_name"], "Bash");

    transport
        .send_sdk_control_response(json!({
            "subtype": "success",
            "request_id": "perm-1",
            "response": {"behavior": "deny", "message": "non"},
        }))
        .await
        .unwrap();

    let messages = collect_until_result(stream, WAIT).await;
    assert_eq!(assistant_texts(&messages), vec!["refused".to_string()]);

    // `send_sdk_control_response` wraps the payload; assert the wire shape.
    let lines = fake.wait_for_stdin_lines(2, WAIT).await;
    let answer: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
    assert_eq!(answer["type"], "control_response");
    assert_eq!(answer["response"]["request_id"], "perm-1");
    assert_eq!(answer["response"]["response"]["behavior"], "deny");

    transport.disconnect().await.unwrap();
}

/// A `hook_callback` request travels the same channel; this is the shape hook
/// dispatch has to answer.
#[tokio::test]
async fn hook_callback_request_reaches_the_control_channel() {
    let fake = Transcript::new()
        .await_stdin()
        .hook_callback("hook-9", "cb-pre-tool", json!({"tool_name": "Write"}))
        .wait_eof()
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let mut control_rx = transport.take_sdk_control_receiver().unwrap();
    let _stream = start_turn(&mut transport, "write a file").await;

    let request = tokio::time::timeout(WAIT, control_rx.recv())
        .await
        .expect("hook callback must arrive")
        .unwrap();
    assert_eq!(request["request"]["subtype"], "hook_callback");
    assert_eq!(request["request"]["callback_id"], "cb-pre-tool");
    assert_eq!(request["request_id"], "hook-9");

    transport.disconnect().await.unwrap();
}

/// The legacy interrupt path: `send_control_request` writes the nested
/// `{"request":{"type":"interrupt","request_id":...}}` shape, and the stdout
/// handler turns the CLI's `control_response` into an `InterruptAck`.
#[tokio::test]
async fn interrupt_is_acknowledged_through_the_legacy_control_channel() {
    let fake = Transcript::new()
        .reply_control_success("interrupt")
        .wait_eof()
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    transport
        .send_control_request(ControlRequest::Interrupt {
            request_id: "int-1".into(),
        })
        .await
        .unwrap();

    let ack = tokio::time::timeout(WAIT, transport.receive_control_response())
        .await
        .expect("an ack must arrive")
        .unwrap()
        .expect("the control channel is open");
    match ack {
        ControlResponse::InterruptAck {
            request_id,
            success,
        } => {
            assert_eq!(request_id, "int-1", "the request_id must be echoed back");
            assert!(success, "subtype=success means the interrupt landed");
        },
    }

    // The fake matched on `request.type`, so the SDK really sent the nested shape.
    let sent: serde_json::Value = serde_json::from_str(&fake.stdin_lines()[0]).unwrap();
    assert_eq!(sent["type"], "control_request");
    assert_eq!(sent["request"]["type"], "interrupt");
    assert_eq!(sent["request"]["request_id"], "int-1");

    transport.disconnect().await.unwrap();
}

/// `subtype: "error"` must come back as an unsuccessful ack, not as a success.
#[tokio::test]
async fn a_control_error_response_produces_an_unsuccessful_ack() {
    let fake = Transcript::new()
        .reply_control_error("interrupt", "nothing to interrupt")
        .wait_eof()
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    transport
        .send_control_request(ControlRequest::Interrupt {
            request_id: "int-err".into(),
        })
        .await
        .unwrap();

    let ack = tokio::time::timeout(WAIT, transport.receive_control_response())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    match ack {
        ControlResponse::InterruptAck {
            request_id,
            success,
        } => {
            assert_eq!(request_id, "int-err");
            assert!(!success);
        },
    }
    transport.disconnect().await.unwrap();
}

/// `send_sdk_control_request` sends an already-formed envelope untouched, and the
/// matching `control_response` is delivered on the SDK control channel so a
/// caller can correlate it by `request_id`.
#[tokio::test]
async fn sdk_control_request_is_sent_verbatim_and_correlated_by_request_id() {
    let fake = Transcript::new()
        .reply_control_with(
            "initialize",
            json!({"subtype": "success", "response": {"commands": ["/clear"]}}),
        )
        .wait_eof()
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let mut control_rx = transport.take_sdk_control_receiver().unwrap();

    transport
        .send_sdk_control_request(json!({
            "type": "control_request",
            "request_id": "req-init-1",
            "request": {"subtype": "initialize", "hooks": null},
        }))
        .await
        .unwrap();

    let response = tokio::time::timeout(WAIT, control_rx.recv())
        .await
        .expect("the scripted response must arrive")
        .unwrap();
    assert_eq!(response["type"], "control_response");
    assert_eq!(response["response"]["request_id"], "req-init-1");
    assert_eq!(response["response"]["response"]["commands"][0], "/clear");

    // Verbatim: no extra wrapping on the way out.
    let sent: serde_json::Value = serde_json::from_str(&fake.stdin_lines()[0]).unwrap();
    assert_eq!(sent["request_id"], "req-init-1");
    assert_eq!(sent["request"]["subtype"], "initialize");

    transport.disconnect().await.unwrap();
}

/// `reply_control_for_id` matches on `request_id` instead of subtype — useful when
/// several requests of the same kind are in flight.
#[tokio::test]
async fn a_control_reply_can_be_matched_on_request_id() {
    let fake = Transcript::new()
        .reply_control_for_id("second")
        .wait_eof()
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let mut control_rx = transport.take_sdk_control_receiver().unwrap();

    for id in ["first", "second"] {
        transport
            .send_sdk_control_request(json!({
                "type": "control_request",
                "request_id": id,
                "request": {"subtype": "set_model", "model": "fake"},
            }))
            .await
            .unwrap();
    }

    let response = tokio::time::timeout(WAIT, control_rx.recv())
        .await
        .expect("only the matching request is answered")
        .unwrap();
    assert_eq!(response["response"]["request_id"], "second");
    transport.disconnect().await.unwrap();
}

// ===========================================================================
// 5. Input lifecycle
// ===========================================================================

/// `end_input` drops the stdin sender, which closes the child's stdin. The
/// transcript's `wait_eof` only completes when that really happens, so the
/// assistant message arriving proves the CLI saw EOF.
#[tokio::test]
async fn end_input_closes_the_child_stdin() {
    let fake = Transcript::new()
        .await_stdin()
        .assistant_text("received your prompt")
        .wait_eof()
        .assistant_text("saw EOF")
        .result_ok("saw EOF")
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let stream = start_turn(&mut transport, "then stop").await;

    transport.end_input().await.unwrap();
    let messages = collect_until_result(stream, WAIT).await;
    assert_eq!(
        assistant_texts(&messages),
        vec!["received your prompt".to_string(), "saw EOF".to_string()]
    );

    // After end_input the stdin channel is gone: sending must fail cleanly.
    let err = transport
        .send_message(user("too late"))
        .await
        .expect_err("stdin is closed");
    assert!(
        matches!(err, SdkError::InvalidState { .. }),
        "expected InvalidState, got {err:?}"
    );
    transport.disconnect().await.unwrap();
}

/// Sending before `connect` is a state error, not a panic and not a silent drop.
#[tokio::test]
async fn sending_before_connect_is_an_invalid_state_error() {
    let fake = default_session();
    let mut transport = fake.transport();
    let err = transport
        .send_message(user("hello"))
        .await
        .expect_err("not connected yet");
    assert!(matches!(err, SdkError::InvalidState { .. }), "got {err:?}");
    assert!(
        matches!(
            transport
                .send_control_request(ControlRequest::Interrupt {
                    request_id: "x".into()
                })
                .await,
            Err(SdkError::InvalidState { .. })
        ),
        "control requests are gated on the same state"
    );
}

/// `connect` on an already-connected transport is a no-op: it must not spawn a
/// second child.
#[tokio::test]
async fn connect_is_idempotent() {
    let fake = Transcript::new()
        .await_stdin()
        .result_ok("ok")
        .wait_eof()
        .build();
    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let pid = transport.child_pid().unwrap();
    transport.connect().await.unwrap();
    assert_eq!(
        transport.child_pid(),
        Some(pid),
        "the second connect must not replace the child"
    );
    transport.disconnect().await.unwrap();
}

/// `disconnect` on a transport that was never connected is a no-op, and calling it
/// twice must not fail.
#[tokio::test]
async fn disconnect_is_safe_to_call_twice_and_before_connect() {
    let fake = Transcript::new().wait_eof_for(500).build();
    let mut transport = fake.transport();
    transport.disconnect().await.expect("no-op before connect");
    transport.connect().await.unwrap();
    transport.disconnect().await.unwrap();
    transport
        .disconnect()
        .await
        .expect("second disconnect is a no-op");
    assert!(!transport.is_connected());
}

/// Dropping the transport without disconnecting must still kill the child — this
/// is what keeps a panicking test from leaking a process.
#[tokio::test]
async fn dropping_the_transport_kills_the_child() {
    let fake = Transcript::new().wait_eof().build();
    let pid = {
        let mut transport = fake.transport();
        transport.connect().await.unwrap();
        transport.child_pid().expect("pid")
    };
    // Give the SIGKILL a moment to be delivered and reaped.
    poll_until(WAIT, || !process_exists(pid)).await;
    assert_no_process(pid);
}

// ===========================================================================
// 6. Failure paths
// ===========================================================================

/// A CLI that exits before printing anything is **not** reported as an error:
/// `connect` only checks that `spawn()` worked, so it still returns `Ok`.
///
/// What it no longer does is keep *claiming* the session: `is_connected()` used
/// to answer `true` for ever, with an empty stream, which is how
/// `OptimizedClient`'s pool handed a dead child to the next caller. The stdout
/// reader now clears a liveness flag on EOF.
#[tokio::test]
async fn a_cli_that_exits_immediately_produces_an_empty_stream() {
    let fake = Transcript::new().exit_with(3).build();
    let mut transport = fake.transport();
    transport
        .connect()
        .await
        .expect("spawn succeeded, so connect reports success");

    let stream = transport.receive_messages();
    let messages = collect_n(stream, 1, Duration::from_millis(400)).await;
    assert!(
        messages.is_empty(),
        "nothing was printed, so nothing arrives: {messages:?}"
    );
    assert!(
        poll_until(WAIT, || !transport.is_connected()).await,
        "the CLI is gone: the transport must stop reporting a live session"
    );
    transport.disconnect().await.unwrap();
}

/// Garbage on stdout is skipped line by line: non-JSON, malformed JSON and valid
/// JSON that is not a message must none of them kill the stream.
#[tokio::test]
async fn garbage_on_stdout_is_skipped_without_killing_the_stream() {
    let fake = Transcript::new()
        .await_stdin()
        .garbage()
        .malformed_json()
        .unparseable_message()
        .raw("")
        .json(json!({"type": "something_the_sdk_does_not_know", "x": 1}))
        .assistant_text("survived")
        .result_ok("survived")
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let stream = start_turn(&mut transport, "send me junk").await;
    let messages = collect_until_result(stream, WAIT).await;
    transport.disconnect().await.unwrap();

    assert_eq!(
        messages.len(),
        2,
        "only the two well-formed messages survive, got {messages:?}"
    );
    assert_eq!(assistant_texts(&messages), vec!["survived".to_string()]);
}

/// A truncated final line (no trailing newline, then the process dies) must be
/// dropped, not half-parsed.
#[tokio::test]
async fn a_truncated_final_line_is_dropped() {
    let fake = Transcript::new()
        .await_stdin()
        .assistant_text("complete")
        .partial(r#"{"type":"result","subtype":"suc"#)
        .exit_with(0)
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let stream = start_turn(&mut transport, "cut me off").await;
    let messages = collect_n(stream, 2, Duration::from_millis(800)).await;
    transport.disconnect().await.unwrap();

    assert_eq!(assistant_texts(&messages), vec!["complete".to_string()]);
    assert!(
        !messages.iter().any(|m| matches!(m, Message::Result { .. })),
        "the half-written result must not be materialised: {messages:?}"
    );
}

/// The CLI dying mid-turn, after an assistant message but before the result.
///
/// Everything already printed arrives — including the `System`/`error` the
/// stderr handler emits once stderr hits EOF — and then the stream **ends**. No
/// error is surfaced (the broadcast carries `Message`, not `Result`, so a
/// consumer learns of the death from the end of the stream, not from an item).
///
/// It used to end *nothing*: the broadcast `Sender` was stored on the transport,
/// so it outlived the child and a consumer looping on `receive_messages()` blocked
/// for ever. See `stream_ends_when_the_cli_dies_mid_stream` below.
#[tokio::test]
async fn a_cli_dying_mid_turn_delivers_what_it_printed_then_ends() {
    let fake = Transcript::new()
        .await_stdin()
        .assistant_text("half an answer")
        .stderr("fatal: the fake CLI is going down")
        .exit_with(9)
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let stream = start_turn(&mut transport, "die halfway").await;

    // Both of these are broadcast from different tasks, so do not depend on the
    // order they interleave in.
    let messages = collect_n(stream, 2, WAIT).await;
    assert_eq!(
        messages.len(),
        2,
        "assistant turn + stderr report, got {messages:?}"
    );
    assert_eq!(
        assistant_texts(&messages),
        vec!["half an answer".to_string()]
    );
    let details = messages
        .iter()
        .find_map(|m| match m {
            Message::System { subtype, data } if subtype == "error" => {
                Some(data["details"].as_str().unwrap_or_default().to_string())
            },
            _ => None,
        })
        .expect("the CLI's stderr must surface as a System/error message");
    assert!(details.contains("going down"), "got {details}");

    // The child is gone. A stream subscribed now is already finished: it yields
    // `None` at once instead of hanging.
    let mut stream = transport.receive_messages();
    let ended = tokio::time::timeout(
        Duration::from_millis(500),
        futures::StreamExt::next(&mut stream),
    )
    .await;
    assert!(
        matches!(ended, Ok(None)),
        "once the child is gone the stream must end, not hang, got {ended:?}"
    );

    transport.disconnect().await.unwrap();
}

/// When the child is gone the message stream ends, so a
/// `while let Some(_) = stream.next()` loop terminates.
///
/// Was a bug (`SubprocessTransport::spawn_process` / `receive_messages`): the
/// broadcast sender was stored in `self.message_broadcast_tx` and therefore
/// outlived the child, so when the stdout reader task exited because the CLI had
/// died, nothing closed the channel and every consumer blocked for ever. The
/// transport now keeps only a `Receiver` — the senders live in the two reader
/// tasks and go with them. Still missing, and reported: the non-zero exit status
/// is not broadcast before the channel closes.
#[tokio::test]
async fn stream_ends_when_the_cli_dies_mid_stream() {
    let fake = Transcript::new()
        .await_stdin()
        .assistant_text("bye")
        .exit_with(9)
        .build();
    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let mut stream = start_turn(&mut transport, "die").await;

    let mut items = 0;
    let ended = tokio::time::timeout(Duration::from_secs(2), async {
        while futures::StreamExt::next(&mut stream).await.is_some() {
            items += 1;
        }
    })
    .await;
    assert!(
        ended.is_ok(),
        "the stream must end once the child is gone (saw {items} items)"
    );
}

/// stderr is drained, filtered and finally surfaced as a `System`/`error`
/// message — but only once stderr reaches EOF, i.e. after the CLI exits.
#[tokio::test]
async fn stderr_is_surfaced_as_a_system_error_message_at_eof() {
    let fake = Transcript::new()
        .await_stdin()
        .stderr("Error: quota exceeded")
        .comment("filtered as hook-abort noise, must not reach the consumer")
        .stderr("Error in hook callback: whatever")
        .stderr("      at Object.<anonymous> (/x.js:1:1)")
        .result_ok("done")
        .exit_with(0)
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let stream = start_turn(&mut transport, "fail please").await;
    let messages = collect_n(stream, 3, WAIT).await;
    transport.disconnect().await.unwrap();

    let system_error = messages
        .iter()
        .find_map(|m| match m {
            Message::System { subtype, data } if subtype == "error" => Some(data),
            _ => None,
        })
        .expect("stderr must be reported as a System/error message");
    let details = system_error["details"].as_str().unwrap_or_default();
    assert!(details.contains("quota exceeded"), "got {details}");
    assert!(
        !details.contains("Error in hook callback"),
        "hook-abort noise must stay filtered: {details}"
    );
    assert!(
        !details.contains("      at Object"),
        "JS stack frames must stay filtered: {details}"
    );
    assert_eq!(system_error["source"], "stderr");
}

/// Messages printed before the test subscribed are gone: `receive_messages`
/// subscribes to a tokio broadcast, which does not replay history.
///
/// This is a real trap for consumers — the `system`/`init` message is the first
/// thing the CLI prints, so a caller that connects and only then subscribes can
/// miss the session id entirely. The deterministic workaround is the
/// `await_stdin`-first transcript used everywhere else in this file.
///
/// The proof is race-free: an early `subscribe_messages()` stream observes `init`,
/// which establishes that the stdout task had already broadcast it; a stream
/// created *after* that therefore cannot possibly see it.
#[tokio::test]
async fn messages_emitted_before_subscribe_are_lost() {
    let fake = Transcript::new()
        .init("sess-lost")
        .await_stdin()
        .assistant_text("too late")
        .result_ok("too late")
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();

    let mut early = transport
        .subscribe_messages()
        .expect("a broadcast exists once connected");
    let first = tokio::time::timeout(WAIT, futures::StreamExt::next(&mut early))
        .await
        .expect("init must reach an early subscriber")
        .expect("stream item")
        .expect("parsed");
    assert!(
        matches!(&first, Message::System { subtype, .. } if subtype == "init"),
        "got {first:?}"
    );
    drop(early);

    let stream = start_turn(&mut transport, "hello").await;
    let messages = collect_until_result(stream, WAIT).await;
    transport.disconnect().await.unwrap();

    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::System { subtype, .. } if subtype == "init")),
        "documented hazard: init was broadcast before this stream subscribed and is \
         unrecoverable, got {messages:?}"
    );
    assert_eq!(assistant_texts(&messages), vec!["too late".to_string()]);
}

// ===========================================================================
// 7. Directive coverage that the tests above do not already exercise
// ===========================================================================

/// `sleep` and `await_stdin_containing` let a transcript pin the *ordering* of a
/// conversation, not just its content.
#[tokio::test]
async fn sleep_and_contains_make_the_ordering_deterministic() {
    let fake = Transcript::new()
        .comment("ignore the first prompt entirely, answer only the second")
        .await_stdin_containing("deuxieme")
        .sleep_ms(120)
        .assistant_text("answering the second prompt")
        .result_ok("answering the second prompt")
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let stream = transport.receive_messages();
    transport.send_message(user("premier")).await.unwrap();
    transport.send_message(user("deuxieme")).await.unwrap();

    let started = std::time::Instant::now();
    let messages = collect_until_result(stream, WAIT).await;
    transport.disconnect().await.unwrap();

    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "the sleep directive really delays the stream"
    );
    assert_eq!(
        assistant_texts(&messages),
        vec!["answering the second prompt".to_string()]
    );
    assert_eq!(fake.stdin_lines().len(), 2, "both prompts reached the CLI");
}

/// `await_stdin_lines` and `await_stdin_optional`: counted and best-effort waits.
#[tokio::test]
async fn counted_and_optional_stdin_waits() {
    let fake = Transcript::new()
        .await_stdin_lines(2)
        .assistant_text("got both")
        .comment("nothing more will come; continue anyway instead of failing")
        .await_stdin_optional(150)
        .result_ok("got both")
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let stream = transport.receive_messages();
    transport.send_message(user("un")).await.unwrap();
    transport.send_message(user("deux")).await.unwrap();

    let messages = collect_until_result(stream, WAIT).await;
    transport.disconnect().await.unwrap();
    assert_eq!(assistant_texts(&messages), vec!["got both".to_string()]);
    assert!(matches!(messages.last(), Some(Message::Result { .. })));
}

/// A flat `system` message (payload as top-level siblings, no `data` key) is how
/// the current CLI reports workflow progress; the SDK must keep the payload.
#[tokio::test]
async fn flat_system_messages_keep_their_payload() {
    let fake = Transcript::new()
        .await_stdin()
        .system("task_progress", json!({"task_id": "t-1", "completed": 2}))
        .result_ok("ok")
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let stream = start_turn(&mut transport, "progress?").await;
    let messages = collect_until_result(stream, WAIT).await;
    transport.disconnect().await.unwrap();

    match &messages[0] {
        Message::System { subtype, data } => {
            assert_eq!(subtype, "task_progress");
            assert_eq!(data["task_id"], "t-1");
            assert_eq!(data["completed"], 2);
        },
        other => panic!("expected a flat system message, got {other:?}"),
    }
}

/// `control_response` lines with no matching request are dropped on the legacy
/// channel but still forwarded on the SDK control channel.
#[tokio::test]
async fn an_unsolicited_control_response_is_still_forwarded() {
    let fake = Transcript::new()
        .await_stdin()
        .control_response("never-requested", true)
        .result_ok("ok")
        .wait_eof()
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let mut control_rx = transport.take_sdk_control_receiver().unwrap();
    let _stream = start_turn(&mut transport, "go").await;

    let forwarded = tokio::time::timeout(WAIT, control_rx.recv())
        .await
        .expect("forwarded even with no pending request")
        .unwrap();
    assert_eq!(forwarded["response"]["request_id"], "never-requested");

    // And the legacy channel also sees it, as an InterruptAck for an id nobody asked about.
    let ack = tokio::time::timeout(WAIT, transport.receive_control_response())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        ack,
        ControlResponse::InterruptAck { ref request_id, success: true } if request_id == "never-requested"
    ));
    transport.disconnect().await.unwrap();
}

/// `clone_stdin_sender` is the lock-free write path used while a stream holds the
/// transport; a line written through it must reach the CLI.
#[tokio::test]
async fn the_cloned_stdin_sender_writes_to_the_child() {
    let fake = Transcript::new()
        .await_stdin_containing("out-of-band")
        .assistant_text("heard you")
        .result_ok("heard you")
        .build();

    let mut transport = fake.transport();
    assert!(
        transport.clone_stdin_sender().is_none(),
        "no stdin before connect"
    );
    transport.connect().await.unwrap();
    let stream = transport.receive_messages();
    let stdin = transport
        .clone_stdin_sender()
        .expect("stdin sender after connect");
    stdin
        .send(json!({"type": "user", "note": "out-of-band"}).to_string())
        .await
        .unwrap();

    let messages = collect_until_result(stream, WAIT).await;
    transport.disconnect().await.unwrap();
    assert_eq!(assistant_texts(&messages), vec!["heard you".to_string()]);
}

// ===========================================================================
// helpers
// ===========================================================================

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    // Signal 0 only probes for existence/permission.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// No portable equivalent of `kill(pid, 0)`; on Windows the liveness assertions
/// below are vacuous and only the `Result`/state assertions carry weight.
#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    false
}

fn assert_no_process(pid: u32) {
    assert!(
        !process_exists(pid),
        "child {pid} is still alive after shutdown — a leaked process"
    );
}
