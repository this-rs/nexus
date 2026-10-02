//! `ClaudeSDKClientWorking` (`src/client_working.rs`) driven end to end against
//! the `fake_claude` test double: a real `SubprocessTransport`, a real child
//! process, real pipes — no network, no `claude` install, no shell script, so it
//! runs on windows-latest too.
//!
//! Everything has to go through a subprocess here: unlike `ClaudeSDKClient`, this
//! client stores a **concrete** `SubprocessTransport` rather than a
//! `Box<dyn Transport>`, so there is no seam for a scripted transport and no way
//! to reach its error paths from an inline unit test.
//!
//! Every test is `#[serial_test::serial]` because `ClaudeSDKClientWorking::new`
//! calls `std::env::set_var` — a process-wide mutation in a constructor, racing
//! every `Command::spawn` in the same binary.
//!
//! What these tests pin down is mostly *not* a happy path. The reader task this
//! client spawns resubscribes to the transport's broadcast once per iteration and
//! polls it exactly once, sleeping ~110 ms in between, while holding the transport
//! mutex across `stream.next().await`. The consequences — lost messages, a
//! `send_user_message` that cannot run while the CLI is quiet, a `disconnect` that
//! blocks on the same lock — are asserted below one by one.

mod support;

use std::time::Duration;

use nexus_claude::{ClaudeCodeOptions, ClaudeSDKClientWorking, Message, SdkError};
use support::*;

/// Generous enough for a process spawn on a loaded CI runner, short enough that a
/// genuinely stuck test fails instead of eating the job's time budget.
const WAIT: Duration = Duration::from_secs(5);

/// Long enough that the reader task has certainly completed one full cycle
/// (poll + 100 ms sleep + state check), short enough to keep the file quick.
const ONE_CYCLE: Duration = Duration::from_millis(400);

/// The name of the environment variable the constructor writes.
const ENTRYPOINT: &str = "CLAUDE_CODE_ENTRYPOINT";

fn invalid_state_message(error: &SdkError) -> String {
    match error {
        SdkError::InvalidState { message } => message.clone(),
        other => panic!("expected SdkError::InvalidState, got {other:?}"),
    }
}

// ===========================================================================
// 1. Construction
// ===========================================================================

/// `new` is documented as "Create a new client" and nothing more, but it mutates
/// the **process** environment, and it does so unconditionally: a caller that had
/// deliberately set `CLAUDE_CODE_ENTRYPOINT` (to attribute usage to its own
/// wrapper, say) silently loses that value the moment any client is constructed.
/// `ClaudeSDKClient::new`, by contrast, leaves the variable alone and lets
/// `build_command` set it per child process.
#[tokio::test]
#[serial_test::serial]
async fn new_clobbers_a_caller_supplied_entrypoint_env_var() {
    // SAFETY: this test is `#[serial]`, so no other test in this binary is
    // reading the environment or spawning a process concurrently.
    unsafe { std::env::set_var(ENTRYPOINT, "my-own-wrapper") };

    let client = ClaudeSDKClientWorking::new(ClaudeCodeOptions::default());

    assert_eq!(
        std::env::var(ENTRYPOINT).ok(),
        Some("sdk-rust".to_string()),
        "the constructor overwrote the caller's value instead of leaving it alone"
    );
    assert!(
        !client.is_connected().await,
        "a freshly constructed client must start Disconnected"
    );
}

/// A client built on a `cli_path` that does not exist constructs fine — the
/// failure is deferred to `connect`, where `SubprocessTransport::connect` cannot
/// spawn. What matters is that the failure is *not* half-applied: the state stays
/// `Disconnected`, so the client does not go on claiming a session, and every
/// write path keeps refusing.
#[tokio::test]
#[serial_test::serial]
async fn connect_fails_on_a_missing_binary_without_claiming_a_session() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let options = ClaudeCodeOptions {
        cli_path: Some(dir.path().join("definitely-not-a-cli")),
        ..Default::default()
    };

    let mut client = ClaudeSDKClientWorking::new(options);
    let error = client
        .connect(Some("hello".to_string()))
        .await
        .expect_err("spawning a path that does not exist must fail");
    assert!(
        matches!(error, SdkError::ProcessError(_)),
        "expected the spawn error to surface verbatim, got {error:?}"
    );

    assert!(!client.is_connected().await);
    let refused = client
        .send_user_message("anyone there?".to_string())
        .await
        .expect_err("a client that never connected must refuse to send");
    assert_eq!(invalid_state_message(&refused), "Not connected");
}

