//! The server, in process: protocol, profile, caps, isolation, cancellation, and the HTTP
//! transport driven by the native harness's own MCP client (N18).
//!
//! Every refusal here comes with a counter that proves the tool did not run: "refused" and
//! "ran anyway and then said no" look the same from the outside.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nexus_claude::agent::{EnvSpec, McpServerSpec};
use nexus_claude::providers::native::{McpClient, McpConfig, McpLaunch};
use nexus_tools::http;
use nexus_tools::testing::{EchoTool, WriteTool};
use nexus_tools::{
    Annotations, CallContext, Claims, Profile, Server, Session, SigningKey, Tool, ToolRegistry,
    ToolResult, issue, serve_lines,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

// ---------------------------------------------------------------------------
// Tools that watch what happens to them
// ---------------------------------------------------------------------------

/// Counts how many times it actually ran.
struct Counting {
    name: &'static str,
    ran: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Counting {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "counts its executions"
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn call(&self, _context: &CallContext, _arguments: Value) -> ToolResult {
        self.ran.fetch_add(1, Ordering::SeqCst);
        ToolResult::ok("ran")
    }
}

/// Answers with a very long text.
struct Big;

#[async_trait]
impl Tool for Big {
    fn name(&self) -> &str {
        "big"
    }

    fn description(&self) -> &str {
        "a long answer"
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }

    fn annotations(&self) -> Annotations {
        Annotations::read_only()
    }

    async fn call(&self, _context: &CallContext, _arguments: Value) -> ToolResult {
        ToolResult::ok("x".repeat(500))
    }
}

/// Panics.
struct Boom;

#[async_trait]
impl Tool for Boom {
    fn name(&self) -> &str {
        "boom"
    }

    fn description(&self) -> &str {
        "panics"
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn call(&self, _context: &CallContext, _arguments: Value) -> ToolResult {
        panic!("this tool is broken");
    }
}

/// Sleeps for a long time and records whether it was dropped before finishing.
struct Slow {
    dropped: Arc<AtomicBool>,
}

struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl Tool for Slow {
    fn name(&self) -> &str {
        "slow"
    }

    fn description(&self) -> &str {
        "sleeps"
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn call(&self, _context: &CallContext, _arguments: Value) -> ToolResult {
        let _guard = SetOnDrop(Arc::clone(&self.dropped));
        tokio::time::sleep(Duration::from_secs(60)).await;
        ToolResult::ok("finished")
    }
}

/// Keeps a counter in the session's state.
struct Tally;

#[async_trait]
impl Tool for Tally {
    fn name(&self) -> &str {
        "tally"
    }

    fn description(&self) -> &str {
        "counts calls of the session"
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn call(&self, context: &CallContext, _arguments: Value) -> ToolResult {
        let counter = context.state.get_or_init(|| AtomicUsize::new(0));
        ToolResult::ok((counter.fetch_add(1, Ordering::SeqCst) + 1).to_string())
    }
}

fn registry(ran: &Arc<AtomicUsize>) -> ToolRegistry {
    ToolRegistry::new()
        .with(EchoTool)
        .with(WriteTool)
        .with(Counting {
            name: "counted",
            ran: Arc::clone(ran),
        })
        .with(Big)
        .with(Boom)
        .with(Tally)
}

// ---------------------------------------------------------------------------
// stdio, in memory
// ---------------------------------------------------------------------------

struct Wire {
    to_server: tokio::io::DuplexStream,
    from_server: BufReader<tokio::io::DuplexStream>,
}

impl Wire {
    fn start(server: Server, profile: Profile) -> Self {
        let (to_server, server_in) = tokio::io::duplex(1 << 20);
        let (server_out, from_server) = tokio::io::duplex(1 << 20);
        tokio::spawn(serve_lines(
            Arc::new(server),
            Session::new(profile),
            BufReader::new(server_in),
            server_out,
        ));
        Self {
            to_server,
            from_server: BufReader::new(from_server),
        }
    }

    async fn send(&mut self, message: Value) {
        self.to_server
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn read(&mut self) -> Value {
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(10),
            self.from_server.read_line(&mut line),
        )
        .await
        .expect("an answer in time")
        .unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line:?}"))
    }

    async fn ask(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        self.read().await
    }
}

fn call(name: &str, arguments: Value) -> Value {
    json!({"name": name, "arguments": arguments})
}

#[tokio::test]
async fn initialize_ping_and_listing_work_over_a_line_stream() {
    let ran = Arc::new(AtomicUsize::new(0));
    let mut wire = Wire::start(Server::new(registry(&ran)), Profile::unrestricted("s"));

    let init = wire
        .ask(
            1,
            "initialize",
            json!({"protocolVersion": "2025-03-26", "capabilities": {}}),
        )
        .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(init["result"]["serverInfo"]["name"], "nexus-tools");
    // An unknown version is answered with ours, not refused.
    let other = wire
        .ask(2, "initialize", json!({"protocolVersion": "1999-01-01"}))
        .await;
    assert_eq!(other["result"]["protocolVersion"], "2025-03-26");

    assert_eq!(wire.ask(3, "ping", json!({})).await["result"], json!({}));

    let listed = wire.ask(4, "tools/list", json!({})).await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["big", "boom", "counted", "echo", "tally", "write"]);

    // Annotations carry the policy categories.
    let by_name: BTreeMap<&str, &Value> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["name"].as_str().unwrap(), t))
        .collect();
    assert_eq!(by_name["echo"]["annotations"]["readOnlyHint"], true);
    assert_eq!(by_name["write"]["annotations"]["readOnlyHint"], false);
    assert_eq!(by_name["write"]["annotations"]["destructiveHint"], true);

    let called = wire
        .ask(5, "tools/call", call("echo", json!({"text": "hi"})))
        .await;
    assert_eq!(called["result"]["content"][0]["text"], "echo: hi");
    assert_eq!(called["result"]["isError"], false);

    let unknown = wire.ask(6, "nope/method", json!({})).await;
    assert_eq!(unknown["error"]["code"], -32601);
}

/// The central rule (A35): a tool outside the profile is not listed and is never run, and
/// the answer for "forbidden" is the answer for "does not exist".
#[tokio::test]
async fn a_tool_outside_the_profile_is_never_listed_and_never_run() {
    let ran = Arc::new(AtomicUsize::new(0));
    let mut wire = Wire::start(Server::new(registry(&ran)), Profile::only("s", ["echo"]));

    let listed = wire.ask(1, "tools/list", json!({})).await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["echo"], "only the profile's tools are listed");

    let forbidden = wire.ask(2, "tools/call", call("counted", json!({}))).await;
    let missing = wire
        .ask(3, "tools/call", call("no-such-tool", json!({})))
        .await;
    assert_eq!(forbidden["error"]["code"], -32602);
    assert_eq!(missing["error"]["code"], -32602);
    assert_eq!(
        forbidden["error"]["message"]
            .as_str()
            .unwrap()
            .replace("counted", "X"),
        missing["error"]["message"]
            .as_str()
            .unwrap()
            .replace("no-such-tool", "X"),
        "forbidden and missing look the same"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "the forbidden tool never ran"
    );

    // Control: the same tool, allowed, does run.
    let mut allowed = Wire::start(Server::new(registry(&ran)), Profile::only("s", ["counted"]));
    allowed
        .ask(1, "tools/call", call("counted", json!({})))
        .await;
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn bad_arguments_are_a_protocol_error_before_any_tool_runs() {
    let ran = Arc::new(AtomicUsize::new(0));
    let mut wire = Wire::start(Server::new(registry(&ran)), Profile::unrestricted("s"));
    let no_name = wire.ask(1, "tools/call", json!({"arguments": {}})).await;
    assert_eq!(no_name["error"]["code"], -32602);
    let not_object = wire
        .ask(
            2,
            "tools/call",
            json!({"name": "counted", "arguments": [1, 2]}),
        )
        .await;
    assert_eq!(not_object["error"]["code"], -32602);
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    // No `arguments` at all is an empty object, not an error.
    let none = wire.ask(3, "tools/call", json!({"name": "counted"})).await;
    assert_eq!(none["result"]["isError"], false);
}

#[tokio::test]
async fn an_output_over_the_cap_is_cut_with_a_marker_that_says_so() {
    let ran = Arc::new(AtomicUsize::new(0));
    let server = Server::new(registry(&ran)).with_max_output_chars(100);
    let mut wire = Wire::start(server, Profile::unrestricted("s"));
    let answer = wire.ask(1, "tools/call", call("big", json!({}))).await;
    let text = answer["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with(&"x".repeat(100)), "{text}");
    assert!(
        text.contains("[output truncated: 400 characters omitted of 500]"),
        "{text}"
    );
    assert_eq!(text.chars().filter(|c| *c == 'x').count(), 100);
}

#[tokio::test]
async fn a_panicking_tool_is_one_failed_call_not_a_dead_server() {
    let ran = Arc::new(AtomicUsize::new(0));
    let mut wire = Wire::start(Server::new(registry(&ran)), Profile::unrestricted("s"));
    let boom = wire.ask(1, "tools/call", call("boom", json!({}))).await;
    assert_eq!(boom["result"]["isError"], true);
    assert!(
        !boom["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("broken"),
        "the panic message is not echoed to the model"
    );
    let after = wire
        .ask(2, "tools/call", call("echo", json!({"text": "still here"})))
        .await;
    assert_eq!(after["result"]["content"][0]["text"], "echo: still here");
}

#[tokio::test]
async fn cancelling_a_request_aborts_the_call_and_sends_no_answer() {
    let dropped = Arc::new(AtomicBool::new(false));
    let registry = ToolRegistry::new()
        .with(Slow {
            dropped: Arc::clone(&dropped),
        })
        .with(EchoTool);
    let mut wire = Wire::start(Server::new(registry), Profile::unrestricted("s"));
    wire.send(json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": call("slow", json!({}))}))
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!dropped.load(Ordering::SeqCst), "still running");
    wire.send(
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 7}}),
    )
    .await;
    for _ in 0..100 {
        if dropped.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(dropped.load(Ordering::SeqCst), "the call was aborted");
    // The next answer on the wire is the next request's: nothing was sent for id 7.
    let next = wire
        .ask(8, "tools/call", call("echo", json!({"text": "ok"})))
        .await;
    assert_eq!(next["id"], 8);
}

#[tokio::test]
async fn invalid_json_is_answered_and_the_session_goes_on() {
    let ran = Arc::new(AtomicUsize::new(0));
    let mut wire = Wire::start(Server::new(registry(&ran)), Profile::unrestricted("s"));
    wire.to_server
        .write_all(b"this is not json\n")
        .await
        .unwrap();
    assert_eq!(wire.read().await["error"]["code"], -32700);
    assert_eq!(wire.ask(1, "ping", json!({})).await["result"], json!({}));
}

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

static LOG: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The server logs the tool and the outcome, never the arguments or the result.
#[tokio::test]
async fn the_log_names_the_tool_and_never_what_it_was_given() {
    let buffer = LOG
        .get_or_init(|| {
            let buffer = Arc::new(Mutex::new(Vec::new()));
            tracing_subscriber::fmt()
                .with_writer(LogWriter(Arc::clone(&buffer)))
                .with_ansi(false)
                .init();
            buffer
        })
        .clone();
    let ran = Arc::new(AtomicUsize::new(0));
    let mut wire = Wire::start(Server::new(registry(&ran)), Profile::unrestricted("s"));
    let sentinel = "sk-sentinel-never-in-a-log-9f3a";
    wire.ask(1, "tools/call", call("echo", json!({"text": sentinel})))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let log = String::from_utf8_lossy(&buffer.lock().unwrap_or_else(PoisonError::into_inner))
        .into_owned();
    assert!(log.contains("tool call") && log.contains("echo"), "{log}");
    assert!(!log.contains(sentinel), "the log holds the argument: {log}");
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef-test-key";

fn key() -> SigningKey {
    SigningKey::new(KEY.to_vec()).unwrap()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn token(sid: &str, tools: &[&str], ttl: i64) -> String {
    issue(
        &key(),
        &Claims {
            sid: sid.into(),
            tools: tools.iter().map(|t| (*t).to_owned()).collect(),
            exp: (now() as i64 + ttl) as u64,
        },
    )
}

struct Http {
    address: std::net::SocketAddr,
}

async fn start_http(ran: &Arc<AtomicUsize>, origins: &[&str]) -> Http {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = http::router(
        Arc::new(Server::new(registry(ran))),
        key(),
        origins.iter().map(|o| (*o).to_owned()).collect(),
        address,
    );
    tokio::spawn(http::serve(listener, router));
    Http { address }
}

struct Reply {
    status: u16,
    headers: String,
    body: String,
}

/// One HTTP/1.1 request on a fresh connection.
async fn raw(address: std::net::SocketAddr, request: &str) -> Reply {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    // A server that refuses early may close before the whole request is written.
    let _ = stream.write_all(request.as_bytes()).await;
    let mut bytes = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut bytes)).await;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Reply {
        status,
        headers: head.to_ascii_lowercase(),
        body: body.to_owned(),
    }
}

fn post(address: std::net::SocketAddr, headers: &[(&str, String)], body: &str) -> String {
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    request
}

fn bearer(token: &str) -> (&'static str, String) {
    ("Authorization", format!("Bearer {token}"))
}

#[tokio::test]
async fn the_native_harness_client_drives_the_http_server() {
    let ran = Arc::new(AtomicUsize::new(0));
    let http = start_http(&ran, &[]).await;
    let spec = McpServerSpec::Http {
        url: format!("http://{}/mcp", http.address),
        headers: [(
            "Authorization".to_owned(),
            format!("Bearer {}", token("c1", &["echo", "counted"], 600)),
        )]
        .into(),
    };
    let client = McpClient::connect(
        "nexus",
        &spec,
        &McpLaunch {
            cwd: std::env::temp_dir(),
            env: EnvSpec::default(),
            home: None,
        },
        &McpConfig {
            allow_private_network: true,
            ..McpConfig::default()
        },
    )
    .await
    .expect("the real client completes the MCP handshake");

    let tools = client.list_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["counted", "echo"], "the token's tools only");
    assert!(tools.iter().find(|t| t.name == "echo").unwrap().read_only);

    let answer = client
        .call_tool("echo", json!({"text": "over http"}), std::future::pending())
        .await
        .unwrap();
    assert_eq!(answer.text, "echo: over http");
    assert!(!answer.is_error);

    // A tool outside the token is refused and never runs.
    let before = ran.load(Ordering::SeqCst);
    let refused = client
        .call_tool("write", json!({"text": "x"}), std::future::pending())
        .await
        .unwrap();
    assert!(refused.is_error, "{refused:?}");
    assert_eq!(ran.load(Ordering::SeqCst), before);
    client.close().await;
}

