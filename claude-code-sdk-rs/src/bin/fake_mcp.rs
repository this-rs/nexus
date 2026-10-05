//! `fake_mcp` — a dependency-free MCP server for the tests of the native harness
//! (`providers::native::mcp`). Standard library and `serde_json` only.
//!
//! Two transports, chosen by the first argument:
//!
//! - `--stdio` (default): newline-delimited JSON-RPC on stdin/stdout;
//! - `--http`: streamable HTTP on `127.0.0.1:0`, prints `LISTENING <port>`; `POST`
//!   with a JSON body; the answer is JSON, or SSE when `FAKE_MCP_SSE=1`; the
//!   `Mcp-Session-Id` header is issued at `initialize`.
//!
//! # Environment
//!
//! | Variable | Meaning |
//! |---|---|
//! | `FAKE_MCP_LOG` | JSONL file: one line per event (`call`, `cancelled`, `http`); never a header value |
//! | `FAKE_MCP_TOKEN` | http: the bearer token required (`401` otherwise) |
//! | `FAKE_MCP_SSE` | http: answer with an SSE stream |
//! | `FAKE_MCP_SLOW_MAX_MS` | longest `slow` sleeps (default 30000) |
//! | `FAKE_MCP_MAX_RUNTIME_MS` | watchdog: hard exit after this long (default 120000) |
//!
//! # Tools
//!
//! | Tool | Annotations | Does |
//! |---|---|---|
//! | `echo {text}` | `readOnlyHint` | answers `echo: <text>` |
//! | `readonly` | `readOnlyHint` | answers `readonly ok` |
//! | `slow` | `readOnlyHint` | sleeps until `notifications/cancelled` for its call (logs `slow_started`, `slow_cancelled`) |
//! | `write {text}` | none | answers `write ok: <text>` |
//! | `fail` | none | answers an `isError` result `boom` |
//! | `die` | none | exits with code 7 |
//! | `env {name?}` | `readOnlyHint` | the names of its environment variables, and `name=value` for the one asked |
//! | `argv` | `readOnlyHint` | its command-line arguments |
//! | `big {n}` | `readOnlyHint` | a text of `n` characters |

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::exit;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[derive(Default)]
struct Shared {
    cancelled: Mutex<HashSet<u64>>,
}

fn log(event: Value) {
    let Ok(path) = std::env::var("FAKE_MCP_LOG") else {
        return;
    };
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        // One write per line: concurrent calls must not interleave their fragments.
        let _ = file.write_all(format!("{event}\n").as_bytes());
    }
}

fn tools_list() -> Value {
    let text_schema = json!({"type": "object", "properties": {"text": {"type": "string"}}});
    let none = json!({"type": "object", "properties": {}});
    let read_only = json!({"readOnlyHint": true});
    json!({"tools": [
        {"name": "echo", "description": "Echoes its text", "inputSchema": text_schema, "annotations": read_only},
        {"name": "readonly", "description": "A read-only tool", "inputSchema": none, "annotations": read_only},
        {"name": "slow", "description": "Sleeps until cancelled", "inputSchema": none, "annotations": read_only},
        {"name": "write", "description": "A tool that writes", "inputSchema": text_schema},
        {"name": "fail", "description": "Always fails", "inputSchema": none},
        {"name": "die", "description": "Exits the server", "inputSchema": none},
        {"name": "env", "description": "Environment of the server", "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}}, "annotations": read_only},
        {"name": "argv", "description": "Arguments of the server", "inputSchema": none, "annotations": read_only},
        {"name": "big", "description": "A long text", "inputSchema": {"type": "object", "properties": {"n": {"type": "integer"}}}, "annotations": read_only},
    ]})
}

fn text_result(text: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": text.into()}]})
}

fn call_tool(id: u64, name: &str, args: &Value, shared: &Shared) -> Result<Value, String> {
    log(json!({"event": "call", "tool": name}));
    match name {
        "echo" => Ok(text_result(format!(
            "echo: {}",
            args.get("text").and_then(Value::as_str).unwrap_or_default()
        ))),
        "readonly" => Ok(text_result("readonly ok")),
        "write" => Ok(text_result(format!(
            "write ok: {}",
            args.get("text").and_then(Value::as_str).unwrap_or_default()
        ))),
        "fail" => Ok(json!({"content": [{"type": "text", "text": "boom"}], "isError": true})),
        "die" => exit(7),
        "env" => {
            let mut names: Vec<String> = std::env::vars().map(|(name, _)| name).collect();
            names.sort();
            let mut text = format!("names: {}", names.join(","));
            if let Some(asked) = args.get("name").and_then(Value::as_str) {
                text.push_str(&format!(
                    "\n{asked}={}",
                    std::env::var(asked).unwrap_or_else(|_| "<unset>".into())
                ));
            }
            Ok(text_result(text))
        },
        "argv" => Ok(text_result(
            std::env::args().skip(1).collect::<Vec<_>>().join(" "),
        )),
        "big" => {
            let n = args.get("n").and_then(Value::as_u64).unwrap_or(10) as usize;
            Ok(text_result("x".repeat(n)))
        },
        "slow" => {
            log(json!({"event": "slow_started"}));
            let limit: u64 = std::env::var("FAKE_MCP_SLOW_MAX_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30_000);
            let started = Instant::now();
            while started.elapsed() < Duration::from_millis(limit) {
                if shared.cancelled.lock().unwrap().contains(&id) {
                    log(json!({"event": "slow_cancelled"}));
                    return Ok(
                        json!({"content": [{"type": "text", "text": "cancelled"}], "isError": true}),
                    );
                }
                thread::sleep(Duration::from_millis(20));
            }
            Ok(text_result("slow finished"))
        },
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Answers one JSON-RPC message; `None` for a notification.
fn handle(message: &Value, shared: &Shared) -> Option<Value> {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let Some(id) = message.get("id") else {
        if method == "notifications/cancelled"
            && let Some(request) = message.pointer("/params/requestId").and_then(Value::as_u64)
        {
            shared.cancelled.lock().unwrap().insert(request);
            log(json!({"event": "cancelled", "request": request}));
        }
        return None;
    };
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fake_mcp", "version": "0"},
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list()),
        "tools/call" => {
            let name = message
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = message
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));
            call_tool(id.as_u64().unwrap_or(0), name, &args, shared)
        },
        other => Err(format!("method not found: {other}")),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(message) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": message}})
        },
    })
}

