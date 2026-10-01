//! `ClaudeSDKClient` (`src/client.rs`) driven end to end against the
//! `fake_claude` test double: a real `SubprocessTransport`, a real child process,
//! real pipes and real line framing — no network, no `claude` install, no shell
//! script, so it runs on windows-latest too.
//!
//! The error-injection half of this file's job (a transport that refuses to
//! connect, a control channel that hangs, a panicking transport) lives in the
//! inline `#[cfg(test)] mod tests` of `src/client.rs`, which can swap in a
//! scripted `Transport`. What only a real subprocess can prove is here: the wire
//! shape the client writes, the control-protocol handshake, and what
//! `ClaudeSDKClient::new` does with `ClaudeCodeOptions`.
//!
//! The one ordering rule: `receive_messages()` ends up on a tokio broadcast,
//! which never replays. Subscribe before the CLI prints anything — start the
//! transcript with `await_stdin()` and send the prompt afterwards.

mod support;

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use futures::stream::{Stream, StreamExt};
use nexus_claude::token_tracker::{BudgetLimit, BudgetWarningCallback};
use nexus_claude::{ClaudeCodeOptions, ClaudeSDKClient, Message, Result as SdkResult, SdkError};
use serde_json::{Value, json};
use support::*;

/// Generous enough for a process spawn on a loaded CI runner, short enough that a
/// genuinely stuck test fails instead of eating the job's time budget.
const WAIT: Duration = Duration::from_secs(5);

/// Pull messages until the terminal `result` (inclusive), the stream ends, or
/// `within` elapses.
async fn until_result(
    stream: impl Stream<Item = SdkResult<Message>>,
    within: Duration,
) -> Vec<Message> {
    let mut stream = Box::pin(stream);
    let mut out = Vec::new();
    let _ = tokio::time::timeout(within, async {
        while let Some(item) = stream.next().await {
            match item {
                Ok(message) => {
                    let done = matches!(message, Message::Result { .. });
                    out.push(message);
                    if done {
                        break;
                    }
                },
                Err(_) => break,
            }
        }
    })
    .await;
    out
}

/// Pull exactly `n` messages, or fewer if `within` elapses first.
async fn exactly(
    stream: impl Stream<Item = SdkResult<Message>>,
    n: usize,
    within: Duration,
) -> Vec<Message> {
    let mut stream = Box::pin(stream);
    let mut out = Vec::new();
    let _ = tokio::time::timeout(within, async {
        while out.len() < n {
            match stream.next().await {
                Some(Ok(message)) => out.push(message),
                Some(Err(_)) | None => break,
            }
        }
    })
    .await;
    out
}

/// The `session_id` of the single `system`/`init` message in a collected turn.
fn init_session_id(messages: &[Message]) -> Option<String> {
    let inits: Vec<&Message> = messages
        .iter()
        .filter(|m| matches!(m, Message::System { subtype, .. } if subtype == "init"))
        .collect();
    assert!(
        inits.len() <= 1,
        "the handshake must be delivered at most once, got {} copies",
        inits.len()
    );
    match inits.first() {
        Some(Message::System { data, .. }) => {
            Some(data["session_id"].as_str().unwrap_or_default().to_string())
        },
        _ => None,
    }
}

/// Poll until `condition` holds, or `within` elapses.
async fn wait_until(within: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if condition() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The `request.subtype` of every control request the client wrote to the CLI.
fn control_subtypes(lines: &[Value]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| l["type"] == "control_request")
        .map(|l| {
            l["request"]["subtype"]
                .as_str()
                .or_else(|| l["request"]["type"].as_str())
                .unwrap_or("?")
                .to_string()
        })
        .collect()
}

// ===========================================================================
// 1. The nominal turn, through a real child process
// ===========================================================================

