//! `fake_openai` — a dependency-free HTTP/1.1 stand-in for an OpenAI-compatible
//! server, for the tests of `model::OpenAiEndpoint`. Standard library and `serde_json`
//! only: `std::net::TcpListener`, one thread per connection, `Connection: close`.
//!
//! It listens on `127.0.0.1:0` and prints `LISTENING <port>` on stdout.
//!
//! # Environment
//!
//! | Variable | Meaning |
//! |---|---|
//! | `FAKE_OPENAI_SCRIPT` | JSON file: a list of routes (below) |
//! | `FAKE_OPENAI_REQUESTS_OUT` | JSONL file: one line per request received |
//! | `FAKE_OPENAI_MAX_RUNTIME_MS` | watchdog: hard exit after this long (default 120000) |
//!
//! # Route
//!
//! `{method, path, body_contains?, status, headers?, body? | sse?, delay_ms?,
//! event_delay_ms?, close_after?, chunked?}`
//!
//! - A request matches a route when method and path (query ignored) are equal and the
//!   request body contains `body_contains` (if given).
//! - Routes are consumed in list order: the first matching route not used yet answers;
//!   when every matching route has been used, the last matching one is replayed.
//! - `body` is a string, or any JSON value (serialised). It is sent with
//!   `Content-Length`, or with `Transfer-Encoding: chunked` when `chunked` is true.
//! - `sse` is a list; each item (a string, or a JSON value serialised) becomes
//!   `data: <item>\n\n`, except a string starting with `:` which is sent as a comment
//!   line. The response is chunked, one chunk per event. `close_after: n` closes the
//!   connection abruptly after n events (no final chunk). `event_delay_ms` sleeps
//!   before each event.
//! - No route matches: `404` with a JSON error body.
//!
//! The request log records the method, the path, the header **names** and whether an
//! `Authorization` header was present, never its value, and the body.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::exit;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

struct Route {
    method: String,
    path: String,
    body_contains: Option<String>,
    status: u16,
    headers: Vec<(String, String)>,
    body: Option<String>,
    sse: Option<Vec<String>>,
    delay_ms: u64,
    event_delay_ms: u64,
    close_after: Option<usize>,
    chunked: bool,
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn load_routes() -> Vec<Route> {
    let Ok(path) = std::env::var("FAKE_OPENAI_SCRIPT") else {
        return Vec::new();
    };
    let text = std::fs::read_to_string(&path).expect("cannot read FAKE_OPENAI_SCRIPT");
    let value: Value = serde_json::from_str(&text).expect("FAKE_OPENAI_SCRIPT is not JSON");
    value
        .as_array()
        .expect("script must be a JSON list")
        .iter()
        .map(|route| Route {
            method: route["method"].as_str().unwrap_or("GET").to_string(),
            path: route["path"].as_str().unwrap_or("/").to_string(),
            body_contains: route["body_contains"].as_str().map(str::to_string),
            status: route["status"].as_u64().unwrap_or(200) as u16,
            headers: route["headers"]
                .as_object()
                .map(|map| map.iter().map(|(k, v)| (k.clone(), text_of(v))).collect())
                .unwrap_or_default(),
            body: route.get("body").filter(|b| !b.is_null()).map(text_of),
            sse: route["sse"]
                .as_array()
                .map(|items| items.iter().map(text_of).collect()),
            delay_ms: route["delay_ms"].as_u64().unwrap_or(0),
            event_delay_ms: route["event_delay_ms"].as_u64().unwrap_or(0),
            close_after: route["close_after"].as_u64().map(|n| n as usize),
            chunked: route["chunked"].as_bool().unwrap_or(false),
        })
        .collect()
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let path = target.split('?').next().unwrap_or("").to_string();
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
    let find = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    };
    let mut body = Vec::new();
    if find("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        loop {
            let mut size_line = String::new();
            reader.read_line(&mut size_line).ok()?;
            let size = usize::from_str_radix(size_line.trim().split(';').next()?, 16).ok()?;
            if size == 0 {
                let mut trailer = String::new();
                reader.read_line(&mut trailer).ok();
                break;
            }
            let mut chunk = vec![0; size];
            reader.read_exact(&mut chunk).ok()?;
            body.extend_from_slice(&chunk);
            let mut crlf = String::new();
            reader.read_line(&mut crlf).ok()?;
        }
    } else if let Some(length) = find("content-length").and_then(|v| v.parse::<usize>().ok()) {
        body = vec![0; length];
        reader.read_exact(&mut body).ok()?;
    }
    Some(Request {
        method,
        path,
        headers,
        body,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        301 => "Moved Permanently",
        302 => "Found",
        307 => "Temporary Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        529 => "Overloaded",
        _ => "Status",
    }
}

