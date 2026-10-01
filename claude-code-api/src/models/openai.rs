use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub n: Option<i32>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub stop: Option<Vec<String>>,
    #[serde(default)]
    pub max_tokens: Option<i32>,
    #[serde(default)]
    pub presence_penalty: Option<f32>,
    #[serde(default)]
    pub frequency_penalty: Option<f32>,
    #[serde(default)]
    pub logit_bias: Option<Value>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub tools: Option<Vec<Tool>>,
    #[serde(default)]
    pub tool_choice: Option<ToolChoice>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<MessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Array(Vec<ContentPart>),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ImageUrl {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatChoice>,
    pub usage: Usage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatChoice {
    pub index: i32,
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Usage {
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChatCompletionStreamResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<StreamChoice>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StreamChoice {
    pub index: i32,
    pub delta: DeltaMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct DeltaMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<DeltaToolCall>>,
}

/// Tool call delta for streaming responses (OpenAI format).
/// First chunk includes index + id + type + function.name + function.arguments (partial).
/// Subsequent chunks include index + function.arguments (partial).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DeltaToolCall {
    pub index: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub tool_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<DeltaFunctionCall>,
}

/// Function call delta for streaming tool calls.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DeltaFunctionCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Model {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub owned_by: String,
}

// Tool calling support (functions are deprecated)
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Tool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDefinition,
}

/// `tool_choice`, in the two shapes OpenAI defines: a mode string, or a named
/// function.
///
/// Both used to be unreachable. `Auto` and `None` were **unit** variants of an
/// `untagged` enum, and an untagged unit variant only ever matches `null` — which
/// `Option<ToolChoice>` swallows first. So `"tool_choice": "auto"`, the form every
/// OpenAI client sends, was refused with *"data did not match any variant of
/// untagged enum ToolChoice"* (HTTP 422), `ToolChoice::None` could never be
/// produced from a request, and both variants serialised back out as `null`.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum ToolChoice {
    /// `"auto"`, `"none"` or `"required"`.
    Mode(ToolChoiceMode),
    /// `{"type": "function", "function": {"name": "…"}}`
    Tool {
        #[serde(rename = "type")]
        tool_type: String,
        function: ToolChoiceFunction,
    },
}

