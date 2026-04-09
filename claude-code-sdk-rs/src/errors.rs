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
}