/// `new()` -> `connect()` -> subscribe -> send -> stream -> `disconnect()`, with
/// the wire shape, the session registry and the token accounting all checked
/// against what the real CLI protocol carries.
#[tokio::test]
async fn a_whole_turn_against_the_real_cli_protocol() {
    let fake = Transcript::new()
        .comment("wait for the prompt so nothing is printed before we subscribe")
        .await_stdin()
        .init("sess-turn")
        .assistant_text("bonjour")
        .result_ok("bonjour")
        .comment("stay alive so disconnect() has a live child to shut down")
        .wait_eof()
        .build();

    let mut client = ClaudeSDKClient::new(fake.options());
    client.connect(None).await.expect("connect to fake_claude");
    assert!(client.is_connected().await);
    assert!(
        client.get_sessions().await.is_empty(),
        "connect(None) sends nothing, so no session exists yet"
    );

    // Subscribe first, then send: the only ordering that cannot lose output.
    let stream = client.receive_messages().await;
    client
        .send_user_message("salut".to_string())
        .await
        .expect("send the prompt");
    let messages = until_result(stream, WAIT).await;

    assert_eq!(
        messages.len(),
        3,
        "init + assistant + result, got {messages:?}"
    );
    assert_eq!(init_session_id(&messages).as_deref(), Some("sess-turn"));
    assert_eq!(assistant_texts(&messages), vec!["bonjour".to_string()]);
    assert!(matches!(
        messages.last(),
        Some(Message::Result {
            is_error: false,
            ..
        })
    ));
    assert_eq!(client.get_sessions().await, vec!["default".to_string()]);

    // The prompt really travelled down the child's stdin, in the shape
    // `InputMessage::user` produces, on the hard-coded "default" session.
    let sent = fake.wait_for_stdin_lines(1, WAIT).await;
    let sent: Value = serde_json::from_str(&sent[0]).expect("stdin line is JSON");
    assert_eq!(sent["type"], "user");
    assert_eq!(sent["message"]["content"], "salut");
    assert_eq!(sent["session_id"], "default");

    // `new()` advertises the SDK to the CLI through the environment, as a side
    // effect of construction.
    let invocation = fake.invocation();
    assert_eq!(
        invocation.env("CLAUDE_CODE_ENTRYPOINT").as_deref(),
        Some("sdk-rust"),
        "ClaudeSDKClient::new sets CLAUDE_CODE_ENTRYPOINT for the child"
    );

    // The `usage` object of the real `result` message feeds the budget manager.
    let usage = client.get_usage_stats().await;
    assert_eq!(usage.total_input_tokens, 3);
    assert_eq!(usage.total_output_tokens, 5);
    assert_eq!(usage.session_count, 1);
    assert!((usage.total_cost_usd - 0.0001).abs() < 1e-9);

    client.disconnect().await.expect("disconnect");
    assert!(!client.is_connected().await);
    assert!(
        client.get_sessions().await.is_empty(),
        "disconnect clears the session registry"
    );
}

/// With nobody subscribed, the receiver task buffers the whole turn and keeps the
/// handshake for `get_server_info()`. Subscribing afterwards replays the buffer
/// — and empties it, so the handshake is then lost for good.
#[tokio::test]
async fn an_unobserved_turn_is_buffered_then_replayed_once() {
    let fake = Transcript::new()
        .await_stdin()
        .init("sess-buffered")
        .assistant_text("bonjour")
        .result_ok("bonjour")
        .wait_eof()
        .build();

    let mut client = ClaudeSDKClient::new(fake.options());
    client
        .connect(Some("salut".to_string()))
        .await
        .expect("connect with an initial prompt");

    // The initial prompt went out from inside connect().
    let sent = fake.wait_for_stdin_lines(1, WAIT).await;
    let sent: Value = serde_json::from_str(&sent[0]).expect("stdin line is JSON");
    assert_eq!(sent["message"]["content"], "salut");

    let mut info = None;
    for _ in 0..200u32 {
        info = client.get_server_info().await;
        if info.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let info = info.expect("the handshake must be buffered for get_server_info()");
    assert_eq!(info["session_id"], "sess-buffered");
    assert_eq!(info["model"], "fake-claude");

    // Order between a replayed and a live message is not guaranteed, so assert on
    // the contents rather than the sequence.
    let messages = exactly(client.receive_messages().await, 3, WAIT).await;
    assert_eq!(messages.len(), 3, "got {messages:?}");
    assert_eq!(init_session_id(&messages).as_deref(), Some("sess-buffered"));
    assert_eq!(assistant_texts(&messages), vec!["bonjour".to_string()]);
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, Message::Result { .. }))
            .count(),
        1
    );

    assert!(
        client.get_server_info().await.is_none(),
        "receive_messages() drained the buffer, so the handshake is gone"
    );
    client.disconnect().await.expect("disconnect");
}

// ===========================================================================
// 2. The legacy interrupt channel (no control protocol)
// ===========================================================================

