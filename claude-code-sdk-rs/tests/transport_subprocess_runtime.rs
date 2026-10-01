//! Runtime behaviour of `SubprocessTransport` — the parts that need a real
//! child process, real pipes and the real line framing.
//!
//! Everything here drives the `fake_claude` test double (no network, no `claude`
//! install, no shell). `tests/fake_cli_harness.rs` is the tutorial; this file is
//! the exhaustive sweep of `src/transport/subprocess.rs`: the branches of the
//! stdout dispatcher, the stderr filters and diagnostics, the stdin writer's
//! failure path, the shutdown escalation, and the state guards.
//!
//! The one ordering rule stays: `receive_messages()` subscribes to a tokio
//! broadcast, which never replays. Start transcripts with `await_stdin()` and go
//! through [`support::start_turn`].

mod support;

use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use nexus_claude::{ClaudeCodeOptions, ControlRequest, McpServerConfig, Message, SdkError};
use serde_json::json;
use support::*;

/// Generous enough for a spawn on a loaded runner, short enough that a genuinely
/// stuck test fails instead of eating the job's time budget.
const WAIT: Duration = Duration::from_secs(5);

// ===========================================================================
// log capture
// ===========================================================================

/// `tracing` macros do not evaluate their arguments when no subscriber is
/// interested, so the transport's logging code — including the redaction that
/// keeps MCP credentials out of the spawn log — only runs under a subscriber.
/// One is installed for the whole binary, writing into [`LOG`] instead of the
/// terminal.
static LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct LogSink;

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut held) = LOG.lock() {
            held.extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture_logs() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(|| LogSink)
            .try_init();
    });
}

fn logged() -> String {
    let held = LOG.lock().expect("log buffer");
    String::from_utf8_lossy(held.as_slice()).into_owned()
}

// ===========================================================================
// 1. The spawn log must not become the place a credential surfaces
// ===========================================================================

/// `spawn_process` logs the command line it is about to run, and `build_command`
/// logs it a second time at debug level. Both must go through
/// `describe_command_redacted`: `--mcp-config` carries each MCP server's `env`,
/// which in this repository is a database password and a session token.
///
/// Regression: the `info!` in `spawn_process` was fixed (commit "never write
/// secrets to the log when spawning the CLI") but the `debug!` in
/// `build_command` still printed `cmd.get_args()` verbatim — i.e. the whole
/// `--mcp-config` JSON — whenever debug logging was on.
#[tokio::test]
async fn no_log_line_of_a_spawn_carries_an_mcp_credential() {
    capture_logs();
    let fake = Transcript::new().exit_with(0).build();

    let mut servers = std::collections::HashMap::new();
    servers.insert(
        "po".to_string(),
        McpServerConfig::Stdio {
            command: "po-mcp".to_string(),
            args: None,
            env: Some(std::collections::HashMap::from([(
                "NEO4J_PASSWORD".to_string(),
                "hunter2-from-the-spawn-log".to_string(),
            )])),
        },
    );
    let options = ClaudeCodeOptions::builder().mcp_servers(servers).build();

    let mut transport = fake.transport_with(options);
    transport.connect().await.unwrap();
    let invocation = fake.wait_for_invocation(WAIT).await;
    transport.disconnect().await.unwrap();

    // The credential really was on the command line...
    assert!(
        invocation
            .flag_value("--mcp-config")
            .is_some_and(|v| v.contains("hunter2-from-the-spawn-log")),
        "the test must actually put the secret on the command line"
    );
    // ...and nowhere in the log.
    let log = logged();
    assert!(
        !log.contains("hunter2-from-the-spawn-log"),
        "the MCP credential leaked into the log"
    );
    assert!(
        log.contains("Starting Claude CLI with command: program="),
        "spawn_process's own log line must be the redacted description, got:\n{log}"
    );
    assert!(
        log.contains("Executing Claude CLI command: program="),
        "and so must build_command's debug line, got:\n{log}"
    );
    assert!(
        log.contains("<redacted"),
        "the mcp-config value is replaced, not dropped, got:\n{log}"
    );
}

// ===========================================================================
// 2. Process group — what `disconnect` and `Drop` assume when they signal -pid
// ===========================================================================

