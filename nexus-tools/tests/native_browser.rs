//! The optional browser behind the native harness (N23): attached only when installed and
//! authorised, never with stealth or private-network access, classified read / navigation /
//! interaction, destinations judged before the call. `fake_obscura` imitates the tool list of
//! `obscura mcp`; the test against a real Obscura is `#[ignore]`d (none is installed here).
#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use nexus_claude::agent::{
    AgentEvent, AgentProvider, AgentSession, PermissionDecision, PolicyMode, SessionSpec,
    ToolOutput, ToolPolicy, TurnInput,
};
use nexus_claude::model::{EndpointQuirks, OpenAiEndpoint, OpenAiEndpointConfig};
use nexus_claude::providers::native::{BrowserTools, NativeConfig, NativeProvider};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const FAKE_OBSCURA: &str = env!("CARGO_BIN_EXE_fake_obscura");

// A scripted model: the same few lines as native_e2e.rs.
fn delta(d: Value) -> Value {
    json!({"choices": [{"index": 0, "delta": d}]})
}
fn finish(r: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": r}]})
}

type Step = (Option<String>, Vec<Value>);

fn probe() -> Step {
    (
        Some("Call the ping tool now".into()),
        vec![
            delta(
                json!({"tool_calls": [{"index": 0, "id": "p1", "function": {"name": "ping", "arguments": "{}"}}]}),
            ),
            finish("tool_calls"),
            json!("[DONE]"),
        ],
    )
}

fn call(after: Option<&str>, id: &str, name: &str, args: Value) -> Step {
    (
        after.map(|a| format!("\"tool_call_id\":\"{a}\"")),
        vec![
            delta(
                json!({"tool_calls": [{"index": 0, "id": id, "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}),
            ),
            finish("tool_calls"),
            json!("[DONE]"),
        ],
    )
}

fn say(after: Option<&str>) -> Step {
    (
        after.map(|a| format!("\"tool_call_id\":\"{a}\"")),
        vec![
            delta(json!({"content": "done"})),
            finish("stop"),
            json!("[DONE]"),
        ],
    )
}

async fn model(steps: Vec<Step>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let steps = Arc::new(Mutex::new(
        steps.into_iter().map(|s| (s, false)).collect::<Vec<_>>(),
    ));
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let steps = Arc::clone(&steps);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head, len) = loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let h = String::from_utf8_lossy(&buf[..at]).to_ascii_lowercase();
                        let l = h
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (at + 4, l);
                    }
                };
                while buf.len() < head + len {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = String::from_utf8_lossy(&buf[head..head + len]).into_owned();
                let events = {
                    let mut steps = steps.lock().unwrap();
                    let m = |s: &Step| s.0.as_ref().is_none_or(|n| body.contains(n.as_str()));
                    let pick = steps
                        .iter()
                        .position(|(s, used)| !*used && m(s))
                        .or_else(|| steps.iter().rposition(|(s, _)| m(s)));
                    match pick {
                        Some(i) => {
                            steps[i].1 = true;
                            steps[i].0.1.clone()
                        },
                        None => vec![
                            delta(json!({"content": "?"})),
                            finish("stop"),
                            json!("[DONE]"),
                        ],
                    }
                };
                let mut out = String::from(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                );
                for e in events {
                    let t = match &e {
                        Value::String(s) => s.clone(),
                        o => o.to_string(),
                    };
                    out.push_str(&format!("data: {t}\n\n"));
                }
                let _ = stream.write_all(out.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    addr
}

fn provider(addr: std::net::SocketAddr, browser: Option<BrowserTools>) -> NativeProvider {
    let mut config = NativeConfig::new("browser-e2e");
    config.default_model = Some("m".into());
    config.browser = browser;
    let mut ep = OpenAiEndpointConfig::new("browser-e2e", format!("http://{addr}/v1"));
    ep.quirks = EndpointQuirks::deepseek();
    ep.response_timeout = Duration::from_secs(20);
    ep.idle_timeout = Duration::from_secs(20);
    NativeProvider::new(
        config,
        Arc::new(OpenAiEndpoint::new(
            ep,
            Arc::new(nexus_claude::agent::EnvCredentialResolver),
        )),
    )
}

fn fake(log: &std::path::Path) -> BrowserTools {
    let mut tools = BrowserTools::new(FAKE_OBSCURA);
    tools
        .env
        .insert("FAKE_OBSCURA_LOG".into(), log.display().to_string());
    tools
}

async fn open(
    p: &NativeProvider,
    mode: PolicyMode,
    allow: &[&str],
    deny: &[&str],
) -> Arc<dyn AgentSession> {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = SessionSpec::new(dir.path());
    spec.model = Some("m".into());
    spec.policy = ToolPolicy::from_patterns(mode, allow, deny).unwrap();
    let session = p.open(spec).await.expect("opens");
    std::mem::forget(dir);
    session
}

async fn started(session: &dyn AgentSession) -> (Vec<String>, Vec<AgentEvent>) {
    let mut oob = session.out_of_band().unwrap();
    let mut events = Vec::new();
    let mut tools = Vec::new();
    while let Ok(Some(e)) = tokio::time::timeout(Duration::from_millis(300), oob.next()).await {
        if let AgentEvent::SessionStarted { tools: t, .. } = &e {
            tools = t.clone();
        }
        events.push(e);
    }
    (tools, events)
}

async fn turn(
    session: &dyn AgentSession,
    decide: impl Fn(&str) -> PermissionDecision,
) -> Vec<AgentEvent> {
    let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    let mut events = Vec::new();
    let read = async {
        while let Some(e) = stream.next().await {
            if let AgentEvent::PermissionAsk {
                request_id,
                tool_name,
                ..
            } = &e
            {
                session
                    .answer_permission(request_id, decide(tool_name))
                    .await
                    .unwrap();
            }
            events.push(e);
        }
    };
    tokio::time::timeout(Duration::from_secs(60), read)
        .await
        .expect("turn ends");
    events
}

fn result_text(events: &[AgentEvent], id: &str) -> (String, bool) {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult {
                id: rid,
                output,
                is_error,
                ..
            } if rid == id => Some((
                match output {
                    Some(ToolOutput::Text(t)) => t.clone(),
                    _ => String::new(),
                },
                *is_error,
            )),
            _ => None,
        })
        .unwrap_or_default()
}

fn log_lines(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[tokio::test]
async fn without_the_executable_there_is_no_browser_tool_and_a_call_is_refused() {
    let addr = model(vec![
        probe(),
        call(
            None,
            "n1",
            "mcp__browser__browser_navigate",
            json!({"url": "https://example.com/"}),
        ),
        say(Some("n1")),
    ])
    .await;
    let p = provider(addr, Some(BrowserTools::new("/nonexistent/obscura")));
    let session = open(&p, PolicyMode::Ask, &[], &[]).await;
    let (tools, events) = started(&*session).await;
    assert!(!tools.iter().any(|t| t.contains("browser_")), "{tools:?}");
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::ProviderNotice { kind, data } if kind == "browser_unavailable" && data["reason"] == "executable_not_found")),
        "{events:?}"
    );
    let events = turn(&*session, |_| PermissionDecision::allow_once()).await;
    let (text, is_error) = result_text(&events, "n1");
    assert!(is_error && text.contains("unknown tool"), "{text}");
    session.close().await.unwrap();
}

