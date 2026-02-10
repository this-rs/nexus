//! Message parsing utilities
//!
//! This module handles parsing of JSON messages from the Claude CLI into
//! strongly typed Message enums.

use crate::{
    errors::{Result, SdkError},
    types::{
        AssistantMessage, ContentBlock, ContentValue, Message, StreamDelta, StreamEventData,
        TextContent, ThinkingContent, ToolResultContent, ToolUseContent, UserMessage,
    },
};
use serde_json::Value;
use tracing::{debug, trace};

/// Parse a JSON value into a Message
pub fn parse_message(json: Value) -> Result<Option<Message>> {
    // Get message type
    let msg_type = json
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| SdkError::parse_error("Missing 'type' field", json.to_string()))?;

    match msg_type {
        "user" => parse_user_message(json),
        "assistant" => parse_assistant_message(json),
        "system" => parse_system_message(json),
        "result" => parse_result_message(json),
        "stream_event" => parse_stream_event(json),
        _ => {
            debug!("Ignoring message type: {}", msg_type);
            Ok(None)
        },
    }
}

/// Parse a user message
fn parse_user_message(json: Value) -> Result<Option<Message>> {
    let message = json
        .get("message")
        .ok_or_else(|| SdkError::parse_error("Missing 'message' field", json.to_string()))?;

    // Handle different content formats:
    // 1. String content: simple user text prompt
    // 2. Array content: tool results (the CLI sends tool_result blocks as a user message)
    let (content, content_blocks) =
        if let Some(content_str) = message.get("content").and_then(|v| v.as_str()) {
            // Simple string content
            (content_str.to_string(), None)
        } else if let Some(content_array) = message.get("content").and_then(|v| v.as_array()) {
            // Array content — parse each item as a content block (tool_result, text, etc.)
            let mut blocks = Vec::new();
            for item in content_array {
                if let Some(block) = parse_content_block(item)? {
                    blocks.push(block);
                }
            }
            debug!(
                "Parsed user message with {} content blocks (tool results)",
                blocks.len()
            );
            let blocks_opt = if blocks.is_empty() {
                None
            } else {
                Some(blocks)
            };
            (String::new(), blocks_opt)
        } else {
            return Err(SdkError::parse_error(
                "Missing or invalid 'content' field",
                json.to_string(),
            ));
        };

    let parent_tool_use_id = json
        .get("parent_tool_use_id")
        .and_then(|v| v.as_str())
        .map(String::from);

    Ok(Some(Message::User {
        message: UserMessage {
            content,
            content_blocks,
        },
        parent_tool_use_id,
    }))
}

/// Parse an assistant message
fn parse_assistant_message(json: Value) -> Result<Option<Message>> {
    let message = json
        .get("message")
        .ok_or_else(|| SdkError::parse_error("Missing 'message' field", json.to_string()))?;

    let content_array = message
        .get("content")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            SdkError::parse_error("Missing or invalid 'content' array", json.to_string())
        })?;

    let mut content_blocks = Vec::new();

    for content_item in content_array {
        if let Some(block) = parse_content_block(content_item)? {
            content_blocks.push(block);
        }
    }

    let parent_tool_use_id = json
        .get("parent_tool_use_id")
        .and_then(|v| v.as_str())
        .map(String::from);

    Ok(Some(Message::Assistant {
        message: AssistantMessage {
            content: content_blocks,
        },
        parent_tool_use_id,
    }))
}

