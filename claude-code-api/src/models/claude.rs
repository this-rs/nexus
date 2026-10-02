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

impl ClaudeCodeOutput {
    /// Returns the parent_tool_use_id if this output is from a subagent sidechain.
    /// None = top-level message, Some(id) = message from a subagent Task execution.
    pub fn parent_tool_use_id(&self) -> Option<&str> {
        self.data.get("parent_tool_use_id").and_then(|v| v.as_str())
    }

    /// Returns true if this output is from a subagent sidechain (has parent_tool_use_id).
    pub fn is_sidechain(&self) -> bool {
        self.parent_tool_use_id().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_sidechain_detection() {
        // Top-level message (no parent_tool_use_id)
        let top_level = ClaudeCodeOutput {
            r#type: "assistant".to_string(),
            subtype: None,
            data: json!({
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "Hello"}]
                }
            }),
        };
        assert!(!top_level.is_sidechain());
        assert!(top_level.parent_tool_use_id().is_none());

        // Sidechain message (has parent_tool_use_id)
        let sidechain = ClaudeCodeOutput {
            r#type: "assistant".to_string(),
            subtype: None,
            data: json!({
                "parent_tool_use_id": "toolu_abc123",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "Subagent response"}]
                }
            }),
        };
        assert!(sidechain.is_sidechain());
        assert_eq!(sidechain.parent_tool_use_id(), Some("toolu_abc123"));
    }

    #[test]
    fn test_result_message_not_sidechain() {
        let result = ClaudeCodeOutput {
            r#type: "result".to_string(),
            subtype: Some("conversation_turn".to_string()),
            data: json!({
                "duration_ms": 1000,
                "is_error": false,
                "num_turns": 1,
                "session_id": "test"
            }),
        };
        assert!(!result.is_sidechain());
    }

    #[test]
    fn test_null_parent_tool_use_id_not_sidechain() {
        // Explicit null should not be treated as sidechain
        let output = ClaudeCodeOutput {
            r#type: "assistant".to_string(),
            subtype: None,
            data: json!({
                "parent_tool_use_id": null,
                "message": {"role": "assistant", "content": []}
            }),
        };
        assert!(!output.is_sidechain());
        assert!(output.parent_tool_use_id().is_none());
    }

    /// A non-string `parent_tool_use_id` is not a sidechain either: the accessor
    /// goes through `as_str()`, so a number or an object is read as "absent"
    /// rather than stringified or rejected.
    #[test]
    fn test_non_string_parent_tool_use_id_is_read_as_absent() {
        for bogus in [json!(42), json!(true), json!({"id": "toolu_1"}), json!([])] {
            let output = ClaudeCodeOutput {
                r#type: "assistant".to_string(),
                subtype: None,
                data: json!({"parent_tool_use_id": bogus}),
            };
            assert!(
                !output.is_sidechain(),
                "parent_tool_use_id = {bogus} must not count as a sidechain"
            );
            assert_eq!(output.parent_tool_use_id(), None);
        }
    }

    /// `data` is `#[serde(flatten)]`, so a `type` key inside it collides with the
    /// struct's own `type` field.
    ///
    /// Serializing emits the key twice — `{"type":"assistant","type":"nested"}` —
    /// and deserializing that output fails with `duplicate field \`type\``. The
    /// type therefore does **not** round-trip through itself.
    ///
    /// This is latent rather than live: the `claude --output-format stream-json`
    /// transcript carries exactly one top-level `type` per line, so the gateway's
    /// read path never builds such a value. It bites any code that re-serializes
    /// a `ClaudeCodeOutput` whose `data` it did not author.
    #[test]
    fn test_flattened_data_colliding_with_type_breaks_the_round_trip() {
        let output = ClaudeCodeOutput {
            r#type: "assistant".to_string(),
            subtype: None,
            data: json!({"type": "nested", "x": 1}),
        };

        let encoded = serde_json::to_string(&output).expect("serializing never fails");
        assert_eq!(encoded, r#"{"type":"assistant","type":"nested","x":1}"#);

        let decoded = serde_json::from_str::<ClaudeCodeOutput>(&encoded);
        let error = decoded
            .expect_err("the duplicated key must not deserialize")
            .to_string();
        assert!(
            error.contains("duplicate field `type`"),
            "expected a duplicate-field error, got {error:?}"
        );
    }

    /// Without a colliding key the round trip is exact, including the
    /// `skip_serializing_if` on `subtype`.
    #[test]
    fn test_round_trip_is_exact_without_a_colliding_key() {
        let output = ClaudeCodeOutput {
            r#type: "result".to_string(),
            subtype: Some("success".to_string()),
            data: json!({"session_id": "s-1", "is_error": false}),
        };

        let encoded = serde_json::to_string(&output).unwrap();
        let decoded: ClaudeCodeOutput = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.r#type, "result");
        assert_eq!(decoded.subtype.as_deref(), Some("success"));
        assert_eq!(
            decoded.data,
            json!({"session_id": "s-1", "is_error": false})
        );

        // `subtype: None` disappears from the wire rather than becoming `null`.
        let without = ClaudeCodeOutput {
            r#type: "assistant".to_string(),
            subtype: None,
            data: json!({}),
        };
        assert_eq!(
            serde_json::to_string(&without).unwrap(),
            r#"{"type":"assistant"}"#
        );
    }

    /// `ContentBlock` has a single variant, `Text`. Every other block kind the
    /// Anthropic API emits — `tool_use`, `tool_result`, `thinking`, `image` — is
    /// refused, so `ClaudeMessage`, and with it the whole `ClaudeStreamEvent`
    /// model, cannot deserialize a message that uses a tool.
    ///
    /// Nothing in production deserializes these types (see `utils/parser.rs`,
    /// which is the only consumer and is itself unreachable), which is why the
    /// gap has never shown up as a parse failure at run time.
    #[test]
    fn test_content_block_refuses_every_kind_but_text() {
        let text: ContentBlock = serde_json::from_value(json!({"type": "text", "text": "bonjour"}))
            .expect("a text block must parse");
        let ContentBlock::Text { text } = text;
        assert_eq!(text, "bonjour");

        for block in [
            json!({"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}}),
            json!({"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}),
            json!({"type": "thinking", "thinking": "hmm"}),
        ] {
            let kind = block["type"].as_str().unwrap().to_string();
            assert!(
                serde_json::from_value::<ContentBlock>(block).is_err(),
                "ContentBlock must not pretend to understand a {kind} block"
            );
        }
    }

    /// The stream-event tags are the snake_case names the Anthropic SSE wire
    /// format uses, and an unknown event name is refused rather than defaulted.
    #[test]
    fn test_stream_event_tags_match_the_wire_names() {
        let stop: ClaudeStreamEvent = serde_json::from_value(json!({"type": "message_stop"}))
            .expect("message_stop must parse");
        assert!(matches!(stop, ClaudeStreamEvent::MessageStop));

        let delta: ClaudeStreamEvent = serde_json::from_value(json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "hi"}
        }))
        .expect("content_block_delta must parse");
        assert!(matches!(
            delta,
            ClaudeStreamEvent::ContentBlockDelta { index: 0, .. }
        ));

        assert!(
            serde_json::from_value::<ClaudeStreamEvent>(json!({"type": "ping"})).is_err(),
            "an unknown event tag must be refused, not silently ignored"
        );
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeModel {
    pub id: String,
    pub display_name: String,
    pub context_window: i32,
}

impl ClaudeModel {
    /// Static fallback catalog of current Anthropic models.
    ///
    /// Used when the dynamic ModelRegistry cannot reach the Anthropic Models
    /// API (no ANTHROPIC_API_KEY, network error). Keep in sync with
    /// <https://platform.claude.com/docs/en/about-claude/models/overview>
    pub fn all() -> Vec<Self> {
        vec![
            // Claude 5 Series (2026)
            Self {
                id: "claude-fable-5-1".to_string(),
                display_name: "Claude Fable 5.1".to_string(),
                context_window: 1000000,
            },
            Self {
                id: "claude-opus-5-5".to_string(),
                display_name: "Claude Opus 5.5".to_string(),
                context_window: 1000000,
            },
            Self {
                id: "claude-fable-5".to_string(),
                display_name: "Claude Fable 5".to_string(),
                context_window: 1000000,
            },
            Self {
                id: "claude-opus-5".to_string(),
                display_name: "Claude Opus 5".to_string(),
                context_window: 1000000,
            },
            Self {
                id: "claude-sonnet-5".to_string(),
                display_name: "Claude Sonnet 5".to_string(),
                context_window: 1000000,
            },
            // Claude 4.x Series (2025-2026)
            Self {
                id: "claude-opus-4-8".to_string(),
                display_name: "Claude Opus 4.8".to_string(),
                context_window: 1000000,
            },
            Self {
                id: "claude-opus-4-7".to_string(),
                display_name: "Claude Opus 4.7".to_string(),
                context_window: 1000000,
            },
            Self {
                id: "claude-opus-4-6".to_string(),
                display_name: "Claude Opus 4.6".to_string(),
                context_window: 1000000,
            },
            Self {
                id: "claude-opus-4-5".to_string(),
                display_name: "Claude Opus 4.5".to_string(),
                context_window: 200000,
            },
            Self {
                id: "claude-sonnet-4-6".to_string(),
                display_name: "Claude Sonnet 4.6".to_string(),
                context_window: 1000000,
            },
            Self {
                id: "claude-sonnet-4-5".to_string(),
                display_name: "Claude Sonnet 4.5".to_string(),
                context_window: 200000,
            },
            Self {
                id: "claude-haiku-4-5".to_string(),
                display_name: "Claude Haiku 4.5".to_string(),
                context_window: 200000,
            },
        ]
    }
}
