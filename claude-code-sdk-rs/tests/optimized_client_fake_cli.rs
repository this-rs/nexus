//! `OptimizedClient` driven against the `fake_claude` test double.
//!
//! Everything here is real except the CLI: a real `ConnectionPool`, real
//! `SubprocessTransport`s, real spawns and pipes. No network, no `claude`
//! install, no shell script (so it runs on windows-latest too). The unit tests
//! in `src/optimized_client.rs` pin the pool's bookkeeping with a transport
//! double; these pin what happens once a process is actually on the other end.

mod support;

use std::time::Duration;

use nexus_claude::{ClientMode, Message, OptimizedClient};
use support::*;

/// Generous enough for two process spawns on a loaded runner, far below
/// `execute_query`'s own hard-coded 120 s timeout — so a stuck turn fails the
/// test in seconds instead of stalling the job for two minutes.
const WAIT: Duration = Duration::from_secs(20);

fn client(fake: &FakeCli, mode: ClientMode) -> OptimizedClient {
    OptimizedClient::new(fake.options(), mode).expect("options point at the fake CLI")
}

/// Run one query, failing the test rather than waiting out the internal timeout.
async fn query(client: &OptimizedClient, prompt: &str) -> Vec<Message> {
    tokio::time::timeout(WAIT, client.query(prompt.to_string()))
        .await
        .expect("the turn must finish well before execute_query's 120 s timeout")
        .expect("the fake CLI answers this turn")
}

fn has_init(messages: &[Message]) -> bool {
    messages
        .iter()
        .any(|m| matches!(m, Message::System { subtype, .. } if subtype == "init"))
}

/// The whole one-shot path: pool spawns the CLI, the prompt goes out, the turn
/// comes back, and the `result` message's usage is billed to the client.
///
/// `init` is printed *after* the prompt here, which is what makes it observable:
/// the subscription is taken before the prompt is written, so nothing the CLI
/// says in answer to a prompt can be missed.
#[tokio::test]
async fn a_one_shot_query_drives_the_fake_cli_end_to_end() {
    let fake = Transcript::new()
        .await_stdin()
        .init("sess-opt")
        .assistant_text("quatre")
        .result_ok("quatre")
        .wait_eof()
        .build();
    let client = client(&fake, ClientMode::OneShot);

    let messages = query(&client, "2 + 2 ?").await;

    assert_eq!(assistant_texts(&messages), vec!["quatre".to_string()]);
    assert!(
        has_init(&messages),
        "everything printed after the prompt is observable: {messages:?}"
    );
    assert!(matches!(messages.last(), Some(Message::Result { .. })));
    assert_eq!(
        fake.stdin_lines().len(),
        1,
        "exactly one prompt reached the CLI"
    );

    let usage = client.get_usage_stats().await;
    assert_eq!(
        usage.total_input_tokens, 3,
        "the `result` message bills 3/5"
    );
    assert_eq!(usage.total_output_tokens, 5);
    assert_eq!(usage.session_count, 1);
    assert!(!client.is_budget_exceeded().await, "no limit was set");
}

/// A second query on a CLI that is still alive reuses the pooled connection.
/// The proof is the answer itself: a fresh process would have replayed the
/// transcript from the top and said `un` again.
#[tokio::test]
async fn the_pool_reuses_a_live_connection_for_the_next_query() {
    let fake = Transcript::new()
        .await_stdin()
        .init("sess-reuse")
        .assistant_text("un")
        .result_ok("un")
        .await_stdin()
        .assistant_text("deux")
        .result_ok("deux")
        .wait_eof()
        .build();
    let client = client(&fake, ClientMode::OneShot);

    let first = query(&client, "premier").await;
    let second = query(&client, "second").await;

    assert_eq!(assistant_texts(&first), vec!["un".to_string()]);
    assert_eq!(
        assistant_texts(&second),
        vec!["deux".to_string()],
        "a replacement process would have answered `un` again"
    );
    assert!(
        !has_init(&second),
        "the handshake belongs to the first turn only: {second:?}"
    );
    assert_eq!(
        fake.stdin_lines().len(),
        2,
        "both prompts went to the same process"
    );
    assert_eq!(client.get_usage_stats().await.session_count, 2);
}