/// Parse a content block
fn parse_content_block(json: &Value) -> Result<Option<ContentBlock>> {
    // First check if it has a type field
    if let Some(block_type) = json.get("type").and_then(|v| v.as_str()) {
        match block_type {
            "text" => {
                let text = json.get("text").and_then(|v| v.as_str()).ok_or_else(|| {
                    SdkError::parse_error("Missing 'text' field in text block", json.to_string())
                })?;
                Ok(Some(ContentBlock::Text(TextContent {
                    text: text.to_string(),
                })))
            },
            "thinking" => {
                let thinking = json
                    .get("thinking")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        SdkError::parse_error(
                            "Missing 'thinking' field in thinking block",
                            json.to_string(),
                        )
                    })?;
                let signature =
                    json.get("signature")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            SdkError::parse_error(
                                "Missing 'signature' field in thinking block",
                                json.to_string(),
                            )
                        })?;
                Ok(Some(ContentBlock::Thinking(ThinkingContent {
                    thinking: thinking.to_string(),
                    signature: signature.to_string(),
                })))
            },
            "tool_use" => {
                let id = json.get("id").and_then(|v| v.as_str()).ok_or_else(|| {
                    SdkError::parse_error("Missing 'id' field in tool_use block", json.to_string())
                })?;
                let name = json.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
                    SdkError::parse_error(
                        "Missing 'name' field in tool_use block",
                        json.to_string(),
                    )
                })?;
                let input = json
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

                Ok(Some(ContentBlock::ToolUse(ToolUseContent {
                    id: id.to_string(),
                    name: name.to_string(),
                    input,
                })))
            },
            "tool_result" => {
                let tool_use_id = json
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        SdkError::parse_error(
                            "Missing 'tool_use_id' field in tool_result block",
                            json.to_string(),
                        )
                    })?;

                let content = if let Some(content_val) = json.get("content") {
                    if let Some(text) = content_val.as_str() {
                        Some(ContentValue::Text(text.to_string()))
                    } else {
                        content_val
                            .as_array()
                            .map(|array| ContentValue::Structured(array.clone()))
                    }
                } else {
                    None
                };

                let is_error = json.get("is_error").and_then(|v| v.as_bool());

                Ok(Some(ContentBlock::ToolResult(ToolResultContent {
                    tool_use_id: tool_use_id.to_string(),
                    content,
                    is_error,
                })))
            },
            _ => {
                debug!("Unknown content block type: {}", block_type);
                Ok(None)
            },
        }
    } else {
        // Try to parse as a simple text block (backward compatibility)
        if let Some(text) = json.get("text").and_then(|v| v.as_str()) {
            Ok(Some(ContentBlock::Text(TextContent {
                text: text.to_string(),
            })))
        } else {
            trace!("Skipping non-text content block without type");
            Ok(None)
        }
    }
}

/// Parse a system message
fn parse_system_message(json: Value) -> Result<Option<Message>> {
    let subtype = json
        .get("subtype")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let data = json
        .get("data")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    Ok(Some(Message::System { subtype, data }))
}

/// Parse a result message
fn parse_result_message(json: Value) -> Result<Option<Message>> {
    // Use serde to parse the full result message
    match serde_json::from_value::<Message>(json.clone()) {
        Ok(msg) => Ok(Some(msg)),
        Err(_e) => {
            // Fallback: create a minimal result message
            let subtype = json
                .get("subtype")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let duration_ms = json
                .get("duration_ms")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);

            let session_id = json
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            Ok(Some(Message::Result {
                subtype,
                duration_ms,
                duration_api_ms: json
                    .get("duration_api_ms")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
                is_error: json
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                num_turns: json.get("num_turns").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                session_id,
                total_cost_usd: json.get("total_cost_usd").and_then(|v| v.as_f64()),
                usage: json.get("usage").cloned(),
                result: json
                    .get("result")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                structured_output: json
                    .get("structured_output")
                    .or_else(|| json.get("structuredOutput"))
                    .and_then(|v| (!v.is_null()).then(|| v.clone())),
            }))
        },
    }
}

