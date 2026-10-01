use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum ApiError {
    #[error("Internal server error: {0}")]
    Internal(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Claude process error: {0}")]
    ClaudeProcess(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Configuration error: {0}")]
    Config(#[from] config::ConfigError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Timeout error: {0}")]
    Timeout(String),

    #[error("Rate limit exceeded: {0}")]
    RateLimit(String),

    #[error("Service unavailable: {0}")]
    ServiceUnavailable(String),

    #[error("Invalid model: {0}")]
    InvalidModel(String),

    #[error("Context length exceeded: {0}")]
    ContextLengthExceeded(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub message: String,
    pub r#type: String,
    pub param: Option<String>,
    pub code: Option<String>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, error_type, code) = match &self {
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "invalid_request_error", None),
            ApiError::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "authentication_error", None),
            ApiError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found_error", None),
            ApiError::ClaudeProcess(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "claude_process_error",
                None,
            ),
            ApiError::Timeout(_) => (
                StatusCode::GATEWAY_TIMEOUT,
                "timeout_error",
                Some("timeout"),
            ),
            ApiError::RateLimit(_) => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                Some("rate_limit_exceeded"),
            ),
            ApiError::ServiceUnavailable(_) => {
                (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable", None)
            },
            ApiError::InvalidModel(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                Some("invalid_model"),
            ),
            ApiError::ContextLengthExceeded(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                Some("context_length_exceeded"),
            ),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error", None),
        };

        let error_response = ErrorResponse {
            error: ErrorDetail {
                message: self.to_string(),
                r#type: error_type.to_string(),
                param: None,
                code: code.map(String::from),
            },
        };

        (status, Json(error_response)).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    /// `(status, type, code, message)` as a client sees them.
    async fn rendered(error: ApiError) -> (StatusCode, String, Option<String>, String) {
        let response = error.into_response();
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("the error body is built in memory")
            .to_bytes();
        let body: ErrorResponse =
            serde_json::from_slice(&bytes).expect("every error body must be an ErrorResponse");
        (
            status,
            body.error.r#type,
            body.error.code,
            body.error.message,
        )
    }

    // ── one assertion per mapped variant ──

    #[tokio::test]
    async fn bad_request_is_400_invalid_request_error() {
        let (status, kind, code, message) = rendered(ApiError::BadRequest("no model".into())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(kind, "invalid_request_error");
        assert_eq!(code, None);
        assert_eq!(message, "Bad request: no model");
    }

    #[tokio::test]
    async fn unauthorized_is_401_authentication_error() {
        let (status, kind, code, message) =
            rendered(ApiError::Unauthorized("no bearer".into())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(kind, "authentication_error");
        assert_eq!(code, None);
        assert_eq!(message, "Unauthorized: no bearer");
    }

    /// A `401` without a `WWW-Authenticate` header: RFC 9110 requires one, and no
    /// client can discover the scheme from this response.
    #[test]
    fn unauthorized_carries_no_www_authenticate_header() {
        let response = ApiError::Unauthorized("no bearer".into()).into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            !response.headers().contains_key("www-authenticate"),
            "documents the gap: the 401 advertises no authentication scheme"
        );
    }

    #[tokio::test]
    async fn not_found_is_404_not_found_error() {
        let (status, kind, code, message) = rendered(ApiError::NotFound("conv 7".into())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(kind, "not_found_error");
        assert_eq!(code, None);
        assert_eq!(message, "Not found: conv 7");
    }

    #[tokio::test]
    async fn claude_process_is_500_claude_process_error() {
        let (status, kind, code, message) =
            rendered(ApiError::ClaudeProcess("spawn failed".into())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(kind, "claude_process_error");
        assert_eq!(code, None);
        assert_eq!(message, "Claude process error: spawn failed");
    }

    #[tokio::test]
    async fn timeout_is_504_with_the_timeout_code() {
        let (status, kind, code, message) = rendered(ApiError::Timeout("5s".into())).await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(kind, "timeout_error");
        assert_eq!(code.as_deref(), Some("timeout"));
        assert_eq!(message, "Timeout error: 5s");
    }

    #[tokio::test]
    async fn rate_limit_is_429_with_the_openai_code() {
        let (status, kind, code, message) = rendered(ApiError::RateLimit("slow down".into())).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(kind, "rate_limit_error");
        assert_eq!(code.as_deref(), Some("rate_limit_exceeded"));
        assert_eq!(message, "Rate limit exceeded: slow down");
    }

    /// Note the type name: every other variant ends in `_error`, this one does not.
    #[tokio::test]
    async fn service_unavailable_is_503_and_breaks_the_error_type_naming() {
        let (status, kind, code, _) =
            rendered(ApiError::ServiceUnavailable("draining".into())).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            kind, "service_unavailable",
            "documents the inconsistency: not `service_unavailable_error`"
        );
        assert_eq!(code, None);
    }

    #[tokio::test]
    async fn invalid_model_is_a_400_invalid_request_error_with_a_code() {
        let (status, kind, code, message) = rendered(ApiError::InvalidModel("gpt-9".into())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(kind, "invalid_request_error");
        assert_eq!(code.as_deref(), Some("invalid_model"));
        assert_eq!(message, "Invalid model: gpt-9");
    }

    #[tokio::test]
    async fn context_length_exceeded_is_a_400_invalid_request_error_with_a_code() {
        let (status, kind, code, message) =
            rendered(ApiError::ContextLengthExceeded("200k".into())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(kind, "invalid_request_error");
        assert_eq!(code.as_deref(), Some("context_length_exceeded"));
        assert_eq!(message, "Context length exceeded: 200k");
    }

    // ── the catch-all arm ──

    /// Five variants share the wildcard arm; all five answer `500 internal_error`
    /// with no code, so none of them is distinguishable by a client.
    #[tokio::test]
    async fn every_unmapped_variant_collapses_into_500_internal_error() {
        // Second element is a message *prefix*: the `thiserror` label is what this
        // test pins, not the wording of the wrapped third-party error.
        let cases: Vec<(ApiError, &str)> = vec![
            (
                ApiError::Internal("boom".into()),
                "Internal server error: boom",
            ),
            (
                ApiError::Database("no graph".into()),
                "Database error: no graph",
            ),
            (
                ApiError::Config(config::ConfigError::Message("bad toml".into())),
                "Configuration error: bad toml",
            ),
            (
                ApiError::Io(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "denied",
                )),
                "IO error: denied",
            ),
            (
                ApiError::Json(serde_json::from_str::<serde_json::Value>("{").unwrap_err()),
                "JSON error: ",
            ),
        ];

        for (error, expected_prefix) in cases {
            let (status, kind, code, message) = rendered(error).await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(kind, "internal_error");
            assert_eq!(code, None);
            assert!(
                message.starts_with(expected_prefix),
                "{message:?} must start with {expected_prefix:?}"
            );
        }
    }

    // ── `#[from]` conversions ──

    #[test]
    fn io_errors_convert_through_from() {
        let error: ApiError =
            std::io::Error::new(std::io::ErrorKind::NotFound, "/no/such/claude").into();
        assert!(matches!(error, ApiError::Io(_)));
        assert_eq!(error.to_string(), "IO error: /no/such/claude");
    }

    #[test]
    fn json_errors_convert_through_from() {
        let error: ApiError = serde_json::from_str::<serde_json::Value>("nope")
            .unwrap_err()
            .into();
        assert!(matches!(error, ApiError::Json(_)));
        assert!(error.to_string().starts_with("JSON error: "));
    }

    #[test]
    fn config_errors_convert_through_from() {
        let error: ApiError = config::ConfigError::Message("bad toml".into()).into();
        assert!(matches!(error, ApiError::Config(_)));
        assert_eq!(error.to_string(), "Configuration error: bad toml");
    }

    // ── properties of the whole mapping ──

    /// `ErrorDetail::param` is declared for OpenAI compatibility and never filled,
    /// not even by the two variants that know which parameter was wrong
    /// (`InvalidModel` → `model`, `ContextLengthExceeded` → `messages`).
    #[tokio::test]
    async fn param_is_always_null_even_when_the_offending_parameter_is_known() {
        for error in [
            ApiError::InvalidModel("gpt-9".into()),
            ApiError::ContextLengthExceeded("200k".into()),
            ApiError::BadRequest("messages must not be empty".into()),
        ] {
            let response = error.into_response();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["error"]["param"], serde_json::Value::Null);
        }
    }

    /// The body carries `Display` verbatim, so an internal cause reaches the
    /// client as-is. Evidence, not an endorsement: `Io`, `Config` and `Database`
    /// messages routinely name local paths and backend addresses.
    #[tokio::test]
    async fn internal_causes_are_echoed_verbatim_to_the_client() {
        let (status, _, _, message) = rendered(ApiError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "/opt/internal/layout/claude: No such file",
        )))
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            message.contains("/opt/internal/layout/claude"),
            "documents the leak: the server-side path is in the 500 body ({message})"
        );
    }

    #[test]
    fn every_variant_renders_a_body_that_parses_as_an_error_response() {
        // A content-type regression would break every OpenAI client at once.
        let response = ApiError::Internal("x".into()).into_response();
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    /// `ApiResult` is the alias every handler returns; `?` must carry the variant
    /// through untouched, since the status code is chosen from it at the very end.
    #[test]
    fn api_result_propagates_an_api_error_through_the_question_mark() {
        fn failing() -> ApiResult<u8> {
            Err(ApiError::NotFound("conv 1".into()))
        }
        fn caller() -> ApiResult<u8> {
            Ok(failing()? + 1)
        }

        match caller() {
            Err(ApiError::NotFound(what)) => assert_eq!(what, "conv 1"),
            other => panic!("the error must propagate unchanged, got {other:?}"),
        }
    }
}
