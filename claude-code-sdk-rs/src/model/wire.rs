//! OpenAI chat/completions wire format, pure (no I/O): the request JSON built from
//! a [`CompletionRequest`] and the instance quirks, and the assembly of streamed
//! `data:` payloads into [`CompletionChunk`]s.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::quirks::EndpointQuirks;
use super::{ChatMessage, CompletionChunk, CompletionRequest, FinishReason, Role, ToolCallChunk};
use crate::agent::{ProviderError, Usage};

/// Name of the dummy tool of the probe.
pub(crate) const PROBE_TOOL: &str = "ping";

/// Moves system messages that follow a non-system message into the leading system
/// message (created when absent). Order is preserved; contents are joined by a blank line.
pub(crate) fn fold_late_system(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    let mut late: Vec<String> = Vec::new();
    let mut started = false;
    for message in messages {
        if message.role == Role::System && started {
            if let Some(content) = &message.content {
                late.push(content.clone());
            }
            continue;
        }
        if message.role != Role::System {
            started = true;
        }
        out.push(message.clone());
    }
    if late.is_empty() {
        return out;
    }
    let folded = late.join("\n\n");
    match out.first_mut() {
        Some(first) if first.role == Role::System => {
            let content = first.content.get_or_insert_with(String::new);
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            content.push_str(&folded);
        },
        _ => out.insert(0, ChatMessage::system(folded)),
    }
    out
}

/// Stands in for the result of a call the transcript never answered.
pub(crate) const MISSING_RESULT: &str =
    "[no result: the tool call did not complete in this session]";

/// Makes every `tool_calls`/`tool` pair of a transcript whole, so a history that
/// was cut, compacted or interrupted mid-turn still forms a request the server
/// accepts (DeepSeek, like OpenAI, rejects a `tool` message answering a call it
/// cannot see, and a call left unanswered).
///
/// Two repairs, both minimal:
///
/// * a `tool` message whose `tool_call_id` answers no call announced earlier (or
///   answers one twice, or carries no id at all) is **dropped** — its call is
///   gone, so there is nothing for the server to attach it to;
/// * a call announced by an assistant message and never answered gets a
///   synthetic `tool` message carrying [`MISSING_RESULT`], inserted where its
///   answer belonged.
///
/// Assistant messages are never rewritten: the repair only drops and appends
/// `tool` messages, so the expensive part of the prefix stays byte-identical and
/// the cache holds. The function is the identity on a well-formed transcript.
pub(crate) fn pair_tool_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    // Calls announced by the most recent assistant message, still unanswered.
    let mut pending: Vec<String> = Vec::new();
    let flush = |out: &mut Vec<ChatMessage>, pending: &mut Vec<String>| {
        for id in pending.drain(..) {
            out.push(ChatMessage {
                role: Role::Tool,
                content: Some(MISSING_RESULT.to_owned()),
                images: Vec::new(),
                reasoning: None,
                tool_calls: Vec::new(),
                tool_call_id: Some(id),
                wire_echo: None,
            });
        }
    };
    for message in messages {
        if message.role == Role::Tool {
            // Answers a call we announced and have not seen answered yet?
            match message
                .tool_call_id
                .as_deref()
                .and_then(|id| pending.iter().position(|open| open == id))
            {
                Some(position) => {
                    pending.remove(position);
                    out.push(message.clone());
                },
                None => continue,
            }
            continue;
        }
        // Any non-`tool` message closes the answer block of the previous one.
        flush(&mut out, &mut pending);
        out.push(message.clone());
        if message.role == Role::Assistant {
            pending = message.tool_calls.iter().map(|c| c.id.clone()).collect();
        }
    }
    flush(&mut out, &mut pending);
    out
}

fn message_json(message: &ChatMessage, quirks: &EndpointQuirks) -> Value {
    let mut object = Map::new();
    object.insert("role".into(), json!(message.role.as_str()));
    let content = if message.images.is_empty() {
        match (&message.content, message.tool_calls.is_empty()) {
            (Some(text), _) => Value::String(text.clone()),
            (None, false) => Value::Null,
            (None, true) => Value::String(String::new()),
        }
    } else {
        // A message with images is a list of parts: its text first, then one
        // `image_url` part per image, the image inline as a `data:` URL.
        let mut parts = Vec::with_capacity(message.images.len() + 1);
        if let Some(text) = message.content.as_deref().filter(|text| !text.is_empty()) {
            parts.push(json!({"type": "text", "text": text}));
        }
        parts.extend(
            message
                .images
                .iter()
                .map(|image| json!({"type": "image_url", "image_url": {"url": image.data_url()}})),
        );
        Value::Array(parts)
    };
    object.insert("content".into(), content);
    if message.role == Role::Assistant {
        if !message.tool_calls.is_empty() {
            let calls: Vec<Value> = message
                .tool_calls
                .iter()
                .map(|call| {
                    let arguments = if quirks.tool_args_as_object {
                        serde_json::from_str::<Value>(&call.arguments)
                            .ok()
                            .filter(Value::is_object)
                            .unwrap_or_else(|| json!({}))
                    } else {
                        Value::String(call.arguments.clone())
                    };
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": {"name": call.name, "arguments": arguments},
                    })
                })
                .collect();
            object.insert("tool_calls".into(), Value::Array(calls));
        }
        // DeepSeek: the reasoning of any assistant turn that reasoned must come
        // back, tools or not. Dropping it when a later request offers no tools
        // re-serialises the earlier assistant messages differently and breaks
        // the prefix cache on the first tool-less turn after a reasoned one.
        // A turn that called a tool without reasoning still owes the field: the
        // server fills it with an empty string, so omitting it re-serialises the
        // message differently from the way the server stored it.
        if quirks.echo_reasoning_with_tools
            && let Some(reasoning) = message
                .reasoning
                .clone()
                .or_else(|| (!message.tool_calls.is_empty()).then(String::new))
        {
            object.insert(
                quirks.reasoning_field.key().into(),
                Value::String(reasoning),
            );
        }
    }
    if let Some(id) = &message.tool_call_id {
        object.insert("tool_call_id".into(), Value::String(id.clone()));
    }
    Value::Object(object)
}