/// The counterpart: a CLI that exited must not be handed to the next caller.
///
/// Both `release` and `acquire` ask `is_connected()`, which only became
/// trustworthy once `SubprocessTransport` started clearing a liveness flag when
/// its stdout reaches EOF. While it answered `true` for ever, the second query
/// here wrote into a dead pipe and ended on the 120 s timeout.
#[tokio::test]
async fn a_cli_that_exited_is_replaced_instead_of_reused() {
    let fake = Transcript::new()
        .await_stdin()
        .init("sess-dead")
        .assistant_text("une seule fois")
        .result_ok("une seule fois")
        .exit_with(0)
        .build();
    let client = client(&fake, ClientMode::OneShot);

    let first = query(&client, "premier").await;
    assert_eq!(assistant_texts(&first), vec!["une seule fois".to_string()]);

    // Let the stdout reader notice the EOF the exiting child just produced.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let second = query(&client, "second").await;
    assert_eq!(
        assistant_texts(&second),
        vec!["une seule fois".to_string()],
        "a fresh CLI was spawned, so the transcript was replayed from the top"
    );
    assert!(
        has_init(&second),
        "a replacement process prints its own handshake: {second:?}"
    );
    assert_eq!(client.get_usage_stats().await.session_count, 2);
}

/// A CLI that dies mid-turn is not reported as a failure: the message stream
/// just ends, `collect_messages` returns what it collected, and the caller gets
/// `Ok` with a turn that has no `result` message — indistinguishable from a
/// complete one unless the caller checks for itself.
#[tokio::test]
async fn a_cli_that_dies_mid_turn_is_reported_as_a_successful_partial_turn() {
    let fake = Transcript::new()
        .await_stdin()
        .assistant_text("coupe en deux")
        .exit_with(7)
        .build();
    let client = client(&fake, ClientMode::OneShot);

    let messages = query(&client, "raconte").await;

    assert_eq!(
        assistant_texts(&messages),
        vec!["coupe en deux".to_string()]
    );
    assert!(
        !messages.iter().any(|m| matches!(m, Message::Result { .. })),
        "the turn never completed, yet the call succeeded: {messages:?}"
    );
    assert_eq!(
        client.get_usage_stats().await.session_count,
        0,
        "no result message means nothing was billed"
    );
}

/// The interactive path over a real transport: the background processor
/// subscribes to the subprocess broadcast, `send_interactive` writes the prompt
/// while the processor is waiting, and the turn comes out of the channel.
///
/// Multi-threaded on purpose: the processor must make progress while the test
/// task is inside `send_interactive`, which is exactly what the old
/// write-lock-across-`await` shape made impossible.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interactive_session_round_trips_over_a_real_subprocess() {
    let fake = Transcript::new()
        .await_stdin()
        .assistant_text("bonjour")
        .result_ok("bonjour")
        .wait_eof()
        .build();
    let client = client(&fake, ClientMode::Interactive);

    client
        .start_interactive_session()
        .await
        .expect("the session starts");
    tokio::time::timeout(WAIT, client.send_interactive("salut".to_string()))
        .await
        .expect("send_interactive must not wait on the message processor")
        .expect("send");

    let messages = tokio::time::timeout(WAIT, client.receive_interactive())
        .await
        .expect("the processor must forward the turn")
        .expect("receive");

    assert_eq!(assistant_texts(&messages), vec!["bonjour".to_string()]);
    assert!(matches!(messages.last(), Some(Message::Result { .. })));

    client.interrupt().await.expect("interrupt reaches the CLI");
    let control = fake
        .wait_for_stdin_lines(2, WAIT)
        .await
        .into_iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(&line).ok())
        .find(|line| line["type"] == "control_request")
        .expect("the interrupt was written to the CLI's stdin");
    assert_eq!(control["request"]["type"], "interrupt");

    client
        .end_interactive_session()
        .await
        .expect("the session ends");
}

/// Batch mode over the pool: one permit, one connection, two scripted turns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_runs_both_prompts_over_one_pooled_connection() {
    let fake = Transcript::new()
        .await_stdin()
        .assistant_text("un")
        .result_ok("un")
        .await_stdin()
        .assistant_text("deux")
        .result_ok("deux")
        .wait_eof()
        .build();
    let client = client(&fake, ClientMode::Batch { max_concurrent: 1 });

    let results = tokio::time::timeout(
        WAIT,
        client.process_batch(vec!["premier".to_string(), "second".to_string()]),
    )
    .await
    .expect("the batch must finish")
    .expect("batch mode accepts the batch");

    assert_eq!(results.len(), 2);
    let answers: Vec<String> = results
        .iter()
        .map(|result| {
            let messages = result.as_ref().expect("every prompt succeeds");
            assistant_texts(messages).join("")
        })
        .collect();
    assert_eq!(answers, vec!["un".to_string(), "deux".to_string()]);
    assert_eq!(client.get_usage_stats().await.session_count, 2);
}
