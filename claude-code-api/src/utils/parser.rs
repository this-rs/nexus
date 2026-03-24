use crate::models::{
    claude::{ClaudeCodeOutput, ClaudeStreamEvent, ContentDelta},
    openai::{ChatCompletionStreamResponse, DeltaMessage, StreamChoice},
};
use chrono::Utc;
use uuid::Uuid;

#[allow(dead_code)]
pub fn claude_to_openai_stream(
    claude_output: ClaudeCodeOutput,
    model: &str,
) -> Option<ChatCompletionStreamResponse> {
    match claude_output.r#type.as_str() {
        "assistant" => {
            // 处理助手消息
            if let Some(message) = claude_output.data.get("message")
                && let Some(content_array) = message.get("content").and_then(|c| c.as_array())
            {
                for content in content_array {
                    if let Some(text) = content.get("text").and_then(|t| t.as_str()) {
                        return Some(ChatCompletionStreamResponse {
                            id: Uuid::new_v4().to_string(),
                            object: "chat.completion.chunk".to_string(),
                            created: Utc::now().timestamp(),
                            model: model.to_string(),
                            choices: vec![StreamChoice {
                                index: 0,
                                delta: DeltaMessage {
                                    role: Some("assistant".to_string()),
                                    content: Some(text.to_string()),
                                    tool_calls: None,
                                },
                                finish_reason: None,
                            }],
                        });
                    }
                }
            }
        },
        "result" => {
            // 会话结束
            return Some(ChatCompletionStreamResponse {
                id: Uuid::new_v4().to_string(),
                object: "chat.completion.chunk".to_string(),
                created: Utc::now().timestamp(),
                model: model.to_string(),
                choices: vec![StreamChoice {
                    index: 0,
                    delta: DeltaMessage::default(),
                    finish_reason: Some("stop".to_string()),
                }],
            });
        },
        _ => {},
    }

    None
}

