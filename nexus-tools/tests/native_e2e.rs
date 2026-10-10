//! `nexus-tools` behind the native harness (N24): a session with no tool configuration has the
//! tools; a policy written the Claude Code way applies to them; each tool works through a
//! scripted model turn; the security rules hold end to end.
//!
//! Real `NativeProvider`, real `nexus-tools` process over stdio (the harness's own child), a
//! scripted OpenAI-compatible server, a local fake search engine. Nothing leaves 127.0.0.1.
#![cfg(unix)]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use nexus_claude::agent::{
    AgentEvent, AgentProvider, AgentSession, BackgroundTask, BackgroundTaskKind,
    BackgroundTaskStatus, CancelScope, PermissionDecision, PolicyMode, ProviderError, SessionSpec,
    StopReason, ToolCategory, ToolOutput, ToolPolicy, TurnInput,
};
use nexus_claude::model::{EndpointQuirks, OpenAiEndpoint, OpenAiEndpointConfig};
use nexus_claude::providers::native::{DefaultTools, NativeConfig, NativeProvider};
use nexus_claude::testkit::conformance::{
    ConformanceTarget, Prepared, Scenario, ScenarioOutcome, run_scenario,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const NEXUS_TOOLS: &str = env!("CARGO_BIN_EXE_nexus-tools");

// ---------------------------------------------------------------------------
// A scripted OpenAI-compatible server
// ---------------------------------------------------------------------------

struct Route {
    needle: Option<String>,
    events: Vec<Value>,
}

struct Model {
    addr: std::net::SocketAddr,
}

fn delta(delta: Value) -> Value {
    json!({"choices": [{"index": 0, "delta": delta}]})
}

fn finish(reason: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
}

/// The probe the harness makes before it trusts a model with tools.
fn probe() -> Route {
    Route {
        needle: Some("Call the ping tool now".into()),
        events: vec![
            delta(
                json!({"tool_calls": [{"index": 0, "id": "p1", "function": {"name": "ping", "arguments": "{}"}}]}),
            ),
            finish("tool_calls"),
            json!("[DONE]"),
        ],
    }
}

/// The model asks for one tool call, once the request carries `after` (the previous call's
/// result), or at once when `after` is `None`.
fn call(after: Option<&str>, id: &str, name: &str, arguments: Value) -> Route {
    Route {
        needle: after.map(|a| format!("\"tool_call_id\":\"{a}\"")),
        events: vec![
            delta(
                json!({"tool_calls": [{"index": 0, "id": id, "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}]}),
            ),
            finish("tool_calls"),
            json!("[DONE]"),
        ],
    }
}

fn say(after: Option<&str>, text: &str) -> Route {
    Route {
        needle: after.map(|a| format!("\"tool_call_id\":\"{a}\"")),
        events: vec![
            delta(json!({"content": text})),
            finish("stop"),
            json!("[DONE]"),
        ],
    }
}

async fn model(routes: Vec<Route>) -> Model {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let routes = Arc::new(Mutex::new(
        routes.into_iter().map(|r| (r, false)).collect::<Vec<_>>(),
    ));
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let routes = Arc::clone(&routes);
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head_end, length) = loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                    if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buffer[..at]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (at + 4, length);
                    }
                };
                while buffer.len() < head_end + length {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                }
                let body =
                    String::from_utf8_lossy(&buffer[head_end..head_end + length]).into_owned();
                let events = {
                    let mut routes = routes.lock().unwrap();
                    let matches =
                        |r: &Route| r.needle.as_ref().is_none_or(|n| body.contains(n.as_str()));
                    let pick = routes
                        .iter()
                        .position(|(r, used)| !*used && matches(r))
                        .or_else(|| routes.iter().rposition(|(r, _)| matches(r)));
                    match pick {
                        Some(i) => {
                            routes[i].1 = true;
                            routes[i].0.events.clone()
                        },
                        None => vec![
                            delta(json!({"content": "(no scripted answer)"})),
                            finish("stop"),
                            json!("[DONE]"),
                        ],
                    }
                };
                let mut out = String::from(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                );
                for event in events {
                    let text = match &event {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    out.push_str(&format!("data: {text}\n\n"));
                }
                let _ = stream.write_all(out.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    Model { addr }
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

struct Rig {
    provider: NativeProvider,
    cwd: tempfile::TempDir,
}

fn rig(model: &Model, tools: Option<DefaultTools>) -> Rig {
    rig_in(tempfile::tempdir().unwrap(), model, tools)
}

fn rig_in(cwd: tempfile::TempDir, model: &Model, tools: Option<DefaultTools>) -> Rig {
    let mut config = NativeConfig::new("native-e2e");
    config.default_model = Some("m".to_owned());
    config.default_tools = tools;
    let mut endpoint = OpenAiEndpointConfig::new("native-e2e", format!("http://{}/v1", model.addr));
    endpoint.quirks = EndpointQuirks::deepseek();
    endpoint.response_timeout = Duration::from_secs(20);
    endpoint.idle_timeout = Duration::from_secs(20);
    let endpoint = Arc::new(OpenAiEndpoint::new(
        endpoint,
        Arc::new(nexus_claude::agent::EnvCredentialResolver),
    ));
    Rig {
        provider: NativeProvider::new(config, endpoint),
        cwd,
    }
}

fn default_tools(extra: &[&str]) -> DefaultTools {
    let mut tools = DefaultTools::new(NEXUS_TOOLS);
    tools.args = extra.iter().map(|a| (*a).to_owned()).collect();
    tools
}

fn policy(mode: PolicyMode, allow: &[&str], deny: &[&str]) -> ToolPolicy {
    ToolPolicy::from_patterns(mode, allow, deny).unwrap()
}

impl Rig {
    async fn open(&self, policy: ToolPolicy) -> Arc<dyn AgentSession> {
        let mut spec = SessionSpec::new(self.cwd.path());
        spec.model = Some("m".to_owned());
        spec.policy = policy;
        self.provider.open(spec).await.expect("the session opens")
    }
}

/// One turn, answering every permission request with `decide(canonical name, input)`.
async fn turn(
    session: &dyn AgentSession,
    decide: impl Fn(&str, &Value) -> PermissionDecision,
) -> Vec<AgentEvent> {
    let mut stream = session
        .send_turn(TurnInput::text("go"))
        .await
        .expect("send_turn");
    let mut events = Vec::new();
    let read = async {
        while let Some(event) = stream.next().await {
            if let AgentEvent::PermissionAsk {
                request_id,
                canonical,
                input,
                ..
            } = &event
            {
                let answer = decide(canonical.as_deref().unwrap_or_default(), input);
                session
                    .answer_permission(request_id, answer)
                    .await
                    .expect("answer");
            }
            events.push(event);
        }
    };
    tokio::time::timeout(Duration::from_secs(60), read)
        .await
        .expect("the turn ends");
    events
}

fn deny_all(_: &str, _: &Value) -> PermissionDecision {
    PermissionDecision::deny()
}

/// `(tool call id → (canonical, category, output text, is_error))`, in call order.
fn results(events: &[AgentEvent]) -> Vec<(String, String, ToolCategory, String, bool)> {
    let mut calls = Vec::new();
    for event in events {
        if let AgentEvent::ToolCall {
            id,
            canonical,
            category,
            ..
        } = event
        {
            calls.push((id.clone(), canonical.clone().unwrap_or_default(), *category));
        }
    }
    calls
        .into_iter()
        .map(|(id, canonical, category)| {
            let (text, is_error) = events
                .iter()
                .find_map(|e| match e {
                    AgentEvent::ToolResult {
                        id: rid,
                        output,
                        is_error,
                        ..
                    } if *rid == id => {
                        let text = match output {
                            Some(ToolOutput::Text(t)) => t.clone(),
                            Some(ToolOutput::Blocks(b)) => Value::Array(b.clone()).to_string(),
                            _ => String::new(),
                        };
                        Some((text, *is_error))
                    },
                    _ => None,
                })
                .unwrap_or_default();
            (id, canonical, category, text, is_error)
        })
        .collect()
}

fn completed(events: &[AgentEvent]) -> bool {
    events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::Done {
                stop_reason: StopReason::Completed,
                ..
            }
        )
    })
}

fn file(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The default profile
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_session_with_no_tool_configuration_has_the_tools() {
    let search = nexus_tools::fake_search::start("unused").await;
    let model = model(vec![probe(), say(None, "hi")]).await;
    let rig = rig(
        &model,
        Some(default_tools(&[
            "--search-engine",
            &format!("searxng:{}/searxng", search.base()),
            "--search-allow-private",
        ])),
    );
    let session = rig.open(policy(PolicyMode::Ask, &[], &[])).await;
    let mut notices = session.out_of_band().expect("out of band events");
    let started = tokio::time::timeout(Duration::from_secs(10), notices.next())
        .await
        .unwrap()
        .unwrap();
    let AgentEvent::SessionStarted {
        tools, mcp_servers, ..
    } = started
    else {
        panic!("{started:?}")
    };
    for name in [
        "Read",
        "Write",
        "Edit",
        "Glob",
        "Grep",
        "NotebookEdit",
        "Bash",
        "WebFetch",
        "WebSearch",
    ] {
        assert!(
            tools.contains(&format!("mcp__nexus__{name}")),
            "{name} missing from {tools:?}"
        );
    }
    assert!(
        mcp_servers
            .iter()
            .any(|s| s.name == "nexus" && s.status == "connected"),
        "{mcp_servers:?}"
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn no_default_tools_unless_the_instance_asks_for_them() {
    let model = model(vec![probe(), say(None, "hi")]).await;
    let rig = rig(&model, None);
    let session = rig.open(policy(PolicyMode::Ask, &[], &[])).await;
    let started = session.out_of_band().unwrap().next().await.unwrap();
    let AgentEvent::SessionStarted { tools, .. } = started else {
        panic!()
    };
    assert!(tools.is_empty(), "{tools:?}");
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_policy_allow_list_exposes_tools_by_their_canonical_name() {
    let model = model(vec![probe(), say(None, "hi")]).await;
    let rig = rig(&model, Some(default_tools(&[])));
    let session = rig
        .open(policy(PolicyMode::Ask, &["Read", "Grep", "Glob"], &[]))
        .await;
    let started = session.out_of_band().unwrap().next().await.unwrap();
    let AgentEvent::SessionStarted { tools, .. } = started else {
        panic!()
    };
    let mut tools = tools;
    tools.sort();
    assert_eq!(
        tools,
        ["mcp__nexus__Glob", "mcp__nexus__Grep", "mcp__nexus__Read"]
    );
    session.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Each tool, through a scripted turn
// ---------------------------------------------------------------------------

fn in_dir(cwd: &tempfile::TempDir, name: &str) -> String {
    std::fs::canonicalize(cwd.path())
        .unwrap()
        .join(name)
        .display()
        .to_string()
}

#[tokio::test]
async fn every_tool_works_through_the_native_harness_with_its_canonical_name_and_category() {
    let search = nexus_tools::fake_search::start("unused").await;
    let cwd = tempfile::tempdir().unwrap();
    let (a, nb) = (in_dir(&cwd, "a.txt"), in_dir(&cwd, "nb.ipynb"));
    let notebook = "{\"cells\":[{\"id\":\"c1\",\"cell_type\":\"code\",\"metadata\":{},\"source\":\"x\",\"outputs\":[],\"execution_count\":null}],\"metadata\":{},\"nbformat\":4,\"nbformat_minor\":5}";
    let model = model(vec![
        probe(),
        call(
            None,
            "w1",
            "mcp__nexus__Write",
            json!({"file_path": a, "content": "alpha\nbeta\nalpha 2\n"}),
        ),
        call(
            Some("w1"),
            "r1",
            "mcp__nexus__Read",
            json!({"file_path": a}),
        ),
        call(
            Some("r1"),
            "e1",
            "mcp__nexus__Edit",
            json!({"file_path": a, "old_string": "beta", "new_string": "BETA"}),
        ),
        call(
            Some("e1"),
            "g1",
            "mcp__nexus__Glob",
            json!({"pattern": "*.txt"}),
        ),
        call(
            Some("g1"),
            "s1",
            "mcp__nexus__Grep",
            json!({"pattern": "alpha", "output_mode": "content"}),
        ),
        call(
            Some("s1"),
            "n0",
            "mcp__nexus__Write",
            json!({"file_path": nb, "content": notebook}),
        ),
        call(
            Some("n0"),
            "n1",
            "mcp__nexus__NotebookEdit",
            json!({"notebook_path": nb, "cell_id": "c1", "new_source": "y = 2"}),
        ),
        call(
            Some("n1"),
            "b1",
            "mcp__nexus__Bash",
            json!({"command": "echo hello from bash"}),
        ),
        call(
            Some("b1"),
            "f1",
            "mcp__nexus__WebFetch",
            json!({"url": "http://127.0.0.1:9/", "prompt": "x"}),
        ),
        call(
            Some("f1"),
            "q1",
            "mcp__nexus__WebSearch",
            json!({"query": "native harness tools"}),
        ),
        say(Some("q1"), "all done"),
    ])
    .await;
    let rig = rig_in(
        cwd,
        &model,
        Some(default_tools(&[
            "--search-engine",
            &format!("searxng:{}/searxng", search.base()),
            "--search-allow-private",
        ])),
    );
    let allow = [
        "Read",
        "Write",
        "Edit",
        "Glob",
        "Grep",
        "NotebookEdit",
        "Bash(echo *)",
        "WebFetch",
        "WebSearch",
    ];
    let session = rig.open(policy(PolicyMode::Ask, &allow, &[])).await;
    let events = turn(&*session, deny_all).await;
    assert!(completed(&events), "{events:#?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::PermissionAsk { .. })),
        "everything was allowed by pattern: nothing should have asked"
    );
    let results = results(&events);
    let by_id = |id: &str| {
        results
            .iter()
            .find(|r| r.0 == id)
            .unwrap_or_else(|| panic!("no result for {id}"))
    };

    // Canonical name and category on the event, not `mcp__nexus__…` / `mcp`.
    for (id, name, category) in [
        ("w1", "Write", ToolCategory::Edit),
        ("r1", "Read", ToolCategory::Read),
        ("e1", "Edit", ToolCategory::Edit),
        ("g1", "Glob", ToolCategory::Search),
        ("s1", "Grep", ToolCategory::Search),
        ("n1", "NotebookEdit", ToolCategory::Edit),
        ("b1", "Bash", ToolCategory::Command),
        ("f1", "WebFetch", ToolCategory::Web),
        ("q1", "WebSearch", ToolCategory::Web),
    ] {
        let r = by_id(id);
        assert_eq!((r.1.as_str(), r.2), (name, category), "{id}");
    }
    assert!(
        by_id("w1").3.starts_with("File created successfully"),
        "{}",
        by_id("w1").3
    );
    assert!(
        by_id("r1").3.starts_with("1\talpha\n2\tbeta\n3\talpha 2"),
        "{}",
        by_id("r1").3
    );
    assert!(
        by_id("e1").3.contains("has been updated successfully"),
        "{}",
        by_id("e1").3
    );
    assert_eq!(file(&a), "alpha\nBETA\nalpha 2\n");
    assert!(by_id("g1").3.contains("a.txt"), "{}", by_id("g1").3);
    assert!(
        by_id("s1").3.contains("1:alpha") && by_id("s1").3.contains("3:alpha 2"),
        "{}",
        by_id("s1").3
    );
    assert!(
        by_id("n1").3.starts_with("Updated cell c1"),
        "{}",
        by_id("n1").3
    );
    assert_eq!(by_id("b1").3, "hello from bash");
    // WebFetch of a loopback address: refused by the SSRF guard, end to end.
    assert!(
        by_id("f1").4 && by_id("f1").3.contains("blocked_address"),
        "{:?}",
        by_id("f1")
    );
    assert!(
        !by_id("q1").4 && by_id("q1").3.contains("Result 1 for native harness tools"),
        "{}",
        by_id("q1").3
    );
    session.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Policy patterns written the Claude Code way
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_deny_on_a_canonical_name_stops_the_call_before_the_tool_runs() {
    let cwd = tempfile::tempdir().unwrap();
    std::fs::write(cwd.path().join(".env"), "SECRET=hunter2\n").unwrap();
    std::fs::write(cwd.path().join("fine.txt"), "fine\n").unwrap();
    let marker = in_dir(&cwd, "marker");
    let model = model(vec![
        probe(),
        call(
            None,
            "r1",
            "mcp__nexus__Read",
            json!({"file_path": in_dir(&cwd, ".env")}),
        ),
        call(
            Some("r1"),
            "r2",
            "mcp__nexus__Read",
            json!({"file_path": "./sub/../.env"}),
        ),
        call(
            Some("r2"),
            "r3",
            "mcp__nexus__Read",
            json!({"file_path": "fine.txt"}),
        ),
        call(
            Some("r3"),
            "b1",
            "mcp__nexus__Bash",
            json!({"command": format!("echo x; touch {marker}")}),
        ),
        say(Some("b1"), "done"),
    ])
    .await;
    let rig = rig_in(cwd, &model, Some(default_tools(&[])));
    let session = rig
        .open(policy(
            PolicyMode::Ask,
            &["Read", "Bash"],
            &["Read(.env*)", "Bash(touch *)"],
        ))
        .await;
    let events = turn(&*session, deny_all).await;
    let all = results(&events);
    let by_id = |id: &str| all.iter().find(|r| r.0 == id).unwrap();
    // Both spellings of the secret file are refused, and what the file holds never appears.
    for id in ["r1", "r2"] {
        assert!(by_id(id).4, "{id} should be an error: {:?}", by_id(id));
        assert!(by_id(id).3.contains("denied by policy"), "{}", by_id(id).3);
    }
    assert!(
        !format!("{events:?}").contains("hunter2"),
        "the secret reached the events"
    );
    assert_eq!(
        by_id("r3").3,
        "1\tfine\n2\t",
        "an ordinary read still works"
    );
    // The command hid a denied one behind an allowed `echo`: refused, and the tool never ran.
    assert!(
        by_id("b1").4 && by_id("b1").3.contains("denied by policy"),
        "{:?}",
        by_id("b1")
    );
    assert!(
        !Path::new(&marker).exists(),
        "the denied command ran anyway"
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn an_allowed_prefix_does_not_cover_a_command_chained_after_it() {
    let cwd = tempfile::tempdir().unwrap();
    let marker = in_dir(&cwd, "marker");
    let model = model(vec![
        probe(),
        call(
            None,
            "b1",
            "mcp__nexus__Bash",
            json!({"command": "echo ok"}),
        ),
        call(
            Some("b1"),
            "b2",
            "mcp__nexus__Bash",
            json!({"command": format!("echo ok && touch {marker}")}),
        ),
        say(Some("b2"), "done"),
    ])
    .await;
    let rig = rig_in(cwd, &model, Some(default_tools(&[])));
    let session = rig
        .open(policy(PolicyMode::Ask, &["Bash(echo *)"], &[]))
        .await;
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&asked);
    let events = turn(&*session, move |name, input| {
        log.lock().unwrap().push((
            name.to_owned(),
            input["command"].as_str().unwrap_or_default().to_owned(),
        ));
        PermissionDecision::deny()
    })
    .await;
    let all = results(&events);
    assert_eq!(
        all.iter().find(|r| r.0 == "b1").unwrap().3,
        "ok",
        "the plain echo ran without asking"
    );
    // The chained one asked (it is not covered by `Bash(echo *)`) and, refused, never ran.
    let asked = asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0].0, "Bash", "the request carries the canonical name");
    assert!(asked[0].1.contains("&& touch"), "{asked:?}");
    assert!(!Path::new(&marker).exists());
    session.close().await.unwrap();
}

#[tokio::test]
async fn auto_edits_lets_edits_through_and_still_asks_for_commands_and_the_web() {
    let cwd = tempfile::tempdir().unwrap();
    let a = in_dir(&cwd, "a.txt");
    let model = model(vec![
        probe(),
        call(
            None,
            "w1",
            "mcp__nexus__Write",
            json!({"file_path": a, "content": "x\n"}),
        ),
        call(
            Some("w1"),
            "b1",
            "mcp__nexus__Bash",
            json!({"command": "ls"}),
        ),
        call(
            Some("b1"),
            "f1",
            "mcp__nexus__WebFetch",
            json!({"url": "https://example.com/"}),
        ),
        say(Some("f1"), "done"),
    ])
    .await;
    let rig = rig_in(cwd, &model, Some(default_tools(&[])));
    let session = rig.open(policy(PolicyMode::AutoEdits, &[], &[])).await;
    let asked = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&asked);
    turn(&*session, move |name, _| {
        log.lock().unwrap().push(name.to_owned());
        PermissionDecision::deny()
    })
    .await;
    assert_eq!(file(&a), "x\n", "the write went through without asking");
    assert_eq!(*asked.lock().unwrap(), ["Bash", "WebFetch"]);
    session.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// The security rules of N15, on these tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_tool_outside_the_profile_is_refused_and_never_runs() {
    let cwd = tempfile::tempdir().unwrap();
    let marker = in_dir(&cwd, "marker");
    let model = model(vec![
        probe(),
        call(
            None,
            "b1",
            "mcp__nexus__Bash",
            json!({"command": format!("touch {marker}")}),
        ),
        say(Some("b1"), "done"),
    ])
    .await;
    let rig = rig_in(cwd, &model, Some(default_tools(&[])));
    // The profile is `Read` only: `Bash` was never offered.
    let session = rig.open(policy(PolicyMode::Ask, &["Read"], &[])).await;
    let events = turn(&*session, deny_all).await;
    let all = results(&events);
    let b1 = all.iter().find(|r| r.0 == "b1").unwrap();
    assert!(
        b1.4 && (b1.3.contains("unknown tool") || b1.3.contains("not available")),
        "{b1:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::PermissionAsk { .. })),
        "not even asked"
    );
    assert!(!Path::new(&marker).exists());
    session.close().await.unwrap();
}

/// The process the real `NativeProvider` launches for a session carries the session's bound
/// (N27): `--tools` is what its policy exposes, `--cwd` its own directory. Read from `ps`, so the
/// launch path of `open` is what is checked, not a helper.
#[tokio::test]
async fn the_harness_launches_the_session_server_bounded_by_its_policy() {
    let model = model(vec![probe(), say(None, "hi")]).await;
    let rig = rig(&model, Some(default_tools(&[])));
    let session = rig
        .open(policy(PolicyMode::Ask, &["Read", "Grep"], &["Bash"]))
        .await;
    let cwd = rig.cwd.path().display().to_string();
    let output = std::process::Command::new("ps")
        .args(["-axww", "-o", "args="])
        .output()
        .expect("ps");
    let listing = String::from_utf8_lossy(&output.stdout);
    let line = listing
        .lines()
        .find(|l| l.contains(NEXUS_TOOLS) && l.contains(&cwd))
        .unwrap_or_else(|| panic!("no nexus-tools process for {cwd}:\n{listing}"));
    assert!(line.contains("--trust-harness"), "{line}");
    assert!(line.contains("--tools Read,Grep"), "{line}");
    assert!(!line.contains("--listen"), "{line}");
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_command_sees_no_host_variable_through_the_harness() {
    let cwd = tempfile::tempdir().unwrap();
    // CARGO_MANIFEST_DIR is in this test's environment (cargo sets it).
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
    let model = model(vec![
        probe(),
        call(None, "b1", "mcp__nexus__Bash", json!({"command": "echo \"[$CARGO_MANIFEST_DIR][$HOME]\"; env | cut -d= -f1 | sort | tr '\\n' ' '"})),
        say(Some("b1"), "done"),
    ])
    .await;
    let rig = rig_in(cwd, &model, Some(default_tools(&[])));
    let session = rig.open(policy(PolicyMode::Ask, &["Bash"], &[])).await;
    let events = turn(&*session, deny_all).await;
    let all = results(&events);
    let out = &all.iter().find(|r| r.0 == "b1").unwrap().3;
    let first = out.lines().next().unwrap();
    assert!(
        first.starts_with("[]["),
        "the host variable reached the command: {out}"
    );
    assert!(!out.contains("CARGO_"), "{out}");
    let real_home = std::env::var("HOME").unwrap_or_default();
    assert!(
        real_home.is_empty() || !first.contains(&real_home),
        "the user's HOME is visible: {first}"
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_session_that_ends_leaves_no_command_running() {
    let cwd = tempfile::tempdir().unwrap();
    let pid_file = in_dir(&cwd, "pid");
    let model = model(vec![
        probe(),
        call(None, "b1", "mcp__nexus__Bash", json!({"command": format!("echo $$ > {pid_file}; exec sleep 300"), "run_in_background": true})),
        say(Some("b1"), "started"),
    ])
    .await;
    let rig = rig_in(cwd, &model, Some(default_tools(&[])));
    let session = rig.open(policy(PolicyMode::Ask, &["Bash"], &[])).await;
    turn(&*session, deny_all).await;
    for _ in 0..100 {
        if file(&pid_file).trim().parse::<u32>().is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pid: u32 = file(&pid_file)
        .trim()
        .parse()
        .expect("the background command started");
    let alive = |pid: u32| {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    assert!(alive(pid));
    session.close().await.unwrap();
    drop(session);
    for _ in 0..150 {
        if !alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the command outlived the session");
}

// ---------------------------------------------------------------------------
// Background tasks (contract §4 `background_tasks`, §10 `cancel_tools(task)`)
// ---------------------------------------------------------------------------

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

async fn wait_for_pid(path: &str) -> u32 {
    for _ in 0..100 {
        if let Ok(pid) = file(path).trim().parse::<u32>() {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the background command never wrote its pid");
}

/// Every task of every `background_tasks` snapshot, in order.
fn snapshots(events: &[AgentEvent]) -> Vec<Vec<BackgroundTask>> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::BackgroundTasks { tasks } => Some(tasks.clone()),
            _ => None,
        })
        .collect()
}

/// The out-of-band events of a session, collected in the background.
fn collect_out_of_band(session: &dyn AgentSession) -> Arc<Mutex<Vec<AgentEvent>>> {
    let mut stream = session.out_of_band().expect("the out-of-band stream");
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            sink.lock().unwrap().push(event);
        }
    });
    events
}

#[tokio::test]
async fn a_background_command_is_tracked_and_cancel_by_task_kills_its_process_group() {
    let cwd = tempfile::tempdir().unwrap();
    let pid_file = in_dir(&cwd, "pid");
    let command = format!("echo $$ > {pid_file}; exec sleep 300");
    let model = model(vec![
        probe(),
        call(
            None,
            "b1",
            "mcp__nexus__Bash",
            json!({"command": command, "run_in_background": true}),
        ),
        say(Some("b1"), "started"),
    ])
    .await;
    let rig = rig_in(cwd, &model, Some(default_tools(&[])));
    let session = rig.open(policy(PolicyMode::Ask, &["Bash"], &[])).await;
    assert!(
        session.capabilities().background_tasks,
        "a session whose nexus-tools serves Bash declares background_tasks"
    );
    let out_of_band = collect_out_of_band(&*session);
    let events = turn(&*session, deny_all).await;
    assert!(completed(&events), "{events:?}");

    // The snapshot comes after the result of the call that started the task.
    let result_at = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolResult { id, is_error: false, .. } if id == "b1"))
        .expect("the Bash call succeeded");
    let snapshot_at = events
        .iter()
        .position(|e| matches!(e, AgentEvent::BackgroundTasks { .. }))
        .expect("a background_tasks snapshot in the turn");
    assert!(snapshot_at > result_at);
    let tasks = snapshots(&events).remove(0);
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    let task = tasks[0].clone();
    assert_eq!(task.kind, BackgroundTaskKind::Shell);
    assert_eq!(task.status, BackgroundTaskStatus::Running);
    assert_eq!(task.description, command);
    assert_eq!(task.tool_call_id.as_deref(), Some("b1"));
    assert!(task.started_at_ms.is_some());
    // The id is nexus-tools' own (the one its TaskStop knows), named in the text too.
    let (_, _, _, text, _) = results(&events).remove(0);
    assert!(
        text.contains(&format!("ID: {}", task.id)),
        "{} not in {text}",
        task.id
    );

    let pid = wait_for_pid(&pid_file).await;
    assert!(alive(pid));
    assert_eq!(task.pid.map(alive), Some(true));

    let outcome = session
        .cancel_tools(CancelScope::Task {
            id: task.id.clone(),
        })
        .await
        .expect("cancel_tools(task)");
    assert_eq!(outcome.tools_cancelled, 1);
    for _ in 0..100 {
        if !alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !alive(pid),
        "the background command survived cancel_tools(task)"
    );

    // The table says so, out of band (no turn runs), and only once.
    let mut killed = Vec::new();
    for _ in 0..100 {
        killed = snapshots(&out_of_band.lock().unwrap())
            .into_iter()
            .filter(|tasks| {
                tasks
                    .iter()
                    .any(|t| t.id == task.id && t.status == BackgroundTaskStatus::Killed)
            })
            .collect();
        if !killed.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(killed.len(), 1, "{:?}", out_of_band.lock().unwrap());
    assert_eq!(killed[0].len(), 1);

    // A task that is over: nothing to stop. An unknown one: an invalid request.
    let again = session
        .cancel_tools(CancelScope::Task {
            id: task.id.clone(),
        })
        .await
        .expect("cancel_tools(task) of a stopped task");
    assert_eq!(again.tools_cancelled, 0);
    assert!(matches!(
        session
            .cancel_tools(CancelScope::Task { id: "nope".into() })
            .await,
        Err(ProviderError::InvalidRequest { .. })
    ));
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_background_command_that_ends_on_its_own_is_reported_completed() {
    let model = model(vec![
        probe(),
        call(
            None,
            "b1",
            "mcp__nexus__Bash",
            json!({"command": "sleep 0.3", "run_in_background": true}),
        ),
        say(Some("b1"), "started"),
    ])
    .await;
    let rig = rig(&model, Some(default_tools(&[])));
    let session = rig.open(policy(PolicyMode::Trust, &[], &[])).await;
    let out_of_band = collect_out_of_band(&*session);
    let events = turn(&*session, deny_all).await;
    let id = snapshots(&events)
        .first()
        .and_then(|tasks| tasks.first())
        .map(|task| task.id.clone())
        .expect("a background_tasks snapshot");
    for _ in 0..100 {
        let all: Vec<AgentEvent> = events
            .iter()
            .cloned()
            .chain(out_of_band.lock().unwrap().iter().cloned())
            .collect();
        if snapshots(&all).iter().any(|tasks| {
            tasks
                .iter()
                .any(|t| t.id == id && t.status == BackgroundTaskStatus::Completed)
        }) {
            session.close().await.unwrap();
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "the end of the task was never reported: {:?}",
        out_of_band.lock().unwrap()
    );
}

#[tokio::test]
async fn without_bash_a_session_has_no_background_tasks() {
    let model = model(vec![probe(), say(None, "hi")]).await;
    let rig = rig(&model, Some(default_tools(&[])));
    let session = rig
        .open(policy(PolicyMode::Ask, &["Read", "Grep"], &[]))
        .await;
    assert!(!session.capabilities().background_tasks);
    assert_eq!(
        session
            .cancel_tools(CancelScope::Task { id: "t".into() })
            .await
            .unwrap_err(),
        ProviderError::unsupported("background_tasks")
    );
    session.close().await.unwrap();
}

/// The conformance scenario `annulation_tache`, played for real: the native harness, the real
/// `nexus-tools`, a background `sleep` started by a scripted model, cancelled by task.
struct BackgroundTarget {
    provider: Arc<NativeProvider>,
}

#[async_trait::async_trait]
impl ConformanceTarget for BackgroundTarget {
    fn name(&self) -> &str {
        "native + nexus-tools (background tasks)"
    }

    fn provider(&self) -> Arc<dyn AgentProvider> {
        self.provider.clone()
    }

    async fn prepare(&self, scenario: Scenario) -> Option<Prepared> {
        let model = model(vec![
            probe(),
            call(
                None,
                "b1",
                "mcp__nexus__Bash",
                json!({"command": "exec sleep 300", "run_in_background": true}),
            ),
            say(Some("b1"), "started"),
        ])
        .await;
        let cwd = tempfile::tempdir().unwrap();
        let rig = rig_in(cwd, &model, Some(default_tools(&[])));
        let provider = Arc::new(rig.provider);
        provider.refresh_capabilities("m").await.ok()?;
        let mut spec = SessionSpec::new(rig.cwd.path());
        spec.model = Some("m".to_owned());
        let _ = scenario;
        Some(Prepared {
            provider,
            spec,
            resume: None,
            guard: Some(Box::new(rig.cwd)),
        })
    }
}

#[tokio::test]
async fn the_task_cancel_conformance_scenario_plays_for_real_on_native() {
    let model = model(vec![probe()]).await;
    let rig = rig(&model, Some(default_tools(&[])));
    let provider = Arc::new(rig.provider);
    provider.refresh_capabilities("m").await.unwrap();
    assert!(provider.capabilities(Some("m")).background_tasks);
    let target = BackgroundTarget { provider };
    assert_eq!(
        run_scenario(&target, Scenario::AnnulationTache).await,
        ScenarioOutcome::Passed
    );
}
