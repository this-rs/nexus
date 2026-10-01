use axum::{
    Json,
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::time::Instant;
use tracing::{error, warn};

use crate::models::error::{ErrorDetail, ErrorResponse};

pub async fn handle_errors(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let path = req.uri().path().to_string();
    let method = req.method().to_string();

    let response = next.run(req).await;

    let elapsed = start.elapsed();
    let status = response.status();

    if status.is_server_error() {
        error!(
            "Server error: {} {} - Status: {} - Duration: {:?}",
            method, path, status, elapsed
        );
    } else if status.is_client_error() && status != StatusCode::NOT_FOUND {
        warn!(
            "Client error: {} {} - Status: {} - Duration: {:?}",
            method, path, status, elapsed
        );
    }

    response
}

/// The human-readable payload of a caught panic.
///
/// Extracted from [`handle_panic`] so each `downcast` arm can be asserted
/// directly: the HTTP response deliberately carries none of this, so the only
/// other evidence of which arm ran is a log line.
fn panic_details(err: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = err.downcast_ref::<&str>() {
        s.to_string()
    } else {
        "Unknown panic".to_string()
    }
}

/// Turn a caught panic into a `500` with an OpenAI-shaped error body.
///
/// **Not mounted.** The gateway's middleware stack (see
/// [`crate::build_router`]) has no `CatchPanicLayer`, so nothing ever calls this:
/// a panicking handler still tears the connection down without a response.
#[allow(dead_code)]
pub async fn handle_panic(err: Box<dyn std::any::Any + Send + 'static>) -> Response {
    let details = panic_details(&*err);

    error!("Panic occurred: {}", details);

    let error_response = ErrorResponse {
        error: ErrorDetail {
            message: "Internal server error".to_string(),
            r#type: "internal_error".to_string(),
            param: None,
            code: Some("panic".to_string()),
        },
    };

    (StatusCode::INTERNAL_SERVER_ERROR, Json(error_response)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::Request, routing::get};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("a middleware response body is already in memory")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("the error body must be JSON")
    }

    /// A router whose single route answers with `status` and the body `"payload"`,
    /// wrapped in the real [`handle_errors`] middleware.
    fn app_answering(status: StatusCode) -> Router {
        Router::new()
            .route("/probe", get(move || async move { (status, "payload") }))
            .layer(axum::middleware::from_fn(handle_errors))
    }

    async fn probe(status: StatusCode) -> (StatusCode, String) {
        let response = app_answering(status)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .body(Body::empty())
                    .expect("a GET request with no body"),
            )
            .await
            .expect("the middleware stack is infallible");
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    // ── handle_errors: a logger, and nothing else ──

    /// The middleware's whole contract: whatever the branch it takes to log, it
    /// must hand the inner response back untouched.
    #[tokio::test]
    async fn handle_errors_is_transparent_on_every_logging_branch() {
        for status in [
            StatusCode::OK,                    // no log
            StatusCode::NOT_FOUND,             // deliberately not logged
            StatusCode::BAD_REQUEST,           // warn! branch
            StatusCode::TOO_MANY_REQUESTS,     // warn! branch
            StatusCode::INTERNAL_SERVER_ERROR, // error! branch
            StatusCode::SERVICE_UNAVAILABLE,   // error! branch
        ] {
            assert_eq!(
                probe(status).await,
                (status, "payload".to_string()),
                "handle_errors must not alter a {status} response"
            );
        }
    }

    /// `404` is singled out in the condition, so pin that: it is a client error
    /// and it still must not be rewritten.
    #[tokio::test]
    async fn a_404_passes_through_like_any_other_status() {
        let (status, body) = probe(StatusCode::NOT_FOUND).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, "payload");
    }

    // ── panic_details: one assertion per downcast arm ──

    #[test]
    fn panic_details_reads_a_string_payload() {
        let err: Box<dyn std::any::Any + Send> = Box::new("panicked with a String".to_string());
        assert_eq!(panic_details(&*err), "panicked with a String");
    }

    #[test]
    fn panic_details_reads_a_static_str_payload() {
        // `panic!("literal")` produces a `&'static str`, not a `String`.
        let err: Box<dyn std::any::Any + Send> = Box::new("panicked with a literal");
        assert_eq!(panic_details(&*err), "panicked with a literal");
    }

    #[test]
    fn panic_details_falls_back_for_an_arbitrary_payload() {
        // `panic_any(42)` — neither arm matches, and the detail is simply lost.
        let err: Box<dyn std::any::Any + Send> = Box::new(42_u32);
        assert_eq!(panic_details(&*err), "Unknown panic");
    }

    /// Why the two arms exist: `std` hands a `&'static str` for `panic!("lit")`
    /// and a `String` for `panic!("{}", x)`. Both panics below are caught, so the
    /// "panicked at" lines they print to stderr are expected noise.
    #[test]
    fn panic_details_is_the_real_payload_of_a_caught_panic() {
        let caught = std::panic::catch_unwind(|| {
            panic!("from a literal");
        });
        let err = caught.expect_err("the closure panicked");
        assert_eq!(panic_details(&*err), "from a literal");

        let caught = std::panic::catch_unwind(|| {
            panic!("formatted {}", 7);
        });
        let err = caught.expect_err("the closure panicked");
        assert_eq!(panic_details(&*err), "formatted 7");
    }

    // ── handle_panic: the response ──

    #[tokio::test]
    async fn handle_panic_answers_500_with_an_openai_error_body() {
        let response = handle_panic(Box::new("boom".to_string())).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        assert_eq!(
            body_json(response).await,
            serde_json::json!({
                "error": {
                    "message": "Internal server error",
                    "type": "internal_error",
                    "param": null,
                    "code": "panic",
                }
            })
        );
    }

    /// The panic message can name internals, so it must stay in the log: the body
    /// is the same fixed text for every payload shape.
    #[tokio::test]
    async fn handle_panic_never_leaks_the_panic_message_to_the_client() {
        let payloads: Vec<Box<dyn std::any::Any + Send>> = vec![
            Box::new("assertion failed: /srv/secret/path".to_string()),
            Box::new("index out of bounds: the len is 0"),
            Box::new(1_i64),
        ];

        for payload in payloads {
            let body = body_json(handle_panic(payload).await).await;
            assert_eq!(body["error"]["message"], "Internal server error");
            assert_eq!(body["error"]["code"], "panic");
        }
    }
}