#[tokio::test]
async fn http_refuses_everything_that_is_not_a_well_formed_authorised_request() {
    let ran = Arc::new(AtomicUsize::new(0));
    let http = start_http(&ran, &["https://app.example.test"]).await;
    let a = http.address;
    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let good = token("s", &["echo"], 600);

    // No token, a bad token, an expired token: all one answer.
    assert_eq!(raw(a, &post(a, &[], ping)).await.status, 401);
    assert_eq!(
        raw(a, &post(a, &[bearer("garbage")], ping)).await.status,
        401
    );
    assert_eq!(
        raw(a, &post(a, &[bearer(&token("s", &["echo"], -10))], ping))
            .await
            .status,
        401
    );
    // The token must be bearer-prefixed.
    assert_eq!(
        raw(a, &post(a, &[("Authorization", good.clone())], ping))
            .await
            .status,
        401
    );

    // A page in a browser (it has an Origin) is refused unless the origin was allowed.
    let from_page = raw(
        a,
        &post(
            a,
            &[
                bearer(&good),
                ("Origin", "https://evil.example.test".into()),
            ],
            ping,
        ),
    )
    .await;
    assert_eq!(from_page.status, 403, "{}", from_page.body);
    let allowed = raw(
        a,
        &post(
            a,
            &[bearer(&good), ("Origin", "https://app.example.test".into())],
            ping,
        ),
    )
    .await;
    assert_eq!(allowed.status, 200, "{}", allowed.body);

    // A loopback server answers only to a loopback Host: a DNS-rebinding page cannot reach it.
    let rebinding = format!(
        "POST /mcp HTTP/1.1\r\nHost: attacker.example.test\r\nConnection: close\r\nAuthorization: Bearer {good}\r\nContent-Length: {}\r\n\r\n{ping}",
        ping.len()
    );
    assert_eq!(raw(a, &rebinding).await.status, 403);

    // A well-formed request works; GET is not allowed; the body is capped.
    let ok = raw(a, &post(a, &[bearer(&good)], ping)).await;
    assert_eq!(ok.status, 200);
    assert!(ok.body.contains("\"result\""), "{}", ok.body);
    assert_eq!(
        raw(
            a,
            &format!("GET /mcp HTTP/1.1\r\nHost: {a}\r\nConnection: close\r\n\r\n")
        )
        .await
        .status,
        405
    );
    let huge = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"ping","params":{{"pad":"{}"}}}}"#,
        "a".repeat(2 << 20)
    );
    assert_eq!(raw(a, &post(a, &[bearer(&good)], &huge)).await.status, 413);
    assert_eq!(
        raw(a, &post(a, &[bearer(&good)], "not json")).await.status,
        400
    );
    assert_eq!(
        raw(
            a,
            &format!("GET /health HTTP/1.1\r\nHost: {a}\r\nConnection: close\r\n\r\n")
        )
        .await
        .status,
        200
    );
}

