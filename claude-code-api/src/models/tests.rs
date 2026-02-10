#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use super::super::claude::*;
    use super::super::openai::*;

    #[test]
    fn test_chat_message_serialization() {
        let message = ChatMessage {
            role: "user".to_string(),
            content: Some(MessageContent::Text("Hello".to_string())),
            name: None,
            tool_calls: None,
        };

        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"Hello\""));
    }

    #[test]
    fn test_claude_model_list() {
        let models = ClaudeModel::all();
        assert_eq!(models.len(), 8); // 3 Claude 4 + 2 Claude 3.7 + 2 Claude 3.5 + 1 Claude 3

        let model_ids: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
        // Check Claude 4 models
        assert!(model_ids.contains(&"claude-opus-4-1-20250805".to_string()));
        assert!(model_ids.contains(&"claude-opus-4-20250514".to_string()));
        assert!(model_ids.contains(&"claude-sonnet-4-20250514".to_string()));
        // Check Claude 3.7 models
        assert!(model_ids.contains(&"claude-3-7-sonnet-20250219".to_string()));
        assert!(model_ids.contains(&"claude-3-7-sonnet-latest".to_string()));
        // Check Claude 3.5 models
        assert!(model_ids.contains(&"claude-3-5-haiku-20241022".to_string()));
        assert!(model_ids.contains(&"claude-3-5-haiku-latest".to_string()));
        // Check Claude 3 models
        assert!(model_ids.contains(&"claude-3-haiku-20240307".to_string()));
    }

    #[test]
    fn test_message_content_variants() {
        let text_content = MessageContent::Text("Hello".to_string());
        let array_content = MessageContent::Array(vec![
            ContentPart::Text {
                text: "Hello".to_string(),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "https://example.com/image.png".to_string(),
                    detail: Some("high".to_string()),
                },
            },
        ]);

        let text_json = serde_json::to_string(&text_content).unwrap();
        assert_eq!(text_json, "\"Hello\"");

        let array_json = serde_json::to_string(&array_content).unwrap();
        assert!(array_json.contains("\"type\":\"text\""));
        assert!(array_json.contains("\"type\":\"image_url\""));
    }

    // === Tool call serialization tests ===

    #[test]
    fn test_tool_call_serialization() {
        let tool_call = ToolCall {
            id: "toolu_abc123".to_string(),
            tool_type: "function".to_string(),
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: r#"{"path":"/tmp/test.txt"}"#.to_string(),
            },
        };

        let json = serde_json::to_string(&tool_call).unwrap();
        assert!(json.contains("\"id\":\"toolu_abc123\""));
        assert!(json.contains("\"type\":\"function\""));
        assert!(json.contains("\"name\":\"read_file\""));
        assert!(json.contains("\"arguments\""));

        // Deserialize back
        let deserialized: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "toolu_abc123");
        assert_eq!(deserialized.function.name, "read_file");
    }

    #[test]
    fn test_chat_message_with_tool_calls() {
        // Assistant message with tool_calls and no text content (pure tool call)
        let message = ChatMessage {
            role: "assistant".to_string(),
            content: None,
            name: None,
            tool_calls: Some(vec![
                ToolCall {
                    id: "call_1".to_string(),
                    tool_type: "function".to_string(),
                    function: FunctionCall {
                        name: "Bash".to_string(),
                        arguments: r#"{"command":"ls"}"#.to_string(),
                    },
                },
                ToolCall {
                    id: "call_2".to_string(),
                    tool_type: "function".to_string(),
                    function: FunctionCall {
                        name: "Read".to_string(),
                        arguments: r#"{"file_path":"/tmp/f.txt"}"#.to_string(),
                    },
                },
            ]),
        };

        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains("\"tool_calls\""));
        assert!(json.contains("\"Bash\""));
        assert!(json.contains("\"Read\""));
        // content should not appear (skip_serializing_if = None)
        // tool_calls should have 2 elements
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["tool_calls"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_chat_message_with_text_and_tool_calls() {
        // Assistant message with both text content AND tool_calls
        let message = ChatMessage {
            role: "assistant".to_string(),
            content: Some(MessageContent::Text(
                "I'll read that file for you.".to_string(),
            )),
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                tool_type: "function".to_string(),
                function: FunctionCall {
                    name: "Read".to_string(),
                    arguments: r#"{"file_path":"/tmp/test.txt"}"#.to_string(),
                },
            }]),
        };

        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains("I'll read that file for you."));
        assert!(json.contains("\"tool_calls\""));
    }

    #[test]
    fn test_delta_tool_call_serialization() {
        let delta = DeltaMessage {
            role: None,
            content: None,
            tool_calls: Some(vec![DeltaToolCall {
                index: 0,
                id: Some("toolu_xyz".to_string()),
                tool_type: Some("function".to_string()),
                function: Some(DeltaFunctionCall {
                    name: Some("Bash".to_string()),
                    arguments: Some(r#"{"command":"pwd"}"#.to_string()),
                }),
            }]),
        };

        let json = serde_json::to_string(&delta).unwrap();
        assert!(json.contains("\"tool_calls\""));
        assert!(json.contains("\"index\":0"));
        assert!(json.contains("\"id\":\"toolu_xyz\""));
        assert!(json.contains("\"type\":\"function\""));
        assert!(json.contains("\"name\":\"Bash\""));
        assert!(json.contains("\"arguments\""));

        // Verify no role/content fields when None (skip_serializing_if)
        assert!(!json.contains("\"role\""));
        assert!(!json.contains("\"content\""));
    }

    #[test]
    fn test_delta_message_default_has_no_tool_calls() {
        let delta = DeltaMessage::default();
        let json = serde_json::to_string(&delta).unwrap();
        // Default should serialize to empty object (all fields None → skipped)
        assert_eq!(json, "{}");
    }

    #[test]
    fn test_delta_tool_call_partial_function() {
        // In streaming, subsequent chunks may only have index + arguments (no id/type/name)
        let delta = DeltaToolCall {
            index: 0,
            id: None,
            tool_type: None,
            function: Some(DeltaFunctionCall {
                name: None,
                arguments: Some(r#"partial_json"#.to_string()),
            }),
        };

        let json = serde_json::to_string(&delta).unwrap();
        assert!(json.contains("\"index\":0"));
        assert!(json.contains("\"arguments\":\"partial_json\""));
        // These should be absent (skip_serializing_if = None)
        assert!(!json.contains("\"id\""));
        assert!(!json.contains("\"type\""));
        assert!(!json.contains("\"name\""));
    }
}
