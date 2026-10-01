//! `InteractiveClient` driven against the `fake_claude` test double, i.e. a real
//! `SubprocessTransport` with a real child process, a real stdin pipe and a real
//! broadcast — no network, no `claude` install, no shell script.
//!
//! The in-crate unit tests of `src/interactive.rs` cover the branches with a
//! scripted transport; what cannot be faked is checked here: the pid, the stdin
//! sender, the out-of-band broadcast, and the fact that the client's
//! re-subscribing receive loops only work when the CLI's output is spaced out.

mod support;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use nexus_claude::{
    HookCallback, HookContext, HookInput, HookJSONOutput, HookMatcher, InteractiveClient, Message,
    SdkError, SyncHookJSONOutput,
};
use serde_json::json;
use support::*;
use tokio::sync::Mutex;

/// Generous enough for a process spawn on a loaded CI box, short enough that a
/// stuck test fails instead of hanging.
const WAIT: Duration = Duration::from_secs(10);

/// The gap the fake leaves before its first line, so that a caller which
/// subscribes only *after* writing its prompt cannot miss it. This is a
/// property of the `send_message` + `receive_response` split, not of the
/// receive loop: `send_and_receive` subscribes before it writes.
const SPACING_MS: u64 = 300;

/// A client over a real `SubprocessTransport` pointed at the fake.
fn client_for(fake: &FakeCli) -> InteractiveClient {
    InteractiveClient::from_transport(Box::new(fake.transport()))
}

/// Drain a turn stream up to and including its `result` message.
///
/// `send_and_receive_stream` borrows the client, so its stream is not `'static`
/// and cannot go through `support::collect_until_result`.
async fn collect_turn(
    stream: impl futures::Stream<Item = nexus_claude::Result<Message>>,
) -> Vec<Message> {
    let mut stream = std::pin::pin!(stream);
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let message = item.expect("no transport error");
        let done = matches!(message, Message::Result { .. });
        out.push(message);
        if done {
            break;
        }
    }
    out
}

#[tokio::test]
async fn pid_stdin_sender_and_broadcast_appear_only_while_the_subprocess_runs() {
    let fake = Transcript::new()
        .await_stdin()
        .init("sess-passthrough")
        .result_ok("done")
        .wait_eof()
        .build();
    let mut client = client_for(&fake);

    // Before connect() there is no child, no pipe and no broadcast.
    assert!(client.child_pid().await.is_none());
    assert!(client.clone_stdin_sender().await.is_none());
    assert!(client.subscribe_messages().await.is_none());

    client.connect().await.unwrap();

    let pid = client.child_pid().await.expect("a spawned child has a pid");
    assert!(pid > 0, "pid should be a real process id, got {pid}");
    assert!(
        client.clone_stdin_sender().await.is_some(),
        "a connected subprocess transport exposes its stdin writer"
    );
    let oob = client
        .subscribe_messages()
        .await
        .expect("a connected subprocess transport exposes its broadcast");

    // The out-of-band stream sees the session messages, and it survives other
    // operations on the client because it does not borrow the transport lock.
    let turn = client
        .send_and_receive_stream("salut".to_string())
        .await
        .unwrap();
    let collected = tokio::time::timeout(WAIT, collect_turn(turn))
        .await
        .expect("the turn completes");
    assert!(
        matches!(collected.last(), Some(Message::Result { .. })),
        "the turn ends on the result message, got {collected:?}"
    );
    let out_of_band = collect_until_result(oob, WAIT).await;
    assert_eq!(
        out_of_band.len(),
        collected.len(),
        "the out-of-band subscriber saw the same turn: {out_of_band:?}"
    );

    client.disconnect().await.unwrap();
    assert!(
        client.child_pid().await.is_none(),
        "disconnect reaps the child, so there is no pid left to signal"
    );
    assert!(
        client.clone_stdin_sender().await.is_none(),
        "disconnect closes stdin, which is what signals EOF to the CLI"
    );
}