/// Without a query handler, `interrupt()` writes the nested legacy envelope and
/// blocks on the CLI's `control_response`, correlated by the `interrupt_N` id the
/// client generates itself.
#[tokio::test]
async fn interrupt_is_acknowledged_over_the_legacy_control_channel() {
    let fake = Transcript::new()
        .reply_control_success("interrupt")
        .reply_control_success("interrupt")
        .wait_eof()
        .build();

    let mut client = ClaudeSDKClient::new(fake.options());
    client.connect(None).await.expect("connect to fake_claude");

    client.interrupt().await.expect("first interrupt");
    client.interrupt().await.expect("second interrupt");

    let lines = fake.wait_for_stdin_lines(2, WAIT).await;
    let first: Value = serde_json::from_str(&lines[0]).expect("JSON");
    assert_eq!(first["type"], "control_request");
    assert_eq!(
        first["request"]["type"], "interrupt",
        "the legacy envelope nests the kind under `request.type`"
    );
    assert_eq!(first["request"]["request_id"], "interrupt_1");
    let second: Value = serde_json::from_str(&lines[1]).expect("JSON");
    assert_eq!(second["request"]["request_id"], "interrupt_2");

    client.disconnect().await.expect("disconnect");
}

// ===========================================================================
// 3. The control protocol
// ===========================================================================

/// `enable_file_checkpointing` switches the control protocol on, which makes
/// `connect()` perform an `initialize` handshake and unlocks the four requests
/// that go through the query handler.
#[tokio::test]
async fn the_handshake_unlocks_the_control_protocol_requests() {
    let fake = Transcript::new()
        .reply_control_with(
            "initialize",
            json!({"subtype": "success", "response": {"commands": ["/clear"]}}),
        )
        .reply_control_success("set_permission_mode")
        .reply_control_success("set_model")
        .reply_control_success("set_model")
        .reply_control_success("rewind_files")
        .reply_control_success("interrupt")
        .wait_eof()
        .build();

    let options = fake.options_with(
        ClaudeCodeOptions::builder()
            .enable_file_checkpointing(true)
            .build(),
    );
    let mut client = ClaudeSDKClient::new(options);
    client.connect(None).await.expect("handshake succeeds");

    // get_server_info() now answers from the handshake result, not the buffer.
    let info = client
        .get_server_info()
        .await
        .expect("the initialize response is kept");
    assert_eq!(
        info["commands"][0], "/clear",
        "the payload under `response` is what callers get"
    );

    client
        .set_permission_mode("plan")
        .await
        .expect("set_permission_mode");
    client
        .set_model(Some("claude-opus-5".to_string()))
        .await
        .expect("set_model");
    client.set_model(None).await.expect("set_model(None)");
    client.rewind_files("uuid-42").await.expect("rewind_files");
    client.interrupt().await.expect("interrupt via the handler");

    let lines = fake.wait_for_stdin_lines(6, WAIT).await;
    let lines: Vec<Value> = lines
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert_eq!(
        control_subtypes(&lines),
        vec![
            "initialize".to_string(),
            "set_permission_mode".to_string(),
            "set_model".to_string(),
            "set_model".to_string(),
            "rewind_files".to_string(),
            "interrupt".to_string(),
        ],
        "every call must reach the CLI as its own control request, in order"
    );
    assert_eq!(lines[1]["request"]["mode"], "plan");
    assert_eq!(lines[2]["request"]["model"], "claude-opus-5");
    assert_eq!(
        lines[3]["request"]["model"],
        Value::Null,
        "set_model(None) must be sent as an explicit null, not omitted"
    );
    assert_eq!(lines[4]["request"]["user_message_id"], "uuid-42");
    for line in &lines {
        assert!(
            line["request_id"].is_string(),
            "the control protocol carries the id at the top level: {line}"
        );
    }

    client.disconnect().await.expect("disconnect");
}

/// A `subtype: "error"` control response becomes a `ControlRequestError` carrying
/// the CLI's own message, instead of being silently treated as a success.
#[tokio::test]
async fn a_refused_control_request_surfaces_the_cli_error() {
    let fake = Transcript::new()
        .reply_control_success("initialize")
        .reply_control_error("set_permission_mode", "mode inconnu")
        .wait_eof()
        .build();

    let options = fake.options_with(
        ClaudeCodeOptions::builder()
            .enable_file_checkpointing(true)
            .build(),
    );
    let mut client = ClaudeSDKClient::new(options);
    client.connect(None).await.expect("handshake succeeds");

    let error = client
        .set_permission_mode("galactique")
        .await
        .expect_err("the CLI refused the mode");
    assert!(
        matches!(error, SdkError::ControlRequestError(ref m) if m == "mode inconnu"),
        "got {error:?}"
    );

    client.disconnect().await.expect("disconnect");
}

