//! `api::chat` over the production router: the decisions `chat_completions` and
//! `interrupt_session` make before, around and after the CLI call.
//!
//! The happy paths live in `support_harness.rs`; everything here is a refusal, an
//! identity or a persistence rule — the parts that used to be wrong.

mod support;

use std::path::PathBuf;

use axum::http::StatusCode;
use claude_code_api::models::openai::ChatCompletionResponse;
use serde_json::json;
use support::{
    FakeClaudeCli, TestSettings, openai, test_app, test_app_with, test_app_with_cli,
    test_app_with_components, test_components,
};
use tempfile::TempDir;

// ===========================================================================
// An unknown conversation_id is the caller's mistake, and costs nothing
// ===========================================================================

/// `conversation_id` is taken verbatim from the client. When it names nothing,
/// the answer is `404 not_found_error` — not the `500 internal_error` that the
/// late `add_message` failure used to produce.
#[tokio::test]
async fn an_unknown_conversation_id_is_a_404() {
    let cli = FakeClaudeCli::replying("peu importe");
    let server = test_app_with_cli(&cli).await;

    let response = server
        .post("/v1/chat/completions")
        .json(
            &openai::request()
                .message(openai::user("salut"))
                .conversation_id("nope")
                .build(),
        )
        .await;

    response.assert_status(StatusCode::NOT_FOUND);
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"]["type"], "not_found_error");
    assert_eq!(
        body["error"]["message"],
        "Not found: Conversation not found"
    );
}

/// The check happens *before* the CLI is launched, which is what makes it worth
/// anything: a turn that cannot be stored must not be billed first.
///
/// `test_app()` points `claude.command` at a path that cannot be spawned, so any
/// request that reaches the CLI comes back `500 claude_process_error`. Getting a
/// `404` is therefore proof that nothing was spawned.
#[tokio::test]
async fn the_conversation_is_verified_before_the_cli_is_spawned() {
    let server = test_app().await;

    let response = server
        .post("/v1/chat/completions")
        .json(
            &openai::request()
                .message(openai::user("salut"))
                .conversation_id("inconnu")
                .build(),
        )
        .await;

    response.assert_status(StatusCode::NOT_FOUND);
    let body: serde_json::Value = response.json();
    assert_eq!(
        body["error"]["type"], "not_found_error",
        "a 500 claude_process_error here would mean the CLI ran first"
    );
}

// ===========================================================================
// What a turn leaves behind
// ===========================================================================

/// A non-streaming turn appends the client messages *and* the assistant answer to
/// the conversation, and stamps the id it used on the response.
#[tokio::test]
async fn a_non_streaming_turn_stores_both_sides() {
    let cli = FakeClaudeCli::replying("la reponse");
    let components = test_components(TestSettings::new().command(cli.command()).build()).await;
    let conversations = components.chat_state.conversation_manager.clone();
    let server = test_app_with_components(components);

    let id = conversations
        .create_conversation(Some(openai::TEST_MODEL.to_string()))
        .await
        .expect("the in-memory store creates a conversation");

    let response = server
        .post("/v1/chat/completions")
        .json(
            &openai::request()
                .message(openai::user("ma question"))
                .conversation_id(&id)
                .build(),
        )
        .await;

    response.assert_status_ok();
    let body: ChatCompletionResponse = response.json();
    assert_eq!(body.conversation_id.as_deref(), Some(id.as_str()));

    let stored = conversations
        .get_conversation(&id)
        .await
        .expect("the conversation still exists");
    let turns: Vec<(&str, Option<String>)> = stored
        .messages
        .iter()
        .map(|m| (m.role.as_str(), openai::text_of(m)))
        .collect();
    assert_eq!(
        turns,
        vec![
            ("user", Some("ma question".to_string())),
            ("assistant", Some("la reponse".to_string())),
        ]
    );
}

/// A **streamed** turn is persisted too. It used to be lost: `add_message` was
/// only called in the non-streaming branch, so a conversation driven with
/// `"stream": true` had no history to replay on the next turn.
#[tokio::test]
async fn a_streamed_turn_is_persisted_as_well() {
    let cli = FakeClaudeCli::replying("reponse diffusee");
    let components = test_components(TestSettings::new().command(cli.command()).build()).await;
    let conversations = components.chat_state.conversation_manager.clone();
    let server = test_app_with_components(components);

    let id = conversations
        .create_conversation(None)
        .await
        .expect("create a conversation");

    let response = server
        .post("/v1/chat/completions")
        .json(
            &openai::request()
                .message(openai::user("question diffusee"))
                .conversation_id(&id)
                .stream(true)
                .build(),
        )
        .await;
    response.assert_status_ok();

    let stored = conversations
        .get_conversation(&id)
        .await
        .expect("the conversation still exists");
    let turns: Vec<(&str, Option<String>)> = stored
        .messages
        .iter()
        .map(|m| (m.role.as_str(), openai::text_of(m)))
        .collect();
    assert_eq!(
        turns,
        vec![
            ("user", Some("question diffusee".to_string())),
            ("assistant", Some("reponse diffusee".to_string())),
        ],
        "a streamed conversation must accumulate history like any other"
    );
}