fn run_stdio() {
    let shared = Arc::new(Shared::default());
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    for line in BufReader::new(std::io::stdin()).lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (shared, stdout) = (Arc::clone(&shared), Arc::clone(&stdout));
        let work = move || {
            if let Some(answer) = handle(&message, &shared) {
                let mut out = stdout.lock().unwrap();
                let _ = writeln!(out, "{answer}");
                let _ = out.flush();
            }
        };
        // Calls run apart: `slow` must not hold up the cancellation that ends it.
        if message_is_call(&line) {
            thread::spawn(work);
        } else {
            work();
        }
    }
}

fn message_is_call(line: &str) -> bool {
    line.contains("\"tools/call\"")
}

struct Request {
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    if !line.starts_with("POST ") && !line.starts_with("DELETE ") {
        return None;
    }
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            return None;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Request { headers, body })
}

fn respond(stream: &mut TcpStream, status: &str, headers: &[(&str, String)], body: &str) {
    let mut head = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

fn serve(mut stream: TcpStream, shared: Arc<Shared>) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    let header = |name: &str| {
        request
            .headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    };
    let expected = std::env::var("FAKE_MCP_TOKEN").ok();
    let authorised = match &expected {
        Some(token) => {
            header("authorization").as_deref() == Some(format!("Bearer {token}").as_str())
        },
        None => true,
    };
    log(json!({
        "event": "http",
        "has_authorization": header("authorization").is_some(),
        "authorised": authorised,
        "session": header("mcp-session-id").is_some(),
    }));
    if !authorised {
        respond(&mut stream, "401 Unauthorized", &[], "{}");
        return;
    }
    let Ok(message) = serde_json::from_slice::<Value>(&request.body) else {
        // DELETE (end of session) or garbage.
        respond(&mut stream, "200 OK", &[], "");
        return;
    };
    let is_initialize = message["method"] == "initialize";
    let Some(answer) = handle(&message, &shared) else {
        respond(&mut stream, "202 Accepted", &[], "");
        return;
    };
    let mut headers: Vec<(&str, String)> = Vec::new();
    if is_initialize {
        headers.push(("Mcp-Session-Id", "fake-session-1".to_owned()));
    }
    if std::env::var("FAKE_MCP_SSE").is_ok_and(|v| v == "1") {
        headers.push(("Content-Type", "text/event-stream".to_owned()));
        respond(
            &mut stream,
            "200 OK",
            &headers,
            &format!("event: message\ndata: {answer}\n\n"),
        );
    } else {
        headers.push(("Content-Type", "application/json".to_owned()));
        respond(&mut stream, "200 OK", &headers, &answer.to_string());
    }
}

fn run_http() {
    let shared = Arc::new(Shared::default());
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    println!("LISTENING {port}");
    let _ = std::io::stdout().flush();
    for stream in listener.incoming().flatten() {
        let shared = Arc::clone(&shared);
        thread::spawn(move || serve(stream, shared));
    }
}

fn main() {
    let max_ms = std::env::var("FAKE_MCP_MAX_RUNTIME_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120_000);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(max_ms));
        eprintln!("fake_mcp: watchdog expired");
        exit(3);
    });
    if std::env::args().any(|arg| arg == "--http") {
        run_http();
    } else {
        run_stdio();
    }
}
