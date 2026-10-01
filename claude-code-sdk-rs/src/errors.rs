//! Error types for the Claude Code SDK
//!
//! This module defines all error types that can occur when using the SDK.
//! The errors are designed to be informative and actionable, helping users
//! understand what went wrong and how to fix it.

use thiserror::Error;

/// Main error type for the Claude Code SDK
#[derive(Error, Debug)]
pub enum SdkError {
    /// Claude CLI executable was not found
    #[error(
        "Claude CLI not found. Install with: npm install -g @anthropic-ai/claude-code\n\nSearched in:\n{searched_paths}"
    )]
    CliNotFound {
        /// Paths that were searched for the CLI
        searched_paths: String,
    },

    /// Failed to connect to Claude CLI
    #[error("Failed to connect to Claude CLI: {0}")]
    ConnectionError(String),

    /// Process-related errors
    #[error("Process error: {0}")]
    ProcessError(#[from] std::io::Error),

    /// Failed to parse a message
    #[error("Failed to parse message: {error}\nRaw message: {raw}")]
    MessageParseError {
        /// Parse error description
        error: String,
        /// Raw message that failed to parse
        raw: String,
    },

    /// JSON serialization/deserialization errors
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    /// CLI JSON decode error
    #[error("Failed to decode JSON from CLI output: {line}")]
    CliJsonDecodeError {
        /// Line that failed to decode
        line: String,
        /// Original error
        #[source]
        original_error: serde_json::Error,
    },

    /// Transport layer errors
    #[error("Transport error: {0}")]
    TransportError(String),

    /// Timeout waiting for response
    #[error("Timeout waiting for response after {seconds} seconds")]
    Timeout {
        /// Number of seconds waited before timeout
        seconds: u64,
    },

    /// Session not found
    #[error("Session not found: {0}")]
    SessionNotFound(String),

    /// Invalid configuration
    #[error("Invalid configuration: {0}")]
    ConfigError(String),

    /// Control request failed
    #[error("Control request failed: {0}")]
    ControlRequestError(String),

    /// Unexpected response type
    #[error("Unexpected response type: expected {expected}, got {actual}")]
    UnexpectedResponse {
        /// Expected response type
        expected: String,
        /// Actual response type received
        actual: String,
    },

    /// CLI returned an error
    #[error("Claude CLI error: {message}")]
    CliError {
        /// Error message from CLI
        message: String,
        /// Error code if available
        code: Option<String>,
    },

    /// Channel send error
    #[error("Failed to send message through channel")]
    ChannelSendError,

    /// Channel receive error
    #[error("Channel closed unexpectedly")]
    ChannelClosed,

    /// Invalid state transition
    #[error("Invalid state: {message}")]
    InvalidState {
        /// Description of the invalid state
        message: String,
    },

    /// Process exited unexpectedly
    #[error("Claude process exited unexpectedly with code {code:?}")]
    ProcessExited {
        /// Exit code if available
        code: Option<i32>,
    },

    /// Stream ended unexpectedly
    #[error("Stream ended unexpectedly")]
    UnexpectedStreamEnd,

    /// Feature not supported
    #[error("Feature not supported: {feature}")]
    NotSupported {
        /// Description of unsupported feature
        feature: String,
    },
}

/// Result type alias for SDK operations
pub type Result<T> = std::result::Result<T, SdkError>;

impl SdkError {
    /// Create a new MessageParseError
    pub fn parse_error(error: impl Into<String>, raw: impl Into<String>) -> Self {
        Self::MessageParseError {
            error: error.into(),
            raw: raw.into(),
        }
    }

    /// Create a new Timeout error
    pub fn timeout(seconds: u64) -> Self {
        Self::Timeout { seconds }
    }

    /// Create a new UnexpectedResponse error
    pub fn unexpected_response(expected: impl Into<String>, actual: impl Into<String>) -> Self {
        Self::UnexpectedResponse {
            expected: expected.into(),
            actual: actual.into(),
        }
    }