/// Parse a stream event message (for real-time token streaming)
fn parse_stream_event(json: Value) -> Result<Option<Message>> {
    let event = json.get("event").ok_or_else(|| {
        SdkError::parse_error("Missing 'event' field in stream_event", json.to_string())
    })?;

    let event_type = event
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| SdkError::parse_error("Missing 'type' in event", json.to_string()))?;

    let session_id = json
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(String::from);

    let event_data = match event_type {
        "message_start" => {
            let message = event.get("message").cloned().unwrap_or(Value::Null);
            StreamEventData::MessageStart { message }
        },
        "content_block_start" => {
            let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let content_block = event.get("content_block").cloned().unwrap_or(Value::Null);
            StreamEventData::ContentBlockStart {
                index,
                content_block,
            }
        },
        "content_block_delta" => {
            let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let delta_obj = event.get("delta").ok_or_else(|| {
                SdkError::parse_error("Missing 'delta' in content_block_delta", json.to_string())
            })?;

            let delta_type = delta_obj
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("text_delta");

            let delta = match delta_type {
                "text_delta" => {
                    let text = delta_obj
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    StreamDelta::TextDelta { text }
                },
                "thinking_delta" => {
                    let thinking = delta_obj
                        .get("thinking")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    StreamDelta::ThinkingDelta { thinking }
                },
                "input_json_delta" => {
                    let partial_json = delta_obj
                        .get("partial_json")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    StreamDelta::InputJsonDelta { partial_json }
                },
                _ => {
                    // Default to text delta for unknown types
                    let text = delta_obj
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    StreamDelta::TextDelta { text }
                },
            };

            StreamEventData::ContentBlockDelta { index, delta }
        },
        "content_block_stop" => {
            let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            StreamEventData::ContentBlockStop { index }
        },
        "message_delta" => {
            let delta = event.get("delta").cloned().unwrap_or(Value::Null);
            let usage = event.get("usage").cloned();
            StreamEventData::MessageDelta { delta, usage }
        },
        "message_stop" => StreamEventData::MessageStop,
        _ => {
            debug!("Unknown stream event type: {}", event_type);
            return Ok(None);
        },
    };

    let parent_tool_use_id = json
        .get("parent_tool_use_id")
        .and_then(|v| v.as_str())
        .map(String::from);

    Ok(Some(Message::StreamEvent {
        event: event_data,
        session_id,
        parent_tool_use_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_user_message() {
        let json = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": "Hello, Claude!"
            }
        });

        let result = parse_message(json).unwrap();
        assert!(result.is_some());

        if let Some(Message::User {
            message,
            parent_tool_use_id,
        }) = result
        {
            assert_eq!(message.content, "Hello, Claude!");
            assert!(parent_tool_use_id.is_none());
        } else {
            panic!("Expected User message");
        }
    }

    #[test]
    fn test_parse_assistant_message_with_text() {
        let json = json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {
                        "type": "text",
                        "text": "Hello! How can I help you?"
                    }
                ]
            }
        });

        let result = parse_message(json).unwrap();
        assert!(result.is_some());

        if let Some(Message::Assistant {
            message,
            parent_tool_use_id,
        }) = result
        {
            assert_eq!(message.content.len(), 1);
            assert!(parent_tool_use_id.is_none());
            if let ContentBlock::Text(text) = &message.content[0] {
                assert_eq!(text.text, "Hello! How can I help you?");
            } else {
                panic!("Expected Text content block");
            }
        } else {
            panic!("Expected Assistant message");
        }
    }

    #[test]
    fn test_parse_thinking_block() {
        let json = json!({
            "type": "thinking",
            "thinking": "Let me analyze this problem...",
            "signature": "thinking_sig_123"
        });

        let result = parse_content_block(&json).unwrap();
        assert!(result.is_some());

        if let Some(ContentBlock::Thinking(thinking)) = result {
            assert_eq!(thinking.thinking, "Let me analyze this problem...");
            assert_eq!(thinking.signature, "thinking_sig_123");
        } else {
            panic!("Expected Thinking content block");
        }
    }

    #[test]
    fn test_parse_tool_use_block() {
        let json = json!({
            "type": "tool_use",
            "id": "tool_123",
            "name": "read_file",
            "input": {
                "path": "/tmp/test.txt"
            }
        });

        let result = parse_content_block(&json).unwrap();
        assert!(result.is_some());

        if let Some(ContentBlock::ToolUse(tool_use)) = result {
            assert_eq!(tool_use.id, "tool_123");
            assert_eq!(tool_use.name, "read_file");
            assert_eq!(tool_use.input["path"], "/tmp/test.txt");
        } else {
            panic!("Expected ToolUse content block");
        }
    }

    #[test]
    fn test_parse_system_message() {
        let json = json!({
            "type": "system",
            "subtype": "status",
            "data": {
                "status": "ready"
            }
        });

        let result = parse_message(json).unwrap();
        assert!(result.is_some());

        if let Some(Message::System { subtype, data }) = result {
            assert_eq!(subtype, "status");
            assert_eq!(data["status"], "ready");
        } else {
            panic!("Expected System message");
        }
    }

    #[test]
    fn test_parse_result_message() {
        let json = json!({
            "type": "result",
            "subtype": "conversation_turn",
            "duration_ms": 1234,
            "duration_api_ms": 1000,
            "is_error": false,
            "num_turns": 1,
            "session_id": "test_session",
            "total_cost_usd": 0.001
        });

        let result = parse_message(json).unwrap();
        assert!(result.is_some());

        if let Some(Message::Result {
            subtype,
            duration_ms,
            session_id,
            total_cost_usd,
            ..
        }) = result
        {
            assert_eq!(subtype, "conversation_turn");
            assert_eq!(duration_ms, 1234);
            assert_eq!(session_id, "test_session");
            assert_eq!(total_cost_usd, Some(0.001));
        } else {
            panic!("Expected Result message");
        }
    }

    #[test]
    fn test_parse_result_message_structured_output_alias() {
        let json = json!({
            "type": "result",
            "subtype": "conversation_turn",
            "duration_ms": 1,
            "duration_api_ms": 1,
            "is_error": false,
            "num_turns": 1,
            "session_id": "test_session",
            "structuredOutput": {"answer": 42}
        });

        let result = parse_message(json).unwrap();
        assert!(result.is_some());

        if let Some(Message::Result {
            structured_output, ..
        }) = result
        {
            assert_eq!(structured_output, Some(json!({"answer": 42})));
        } else {
            panic!("Expected Result message");
        }
    }

    #[test]
    fn test_parse_unknown_message_type() {
        let json = json!({
            "type": "unknown_type",
            "data": "some data"
        });

        let result = parse_message(json).unwrap();
        assert!(result.is_none());
    }

    // === Sidechain / parent_tool_use_id tests ===

    #[test]
    fn test_parse_assistant_message_with_parent_tool_use_id() {
        let json = json!({
            "type": "assistant",
            "parent_tool_use_id": "toolu_abc123",
            "message": {
                "role": "assistant",
                "content": [
                    {
                        "type": "text",
                        "text": "Subagent response"
                    }
                ]
            }
        });

        let result = parse_message(json).unwrap();
        assert!(result.is_some());

        if let Some(Message::Assistant {
            message,
            parent_tool_use_id,
        }) = result
        {
            assert_eq!(message.content.len(), 1);
            assert_eq!(parent_tool_use_id, Some("toolu_abc123".to_string()));
            if let ContentBlock::Text(text) = &message.content[0] {
                assert_eq!(text.text, "Subagent response");
            } else {
                panic!("Expected Text content block");
            }
        } else {
            panic!("Expected Assistant message");
        }
    }

    #[test]
    fn test_parse_user_message_with_parent_tool_use_id() {
        let json = json!({
            "type": "user",
            "parent_tool_use_id": "toolu_xyz789",
            "message": {
                "role": "user",
                "content": "Subagent user prompt"
            }
        });

        let result = parse_message(json).unwrap();
        assert!(result.is_some());

        if let Some(Message::User {
            message,
            parent_tool_use_id,
        }) = result
        {
            assert_eq!(message.content, "Subagent user prompt");
            assert_eq!(parent_tool_use_id, Some("toolu_xyz789".to_string()));
        } else {
            panic!("Expected User message");
        }
    }

    #[test]
    fn test_is_sidechain_helper() {
        // Top-level message (no parent_tool_use_id)
        let top_level = Message::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent {
                    text: "Hello".to_string(),
                })],
            },
            parent_tool_use_id: None,
        };
        assert!(!top_level.is_sidechain());
        assert!(top_level.is_top_level());
        assert!(top_level.parent_tool_use_id().is_none());

        // Sidechain message (has parent_tool_use_id)
        let sidechain = Message::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent {
                    text: "Subagent response".to_string(),
                })],
            },
            parent_tool_use_id: Some("toolu_abc123".to_string()),
        };
        assert!(sidechain.is_sidechain());
        assert!(!sidechain.is_top_level());
        assert_eq!(sidechain.parent_tool_use_id(), Some("toolu_abc123"));

        // System messages are never sidechains
        let system = Message::System {
            subtype: "status".to_string(),
            data: json!({}),
        };
        assert!(!system.is_sidechain());
        assert!(system.is_top_level());

        // Result messages are never sidechains
        let result = Message::Result {
            subtype: "done".to_string(),
            duration_ms: 100,
            duration_api_ms: 80,
            is_error: false,
            num_turns: 1,
            session_id: "test".to_string(),
            total_cost_usd: None,
            usage: None,
            result: None,
            structured_output: None,
        };
        assert!(!result.is_sidechain());
        assert!(result.is_top_level());
    }

    #[test]
    fn test_user_message_is_sidechain() {
        let sidechain_user = Message::User {
            message: UserMessage {
                content: "subagent prompt".to_string(),
                content_blocks: None,
            },
            parent_tool_use_id: Some("toolu_def456".to_string()),
        };
        assert!(sidechain_user.is_sidechain());
        assert_eq!(sidechain_user.parent_tool_use_id(), Some("toolu_def456"));
    }

    #[test]
    fn test_parse_user_message_with_tool_result_array() {
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_abc123",
                        "content": "File contents here...",
                        "is_error": false
                    }
                ]
            }
        });

        let result = parse_message(json).unwrap();
        assert!(
            result.is_some(),
            "User message with tool_result array should be parsed, not skipped"
        );
        let msg = result.unwrap();

        if let Message::User { message, .. } = &msg {
            assert!(
                message.content.is_empty(),
                "Text content should be empty for tool-result-only messages"
            );
            assert!(
                message.content_blocks.is_some(),
                "content_blocks should be present"
            );
            let blocks = message.content_blocks.as_ref().unwrap();
            assert_eq!(blocks.len(), 1);
            assert!(
                matches!(&blocks[0], ContentBlock::ToolResult(tr) if tr.tool_use_id == "toolu_abc123")
            );
        } else {
            panic!("Expected Message::User, got {:?}", msg);
        }
    }

    #[test]
    fn test_parse_user_message_with_multiple_tool_results() {
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_001",
                        "content": "Result 1",
                        "is_error": false
                    },
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_002",
                        "content": "Error occurred",
                        "is_error": true
                    }
                ]
            }
        });

        let result = parse_message(json).unwrap().unwrap();
        if let Message::User { message, .. } = &result {
            let blocks = message.content_blocks.as_ref().unwrap();
            assert_eq!(blocks.len(), 2);
            assert!(
                matches!(&blocks[0], ContentBlock::ToolResult(tr) if tr.tool_use_id == "toolu_001" && tr.is_error == Some(false))
            );
            assert!(
                matches!(&blocks[1], ContentBlock::ToolResult(tr) if tr.tool_use_id == "toolu_002" && tr.is_error == Some(true))
            );
        } else {
            panic!("Expected Message::User");
        }
    }

    #[test]
    fn test_parse_user_message_string_content_has_no_blocks() {
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "content": "Hello, just a normal message"
            }
        });

        let result = parse_message(json).unwrap().unwrap();
        if let Message::User { message, .. } = &result {
            assert_eq!(message.content, "Hello, just a normal message");
            assert!(message.content_blocks.is_none());
        } else {
            panic!("Expected Message::User");
        }
    }
}
