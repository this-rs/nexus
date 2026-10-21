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
    // Through the real router, end to end: a streamed call finishes on
    // `tool_calls`, not `stop` (see `a_turn_that_called_a_tool_finishes_on_tool_calls`
    // for the handler-level case and the reason).
    assert_eq!(sse::finish_reasons(&chunks), vec!["tool_calls".to_string()]);
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
// How a turn ends: finish_reason, usage, errors
// ===========================================================================

/// OpenAI says `tool_calls` when the model stopped in order to call a tool, and
/// `stop` only when the answer is complete. The handler used to say `stop` either
/// way, so a client driving an agent loop could not tell "here is your answer"
/// from "run this tool and come back".
#[tokio::test]
async fn a_turn_that_called_a_tool_finishes_on_tool_calls() {
    let chunks = chunks_for(&[
        claude_output::assistant_tool_use("call_1", "Read", serde_json::json!({"file": "a"})),
        claude_output::result_success(),
    ])
    .await;

    assert_eq!(
        sse::tool_calls(&chunks),
        [("Read".to_string(), r#"{"file":"a"}"#.to_string())]
    );
    assert_eq!(
        sse::finish_reasons(&chunks),
        vec!["tool_calls".to_string()],
        "a turn that stopped to call a tool did not stop normally: {chunks:#?}"
    );
}

/// And a turn that only spoke still finishes on `stop`.
#[tokio::test]
async fn a_turn_that_only_spoke_finishes_on_stop() {
    let chunks = chunks_for(&[
        claude_output::assistant_text("voila"),
        claude_output::result_success(),
    ])
    .await;

    assert_eq!(sse::finish_reasons(&chunks), vec!["stop".to_string()]);
    assert!(
        sse::usages(&chunks).is_empty(),
        "no counts reported, none invented"
    );
}

/// `stream_options.include_usage` buys an OpenAI client one last chunk, with no
/// choices and the turn's token counts. `prompt_tokens` is everything the model
/// read, caches included, because that is what the client bills against.
#[tokio::test]
async fn a_turn_ends_with_the_counts_the_cli_reported() {
    let chunks = chunks_for(&[
        claude_output::assistant_text("deux mots"),
        claude_output::result_with_usage(serde_json::json!({
            "input_tokens": 11,
            "output_tokens": 5,
            "cache_read_input_tokens": 100,
            "cache_creation_input_tokens": 4,
        })),
    ])
    .await;

    assert_eq!(
        sse::usages(&chunks),
        [(115, 5, 120)],
        "prompt_tokens = input + cache read + cache creation: {chunks:#?}"
    );
    let last = chunks.last().expect("a usage chunk");
    assert!(
        last.usage.is_some() && last.choices.is_empty(),
        "the usage chunk comes last and carries no choices: {last:#?}"
    );
    assert_eq!(sse::finish_reasons(&chunks), vec!["stop".to_string()]);
}

/// A `result` message flagged `is_error: true` is not a completed answer.
///
/// Was `#[ignore]`: the handler matched on `output.r#type` alone, so an error
/// result took the success arm — it emitted `finish_reason: "stop"` *and* set
/// `completed_normally`, which disarmed `SseDisconnectGuard` and left the CLI
/// session running.
#[tokio::test]
async fn a_failed_result_is_not_streamed_as_a_normal_stop() {
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
    assert_eq!(
        sse::errors(&chunks),
        vec!["boom".to_string()],
        "the client learns why, in band: {chunks:#?}"
    );
    assert_eq!(
        sse::text(&chunks),
        "partiel",
        "what was said is still delivered"
    );
}

/// A CLI output of type `error` used to fall into the `_ => {}` arm and produce
/// nothing: the client got a `[DONE]` it could not tell from a short answer.
///
/// Was `#[ignore]`.
#[tokio::test]
async fn an_error_output_reaches_the_client() {
    let chunks = chunks_for(&[claude_output::unknown("error")]).await;

    assert!(
        chunks.len() > 1,
        "the client must learn about the error, not just get the role chunk: {chunks:#?}"
    );
    assert_eq!(sse::errors(&chunks).len(), 1, "{chunks:#?}");
    assert!(
        sse::finish_reasons(&chunks).is_empty(),
        "an error frame is not a finish_reason"
    );
}

/// An output type nobody knows is still ignored — the `_ => {}` arm is for the
/// unknown, not for failures.
#[tokio::test]
async fn an_unknown_output_type_is_still_ignored() {
    let chunks = chunks_for(&[claude_output::unknown("telemetry")]).await;

    assert_eq!(chunks.len(), 1, "the role chunk alone: {chunks:#?}");
    assert!(sse::errors(&chunks).is_empty());
}