// ===========================================================================
// 2. The wire shape of a prompt
// ===========================================================================

/// The prompt is wrapped as a `user` message — with the session id hard-coded to
/// the literal `"default"`. The CLI has just announced its real session id in the
/// `init` handshake (`sess-real` here) and the client ignores it, so nothing the
/// client writes can ever be correlated with the session the CLI reports.
#[tokio::test]
#[serial_test::serial]
async fn the_prompt_is_sent_with_a_hard_coded_default_session_id() {
    let fake = Transcript::new()
        .await_stdin()
        .init("sess-real")
        .wait_eof_for(400)
        .build();

    let mut client = ClaudeSDKClientWorking::new(fake.options());
    client
        .connect(Some("salut".to_string()))
        .await
        .expect("connect");

    let lines = fake.wait_for_stdin_lines(1, WAIT).await;
    assert_eq!(lines.len(), 1, "exactly the initial prompt: {lines:?}");
    let sent = &fake.stdin_json()[0];
    assert_eq!(sent["type"], "user");
    assert_eq!(sent["message"]["role"], "user");
    assert_eq!(sent["message"]["content"], "salut");
    assert_eq!(
        sent["session_id"], "default",
        "the CLI announced `sess-real`; the client writes the literal `default`"
    );

    client.disconnect().await.expect("disconnect");
}

/// `connect` returns `Ok(())` when already connected — before looking at
/// `initial_prompt`. So the second call is not merely idempotent: it *swallows*
/// its argument. A caller that reconnects-with-a-prompt as a retry gets a
/// success and a prompt that was never written to the CLI.
#[tokio::test]
#[serial_test::serial]
async fn a_second_connect_reports_success_and_silently_drops_its_prompt() {
    let fake = Transcript::new()
        .await_stdin()
        .init("sess-twice")
        .wait_eof_for(600)
        .build();

    let mut client = ClaudeSDKClientWorking::new(fake.options());
    client
        .connect(Some("premier".to_string()))
        .await
        .expect("first connect");
    fake.wait_for_stdin_lines(1, WAIT).await;

    client
        .connect(Some("second".to_string()))
        .await
        .expect("a second connect is reported as a success");

    // Give a prompt that was going to be written every chance to appear.
    tokio::time::sleep(ONE_CYCLE).await;
    let lines = fake.stdin_lines();
    assert_eq!(
        lines.len(),
        1,
        "the second prompt was dropped, not queued: {lines:?}"
    );
    assert!(lines[0].contains("premier"));
    assert!(
        !lines.iter().any(|l| l.contains("second")),
        "the prompt handed to the second connect never reached the CLI: {lines:?}"
    );

    client.disconnect().await.expect("disconnect");
}

// ===========================================================================
// 3. Reading: what the reader task does and does not deliver
// ===========================================================================

/// The happy path, and the only shape in which it works: one message per reader
/// cycle. The transcript spaces its output by 300 ms — more than the ~110 ms the
/// reader spends unsubscribed — so nothing is lost and `receive_response` stops
/// on the terminal `result`.
#[tokio::test]
#[serial_test::serial]
async fn receive_response_collects_a_slow_turn_up_to_the_result() {
    let fake = Transcript::new()
        .await_stdin()
        .sleep_ms(300)
        .init("sess-slow")
        .sleep_ms(300)
        .assistant_text("bonjour")
        .sleep_ms(300)
        .result_ok("bonjour")
        .wait_eof_for(300)
        .build();

    let mut client = ClaudeSDKClientWorking::new(fake.options());
    client
        .connect(Some("salut".to_string()))
        .await
        .expect("connect");

    let messages = tokio::time::timeout(WAIT, client.receive_response())
        .await
        .expect("receive_response must not hang on a correctly spaced turn")
        .expect("receive_response");

    assert_eq!(
        messages.len(),
        3,
        "init + assistant + result, in order: {messages:?}"
    );
    match &messages[0] {
        Message::System { subtype, data } => {
            assert_eq!(subtype, "init");
            assert_eq!(data["session_id"], "sess-slow");
        },
        other => panic!("expected the init handshake first, got {other:?}"),
    }
    assert_eq!(assistant_texts(&messages), vec!["bonjour".to_string()]);
    assert!(
        matches!(messages.last(), Some(Message::Result { .. })),
        "the vec must stop on the result: {messages:?}"
    );

    // The CLI is gone now; let the reader task spin over the finished stream a
    // few times before taking the transport away from it.
    tokio::time::sleep(ONE_CYCLE).await;
    assert!(
        client.is_connected().await,
        "nothing sets the state back when the CLI exits — the client still \
         reports a live session"
    );
    client.disconnect().await.expect("disconnect");
}