// ===========================================================================
// The response cache
// ===========================================================================

/// A cache hit must not hand the caller the conversation the entry was created
/// for.
///
/// `ResponseCache::generate_key` keys on `(model, messages)`, so two unrelated
/// callers that send the same prompt collide. Replaying the stored
/// `conversation_id` gave the second one an identifier pointing at the first
/// one's history — which it could then read and append to.
#[tokio::test]
async fn a_cache_hit_keeps_the_callers_own_conversation() {
    let cli = FakeClaudeCli::replying("meme question, meme reponse");
    let server = test_app_with_cli(&cli).await;

    let first: ChatCompletionResponse = server
        .post("/v1/chat/completions")
        .json(&openai::chat_request("question partagee"))
        .await
        .json();
    let second: ChatCompletionResponse = server
        .post("/v1/chat/completions")
        .json(&openai::chat_request("question partagee"))
        .await
        .json();

    assert_eq!(
        first.id, second.id,
        "the second request must be served from the cache"
    );
    assert!(second.conversation_id.is_some());
    assert_ne!(
        first.conversation_id, second.conversation_id,
        "each caller must keep the conversation the gateway created for it"
    );
}

/// BUG (`chat_completions` + `ResponseCache::generate_key`): the cache key is
/// `(model, messages)`. `tools`, `tool_choice`, `temperature`, `max_tokens` and
/// `stop` are not part of it, so a caller that declares a tool is served the
/// answer computed for a caller that declared none and never sees a tool call.
///
/// Triggering input: the same prompt twice, the second time with `tools`.
/// Expected: `finish_reason: "tool_calls"` on the second answer.
/// Actual: the cached text answer, `finish_reason: "stop"`.
/// Not fixed here: `generate_key` lives in `core::cache`, which another owner
/// holds; the key needs the request parameters, not a patch at the call site.
#[tokio::test]
#[ignore = "documents a bug: the response cache ignores tools and every sampling parameter"]
async fn a_request_that_declares_tools_is_not_served_a_toolless_cache_hit() {
    let cli = FakeClaudeCli::replying(r#"{"city": "Lyon"}"#);
    let server = test_app_with_cli(&cli).await;

    let plain: ChatCompletionResponse = server
        .post("/v1/chat/completions")
        .json(&openai::chat_request("quel temps a Lyon ?"))
        .await
        .json();
    assert_eq!(
        plain.choices[0].finish_reason.as_deref(),
        Some("stop"),
        "without declared tools the JSON answer stays text"
    );

    let with_tools: ChatCompletionResponse = server
        .post("/v1/chat/completions")
        .json(
            &openai::request()
                .message(openai::user("quel temps a Lyon ?"))
                .tools(vec![openai::tool(
                    "get_weather",
                    "Current weather",
                    json!({"type": "object", "properties": {"city": {"type": "string"}}}),
                )])
                .build(),
        )
        .await
        .json();
    assert_eq!(
        with_tools.choices[0].finish_reason.as_deref(),
        Some("tool_calls"),
        "a cached answer computed without tools must not be replayed to a caller that declared one"
    );
}

// ===========================================================================
// Session identity
// ===========================================================================

/// The interactive session must be filed under the `conversation_id` the client
/// was handed back.
///
/// It used to be filed under a UUID minted inside
/// `get_or_create_session_and_send` whenever the request carried no
/// `conversation_id`, so the client held an identifier that reached nothing:
/// `POST /v1/sessions/{id}/interrupt` always answered 404 and the SSE disconnect
/// guard always resolved `Ok(false)`.
#[tokio::test]
async fn the_interactive_session_answers_to_the_id_the_client_received() {
    let cli = FakeClaudeCli::replying("bonjour");
    let components = test_components(
        TestSettings::new()
            .command(cli.command())
            .interactive_sessions(true)
            .build(),
    )
    .await;
    let sessions = components.chat_state.interactive_session_manager.clone();
    let server = test_app_with_components(components);

    let body: ChatCompletionResponse = server
        .post("/v1/chat/completions")
        .json(&openai::chat_request("salut"))
        .await
        .json();
    let conversation_id = body
        .conversation_id
        .expect("the gateway returns the conversation it created");

    assert_eq!(
        sessions.active_sessions(),
        1,
        "the completion must have created exactly one interactive session"
    );
    assert!(
        !matches!(sessions.interrupt_session(&conversation_id), Ok(false)),
        "Ok(false) means no session is filed under {conversation_id}"
    );
}

/// The HTTP symptom of the same identity: the interrupt route must reach the
/// session created by the completion.
#[tokio::test]
async fn interrupting_the_session_of_a_live_completion_is_not_a_404() {
    let cli = StdinHoldingCli::replying("je reflechis");
    let server = test_app_with(
        TestSettings::new()
            .command(cli.command())
            .interactive_sessions(true)
            .build(),
    )
    .await;

    let body: ChatCompletionResponse = server
        .post("/v1/chat/completions")
        .json(&openai::chat_request("salut"))
        .await
        .json();
    let conversation_id = body.conversation_id.expect("a conversation id");

    let response = server
        .post(&format!("/v1/sessions/{conversation_id}/interrupt"))
        .await;
    response.assert_status_ok();
    let interrupted: serde_json::Value = response.json();
    assert_eq!(interrupted["status"], "interrupted");
    assert_eq!(interrupted["conversation_id"], conversation_id);
}

/// Once the CLI process is gone, its stdin channel is closed and the interrupt
/// cannot be delivered: that is a `500`, not a silent success.
#[tokio::test]
async fn interrupting_a_dead_cli_reports_the_closed_channel() {
    let cli = FakeClaudeCli::replying("et je m'en vais");
    let server = test_app_with(
        TestSettings::new()
            .command(cli.command())
            .interactive_sessions(true)
            .build(),
    )
    .await;

    let body: ChatCompletionResponse = server
        .post("/v1/chat/completions")
        .json(&openai::chat_request("salut"))
        .await
        .json();
    let conversation_id = body.conversation_id.expect("a conversation id");

    // The fake CLI printed its transcript and exited. The writer task only
    // notices when it next writes to the dead pipe, so the first interrupt may
    // still be accepted; the following one cannot be.
    let mut last = StatusCode::OK;
    for _ in 0..50 {
        let response = server
            .post(&format!("/v1/sessions/{conversation_id}/interrupt"))
            .await;
        last = response.status_code();
        if last == StatusCode::INTERNAL_SERVER_ERROR {
            let body: serde_json::Value = response.json();
            assert!(
                body["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("stdin channel is closed"),
                "unexpected error body: {body}"
            );
            assert_eq!(body["conversation_id"], conversation_id);
            return;
        }
        assert_eq!(
            last,
            StatusCode::OK,
            "a session that exists answers 200 or 500, never 404"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the interrupt never reported the closed stdin channel (last status {last})");
}

// ===========================================================================
// The timeout, end to end
// ===========================================================================

/// A CLI that holds the connection and never answers must end as
/// `500 claude_process_error` once `claude.timeout_seconds` is spent — not hang.
///
/// This is the HTTP face of the budget fix: the loop now bounds each `recv` by the
/// remaining budget, so a one-second setting really costs one second instead of a
/// whole five-second slice. The exact duration is asserted on a paused clock in the
/// unit tests; here what matters is the status and the message.
///
/// It has to go through the interactive manager: that one keeps the CLI's stdin
/// open for the life of the session, so a silent CLI stays alive and the handler
/// really waits. `ClaudeManager::create_session_with_message` (the `ProcessPool`
/// path) writes the prompt and lets stdin close, so the same fake CLI exits at
/// once, the channel closes and the turn ends as an empty `200` instead.
#[tokio::test]
async fn a_cli_that_never_answers_times_out_as_a_claude_process_error() {
    let cli = StdinHoldingCli::silent();
    let server = test_app_with(
        TestSettings::new()
            .command(cli.command())
            .interactive_sessions(true)
            .timeout_seconds(1)
            .build(),
    )
    .await;

    let response = server
        .post("/v1/chat/completions")
        .json(&openai::chat_request("reponds quelque chose"))
        .await;

    response.assert_status(StatusCode::INTERNAL_SERVER_ERROR);
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"]["type"], "claude_process_error");
    assert_eq!(
        body["error"]["message"],
        "Claude process error: Timeout waiting for response after 1 seconds"
    );
}

// ===========================================================================
// image_url: the gateway resolves these server side, so they are attack surface
// ===========================================================================

/// A filesystem path is not an `image_url`. It used to be passed through as one:
/// `process_image_url` returned the string unchanged and the prompt got
/// `Image: /etc/passwd`, which the CLI reads.
#[tokio::test]
async fn an_image_url_that_is_a_local_path_is_refused() {
    let cli = FakeClaudeCli::replying("jamais atteint");
    let server = test_app_with_cli(&cli).await;

    let response = server
        .post("/v1/chat/completions")
        .json(
            &openai::request()
                .message(openai::multimodal("user", &["lis ceci"], &["/etc/passwd"]))
                .build(),
        )
        .await;

    response.assert_status(StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("data:image/"),
        "the refusal must say what is accepted: {body}"
    );
}

/// The cloud metadata endpoint is reachable from the server and from nowhere else:
/// fetching a client-supplied `http://169.254.169.254/...` would hand over the
/// instance credentials.
#[tokio::test]
async fn an_image_url_on_the_metadata_endpoint_is_refused() {
    let cli = FakeClaudeCli::replying("jamais atteint");
    let server = test_app_with_cli(&cli).await;

    for url in [
        "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
        "http://127.0.0.1:9200/_cluster/health",
        "file:///etc/shadow",
    ] {
        let response = server
            .post("/v1/chat/completions")
            .json(
                &openai::request()
                    .message(openai::multimodal("user", &["regarde"], &[url]))
                    .build(),
            )
            .await;
        assert_eq!(
            response.status_code(),
            StatusCode::BAD_REQUEST,
            "{url} must not be fetched by the gateway"
        );
    }
}

/// A `data:image/...` URL is still accepted and still reaches the CLI as a file.
#[tokio::test]
async fn a_data_url_image_still_reaches_the_cli() {
    let cli = FakeClaudeCli::replying("image vue");
    let server = test_app_with_cli(&cli).await;

    let response = server
        .post("/v1/chat/completions")
        .json(
            &openai::request()
                .message(openai::multimodal(
                    "user",
                    &["decris"],
                    &["data:image/png;base64,bmV4dXM="],
                ))
                .build(),
        )
        .await;

    response.assert_status_ok();
    let body: ChatCompletionResponse = response.json();
    assert_eq!(openai::first_text(&body).as_deref(), Some("image vue"));
}

// ===========================================================================
// A fake CLI that keeps its stdin open
// ===========================================================================

/// Like [`FakeClaudeCli`], but the script blocks on stdin after printing its
/// transcript instead of exiting.
///
/// `FakeClaudeCli` exits immediately, which leaves an interactive session holding
/// a dead process: writing the interrupt to its stdin then fails, and whether the
/// gateway notices before or after the interrupt is a race. Here the process stays
/// alive for as long as the gateway holds its stdin — until the `TestServer`, and
/// with it `InteractiveSessionManager`, is dropped at the end of the test. That
/// `Drop` kills the process group.
struct StdinHoldingCli {
    _dir: TempDir,
    script: PathBuf,
}

impl StdinHoldingCli {
    fn replying(text: &str) -> Self {
        Self::with_transcript(&format!(
            "{}\n{}\n",
            serde_json::json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
            }),
            serde_json::json!({"type": "result", "subtype": "success", "is_error": false}),
        ))
    }

    /// A CLI that is alive and says nothing at all — the only way to reach the
    /// timeout branch of `handle_non_streaming_response` over HTTP, since a CLI
    /// that exits closes the channel and ends the turn instead.
    fn silent() -> Self {
        Self::with_transcript("")
    }

    fn with_transcript(transcript: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir for the stdin-holding CLI");
        let payload = dir.path().join("payload.ndjson");
        std::fs::write(&payload, transcript).expect("write the transcript");

        let body = if cfg!(windows) {
            // `findstr` reads stdin until the pipe closes, then the script ends.
            format!(
                "@echo off\r\ntype \"{}\"\r\nfindstr \"^\" > nul\r\nexit /b 0\r\n",
                payload.display()
            )
        } else {
            format!(
                "#!/bin/sh\ncat '{}'\ncat > /dev/null\nexit 0\n",
                payload.display()
            )
        };

        let script = support::fake_exec::plant_fake_cli(dir.path(), &body);

        Self { _dir: dir, script }
    }

    fn command(&self) -> String {
        self.script.to_string_lossy().into_owned()
    }
}