#[allow(dead_code)]
fn convert_claude_event_to_openai(
    event: ClaudeStreamEvent,
    model: &str,
) -> Option<ChatCompletionStreamResponse> {
    match event {
        ClaudeStreamEvent::MessageStart { .. } => Some(ChatCompletionStreamResponse {
            id: Uuid::new_v4().to_string(),
            object: "chat.completion.chunk".to_string(),
            created: Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: DeltaMessage {
                    role: Some("assistant".to_string()),
                    content: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
        }),
        ClaudeStreamEvent::ContentBlockDelta { delta, .. } => match delta {
            ContentDelta::TextDelta { text } => Some(ChatCompletionStreamResponse {
                id: Uuid::new_v4().to_string(),
                object: "chat.completion.chunk".to_string(),
                created: Utc::now().timestamp(),
                model: model.to_string(),
                choices: vec![StreamChoice {
                    index: 0,
                    delta: DeltaMessage {
                        role: None,
                        content: Some(text),
                        tool_calls: None,
                    },
                    finish_reason: None,
                }],
            }),
        },
        ClaudeStreamEvent::MessageStop => Some(ChatCompletionStreamResponse {
            id: Uuid::new_v4().to_string(),
            object: "chat.completion.chunk".to_string(),
            created: Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: DeltaMessage::default(),
                finish_reason: Some("stop".to_string()),
            }],
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::claude::ClaudeMessage;
    use serde_json::json;

    fn make_claude_output(type_str: &str, data: serde_json::Value) -> ClaudeCodeOutput {
        ClaudeCodeOutput {
            r#type: type_str.to_string(),
            subtype: None,
            data,
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  claude_to_openai_stream
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn test_assistant_message_with_text() {
        let output = make_claude_output(
            "assistant",
            json!({
                "message": {
                    "content": [{"type": "text", "text": "Hello, world!"}]
                }
            }),
        );
        let result = claude_to_openai_stream(output, "test-model");
        assert!(result.is_some());
        let resp = result.unwrap();
        assert_eq!(resp.object, "chat.completion.chunk");
        assert_eq!(resp.model, "test-model");
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].delta.role.as_deref(), Some("assistant"));
        assert_eq!(
            resp.choices[0].delta.content.as_deref(),
            Some("Hello, world!")
        );
        assert!(resp.choices[0].finish_reason.is_none());
    }

    #[test]
    fn test_result_message() {
        let output = make_claude_output(
            "result",
            json!({
                "duration_ms": 500,
                "is_error": false
            }),
        );
        let result = claude_to_openai_stream(output, "test-model");
        assert!(result.is_some());
        let resp = result.unwrap();
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn test_unknown_type_returns_none() {
        let output = make_claude_output("system", json!({"info": "ignored"}));
        let result = claude_to_openai_stream(output, "test-model");
        assert!(result.is_none());
    }

    #[test]
    fn test_assistant_message_no_content_returns_none() {
        let output = make_claude_output(
            "assistant",
            json!({
                "message": {
                    "content": []
                }
            }),
        );
        let result = claude_to_openai_stream(output, "test-model");
        assert!(result.is_none());
    }

    #[test]
    fn test_assistant_message_no_message_key_returns_none() {
        let output = make_claude_output("assistant", json!({"other": "data"}));
        let result = claude_to_openai_stream(output, "test-model");
        assert!(result.is_none());
    }

    #[test]
    fn test_assistant_message_content_without_text_returns_none() {
        let output = make_claude_output(
            "assistant",
            json!({
                "message": {
                    "content": [{"type": "image", "url": "http://example.com/img.png"}]
                }
            }),
        );
        let result = claude_to_openai_stream(output, "test-model");
        assert!(result.is_none());
    }

    #[test]
    fn test_assistant_picks_first_text_block() {
        let output = make_claude_output(
            "assistant",
            json!({
                "message": {
                    "content": [
                        {"type": "text", "text": "First"},
                        {"type": "text", "text": "Second"}
                    ]
                }
            }),
        );
        let result = claude_to_openai_stream(output, "m");
        assert!(result.is_some());
        assert_eq!(
            result.unwrap().choices[0].delta.content.as_deref(),
            Some("First")
        );
    }

    // ═══════════════════════════════════════════════════════════════
    //  convert_claude_event_to_openai
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn test_event_message_start() {
        let event = ClaudeStreamEvent::MessageStart {
            message: ClaudeMessage {
                id: "msg_1".to_string(),
                r#type: "message".to_string(),
                role: "assistant".to_string(),
                content: vec![],
                model: "claude-3".to_string(),
                stop_reason: None,
                stop_sequence: None,
                usage: crate::models::claude::Usage {
                    input_tokens: 10,
                    output_tokens: 0,
                },
            },
        };
        let result = convert_claude_event_to_openai(event, "test-model");
        assert!(result.is_some());
        let resp = result.unwrap();
        assert_eq!(resp.choices[0].delta.role.as_deref(), Some("assistant"));
        assert!(resp.choices[0].delta.content.is_none());
    }

    #[test]
    fn test_event_content_block_delta() {
        let event = ClaudeStreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentDelta::TextDelta {
                text: "chunk of text".to_string(),
            },
        };
        let result = convert_claude_event_to_openai(event, "test-model");
        assert!(result.is_some());
        let resp = result.unwrap();
        assert!(resp.choices[0].delta.role.is_none());
        assert_eq!(
            resp.choices[0].delta.content.as_deref(),
            Some("chunk of text")
        );
    }

    #[test]
    fn test_event_message_stop() {
        let event = ClaudeStreamEvent::MessageStop;
        let result = convert_claude_event_to_openai(event, "test-model");
        assert!(result.is_some());
        let resp = result.unwrap();
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn test_event_content_block_stop_returns_none() {
        let event = ClaudeStreamEvent::ContentBlockStop { index: 0 };
        let result = convert_claude_event_to_openai(event, "test-model");
        assert!(result.is_none());
    }

    #[test]
    fn test_event_error_returns_none() {
        let event = ClaudeStreamEvent::Error {
            error: crate::models::claude::ClaudeError {
                r#type: "server_error".to_string(),
                message: "something went wrong".to_string(),
            },
        };
        let result = convert_claude_event_to_openai(event, "test-model");
        assert!(result.is_none());
    }

    #[test]
    fn test_event_message_delta_returns_none() {
        let event = ClaudeStreamEvent::MessageDelta {
            delta: crate::models::claude::MessageDelta {
                stop_reason: Some("end_turn".to_string()),
                stop_sequence: None,
            },
            usage: crate::models::claude::Usage {
                input_tokens: 10,
                output_tokens: 50,
            },
        };
        let result = convert_claude_event_to_openai(event, "test-model");
        assert!(result.is_none());
    }
}
