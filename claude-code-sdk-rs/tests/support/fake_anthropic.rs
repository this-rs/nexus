//! Test support: scripted routes of a fake Anthropic Messages API, served by the
//! `fake_openai` binary (`POST /v1/messages`, events framed `event: <type>` +
//! `data: <json>` as the real API does), and an [`AnthropicEndpoint`] pointed at it.
//!
//! Include it with `#[path = "support/fake_anthropic.rs"] mod fake_anthropic;`; the
//! including file also includes `support/fake_openai.rs`.

#![allow(dead_code)]

use std::sync::Arc;

use nexus_claude::agent::{CredentialRef, CredentialResolver};
use nexus_claude::model::{AnthropicAuth, AnthropicEndpoint, AnthropicEndpointConfig};
use serde_json::{Value, json};

/// The marker of a request that carries tool results.
pub const TOOL_RESULT: &str = "\"type\":\"tool_result\"";
/// The marker of a request that carries an image.
pub const IMAGE: &str = "\"type\":\"image\"";

/// An endpoint on `base_url` (the fake's, ending in `/v1`), key as `x-api-key`.
pub fn anthropic_endpoint(
    base_url: String,
    credential: CredentialRef,
    resolver: Arc<dyn CredentialResolver>,
) -> Arc<AnthropicEndpoint> {
    let mut config = AnthropicEndpointConfig::gateway("native-test", base_url);
    config.auth = AnthropicAuth::ApiKey;
    config.credential = credential;
    config.response_timeout = std::time::Duration::from_secs(15);
    config.idle_timeout = std::time::Duration::from_secs(15);
    Arc::new(AnthropicEndpoint::new(config, resolver))
}

fn route(body_contains: Option<&str>, events: Vec<Value>, event_delay_ms: u64) -> Value {
    let mut route = json!({"method": "POST", "path": "/v1/messages", "status": 200,
        "sse": events, "sse_event_names": true});
    if let Some(needle) = body_contains {
        route["body_contains"] = json!(needle);
    }
    if event_delay_ms > 0 {
        route["event_delay_ms"] = json!(event_delay_ms);
    }
    route
}

fn message_start(input: u64) -> Value {
    json!({"type": "message_start", "message": {"id": "msg_1", "type": "message",
        "role": "assistant", "model": "m", "content": [], "stop_reason": null,
        "usage": {"input_tokens": input, "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0, "output_tokens": 1}}})
}

/// A thinking block at `index`, with its signature.
fn thinking_events(index: u64, thinking: &str, signature: &str) -> Vec<Value> {
    vec![
        json!({"type": "content_block_start", "index": index,
            "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": index,
            "delta": {"type": "thinking_delta", "thinking": thinking}}),
        json!({"type": "content_block_delta", "index": index,
            "delta": {"type": "signature_delta", "signature": signature}}),
        json!({"type": "content_block_stop", "index": index}),
    ]
}

fn text_events(index: u64, text: &str) -> Vec<Value> {
    let middle = text.len() / 2;
    vec![
        json!({"type": "content_block_start", "index": index,
            "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": index,
            "delta": {"type": "text_delta", "text": &text[..middle]}}),
        json!({"type": "content_block_delta", "index": index,
            "delta": {"type": "text_delta", "text": &text[middle..]}}),
        json!({"type": "content_block_stop", "index": index}),
    ]
}

fn end(stop_reason: &str, output: u64) -> Vec<Value> {
    vec![
        json!({"type": "message_delta", "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": {"output_tokens": output}}),
        json!({"type": "message_stop"}),
    ]
}

/// A final answer: optional thinking (signed `sig-final`), the text in two deltas.
/// `usage` is (input, output).
pub fn text_reply(
    body_contains: Option<&str>,
    text: &str,
    thinking: Option<&str>,
    usage: Option<(u64, u64)>,
) -> Value {
    let (input, output) = usage.unwrap_or((10, 5));
    let mut events = vec![message_start(input), json!({"type": "ping"})];
    let mut index = 0;
    if let Some(thinking) = thinking {
        events.extend(thinking_events(index, thinking, "sig-final"));
        index += 1;
    }
    events.extend(text_events(index, text));
    events.extend(end("end_turn", output));
    route(body_contains, events, 0)
}

/// An answer asking for tools, `(id, name, input)`, after an optional
/// `(thinking, signature)` block; the input of each call in two fragments.
pub fn tool_reply(
    body_contains: Option<&str>,
    calls: &[(&str, &str, Value)],
    thinking: Option<(&str, &str)>,
    usage: Option<(u64, u64)>,
) -> Value {
    let (input, output) = usage.unwrap_or((10, 5));
    let mut events = vec![message_start(input)];
    let mut index = 0;
    if let Some((thinking, signature)) = thinking {
        events.extend(thinking_events(index, thinking, signature));
        index += 1;
    }
    for (id, name, arguments) in calls {
        let text = arguments.to_string();
        let middle = text.len() / 2;
        events.push(json!({"type": "content_block_start", "index": index,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}));
        for piece in [&text[..middle], &text[middle..]] {
            events.push(json!({"type": "content_block_delta", "index": index,
                "delta": {"type": "input_json_delta", "partial_json": piece}}));
        }
        events.push(json!({"type": "content_block_stop", "index": index}));
        index += 1;
    }
    events.extend(end("tool_use", output));
    route(body_contains, events, 0)
}

/// An answer that starts and then dribbles keep-alive comments for ~30 s.
pub fn busy_reply(body_contains: Option<&str>) -> Value {
    let mut events = vec![
        message_start(10),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "start"}}),
    ];
    events.extend((0..300).map(|_| json!(": keep-alive")));
    route(body_contains, events, 100)
}

/// An HTTP error with an Anthropic error body.
pub fn status_reply(body_contains: Option<&str>, status: u16, kind: &str, message: &str) -> Value {
    let mut route = json!({"method": "POST", "path": "/v1/messages", "status": status,
        "body": {"type": "error", "error": {"type": kind, "message": message}}});
    if let Some(needle) = body_contains {
        route["body_contains"] = json!(needle);
    }
    route
}

/// An error event inside the stream, after the headers.
pub fn stream_error_reply(body_contains: Option<&str>, kind: &str, message: &str) -> Value {
    route(
        body_contains,
        vec![
            message_start(10),
            json!({"type": "error", "error": {"type": kind, "message": message}}),
        ],
        0,
    )
}

/// The catalogue: `m` with its window and vision, and `other`.
pub fn models_route(window: u64, vision: bool) -> Value {
    json!({"method": "GET", "path": "/v1/models", "status": 200, "body": {
        "data": [
            {"type": "model", "id": "m", "display_name": "M", "max_input_tokens": window,
                "capabilities": {"image_input": {"supported": vision}, "thinking": {"supported": true}}},
            {"type": "model", "id": "other", "display_name": "Other"}],
        "has_more": false, "first_id": "m", "last_id": "other"}})
}

/// The request bodies sent to `/v1/messages`, in order.
pub fn message_bodies(requests: &[Value]) -> Vec<Value> {
    requests
        .iter()
        .filter(|r| r["method"] == "POST" && r["path"] == "/v1/messages")
        .map(|r| r["body"].clone())
        .collect()
}
