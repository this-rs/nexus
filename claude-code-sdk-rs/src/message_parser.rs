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

    // The CLI emits system messages in two shapes:
    //   1. Nested: the payload lives under a "data" object.
    //   2. Flat (current CLI): the payload is carried as top-level sibling fields,
    //      with NO "data" key. This is how `init`, `status`, and ALL Dynamic
    //      Workflow lifecycle events arrive — e.g.
    //      `{"type":"system","subtype":"task_progress","task_id":"...",
    //        "workflow_progress":[...],"usage":{...},...}`.
    // The old code only copied "data", so flat messages surfaced as
    // `System { subtype, data: {} }` — the subtype was kept but the entire
    // payload (task ids, per-agent fan-out state, usage, completion status) was
    // silently dropped, leaving consumers blind to workflow progress.
    //
    // Preserve the payload in BOTH shapes: prefer an explicit "data" object when
    // present, otherwise gather every top-level field except the envelope keys
    // ("type"/"subtype"). An envelope-only message still yields an empty object.
    //
    // A `"data": null` is treated as *absent*, not as an explicit payload: null
    // carries nothing, so taking that branch would throw away exactly the flat
    // sibling fields this code exists to keep. See
    // `test_parse_system_message_null_data_falls_back_to_flat_payload`.
    let data = if let Some(d) = json.get("data").filter(|d| !d.is_null()) {
        d.clone()
    } else if let Value::Object(map) = &json {
        let mut payload = map.clone();
        payload.remove("type");
        payload.remove("subtype");
        // Only reachable holding a null (a non-null "data" took the branch
        // above), so this strips the empty key rather than re-injecting it.
        payload.remove("data");
        Value::Object(payload)
    } else {
        Value::Object(serde_json::Map::new())
    };

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

    // === Additional coverage tests ===

    #[test]
    fn test_parse_message_missing_type_field() {
        let json = json!({"data": "no type here"});
        let result = parse_message(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_user_message_missing_message_field() {
        let json = json!({"type": "user"});
        let result = parse_message(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_user_message_invalid_content() {
        let json = json!({
            "type": "user",
            "message": {
                "content": 12345
            }
        });
        let result = parse_message(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_user_message_empty_array_content() {
        let json = json!({
            "type": "user",
            "message": {
                "content": []
            }
        });
        let result = parse_message(json).unwrap().unwrap();
        if let Message::User { message, .. } = &result {
            assert!(message.content.is_empty());
            assert!(
                message.content_blocks.is_none(),
                "Empty blocks should become None"
            );
        } else {
            panic!("Expected Message::User");
        }
    }

    #[test]
    fn test_parse_assistant_message_missing_message_field() {
        let json = json!({"type": "assistant"});
        let result = parse_message(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_assistant_message_content_not_array() {
        let json = json!({
            "type": "assistant",
            "message": {
                "content": "this is a string, not an array"
            }
        });
        let result = parse_message(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_content_block_text_missing_text() {
        let json = json!({"type": "text"});
        let result = parse_content_block(&json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_content_block_thinking_missing_thinking() {
        let json = json!({"type": "thinking", "signature": "sig"});
        let result = parse_content_block(&json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_content_block_thinking_missing_signature() {
        let json = json!({"type": "thinking", "thinking": "hmm"});
        let result = parse_content_block(&json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_content_block_tool_use_missing_id() {
        let json = json!({"type": "tool_use", "name": "read_file", "input": {}});
        let result = parse_content_block(&json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_content_block_tool_use_missing_name() {
        let json = json!({"type": "tool_use", "id": "tool_1", "input": {}});
        let result = parse_content_block(&json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_content_block_tool_use_no_input_defaults() {
        let json = json!({"type": "tool_use", "id": "tool_1", "name": "my_tool"});
        let result = parse_content_block(&json).unwrap().unwrap();
        if let ContentBlock::ToolUse(tu) = result {
            assert_eq!(tu.id, "tool_1");
            assert_eq!(tu.name, "my_tool");
            assert_eq!(tu.input, json!({}));
        } else {
            panic!("Expected ToolUse block");
        }
    }

    #[test]
    fn test_parse_content_block_tool_result_missing_tool_use_id() {
        let json = json!({"type": "tool_result", "content": "result"});
        let result = parse_content_block(&json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_content_block_tool_result_structured_content() {
        let json = json!({
            "type": "tool_result",
            "tool_use_id": "toolu_1",
            "content": [{"type": "text", "text": "structured"}]
        });
        let result = parse_content_block(&json).unwrap().unwrap();
        if let ContentBlock::ToolResult(tr) = result {
            assert_eq!(tr.tool_use_id, "toolu_1");
            assert!(matches!(tr.content, Some(ContentValue::Structured(_))));
        } else {
            panic!("Expected ToolResult block");
        }
    }

    #[test]
    fn test_parse_content_block_tool_result_no_content() {
        let json = json!({
            "type": "tool_result",
            "tool_use_id": "toolu_1"
        });
        let result = parse_content_block(&json).unwrap().unwrap();
        if let ContentBlock::ToolResult(tr) = result {
            assert_eq!(tr.tool_use_id, "toolu_1");
            assert!(tr.content.is_none());
        } else {
            panic!("Expected ToolResult block");
        }
    }

    #[test]
    fn test_parse_content_block_unknown_type_returns_none() {
        let json = json!({"type": "image", "data": "base64..."});
        let result = parse_content_block(&json).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_content_block_no_type_with_text_backward_compat() {
        let json = json!({"text": "fallback text"});
        let result = parse_content_block(&json).unwrap().unwrap();
        if let ContentBlock::Text(t) = result {
            assert_eq!(t.text, "fallback text");
        } else {
            panic!("Expected Text block");
        }
    }

    #[test]
    fn test_parse_content_block_no_type_no_text_returns_none() {
        let json = json!({"something": "else"});
        let result = parse_content_block(&json).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_system_message_missing_subtype_defaults() {
        let json = json!({
            "type": "system",
            "data": {"key": "value"}
        });
        let result = parse_message(json).unwrap().unwrap();
        if let Message::System { subtype, data } = result {
            assert_eq!(subtype, "unknown");
            assert_eq!(data["key"], "value");
        } else {
            panic!("Expected System message");
        }
    }

    #[test]
    fn test_parse_system_message_missing_data_defaults() {
        let json = json!({
            "type": "system",
            "subtype": "info"
        });
        let result = parse_message(json).unwrap().unwrap();
        if let Message::System { subtype, data } = result {
            assert_eq!(subtype, "info");
            assert_eq!(data, json!({}));
        } else {
            panic!("Expected System message");
        }
    }

    #[test]
    fn test_parse_system_message_flat_workflow_event_preserves_payload() {
        // Real shape emitted by Claude Code Dynamic Workflows (CLI >= 2.1.154):
        // the payload is carried as TOP-LEVEL sibling fields, with no "data" key.
        // The parser must preserve the full payload so consumers can observe
        // workflow fan-out progress (task id, per-agent state, usage), not just
        // the subtype.
        let json = json!({
            "type": "system",
            "subtype": "task_progress",
            "task_id": "wbk63ch2d",
            "tool_use_id": "toolu_01EUAFxhpsLmYU8hJQN5ud8x",
            "usage": { "total_tokens": 7681, "duration_ms": 1400 },
            "workflow_progress": [
                { "type": "workflow_agent", "index": 1, "state": "start" }
            ],
            "session_id": "bf72e564-78cd-41d3-9dba-35a75d77c0ce"
        });
        let result = parse_message(json).unwrap().unwrap();
        if let Message::System { subtype, data } = result {
            assert_eq!(subtype, "task_progress");
            // Envelope keys are stripped...
            assert!(data.get("type").is_none());
            assert!(data.get("subtype").is_none());
            // ...but the workflow payload is fully preserved.
            assert_eq!(data["task_id"], "wbk63ch2d");
            assert_eq!(data["usage"]["total_tokens"], 7681);
            assert_eq!(data["workflow_progress"][0]["state"], "start");
            assert_eq!(data["session_id"], "bf72e564-78cd-41d3-9dba-35a75d77c0ce");
        } else {
            panic!("Expected System message");
        }
    }

    #[test]
    fn test_parse_system_message_nested_data_still_wins() {
        // Backward-compat: when an explicit "data" object IS present, it is used
        // verbatim and sibling fields are NOT merged in.
        let json = json!({
            "type": "system",
            "subtype": "status",
            "data": { "status": "ready" },
            "ignored_sibling": "should_not_leak"
        });
        let result = parse_message(json).unwrap().unwrap();
        if let Message::System { subtype, data } = result {
            assert_eq!(subtype, "status");
            assert_eq!(data["status"], "ready");
            assert!(data.get("ignored_sibling").is_none());
        } else {
            panic!("Expected System message");
        }
    }

    #[test]
    fn test_parse_result_message_fallback_path() {
        // Provide a JSON that will fail serde deserialization of Message
        // (e.g., missing required fields that serde expects but the fallback handles).
        // The serde path expects "type": "result" as the tag, but also needs
        // all required fields. We add an extra unrecognized field structure
        // that makes serde fail, triggering the fallback.
        let json = json!({
            "type": "result",
            "subtype": "conversation_turn",
            "duration_ms": 500,
            "duration_api_ms": 400,
            "is_error": true,
            "num_turns": 3,
            "session_id": "sess_fallback",
            "total_cost_usd": 0.05,
            "result": "some result text",
            "usage": {"input_tokens": 100},
            "structured_output": {"key": "val"}
        });

        // This should succeed via either serde or fallback
        let result = parse_message(json).unwrap().unwrap();
        if let Message::Result {
            subtype,
            duration_ms,
            duration_api_ms,
            is_error,
            num_turns,
            session_id,
            total_cost_usd,
            result,
            ..
        } = result
        {
            assert_eq!(subtype, "conversation_turn");
            assert_eq!(duration_ms, 500);
            assert_eq!(duration_api_ms, 400);
            assert!(is_error);
            assert_eq!(num_turns, 3);
            assert_eq!(session_id, "sess_fallback");
            assert_eq!(total_cost_usd, Some(0.05));
            assert_eq!(result, Some("some result text".to_string()));
        } else {
            panic!("Expected Result message");
        }
    }

    #[test]
    fn test_parse_result_message_fallback_with_minimal_fields() {
        // Force fallback by providing num_turns as a string (serde will reject it)
        // but the fallback parses it manually with defaults
        let json = json!({
            "type": "result",
            "num_turns": "not_a_number"
        });
        let result = parse_message(json).unwrap().unwrap();
        if let Message::Result {
            subtype,
            duration_ms,
            session_id,
            num_turns,
            is_error,
            ..
        } = result
        {
            assert_eq!(subtype, "unknown");
            assert_eq!(duration_ms, 0);
            assert_eq!(session_id, "unknown");
            assert_eq!(num_turns, 0);
            assert!(!is_error);
        } else {
            panic!("Expected Result message from fallback");
        }
    }

    #[test]
    fn test_parse_stream_event_missing_event_field() {
        let json = json!({"type": "stream_event"});
        let result = parse_message(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_stream_event_missing_event_type() {
        let json = json!({
            "type": "stream_event",
            "event": {}
        });
        let result = parse_message(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_stream_event_message_start() {
        let json = json!({
            "type": "stream_event",
            "event": {
                "type": "message_start",
                "message": {"id": "msg_1", "role": "assistant"}
            }
        });
        let result = parse_message(json).unwrap().unwrap();
        if let Message::StreamEvent { event, .. } = result {
            assert!(matches!(event, StreamEventData::MessageStart { .. }));
            if let StreamEventData::MessageStart { message } = event {
                assert_eq!(message["id"], "msg_1");
            }
        } else {
            panic!("Expected StreamEvent");
        }
    }

    #[test]
    fn test_parse_stream_event_content_block_start() {
        let json = json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }
        });
        let result = parse_message(json).unwrap().unwrap();
        if let Message::StreamEvent { event, .. } = result {
            if let StreamEventData::ContentBlockStart {
                index,
                content_block,
            } = event
            {
                assert_eq!(index, 0);
                assert_eq!(content_block["type"], "text");
            } else {
                panic!("Expected ContentBlockStart");
            }
        } else {
            panic!("Expected StreamEvent");
        }
    }

    #[test]
    fn test_parse_stream_event_content_block_delta_text() {
        let json = json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_delta",
                "index": 1,
                "delta": {
                    "type": "text_delta",
                    "text": "Hello"
                }
            }
        });
        let result = parse_message(json).unwrap().unwrap();
        if let Message::StreamEvent { event, .. } = result {
            if let StreamEventData::ContentBlockDelta { index, delta } = event {
                assert_eq!(index, 1);
                assert_eq!(
                    delta,
                    StreamDelta::TextDelta {
                        text: "Hello".to_string()
                    }
                );
            } else {
                panic!("Expected ContentBlockDelta");
            }
        } else {
            panic!("Expected StreamEvent");
        }
    }

    #[test]
    fn test_parse_stream_event_content_block_delta_thinking() {
        let json = json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_delta",
                "index": 0,
                "delta": {
                    "type": "thinking_delta",
                    "thinking": "Let me think..."
                }
            }
        });
        let result = parse_message(json).unwrap().unwrap();
        if let Message::StreamEvent { event, .. } = result {
            if let StreamEventData::ContentBlockDelta { delta, .. } = event {
                assert_eq!(
                    delta,
                    StreamDelta::ThinkingDelta {
                        thinking: "Let me think...".to_string()
                    }
                );
            } else {
                panic!("Expected ContentBlockDelta");
            }
        } else {
            panic!("Expected StreamEvent");
        }
    }

    #[test]
    fn test_parse_stream_event_content_block_delta_input_json() {
        let json = json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_delta",
                "index": 2,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": "{\"path\":"
                }
            }
        });
        let result = parse_message(json).unwrap().unwrap();
        if let Message::StreamEvent { event, .. } = result {
            if let StreamEventData::ContentBlockDelta { index, delta } = event {
                assert_eq!(index, 2);
                assert_eq!(
                    delta,
                    StreamDelta::InputJsonDelta {
                        partial_json: "{\"path\":".to_string()
                    }
                );
            } else {
                panic!("Expected ContentBlockDelta");
            }
        } else {
            panic!("Expected StreamEvent");
        }
    }

    #[test]
    fn test_parse_stream_event_content_block_delta_unknown_type() {
        let json = json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_delta",
                "index": 0,
                "delta": {
                    "type": "some_future_delta",
                    "text": "fallback text"
                }
            }
        });
        let result = parse_message(json).unwrap().unwrap();
        if let Message::StreamEvent { event, .. } = result {
            if let StreamEventData::ContentBlockDelta { delta, .. } = event {
                // Unknown delta type falls back to TextDelta
                assert_eq!(
                    delta,
                    StreamDelta::TextDelta {
                        text: "fallback text".to_string()
                    }
                );
            } else {
                panic!("Expected ContentBlockDelta");
            }
        } else {
            panic!("Expected StreamEvent");
        }
    }

    #[test]
    fn test_parse_stream_event_content_block_delta_missing_delta() {
        let json = json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_delta",
                "index": 0
            }
        });
        let result = parse_message(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_stream_event_content_block_stop() {
        let json = json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_stop",
                "index": 3
            }
        });
        let result = parse_message(json).unwrap().unwrap();
        if let Message::StreamEvent { event, .. } = result {
            assert_eq!(event, StreamEventData::ContentBlockStop { index: 3 });
        } else {
            panic!("Expected StreamEvent");
        }
    }

    #[test]
    fn test_parse_stream_event_message_delta() {
        let json = json!({
            "type": "stream_event",
            "event": {
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 50}
            }
        });
        let result = parse_message(json).unwrap().unwrap();
        if let Message::StreamEvent { event, .. } = result {
            if let StreamEventData::MessageDelta { delta, usage } = event {
                assert_eq!(delta["stop_reason"], "end_turn");
                assert!(usage.is_some());
                assert_eq!(usage.unwrap()["output_tokens"], 50);
            } else {
                panic!("Expected MessageDelta");
            }
        } else {
            panic!("Expected StreamEvent");
        }
    }

    #[test]
    fn test_parse_stream_event_message_stop() {
        let json = json!({
            "type": "stream_event",
            "event": {
                "type": "message_stop"
            }
        });
        let result = parse_message(json).unwrap().unwrap();
        if let Message::StreamEvent { event, .. } = result {
            assert_eq!(event, StreamEventData::MessageStop);
        } else {
            panic!("Expected StreamEvent");
        }
    }

    #[test]
    fn test_parse_stream_event_unknown_type_returns_none() {
        let json = json!({
            "type": "stream_event",
            "event": {
                "type": "some_future_event"
            }
        });
        let result = parse_message(json).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_stream_event_with_parent_tool_use_id() {
        let json = json!({
            "type": "stream_event",
            "parent_tool_use_id": "toolu_sub123",
            "session_id": "sess_abc",
            "event": {
                "type": "message_stop"
            }
        });
        let result = parse_message(json).unwrap().unwrap();
        if let Message::StreamEvent {
            event,
            session_id,
            parent_tool_use_id,
        } = result
        {
            assert_eq!(event, StreamEventData::MessageStop);
            assert_eq!(session_id, Some("sess_abc".to_string()));
            assert_eq!(parent_tool_use_id, Some("toolu_sub123".to_string()));
        } else {
            panic!("Expected StreamEvent");
        }
    }

    // ====================================================================
    // Blocks the parser drops, and the diagnostics that say so
    // ====================================================================

    /// A user message whose array content holds *only* block types the parser
    /// does not know collapses to "no content at all": empty `content` AND
    /// `content_blocks == None`. The message itself is still delivered, so a
    /// consumer that only looks at `content` sees an empty user turn rather
    /// than an error.
    #[test]
    fn test_parse_user_message_array_of_only_unknown_blocks_yields_nothing() {
        let json = json!({
            "type": "user",
            "message": {
                "content": [
                    {"type": "image", "source": {"data": "iVBOR"}},
                    {"type": "server_tool_use", "id": "srvtoolu_1"}
                ]
            }
        });
        let result = parse_message(json).unwrap().unwrap();
        match &result {
            Message::User { message, .. } => {
                assert_eq!(message.content, "");
                assert_eq!(
                    message.content_blocks, None,
                    "unknown blocks are dropped, and an all-dropped array becomes None"
                );
            },
            other => panic!("Expected Message::User, got {other:?}"),
        }
    }

    /// Same drop on the assistant side, but mixed with a known block: the
    /// unknown block is skipped and the surviving blocks keep their relative
    /// order (the dropped one does not leave a hole or shift anything).
    #[test]
    fn test_parse_assistant_message_skips_unknown_blocks_and_keeps_order() {
        let json = json!({
            "type": "assistant",
            "message": {
                "content": [
                    {"type": "text", "text": "first"},
                    {"type": "redacted_thinking", "data": "opaque"},
                    {"type": "text", "text": "second"}
                ]
            }
        });
        let result = parse_message(json).unwrap().unwrap();
        match &result {
            Message::Assistant { message, .. } => {
                assert_eq!(
                    message.content,
                    vec![
                        ContentBlock::Text(TextContent {
                            text: "first".to_string()
                        }),
                        ContentBlock::Text(TextContent {
                            text: "second".to_string()
                        }),
                    ],
                    "redacted_thinking is dropped silently, order of survivors preserved"
                );
            },
            other => panic!("Expected Message::Assistant, got {other:?}"),
        }
    }

    /// A *malformed* known block inside a user array is NOT dropped: the `?`
    /// in the loop aborts the whole message. One bad tool_result loses the
    /// entire user turn, including the sibling blocks that were fine.
    #[test]
    fn test_parse_user_message_array_with_malformed_known_block_fails_whole_message() {
        let json = json!({
            "type": "user",
            "message": {
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_ok", "content": "fine"},
                    {"type": "text"}
                ]
            }
        });
        let err = parse_message(json).unwrap_err();
        match &err {
            SdkError::MessageParseError { error, .. } => {
                assert_eq!(error, "Missing 'text' field in text block");
            },
            other => panic!("Expected MessageParseError, got {other:?}"),
        }
    }

    /// Minimal in-memory `MakeWriter` so a test can read back what `tracing`
    /// actually emitted.
    #[derive(Clone, Default)]
    struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl CapturedLog {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().expect("log mutex").clone()).expect("utf8 log")
        }
    }

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log mutex").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for CapturedLog {
        type Writer = CapturedLog;
        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    /// The block count in the DEBUG line is the only trace a dropped block
    /// leaves. This asserts the number is the count of blocks *kept* (2), not
    /// the number of items received (3) — otherwise the log would hide the drop.
    ///
    /// Coverage note: `llvm-cov` still reports the `blocks.len()` argument of
    /// that `debug!` as uncovered even though this test reads the rendered line
    /// back. `tracing` passes the argument through `format_args!`, so the value
    /// is captured by reference here and only formatted later, inside the
    /// subscriber — the region llvm-cov maps onto that line is never entered.
    /// The behaviour is verified; the counter cannot be.
    #[test]
    fn test_user_message_debug_log_reports_kept_block_count() {
        let log = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(log.clone())
            .finish();

        let json = json!({
            "type": "user",
            "message": {
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "a"},
                    {"type": "image", "source": {"data": "iVBOR"}},
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": "b"}
                ]
            }
        });

        let parsed =
            tracing::subscriber::with_default(subscriber, || parse_message(json).unwrap().unwrap());

        match &parsed {
            Message::User { message, .. } => {
                assert_eq!(message.content_blocks.as_ref().map(Vec::len), Some(2));
            },
            other => panic!("Expected Message::User, got {other:?}"),
        }

        let emitted = log.contents();
        assert!(
            emitted.contains("Parsed user message with 2 content blocks"),
            "DEBUG line must report the count of blocks kept; got: {emitted}"
        );
    }

    /// `tool_result.content` is only read as a string or an array. Any other
    /// JSON — a number, an object, an explicit null — is neither converted nor
    /// refused: it becomes `content: None`, indistinguishable from a tool that
    /// returned nothing at all.
    #[test]
    fn test_parse_content_block_tool_result_non_string_non_array_content_is_dropped() {
        for payload in [json!(42), json!({"stdout": "ok"}), json!(null), json!(true)] {
            let block = parse_content_block(&json!({
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": payload
            }))
            .unwrap()
            .unwrap();
            assert_eq!(
                block,
                ContentBlock::ToolResult(ToolResultContent {
                    tool_use_id: "toolu_1".to_string(),
                    content: None,
                    is_error: None,
                }),
                "content {payload} is dropped rather than refused"
            );
        }
    }

    // ====================================================================
    // parse_system_message — the three shapes of `data`
    // ====================================================================

    /// Defensive `else` of the `Value::Object` match. Unreachable through
    /// `parse_message` (a non-object JSON has no "type" string, so the
    /// envelope check rejects it first), so it is exercised by calling the
    /// private function directly: a non-object payload yields `{}`, never a
    /// panic.
    #[test]
    fn test_parse_system_message_non_object_json_yields_empty_object() {
        let result = parse_system_message(Value::String("not an envelope".into()))
            .unwrap()
            .unwrap();
        assert_eq!(
            result,
            Message::System {
                subtype: "unknown".to_string(),
                data: json!({}),
            }
        );
    }

    /// Proof that the branch above really is out of reach from the public
    /// entry point: `parse_message` refuses a non-object before any subtype
    /// dispatch happens.
    #[test]
    fn test_parse_message_rejects_non_object_json() {
        let err = parse_message(json!(["system"])).unwrap_err();
        match &err {
            SdkError::MessageParseError { error, .. } => {
                assert_eq!(error, "Missing 'type' field");
            },
            other => panic!("Expected MessageParseError, got {other:?}"),
        }
    }

    /// `"type"` present but not a string is rejected the same way — the
    /// parser never coerces.
    #[test]
    fn test_parse_message_rejects_non_string_type() {
        let err = parse_message(json!({"type": 7})).unwrap_err();
        assert!(matches!(err, SdkError::MessageParseError { .. }));
    }

    /// An explicit `"data": null` must NOT win over the flat siblings: null
    /// carries no payload, so taking that branch would throw away exactly the
    /// workflow state the flat-shape handling exists to preserve.
    #[test]
    fn test_parse_system_message_null_data_falls_back_to_flat_payload() {
        let json = json!({
            "type": "system",
            "subtype": "task_progress",
            "data": null,
            "task_id": "task-42",
            "workflow_progress": [{"agent": "a1", "state": "running"}]
        });
        let result = parse_message(json).unwrap().unwrap();
        match &result {
            Message::System { subtype, data } => {
                assert_eq!(subtype, "task_progress");
                assert_eq!(data.get("task_id"), Some(&json!("task-42")));
                assert_eq!(
                    data.get("workflow_progress"),
                    Some(&json!([{"agent": "a1", "state": "running"}])),
                    "flat payload must survive an explicit null `data`"
                );
                assert!(
                    data.get("data").is_none(),
                    "the null `data` key itself is not re-injected into the payload"
                );
            },
            other => panic!("Expected Message::System, got {other:?}"),
        }
    }

    /// A non-null, non-object `data` is still taken verbatim: the parser does
    /// not require `data` to be a JSON object despite the field's name and
    /// doc. Pinned so a future tightening is a deliberate change.
    #[test]
    fn test_parse_system_message_scalar_data_is_taken_verbatim() {
        let json = json!({
            "type": "system",
            "subtype": "status",
            "data": "ready",
            "ignored_sibling": 1
        });
        let result = parse_message(json).unwrap().unwrap();
        assert_eq!(
            result,
            Message::System {
                subtype: "status".to_string(),
                data: json!("ready"),
            }
        );
    }

    // ====================================================================
    // parse_result_message — the fallback is a silent default machine
    // ====================================================================

    /// A result message with a well-formed envelope but a wrongly typed
    /// `duration_ms` does not fail: serde rejects it, the fallback kicks in,
    /// and the bad number becomes 0 with no error and no log. The surrounding
    /// fields are still recovered.
    #[test]
    fn test_parse_result_message_wrong_typed_duration_silently_becomes_zero() {
        let json = json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": "1500",
            "duration_api_ms": 1200,
            "is_error": false,
            "num_turns": 2,
            "session_id": "sess_x",
            "result": "done"
        });
        let result = parse_message(json).unwrap().unwrap();
        match &result {
            Message::Result {
                duration_ms,
                duration_api_ms,
                session_id,
                result: text,
                ..
            } => {
                assert_eq!(*duration_ms, 0, "a string duration is silently zeroed");
                assert_eq!(*duration_api_ms, 1200);
                assert_eq!(session_id, "sess_x");
                assert_eq!(text.as_deref(), Some("done"));
            },
            other => panic!("Expected Message::Result, got {other:?}"),
        }
    }

    /// `num_turns` is read as i64 then cast with `as i32`: a value past
    /// i32::MAX wraps instead of being refused. Pinned because the wrap is
    /// silent — the caller sees a negative turn count.
    #[test]
    fn test_parse_result_message_num_turns_wraps_past_i32() {
        let json = json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 1,
            "duration_api_ms": 1,
            "is_error": false,
            "num_turns": 2_147_483_648_i64,
            "session_id": "sess_x"
        });
        let result = parse_message(json).unwrap().unwrap();
        match &result {
            Message::Result { num_turns, .. } => {
                assert_eq!(
                    *num_turns,
                    i32::MIN,
                    "i64 -> i32 cast wraps rather than refusing the message"
                );
            },
            other => panic!("Expected Message::Result, got {other:?}"),
        }
    }

    /// A result message missing every required field still parses: the
    /// fallback invents "unknown"/0/false defaults. This is the shape a
    /// consumer must be ready for — `is_error == false` here means "the CLI
    /// told us nothing", not "the turn succeeded".
    #[test]
    fn test_parse_result_message_envelope_only_is_all_defaults() {
        let result = parse_message(json!({"type": "result"})).unwrap().unwrap();
        assert_eq!(
            result,
            Message::Result {
                subtype: "unknown".to_string(),
                duration_ms: 0,
                duration_api_ms: 0,
                is_error: false,
                num_turns: 0,
                session_id: "unknown".to_string(),
                total_cost_usd: None,
                usage: None,
                result: None,
                structured_output: None,
            }
        );
    }

    /// `structured_output: null` is normalised to `None` on the fallback path
    /// too, so consumers never have to distinguish `None` from `Some(null)`.
    #[test]
    fn test_parse_result_message_fallback_drops_null_structured_output() {
        let json = json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": "bad",
            "structured_output": null
        });
        let result = parse_message(json).unwrap().unwrap();
        match &result {
            Message::Result {
                structured_output, ..
            } => assert_eq!(*structured_output, None),
            other => panic!("Expected Message::Result, got {other:?}"),
        }
    }

    // ====================================================================
    // Parser strings vs. serde tags — the two must not drift
    // ====================================================================

    /// `parse_stream_event` matches event types as hand-written string
    /// literals, while `StreamEventData` derives its tag from
    /// `rename_all = "snake_case"`. If the two ever disagree, the SDK would
    /// emit a `type` it cannot itself parse back. This drives every variant
    /// the parser builds through parse -> serialize and asserts the tag it
    /// serialises to is the very string the parser accepted.
    #[test]
    fn test_stream_event_type_literals_match_serde_tags() {
        let cases: Vec<(&str, Value)> = vec![
            (
                "message_start",
                json!({"type": "message_start", "message": {"id": "m1"}}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "hi"}}),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 2}),
            ),
            (
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ];

        for (expected_tag, event) in cases {
            let parsed = parse_message(json!({"type": "stream_event", "event": event}))
                .unwrap()
                .unwrap_or_else(|| panic!("{expected_tag} must not be dropped"));
            let Message::StreamEvent { event: data, .. } = parsed else {
                panic!("{expected_tag}: expected StreamEvent");
            };
            let reserialised = serde_json::to_value(&data).unwrap();
            assert_eq!(
                reserialised.get("type").and_then(Value::as_str),
                Some(expected_tag),
                "serde tag must equal the literal parse_stream_event matches on"
            );
            // And the round-trip is lossless, so the SDK can re-read its own output.
            assert_eq!(
                serde_json::from_value::<StreamEventData>(reserialised).unwrap(),
                data
            );
        }
    }

    /// Same contract one level down, for the three delta kinds.
    #[test]
    fn test_stream_delta_type_literals_match_serde_tags() {
        let cases: Vec<(&str, Value)> = vec![
            ("text_delta", json!({"type": "text_delta", "text": "tok"})),
            (
                "thinking_delta",
                json!({"type": "thinking_delta", "thinking": "hmm"}),
            ),
            (
                "input_json_delta",
                json!({"type": "input_json_delta", "partial_json": "{\"a\":"}),
            ),
        ];

        for (expected_tag, delta) in cases {
            let parsed = parse_message(json!({
                "type": "stream_event",
                "event": {"type": "content_block_delta", "index": 0, "delta": delta}
            }))
            .unwrap()
            .unwrap();
            let Message::StreamEvent {
                event:
                    StreamEventData::ContentBlockDelta {
                        delta: parsed_delta,
                        ..
                    },
                ..
            } = parsed
            else {
                panic!("{expected_tag}: expected ContentBlockDelta");
            };
            let reserialised = serde_json::to_value(&parsed_delta).unwrap();
            assert_eq!(
                reserialised.get("type").and_then(Value::as_str),
                Some(expected_tag)
            );
            assert_eq!(
                serde_json::from_value::<StreamDelta>(reserialised).unwrap(),
                parsed_delta
            );
        }
    }

    /// A negative or non-integral `index` is not refused — `as_u64()` returns
    /// None and the block index silently becomes 0, which is a *valid* index.
    /// A consumer reassembling blocks by index would merge the wrong block.
    #[test]
    fn test_parse_stream_event_negative_index_silently_becomes_zero() {
        for bad_index in [json!(-1), json!(1.5), json!("3")] {
            let parsed = parse_message(json!({
                "type": "stream_event",
                "event": {"type": "content_block_stop", "index": bad_index}
            }))
            .unwrap()
            .unwrap();
            assert_eq!(
                parsed,
                Message::StreamEvent {
                    event: StreamEventData::ContentBlockStop { index: 0 },
                    session_id: None,
                    parent_tool_use_id: None,
                },
                "index {bad_index} should have been refused, it collapses to 0 instead"
            );
        }
    }

    /// Parsing the CLI's JSON and reading the result back through **serde**
    /// is lossless for every message kind. This is the property a consumer
    /// relies on when it persists `Message` values and reloads them.
    #[test]
    fn test_parsed_messages_survive_a_serde_round_trip() {
        let inputs = vec![
            json!({"type": "user", "message": {"content": "hello"}, "parent_tool_use_id": "toolu_p"}),
            json!({
                "type": "assistant",
                "message": {"content": [
                    {"type": "text", "text": "hi"},
                    {"type": "thinking", "thinking": "t", "signature": "s"},
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "/a"}}
                ]}
            }),
            json!({"type": "system", "subtype": "init", "data": {"k": "v"}}),
            json!({
                "type": "result", "subtype": "success", "duration_ms": 5, "duration_api_ms": 4,
                "is_error": false, "num_turns": 1, "session_id": "s1", "total_cost_usd": 0.01
            }),
            json!({
                "type": "stream_event", "session_id": "s1",
                "event": {"type": "content_block_delta", "index": 0,
                          "delta": {"type": "text_delta", "text": "x"}}
            }),
        ];

        for input in inputs {
            let first = parse_message(input.clone()).unwrap().unwrap();
            let round = serde_json::to_value(&first).unwrap();
            let second: Message = serde_json::from_value(round.clone())
                .unwrap_or_else(|e| panic!("serde could not re-read {input}: {e}"));
            assert_eq!(first, second, "round trip changed the message: {round}");
        }
    }

    /// But the *parser* cannot re-read what the SDK serialises. Because
    /// `ContentBlock` is `#[serde(untagged)]`, serialisation emits no `"type"`,
    /// and `parse_content_block()` only has a no-`type` fallback for text
    /// blocks. So feeding the SDK's own output back through `parse_message()`
    /// **silently drops every thinking and tool_use block** — no error, no
    /// warning, a shorter content array.
    ///
    /// Pinning the broken behaviour here rather than fixing it: the fix is
    /// either a tag on `ContentBlock` (changes the wire format for every other
    /// consumer of the type) or shape-guessing in `parse_content_block()`
    /// (guesses where the CLI is explicit). Both are larger than this file.
    /// See `test_reparsing_own_output_should_preserve_all_blocks` below for the
    /// behaviour that is wanted.
    #[test]
    fn test_reparsing_the_sdks_own_output_drops_thinking_and_tool_use_blocks() {
        let original = parse_message(json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "text", "text": "hi"},
                {"type": "thinking", "thinking": "t", "signature": "s"},
                {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "/a"}}
            ]}
        }))
        .unwrap()
        .unwrap();

        let serialised = serde_json::to_value(&original).unwrap();
        let reparsed = parse_message(serialised).unwrap().unwrap();

        let Message::Assistant { message, .. } = &reparsed else {
            panic!("expected Message::Assistant");
        };
        assert_eq!(
            message.content,
            vec![ContentBlock::Text(TextContent {
                text: "hi".to_string()
            })],
            "only the text block survives a re-parse of the SDK's own output"
        );
        assert_ne!(original, reparsed, "the loss is silent, not an error");
    }

    /// The behaviour that ought to hold: `parse_message()` is idempotent over
    /// the SDK's own serialisation. Fails today because the untagged
    /// `Serialize` for `types::ContentBlock` writes no `"type"` key and
    /// `parse_content_block()` drops any untyped non-text block.
    /// Triggering input: an assistant message containing a `thinking` or
    /// `tool_use` block, serialised then re-parsed.
    #[test]
    #[ignore = "bug: untagged ContentBlock Serialize emits no `type`; parse_content_block drops untyped non-text blocks"]
    fn test_reparsing_own_output_should_preserve_all_blocks() {
        let original = parse_message(json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "thinking", "thinking": "t", "signature": "s"},
                {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "/a"}}
            ]}
        }))
        .unwrap()
        .unwrap();
        let serialised = serde_json::to_value(&original).unwrap();
        let reparsed = parse_message(serialised).unwrap().unwrap();
        assert_eq!(original, reparsed);
    }

    /// The round trip above hides one real asymmetry: `ContentBlock` is
    /// `#[serde(untagged)]`, so serialising an assistant message emits content
    /// blocks with **no `"type"` discriminator**. It round-trips only because
    /// untagged deserialisation guesses from the field shape — the output is
    /// not the wire format the CLI itself produces.
    #[test]
    fn test_serialised_content_blocks_lose_their_type_discriminator() {
        let parsed = parse_message(json!({
            "type": "assistant",
            "message": {"content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}]}
        }))
        .unwrap()
        .unwrap();

        let out = serde_json::to_value(&parsed).unwrap();
        let block = &out["message"]["content"][0];
        assert_eq!(
            block,
            &json!({"id": "toolu_1", "name": "Bash", "input": {}}),
            "untagged ContentBlock drops `type` on the way out"
        );
        assert!(block.get("type").is_none());
    }

    /// Unknown envelope types are dropped, not rejected: `Ok(None)`. Covers
    /// the forward-compatibility contract for message types the CLI adds
    /// later.
    #[test]
    fn test_unknown_envelope_and_stream_event_types_are_dropped_not_errors() {
        assert_eq!(
            parse_message(json!({"type": "compact_boundary"})).unwrap(),
            None
        );
        assert_eq!(
            parse_message(json!({
                "type": "stream_event",
                "event": {"type": "message_pause"}
            }))
            .unwrap(),
            None
        );
    }
}
