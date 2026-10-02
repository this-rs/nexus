//! `text/event-stream` plumbing for the OpenAI-compatible streaming endpoint.

use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::{Stream, StreamExt};
use serde::Serialize;
use std::convert::Infallible;
use std::time::Duration;
use tracing::error;

/// Wrap `stream` in an OpenAI-compatible SSE response.
///
/// Each item becomes one `data:` line, and the body is **terminated by
/// [`create_done_event`]**, i.e. `data: [DONE]`. That sentinel is not decorative:
/// openai-python, openai-node and LangChain all stop reading on it, so a stream
/// that merely ends after its last `finish_reason` chunk leaves them waiting for
/// the socket to close or reporting a truncated response.
pub fn create_sse_stream<S, T>(stream: S) -> Sse<impl Stream<Item = Result<Event, Infallible>>>
where
    S: Stream<Item = T> + Send + 'static,
    T: Serialize,
{
    let event_stream = stream
        .map(|data| Ok(data_event(&data)))
        .chain(futures::stream::once(async { Ok(create_done_event()) }));

    Sse::new(event_stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(30))
            .text("keep-alive"),
    )
}

/// One `data:` frame carrying `data` as JSON.
///
/// A serialisation failure used to be swallowed by `unwrap_or_default()`, which
/// emitted a frame with an **empty** payload — indistinguishable, to a client,
/// from a chunk it should ignore, so a dropped chunk left a silent hole in the
/// answer. It is now logged and reported in band as an OpenAI-shaped error
/// object, which a client can actually surface.
fn data_event<T: Serialize>(data: &T) -> Event {
    match serde_json::to_string(data) {
        Ok(json) => Event::default().data(json),
        Err(e) => {
            error!("SSE chunk could not be serialised, reporting it in band: {e}");
            let payload = serde_json::json!({
                "error": {
                    "message": format!("failed to serialize stream chunk: {e}"),
                    "type": "internal_error",
                    "param": null,
                    "code": "chunk_serialization_failed",
                }
            });
            Event::default().data(payload.to_string())
        },
    }
}

/// The `data: [DONE]` sentinel that closes an OpenAI-compatible SSE stream.
pub fn create_done_event() -> Event {
    Event::default().data("[DONE]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;
    use std::collections::BTreeMap;

    /// Render `create_sse_stream`'s response body exactly as a client would read it.
    async fn body_of<S, T>(stream: S) -> String
    where
        S: Stream<Item = T> + Send + 'static,
        T: Serialize + 'static,
    {
        let response = create_sse_stream(stream).into_response();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("the SSE body must terminate")
            .to_bytes();
        String::from_utf8(bytes.to_vec()).expect("SSE bodies are UTF-8")
    }

    /// The payload of every `data:` line, in order.
    fn frames(body: &str) -> Vec<String> {
        body.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|payload| payload.trim().to_string())
            .collect()
    }

    #[tokio::test]
    async fn stream_ends_with_the_done_sentinel() {
        let body = body_of(futures::stream::iter(vec![
            serde_json::json!({"n": 1}),
            serde_json::json!({"n": 2}),
        ]))
        .await;

        assert_eq!(
            frames(&body),
            vec![r#"{"n":1}"#, r#"{"n":2}"#, "[DONE]"],
            "every chunk, then the sentinel, in order"
        );
    }

    #[tokio::test]
    async fn each_item_is_one_data_frame_terminated_by_a_blank_line() {
        let body = body_of(futures::stream::iter(vec![serde_json::json!({"a": "b"})])).await;

        assert_eq!(
            body, "data: {\"a\":\"b\"}\n\ndata: [DONE]\n\n",
            "SSE frames are `data: …` separated by blank lines"
        );
    }

    #[tokio::test]
    async fn an_empty_stream_still_emits_the_sentinel() {
        let body = body_of(futures::stream::iter(Vec::<serde_json::Value>::new())).await;

        assert_eq!(
            frames(&body),
            vec!["[DONE]"],
            "a client must be told the empty stream is over"
        );
    }

    /// A value `serde_json` refuses: a JSON object's keys must be strings, and a
    /// tuple key is not one. It is a plain `std` type on purpose — a fixture
    /// declared inside `mod tests` would put `tests` in the monomorphised symbol
    /// of `data_event`, and `scripts/coverage_logic_only.py` would then score this
    /// file's production lines as test code.
    fn unserialisable() -> BTreeMap<(u8, u8), u8> {
        BTreeMap::from([((1, 2), 3)])
    }

    #[tokio::test]
    async fn a_chunk_that_cannot_be_serialised_is_reported_not_blanked() {
        // Sanity: the fixture really does fail, so the test cannot pass vacuously.
        assert!(serde_json::to_string(&unserialisable()).is_err());

        let body = body_of(futures::stream::iter(vec![unserialisable()])).await;
        let frames = frames(&body);

        assert_eq!(
            frames.len(),
            2,
            "the failed chunk plus the sentinel: {frames:?}"
        );
        assert_eq!(frames[1], "[DONE]");

        let reported: serde_json::Value =
            serde_json::from_str(&frames[0]).expect("the failure must itself be valid JSON");
        assert_eq!(reported["error"]["type"], "internal_error");
        assert_eq!(reported["error"]["code"], "chunk_serialization_failed");
        assert!(
            reported["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("key must be a string"),
            "the client needs the cause, not an empty frame: {reported}"
        );
    }
}