/// Builds the request body. `forced_tool` names a tool the model must call
/// (probe); it becomes `tool_choice` unless the quirks omit `tool_choice`.
pub(crate) fn build_request(
    request: &CompletionRequest,
    quirks: &EndpointQuirks,
    forced_tool: Option<&str>,
) -> Value {
    let with_tools = !request.tools.is_empty();
    // Unconditional: an orphan `tool` message, or a call left unanswered, is
    // invalid on every OpenAI-compatible server, and the repair is the identity
    // on a transcript that has neither.
    let paired = pair_tool_messages(&request.messages);
    let folded;
    let messages: &[ChatMessage] = if quirks.fold_late_system {
        folded = fold_late_system(&paired);
        &folded
    } else {
        &paired
    };
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert(
        "messages".into(),
        Value::Array(
            messages
                .iter()
                .map(|message| message_json(message, quirks))
                .collect(),
        ),
    );
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({"include_usage": true}));
    if let Some(max_tokens) = request.max_tokens {
        body.insert("max_tokens".into(), json!(max_tokens));
    }
    // An endpoint whose thinking mode pins the temperature is not sent one: see
    // `EndpointQuirks::thinking_ignores_temperature` for why it would only cost
    // a prefix-cache miss.
    if let Some(temperature) = request
        .temperature
        .filter(|_| !quirks.thinking_ignores_temperature)
    {
        body.insert("temperature".into(), json!(temperature));
    }
    if with_tools {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    },
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
        if !quirks.omit_tool_choice {
            let choice = match forced_tool {
                Some(name) if !quirks.no_forced_tool_choice => {
                    json!({"type": "function", "function": {"name": name}})
                },
                _ => json!("auto"),
            };
            body.insert("tool_choice".into(), choice);
        }
        if let Some(parallel) = request
            .parallel_tool_calls
            .or(quirks.explicit_parallel_tool_calls)
        {
            body.insert("parallel_tool_calls".into(), json!(parallel));
        }
    }
    Value::Object(body)
}

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Turns the `data:` payloads of one streamed completion into chunks.
///
/// Fragmented tool calls (several deltas per call, matched by `index`) and whole
/// ones (one delta) end up as the same single [`CompletionChunk::ToolCall`],
/// emitted when the finish reason arrives (or at the end of the stream).
#[derive(Debug)]
pub(crate) struct StreamParser {
    quirks: EndpointQuirks,
    calls: BTreeMap<u64, PartialCall>,
    finish: Option<FinishReason>,
    done: bool,
    had_calls: bool,
    /// Reasoning field name seen in the stream.
    pub(crate) reasoning_field_seen: Option<&'static str>,
}

impl StreamParser {
    pub(crate) fn new(quirks: EndpointQuirks) -> Self {
        Self {
            quirks,
            calls: BTreeMap::new(),
            finish: None,
            done: false,
            had_calls: false,
            reasoning_field_seen: None,
        }
    }

