//! Does the gateway's `text/event-stream` body obey the OpenAI streaming
//! protocol?
//!
//! Two levels, both production code:
//!
//! * `POST /v1/chat/completions` with `"stream": true` through `build_router`,
//!   which exercises `utils::streaming::create_sse_stream` — the framing, the
//!   `[DONE]` sentinel, the `chat.completion.chunk` envelope;
//! * `api::streaming_handler::handle_enhanced_streaming_response` fed from
//!   `claude_output::channel`, which exercises the chunker without a subprocess —
//!   and is therefore where the multi-byte assertions live, since the fake CLI
//!   round-trips its payload through `type` on Windows.

mod support;

use claude_code_api::api::streaming_handler::handle_enhanced_streaming_response;
use claude_code_api::models::openai::ChatCompletionStreamResponse;
use support::{FakeClaudeCli, claude_output, openai, sse, test_app_with_cli};

/// The SSE body of one streamed turn, as a client reads it.
async fn streamed_body(cli: &FakeClaudeCli) -> String {
    let server = test_app_with_cli(cli).await;
    server
        .post("/v1/chat/completions")
        .json(&openai::streaming_request("raconte"))
        .await
        .text()
}

/// The chunks `handle_enhanced_streaming_response` yields for `transcript`.
async fn chunks_for(
    transcript: &[claude_code_api::models::claude::ClaudeCodeOutput],
) -> Vec<ChatCompletionStreamResponse> {
    let rx = claude_output::channel(transcript);
    sse::collect(
        handle_enhanced_streaming_response(openai::TEST_MODEL.to_string(), rx, None, None).await,
    )
    .await
}

// ===========================================================================
// The `[DONE]` sentinel
// ===========================================================================

/// `create_done_event()` existed in `utils/streaming.rs` and was never called, so
/// the body simply stopped after the last chunk. openai-python, openai-node and
/// LangChain all stop reading on `data: [DONE]`; without it they wait for the
/// socket to close or report a truncated stream.
#[tokio::test]
async fn a_streamed_turn_ends_with_the_done_sentinel() {
    let cli = FakeClaudeCli::replying("court");
    let body = streamed_body(&cli).await;
    let frames = sse::frames(&body);

    assert_eq!(
        frames.last().map(String::as_str),
        Some("[DONE]"),
        "OpenAI clients need the [DONE] sentinel: {frames:?}"
    );
    assert_eq!(
        frames.iter().filter(|f| *f == "[DONE]").count(),
        1,
        "exactly one sentinel, at the end"
    );
}

/// The sentinel is unconditional: a CLI that printed nothing at all still closes
/// its stream properly, which is the only way a client can stop waiting.
#[tokio::test]
async fn a_silent_cli_still_closes_the_stream_with_the_sentinel() {
    let cli = FakeClaudeCli::silent();
    let body = streamed_body(&cli).await;
    let frames = sse::frames(&body);

    assert_eq!(frames.last().map(String::as_str), Some("[DONE]"));
    assert_eq!(
        sse::finish_reasons(&sse::chunks(&body)),
        Vec::<String>::new(),
        "documents the gap: the turn is closed without ever being finished — a \
         client sees [DONE] with no finish_reason and must treat it as truncated"
    );
}

/// Same shape when the CLI dies after its first words: the sentinel arrives, the
/// `finish_reason` does not.
#[tokio::test]
async fn a_turn_cut_before_the_result_has_no_finish_reason() {
    let chunks = chunks_for(&[claude_output::assistant_text("debut de reponse")]).await;

    assert_eq!(sse::text(&chunks), "debut de reponse");
    assert!(
        sse::finish_reasons(&chunks).is_empty(),
        "no `result` message, so no end-of-message marker"
    );
}

// ===========================================================================
// The `chat.completion.chunk` envelope
// ===========================================================================

#[tokio::test]
async fn every_frame_before_the_sentinel_is_a_well_formed_chunk() {
    let cli = FakeClaudeCli::replying("une reponse assez longue pour etre decoupee");
    let body = streamed_body(&cli).await;
    let chunks = sse::chunks(&body);

    assert!(chunks.len() > 2, "the text must be split: {chunks:#?}");

    let id = &chunks[0].id;
    for chunk in &chunks {
        assert_eq!(chunk.object, "chat.completion.chunk");
        assert_eq!(&chunk.id, id, "every chunk of a turn shares one id");
        assert_eq!(chunk.model, openai::TEST_MODEL);
        assert_eq!(chunk.choices.len(), 1, "the gateway never fans out choices");
        assert_eq!(chunk.choices[0].index, 0);
    }
}