/// The string forms of [`ToolChoice`].
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoiceMode {
    Auto,
    None,
    Required,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ToolChoiceFunction {
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ModelList {
    pub object: String,
    pub data: Vec<Model>,
}

impl Default for ChatCompletionRequest {
    fn default() -> Self {
        Self {
            model: "claude-3-opus-20240229".to_string(),
            messages: vec![],
            temperature: None,
            top_p: None,
            n: Some(1),
            stream: Some(false),
            stop: None,
            max_tokens: None,
            presence_penalty: None,
            frequency_penalty: None,
            logit_bias: None,
            user: None,
            conversation_id: None,
            tools: None,
            tool_choice: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_request(json: serde_json::Value) -> Result<ChatCompletionRequest, serde_json::Error> {
        serde_json::from_value(json)
    }

    fn minimal() -> serde_json::Value {
        serde_json::json!({
            "model": "claude-sonnet-4",
            "messages": [{"role": "user", "content": "salut"}],
        })
    }

    // ── what the wire format must accept ──

    #[test]
    fn a_minimal_request_needs_only_model_and_messages() {
        let request = parse_request(minimal()).expect("the OpenAI minimum must parse");
        assert_eq!(request.model, "claude-sonnet-4");
        assert_eq!(request.messages.len(), 1);
        // `#[serde(default)]` everywhere else, so every option is absent, not 1/false.
        assert_eq!(request.n, None);
        assert_eq!(request.stream, None);
        assert!(request.tools.is_none());
        assert!(request.tool_choice.is_none());
    }

    #[test]
    fn model_and_messages_are_mandatory() {
        assert!(parse_request(serde_json::json!({"messages": []})).is_err());
        assert!(parse_request(serde_json::json!({"model": "m"})).is_err());
    }

    #[test]
    fn string_content_parses_as_text_and_array_content_as_parts() {
        let request = parse_request(serde_json::json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "plain"},
                {"role": "user", "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA"}},
                ]},
            ],
        }))
        .expect("both content shapes are OpenAI-legal");

        assert!(matches!(
            request.messages[0].content,
            Some(MessageContent::Text(ref t)) if t == "plain"
        ));
        match request.messages[1].content.as_ref().expect("array content") {
            MessageContent::Array(parts) => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(parts[0], ContentPart::Text { ref text } if text == "look"));
                match &parts[1] {
                    ContentPart::ImageUrl { image_url } => {
                        assert!(image_url.url.starts_with("data:image/png"));
                        assert_eq!(image_url.detail, None);
                    },
                    other => panic!("expected an image part, got {other:?}"),
                }
            },
            other => panic!("expected array content, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_content_part_type_is_refused() {
        // `#[serde(tag = "type")]` is strict here, which is the right call: a part
        // the gateway cannot forward must not be silently dropped.
        let error = parse_request(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": [{"type": "audio", "data": "x"}]}],
        }))
        .expect_err("an unsupported part must be refused");
        // …but `untagged` on `MessageContent` throws the inner cause away, so the
        // 422 the client gets never names the part it should remove.
        let message = error.to_string();
        assert!(
            !message.contains("audio"),
            "documents the gap: the real cause would be useful here ({message})"
        );
    }

    #[test]
    fn a_message_may_carry_no_content_at_all() {
        // An assistant turn that only calls a tool has `content: null`.
        let request = parse_request(serde_json::json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "ls", "arguments": "{}"},
                }],
            }],
        }))
        .expect("a tool-only assistant turn is legal");

        assert!(request.messages[0].content.is_none());
        let calls = request.messages[0].tool_calls.as_ref().expect("tool_calls");
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].tool_type, "function");
        assert_eq!(calls[0].function.name, "ls");
    }

    // ── tool_choice: the three shapes OpenAI defines ──

    #[test]
    fn tool_choice_accepts_the_openai_string_modes() {
        for (json, expected) in [
            ("auto", ToolChoiceMode::Auto),
            ("none", ToolChoiceMode::None),
            ("required", ToolChoiceMode::Required),
        ] {
            let mut body = minimal();
            body["tool_choice"] = serde_json::Value::String(json.to_string());
            let request = parse_request(body)
                .unwrap_or_else(|e| panic!("tool_choice {json:?} must parse: {e}"));
            assert_eq!(
                request.tool_choice,
                Some(ToolChoice::Mode(expected)),
                "tool_choice {json:?}"
            );
        }
    }

    #[test]
    fn tool_choice_accepts_a_named_function() {
        let mut body = minimal();
        body["tool_choice"] =
            serde_json::json!({"type": "function", "function": {"name": "search"}});
        let request = parse_request(body).expect("a forced tool is OpenAI-legal");

        match request.tool_choice.expect("tool_choice") {
            ToolChoice::Tool {
                tool_type,
                function,
            } => {
                assert_eq!(tool_type, "function");
                assert_eq!(function.name, "search");
            },
            other => panic!("expected a forced tool, got {other:?}"),
        }
    }

    #[test]
    fn tool_choice_modes_serialise_back_to_distinct_strings() {
        let rendered = |mode| serde_json::to_value(ToolChoice::Mode(mode)).unwrap();
        assert_eq!(rendered(ToolChoiceMode::Auto), serde_json::json!("auto"));
        assert_eq!(rendered(ToolChoiceMode::None), serde_json::json!("none"));
        assert_eq!(
            rendered(ToolChoiceMode::Required),
            serde_json::json!("required")
        );
    }

    #[test]
    fn an_unknown_tool_choice_is_refused() {
        let mut body = minimal();
        body["tool_choice"] = serde_json::json!("whatever");
        assert!(
            parse_request(body).is_err(),
            "an unknown mode must be refused, not silently mapped"
        );
    }

    // ── tools ──

    #[test]
    fn a_tool_definition_round_trips() {
        let mut body = minimal();
        body["tools"] = serde_json::json!([{
            "type": "function",
            "function": {
                "name": "grep",
                "description": "search",
                "parameters": {"type": "object", "properties": {}},
            },
        }]);
        let request = parse_request(body).expect("a tool list is legal");
        let tools = request.tools.as_ref().expect("tools");
        assert_eq!(tools[0].tool_type, "function");
        assert_eq!(tools[0].function.name, "grep");
        assert_eq!(tools[0].function.description.as_deref(), Some("search"));

        // `parameters` is mandatory — OpenAI allows a function without any.
        let mut body = minimal();
        body["tools"] = serde_json::json!([{
            "type": "function",
            "function": {"name": "noop"},
        }]);
        assert!(
            parse_request(body).is_err(),
            "documents the gap: FunctionDefinition::parameters has no default"
        );
    }

    // ── responses ──

    #[test]
    fn a_response_omits_the_optional_fields_it_has_no_value_for() {
        let response = ChatCompletionResponse {
            id: "chatcmpl-1".into(),
            object: "chat.completion".into(),
            created: 0,
            model: "m".into(),
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".into(),
                    content: Some(MessageContent::Text("hi".into())),
                    name: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 1,
                completion_tokens: 2,
                total_tokens: 3,
            },
            conversation_id: None,
        };

        assert_eq!(
            serde_json::to_value(&response).unwrap(),
            serde_json::json!({
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "created": 0,
                "model": "m",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "hi"},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3},
            })
        );
    }

    #[test]
    fn a_stream_chunk_with_an_empty_delta_serialises_to_an_empty_object() {
        // This is the shape of the final `finish_reason` chunk.
        let chunk = ChatCompletionStreamResponse {
            id: "chatcmpl-1".into(),
            object: "chat.completion.chunk".into(),
            created: 0,
            model: "m".into(),
            choices: vec![StreamChoice {
                index: 0,
                delta: DeltaMessage::default(),
                finish_reason: Some("stop".into()),
            }],
        };

        assert_eq!(
            serde_json::to_value(&chunk).unwrap(),
            serde_json::json!({
                "id": "chatcmpl-1",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "m",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            })
        );
    }

    #[test]
    fn a_tool_call_delta_keeps_only_the_fields_it_sets() {
        let chunk = DeltaToolCall {
            index: 1,
            id: None,
            tool_type: None,
            function: Some(DeltaFunctionCall {
                name: None,
                arguments: Some("{\"p\":1}".into()),
            }),
        };
        assert_eq!(
            serde_json::to_value(&chunk).unwrap(),
            serde_json::json!({"index": 1, "function": {"arguments": "{\"p\":1}"}}),
        );
    }

    #[test]
    fn a_model_list_round_trips() {
        let list = ModelList {
            object: "list".into(),
            data: vec![Model {
                id: "claude-sonnet-4".into(),
                object: "model".into(),
                created: 42,
                owned_by: "anthropic".into(),
            }],
        };
        let json = serde_json::to_value(&list).unwrap();
        assert_eq!(json["data"][0]["owned_by"], "anthropic");
        let back: ModelList = serde_json::from_value(json).unwrap();
        assert_eq!(back.data[0].id, "claude-sonnet-4");
    }

    // ── Default ──

    /// `ChatCompletionRequest::default()` hardcodes a Claude 3 model id the
    /// gateway's registry no longer lists. Nothing in production builds a request
    /// this way, so it only ever misleads a caller that does.
    #[test]
    fn the_default_request_names_a_legacy_model_and_no_messages() {
        let default = ChatCompletionRequest::default();
        assert_eq!(default.model, "claude-3-opus-20240229");
        assert!(default.messages.is_empty());
        assert_eq!(default.n, Some(1));
        assert_eq!(default.stream, Some(false));
        assert!(default.temperature.is_none());
        assert!(default.max_tokens.is_none());
    }

    #[test]
    fn the_default_request_is_not_a_valid_request() {
        // `messages: []` would be rejected downstream, so `Default` cannot be used
        // as a starting point without overriding at least two fields.
        let json = serde_json::to_value(ChatCompletionRequest::default()).unwrap();
        assert_eq!(json["messages"], serde_json::json!([]));
    }
}