fn write_chunk(stream: &mut TcpStream, data: &[u8]) -> std::io::Result<()> {
    stream.write_all(format!("{:x}\r\n", data.len()).as_bytes())?;
    stream.write_all(data)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

fn respond(stream: &mut TcpStream, route: &Route) -> std::io::Result<()> {
    if route.delay_ms > 0 {
        thread::sleep(Duration::from_millis(route.delay_ms));
    }
    let mut head = format!("HTTP/1.1 {} {}\r\n", route.status, reason(route.status));
    let has = |name: &str| {
        route
            .headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(name))
    };
    for (name, value) in &route.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("Connection: close\r\n");
    if let Some(events) = &route.sse {
        if !has("content-type") {
            head.push_str("Content-Type: text/event-stream\r\n");
        }
        head.push_str("Cache-Control: no-cache\r\nTransfer-Encoding: chunked\r\n\r\n");
        stream.write_all(head.as_bytes())?;
        stream.flush()?;
        for (sent, event) in events.iter().enumerate() {
            if route.close_after == Some(sent) {
                return Ok(());
            }
            if route.event_delay_ms > 0 {
                thread::sleep(Duration::from_millis(route.event_delay_ms));
            }
            let frame = if event.starts_with(':') {
                format!("{event}\n\n")
            } else {
                format!("data: {event}\n\n")
            };
            write_chunk(stream, frame.as_bytes())?;
        }
        if route.close_after.is_some() {
            // close_after reached or beyond: end without the final chunk.
            return Ok(());
        }
        stream.write_all(b"0\r\n\r\n")?;
        return stream.flush();
    }
    let body = route.body.clone().unwrap_or_default();
    if !has("content-type") {
        head.push_str("Content-Type: application/json\r\n");
    }
    if route.chunked {
        head.push_str("Transfer-Encoding: chunked\r\n\r\n");
        stream.write_all(head.as_bytes())?;
        let bytes = body.as_bytes();
        let middle = bytes.len() / 2;
        for part in [&bytes[..middle], &bytes[middle..]] {
            if !part.is_empty() {
                write_chunk(stream, part)?;
            }
        }
        stream.write_all(b"0\r\n\r\n")?;
    } else {
        head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        stream.write_all(head.as_bytes())?;
        stream.write_all(body.as_bytes())?;
    }
    stream.flush()
}

fn log_request(request: &Request) {
    let Ok(path) = std::env::var("FAKE_OPENAI_REQUESTS_OUT") else {
        return;
    };
    let body_text = String::from_utf8_lossy(&request.body).to_string();
    let body = serde_json::from_str::<Value>(&body_text).unwrap_or(Value::String(body_text));
    let entry = json!({
        "method": request.method,
        "path": request.path,
        "headers": request.headers.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
        "authorization_present": request.headers.iter().any(|(n, _)| n == "authorization"),
        "body": body,
    });
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{entry}");
    }
}

fn handle(mut stream: TcpStream, routes: Arc<Vec<Route>>, used: Arc<Mutex<Vec<usize>>>) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    log_request(&request);
    let body_text = String::from_utf8_lossy(&request.body).to_string();
    let chosen = {
        let mut used = used.lock().unwrap();
        let candidates: Vec<usize> = routes
            .iter()
            .enumerate()
            .filter(|(_, route)| {
                route.method.eq_ignore_ascii_case(&request.method)
                    && route.path == request.path
                    && route
                        .body_contains
                        .as_ref()
                        .is_none_or(|needle| body_text.contains(needle))
            })
            .map(|(index, _)| index)
            .collect();
        let index = candidates
            .iter()
            .copied()
            .find(|index| used[*index] == 0)
            .or_else(|| candidates.last().copied());
        if let Some(index) = index {
            used[index] += 1;
        }
        index
    };
    let _ = match chosen {
        Some(index) => respond(&mut stream, &routes[index]),
        None => {
            let miss = Route {
                method: String::new(),
                path: String::new(),
                body_contains: None,
                status: 404,
                headers: Vec::new(),
                body: Some(r#"{"error":{"message":"fake_openai: no route"}}"#.to_string()),
                sse: None,
                delay_ms: 0,
                event_delay_ms: 0,
                close_after: None,
                chunked: false,
            };
            respond(&mut stream, &miss)
        },
    };
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

fn main() {
    let max_ms = std::env::var("FAKE_OPENAI_MAX_RUNTIME_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120_000);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(max_ms));
        eprintln!("fake_openai: watchdog expired");
        exit(3);
    });
    let routes = Arc::new(load_routes());
    let used = Arc::new(Mutex::new(vec![0usize; routes.len()]));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    println!("LISTENING {port}");
    let _ = std::io::stdout().flush();
    for stream in listener.incoming().flatten() {
        let (routes, used) = (Arc::clone(&routes), Arc::clone(&used));
        thread::spawn(move || handle(stream, routes, used));
    }
}