#[tokio::test]
async fn the_first_chunk_announces_the_assistant_role_and_nothing_else() {
    let cli = FakeClaudeCli::replying("bonjour");
    let body = streamed_body(&cli).await;
    let chunks = sse::chunks(&body);

    let first = &chunks[0].choices[0];
    assert_eq!(first.delta.role.as_deref(), Some("assistant"));
    assert_eq!(first.delta.content, None);
    assert_eq!(first.finish_reason, None);
}

/// `finish_reason` must appear exactly once, on the last chunk. A second one, or
/// one in the middle, ends the message early for every OpenAI client.
#[tokio::test]
async fn finish_reason_appears_once_on_the_last_chunk() {
    let cli = FakeClaudeCli::replying("quelques mots a decouper en morceaux");
    let body = streamed_body(&cli).await;
    let chunks = sse::chunks(&body);

    assert_eq!(sse::finish_reasons(&chunks), vec!["stop".to_string()]);

    let last = chunks.last().expect("at least one chunk");
    assert_eq!(last.choices[0].finish_reason.as_deref(), Some("stop"));
    assert_eq!(
        last.choices[0].delta.content, None,
        "the terminal chunk carries no content"
    );
}

/// The whole answer must survive the chunking, in order.
#[tokio::test]
async fn the_streamed_deltas_reassemble_the_answer() {
    let answer = "Un paragraphe entier, decoupe en petits morceaux par le gateway.";
    let cli = FakeClaudeCli::replying(answer);
    let body = streamed_body(&cli).await;

    assert_eq!(sse::text(&sse::chunks(&body)), answer);
}

// ===========================================================================
// Non-ASCII answers — the chunker used to abort the task here
// ===========================================================================

/// `utils::text_chunker` cut on byte offsets, so slicing a multi-byte character
/// in half panicked inside the `async_stream` generator and the streaming task
/// died mid-body. Every one of these answers is long enough to be split at the
/// handler's `chunk_size: 15`.
#[tokio::test]
async fn a_multibyte_answer_streams_intact() {
    for answer in [
        "日本語のテキストを分割する必要があります。",
        "Les élèves préfèrent répéter la leçon à côté du poêle ancien.",
        "Déjà là ! Très bien. Ça va être génial, non ?",
        "mixed 🚀 emoji 🇫🇷 and 👨‍👩‍👧‍👦 families in one answer",
        "Кириллица и ещё немного текста для разбиения на части.",
    ] {
        let chunks = chunks_for(&[
            claude_output::assistant_text(answer),
            claude_output::result_success(),
        ])
        .await;

        assert_eq!(sse::text(&chunks), answer, "lossless for {answer:?}");
        assert_eq!(sse::finish_reasons(&chunks), vec!["stop".to_string()]);
    }
}

/// Every delta must itself be valid UTF-8 text a client can append as-is — a
/// chunk boundary in the middle of a character would not even serialise.
#[tokio::test]
async fn no_delta_splits_a_character() {
    let answer = "日本語のテキストを分割する必要があります。".repeat(3);
    let chunks = chunks_for(&[
        claude_output::assistant_text(&answer),
        claude_output::result_success(),
    ])
    .await;

    let deltas: Vec<&str> = chunks
        .iter()
        .flat_map(|c| c.choices.iter())
        .filter_map(|c| c.delta.content.as_deref())
        .collect();

    assert!(deltas.len() > 1, "the text must be split: {deltas:?}");
    assert_eq!(deltas.concat(), answer);
    for delta in &deltas {
        assert!(!delta.is_empty(), "an empty delta is wasted bandwidth");
    }
}

// ===========================================================================
// Tool calls and sidechains
// ===========================================================================

