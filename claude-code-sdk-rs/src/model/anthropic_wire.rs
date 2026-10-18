//! Anthropic Messages API wire format, pure (no I/O): the request JSON built from a
//! [`CompletionRequest`], with its prompt-cache points, and the assembly of the
//! streamed events into [`CompletionChunk`]s (CTX5).
//!
//! # The transcript on this wire
//!
//! | Transcript | Messages API |
//! |---|---|
//! | leading `system` messages | top-level `system`, one text block |
//! | a `system` message after the conversation started | a `<system-reminder>` text block of the user side |
//! | `user` (text, images) | `user`: a `text` block, then one base64 `image` block per image |
//! | `assistant` with a [`WireEcho`] of this protocol | its blocks, verbatim (thinking + signature, in order) |
//! | `assistant` without one | a `text` block, then one `tool_use` block per call (reasoning without a signature cannot be sent back, and is not) |
//! | `tool` | a `tool_result` block of the user side |
//!
//! Consecutive blocks of the same side are one message (the API alternates
//! `user` / `assistant`): the results of one round of tool calls and the images a tool
//! returned are one user message, the results first. An empty block is never sent.
//!
//! # The three cache points
//!
//! `cache_control: {"type": "ephemeral"}` on the last tool definition, on the system
//! block and on the last block of the last message (a reasoning block excepted): the
//! tools, the system prompt and the whole history so far are each a read point for
//! the next request. Three of the four breakpoints the API allows. The markers are
//! never in [`wire_messages`], the form the history check of the native loop
//! fingerprints: the moving last-turn marker is not a change of the history.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::{
    ChatMessage, CompletionChunk, CompletionRequest, FinishReason, Role, ToolCallChunk, WireEcho,
};
use crate::agent::{ProviderError, Usage};

/// The protocol name an Anthropic [`WireEcho`] carries.
pub(crate) const ECHO_PROTOCOL: &str = "anthropic_messages";

/// `max_tokens` when the request names none: the API requires one.
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 32_000;

/// How a request is written, beyond what the [`CompletionRequest`] says.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WireOptions {
    /// The `thinking` object sent as is (`{"type": "adaptive"}`…); `None` sends none
    /// and leaves it to the model's default.
    pub thinking: Option<Value>,
    /// Put the three cache points (default `true`).
    pub prompt_cache: bool,
    /// `max_tokens` when the request has none.
    pub default_max_tokens: u32,
}