/// `build_command`'s `pre_exec` makes the child a process-group leader so that
/// `disconnect`/`Drop` can signal `-pid` and take the CLI's own children (bash,
/// find, sleep) with it. If `setpgid` silently failed, `-pid` would address the
/// SDK's *own* group — this asserts the invariant those two signals rely on.
#[cfg(unix)]
#[tokio::test]
async fn the_child_leads_its_own_process_group() {
    capture_logs();
    let fake = Transcript::new().wait_eof().build();
    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let pid = transport.child_pid().expect("pid") as i32;

    let pgid = unsafe { libc::getpgid(pid) };
    assert_eq!(
        pgid, pid,
        "the child must lead its own group, otherwise kill(-pid) hits the SDK's group"
    );
    assert_ne!(
        pgid,
        unsafe { libc::getpgid(0) },
        "and that group must not be the test runner's"
    );

    transport.disconnect().await.unwrap();
}

/// A `cli_path` that is not an executable fails at `spawn`, which is the one
/// launch failure `connect` really does report — as `ProcessError`, carrying the
/// OS error, and without leaving the transport looking connected.
#[tokio::test]
async fn a_cli_path_that_cannot_be_spawned_is_a_process_error() {
    capture_logs();
    let dir = tempfile::tempdir().expect("temp dir");
    let not_an_executable = dir.path().join("claude");
    std::fs::write(&not_an_executable, b"not a program").expect("write");

    let mut transport =
        SubprocessTransport::with_cli_path(ClaudeCodeOptions::default(), &not_an_executable);
    let err = transport
        .connect()
        .await
        .expect_err("this cannot be executed");
    assert!(
        matches!(err, SdkError::ProcessError(_)),
        "expected the OS error to be carried through, got {err:?}"
    );
    assert!(!transport.is_connected());
    assert!(transport.child_pid().is_none());
}

// ===========================================================================
// 3. Liveness and stream termination
// ===========================================================================

/// `connect()` only checks that `spawn()` worked, so a CLI that dies immediately
/// still produces `Ok(())`. What must not survive is the *claim* of being
/// connected: once the CLI's stdout reaches EOF the session is over and
/// `is_connected()` has to say so.
#[tokio::test]
async fn is_connected_turns_false_once_the_cli_is_gone() {
    capture_logs();
    let fake = Transcript::new().exit_with(3).build();
    let mut transport = fake.transport();
    transport
        .connect()
        .await
        .expect("spawn succeeded, so connect still reports success");

    assert!(
        poll_until(WAIT, || !transport.is_connected()).await,
        "is_connected() must stop claiming a session the CLI has already left"
    );
    transport.disconnect().await.unwrap();
}

/// The consumer-visible half of the same fix: the message stream has to *end*,
/// so `while let Some(m) = stream.next().await` terminates instead of blocking
/// forever on a broadcast nobody will ever send on again.
#[tokio::test]
async fn the_message_stream_ends_when_the_cli_exits() {
    capture_logs();
    let fake = Transcript::new()
        .await_stdin()
        .assistant_text("last words")
        .result_ok("last words")
        .exit_with(0)
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let mut stream = start_turn(&mut transport, "then die").await;

    let mut texts = Vec::new();
    let ended = tokio::time::timeout(WAIT, async {
        while let Some(item) = futures::StreamExt::next(&mut stream).await {
            if let Ok(message) = item
                && let Some(text) = assistant_text_of(&message)
            {
                texts.push(text);
            }
        }
    })
    .await;

    assert!(
        ended.is_ok(),
        "the stream must end once every sender is gone"
    );
    assert_eq!(
        texts,
        vec!["last words".to_string()],
        "ending the stream must not drop what was already printed"
    );
    transport.disconnect().await.unwrap();
}

/// A subscriber that stops polling falls behind the broadcast ring buffer. The
/// transport logs the gap and skips it: the stream stays alive and keeps
/// yielding, it does not surface an error and does not end early.
#[tokio::test]
async fn a_lagging_subscriber_loses_messages_but_keeps_its_stream() {
    capture_logs();
    const EMITTED: usize = 40;
    let mut transcript = Transcript::new().await_stdin();
    for i in 0..EMITTED {
        transcript = transcript.assistant_text(&format!("chunk-{i}"));
    }
    let fake = transcript.result_ok("done").build();

    // A two-slot ring buffer: anything the reader writes while we are not
    // polling is overwritten.
    let options = ClaudeCodeOptions::builder()
        .cli_channel_buffer_size(2)
        .build();
    let mut transport = fake.transport_with(options);
    transport.connect().await.unwrap();
    let stream = transport.receive_messages();
    transport.send_message(user("flood me")).await.unwrap();

    // Do not poll while the CLI floods the channel.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let messages = collect_until_result(stream, WAIT).await;
    transport.disconnect().await.unwrap();

    assert!(
        messages.len() < EMITTED,
        "a two-slot buffer cannot have delivered all {EMITTED} messages, got {}",
        messages.len()
    );
    assert!(
        messages.iter().any(|m| matches!(m, Message::Result { .. })),
        "lagging must not end the stream: the later result still arrives, got {messages:?}"
    );
}