#[tokio::test]
async fn http_handles_notifications_batches_and_the_session_header() {
    let ran = Arc::new(AtomicUsize::new(0));
    let http = start_http(&ran, &[]).await;
    let a = http.address;
    let good = bearer(&token("sess-42", &["echo"], 600));

    let notification = raw(
        a,
        &post(
            a,
            std::slice::from_ref(&good),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        ),
    )
    .await;
    assert_eq!(notification.status, 202);

    let init = raw(
        a,
        &post(
            a,
            std::slice::from_ref(&good),
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        ),
    )
    .await;
    assert_eq!(init.status, 200);
    assert!(
        init.headers.contains("mcp-session-id: sess-42"),
        "{}",
        init.headers
    );

    let batch = raw(
        a,
        &post(
            a,
            &[good],
            r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"},{"jsonrpc":"2.0","id":2,"method":"tools/list"}]"#,
        ),
    )
    .await;
    let answers: Value = serde_json::from_str(&batch.body).unwrap();
    assert_eq!(
        answers.as_array().unwrap().len(),
        2,
        "the notification has no answer"
    );
}

/// State lives with the session id, across requests, and `DELETE` forgets it. Later tools
/// need this: the files a session has read decide whether `Edit` may run.
#[tokio::test]
async fn session_state_survives_requests_and_is_forgotten_on_delete() {
    let ran = Arc::new(AtomicUsize::new(0));
    let http = start_http(&ran, &[]).await;
    let a = http.address;
    let mine = bearer(&token("mine", &["tally"], 600));
    let other = bearer(&token("other", &["tally"], 600));
    let tally = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"tally","arguments":{}}}"#;
    let count = |r: &Reply| -> String {
        let v: Value = serde_json::from_str(&r.body).unwrap();
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    assert_eq!(
        count(&raw(a, &post(a, std::slice::from_ref(&mine), tally)).await),
        "1"
    );
    assert_eq!(
        count(&raw(a, &post(a, std::slice::from_ref(&mine), tally)).await),
        "2"
    );
    assert_eq!(
        count(&raw(a, &post(a, std::slice::from_ref(&other), tally)).await),
        "1",
        "another session"
    );
    let delete = format!(
        "DELETE /mcp HTTP/1.1\r\nHost: {a}\r\nConnection: close\r\nAuthorization: {}\r\n\r\n",
        mine.1
    );
    assert_eq!(raw(a, &delete).await.status, 204);
    assert_eq!(
        count(&raw(a, &post(a, &[mine], tally)).await),
        "1",
        "forgotten after DELETE"
    );
}