#[tokio::test]
async fn send_and_receive_stream_drives_a_whole_turn_without_losing_a_message() {
    let fake = Transcript::new()
        .await_stdin()
        .init("sess-stream")
        .assistant_text("bonjour")
        .result_ok("bonjour")
        .wait_eof()
        .build();
    let mut client = client_for(&fake);
    client.connect().await.unwrap();

    // This is the race-free entry point: it subscribes and sends under the same
    // lock, so the fake cannot print before the subscription exists.
    let turn = client
        .send_and_receive_stream("salut".to_string())
        .await
        .unwrap();
    let messages = tokio::time::timeout(WAIT, collect_turn(turn))
        .await
        .expect("the turn completes");

    assert_eq!(
        messages.len(),
        3,
        "init + assistant + result, got {messages:?}"
    );
    match &messages[0] {
        Message::System { subtype, data } => {
            assert_eq!(subtype, "init");
            assert_eq!(data["session_id"], "sess-stream");
        },
        other => panic!("expected the init system message first, got {other:?}"),
    }
    assert_eq!(assistant_texts(&messages), vec!["bonjour".to_string()]);

    // The prompt really went down the child's stdin, in `InputMessage::user` shape.
    let lines = fake.wait_for_stdin_lines(1, WAIT).await;
    let sent: serde_json::Value = serde_json::from_str(&lines[0]).expect("JSON on stdin");
    assert_eq!(sent["type"], "user");
    assert_eq!(sent["message"]["content"], "salut");
    assert_eq!(
        sent["session_id"], "default",
        "the client labels every turn `default`, whatever the CLI's session id is"
    );

    client.disconnect().await.unwrap();
}

#[tokio::test]
async fn send_and_receive_drives_a_real_cli_that_prints_its_turn_in_one_burst() {
    // `send_and_receive` subscribes and writes under the same lock, then keeps
    // that one subscription for the whole turn. The fake therefore prints its
    // three lines back to back, with no sleep anywhere: the version that
    // re-subscribed between every message lost the ones printed while it was
    // unsubscribed and never returned.
    let fake = Transcript::new()
        .await_stdin()
        .init("sess-blocking")
        .assistant_text("bonjour")
        .result_ok("bonjour")
        .wait_eof()
        .build();
    let mut client = client_for(&fake);
    client.connect().await.unwrap();

    let messages = tokio::time::timeout(WAIT, client.send_and_receive("salut".to_string()))
        .await
        .expect("a turn printed in one burst must still be collected whole")
        .expect("no transport error");

    assert_eq!(
        messages.len(),
        3,
        "init + assistant + result, got {messages:?}"
    );
    assert_eq!(assistant_texts(&messages), vec!["bonjour".to_string()]);
    assert!(matches!(messages.last(), Some(Message::Result { .. })));

    client.disconnect().await.unwrap();
}

#[tokio::test]
async fn send_message_then_receive_response_splits_the_same_turn_in_two() {
    // Only the first gap is needed: `send_message` returns before
    // `receive_response` subscribes, so the fake must not print until then.
    // Once subscribed, the rest of the turn may arrive in one burst.
    let fake = Transcript::new()
        .await_stdin()
        .sleep_ms(SPACING_MS)
        .init("sess-two-steps")
        .result_ok("done")
        .wait_eof()
        .build();
    let mut client = client_for(&fake);
    client.connect().await.unwrap();

    client.send_message("salut".to_string()).await.unwrap();
    let messages = tokio::time::timeout(WAIT, client.receive_response())
        .await
        .expect("receive_response returns on the result message")
        .expect("no transport error");

    assert_eq!(messages.len(), 2, "init + result, got {messages:?}");
    assert!(matches!(messages.last(), Some(Message::Result { .. })));
    assert_eq!(
        fake.stdin_lines().len(),
        1,
        "receive_response writes nothing back to the CLI"
    );

    client.disconnect().await.unwrap();
}

