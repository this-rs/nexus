//! Assertions for the gateway's `text/event-stream` responses.
//!
//! Two levels are available:
//!
//! * [`collect`] drives a `Stream<Item = ChatCompletionStreamResponse>` — what
//!   `api::streaming_handler::handle_enhanced_streaming_response` returns —
//!   without any HTTP at all.
//! * [`frames`] / [`chunks`] parse the body axum-test gives back for a real
//!   `POST /v1/chat/completions` with `"stream": true`, i.e. the output of
//!   `utils::streaming::create_sse_stream`.

use claude_code_api::models::openai::ChatCompletionStreamResponse;
use futures::Stream;
use futures::stream::StreamExt;

/// Drain a chunk stream into a vector.
pub async fn collect<S>(stream: S) -> Vec<ChatCompletionStreamResponse>
where
    S: Stream<Item = ChatCompletionStreamResponse>,
{
    stream.collect::<Vec<_>>().await
}

/// The payload of every `data:` line in an SSE body, in order.
///
/// `create_sse_stream` emits one `data:` line per chunk and no `event:` names.
/// Keep-alive comments (`:keep-alive`) are skipped.
pub fn frames(body: &str) -> Vec<String> {
    body.split("\n\n")
        .flat_map(|block| block.lines())
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|payload| payload.trim().to_string())
        .filter(|payload| !payload.is_empty())
        .collect()
}

/// The parsed chunks of an SSE body.
///
/// Panics with the offending frame if one cannot be deserialised, which is the
/// useful failure mode: a malformed chunk is a bug in the gateway, not in the test.
pub fn chunks(body: &str) -> Vec<ChatCompletionStreamResponse> {
    frames(body)
        .into_iter()
        .filter(|frame| frame != "[DONE]")
        .map(|frame| {
            serde_json::from_str(&frame)
                .unwrap_or_else(|e| panic!("SSE frame is not a chunk ({e}): {frame}"))
        })
        .collect()
}

/// Concatenate every `choices[].delta.content` of a chunk list.
///
/// `handle_enhanced_streaming_response` splits assistant text into 15-character
/// pieces, so this is how a test recovers the message that was streamed.
pub fn text(chunks: &[ChatCompletionStreamResponse]) -> String {
    chunks
        .iter()
        .flat_map(|chunk| chunk.choices.iter())
        .filter_map(|choice| choice.delta.content.as_deref())
        .collect()
}

/// The `finish_reason` values present in a chunk list, in order.
pub fn finish_reasons(chunks: &[ChatCompletionStreamResponse]) -> Vec<String> {
    chunks
        .iter()
        .flat_map(|chunk| chunk.choices.iter())
        .filter_map(|choice| choice.finish_reason.clone())
        .collect()
}

/// `(name, arguments)` for every streamed tool-call delta.
pub fn tool_calls(chunks: &[ChatCompletionStreamResponse]) -> Vec<(String, String)> {
    chunks
        .iter()
        .flat_map(|chunk| chunk.choices.iter())
        .filter_map(|choice| choice.delta.tool_calls.as_ref())
        .flatten()
        .filter_map(|call| {
            let function = call.function.as_ref()?;
            Some((
                function.name.clone().unwrap_or_default(),
                function.arguments.clone().unwrap_or_default(),
            ))
        })
        .collect()
}