/// Before `connect` there is no broadcast at all, so `receive_messages` hands
/// back an empty stream rather than panicking or hanging.
#[tokio::test]
async fn receive_messages_before_connect_is_an_empty_stream() {
    capture_logs();
    let fake = default_session();
    let mut transport = fake.transport();
    let messages = collect_n(transport.receive_messages(), 1, Duration::from_millis(200)).await;
    assert!(messages.is_empty(), "got {messages:?}");
    assert!(transport.subscribe_messages().is_none());
}

// ===========================================================================
// 4. The `Transport` trait object — separate code from the inherent methods
// ===========================================================================

/// `subscribe_messages` and `take_sdk_control_receiver` exist twice: as inherent
/// methods (what a concrete `SubprocessTransport` resolves to) and as trait
/// methods (what a `Box<dyn Transport>` calls). Only the trait ones are reached
/// through a trait object, so they need their own test.
#[tokio::test]
async fn the_trait_object_exposes_the_broadcast_and_the_control_channel() {
    capture_logs();
    let fake = Transcript::new()
        .await_stdin()
        .assistant_text("via dyn Transport")
        .result_ok("via dyn Transport")
        .wait_eof()
        .build();

    let mut concrete = fake.transport();
    let transport: &mut dyn Transport = &mut concrete;

    assert!(
        transport.subscribe_messages().is_none(),
        "no broadcast before connect, through the trait too"
    );
    transport.connect().await.unwrap();

    let stream = transport
        .subscribe_messages()
        .expect("the trait method must delegate to the inherent one");
    assert!(
        transport.take_sdk_control_receiver().is_some(),
        "the trait method hands over the inbound control channel"
    );
    assert!(
        transport.take_sdk_control_receiver().is_none(),
        "and only once"
    );

    // `as_any_mut` is the downcast hook callers use to get back to the concrete
    // transport; it must hand back *this* transport, not a new one.
    let pid_through_trait = transport.child_pid();
    let downcast = transport
        .as_any_mut()
        .downcast_mut::<nexus_claude::transport::SubprocessTransport>()
        .expect("as_any_mut must expose the concrete SubprocessTransport");
    assert_eq!(downcast.child_pid(), pid_through_trait);

    transport.send_message(user("hello")).await.unwrap();
    let messages = collect_until_result(stream, WAIT).await;
    assert_eq!(
        assistant_texts(&messages),
        vec!["via dyn Transport".to_string()]
    );
    transport.disconnect().await.unwrap();
}

/// `receive_sdk_control_request` is the awaiting counterpart of
/// `take_sdk_control_receiver`: it borrows the channel instead of moving it, and
/// reports `None` once the channel has been taken away.
#[tokio::test]
async fn receive_sdk_control_request_borrows_the_channel_until_it_is_taken() {
    capture_logs();
    let fake = Transcript::new()
        .await_stdin()
        .permission_request("perm-borrow", "Bash", json!({"command": "ls"}))
        .wait_eof()
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let _stream = start_turn(&mut transport, "ask me").await;

    let request = tokio::time::timeout(WAIT, transport.receive_sdk_control_request())
        .await
        .expect("a control request must arrive")
        .expect("the channel is open");
    assert_eq!(request["request_id"], "perm-borrow");

    let taken = transport.take_sdk_control_receiver();
    assert!(taken.is_some(), "the channel can still be moved out");
    assert!(
        transport.receive_sdk_control_request().await.is_none(),
        "with the channel gone the awaiting accessor reports None instead of hanging"
    );
    transport.disconnect().await.unwrap();
}

// ===========================================================================
// 5. The stdout dispatcher — one branch per message shape
// ===========================================================================