/// The central defect. The reader task calls `transport.receive_messages()`
/// **inside** its loop, which is `broadcast::Receiver::resubscribe()`: a fresh
/// subscription that starts at the current tail. It polls that subscription
/// exactly once, drops it, sleeps ~110 ms, and subscribes again. Anything the CLI
/// printed in between was broadcast to nobody and is unrecoverable.
///
/// So a CLI that answers at normal speed — four lines back to back, which is what
/// the real CLI does — is reduced to its first line. Including the `result`: the
/// turn never terminates, and the caller is left waiting for a message that was
/// thrown away.
#[tokio::test]
#[serial_test::serial]
async fn a_burst_of_messages_is_truncated_to_its_first_line() {
    let fake = Transcript::new()
        .await_stdin()
        // Let the reader task subscribe and park before anything is printed, so
        // that losing the rest cannot be blamed on a late first subscription.
        .sleep_ms(400)
        .init("sess-burst")
        .assistant_text("alpha")
        .assistant_text("beta")
        .result_ok("done")
        .wait_eof_for(500)
        .build();

    let mut client = ClaudeSDKClientWorking::new(fake.options());
    client
        .connect(Some("go".to_string()))
        .await
        .expect("connect");

    let first = tokio::time::timeout(WAIT, client.receive_message())
        .await
        .expect("the first message of the burst must arrive")
        .expect("receive_message")
        .expect("the stream is open");
    match &first {
        Message::System { subtype, data } => {
            assert_eq!(subtype, "init");
            assert_eq!(data["session_id"], "sess-burst");
        },
        other => panic!("expected the init handshake, got {other:?}"),
    }

    // `alpha`, `beta` and the terminal `result` were printed microseconds after
    // `init`, i.e. while the reader held no subscription. They are gone.
    let starved = tokio::time::timeout(Duration::from_millis(1200), client.receive_message()).await;
    assert!(
        starved.is_err(),
        "the three remaining lines of the burst must be unrecoverable, got {starved:?}"
    );

    client.disconnect().await.expect("disconnect");
}

// ===========================================================================
// 4. The transport mutex, held across an await
// ===========================================================================

/// The reader task holds the transport mutex across `stream.next().await`, and
/// that future only resolves when the CLI prints something or dies. Every other
/// transport user has to wait behind it — so a second prompt cannot be written
/// while the CLI is thinking, which is exactly when an interactive client needs
/// to be able to write (a follow-up, an interrupt).
///
/// This is not a slow path: it is unbounded. The send below does not return late,
/// it does not return at all.
#[tokio::test]
#[serial_test::serial]
async fn a_second_prompt_cannot_be_written_while_the_cli_is_quiet() {
    let fake = Transcript::new().await_stdin().wait_eof_for(4_000).build();

    let mut client = ClaudeSDKClientWorking::new(fake.options());
    client
        .connect(Some("premier".to_string()))
        .await
        .expect("connect");
    fake.wait_for_stdin_lines(1, WAIT).await;

    // The reader task is now parked in `next().await`, holding the lock.
    tokio::time::sleep(ONE_CYCLE).await;

    let blocked = tokio::time::timeout(
        Duration::from_millis(1_200),
        client.send_user_message("second".to_string()),
    )
    .await;
    assert!(
        blocked.is_err(),
        "send_user_message must be observed blocked on the transport mutex, got {blocked:?}"
    );
    let lines = fake.stdin_lines();
    assert_eq!(
        lines.len(),
        1,
        "nothing but the first prompt ever reached the CLI: {lines:?}"
    );

    // No `disconnect()` here: it would block on the very same lock — see
    // `disconnect_cannot_complete_while_the_cli_is_quiet`. Dropping the client
    // ends the runtime, which drops the task, the transport and the child.
}