#[tokio::test]
async fn not_configured_means_no_browser_at_all() {
    let addr = model(vec![probe(), say(None)]).await;
    let p = provider(addr, None);
    let session = open(&p, PolicyMode::Ask, &[], &[]).await;
    let (tools, events) = started(&*session).await;
    assert!(tools.is_empty());
    assert!(!events.iter().any(
        |e| matches!(e, AgentEvent::ProviderNotice { kind, .. } if kind == "browser_unavailable")
    ));
    session.close().await.unwrap();
}

#[tokio::test]
async fn an_installed_browser_is_started_as_obscura_mcp_with_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log.jsonl");
    let addr = model(vec![probe(), say(None)]).await;
    let p = provider(addr, Some(fake(&log)));
    let session = open(&p, PolicyMode::Ask, &[], &[]).await;
    let (tools, _) = started(&*session).await;
    for t in [
        "browser_navigate",
        "browser_snapshot",
        "browser_click",
        "browser_evaluate",
    ] {
        assert!(tools.contains(&format!("mcp__browser__{t}")), "{t} missing");
    }
    let start = &log_lines(&log)[0]["start"];
    assert_eq!(start["argv"], json!(["mcp"]), "no stealth, no extra flag");
    assert_eq!(start["allow_private"], "0");
    let names: Vec<&str> = start["env_names"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        !names.iter().any(|n| n.starts_with("CARGO")),
        "a host variable reached the browser: {names:?}"
    );
    session.close().await.unwrap();
    assert!(
        BrowserTools::new(FAKE_OBSCURA)
            .with_args(["--stealth"])
            .is_err()
    );
}