/// The dispatcher recognises four envelopes that are *not* messages and routes
/// each to the SDK control channel: `control_request` (tested in the harness),
/// the newer `{"type":"control","control":{...}}`, the legacy
/// `sdk_control_request`, and a `system` message whose subtype starts with
/// `sdk_control:`. The last one is special: it is forwarded *and* still parsed
/// as a regular message.
#[tokio::test]
async fn every_control_envelope_shape_reaches_the_sdk_control_channel() {
    capture_logs();
    let fake = Transcript::new()
        .await_stdin()
        .json(json!({"type": "control", "control": {"subtype": "new_format", "n": 1}}))
        .json(
            json!({"type": "sdk_control_request", "request_id": "legacy-1",
                     "request": {"subtype": "can_use_tool"}}),
        )
        .system("sdk_control:progress", json!({"step": 2}))
        .result_ok("routed")
        .wait_eof()
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let mut control_rx = transport.take_sdk_control_receiver().unwrap();
    let stream = start_turn(&mut transport, "route these").await;

    // `control` is unwrapped: the inner object is forwarded, not the envelope.
    let new_format = tokio::time::timeout(WAIT, control_rx.recv())
        .await
        .expect("the new control format must be forwarded")
        .unwrap();
    assert_eq!(new_format["subtype"], "new_format");
    assert!(
        new_format.get("type").is_none(),
        "the `control` payload is forwarded without its envelope, got {new_format}"
    );

    // the legacy envelope is forwarded whole, request_id included
    let legacy = tokio::time::timeout(WAIT, control_rx.recv())
        .await
        .expect("the legacy envelope must be forwarded")
        .unwrap();
    assert_eq!(legacy["type"], "sdk_control_request");
    assert_eq!(legacy["request_id"], "legacy-1");

    let system = tokio::time::timeout(WAIT, control_rx.recv())
        .await
        .expect("an sdk_control: system message must be forwarded")
        .unwrap();
    assert_eq!(system["subtype"], "sdk_control:progress");

    // ... and unlike the other three, it is *also* delivered as a Message.
    let messages = collect_until_result(stream, WAIT).await;
    transport.disconnect().await.unwrap();
    assert!(
        messages.iter().any(
            |m| matches!(m, Message::System { subtype, .. } if subtype == "sdk_control:progress")
        ),
        "the sdk_control: system message must reach consumers too, got {messages:?}"
    );
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, Message::System { subtype, .. } if subtype == "new_format")),
        "the control envelopes must NOT reach consumers, got {messages:?}"
    );
}

