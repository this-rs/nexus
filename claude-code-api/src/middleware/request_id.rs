use axum::{extract::Request, http::HeaderName, middleware::Next, response::Response};
use uuid::Uuid;

pub static X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

pub async fn add_request_id(mut req: Request, next: Next) -> Response {
    let request_id = req
        .headers()
        .get(&X_REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    req.headers_mut()
        .insert(X_REQUEST_ID.clone(), request_id.parse().unwrap());

    let mut response = next.run(req).await;

    response
        .headers_mut()
        .insert(X_REQUEST_ID.clone(), request_id.parse().unwrap());

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{HeaderValue, Request, StatusCode, header::HeaderMap},
        routing::get,
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// The id the *handler* saw, echoed as the body, plus the whole response.
    ///
    /// Both halves matter: `add_request_id` rewrites the request headers on the way
    /// in and the response headers on the way out, and a test that only looks at
    /// the response cannot tell whether the handler got the same value.
    async fn round_trip(request: Request<Body>) -> (StatusCode, String, Option<String>) {
        async fn echo(headers: HeaderMap) -> String {
            headers
                .get("x-request-id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<absent>")
                .to_string()
        }

        let app = Router::new()
            .route("/probe", get(echo))
            .layer(axum::middleware::from_fn(add_request_id));

        let response = app.oneshot(request).await.expect("infallible stack");
        let status = response.status();
        let seen_by_response = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            String::from_utf8(bytes.to_vec()).unwrap(),
            seen_by_response,
        )
    }

    fn probe() -> axum::http::request::Builder {
        Request::builder().uri("/probe")
    }

    #[test]
    fn the_header_name_is_the_lowercase_conventional_one() {
        assert_eq!(X_REQUEST_ID.as_str(), "x-request-id");
    }

    #[tokio::test]
    async fn a_request_without_an_id_gets_a_fresh_uuid_v4() {
        let (status, seen_by_handler, seen_by_client) =
            round_trip(probe().body(Body::empty()).unwrap()).await;

        assert_eq!(status, StatusCode::OK);
        let parsed = Uuid::parse_str(&seen_by_handler)
            .expect("the generated id must be a UUID, not an opaque string");
        assert_eq!(
            parsed.get_version_num(),
            4,
            "v4, as documented by Uuid::new_v4"
        );
        assert_eq!(
            seen_by_client.as_deref(),
            Some(seen_by_handler.as_str()),
            "the handler and the client must see the same id"
        );
    }

    #[tokio::test]
    async fn two_requests_without_an_id_get_different_ids() {
        let (_, first, _) = round_trip(probe().body(Body::empty()).unwrap()).await;
        let (_, second, _) = round_trip(probe().body(Body::empty()).unwrap()).await;
        assert_ne!(first, second, "ids must not be reused across requests");
    }

    #[tokio::test]
    async fn a_client_supplied_id_is_propagated_and_echoed_verbatim() {
        let (_, seen_by_handler, seen_by_client) = round_trip(
            probe()
                .header("x-request-id", "trace-abc-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(seen_by_handler, "trace-abc-123");
        assert_eq!(seen_by_client.as_deref(), Some("trace-abc-123"));
    }

    /// The echo is unconditional: no length cap, no charset check beyond "visible
    /// ASCII", no rejection. Anything a client sends comes back in the response
    /// headers and goes into the logs under the request-id field.
    #[tokio::test]
    async fn a_client_supplied_id_is_neither_bounded_nor_sanitised() {
        let hostile = "a".repeat(4096);
        let (_, seen_by_handler, seen_by_client) = round_trip(
            probe()
                .header("x-request-id", hostile.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(seen_by_handler.len(), 4096);
        assert_eq!(seen_by_client.as_deref(), Some(hostile.as_str()));
    }

    /// `to_str()` rejects non-ASCII bytes, so the `unwrap_or_else` fallback runs
    /// and the junk never reaches the handler — which is also the reason the two
    /// `parse().unwrap()` calls below cannot panic: the value they parse is either
    /// a UUID or a string `to_str()` already proved to be header-legal.
    #[tokio::test]
    async fn a_non_ascii_id_is_discarded_in_favour_of_a_generated_one() {
        let junk = HeaderValue::from_bytes(&[0xC3, 0xA9, 0xFF]).expect("a legal header value");
        let (_, seen_by_handler, seen_by_client) = round_trip(
            probe()
                .header("x-request-id", junk)
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert!(
            Uuid::parse_str(&seen_by_handler).is_ok(),
            "the unreadable id must be replaced, not forwarded: {seen_by_handler:?}"
        );
        assert_eq!(seen_by_client.as_deref(), Some(seen_by_handler.as_str()));
    }

    /// A horizontal tab passes `to_str()` *and* `HeaderValue::from_str`, so it
    /// survives the round trip instead of panicking on the `parse().unwrap()`.
    #[tokio::test]
    async fn a_tab_inside_the_id_survives_instead_of_panicking() {
        let tabbed = HeaderValue::from_bytes(b"left\tright").expect("a legal header value");
        let (status, seen_by_handler, seen_by_client) = round_trip(
            probe()
                .header("x-request-id", tabbed)
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(seen_by_handler, "left\tright");
        assert_eq!(seen_by_client.as_deref(), Some("left\tright"));
    }

    /// Only the first value is read; a duplicated header is silently collapsed and
    /// the extra values are dropped from the response.
    #[tokio::test]
    async fn only_the_first_of_several_ids_is_kept() {
        let (_, seen_by_handler, seen_by_client) = round_trip(
            probe()
                .header("x-request-id", "first")
                .header("x-request-id", "second")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(seen_by_handler, "first");
        assert_eq!(seen_by_client.as_deref(), Some("first"));
    }

    /// An empty value is header-legal, so it is kept as the request id rather than
    /// being treated as absent — the response advertises an id of `""`.
    #[tokio::test]
    async fn an_empty_id_is_kept_rather_than_regenerated() {
        let (_, seen_by_handler, seen_by_client) = round_trip(
            probe()
                .header("x-request-id", "")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(seen_by_handler, "");
        assert_eq!(
            seen_by_client.as_deref(),
            Some(""),
            "documents the gap: an empty id is not replaced by a generated one"
        );
    }
}