    /// Feeds one `data:` payload (`[DONE]` included).
    pub(crate) fn feed(&mut self, data: &str) -> Result<Vec<CompletionChunk>, ProviderError> {
        let data = data.trim();
        if data == "[DONE]" {
            self.done = true;
            return self.flush_calls();
        }
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|_| ProviderError::protocol("malformed JSON in an SSE data line"))?;
        if let Some(error) = value.get("error").filter(|e| !e.is_null()) {
            return Err(stream_error(error));
        }
        let mut out = Vec::new();
        if let Some(choice) = value.get("choices").and_then(|c| c.get(0)) {
            if let Some(delta) = choice.get("delta") {
                self.read_delta(delta, &mut out)?;
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                // The reason is read BEFORE the calls are flushed: `length` is what
                // makes a half-written call an honest truncation instead of a
                // protocol violation (see `flush_calls`).
                self.finish = Some(parse_finish(reason));
                out.extend(self.flush_calls()?);
            }
        }
        if let Some(usage) = value.get("usage").and_then(parse_usage) {
            out.push(CompletionChunk::Usage(usage));
        }
        Ok(out)
    }

    /// Ends the stream and returns the closing chunks (`Finish` last).
    pub(crate) fn end(&mut self) -> Result<Vec<CompletionChunk>, ProviderError> {
        let mut out = self.flush_calls()?;
        let reason = match self.finish.take() {
            Some(reason) => reason,
            None if self.done => {
                if self.had_calls {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                }
            },
            None => {
                return Err(ProviderError::protocol(
                    "stream ended before the completion finished",
                ));
            },
        };
        out.push(CompletionChunk::Finish(reason));
        Ok(out)
    }

    fn read_delta(
        &mut self,
        delta: &Value,
        out: &mut Vec<CompletionChunk>,
    ) -> Result<(), ProviderError> {
        if let Some(text) = delta.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            out.push(CompletionChunk::Text(text.to_string()));
        }
        let preferred = self.quirks.reasoning_field;
        for field in [preferred, preferred.other()] {
            if let Some(text) = delta.get(field.key()).and_then(Value::as_str)
                && !text.is_empty()
            {
                self.reasoning_field_seen.get_or_insert(field.key());
                out.push(CompletionChunk::Reasoning(text.to_string()));
                break;
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.read_call_delta(call);
            }
        }
        Ok(())
    }

    fn read_call_delta(&mut self, call: &Value) {
        let id = call.get("id").and_then(Value::as_str).unwrap_or("");
        let index = call.get("index").and_then(Value::as_u64);
        let next_key = self.calls.keys().next_back().map_or(0, |k| k + 1);
        let key = match index {
            // Some servers reuse index 0 for every call: a different id means a new call.
            Some(index) => match self.calls.get(&index) {
                Some(slot) if !id.is_empty() && !slot.id.is_empty() && slot.id != id => next_key,
                _ => index,
            },
            None if !id.is_empty() => self
                .calls
                .iter()
                .find(|(_, slot)| slot.id == id)
                .map_or(next_key, |(key, _)| *key),
            None => self.calls.keys().next_back().copied().unwrap_or(0),
        };
        let slot = self.calls.entry(key).or_default();
        if slot.id.is_empty() {
            slot.id = id.to_string();
        }
        if let Some(function) = call.get("function") {
            if let Some(name) = function.get("name").and_then(Value::as_str)
                && slot.name.is_empty()
            {
                slot.name = name.to_string();
            }
            match function.get("arguments") {
                Some(Value::String(fragment)) => slot.arguments.push_str(fragment),
                Some(object @ Value::Object(_)) => slot.arguments = object.to_string(),
                _ => {},
            }
        }
    }

    /// Turns the calls collected so far into chunks.
    ///
    /// A call whose `arguments` are not a complete JSON document is handled by
    /// what the server said about the turn (decision DS-WIRE):
    ///
    /// * under `finish_reason: "length"` the server is telling us it ran out of
    ///   output budget mid-call. That is a truthful, actionable answer, so the
    ///   half-written call is **dropped** and the turn still ends as
    ///   [`FinishReason::Length`] — the caller can raise `max_tokens` and retry.
    ///   Failing the stream here would replace the one signal that says why.
    /// * under any other reason (or none) a truncated call is a broken server and
    ///   stays a protocol error: nothing in the stream explains the hole.
    ///
    /// Either way no partial call is ever handed to the agent loop.
    fn flush_calls(&mut self) -> Result<Vec<CompletionChunk>, ProviderError> {
        let calls = std::mem::take(&mut self.calls);
        let truncation_was_announced = matches!(self.finish, Some(FinishReason::Length));
        let mut out = Vec::with_capacity(calls.len());
        for (position, (_, call)) in calls.into_iter().enumerate() {
            if call.name.is_empty() {
                if truncation_was_announced {
                    continue;
                }
                return Err(ProviderError::protocol("tool call without a name"));
            }
            let arguments = if call.arguments.trim().is_empty() {
                "{}".to_string()
            } else {
                call.arguments
            };
            if serde_json::from_str::<Value>(&arguments).is_err() {
                if truncation_was_announced {
                    continue;
                }
                return Err(ProviderError::protocol(
                    "tool call arguments are not a complete JSON document",
                ));
            }
            let id = if call.id.is_empty() {
                format!("call_{position}")
            } else {
                call.id
            };
            self.had_calls = true;
            out.push(CompletionChunk::ToolCall(ToolCallChunk {
                id,
                name: call.name,
                arguments,
            }));
        }
        Ok(out)
    }
}

fn parse_finish(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_string()),
    }
}

/// `usage` object to [`Usage`]. Cached tokens are split out of the input count, so
/// `input_tokens` means "not served from the cache" (what the price table expects).
pub(crate) fn parse_usage(value: &Value) -> Option<Usage> {
    let number = |v: Option<&Value>| v.and_then(Value::as_u64);
    let prompt = number(value.get("prompt_tokens"));
    let output = number(value.get("completion_tokens"));
    let cached = number(
        value
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens")),
    )
    .or_else(|| number(value.get("prompt_cache_hit_tokens")));
    let reasoning = number(
        value
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens")),
    );
    let total = number(value.get("total_tokens"));
    if prompt.is_none() && output.is_none() && total.is_none() {
        return None;
    }
    Some(Usage {
        input_tokens: prompt.map(|p| p.saturating_sub(cached.unwrap_or(0))),
        output_tokens: output,
        cache_read_tokens: cached,
        cache_creation_tokens: None,
        reasoning_tokens: reasoning,
        context_tokens: total.or_else(|| Some(prompt? + output?)),
        by_model: Vec::new(),
    })
}