/// A `control_response` only becomes a legacy `InterruptAck` when it carries a
/// `request_id` (or its camelCase spelling). Without one it is forwarded on the
/// SDK channel and dropped on the legacy channel — there is nothing to
/// correlate.
#[tokio::test]
async fn a_control_response_needs_a_request_id_to_become_an_ack() {
    capture_logs();
    let fake = Transcript::new()
        .await_stdin()
        .json(json!({"type": "control_response", "response": {"subtype": "success"}}))
        .json(json!({"type": "control_response",
                     "response": {"subtype": "success", "requestId": "camel-1"}}))
        .result_ok("ok")
        .wait_eof()
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let mut control_rx = transport.take_sdk_control_receiver().unwrap();
    let _stream = start_turn(&mut transport, "go").await;

    // Both are forwarded verbatim on the SDK channel.
    let first = tokio::time::timeout(WAIT, control_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(first["response"].get("request_id").is_none());
    let second = tokio::time::timeout(WAIT, control_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second["response"]["requestId"], "camel-1");

    // Only the second produced an ack, and `requestId` was accepted as the id.
    let ack = tokio::time::timeout(WAIT, transport.receive_control_response())
        .await
        .expect("the camelCase response must be correlated")
        .unwrap()
        .unwrap();
    match ack {
        nexus_claude::ControlResponse::InterruptAck {
            request_id,
            success,
        } => {
            assert_eq!(
                request_id, "camel-1",
                "the id-less response produced no ack, so this is the camelCase one"
            );
            assert!(success);
        },
    }
    transport.disconnect().await.unwrap();
}

/// JSON that is well-formed but has no usable `type` falls straight through to
/// the message parser, which ignores it. Nothing is forwarded and nothing is
/// broadcast — the stream must not stall on it.
#[tokio::test]
async fn json_without_a_string_type_is_ignored_rather_than_routed() {
    capture_logs();
    let fake = Transcript::new()
        .await_stdin()
        .json(json!({"no_type_at_all": 1}))
        .json(json!({"type": 42}))
        .json(json!({"type": null, "x": "y"}))
        .assistant_text("still here")
        .result_ok("still here")
        .wait_eof()
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let mut control_rx = transport.take_sdk_control_receiver().unwrap();
    let stream = start_turn(&mut transport, "typeless").await;
    let messages = collect_until_result(stream, WAIT).await;
    transport.disconnect().await.unwrap();

    assert_eq!(
        messages.len(),
        2,
        "only the assistant turn and the result survive, got {messages:?}"
    );
    assert!(
        control_rx.try_recv().is_err(),
        "a type-less line must not be mistaken for a control envelope"
    );
}

// ===========================================================================
// 6. stderr — filters, diagnostics, and the two user-supplied sinks
// ===========================================================================

/// A writer that keeps what the SDK pushed into `options.debug_stderr`.
struct Tap(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Tap {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("tap").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `debug_stderr` and `stderr_callback` are two independent sinks and both get
/// *every* stderr line, including the ones the broadcast path filters out as
/// hook-abort noise. Setting `debug_stderr` also adds `--debug-to-stderr` to the
/// command line.
#[tokio::test]
async fn both_stderr_sinks_receive_every_line_including_the_filtered_noise() {
    capture_logs();
    let fake = Transcript::new()
        .await_stdin()
        .stderr("Error: quota exceeded")
        .stderr("Error in hook callback: aborted")
        .result_ok("done")
        .exit_with(0)
        .build();

    let tapped = Arc::new(Mutex::new(Vec::new()));
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_in_callback = Arc::clone(&seen);

    let mut options = ClaudeCodeOptions::builder()
        .stderr_callback(Arc::new(move |line: &str| {
            seen_in_callback
                .lock()
                .expect("callback sink")
                .push(line.to_string());
        }))
        .build();
    options.debug_stderr = Some(Arc::new(tokio::sync::Mutex::new(Tap(Arc::clone(&tapped)))));

    let mut transport = fake.transport_with(options);
    transport.connect().await.unwrap();
    assert!(
        fake.wait_for_invocation(WAIT)
            .await
            .has_flag("--debug-to-stderr"),
        "debug_stderr must switch the CLI's own debug output on"
    );
    let stream = start_turn(&mut transport, "fail please").await;
    let _ = collect_until_result(stream, WAIT).await;

    assert!(
        poll_until(WAIT, || seen.lock().expect("sink").len() >= 2).await,
        "both stderr lines must reach the callback, got {:?}",
        seen.lock().expect("sink")
    );
    let callback_lines = seen.lock().expect("sink").join("\n");
    assert!(callback_lines.contains("quota exceeded"));
    assert!(
        callback_lines.contains("Error in hook callback"),
        "the callback is upstream of the noise filter: {callback_lines}"
    );

    let tapped = String::from_utf8_lossy(&tapped.lock().expect("tap").clone()).into_owned();
    assert!(
        tapped.contains("quota exceeded") && tapped.contains("Error in hook callback"),
        "debug_stderr gets the raw lines too, got {tapped:?}"
    );
    transport.disconnect().await.unwrap();
}

/// What reaches consumers as `System`/`error` is the *filtered* buffer: an
/// `AbortError` from the minified CLI bundle and a multi-kilobyte line of
/// bundled JS are noise, not diagnostics. Everything else is kept and, for the
/// known failure shapes, logged with a dedicated explanation.
#[tokio::test]
async fn stderr_noise_is_dropped_while_the_known_failure_shapes_are_kept() {
    capture_logs();
    let minified = format!(
        "var x=Symbol.for(\"react.memo_cache_sentinel\");{}",
        "a".repeat(600)
    );
    // The second marker of the same filter: the bundled entrypoint's own path.
    let bundled = format!(
        "{}//# sourceURL=/$bunfs/root/src/entrypoints/cli.js",
        "b".repeat(600)
    );
    let fake = Transcript::new()
        .await_stdin()
        .stderr("AbortError: The operation was aborted")
        .stderr(minified.clone())
        .stderr(bundled)
        .comment("a blank stderr line is skipped before any filter runs")
        .stderr("")
        .stderr("sh: claude: command not found")
        .stderr("spawn ENOENT")
        .stderr("Unauthorized: invalid API key")
        .stderr("model claude-nope-1 is not available")
        .stderr("error: something went sideways")
        .result_ok("done")
        .exit_with(0)
        .build();

    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let stream = start_turn(&mut transport, "fail loudly").await;
    let messages = collect_n(stream, 4, WAIT).await;
    transport.disconnect().await.unwrap();

    let details = messages
        .iter()
        .find_map(|m| match m {
            Message::System { subtype, data } if subtype == "error" => {
                Some(data["details"].as_str().unwrap_or_default().to_string())
            },
            _ => None,
        })
        .expect("the actionable stderr must surface as System/error");

    for kept in [
        "command not found",
        "spawn ENOENT",
        "Unauthorized",
        "is not available",
        "something went sideways",
    ] {
        assert!(details.contains(kept), "{kept:?} must be kept: {details}");
    }
    assert!(
        !details.contains("AbortError"),
        "a short AbortError is interrupt noise: {details}"
    );
    assert!(
        !details.contains("react.memo_cache_sentinel"),
        "a 600-byte line of bundled JS carries no diagnostic value: {details}"
    );
    assert!(
        !details.contains("bunfs"),
        "the bundled entrypoint's own path is filtered on the same rule: {details}"
    );
    assert!(
        !details.lines().any(|line| line.trim().is_empty()),
        "a blank stderr line never reaches the buffer at all: {details:?}"
    );
}

// ===========================================================================
// 7. stdin — the write path and what happens once the child is gone
// ===========================================================================

/// The stdin writer task owns the pipe. When the child is gone the write fails,
/// the task gives up and drops its receiver — so a later `send_message` is
/// refused with `ChannelSendError` instead of silently vanishing into a channel
/// nobody reads.
#[tokio::test]
async fn sending_to_a_dead_child_eventually_fails_instead_of_vanishing() {
    capture_logs();
    let fake = Transcript::new().exit_with(0).build();
    let mut transport = fake.transport();
    transport.connect().await.unwrap();

    let mut last = None;
    for _ in 0..100 {
        match transport.send_message(user("anybody there?")).await {
            Ok(()) => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(e) => {
                last = Some(e);
                break;
            },
        }
    }
    let err = last.expect("writing to a dead child must eventually be reported");
    assert!(
        matches!(err, SdkError::ChannelSendError),
        "the stdin task is gone, so the channel is closed: got {err:?}"
    );
    transport.disconnect().await.unwrap();
}

/// `end_input` closes stdin for good. A later send must be refused with an error
/// that names the cause — "stdin channel not available" told the caller nothing
/// about *why* it was unavailable.
#[tokio::test]
async fn after_end_input_both_send_paths_name_end_input_in_the_refusal() {
    capture_logs();
    let fake = Transcript::new().await_stdin().wait_eof().build();
    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    transport.send_message(user("first")).await.unwrap();
    transport.end_input().await.unwrap();

    for err in [
        transport
            .send_message(user("too late"))
            .await
            .expect_err("input is closed"),
        transport
            .send_control_request(ControlRequest::Interrupt {
                request_id: "late".into(),
            })
            .await
            .expect_err("input is closed"),
    ] {
        match err {
            SdkError::InvalidState { message } => assert!(
                message.contains("end_input"),
                "the refusal must name the cause, got {message:?}"
            ),
            other => panic!("expected InvalidState, got {other:?}"),
        }
    }
    transport.disconnect().await.unwrap();
}

/// BUG (`SubprocessTransport::end_input`): closing the input stream leaves
/// `state == Connected`, so `is_connected()` keeps claiming a two-way session.
/// Expressing "connected for reading, closed for writing" needs a
/// `TransportState` variant, and that enum lives in `transport/mod.rs` — outside
/// this file's perimeter — so the fix is reported rather than taken alone.
#[tokio::test]
#[ignore = "needs a TransportState::InputClosed variant in transport/mod.rs (another owner)"]
async fn end_input_stops_the_transport_from_claiming_a_two_way_session() {
    capture_logs();
    let fake = Transcript::new().await_stdin().wait_eof().build();
    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    transport.send_message(user("first")).await.unwrap();
    transport.end_input().await.unwrap();
    assert!(
        !transport.is_connected(),
        "input is closed: this is no longer a session a caller can drive"
    );
    transport.disconnect().await.unwrap();
}

/// The SDK control pair used to skip the state check `send_message` applies, so
/// calling them before `connect` reported the internal "stdin channel not
/// available" instead of plainly refusing a disconnected transport.
#[tokio::test]
async fn the_sdk_control_pair_is_gated_on_the_connected_state() {
    capture_logs();
    let fake = default_session();
    let mut transport = fake.transport();

    for err in [
        transport
            .send_sdk_control_request(json!({"type": "control_request"}))
            .await
            .expect_err("not connected yet"),
        transport
            .send_sdk_control_response(json!({"subtype": "success"}))
            .await
            .expect_err("not connected yet"),
    ] {
        match err {
            SdkError::InvalidState { message } => {
                assert_eq!(message, "Not connected", "same refusal as send_message")
            },
            other => panic!("expected InvalidState, got {other:?}"),
        }
    }
}

/// After `end_input` the SDK control pair is refused the same way as
/// `send_message`: the state is still `Connected` but the pipe is gone.
#[tokio::test]
async fn the_sdk_control_pair_is_refused_once_input_is_closed() {
    capture_logs();
    let fake = Transcript::new().await_stdin().wait_eof().build();
    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    transport.send_message(user("first")).await.unwrap();
    transport.end_input().await.unwrap();

    for err in [
        transport
            .send_sdk_control_request(json!({"type": "control_request"}))
            .await
            .expect_err("input is closed"),
        transport
            .send_sdk_control_response(json!({"subtype": "success"}))
            .await
            .expect_err("input is closed"),
    ] {
        match err {
            SdkError::InvalidState { message } => {
                assert!(message.contains("end_input"), "got {message:?}")
            },
            other => panic!("expected InvalidState, got {other:?}"),
        }
    }
    transport.disconnect().await.unwrap();
}

/// `set_close_stdin_after_prompt` is a public setter for a field nothing reads.
/// This pins that: with it on, stdin stays wide open and a second prompt still
/// reaches the CLI.
#[tokio::test]
async fn set_close_stdin_after_prompt_changes_nothing_on_the_wire() {
    capture_logs();
    let fake = Transcript::new()
        .await_stdin_lines(2)
        .assistant_text("got both")
        .result_ok("got both")
        .build();

    let mut transport = fake.transport();
    transport.set_close_stdin_after_prompt(true);
    transport.connect().await.unwrap();
    let stream = transport.receive_messages();
    transport.send_message(user("un")).await.unwrap();
    transport
        .send_message(user("deux"))
        .await
        .expect("stdin is NOT closed after the first prompt: the flag is inert");

    let messages = collect_until_result(stream, WAIT).await;
    transport.disconnect().await.unwrap();
    assert_eq!(assistant_texts(&messages), vec!["got both".to_string()]);
    assert_eq!(fake.stdin_lines().len(), 2);
}

// ===========================================================================
// 8. Shutdown escalation
// ===========================================================================

/// `disconnect` escalates SIGINT -> 200 ms -> SIGTERM -> 500 ms -> SIGKILL, and
/// aims every signal at the process *group* so the CLI's own children go with
/// it. Whichever rung it ends on, two things must hold: it returns well inside
/// the documented 700 ms budget, and nothing is left running.
///
/// The child is stopped first, which is as uncooperative as a test can make the
/// fake. That does **not** force the later rungs everywhere: Darwin resumes a
/// stopped process to deliver a fatal signal, so SIGINT already finishes it
/// there. Reaching SIGTERM/SIGKILL needs a child that *installs* a SIGINT
/// handler, which `fake_claude` deliberately does not — see the report.
#[cfg(unix)]
#[tokio::test]
async fn disconnect_reaps_even_an_uncooperative_child() {
    capture_logs();
    let fake = Transcript::new().wait_eof().build();
    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    let pid = transport.child_pid().expect("pid");

    // SIGSTOP cannot be caught or ignored: the child will not run a line of its
    // own code between here and its death.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGSTOP) }, 0);

    let started = std::time::Instant::now();
    transport.disconnect().await.unwrap();
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "the escalation ladder is bounded by 700 ms plus slack, took {elapsed:?}"
    );
    assert!(!transport.is_connected());
    assert!(
        transport.child_pid().is_none(),
        "disconnect must drop the child handle"
    );
    assert!(
        poll_until(Duration::from_secs(2), || unsafe {
            libc::kill(pid as i32, 0) != 0
        })
        .await,
        "child {pid} outlived disconnect — a leaked process"
    );
}

// ===========================================================================
// 9. `options.user`
// ===========================================================================

/// `options.user` is validated before anything is spawned: a blank value is a
/// configuration error, not a process that silently runs as the wrong user.
#[tokio::test]
async fn a_blank_options_user_is_refused_before_the_child_is_spawned() {
    capture_logs();
    let fake = Transcript::new().exit_with(0).build();
    let options = ClaudeCodeOptions::builder().user("   ").build();
    let mut transport = fake.transport_with(options);

    let err = transport.connect().await.expect_err("blank user");
    match err {
        SdkError::ConfigError(message) => assert!(message.contains("non-empty"), "got {message:?}"),
        other => panic!("expected ConfigError, got {other:?}"),
    }
    assert!(!transport.is_connected());
    assert!(
        transport.child_pid().is_none(),
        "nothing was spawned, so there is no child"
    );
    // And the fake never ran: no invocation record was written.
    assert!(
        !poll_until(Duration::from_millis(300), || fake
            .dir()
            .join("invocation.json")
            .exists())
        .await,
        "the CLI must not be spawned when options.user is invalid"
    );
}

/// An unknown user is reported by name instead of surfacing as an opaque spawn
/// failure.
#[tokio::test]
async fn an_unknown_options_user_is_reported_by_name() {
    capture_logs();
    let fake = Transcript::new().exit_with(0).build();
    let options = ClaudeCodeOptions::builder()
        .user("definitely-not-a-user-on-this-host")
        .build();
    let mut transport = fake.transport_with(options);

    let err = transport.connect().await.expect_err("unknown user");
    let message = err.to_string();
    #[cfg(unix)]
    assert!(
        message.contains("definitely-not-a-user-on-this-host"),
        "got {message}"
    );
    #[cfg(not(unix))]
    assert!(message.contains("only supported on Unix"), "got {message}");
}

// ===========================================================================
// 10. Reported, not fixed
// ===========================================================================

/// BUG (`SubprocessTransport::check_cli_version` / `connect`): the version check
/// returns a `Result<()>` that can never be `Err`, so `connect`'s
/// `if let Err(e) = ...` arm is dead code and an outdated CLI is only a `warn!`.
/// This is exactly where a stale CLI could be diagnosed instead of producing an
/// opaque HTTP 400 ("does not support this model") mid-session. Making `connect`
/// refuse changes its public contract, so it is reported rather than taken here.
#[tokio::test]
#[ignore = "known: an outdated CLI is advisory only; making connect refuse is a public contract change"]
async fn connect_refuses_a_cli_below_the_minimum_version() {
    capture_logs();
    let fake = Transcript::new()
        .await_stdin()
        .result_ok("ok")
        .build_with_version("2.0.1 (Claude Code)");
    let mut transport = fake.transport();
    let err = transport
        .connect()
        .await
        .expect_err("a CLI below MIN_CLI_VERSION must be refused at connect time");
    assert!(
        err.to_string().contains("2.0.1"),
        "the refusal must name the version it found, got {err}"
    );
}

/// BUG (`SubprocessTransport::receive_control_response`): every
/// `control_response` carrying a `request_id` is turned into a
/// `ControlResponse::InterruptAck`, with no correlation against a request the
/// SDK actually sent — `send_control_request` bumps `request_counter` and never
/// reads it. So the answer to `initialize` or `set_model` arrives as an
/// interrupt acknowledgement. Fixing it means new `ControlResponse` variants,
/// and that enum lives in `types.rs` — outside this file's perimeter.
#[tokio::test]
#[ignore = "known: needs ControlResponse variants in types.rs (another owner)"]
async fn a_set_model_response_is_not_an_interrupt_acknowledgement() {
    capture_logs();
    let fake = Transcript::new()
        .reply_control_success("set_model")
        .wait_eof()
        .build();
    let mut transport = fake.transport();
    transport.connect().await.unwrap();
    transport
        .send_sdk_control_request(json!({
            "type": "control_request",
            "request_id": "sm-1",
            "request": {"subtype": "set_model", "model": "fake"},
        }))
        .await
        .unwrap();

    let ack = tokio::time::timeout(
        Duration::from_millis(800),
        transport.receive_control_response(),
    )
    .await;
    assert!(
        ack.is_err() || matches!(ack, Ok(Ok(None))),
        "no interrupt was ever requested, so the legacy channel must stay silent"
    );
    transport.disconnect().await.unwrap();
}