/// A hook callback that records what it was handed.
struct RecordingHook {
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl HookCallback for RecordingHook {
    async fn execute(
        &self,
        input: &HookInput,
        tool_use_id: Option<&str>,
        _context: &HookContext,
    ) -> std::result::Result<HookJSONOutput, SdkError> {
        let event = match input {
            HookInput::PreToolUse(i) => format!("PreToolUse:{}", i.tool_name),
            other => format!("{other:?}"),
        };
        self.seen
            .lock()
            .await
            .push(format!("{event}/{}", tool_use_id.unwrap_or("-")));
        Ok(HookJSONOutput::Sync(SyncHookJSONOutput {
            continue_: Some(true),
            reason: Some("allowed by the test hook".to_string()),
            ..Default::default()
        }))
    }
}

#[tokio::test]
async fn the_hook_round_trip_works_over_the_real_stdin_and_control_channel() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook { seen: seen.clone() }) as Arc<dyn HookCallback>;
    let mut hooks = std::collections::HashMap::new();
    hooks.insert(
        "PreToolUse".to_string(),
        vec![HookMatcher {
            matcher: Some(json!({"tool_name": "Bash"})),
            hooks: vec![hook],
        }],
    );

    // The fake waits for the initialize request, then asks for the hook.
    let fake = Transcript::new()
        .await_stdin_containing("initialize")
        .hook_callback(
            "req-hook-1",
            "PLACEHOLDER",
            json!({
                "hook_event_name": "PreToolUse",
                "session_id": "sess-hooks",
                "transcript_path": "transcript.json",
                "cwd": ".",
                "tool_name": "Bash",
                "tool_input": {"command": "echo hi"}
            }),
        )
        .wait_eof()
        .build();
    let mut client =
        InteractiveClient::from_transport_with_hooks(Box::new(fake.transport()), hooks);
    client.connect().await.unwrap();

    // `initialize_hooks` mints the callback id and tells the CLI about it.
    client.initialize_hooks().await.unwrap();
    let lines = fake.wait_for_stdin_lines(1, WAIT).await;
    let init: serde_json::Value = serde_json::from_str(&lines[0]).expect("JSON on stdin");
    assert_eq!(init["type"], "control_request");
    assert_eq!(init["request"]["subtype"], "initialize");
    let callback_id = init["request"]["hooks"]["PreToolUse"][0]["hookCallbackIds"][0]
        .as_str()
        .expect("one minted callback id")
        .to_string();
    assert_eq!(
        init["request"]["hooks"]["PreToolUse"][0]["matcher"]["tool_name"],
        "Bash"
    );

    // The fake's own hook_callback request really arrives on the control
    // channel — and is ignored, because its callback id was never minted.
    let mut control_rx = client
        .take_sdk_control_receiver()
        .await
        .expect("a connected subprocess transport exposes the control channel");
    let incoming = tokio::time::timeout(WAIT, control_rx.recv())
        .await
        .expect("the request arrives")
        .expect("the channel is open");
    assert!(nexus_claude::is_hook_callback(&incoming));
    assert_eq!(incoming["request"]["callback_id"], "PLACEHOLDER");
    assert!(
        client.dispatch_hook_callback(&incoming).await.is_none(),
        "an unknown callback id is dropped, not dispatched"
    );

    // Replay the same request with the id the client actually published.
    let control_msg = json!({
        "type": "control_request",
        "request_id": "req-hook-1",
        "request": {
            "subtype": "hook_callback",
            "callback_id": callback_id,
            "input": {
                "hook_event_name": "PreToolUse",
                "session_id": "sess-hooks",
                "transcript_path": "transcript.json",
                "cwd": ".",
                "tool_name": "Bash",
                "tool_input": {"command": "echo hi"}
            },
            "tool_use_id": "toolu_42"
        }
    });
    assert!(nexus_claude::is_hook_callback(&control_msg));

    let output = client
        .dispatch_hook_callback(&control_msg)
        .await
        .expect("the callback id is registered")
        .expect("the hook ran");
    assert!(matches!(output, HookJSONOutput::Sync(_)));
    assert_eq!(
        seen.lock().await.as_slice(),
        ["PreToolUse:Bash/toolu_42".to_string()],
        "the hook saw the parsed input and the tool_use_id"
    );

    // And the answer travels back down the real stdin pipe, not through a lock.
    client
        .send_hook_response("req-hook-1", &Ok(output))
        .await
        .unwrap();
    let lines = fake.wait_for_stdin_lines(2, WAIT).await;
    let response: serde_json::Value = serde_json::from_str(&lines[1]).expect("JSON on stdin");
    assert_eq!(response["type"], "control_response");
    assert_eq!(response["response"]["subtype"], "success");
    assert_eq!(response["response"]["request_id"], "req-hook-1");
    assert_eq!(response["response"]["response"]["continue"], true);
    assert_eq!(
        response["response"]["response"]["reason"],
        "allowed by the test hook"
    );

    client.disconnect().await.unwrap();
}