/// `rewind_files` is gated on `enable_file_checkpointing` before the control
/// protocol is ever reached, so a client whose protocol is on for another reason
/// still refuses it — and nothing is written to the CLI.
#[tokio::test]
async fn rewind_files_is_refused_without_file_checkpointing() {
    let fake = Transcript::new()
        .reply_control_success("initialize")
        .wait_eof()
        .build();

    let mut hooks = std::collections::HashMap::new();
    hooks.insert(
        "PreToolUse".to_string(),
        vec![nexus_claude::HookMatcher {
            matcher: None,
            hooks: vec![],
        }],
    );
    let options = fake.options_with(ClaudeCodeOptions::builder().hooks(hooks).build());
    let mut client = ClaudeSDKClient::new(options);
    client.connect(None).await.expect("handshake succeeds");

    let error = client
        .rewind_files("uuid-42")
        .await
        .expect_err("checkpointing is off");
    assert!(
        matches!(error, SdkError::InvalidState { ref message }
            if message.starts_with("File checkpointing is not enabled.")),
        "got {error:?}"
    );

    let lines = fake.wait_for_stdin_lines(1, WAIT).await;
    assert_eq!(
        control_subtypes(
            &lines
                .iter()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect::<Vec<Value>>()
        ),
        vec!["initialize".to_string()],
        "the refusal happens before anything is sent"
    );

    client.disconnect().await.expect("disconnect");
}

// ===========================================================================
// 4. Budget accounting across a real turn
// ===========================================================================

/// The cap is armed before the turn, so the `usage` the CLI reports crosses it and
/// the caller's callback is invoked exactly once.
#[tokio::test]
async fn a_real_turn_can_trip_the_budget_warning_callback() {
    let fake = Transcript::new()
        .await_stdin()
        .assistant_text("bonjour")
        .result_ok("bonjour")
        .wait_eof()
        .build();

    let mut client = ClaudeSDKClient::new(fake.options());
    let seen: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let sink = seen.clone();
    let callback: BudgetWarningCallback = Arc::new(move |message: &str| {
        sink.lock().expect("sink lock").push(message.to_string());
    });
    client
        .set_budget_limit(BudgetLimit::with_tokens(4), Some(callback))
        .await;

    client.connect(None).await.expect("connect to fake_claude");
    let stream = client.receive_messages().await;
    client
        .send_user_message("salut".to_string())
        .await
        .expect("send the prompt");
    let messages = until_result(stream, WAIT).await;
    assert!(matches!(messages.last(), Some(Message::Result { .. })));

    assert!(
        wait_until(WAIT, || !seen.lock().expect("sink lock").is_empty()).await,
        "3 + 5 tokens against a 4-token cap must warn"
    );
    assert_eq!(
        seen.lock().expect("sink lock").as_slice(),
        ["Budget limit exceeded".to_string()]
    );
    assert!(client.is_budget_exceeded().await);

    client.clear_budget_limit().await;
    assert!(!client.is_budget_exceeded().await);
    client.disconnect().await.expect("disconnect");
}

// ===========================================================================
// 5. No CLI at all
// ===========================================================================

/// Restores an environment variable when the test ends, pass or fail.
struct EnvGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// `ClaudeSDKClient::new` cannot report "no CLI installed": it swallows the
/// `CliNotFound` error and builds a transport on an empty path, so the failure
/// only shows up at `connect()` — and as a spawn error, not as `CliNotFound`.
///
/// Unix only: `dirs::home_dir()` reads `$HOME` there, while on Windows it asks
/// the shell for a known folder that no environment variable can redirect.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(claude_cli_lookup_env)]
async fn new_hides_a_missing_cli_until_connect_fails() {
    let home = tempfile::tempdir().expect("temp home");
    let _path = EnvGuard::set("PATH", "");
    let _home = EnvGuard::set("HOME", &home.path().display().to_string());

    if let Ok(found) = nexus_claude::transport::subprocess::find_claude_cli() {
        // Some machine-wide install is still visible with PATH and HOME scrubbed
        // (`/usr/local/bin/claude`, `/opt/homebrew/bin/claude`, ...). Going on
        // would spawn the real CLI, which this suite must never do.
        eprintln!(
            "skipped: a Claude CLI is reachable outside PATH and HOME at {}",
            found.display()
        );
        return;
    }

    let mut client = ClaudeSDKClient::new(ClaudeCodeOptions::default());
    assert!(
        !client.is_connected().await,
        "construction cannot have connected anything"
    );

    let error = client
        .connect(None)
        .await
        .expect_err("an empty CLI path cannot be spawned");
    assert!(
        matches!(error, SdkError::ProcessError(_)),
        "the empty fallback path fails as a spawn error, with no trace of \
         CliNotFound: got {error:?}"
    );
    assert!(!client.is_connected().await);
}