/// The same lock, on the shutdown path: `disconnect` needs the transport mutex to
/// take the transport out, and the reader task will not give it back until the
/// CLI speaks. A caller that bounds the shutdown with a timeout is left with a
/// client that *reports* being disconnected — the state is written first, before
/// the blocking step — while the transport was never taken, never disconnected,
/// and the child process is still alive.
///
/// Writing the state before the fallible step is what makes a failed `disconnect`
/// leave a consistent state in this file and an inconsistent one in `client.rs`.
/// The flip side is here: it also makes the state a claim the client has not
/// honoured.
#[tokio::test]
#[serial_test::serial]
async fn disconnect_cannot_complete_while_the_cli_is_quiet() {
    let fake = Transcript::new().await_stdin().wait_eof_for(4_000).build();

    let mut client = ClaudeSDKClientWorking::new(fake.options());
    client
        .connect(Some("premier".to_string()))
        .await
        .expect("connect");
    fake.wait_for_stdin_lines(1, WAIT).await;
    tokio::time::sleep(ONE_CYCLE).await;

    let blocked = tokio::time::timeout(Duration::from_millis(1_200), client.disconnect()).await;
    assert!(
        blocked.is_err(),
        "disconnect must be observed blocked on the transport mutex, got {blocked:?}"
    );
    assert!(
        !client.is_connected().await,
        "the state was already written to Disconnected, before the step that blocked"
    );
}

// ===========================================================================
// 5. Shutdown
// ===========================================================================

/// `disconnect` before `connect` is a no-op, `disconnect` twice is a no-op, and a
/// completed `disconnect` clears the receiver — after which both read paths
/// refuse with `InvalidState` rather than returning an empty answer.
///
/// It also ends the reader task: the task's first action is to take the transport
/// mutex, and by then the transport has been taken out of the `Option`, so it
/// breaks out immediately (proved by the log assertion in
/// `tests/client_working_reader_task.rs`).
#[tokio::test]
#[serial_test::serial]
async fn disconnect_clears_the_receiver_and_is_idempotent() {
    let fake = Transcript::new().wait_eof_for(2_000).build();

    let mut client = ClaudeSDKClientWorking::new(fake.options());
    client
        .disconnect()
        .await
        .expect("disconnect before connect is a no-op");

    // Nothing between these two calls: `connect` does not yield after spawning
    // the reader task, and `disconnect` only yields once the transport is already
    // out of the `Option`, so the task's very first lock finds it empty.
    client.connect(None).await.expect("connect");
    client.disconnect().await.expect("disconnect");

    // Give the task its turn, so that it observes the empty `Option` and stops
    // instead of being dropped unpolled when the runtime goes away.
    tokio::time::sleep(ONE_CYCLE).await;

    assert!(!client.is_connected().await);

    let read = client
        .receive_message()
        .await
        .expect_err("the receiver was cleared");
    assert_eq!(invalid_state_message(&read), "Not connected");

    let drained = client
        .receive_response()
        .await
        .expect_err("receive_response propagates the same refusal");
    assert_eq!(invalid_state_message(&drained), "Not connected");

    client
        .disconnect()
        .await
        .expect("a second disconnect is a no-op");
    assert!(!client.is_connected().await);
}

/// `receive_message` on a client that was never connected is an error, not an
/// empty read: there is no receiver to take messages from, and the client says so
/// instead of returning `Ok(None)` (which a caller would read as "the turn is
/// over").
#[tokio::test]
#[serial_test::serial]
async fn receive_message_before_connect_is_an_invalid_state_error() {
    let mut client = ClaudeSDKClientWorking::new(ClaudeCodeOptions::default());
    let error = client
        .receive_message()
        .await
        .expect_err("no receiver exists before connect");
    assert_eq!(invalid_state_message(&error), "Not connected");
}