#[tokio::test]
async fn the_two_stream_getters_refuse_before_connect_like_the_other_seven() {
    // All nine turn operations now start with
    // `if !self.connected { return Err(InvalidState) }`. These two used to hand
    // out a stream that was simply over, so a caller who forgot to connect read
    // "the CLI said nothing" instead of an error.
    let fake = Transcript::new().await_stdin().result_ok("done").build();
    let mut client = client_for(&fake);

    let error = client
        .receive_messages_stream()
        .await
        .err()
        .expect("an unconnected client has no messages to stream");
    assert!(
        matches!(&error, SdkError::InvalidState { message } if message == "Not connected"),
        "got {error:?}"
    );

    let error = client
        .receive_response_stream()
        .await
        .err()
        .expect("same thing one layer up");
    assert!(
        matches!(&error, SdkError::InvalidState { message } if message == "Not connected"),
        "got {error:?}"
    );

    // Nothing was spawned and nothing was locked: the client is still usable.
    assert!(client.child_pid().await.is_none());
    client.connect().await.unwrap();
    assert!(
        client.receive_messages_stream().await.is_ok(),
        "and once connected the very same call is accepted"
    );
    client.disconnect().await.unwrap();
}

#[tokio::test]
async fn initialize_hooks_rolls_back_its_callback_ids_when_it_cannot_reach_the_cli() {
    // No `connect()`, so the transport refuses to write. The callback ids are
    // minted and registered *before* the send — a hook_callback fired the instant
    // the CLI reads the init message must find its entry — so a failed send has
    // to take them back out.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::new(RecordingHook { seen }) as Arc<dyn HookCallback>;
    let mut hooks = std::collections::HashMap::new();
    hooks.insert(
        "PreToolUse".to_string(),
        vec![HookMatcher {
            matcher: None,
            hooks: vec![hook],
        }],
    );
    let fake = Transcript::new().await_stdin().build();
    let client = InteractiveClient::from_transport_with_hooks(Box::new(fake.transport()), hooks);

    let error = client.initialize_hooks().await.unwrap_err();
    // The message used to be the transport's internal "Stdin channel not available",
    // which told the caller nothing. `SubprocessTransport::send_sdk_control_request`
    // now applies the same `TransportState` guard as `send_message`, so an
    // un-connected client is refused by name.
    assert!(
        matches!(&error, SdkError::InvalidState { message } if message == "Not connected"),
        "got {error:?}"
    );
    assert!(
        client.hook_callbacks().read().await.is_empty(),
        "an id the CLI will never learn must not stay in the registry"
    );
    // And retrying does not pile up one more leaked id per attempt.
    client.initialize_hooks().await.unwrap_err();
    assert!(client.hook_callbacks().read().await.is_empty());
    assert!(
        fake.stdin_lines().is_empty(),
        "nothing reached the CLI, which was never even spawned"
    );
}