    /// Create a new CliError
    pub fn cli_error(message: impl Into<String>, code: Option<String>) -> Self {
        Self::CliError {
            message: message.into(),
            code,
        }
    }

    /// Create a new InvalidState error
    pub fn invalid_state(message: impl Into<String>) -> Self {
        Self::InvalidState {
            message: message.into(),
        }
    }

    /// Check if the error is recoverable
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            Self::Timeout { .. }
                | Self::ChannelClosed
                | Self::UnexpectedStreamEnd
                | Self::ProcessExited { .. }
        )
    }

    /// Check if the error is a configuration issue
    pub fn is_config_error(&self) -> bool {
        matches!(
            self,
            Self::CliNotFound { .. } | Self::ConfigError(_) | Self::NotSupported { .. }
        )
    }
}

// Implement From for common channel errors
impl<T> From<tokio::sync::mpsc::error::SendError<T>> for SdkError {
    fn from(_: tokio::sync::mpsc::error::SendError<T>) -> Self {
        Self::ChannelSendError
    }
}

impl From<tokio::sync::broadcast::error::RecvError> for SdkError {
    fn from(_: tokio::sync::broadcast::error::RecvError) -> Self {
        Self::ChannelClosed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = SdkError::CliNotFound {
            searched_paths: "/usr/local/bin\n/usr/bin".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("npm install -g @anthropic-ai/claude-code"));
        assert!(msg.contains("/usr/local/bin"));
    }

    #[test]
    fn test_is_recoverable() {
        assert!(SdkError::timeout(30).is_recoverable());
        assert!(SdkError::ChannelClosed.is_recoverable());
        assert!(!SdkError::ConfigError("test".into()).is_recoverable());
    }

    #[test]
    fn test_is_config_error() {
        assert!(SdkError::ConfigError("test".into()).is_config_error());
        assert!(
            SdkError::CliNotFound {
                searched_paths: "test".into()
            }
            .is_config_error()
        );
        assert!(!SdkError::timeout(30).is_config_error());
    }

    #[test]
    fn test_cli_json_decode_error() {
        let line = r#"{"invalid": json"#.to_string();
        let original_err = serde_json::from_str::<serde_json::Value>(&line).unwrap_err();

        let error = SdkError::CliJsonDecodeError {
            line: line.clone(),
            original_error: original_err,
        };

        let error_str = error.to_string();
        assert!(error_str.contains("Failed to decode JSON from CLI output"));
        assert!(error_str.contains(&line));
    }