#[tokio::test]
async fn plan_only_offers_the_reads_and_ask_mode_asks_for_navigation_and_interaction_only() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log.jsonl");
    let addr = model(vec![probe(), say(None)]).await;
    let p = provider(addr, Some(fake(&log)));
    let plan = open(&p, PolicyMode::PlanOnly, &[], &[]).await;
    let (tools, _) = started(&*plan).await;
    assert!(tools.contains(&"mcp__browser__browser_snapshot".to_owned()));
    assert!(tools.contains(&"mcp__browser__browser_markdown".to_owned()));
    for forbidden in [
        "browser_navigate",
        "browser_click",
        "browser_evaluate",
        "browser_set_cookie",
        "browser_fill",
    ] {
        assert!(
            !tools.contains(&format!("mcp__browser__{forbidden}")),
            "{forbidden} offered in plan_only"
        );
    }
    plan.close().await.unwrap();

    let addr = model(vec![
        probe(),
        call(None, "s1", "mcp__browser__browser_snapshot", json!({})),
        call(
            Some("s1"),
            "n1",
            "mcp__browser__browser_navigate",
            json!({"url": "https://example.com/"}),
        ),
        call(
            Some("n1"),
            "c1",
            "mcp__browser__browser_click",
            json!({"selector": "#go"}),
        ),
        call(
            Some("c1"),
            "e1",
            "mcp__browser__browser_evaluate",
            json!({"expression": "1+1"}),
        ),
        say(Some("e1")),
    ])
    .await;
    let log = dir.path().join("log2.jsonl");
    let p = provider(addr, Some(fake(&log)));
    let session = open(&p, PolicyMode::Ask, &[], &[]).await;
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&asked);
    turn(&*session, move |name| {
        seen.lock().unwrap().push(name.to_owned());
        PermissionDecision::allow_once()
    })
    .await;
    assert_eq!(
        *asked.lock().unwrap(),
        [
            "mcp__browser__browser_navigate",
            "mcp__browser__browser_click",
            "mcp__browser__browser_evaluate"
        ],
        "the snapshot is a read and never asks"
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_private_destination_is_refused_before_anyone_is_asked_and_the_browser_is_not_called() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log.jsonl");
    let urls = [
        "http://127.0.0.1:8080/admin",
        "http://169.254.169.254/latest/meta-data/",
        "http://localhost/",
        "file:///etc/passwd",
        "http://[::1]/",
    ];
    let mut steps = vec![probe()];
    let mut previous: Option<String> = None;
    for (i, url) in urls.iter().enumerate() {
        let id = format!("n{i}");
        steps.push(call(
            previous.as_deref(),
            &id,
            "mcp__browser__browser_navigate",
            json!({"url": url}),
        ));
        previous = Some(id);
    }
    steps.push(call(
        previous.as_deref(),
        "ok",
        "mcp__browser__browser_navigate",
        json!({"url": "https://example.com/"}),
    ));
    steps.push(say(Some("ok")));
    let addr = model(steps).await;
    let p = provider(addr, Some(fake(&log)));
    let session = open(
        &p,
        PolicyMode::Ask,
        &["mcp__browser__browser_navigate"],
        &[],
    )
    .await;
    let events = turn(&*session, |_| panic!("a refused destination must not ask")).await;
    for (i, url) in urls.iter().enumerate() {
        let (text, is_error) = result_text(&events, &format!("n{i}"));
        assert!(
            is_error && (text.contains("private networks") || text.contains("only http")),
            "{}: {text}",
            url
        );
    }
    let calls: Vec<Value> = log_lines(&log)
        .into_iter()
        .filter(|l| l.get("call").is_some())
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "only the public address reached the browser: {calls:?}"
    );
    assert_eq!(calls[0]["arguments"]["url"], "https://example.com/");
    session.close().await.unwrap();
}

/// Against a REAL Obscura: `NEXUS_REAL_OBSCURA=/path/to/obscura cargo test -p nexus-tools --features
/// test-tools --test native_browser -- --ignored`. It drives `obscura mcp` through the harness's own MCP
/// client on a local page and reads it back as Markdown. A local page is a private address, which
/// nexus never allows: the test (and only the test) switches Obscura's own opt-in on, in the
/// environment of that one process.
#[tokio::test]
#[ignore = "needs a real obscura: set NEXUS_REAL_OBSCURA"]
async fn a_real_obscura_renders_a_local_page_as_markdown() {
    use nexus_claude::agent::McpServerSpec;
    use nexus_claude::providers::native::{McpClient, McpConfig, McpLaunch};
    let program = std::env::var("NEXUS_REAL_OBSCURA").expect("NEXUS_REAL_OBSCURA");
    let page = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = page.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = page.accept().await else {
                return;
            };
            let mut b = [0u8; 2048];
            let _ = s.read(&mut b).await;
            let body = "<html><body><h1>Hello Obscura</h1><p>rendered <b>by JavaScript</b>: <span id=x></span></p><script>document.getElementById('x').textContent = 6*7</script></body></html>";
            let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
        }
    });
    let spec = McpServerSpec::Stdio {
        command: program,
        args: vec!["mcp".into()],
        env: [("OBSCURA_ALLOW_PRIVATE_NETWORK".to_owned(), "1".to_owned())].into(),
    };
    let client = McpClient::connect(
        "browser",
        &spec,
        &McpLaunch {
            cwd: std::env::temp_dir(),
            env: Default::default(),
            home: None,
        },
        &McpConfig::default(),
    )
    .await
    .expect("handshake");
    let tools = client.list_tools().await.unwrap();
    assert!(
        tools.iter().any(|t| t.name == "browser_navigate")
            && tools.iter().any(|t| t.name == "browser_markdown"),
        "{tools:?}"
    );
    let nav = client
        .call_tool(
            "browser_navigate",
            json!({"url": format!("http://127.0.0.1:{port}/")}),
            std::future::pending(),
        )
        .await
        .unwrap();
    assert!(!nav.is_error, "{}", nav.text);
    let md = client
        .call_tool("browser_markdown", json!({}), std::future::pending())
        .await
        .unwrap();
    assert!(
        md.text.contains("Hello Obscura") && md.text.contains("42"),
        "JavaScript was not run: {}",
        md.text
    );
    client.close().await;
}