/// Classifies an in-stream or HTTP error text.
pub(crate) fn classify_message(text: &str) -> Option<ProviderError> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("context length")
        || lower.contains("context_length")
        || lower.contains("maximum context")
        || lower.contains("context window")
        || lower.contains("too many tokens")
        || lower.contains("prompt is too long")
    {
        return Some(ProviderError::ContextTooSmall {
            needed: None,
            available: None,
        });
    }
    if lower.contains("overloaded") {
        return Some(ProviderError::Overloaded);
    }
    if lower.contains("rate limit") || lower.contains("rate_limit") {
        return Some(ProviderError::RateLimited {
            retry_after_ms: None,
        });
    }
    None
}

fn stream_error(error: &Value) -> ProviderError {
    let text = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("error object in the stream");
    classify_message(text).unwrap_or_else(|| ProviderError::protocol(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ToolSpec;

    fn tool() -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "reads".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn transcript() -> Vec<ChatMessage> {
        let mut call_message = ChatMessage::assistant_tool_calls(vec![ToolCallChunk {
            id: "c1".into(),
            name: "read".into(),
            arguments: r#"{"path":"a"}"#.into(),
        }])
        .with_reasoning("I should read the file");
        call_message.content = None;
        vec![
            ChatMessage::system("be brief"),
            ChatMessage::user("hi"),
            call_message,
            ChatMessage::tool("c1", "contents"),
        ]
    }

    fn request(tools: Vec<ToolSpec>) -> CompletionRequest {
        let mut request = CompletionRequest::new("m", transcript());
        request.tools = tools;
        request
    }

    #[test]
    fn plain_request_shape() {
        let body = build_request(&request(vec![tool()]), &EndpointQuirks::generic(), None);
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert!(body.get("parallel_tool_calls").is_none());
        let assistant = &body["messages"][2];
        assert!(assistant["content"].is_null());
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"a"}"#
        );
        assert!(assistant.get("reasoning_content").is_none());
        assert_eq!(body["messages"][3]["tool_call_id"], "c1");
    }

    #[test]
    fn a_message_with_images_is_a_list_of_parts_with_data_urls() {
        let mut request = CompletionRequest::new(
            "m",
            vec![ChatMessage::user_with_images(
                "what is this?",
                vec![
                    crate::model::ImagePart {
                        media_type: "image/png".into(),
                        data_base64: "AAAA".into(),
                    },
                    crate::model::ImagePart {
                        media_type: "image/jpeg".into(),
                        data_base64: "BBBB".into(),
                    },
                ],
            )],
        );
        request.tools = vec![];
        let body = build_request(&request, &EndpointQuirks::generic(), None);
        let parts = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(parts[0], json!({"type": "text", "text": "what is this?"}));
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert_eq!(parts[2]["image_url"]["url"], "data:image/jpeg;base64,BBBB");
        // Without text the list holds the images alone; without images the
        // content stays a plain string (what every server accepts).
        let mut bare = request.clone();
        bare.messages[0].content = Some(String::new());
        let body = build_request(&bare, &EndpointQuirks::generic(), None);
        assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 2);
        let plain = build_request(&self::request(vec![]), &EndpointQuirks::generic(), None);
        assert_eq!(plain["messages"][1]["content"], "hi");
    }

    /// The roles of a transcript, to read a repair at a glance.
    fn roles(body: &Value) -> Vec<&str> {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn pairing_is_the_identity_on_a_well_formed_transcript() {
        let whole = transcript();
        assert_eq!(pair_tool_messages(&whole), whole);
        // And on one with no tool traffic at all.
        let plain = vec![ChatMessage::user("hi"), ChatMessage::assistant("there")];
        assert_eq!(pair_tool_messages(&plain), plain);
    }

    #[test]
    fn an_orphan_tool_result_never_reaches_the_server() {
        // The assistant message that asked for `c1` was summarised away; only its
        // answer survived. DeepSeek rejects the whole request over it.
        let cut = vec![
            ChatMessage::system("be brief"),
            ChatMessage::tool("c1", "contents"),
            ChatMessage::user("and now?"),
        ];
        let mut request = CompletionRequest::new("m", cut);
        request.tools = vec![tool()];
        let body = build_request(&request, &EndpointQuirks::deepseek(), None);
        assert_eq!(roles(&body), ["system", "user"], "{body:#?}");
        // A result answering a call twice is just as orphan the second time.
        let twice = vec![
            transcript()[2].clone(),
            ChatMessage::tool("c1", "contents"),
            ChatMessage::tool("c1", "contents again"),
        ];
        assert_eq!(pair_tool_messages(&twice).len(), 2);
        // So is one that names no call at all.
        let mut nameless = ChatMessage::tool("c1", "contents");
        nameless.tool_call_id = None;
        assert_eq!(
            pair_tool_messages(&[transcript()[2].clone(), nameless]).len(),
            2,
            "the unanswered call gets a stand-in, the id-less result does not survive"
        );
    }

    #[test]
    fn a_call_left_unanswered_never_reaches_the_server_bare() {
        // The turn was interrupted between the call and its result.
        let interrupted = vec![
            ChatMessage::system("be brief"),
            ChatMessage::user("hi"),
            transcript()[2].clone(),
        ];
        let mut request = CompletionRequest::new("m", interrupted);
        request.tools = vec![tool()];
        let body = build_request(&request, &EndpointQuirks::deepseek(), None);
        assert_eq!(roles(&body), ["system", "user", "assistant", "tool"]);
        assert_eq!(body["messages"][3]["tool_call_id"], "c1");
        assert_eq!(body["messages"][3]["content"], MISSING_RESULT);
        // The assistant message itself is untouched: the prefix still hits the cache.
        assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "c1");
    }

    #[test]
    fn an_unanswered_call_is_closed_before_the_next_user_turn() {
        // Two calls, one answer, then the conversation moves on: the stand-in has
        // to land between the real answer and the user message, not at the end.
        let mut call_message = ChatMessage::assistant_tool_calls(vec![
            ToolCallChunk {
                id: "c1".into(),
                name: "read".into(),
                arguments: r#"{"path":"a"}"#.into(),
            },
            ToolCallChunk {
                id: "c2".into(),
                name: "read".into(),
                arguments: r#"{"path":"b"}"#.into(),
            },
        ]);
        call_message.content = None;
        let half = vec![
            call_message,
            ChatMessage::tool("c2", "contents of b"),
            ChatMessage::user("never mind"),
        ];
        let paired = pair_tool_messages(&half);
        assert_eq!(
            paired
                .iter()
                .map(|m| (m.role, m.tool_call_id.as_deref()))
                .collect::<Vec<_>>(),
            [
                (Role::Assistant, None),
                (Role::Tool, Some("c2")),
                (Role::Tool, Some("c1")),
                (Role::User, None),
            ]
        );
    }

    #[test]
    fn deepseek_fills_the_reasoning_of_a_tool_call_that_had_none() {
        // Their server stores "" for a tool-calling turn that did not reason;
        // leaving the field out re-serialises the message another way.
        let mut unreasoned = transcript();
        unreasoned[2].reasoning = None;
        let mut request = CompletionRequest::new("m", unreasoned);
        request.tools = vec![tool()];
        let body = build_request(&request, &EndpointQuirks::deepseek(), None);
        assert_eq!(body["messages"][2]["reasoning_content"], "");
        // A plain assistant message with neither reasoning nor calls owes nothing.
        let plain = CompletionRequest::new("m", vec![ChatMessage::assistant("hi")]);
        let body = build_request(&plain, &EndpointQuirks::deepseek(), None);
        assert!(body["messages"][0].get("reasoning_content").is_none());
    }

    #[test]
    fn deepseek_sends_no_temperature_because_thinking_pins_it() {
        let mut request = request(vec![tool()]);
        request.temperature = Some(0.25);
        let body = build_request(&request, &EndpointQuirks::deepseek(), None);
        assert!(
            body.get("temperature").is_none(),
            "thinking mode forces 1.0 server-side: asking for 0.25 only breaks the \
             prefix cache"
        );
        let generic = build_request(&request, &EndpointQuirks::generic(), None);
        assert_eq!(generic["temperature"], 0.25);
    }

    #[test]
    fn deepseek_echoes_reasoning_when_tools_are_present() {
        let body = build_request(&request(vec![tool()]), &EndpointQuirks::deepseek(), None);
        assert_eq!(
            body["messages"][2]["reasoning_content"],
            "I should read the file"
        );
    }

    #[test]
    fn deepseek_never_forces_a_tool() {
        // Thinking mode (DeepSeek's default) answers a named tool_choice with a 400.
        let body = build_request(
            &request(vec![tool()]),
            &EndpointQuirks::deepseek(),
            Some("read"),
        );
        assert_eq!(body["tool_choice"], "auto");
        // Another endpoint still forces it.
        let generic = build_request(
            &request(vec![tool()]),
            &EndpointQuirks::generic(),
            Some("read"),
        );
        assert_eq!(generic["tool_choice"]["function"]["name"], "read");
    }

    #[test]
    fn deepseek_echoes_reasoning_without_tools() {
        let body = build_request(&request(vec![]), &EndpointQuirks::deepseek(), None);
        assert_eq!(
            body["messages"][2]["reasoning_content"],
            "I should read the file"
        );
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn generic_never_echoes_reasoning() {
        let body = build_request(&request(vec![tool()]), &EndpointQuirks::generic(), None);
        assert!(body["messages"][2].get("reasoning_content").is_none());
        assert!(body["messages"][2].get("reasoning").is_none());
    }

    #[test]
    fn ollama_omits_tool_choice_even_when_forced() {
        let body = build_request(
            &request(vec![tool()]),
            &EndpointQuirks::ollama(),
            Some("read"),
        );
        assert!(body.get("tool_choice").is_none());
        let forced = build_request(
            &request(vec![tool()]),
            &EndpointQuirks::generic(),
            Some("read"),
        );
        assert_eq!(forced["tool_choice"]["function"]["name"], "read");
    }

    #[test]
    fn explicit_parallel_flag_comes_from_quirks_or_request() {
        let body = build_request(
            &request(vec![tool()]),
            &EndpointQuirks::llama_server(),
            None,
        );
        assert_eq!(body["parallel_tool_calls"], false);
        let mut asked = request(vec![tool()]);
        asked.parallel_tool_calls = Some(true);
        let body = build_request(&asked, &EndpointQuirks::nim(), None);
        assert_eq!(body["parallel_tool_calls"], true);
        // no tools: the flag is not sent
        let body = build_request(&request(vec![]), &EndpointQuirks::nim(), None);
        assert!(body.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn vllm_uses_the_reasoning_field_name() {
        let mut quirks = EndpointQuirks::vllm();
        quirks.echo_reasoning_with_tools = true;
        let body = build_request(&request(vec![tool()]), &quirks, None);
        assert_eq!(body["messages"][2]["reasoning"], "I should read the file");
        assert!(body["messages"][2].get("reasoning_content").is_none());
    }

    #[test]
    fn tool_args_as_object_sends_an_object() {
        let mut quirks = EndpointQuirks::generic();
        quirks.tool_args_as_object = true;
        let body = build_request(&request(vec![tool()]), &quirks, None);
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["arguments"]["path"],
            "a"
        );
    }

    #[test]
    fn late_system_is_folded_into_the_leading_one() {
        let messages = vec![
            ChatMessage::system("a"),
            ChatMessage::user("u"),
            ChatMessage::system("late"),
            ChatMessage::assistant("x"),
        ];
        let folded = fold_late_system(&messages);
        assert_eq!(folded.len(), 3);
        assert_eq!(folded[0].content.as_deref(), Some("a\n\nlate"));
        let lone = fold_late_system(&[ChatMessage::user("u"), ChatMessage::system("late")]);
        assert_eq!(lone[0].role, Role::System);
        assert_eq!(lone[0].content.as_deref(), Some("late"));
        let mut request = CompletionRequest::new("m", messages);
        request.tools = vec![];
        let body = build_request(&request, &EndpointQuirks::llama_server(), None);
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        let untouched = build_request(&request, &EndpointQuirks::generic(), None);
        assert_eq!(untouched["messages"].as_array().unwrap().len(), 4);
    }

    fn run(quirks: EndpointQuirks, events: &[&str]) -> Result<Vec<CompletionChunk>, ProviderError> {
        let mut parser = StreamParser::new(quirks);
        let mut out = Vec::new();
        for event in events {
            out.extend(parser.feed(event)?);
        }
        out.extend(parser.end()?);
        Ok(out)
    }

    fn calls(chunks: &[CompletionChunk]) -> Vec<&ToolCallChunk> {
        chunks
            .iter()
            .filter_map(|c| match c {
                CompletionChunk::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn fragmented_and_whole_tool_calls_give_the_same_chunk() {
        let fragmented = run(
            EndpointQuirks::generic(),
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"read","arguments":""}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                "[DONE]",
            ],
        )
        .unwrap();
        let whole = run(
            EndpointQuirks::generic(),
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}]},"finish_reason":"tool_calls"}]}"#,
                "[DONE]",
            ],
        )
        .unwrap();
        assert_eq!(fragmented, whole);
        assert_eq!(calls(&whole).len(), 1);
        assert_eq!(calls(&whole)[0].arguments, r#"{"path":"a"}"#);
        assert_eq!(
            whole.last(),
            Some(&CompletionChunk::Finish(FinishReason::ToolCalls))
        );
    }

    #[test]
    fn two_parallel_calls_are_kept_apart_by_index_and_by_id() {
        let by_index = run(
            EndpointQuirks::generic(),
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"x","arguments":"{"}},{"index":1,"id":"b","function":{"name":"y","arguments":"{}"}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"}"}}]},"finish_reason":"tool_calls"}]}"#,
            ],
        );
        // no [DONE] but a finish reason: accepted
        let chunks = by_index.unwrap();
        let found = calls(&chunks);
        assert_eq!(
            (found[0].id.as_str(), found[0].arguments.as_str()),
            ("a", "{}")
        );
        assert_eq!((found[1].id.as_str(), found[1].name.as_str()), ("b", "y"));
        let same_index = run(
            EndpointQuirks::generic(),
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"x","arguments":"{}"}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"b","function":{"name":"y","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
            ],
        )
        .unwrap();
        assert_eq!(calls(&same_index).len(), 2);
    }

    #[test]
    fn missing_id_is_generated_and_empty_arguments_become_an_object() {
        let chunks = run(
            EndpointQuirks::generic(),
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"function":{"name":"noargs"}}]},"finish_reason":"tool_calls"}]}"#,
                "[DONE]",
            ],
        )
        .unwrap();
        let call = calls(&chunks)[0].clone();
        assert_eq!(
            (call.id.as_str(), call.arguments.as_str()),
            ("call_0", "{}")
        );
    }

    #[test]
    fn object_arguments_are_serialised() {
        let chunks = run(
            EndpointQuirks::ollama(),
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"id":"c","function":{"name":"x","arguments":{"k":1}}}]},"finish_reason":"tool_calls"}]}"#,
                "[DONE]",
            ],
        )
        .unwrap();
        assert_eq!(calls(&chunks)[0].arguments, r#"{"k":1}"#);
    }

    /// scenario from deepseek-ai/deepseek-recipe (MIT): output truncated inside a
    /// tool call.
    ///
    /// Replaces `truncated_arguments_are_a_protocol_error`, which froze the
    /// opposite choice. `length` means the server ran out of output budget
    /// mid-call: that is the one signal that explains the hole, and an error threw
    /// it away. The half-written call is dropped, the Length reason survives.
    #[test]
    fn arguments_truncated_under_length_keep_the_length_signal() {
        let chunks = run(
            EndpointQuirks::generic(),
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"x","arguments":"{\"a\":"}}]},"finish_reason":"length"}]}"#,
                "[DONE]",
            ],
        )
        .unwrap();
        assert!(
            calls(&chunks).is_empty(),
            "no half-written call reaches the agent loop: {chunks:?}"
        );
        assert_eq!(
            chunks.last(),
            Some(&CompletionChunk::Finish(FinishReason::Length)),
            "{chunks:?}"
        );
    }

    /// The guard it replaces, kept where it still means something: truncated
    /// arguments with nothing in the stream to explain them is a broken server.
    #[test]
    fn truncated_arguments_without_a_length_reason_are_a_protocol_error() {
        let truncated_then_tool_calls = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"x","arguments":"{\"a\":"}}]},"finish_reason":"tool_calls"}]}"#;
        let truncated_with_no_reason = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"x","arguments":"{\"a\":"}}]}}]}"#;
        for payload in [truncated_then_tool_calls, truncated_with_no_reason] {
            let error = run(EndpointQuirks::generic(), &[payload, "[DONE]"]).unwrap_err();
            assert!(
                matches!(error, ProviderError::Protocol { .. }),
                "{payload}: {error:?}"
            );
        }
    }

    /// scenario from deepseek-ai/deepseek-recipe (MIT): a multi-byte character cut
    /// in two.
    ///
    /// `sse::every_split_point_gives_the_same_events` covers the cut in the byte
    /// decoder; this covers the one a server makes on purpose, by splitting a
    /// character across two `content` deltas of the same completion.
    #[test]
    fn a_character_split_across_two_deltas_is_reassembled() {
        let chunks = run(
            EndpointQuirks::generic(),
            &[
                r#"{"choices":[{"delta":{"content":"caf"}}]}"#,
                r#"{"choices":[{"delta":{"content":"é chaud"},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ],
        )
        .unwrap();
        let text: String = chunks
            .iter()
            .filter_map(|c| match c {
                CompletionChunk::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "café chaud");
    }

    /// scenario from deepseek-ai/deepseek-recipe (MIT): a stop sequence cut across
    /// two deltas.
    ///
    /// We never ask for client-side stop sequences, so the invariant that carries
    /// over is the one a consumer scanning for a marker depends on: the pieces
    /// arrive whole, in order, and nothing in between.
    #[test]
    fn a_stop_marker_split_across_two_deltas_arrives_contiguous() {
        let chunks = run(
            EndpointQuirks::generic(),
            &[
                r#"{"choices":[{"delta":{"content":"done <|"}}]}"#,
                r#"{"choices":[{"delta":{"reasoning_content":""}}]}"#,
                r#"{"choices":[{"delta":{"content":"end|>"},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ],
        )
        .unwrap();
        assert_eq!(
            chunks,
            [
                CompletionChunk::Text("done <|".into()),
                CompletionChunk::Text("end|>".into()),
                CompletionChunk::Finish(FinishReason::Stop),
            ],
            "an empty reasoning delta must not land between the two halves"
        );
    }

    #[test]
    fn text_reasoning_usage_finish_order() {
        let chunks = run(
            EndpointQuirks::generic(),
            &[
                r#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
                r#"{"choices":[{"delta":{"reasoning_content":"hm"}}]}"#,
                r#"{"choices":[{"delta":{"content":"Hel"}}]}"#,
                r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
                r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":2}}}"#,
                "[DONE]",
            ],
        )
        .unwrap();
        assert_eq!(chunks[0], CompletionChunk::Reasoning("hm".into()));
        assert_eq!(chunks[1], CompletionChunk::Text("Hel".into()));
        assert_eq!(chunks[2], CompletionChunk::Text("lo".into()));
        let CompletionChunk::Usage(usage) = &chunks[3] else {
            panic!("{chunks:?}")
        };
        assert_eq!(usage.input_tokens, Some(6));
        assert_eq!(usage.cache_read_tokens, Some(4));
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(usage.reasoning_tokens, Some(2));
        assert_eq!(usage.context_tokens, Some(15));
        assert_eq!(chunks[4], CompletionChunk::Finish(FinishReason::Stop));
    }

    #[test]
    fn reasoning_field_is_read_under_either_name() {
        for (quirks, field) in [
            (EndpointQuirks::vllm(), "reasoning"),
            (EndpointQuirks::vllm(), "reasoning_content"),
            (EndpointQuirks::generic(), "reasoning"),
        ] {
            let event =
                format!(r#"{{"choices":[{{"delta":{{"{field}":"t"}},"finish_reason":"stop"}}]}}"#);
            let mut parser = StreamParser::new(quirks);
            let chunks = parser.feed(&event).unwrap();
            assert_eq!(chunks, vec![CompletionChunk::Reasoning("t".into())]);
            assert_eq!(
                parser.reasoning_field_seen,
                Some(if field == "reasoning" {
                    "reasoning"
                } else {
                    "reasoning_content"
                })
            );
        }
    }

    #[test]
    fn stream_without_finish_is_an_error_but_done_alone_is_a_stop() {
        let error = run(
            EndpointQuirks::generic(),
            &[r#"{"choices":[{"delta":{"content":"a"}}]}"#],
        )
        .unwrap_err();
        assert!(matches!(error, ProviderError::Protocol { .. }));
        let chunks = run(EndpointQuirks::generic(), &["[DONE]"]).unwrap();
        assert_eq!(chunks, vec![CompletionChunk::Finish(FinishReason::Stop)]);
    }

    #[test]
    fn error_objects_in_the_stream_are_classified() {
        let overloaded = run(
            EndpointQuirks::generic(),
            &[r#"{"error":{"message":"The server is overloaded"}}"#],
        )
        .unwrap_err();
        assert_eq!(overloaded, ProviderError::Overloaded);
        let other = run(
            EndpointQuirks::generic(),
            &[r#"{"error":{"message":"boom"}}"#],
        )
        .unwrap_err();
        assert!(matches!(other, ProviderError::Protocol { .. }));
        assert!(run(EndpointQuirks::generic(), &["{not json"]).is_err());
    }

    #[test]
    fn deepseek_cache_fields_are_understood() {
        let usage = parse_usage(
            &json!({"prompt_tokens": 100, "completion_tokens": 1, "prompt_cache_hit_tokens": 60}),
        )
        .unwrap();
        assert_eq!(
            (usage.input_tokens, usage.cache_read_tokens),
            (Some(40), Some(60))
        );
        assert!(parse_usage(&json!({})).is_none());
    }

    // ----- mutation-testing additions (T3) -----

    #[test]
    fn calls_without_an_index_are_followed_by_id_and_a_bare_fragment_goes_to_the_last_call() {
        let chunks = run(
            EndpointQuirks::generic(),
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"id":"a","function":{"name":"x","arguments":"{\"k\":"}}]}}]}"#,
                r#"{"choices":[{"delta":{"tool_calls":[{"id":"b","function":{"name":"y","arguments":"{"}}]}}]}"#,
                // Back to the first call, by its id.
                r#"{"choices":[{"delta":{"tool_calls":[{"id":"a","function":{"arguments":"1}"}}]}}]}"#,
                // No id, no index: the last call.
                r#"{"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"}"}}]},"finish_reason":"tool_calls"}]}"#,
                "[DONE]",
            ],
        )
        .unwrap();
        let found = calls(&chunks);
        assert_eq!(found.len(), 2);
        assert_eq!(
            (found[0].id.as_str(), found[0].arguments.as_str()),
            ("a", r#"{"k":1}"#)
        );
        assert_eq!(
            (found[1].id.as_str(), found[1].arguments.as_str()),
            ("b", "{}")
        );
    }

    #[test]
    fn usage_is_read_from_any_one_of_prompt_completion_or_total_and_nothing_else_is_usage() {
        assert_eq!(parse_usage(&json!({})), None);
        assert_eq!(parse_usage(&json!({"unrelated": 3})), None);
        let only_prompt = parse_usage(&json!({"prompt_tokens": 7})).unwrap();
        assert_eq!(only_prompt.input_tokens, Some(7));
        assert_eq!(only_prompt.context_tokens, None);
        let only_output = parse_usage(&json!({"completion_tokens": 3})).unwrap();
        assert_eq!(only_output.output_tokens, Some(3));
        let only_total = parse_usage(&json!({"total_tokens": 9})).unwrap();
        assert_eq!(only_total.context_tokens, Some(9));
    }

    #[test]
    fn the_context_size_is_prompt_plus_completion_when_the_total_is_not_reported() {
        let usage = parse_usage(&json!({"prompt_tokens": 10, "completion_tokens": 5})).unwrap();
        assert_eq!(usage.context_tokens, Some(15));
        let reported =
            parse_usage(&json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 99}))
                .unwrap();
        assert_eq!(reported.context_tokens, Some(99));
    }

    #[test]
    fn every_context_overflow_phrasing_is_context_too_small_and_the_others_are_not() {
        for phrase in [
            "This model's maximum Context Length is 8192",
            "error code: context_length_exceeded",
            "exceeds the maximum context of the model",
            "beyond the context window",
            "too many tokens in the request",
            "the prompt is too long",
        ] {
            assert!(
                matches!(
                    classify_message(phrase),
                    Some(ProviderError::ContextTooSmall { .. })
                ),
                "{phrase}"
            );
        }
        assert!(matches!(
            classify_message("The server is OVERLOADED"),
            Some(ProviderError::Overloaded)
        ));
        for phrase in ["Rate limit reached", "rate_limit_exceeded"] {
            assert!(
                matches!(
                    classify_message(phrase),
                    Some(ProviderError::RateLimited { .. })
                ),
                "{phrase}"
            );
        }
        assert_eq!(classify_message("something else"), None);
        // Words that only look alike.
        assert_eq!(classify_message("context"), None);
        assert_eq!(classify_message("rate"), None);
    }
}