    #[test]
    fn test_parse_error_constructor() {
        let err = SdkError::parse_error("bad json", r#"{"broken"#);
        match &err {
            SdkError::MessageParseError { error, raw } => {
                assert_eq!(error, "bad json");
                assert_eq!(raw, r#"{"broken"#);
            },
            _ => panic!("expected MessageParseError"),
        }
        let msg = err.to_string();
        assert!(msg.contains("bad json"));
        assert!(msg.contains(r#"{"broken"#));
    }

    #[test]
    fn test_timeout_constructor() {
        let err = SdkError::timeout(60);
        match &err {
            SdkError::Timeout { seconds } => assert_eq!(*seconds, 60),
            _ => panic!("expected Timeout"),
        }
        assert!(err.to_string().contains("60"));
    }

    #[test]
    fn test_unexpected_response_constructor() {
        let err = SdkError::unexpected_response("text", "json");
        match &err {
            SdkError::UnexpectedResponse { expected, actual } => {
                assert_eq!(expected, "text");
                assert_eq!(actual, "json");
            },
            _ => panic!("expected UnexpectedResponse"),
        }
    }

    #[test]
    fn test_cli_error_constructor_with_code() {
        let err = SdkError::cli_error("something broke", Some("E001".into()));
        match &err {
            SdkError::CliError { message, code } => {
                assert_eq!(message, "something broke");
                assert_eq!(code.as_deref(), Some("E001"));
            },
            _ => panic!("expected CliError"),
        }
    }

    #[test]
    fn test_cli_error_constructor_without_code() {
        let err = SdkError::cli_error("no code", None);
        match &err {
            SdkError::CliError { message, code } => {
                assert_eq!(message, "no code");
                assert!(code.is_none());
            },
            _ => panic!("expected CliError"),
        }
    }

    #[test]
    fn test_invalid_state_constructor() {
        let err = SdkError::invalid_state("bad state");
        match &err {
            SdkError::InvalidState { message } => assert_eq!(message, "bad state"),
            _ => panic!("expected InvalidState"),
        }
    }

    #[test]
    fn test_is_recoverable_for_all_recoverable_variants() {
        assert!(SdkError::timeout(10).is_recoverable());
        assert!(SdkError::ChannelClosed.is_recoverable());
        assert!(SdkError::UnexpectedStreamEnd.is_recoverable());
        assert!(SdkError::ProcessExited { code: Some(1) }.is_recoverable());
        assert!(SdkError::ProcessExited { code: None }.is_recoverable());
    }

    #[test]
    fn test_is_recoverable_returns_false_for_non_recoverable() {
        assert!(!SdkError::ConnectionError("err".into()).is_recoverable());
        assert!(!SdkError::TransportError("err".into()).is_recoverable());
        assert!(!SdkError::ConfigError("err".into()).is_recoverable());
        assert!(!SdkError::ChannelSendError.is_recoverable());
        assert!(!SdkError::invalid_state("x").is_recoverable());
        assert!(
            !SdkError::NotSupported {
                feature: "x".into()
            }
            .is_recoverable()
        );
        assert!(!SdkError::parse_error("e", "r").is_recoverable());
        assert!(!SdkError::unexpected_response("a", "b").is_recoverable());
        assert!(!SdkError::cli_error("m", None).is_recoverable());
    }

    #[test]
    fn test_is_config_error_for_not_supported() {
        assert!(
            SdkError::NotSupported {
                feature: "streaming".into()
            }
            .is_config_error()
        );
    }

    #[test]
    fn test_display_connection_error() {
        let err = SdkError::ConnectionError("refused".into());
        assert_eq!(err.to_string(), "Failed to connect to Claude CLI: refused");
    }

    #[test]
    fn test_display_transport_error() {
        let err = SdkError::TransportError("broken pipe".into());
        assert_eq!(err.to_string(), "Transport error: broken pipe");
    }

    #[test]
    fn test_display_session_not_found() {
        let err = SdkError::SessionNotFound("abc-123".into());
        assert_eq!(err.to_string(), "Session not found: abc-123");
    }

    #[test]
    fn test_display_control_request_error() {
        let err = SdkError::ControlRequestError("denied".into());
        assert_eq!(err.to_string(), "Control request failed: denied");
    }

    #[test]
    fn test_display_invalid_state() {
        let err = SdkError::invalid_state("not ready");
        assert_eq!(err.to_string(), "Invalid state: not ready");
    }

    #[test]
    fn test_display_process_exited() {
        let err = SdkError::ProcessExited { code: Some(1) };
        assert!(err.to_string().contains("1"));
        let err2 = SdkError::ProcessExited { code: None };
        assert!(err2.to_string().contains("None"));
    }

    #[test]
    fn test_display_unexpected_stream_end() {
        let err = SdkError::UnexpectedStreamEnd;
        assert_eq!(err.to_string(), "Stream ended unexpectedly");
    }

    #[test]
    fn test_display_not_supported() {
        let err = SdkError::NotSupported {
            feature: "mcp".into(),
        };
        assert_eq!(err.to_string(), "Feature not supported: mcp");
    }

    #[test]
    fn test_display_channel_send_error() {
        let err = SdkError::ChannelSendError;
        assert_eq!(err.to_string(), "Failed to send message through channel");
    }

    #[test]
    fn test_display_channel_closed() {
        let err = SdkError::ChannelClosed;
        assert_eq!(err.to_string(), "Channel closed unexpectedly");
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let sdk_err: SdkError = io_err.into();
        match &sdk_err {
            SdkError::ProcessError(_) => {},
            _ => panic!("expected ProcessError from io::Error"),
        }
        assert!(sdk_err.to_string().contains("file missing"));
    }

    #[test]
    fn test_from_serde_json_error() {
        let json_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let sdk_err: SdkError = json_err.into();
        match &sdk_err {
            SdkError::JsonError(_) => {},
            _ => panic!("expected JsonError from serde_json::Error"),
        }
    }

    #[test]
    fn test_from_send_error() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<i32>(1);
        // Drop the receiver so send would fail, but we construct SendError directly
        let send_err = tokio::sync::mpsc::error::SendError(42);
        let sdk_err: SdkError = send_err.into();
        let _ = tx; // keep tx alive to avoid warning
        match &sdk_err {
            SdkError::ChannelSendError => {},
            _ => panic!("expected ChannelSendError from SendError"),
        }
    }

    #[test]
    fn test_from_recv_error() {
        let recv_err = tokio::sync::broadcast::error::RecvError::Closed;
        let sdk_err: SdkError = recv_err.into();
        match &sdk_err {
            SdkError::ChannelClosed => {},
            _ => panic!("expected ChannelClosed from RecvError"),
        }
    }

    // ====================================================================
    // The `source()` chain — what a caller can still inspect
    // ====================================================================

    /// `CliJsonDecodeError` keeps the serde error as `#[source]`. The Display
    /// text deliberately does *not* repeat it, so the underlying reason is only
    /// reachable through `source()`: if that link were lost, "Failed to decode
    /// JSON from CLI output: <line>" would be the whole diagnostic.
    #[test]
    fn test_cli_json_decode_error_exposes_serde_error_as_source() {
        use std::error::Error;

        let line = r#"{"type": "result",}"#.to_string();
        let original = serde_json::from_str::<serde_json::Value>(&line).unwrap_err();
        let original_text = original.to_string();

        let err = SdkError::CliJsonDecodeError {
            line,
            original_error: original,
        };

        let source = err.source().expect("serde error must stay reachable");
        assert_eq!(source.to_string(), original_text);
        assert!(
            !err.to_string().contains(&original_text),
            "Display must not duplicate the source; the chain is the only path to it"
        );
    }

    /// `#[from] std::io::Error` also wires the source chain, so the io kind
    /// survives the conversion and a caller can still match on it.
    #[test]
    fn test_process_error_keeps_io_error_as_source() {
        use std::error::Error;

        let sdk_err: SdkError =
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied").into();
        let source = sdk_err.source().expect("io error must stay reachable");
        let io_err = source
            .downcast_ref::<std::io::Error>()
            .expect("source is the original io::Error");
        assert_eq!(io_err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    /// Same for the serde `#[from]`, including the fact that `JsonError`'s
    /// Display *does* embed the message (unlike `CliJsonDecodeError`).
    #[test]
    fn test_json_error_keeps_serde_error_as_source() {
        use std::error::Error;

        let json_err = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        let text = json_err.to_string();
        let sdk_err: SdkError = json_err.into();
        assert!(sdk_err.to_string().starts_with("JSON error: "));
        assert!(sdk_err.to_string().contains(&text));
        assert!(sdk_err.source().is_some());
    }

    /// Variants built from plain data carry no cause — asserted so that adding
    /// a `#[source]` later is a visible change.
    #[test]
    fn test_data_only_variants_have_no_source() {
        use std::error::Error;

        assert!(SdkError::timeout(1).source().is_none());
        assert!(SdkError::parse_error("e", "r").source().is_none());
        assert!(SdkError::ChannelClosed.source().is_none());
        assert!(
            SdkError::CliNotFound {
                searched_paths: "/usr/bin".into()
            }
            .source()
            .is_none()
        );
    }

    // ====================================================================
    // Display text that callers (and the diagram) rely on
    // ====================================================================

    /// The parse error prints both halves, and labels the raw payload — this
    /// is the only place the offending CLI line is shown to a human.
    #[test]
    fn test_display_message_parse_error_labels_the_raw_payload() {
        let err = SdkError::parse_error("Missing 'type' field", r#"{"foo":1}"#);
        assert_eq!(
            err.to_string(),
            "Failed to parse message: Missing 'type' field\nRaw message: {\"foo\":1}"
        );
    }

    #[test]
    fn test_display_timeout_and_unexpected_response_are_exact() {
        assert_eq!(
            SdkError::timeout(30).to_string(),
            "Timeout waiting for response after 30 seconds"
        );
        assert_eq!(
            SdkError::unexpected_response("result", "system").to_string(),
            "Unexpected response type: expected result, got system"
        );
    }

    /// The error code is kept on the variant but never rendered: a CLI error
    /// with `code: Some("E42")` prints exactly like one without. Pinned
    /// because it means logs alone cannot tell the two apart.
    #[test]
    fn test_display_cli_error_omits_the_code() {
        let with_code = SdkError::cli_error("boom", Some("E42".into()));
        let without = SdkError::cli_error("boom", None);
        assert_eq!(with_code.to_string(), "Claude CLI error: boom");
        assert_eq!(with_code.to_string(), without.to_string());
    }

    /// `CliNotFound` has to stay actionable: it names the install command and
    /// lists every path searched, one per line.
    #[test]
    fn test_display_cli_not_found_lists_every_searched_path() {
        let err = SdkError::CliNotFound {
            searched_paths: "/usr/local/bin/claude\n/opt/homebrew/bin/claude".into(),
        };
        let msg = err.to_string();
        assert!(msg.starts_with(
            "Claude CLI not found. Install with: npm install -g @anthropic-ai/claude-code"
        ));
        assert!(msg.contains("Searched in:"));
        assert!(msg.contains("/usr/local/bin/claude"));
        assert!(msg.contains("/opt/homebrew/bin/claude"));
    }

    // ====================================================================
    // Channel conversions
    // ====================================================================

    /// `RecvError::Lagged(n)` means "the broadcast dropped n messages behind
    /// you" — the receiver is still usable. It is mapped onto `ChannelClosed`,
    /// which `is_recoverable()` reports as true, so a retry loop behaves; but
    /// the count `n` is discarded and the two situations become
    /// indistinguishable downstream.
    #[test]
    fn test_from_recv_error_lagged_collapses_into_channel_closed() {
        let sdk_err: SdkError = tokio::sync::broadcast::error::RecvError::Lagged(7).into();
        assert!(matches!(sdk_err, SdkError::ChannelClosed));
        assert_eq!(sdk_err.to_string(), "Channel closed unexpectedly");
        assert!(
            !sdk_err.to_string().contains('7'),
            "the number of dropped messages is lost in the conversion"
        );
        assert!(sdk_err.is_recoverable());
    }

    /// The `SendError<T>` conversion is generic over the payload and drops it:
    /// whatever could not be sent is gone, and every payload type collapses to
    /// the same unit variant.
    #[test]
    fn test_from_send_error_is_payload_agnostic_and_drops_it() {
        let from_i32: SdkError = tokio::sync::mpsc::error::SendError(42i32).into();
        let from_string: SdkError =
            tokio::sync::mpsc::error::SendError("a message".to_string()).into();
        assert!(matches!(from_i32, SdkError::ChannelSendError));
        assert!(matches!(from_string, SdkError::ChannelSendError));
        assert_eq!(from_i32.to_string(), from_string.to_string());
        assert!(
            !from_string.to_string().contains("a message"),
            "the undelivered payload is not reported"
        );
    }

    // ====================================================================
    // The two classifiers, pinned over every variant
    // ====================================================================

    /// One row per `SdkError` variant, so that adding a variant without
    /// deciding its classification shows up here as a count mismatch rather
    /// than as a silent `false` in production.
    fn every_variant() -> Vec<(&'static str, SdkError, bool, bool)> {
        // (name, error, is_recoverable, is_config_error)
        vec![
            (
                "CliNotFound",
                SdkError::CliNotFound {
                    searched_paths: "p".into(),
                },
                false,
                true,
            ),
            (
                "ConnectionError",
                SdkError::ConnectionError("c".into()),
                false,
                false,
            ),
            (
                "ProcessError",
                SdkError::ProcessError(std::io::Error::other("io")),
                false,
                false,
            ),
            (
                "MessageParseError",
                SdkError::parse_error("e", "r"),
                false,
                false,
            ),
            (
                "JsonError",
                SdkError::JsonError(serde_json::from_str::<serde_json::Value>("{").unwrap_err()),
                false,
                false,
            ),
            (
                "CliJsonDecodeError",
                SdkError::CliJsonDecodeError {
                    line: "x".into(),
                    original_error: serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
                },
                false,
                false,
            ),
            (
                "TransportError",
                SdkError::TransportError("t".into()),
                false,
                false,
            ),
            ("Timeout", SdkError::timeout(1), true, false),
            (
                "SessionNotFound",
                SdkError::SessionNotFound("s".into()),
                false,
                false,
            ),
            (
                "ConfigError",
                SdkError::ConfigError("c".into()),
                false,
                true,
            ),
            (
                "ControlRequestError",
                SdkError::ControlRequestError("c".into()),
                false,
                false,
            ),
            (
                "UnexpectedResponse",
                SdkError::unexpected_response("a", "b"),
                false,
                false,
            ),
            ("CliError", SdkError::cli_error("m", None), false, false),
            ("ChannelSendError", SdkError::ChannelSendError, false, false),
            ("ChannelClosed", SdkError::ChannelClosed, true, false),
            ("InvalidState", SdkError::invalid_state("s"), false, false),
            (
                "ProcessExited",
                SdkError::ProcessExited { code: Some(2) },
                true,
                false,
            ),
            (
                "UnexpectedStreamEnd",
                SdkError::UnexpectedStreamEnd,
                true,
                false,
            ),
            (
                "NotSupported",
                SdkError::NotSupported {
                    feature: "f".into(),
                },
                false,
                true,
            ),
        ]
    }

    #[test]
    fn test_classifiers_over_every_variant() {
        assert_eq!(
            every_variant().len(),
            19,
            "SdkError has 19 variants; a new one must be classified in this table"
        );
        for (name, err, recoverable, config) in every_variant() {
            assert_eq!(
                err.is_recoverable(),
                recoverable,
                "{name}: is_recoverable mismatch"
            );
            assert_eq!(
                err.is_config_error(),
                config,
                "{name}: is_config_error mismatch"
            );
            assert!(
                !(recoverable && config),
                "{name}: the two classes must stay disjoint"
            );
        }
    }

    /// Every variant must render something non-empty: an error whose Display
    /// is blank is worse than no error at all.
    #[test]
    fn test_every_variant_renders_a_non_empty_message() {
        for (name, err, _, _) in every_variant() {
            let msg = err.to_string();
            assert!(!msg.trim().is_empty(), "{name}: empty Display");
        }
    }

    /// `ConnectionError` and `TransportError` are the two buckets a transport
    /// failure lands in, and neither is classified recoverable — so the retry
    /// helpers will not retry a dropped pipe. Pinned as current behaviour.
    #[test]
    fn test_transport_failures_are_not_classified_recoverable() {
        assert!(!SdkError::ConnectionError("broken pipe".into()).is_recoverable());
        assert!(!SdkError::TransportError("broken pipe".into()).is_recoverable());
    }
}