impl Default for WireOptions {
    fn default() -> Self {
        Self {
            thinking: None,
            prompt_cache: true,
            default_max_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

fn ephemeral() -> Value {
    json!({"type": "ephemeral"})
}

/// The message list being built: one entry per side, blocks appended in order.
#[derive(Default)]
struct Turns(Vec<(&'static str, Vec<Value>)>);

impl Turns {
    fn push(&mut self, role: &'static str, block: Value) {
        match self.0.last_mut() {
            Some((last, blocks)) if *last == role => blocks.push(block),
            _ => self.0.push((role, vec![block])),
        }
    }

    fn into_messages(self) -> Vec<Value> {
        self.0
            .into_iter()
            .map(|(role, content)| json!({"role": role, "content": content}))
            .collect()
    }
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// The verbatim blocks of an assistant message, when it carries an echo of this
/// protocol whose tool calls are exactly the message's (else the echo is not used).
fn echoed_blocks(message: &ChatMessage) -> Option<Vec<Value>> {
    let echo = message.wire_echo.as_ref()?;
    if echo.protocol != ECHO_PROTOCOL {
        return None;
    }
    let blocks = echo.content.as_array()?;
    let echoed_ids: Vec<&str> = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|block| block.get("id").and_then(Value::as_str))
        .collect();
    let call_ids: Vec<&str> = message.tool_calls.iter().map(|c| c.id.as_str()).collect();
    if echoed_ids != call_ids {
        return None;
    }
    Some(
        blocks
            .iter()
            .map(|block| {
                let mut block = block.clone();
                if let Some(object) = block.as_object_mut() {
                    object.remove("cache_control");
                }
                block
            })
            .collect(),
    )
}

fn tool_input(arguments: &str) -> Value {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// The system text and the messages of a request, without any cache marker.
fn convert(request: &CompletionRequest) -> (Option<String>, Vec<Value>) {
    let mut system: Vec<&str> = Vec::new();
    let mut turns = Turns::default();
    let mut started = false;
    for message in &request.messages {
        let text = message.content.as_deref().filter(|text| !text.is_empty());
        match message.role {
            Role::System if !started => system.extend(text),
            Role::System => {
                if let Some(text) = text {
                    turns.push(
                        "user",
                        text_block(&format!("<system-reminder>\n{text}\n</system-reminder>")),
                    );
                }
            },
            Role::User => {
                started = true;
                if let Some(text) = text {
                    turns.push("user", text_block(text));
                }
                for image in &message.images {
                    turns.push(
                        "user",
                        json!({"type": "image", "source": {
                            "type": "base64",
                            "media_type": image.media_type,
                            "data": image.data_base64,
                        }}),
                    );
                }
            },
            Role::Assistant => {
                started = true;
                if let Some(blocks) = echoed_blocks(message) {
                    for block in blocks {
                        turns.push("assistant", block);
                    }
                    continue;
                }
                if let Some(text) = text {
                    turns.push("assistant", text_block(text));
                }
                for call in &message.tool_calls {
                    turns.push(
                        "assistant",
                        json!({"type": "tool_use", "id": call.id, "name": call.name,
                            "input": tool_input(&call.arguments)}),
                    );
                }
            },
            Role::Tool => {
                started = true;
                match &message.tool_call_id {
                    Some(id) => {
                        let mut block = json!({"type": "tool_result", "tool_use_id": id});
                        if let Some(text) = text {
                            block["content"] = json!(text);
                        }
                        turns.push("user", block);
                    },
                    None => {
                        if let Some(text) = text {
                            turns.push("user", text_block(text));
                        }
                    },
                }
            },
            #[allow(unreachable_patterns)]
            _ => {},
        }
    }
    let system = (!system.is_empty()).then(|| system.join("\n\n"));
    (system, turns.into_messages())
}

/// The messages of `request` as this wire carries them, without cache markers: what a
/// prompt cache sees of the history.
pub(crate) fn wire_messages(request: &CompletionRequest) -> Vec<Value> {
    convert(request).1
}

/// Puts the last-turn cache point on the last block of the last message that can take
/// one (not a reasoning block).
fn mark_last_turn(messages: &mut [Value]) {
    let Some(blocks) = messages
        .last_mut()
        .and_then(|message| message.get_mut("content"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    let target = blocks.iter_mut().rev().find(|block| {
        !matches!(
            block.get("type").and_then(Value::as_str),
            Some("thinking" | "redacted_thinking")
        )
    });
    if let Some(Value::Object(block)) = target {
        block.insert("cache_control".into(), ephemeral());
    }
}

/// The JSON schema of a tool, as `input_schema` wants it: an object schema.
fn input_schema(parameters: &Value) -> Value {
    match parameters {
        Value::Object(schema) if schema.contains_key("type") => parameters.clone(),
        _ => json!({"type": "object", "properties": {}}),
    }
}

/// Builds the request body (always streamed).
pub(crate) fn build_request(request: &CompletionRequest, options: &WireOptions) -> Value {
    let (system, mut messages) = convert(request);
    if options.prompt_cache {
        mark_last_turn(&mut messages);
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert(
        "max_tokens".into(),
        json!(request.max_tokens.unwrap_or(options.default_max_tokens)),
    );
    if let Some(system) = system {
        let mut block = text_block(&system);
        if options.prompt_cache {
            block["cache_control"] = ephemeral();
        }
        body.insert("system".into(), json!([block]));
    }
    body.insert("messages".into(), Value::Array(messages));
    if !request.tools.is_empty() {
        let mut tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                let mut definition = json!({
                    "name": tool.name,
                    "input_schema": input_schema(&tool.parameters),
                });
                if !tool.description.is_empty() {
                    definition["description"] = json!(tool.description);
                }
                definition
            })
            .collect();
        if options.prompt_cache
            && let Some(Value::Object(last)) = tools.last_mut()
        {
            last.insert("cache_control".into(), ephemeral());
        }
        body.insert("tools".into(), Value::Array(tools));
        if request.parallel_tool_calls == Some(false) {
            body.insert(
                "tool_choice".into(),
                json!({"type": "auto", "disable_parallel_tool_use": true}),
            );
        }
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(thinking) = &options.thinking {
        body.insert("thinking".into(), thinking.clone());
    }
    body.insert("stream".into(), json!(true));
    Value::Object(body)
}

// ---------------------------------------------------------------------------
// The stream
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Block {
    Text(String),
    Thinking {
        thinking: String,
        signature: String,
    },
    Redacted(String),
    ToolUse {
        id: String,
        name: String,
        json: String,
        initial: Value,
    },
    /// A block this client does not act on (a server tool…): not echoed.
    Other,
}

#[derive(Debug, Default)]
struct Counts {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
}

impl Counts {
    /// Takes every count `usage` states (the later report of a stream is cumulative).
    fn read(&mut self, usage: &Value) {
        let number = |key: &str| usage.get(key).and_then(Value::as_u64);
        for (slot, key) in [
            (&mut self.input, "input_tokens"),
            (&mut self.output, "output_tokens"),
            (&mut self.cache_read, "cache_read_input_tokens"),
            (&mut self.cache_creation, "cache_creation_input_tokens"),
        ] {
            if let Some(value) = number(key) {
                *slot = Some(value);
            }
        }
    }

    fn usage(&self) -> Option<Usage> {
        if self.input.is_none() && self.output.is_none() {
            return None;
        }
        let context = self.input.map(|input| {
            input
                + self.cache_read.unwrap_or(0)
                + self.cache_creation.unwrap_or(0)
                + self.output.unwrap_or(0)
        });
        Some(Usage {
            input_tokens: self.input,
            output_tokens: self.output,
            cache_read_tokens: self.cache_read,
            cache_creation_tokens: self.cache_creation,
            reasoning_tokens: None,
            context_tokens: context,
            by_model: Vec::new(),
        })
    }
}

/// Turns the `data:` payloads of one streamed message into chunks.
///
/// Text and reasoning come out as they stream; the tool calls, the [`WireEcho`] of the
/// message (only when it reasoned: without a reasoning block the transcript fields
/// say the same), the usage and the finish reason come out at the end.
#[derive(Debug, Default)]
pub(crate) struct StreamParser {
    blocks: BTreeMap<u64, Block>,
    stop_reason: Option<String>,
    stopped: bool,
    counts: Counts,
}

fn index_of(value: &Value) -> Result<u64, ProviderError> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| ProviderError::protocol("content block event without an index"))
}

impl StreamParser {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feeds one `data:` payload.
    pub(crate) fn feed(&mut self, data: &str) -> Result<Vec<CompletionChunk>, ProviderError> {
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(Vec::new());
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|_| ProviderError::protocol("malformed JSON in an SSE data line"))?;
        let mut out = Vec::new();
        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "message_start" => {
                if let Some(usage) = value.get("message").and_then(|m| m.get("usage")) {
                    self.counts.read(usage);
                }
            },
            "content_block_start" => {
                let index = index_of(&value)?;
                let start = value.get("content_block").cloned().unwrap_or(Value::Null);
                let text = |key: &str| {
                    start
                        .get(key)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned()
                };
                let block = match start.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let initial = text("text");
                        if !initial.is_empty() {
                            out.push(CompletionChunk::Text(initial.clone()));
                        }
                        Block::Text(initial)
                    },
                    Some("thinking") => {
                        let thinking = text("thinking");
                        if !thinking.is_empty() {
                            out.push(CompletionChunk::Reasoning(thinking.clone()));
                        }
                        Block::Thinking {
                            thinking,
                            signature: text("signature"),
                        }
                    },
                    Some("redacted_thinking") => Block::Redacted(text("data")),
                    Some("tool_use") => Block::ToolUse {
                        id: text("id"),
                        name: text("name"),
                        json: String::new(),
                        initial: start.get("input").cloned().unwrap_or_else(|| json!({})),
                    },
                    _ => Block::Other,
                };
                self.blocks.insert(index, block);
            },
            "content_block_delta" => {
                let index = index_of(&value)?;
                let delta = value.get("delta").cloned().unwrap_or(Value::Null);
                let piece = |key: &str| delta.get(key).and_then(Value::as_str).unwrap_or("");
                match (
                    delta.get("type").and_then(Value::as_str),
                    self.blocks.get_mut(&index),
                ) {
                    (Some("text_delta"), Some(Block::Text(text))) => {
                        let piece = piece("text");
                        if !piece.is_empty() {
                            text.push_str(piece);
                            out.push(CompletionChunk::Text(piece.to_owned()));
                        }
                    },
                    (Some("thinking_delta"), Some(Block::Thinking { thinking, .. })) => {
                        let piece = piece("thinking");
                        if !piece.is_empty() {
                            thinking.push_str(piece);
                            out.push(CompletionChunk::Reasoning(piece.to_owned()));
                        }
                    },
                    (Some("signature_delta"), Some(Block::Thinking { signature, .. })) => {
                        signature.push_str(piece("signature"));
                    },
                    (Some("input_json_delta"), Some(Block::ToolUse { json, .. })) => {
                        json.push_str(piece("partial_json"));
                    },
                    (_, None) => {
                        return Err(ProviderError::protocol(
                            "content block delta before its start",
                        ));
                    },
                    // Citations and the deltas of a block this client does not act on.
                    _ => {},
                }
            },
            "message_delta" => {
                if let Some(reason) = value
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(reason.to_owned());
                }
                if let Some(usage) = value.get("usage") {
                    self.counts.read(usage);
                }
            },
            "message_stop" => self.stopped = true,
            "error" => return Err(stream_error(value.get("error").unwrap_or(&Value::Null))),
            // `ping`, `content_block_stop`, and any event added later.
            _ => {},
        }
        Ok(out)
    }

    /// Ends the stream and returns the closing chunks (`Finish` last).
    pub(crate) fn end(&mut self) -> Result<Vec<CompletionChunk>, ProviderError> {
        if !self.stopped && self.stop_reason.is_none() {
            return Err(ProviderError::protocol(
                "stream ended before the message finished",
            ));
        }
        let blocks = std::mem::take(&mut self.blocks);
        let mut out = Vec::new();
        let mut echo = Vec::with_capacity(blocks.len());
        let mut reasoned = false;
        for block in blocks.into_values() {
            match block {
                Block::Text(text) => {
                    if !text.is_empty() {
                        echo.push(text_block(&text));
                    }
                },
                Block::Thinking {
                    thinking,
                    signature,
                } => {
                    reasoned = true;
                    echo.push(
                        json!({"type": "thinking", "thinking": thinking, "signature": signature}),
                    );
                },
                Block::Redacted(data) => {
                    reasoned = true;
                    echo.push(json!({"type": "redacted_thinking", "data": data}));
                },
                Block::ToolUse {
                    id,
                    name,
                    json,
                    initial,
                } => {
                    if name.is_empty() || id.is_empty() {
                        return Err(ProviderError::protocol(
                            "tool_use block without a name or an id",
                        ));
                    }
                    let input = if json.trim().is_empty() {
                        initial
                    } else {
                        serde_json::from_str::<Value>(&json).map_err(|_| {
                            ProviderError::protocol(
                                "tool call arguments are not a complete JSON document",
                            )
                        })?
                    };
                    if !input.is_object() {
                        return Err(ProviderError::protocol(
                            "tool call arguments are not a JSON object",
                        ));
                    }
                    echo.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
                    out.push(CompletionChunk::ToolCall(ToolCallChunk {
                        id,
                        name,
                        arguments: input.to_string(),
                    }));
                },
                Block::Other => {},
            }
        }
        let had_calls = !out.is_empty();
        if reasoned {
            out.push(CompletionChunk::Echo(WireEcho {
                protocol: ECHO_PROTOCOL.to_owned(),
                content: Value::Array(echo),
            }));
        }
        if let Some(usage) = self.counts.usage() {
            out.push(CompletionChunk::Usage(usage));
        }
        let reason = match self.stop_reason.take().as_deref() {
            Some("end_turn" | "stop_sequence") => FinishReason::Stop,
            Some("max_tokens" | "model_context_window_exceeded") => FinishReason::Length,
            Some("tool_use") => FinishReason::ToolCalls,
            Some("refusal") => FinishReason::ContentFilter,
            Some(other) => FinishReason::Other(other.to_owned()),
            None if had_calls => FinishReason::ToolCalls,
            None => FinishReason::Stop,
        };
        out.push(CompletionChunk::Finish(reason));
        Ok(out)
    }
}

/// An `error` event of the stream (or the `error` object of an HTTP error body).
pub(crate) fn stream_error(error: &Value) -> ProviderError {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("error event in the stream");
    match error.get("type").and_then(Value::as_str) {
        Some("overloaded_error") => ProviderError::Overloaded,
        Some("rate_limit_error") => ProviderError::RateLimited {
            retry_after_ms: None,
        },
        Some("authentication_error" | "permission_error") => ProviderError::Unauthorized,
        Some("invalid_request_error") => {
            super::wire::classify_message(message).unwrap_or_else(|| {
                ProviderError::InvalidRequest {
                    detail: crate::agent::redact(message),
                }
            })
        },
        _ => super::wire::classify_message(message)
            .unwrap_or_else(|| ProviderError::protocol(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ImagePart, ToolSpec};

    fn request(messages: Vec<ChatMessage>) -> CompletionRequest {
        let mut request = CompletionRequest::new("claude-test", messages);
        request.tools = vec![
            ToolSpec {
                name: "a".into(),
                description: "first".into(),
                parameters: json!({"type": "object", "properties": {}}),
            },
            ToolSpec {
                name: "b".into(),
                description: String::new(),
                parameters: Value::Null,
            },
        ];
        request
    }

    fn call(id: &str) -> ToolCallChunk {
        ToolCallChunk {
            id: id.into(),
            name: "a".into(),
            arguments: r#"{"x":1}"#.into(),
        }
    }

    #[test]
    fn the_three_cache_points_are_the_tools_the_system_and_the_last_turn() {
        let body = build_request(
            &request(vec![
                ChatMessage::system("be brief"),
                ChatMessage::user("hi"),
                ChatMessage::assistant("hello"),
                ChatMessage::user("again"),
            ]),
            &WireOptions::default(),
        );
        assert_eq!(
            body["tools"][1]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(body["system"][0]["text"], "be brief");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[2]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert!(messages[0]["content"][0].get("cache_control").is_none());
        let markers = body.to_string().matches("cache_control").count();
        assert_eq!(markers, 3, "three of the four breakpoints the API allows");
        // Off: none at all.
        let options = WireOptions {
            prompt_cache: false,
            ..WireOptions::default()
        };
        let plain = build_request(&request(vec![ChatMessage::user("hi")]), &options);
        assert!(!plain.to_string().contains("cache_control"));
        assert_eq!(plain["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(plain["stream"], true);
        assert_eq!(
            plain["tools"][1]["input_schema"],
            json!({"type": "object", "properties": {}})
        );
        assert!(plain["tools"][1].get("description").is_none());
    }

    #[test]
    fn tool_rounds_and_tool_images_are_one_user_message_results_first() {
        let image = ImagePart {
            media_type: "image/png".into(),
            data_base64: "AAAA".into(),
        };
        let messages = wire_messages(&request(vec![
            ChatMessage::user("go"),
            ChatMessage::assistant_tool_calls(vec![call("t1"), call("t2")]),
            ChatMessage::tool("t1", "one"),
            ChatMessage::tool("t2", ""),
            ChatMessage::user_with_images("[images]", vec![image]),
        ]));
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(
            messages[1]["content"],
            json!([
                {"type": "tool_use", "id": "t1", "name": "a", "input": {"x": 1}},
                {"type": "tool_use", "id": "t2", "name": "a", "input": {"x": 1}},
            ])
        );
        assert_eq!(
            messages[2]["content"],
            json!([
                {"type": "tool_result", "tool_use_id": "t1", "content": "one"},
                {"type": "tool_result", "tool_use_id": "t2"},
                {"type": "text", "text": "[images]"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
            ])
        );
        assert!(!Value::Array(messages).to_string().contains("cache_control"));
    }

    #[test]
    fn a_late_system_message_is_a_reminder_and_an_empty_assistant_is_dropped() {
        let (system, messages) = convert(&request(vec![
            ChatMessage::system("one"),
            ChatMessage::system("two"),
            ChatMessage::user("hi"),
            ChatMessage::assistant(""),
            ChatMessage::system("late"),
        ]));
        assert_eq!(system.as_deref(), Some("one\n\ntwo"));
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert_eq!(
            messages[0]["content"][1]["text"],
            "<system-reminder>\nlate\n</system-reminder>"
        );
    }

    #[test]
    fn an_echo_is_sent_back_verbatim_and_only_when_its_calls_are_the_messages() {
        let echo = WireEcho {
            protocol: ECHO_PROTOCOL.into(),
            content: json!([
                {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                {"type": "text", "text": "let me"},
                {"type": "tool_use", "id": "t1", "name": "a", "input": {"x": 1}},
            ]),
        };
        let mut message = ChatMessage::assistant_tool_calls(vec![call("t1")]);
        message.content = Some("let me".into());
        message.reasoning = Some("hmm".into());
        message.wire_echo = Some(echo.clone());
        let messages = wire_messages(&request(vec![ChatMessage::user("go"), message.clone()]));
        assert_eq!(messages[1]["content"], echo.content);
        // Another protocol, or calls that differ: rebuilt from the fields, reasoning left out.
        message.tool_calls[0].id = "renamed".into();
        let messages = wire_messages(&request(vec![ChatMessage::user("go"), message]));
        assert_eq!(
            messages[1]["content"][0],
            json!({"type": "text", "text": "let me"})
        );
        assert!(!messages[1].to_string().contains("thinking"));
    }

    #[test]
    fn the_last_turn_marker_skips_a_reasoning_block() {
        let mut messages = vec![json!({"role": "assistant", "content": [
            {"type": "text", "text": "x"},
            {"type": "thinking", "thinking": "", "signature": "s"},
        ]})];
        mark_last_turn(&mut messages);
        assert!(messages[0]["content"][0].get("cache_control").is_some());
        assert!(messages[0]["content"][1].get("cache_control").is_none());
    }

    #[test]
    fn options_put_thinking_and_parallel_off_where_the_api_wants_them() {
        let mut req = request(vec![ChatMessage::user("hi")]);
        req.parallel_tool_calls = Some(false);
        req.max_tokens = Some(100);
        req.temperature = Some(0.0);
        let options = WireOptions {
            thinking: Some(json!({"type": "adaptive", "display": "summarized"})),
            ..WireOptions::default()
        };
        let body = build_request(&req, &options);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(
            body["tool_choice"],
            json!({"type": "auto", "disable_parallel_tool_use": true})
        );
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["temperature"], 0.0);
    }

    fn run(events: &[Value]) -> Result<Vec<CompletionChunk>, ProviderError> {
        let mut parser = StreamParser::new();
        let mut out = Vec::new();
        for event in events {
            out.extend(parser.feed(&event.to_string())?);
        }
        out.extend(parser.end()?);
        Ok(out)
    }

    #[test]
    fn a_reasoned_tool_call_streams_and_echoes_in_order() {
        let chunks = run(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 10,
                "cache_read_input_tokens": 90, "cache_creation_input_tokens": 5, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "let me "}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "think"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "SIG"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "ok"}}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "a", "input": {}}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"x\":"}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "1}"}}),
            json!({"type": "ping"}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 42}}),
            json!({"type": "message_stop"}),
        ])
        .unwrap();
        assert_eq!(chunks[0], CompletionChunk::Reasoning("let me ".into()));
        assert_eq!(chunks[1], CompletionChunk::Reasoning("think".into()));
        assert_eq!(chunks[2], CompletionChunk::Text("ok".into()));
        assert_eq!(
            chunks[3],
            CompletionChunk::ToolCall(ToolCallChunk {
                id: "toolu_1".into(),
                name: "a".into(),
                arguments: r#"{"x":1}"#.into()
            })
        );
        let CompletionChunk::Echo(echo) = &chunks[4] else {
            panic!("{chunks:?}")
        };
        assert_eq!(
            echo.content,
            json!([
                {"type": "thinking", "thinking": "let me think", "signature": "SIG"},
                {"type": "text", "text": "ok"},
                {"type": "tool_use", "id": "toolu_1", "name": "a", "input": {"x": 1}},
            ])
        );
        let CompletionChunk::Usage(usage) = &chunks[5] else {
            panic!("{chunks:?}")
        };
        assert_eq!(
            (
                usage.input_tokens,
                usage.cache_read_tokens,
                usage.cache_creation_tokens,
                usage.output_tokens,
                usage.context_tokens
            ),
            (Some(10), Some(90), Some(5), Some(42), Some(147))
        );
        assert_eq!(chunks[6], CompletionChunk::Finish(FinishReason::ToolCalls));
    }

    #[test]
    fn a_message_without_reasoning_has_no_echo() {
        let chunks = run(&[
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "hi"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        ])
        .unwrap();
        assert_eq!(
            chunks,
            vec![
                CompletionChunk::Text("hi".into()),
                CompletionChunk::Finish(FinishReason::Stop)
            ]
        );
        let stops = |reason: &str| {
            run(&[json!({"type": "message_delta", "delta": {"stop_reason": reason}})]).unwrap()
        };
        assert_eq!(
            stops("max_tokens"),
            vec![CompletionChunk::Finish(FinishReason::Length)]
        );
        assert_eq!(
            stops("refusal"),
            vec![CompletionChunk::Finish(FinishReason::ContentFilter)]
        );
        assert_eq!(
            stops("pause_turn"),
            vec![CompletionChunk::Finish(FinishReason::Other(
                "pause_turn".into()
            ))]
        );
    }

    #[test]
    fn broken_streams_are_protocol_errors_and_error_events_are_classified() {
        assert!(matches!(
            run(&[
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "x"}})
            ]),
            Err(ProviderError::Protocol { .. })
        ));
        assert!(matches!(
            run(&[
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t", "name": "a", "input": {}}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"x\":"}}),
                json!({"type": "message_stop"}),
            ]),
            Err(ProviderError::Protocol { .. })
        ));
        assert!(matches!(
            run(&[
                json!({"type": "content_block_delta", "index": 3, "delta": {"type": "text_delta", "text": "x"}})
            ]),
            Err(ProviderError::Protocol { .. })
        ));
        let error = |kind: &str, message: &str| {
            run(&[json!({"type": "error", "error": {"type": kind, "message": message}})])
                .unwrap_err()
        };
        assert_eq!(
            error("overloaded_error", "Overloaded"),
            ProviderError::Overloaded
        );
        assert_eq!(
            error("rate_limit_error", "slow down"),
            ProviderError::RateLimited {
                retry_after_ms: None
            }
        );
        assert!(matches!(
            error("invalid_request_error", "prompt is too long: 300000 tokens"),
            ProviderError::ContextTooSmall { .. }
        ));
        assert!(matches!(
            error("api_error", "boom"),
            ProviderError::Protocol { .. }
        ));
    }
}
