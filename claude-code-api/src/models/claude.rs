#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClaudeStreamEvent {
    MessageStart {
        message: ClaudeMessage,
    },
    ContentBlockStart {
        index: i32,
        content_block: ContentBlock,
    },
    ContentBlockDelta {
        index: i32,
        delta: ContentDelta,
    },
    ContentBlockStop {
        index: i32,
    },
    MessageDelta {
        delta: MessageDelta,
        usage: Usage,
    },
    MessageStop,
    Error {
        error: ClaudeError,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClaudeMessage {
    pub id: String,
    pub r#type: String,
    pub role: String,
    pub content: Vec<ContentBlock>,
    pub model: String,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: Usage,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum ContentDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MessageDelta {
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Usage {
    pub input_tokens: i32,
    pub output_tokens: i32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClaudeError {
    pub r#type: String,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClaudeCodeOutput {
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    #[serde(flatten)]
    pub data: Value,
}

/// Parsed CLI error from Claude Code output
#[derive(Debug, Clone, PartialEq)]
pub enum CliErrorKind {
    ContextLengthExceeded,
    RateLimit,
    Overloaded,
    Unknown(String),
}

/// A structured error parsed from Claude CLI output
#[derive(Debug, Clone)]
pub struct CliError {
    pub kind: CliErrorKind,
    pub message: String,
}

impl ClaudeCodeOutput {
    /// Parse a CLI error from a ClaudeCodeOutput with type "error".
    /// Claude CLI returns errors as JSON with `data.error.type` and `data.error.message`.
    /// Falls back to extracting from `data.message` or a generic message.
    pub fn parse_cli_error(&self) -> CliError {
        // Try structured error: { "error": { "type": "...", "message": "..." } }
        if let Some(error_obj) = self.data.get("error") {
            let error_type = error_obj
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("unknown");
            let error_message = error_obj
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown error from Claude CLI")
                .to_string();

            let kind = match error_type {
                "context_length_exceeded" | "invalid_request_error"
                    if error_message.to_lowercase().contains("context length")
                        || error_message.to_lowercase().contains("too many tokens")
                        || error_message.to_lowercase().contains("maximum context") =>
                {
                    CliErrorKind::ContextLengthExceeded
                },
                "context_length_exceeded" => CliErrorKind::ContextLengthExceeded,
                "rate_limit_error" => CliErrorKind::RateLimit,
                "overloaded_error" | "api_error"
                    if error_message.to_lowercase().contains("overloaded") =>
                {
                    CliErrorKind::Overloaded
                },
                "overloaded_error" => CliErrorKind::Overloaded,
                _ => CliErrorKind::Unknown(error_type.to_string()),
            };

            return CliError {
                kind,
                message: error_message,
            };
        }

        // Fallback: try top-level message field
        let message = self
            .data
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("Unknown error from Claude CLI")
            .to_string();

        // Try to detect error kind from message text
        let kind = if message.to_lowercase().contains("context length")
            || message.to_lowercase().contains("too many tokens")
        {
            CliErrorKind::ContextLengthExceeded
        } else if message.to_lowercase().contains("rate limit") {
            CliErrorKind::RateLimit
        } else if message.to_lowercase().contains("overloaded") {
            CliErrorKind::Overloaded
        } else {
            CliErrorKind::Unknown("unknown".to_string())
        };

        CliError { kind, message }
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeModel {
    pub id: String,
    pub display_name: String,
    pub context_window: i32,
}

impl ClaudeModel {
    pub fn all() -> Vec<Self> {
        vec![
            // Claude 4 Series (2025)
            Self {
                id: "claude-opus-4-1-20250805".to_string(),
                display_name: "Claude Opus 4.1".to_string(),
                context_window: 500000,
            },
            Self {
                id: "claude-opus-4-20250514".to_string(),
                display_name: "Claude Opus 4".to_string(),
                context_window: 500000,
            },
            Self {
                id: "claude-sonnet-4-20250514".to_string(),
                display_name: "Claude Sonnet 4".to_string(),
                context_window: 500000,
            },
            // Claude 3.7 Series (2025)
            Self {
                id: "claude-3-7-sonnet-20250219".to_string(),
                display_name: "Claude Sonnet 3.7".to_string(),
                context_window: 200000,
            },
            Self {
                id: "claude-3-7-sonnet-latest".to_string(),
                display_name: "Claude Sonnet 3.7 (Latest)".to_string(),
                context_window: 200000,
            },
            // Claude 3.5 Series (2024)
            Self {
                id: "claude-3-5-haiku-20241022".to_string(),
                display_name: "Claude Haiku 3.5".to_string(),
                context_window: 200000,
            },
            Self {
                id: "claude-3-5-haiku-latest".to_string(),
                display_name: "Claude Haiku 3.5 (Latest)".to_string(),
                context_window: 200000,
            },
            // Claude 3 Series (2024)
            Self {
                id: "claude-3-haiku-20240307".to_string(),
                display_name: "Claude Haiku 3".to_string(),
                context_window: 200000,
            },
        ]
    }

    /// Get the context window size for a given model ID.
    /// Returns the model's context_window with a 90% safety factor applied,
    /// or a fallback of 180_000 (90% of 200K) if the model is unknown.
    pub fn context_window_for_model(model_id: &str) -> usize {
        let window = Self::all()
            .into_iter()
            .find(|m| m.id == model_id)
            .map(|m| m.context_window as usize)
            .unwrap_or(200_000);

        // Apply 90% safety factor to account for estimation imprecision
        (window as f64 * 0.9) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_error_output(data: serde_json::Value) -> ClaudeCodeOutput {
        ClaudeCodeOutput {
            r#type: "error".to_string(),
            subtype: None,
            data,
        }
    }

    #[test]
    fn test_parse_cli_error_context_length_exceeded() {
        let output = make_error_output(json!({
            "error": {
                "type": "context_length_exceeded",
                "message": "This request would exceed the maximum context length of 200000 tokens."
            }
        }));
        let err = output.parse_cli_error();
        assert_eq!(err.kind, CliErrorKind::ContextLengthExceeded);
        assert!(err.message.contains("200000"));
    }

    #[test]
    fn test_parse_cli_error_invalid_request_with_context_hint() {
        let output = make_error_output(json!({
            "error": {
                "type": "invalid_request_error",
                "message": "prompt has too many tokens (250000). Maximum context length is 200000."
            }
        }));
        let err = output.parse_cli_error();
        assert_eq!(err.kind, CliErrorKind::ContextLengthExceeded);
    }

    #[test]
    fn test_parse_cli_error_rate_limit() {
        let output = make_error_output(json!({
            "error": {
                "type": "rate_limit_error",
                "message": "Rate limit exceeded, please retry after 30s"
            }
        }));
        let err = output.parse_cli_error();
        assert_eq!(err.kind, CliErrorKind::RateLimit);
        assert!(err.message.contains("30s"));
    }

    #[test]
    fn test_parse_cli_error_overloaded() {
        let output = make_error_output(json!({
            "error": {
                "type": "overloaded_error",
                "message": "The API is temporarily overloaded"
            }
        }));
        let err = output.parse_cli_error();
        assert_eq!(err.kind, CliErrorKind::Overloaded);
    }

    #[test]
    fn test_parse_cli_error_api_error_overloaded() {
        let output = make_error_output(json!({
            "error": {
                "type": "api_error",
                "message": "Server is overloaded, please try again later"
            }
        }));
        let err = output.parse_cli_error();
        assert_eq!(err.kind, CliErrorKind::Overloaded);
    }

    #[test]
    fn test_parse_cli_error_unknown_type() {
        let output = make_error_output(json!({
            "error": {
                "type": "some_new_error",
                "message": "Something unexpected happened"
            }
        }));
        let err = output.parse_cli_error();
        assert!(matches!(err.kind, CliErrorKind::Unknown(ref t) if t == "some_new_error"));
        assert_eq!(err.message, "Something unexpected happened");
    }

    #[test]
    fn test_parse_cli_error_no_error_field_with_message() {
        let output = make_error_output(json!({
            "message": "context length exceeded for this prompt"
        }));
        let err = output.parse_cli_error();
        assert_eq!(err.kind, CliErrorKind::ContextLengthExceeded);
        assert!(err.message.contains("context length"));
    }

    #[test]
    fn test_parse_cli_error_no_error_field_rate_limit_message() {
        let output = make_error_output(json!({
            "message": "rate limit hit, slow down"
        }));
        let err = output.parse_cli_error();
        assert_eq!(err.kind, CliErrorKind::RateLimit);
    }

    #[test]
    fn test_parse_cli_error_empty_data() {
        let output = make_error_output(json!({}));
        let err = output.parse_cli_error();
        assert!(matches!(err.kind, CliErrorKind::Unknown(_)));
        assert_eq!(err.message, "Unknown error from Claude CLI");
    }

    #[test]
    fn test_parse_cli_error_missing_message_in_error() {
        let output = make_error_output(json!({
            "error": {
                "type": "context_length_exceeded"
            }
        }));
        let err = output.parse_cli_error();
        assert_eq!(err.kind, CliErrorKind::ContextLengthExceeded);
        assert_eq!(err.message, "Unknown error from Claude CLI");
    }

    #[test]
    fn test_context_window_for_known_model() {
        let window = ClaudeModel::context_window_for_model("claude-sonnet-4-20250514");
        // 500_000 * 0.9 = 450_000
        assert_eq!(window, 450_000);
    }

    #[test]
    fn test_context_window_for_claude3_model() {
        let window = ClaudeModel::context_window_for_model("claude-3-7-sonnet-20250219");
        // 200_000 * 0.9 = 180_000
        assert_eq!(window, 180_000);
    }

    #[test]
    fn test_context_window_for_unknown_model() {
        let window = ClaudeModel::context_window_for_model("unknown-model-xyz");
        // fallback 200_000 * 0.9 = 180_000
        assert_eq!(window, 180_000);
    }
}