#[tokio::test]
async fn a_tool_call_is_streamed_then_the_sentinel_closes_the_body() {
    let cli = FakeClaudeCli::calling_tool("call_7", "grep", serde_json::json!({"q": "x"}));
    let body = streamed_body(&cli).await;
    let chunks = sse::chunks(&body);

    assert_eq!(
        sse::tool_calls(&chunks),
        vec![("grep".to_string(), r#"{"q":"x"}"#.to_string())]
    );
    assert_eq!(sse::finish_reasons(&chunks), vec!["stop".to_string()]);
    assert_eq!(
        sse::frames(&body).last().map(String::as_str),
        Some("[DONE]")
    );
}

#[tokio::test]
async fn a_sidechain_message_is_not_streamed_to_the_client() {
    let chunks = chunks_for(&[
        claude_output::sidechain_text("toolu_1", "pensees du sous-agent"),
        claude_output::assistant_text("la vraie reponse"),
        claude_output::result_success(),
    ])
    .await;

    assert_eq!(sse::text(&chunks), "la vraie reponse");
}

/// A client that stops reading gets no `finish_reason`, which is the correct
/// signal — the generator is dropped and nothing further is produced.
#[tokio::test]
async fn abandoning_the_stream_early_yields_no_finish_reason() {
    use futures::stream::StreamExt;

    let rx = claude_output::channel(&[
        claude_output::assistant_text("un debut de reponse assez long pour plusieurs morceaux"),
        claude_output::result_success(),
    ]);
    let mut stream =
        handle_enhanced_streaming_response(openai::TEST_MODEL.to_string(), rx, None, None).await;

    let mut seen = Vec::new();
    for _ in 0..2 {
        seen.push(stream.next().await.expect("two chunks are available"));
    }
    drop(stream);

    assert!(sse::finish_reasons(&seen).is_empty());
    assert_eq!(seen[0].choices[0].delta.role.as_deref(), Some("assistant"));
}

// ===========================================================================
// Bugs outside this file's ownership — evidence, not fixes
// ===========================================================================

/// BUG (`api::streaming_handler::handle_enhanced_streaming_response`): a `result`
/// message flagged `is_error: true` is streamed to the client as a **successful**
/// completion.
///
/// Triggering input: `claude_output::result_error("boom")`, i.e. the CLI's
/// `{"type":"result","subtype":"error","is_error":true}`. The handler matches on
/// `output.r#type` alone, so the error result takes the same arm as a success: it
/// emits `finish_reason: "stop"` *and* stores `completed_normally = true`, which
/// also disarms `SseDisconnectGuard` — so the CLI session is left running instead
/// of being interrupted.
///
/// Expected: no `"stop"`. OpenAI's vocabulary for a turn the model could not
/// finish is a different `finish_reason` (`"length"`, `"content_filter"`) or an
/// in-band error frame; `"stop"` tells the client the answer is complete.
///
/// The fix belongs to `src/api/streaming_handler.rs`, which this file does not
/// own, hence `#[ignore]`.
#[tokio::test]
#[ignore = "api::streaming_handler::handle_enhanced_streaming_response reports a failed result as finish_reason \"stop\""]
async fn a_failed_result_should_not_be_streamed_as_a_normal_stop() {
    let chunks = chunks_for(&[
        claude_output::assistant_text("partiel"),
        claude_output::result_error("boom"),
    ])
    .await;

    assert_ne!(
        sse::finish_reasons(&chunks),
        vec!["stop".to_string()],
        "a failed turn must not claim it stopped normally"
    );
}

/// BUG (`api::streaming_handler::handle_enhanced_streaming_response`): a CLI
/// output of type `error` produces **nothing at all**.
///
/// Triggering input: `{"type": "error", …}` — it falls into the `_ => {}` arm, so
/// the stream ends with the role chunk, no content and no `finish_reason`. The
/// client is handed a `[DONE]` it cannot distinguish from a successful empty
/// answer, and never learns the turn failed.
///
/// Expected: an in-band error frame, or at least a `finish_reason` that is not
/// silence. Fix belongs to `src/api/streaming_handler.rs`, hence `#[ignore]`.
#[tokio::test]
#[ignore = "api::streaming_handler::handle_enhanced_streaming_response drops `type: error` output silently"]
async fn an_error_output_should_reach_the_client() {
    let chunks = chunks_for(&[claude_output::unknown("error")]).await;

    assert!(
        chunks.len() > 1,
        "the client must learn about the error, not just get the role chunk: {chunks:#?}"
    );
}
