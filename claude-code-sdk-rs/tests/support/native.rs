//! Test support of the native harness: scripted `fake_openai` routes, the
//! `fake_mcp` server (stdio spec, or a running HTTP instance) and small helpers.
//!
//! Include it with `#[path = "support/native.rs"] mod native;`; the including
//! file also includes `support/fake_openai.rs`.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use futures::StreamExt;
use nexus_claude::agent::{AgentEvent, EnvCredentialResolver, EventStream, McpServerSpec};
use nexus_claude::model::{EndpointQuirks, OpenAiEndpoint, OpenAiEndpointConfig};
use serde_json::{Value, json};

pub const FAKE_MCP: &str = env!("CARGO_BIN_EXE_fake_mcp");

/// An OpenAI-compatible endpoint on `base_url`, DeepSeek dialect unless told otherwise.
pub fn endpoint(base_url: String, quirks: EndpointQuirks) -> Arc<OpenAiEndpoint> {
    let mut config = OpenAiEndpointConfig::new("native-test", base_url);
    config.quirks = quirks;
    config.response_timeout = std::time::Duration::from_secs(15);
    config.idle_timeout = std::time::Duration::from_secs(15);
    Arc::new(OpenAiEndpoint::new(config, Arc::new(EnvCredentialResolver)))
}

pub fn delta(delta: Value) -> Value {
    json!({"choices": [{"index": 0, "delta": delta}]})
}

pub fn finish(reason: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
}

pub fn usage_event(prompt: u64, completion: u64) -> Value {
    json!({"choices": [], "usage": {
        "prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion}})
}

fn route(body_contains: Option<&str>, events: Vec<Value>, event_delay_ms: u64) -> Value {
    let mut route =
        json!({"method": "POST", "path": "/v1/chat/completions", "status": 200, "sse": events});
    if let Some(needle) = body_contains {
        route["body_contains"] = json!(needle);
    }
    if event_delay_ms > 0 {
        route["event_delay_ms"] = json!(event_delay_ms);
    }
    route
}

/// A final answer: optional reasoning, the text in two deltas, the usage.
pub fn text_reply(
    body_contains: Option<&str>,
    text: &str,
    reasoning: Option<&str>,
    usage: Option<(u64, u64)>,
) -> Value {
    let mut events = Vec::new();
    if let Some(reasoning) = reasoning {
        events.push(delta(json!({"reasoning_content": reasoning})));
    }
    let middle = text.len() / 2;
    events.push(delta(json!({"content": &text[..middle]})));
    events.push(delta(json!({"content": &text[middle..]})));
    events.push(finish("stop"));
    if let Some((prompt, completion)) = usage {
        events.push(usage_event(prompt, completion));
    }
    events.push(json!("[DONE]"));
    route(body_contains, events, 0)
}

/// An answer asking for tools: `(id, name, arguments)`.
pub fn tool_reply(
    body_contains: Option<&str>,
    calls: &[(&str, &str, Value)],
    reasoning: Option<&str>,
    usage: Option<(u64, u64)>,
) -> Value {
    let mut events = Vec::new();
    if let Some(reasoning) = reasoning {
        events.push(delta(json!({"reasoning_content": reasoning})));
    }
    for (index, (id, name, arguments)) in calls.iter().enumerate() {
        events.push(delta(json!({"tool_calls": [{
            "index": index, "id": id, "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()}}]})));
    }
    events.push(finish("tool_calls"));
    if let Some((prompt, completion)) = usage {
        events.push(usage_event(prompt, completion));
    }
    events.push(json!("[DONE]"));
    route(body_contains, events, 0)
}

/// An answer that starts and then dribbles keep-alive comments for ~30 s: a
/// turn that stays busy until it is interrupted.
pub fn busy_reply(body_contains: Option<&str>) -> Value {
    let mut events = vec![delta(json!({"content": "start"}))];
    events.extend((0..300).map(|_| json!(": keep-alive")));
    route(body_contains, events, 100)
}

pub fn status_reply(body_contains: Option<&str>, status: u16, body: Value) -> Value {
    let mut route =
        json!({"method": "POST", "path": "/v1/chat/completions", "status": status, "body": body});
    if let Some(needle) = body_contains {
        route["body_contains"] = json!(needle);
    }
    route
}

/// The probe: a `ping` call (with a reasoning field when asked to).
pub fn probe_route(reasoning: bool) -> Value {
    let mut events = Vec::new();
    if reasoning {
        events.push(delta(json!({"reasoning_content": "hmm"})));
    }
    events.push(delta(json!({"tool_calls": [{"index": 0, "id": "p1", "function": {"name": "ping", "arguments": "{}"}}]})));
    events.push(finish("tool_calls"));
    events.push(json!("[DONE]"));
    route(Some("Call the ping tool now"), events, 0)
}

pub fn models_route(window: u64) -> Value {
    json!({"method": "GET", "path": "/v1/models", "status": 200,
        "body": {"object": "list", "data": [{"id": "m", "context_length": window}, {"id": "other"}]}})
}

/// The `fake_mcp` server over stdio. `log` names the file it appends its events to.
pub fn stdio_mcp(log: Option<&Path>) -> McpServerSpec {
    let mut env = BTreeMap::new();
    if let Some(log) = log {
        env.insert("FAKE_MCP_LOG".to_owned(), log.display().to_string());
    }
    env.insert("FAKE_MCP_SLOW_MAX_MS".to_owned(), "20000".to_owned());
    McpServerSpec::Stdio {
        command: FAKE_MCP.to_owned(),
        args: Vec::new(),
        env,
    }
}

/// A running `fake_mcp --http`. Killed on drop.
pub struct HttpMcp {
    child: Child,
    port: u16,
    pub log: PathBuf,
    _dir: tempfile::TempDir,
}

impl HttpMcp {
    pub fn start(token: Option<&str>, sse: bool) -> Self {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let log = dir.path().join("mcp.jsonl");
        let mut command = Command::new(FAKE_MCP);
        command
            .arg("--http")
            .env("FAKE_MCP_LOG", &log)
            .env("FAKE_MCP_MAX_RUNTIME_MS", "60000")
            .env("FAKE_MCP_SLOW_MAX_MS", "20000")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(token) = token {
            command.env("FAKE_MCP_TOKEN", token);
        }
        if sse {
            command.env("FAKE_MCP_SSE", "1");
        }
        let mut child = command.spawn().expect("spawn fake_mcp --http");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("read port");
        let port = line
            .trim()
            .strip_prefix("LISTENING ")
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("unexpected first line from fake_mcp: {line:?}"));
        Self {
            child,
            port,
            log,
            _dir: dir,
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    pub fn spec(&self, headers: &[(&str, &str)]) -> McpServerSpec {
        McpServerSpec::Http {
            url: self.url(),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        }
    }

    /// Lines of the server's event log.
    pub fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

impl Drop for HttpMcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Events of a log file written by `fake_mcp`.
pub fn log_events(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Reads a stream to its end.
pub async fn collect(mut stream: EventStream) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let read = async {
        while let Some(event) = stream.next().await {
            events.push(event);
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), read)
        .await
        .expect("the turn stream did not end within 30 s");
    events
}

pub fn terminal(events: &[AgentEvent]) -> &AgentEvent {
    events
        .iter()
        .find(|event| event.is_terminal())
        .expect("a terminal event")
}
