//! Behaviour of the native harness that the conformance suite does not pin down:
//! what goes back to the model (reasoning, tool results), compaction, budgets, the
//! tool policy, resume, cancellation, MCP failures over stdio and HTTP, and the
//! absence of secrets in argv, in the child's environment, in errors and in
//! persisted transcripts. Real `NativeProvider` + `OpenAiEndpoint`, scripted
//! `fake_openai`, real `fake_mcp` processes. No network beyond 127.0.0.1.

#[path = "support/fake_openai.rs"]
mod fake_openai;
#[path = "support/native.rs"]
mod native;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fake_openai::FakeOpenAi;
use futures::StreamExt;
use native::*;
use nexus_claude::agent::{
    AgentEvent, AgentProvider, AgentSession, CancelScope, CostBasis, InputBlock, InterruptScope,
    ModelPrice, PermissionDecision, PermissionScope, PolicyMode, ProviderError, ProviderKind,
    ResumeToken, SessionSpec, StopReason, ToolOutput, ToolPattern, ToolPolicy, TurnInput,
};
use nexus_claude::agent::{
    ModelInfo, ProviderHealth, SessionLimits, SystemPromptMode, SystemPromptSpec,
};
use nexus_claude::model::{
    CompletionChunk, CompletionRequest, CompletionStream, EndpointProbe, EndpointQuirks,
    FinishReason, ModelEndpoint, PriceTable,
};
use nexus_claude::providers::native::{
    FileTranscriptStore, MemoryTranscriptStore, NativeConfig, NativeProvider, TranscriptStore,
};
use serde_json::{Value, json};

const TOOL: &str = "\"role\":\"tool\"";

fn price(per_mtok: f64) -> ModelPrice {
    ModelPrice {
        input_per_mtok: per_mtok,
        output_per_mtok: per_mtok,
        cache_read_per_mtok: None,
        cache_write_per_mtok: None,
    }
}

struct Harness {
    server: FakeOpenAi,
    provider: Arc<NativeProvider>,
    cwd: tempfile::TempDir,
    store: Arc<MemoryTranscriptStore>,
}

impl Harness {
    async fn new(routes: Vec<Value>) -> Self {
        Self::with(routes, |_| {}).await
    }

    async fn with(routes: Vec<Value>, tweak: impl FnOnce(&mut NativeConfig)) -> Self {
        let mut all = vec![probe_route(true), models_route(128_000)];
        all.extend(routes);
        Self::raw(all, EndpointQuirks::deepseek(), tweak).await
    }

    /// Every route given, nothing prepended, on `quirks`.
    async fn raw(
        all: Vec<Value>,
        quirks: EndpointQuirks,
        tweak: impl FnOnce(&mut NativeConfig),
    ) -> Self {
        let server = FakeOpenAi::start(json!(all));
        let mut config = NativeConfig::new("native-test");
        config.default_model = Some("m".to_owned());
        tweak(&mut config);
        let store = Arc::new(MemoryTranscriptStore::new());
        let provider = Arc::new(
            NativeProvider::new(config, endpoint(server.base_url(), quirks))
                .with_transcript_store(store.clone()),
        );
        Self {
            server,
            provider,
            cwd: tempfile::tempdir().unwrap(),
            store,
        }
    }

    fn spec(&self) -> SessionSpec {
        let mut spec = SessionSpec::new(self.cwd.path());
        spec.model = Some("m".to_owned());
        spec
    }

    fn spec_with_mcp(&self, log: &std::path::Path) -> SessionSpec {
        let mut spec = self.spec();
        spec.mcp_servers
            .insert("fake".to_owned(), stdio_mcp(Some(log)));
        spec
    }

    async fn open(&self, spec: SessionSpec) -> Arc<dyn AgentSession> {
        self.provider.open(spec).await.expect("open")
    }

    /// The chat requests of the turns (the probe excluded), as JSON bodies.
    fn chat(&self) -> Vec<Value> {
        self.server
            .requests_to("POST", "/v1/chat/completions")
            .into_iter()
            .map(|r| r["body"].clone())
            .filter(|body| !body.to_string().contains("Call the ping tool now"))
            .collect()
    }
}

fn log_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("mcp.jsonl")
}

async fn turn(session: &dyn AgentSession, text: &str) -> Vec<AgentEvent> {
    collect(
        session
            .send_turn(TurnInput::text(text))
            .await
            .expect("send_turn"),
    )
    .await
}

/// Runs a turn, answering every permission request with `decide`.
async fn turn_deciding(
    session: &dyn AgentSession,
    text: &str,
    decide: impl Fn(&str) -> PermissionDecision,
) -> Vec<AgentEvent> {
    let mut stream = session
        .send_turn(TurnInput::text(text))
        .await
        .expect("send_turn");
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        if let AgentEvent::PermissionAsk {
            request_id,
            tool_name,
            ..
        } = &event
        {
            session
                .answer_permission(request_id, decide(tool_name))
                .await
                .expect("answer");
        }
        events.push(event);
    }
    events
}

fn done(events: &[AgentEvent]) -> &AgentEvent {
    terminal(events)
}

fn stop_reason(events: &[AgentEvent]) -> StopReason {
    match done(events) {
        AgentEvent::Done { stop_reason, .. } => *stop_reason,
        other => panic!("expected done, got {other:?}"),
    }
}

fn tool_results(events: &[AgentEvent]) -> Vec<(String, bool, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolResult {
                id,
                output,
                is_error,
                ..
            } => Some((
                id.clone(),
                *is_error,
                match output {
                    Some(ToolOutput::Text(text)) => text.clone(),
                    other => format!("{other:?}"),
                },
            )),
            _ => None,
        })
        .collect()
}

fn mcp_calls(log: &std::path::Path, tool: &str) -> usize {
    log_events(log)
        .iter()
        .filter(|e| e["event"] == "call" && e["tool"] == tool)
        .count()
}

fn offered_tools(body: &Value) -> Vec<String> {
    let mut names: Vec<String> = body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t["function"]["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn echo_call() -> (&'static str, &'static str, Value) {
    ("c1", "mcp__fake__echo", json!({"text": "hi"}))
}

// ---------------------------------------------------------------------------
// Reasoning (A39) and compaction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deepseek_reasoning_comes_back_with_the_tools_on_every_later_request() {
    let h = Harness::new(vec![
        tool_reply(
            Some("first"),
            &[echo_call()],
            Some("I should call echo"),
            Some((100, 10)),
        ),
        text_reply(
            Some(TOOL),
            "the tool said hi",
            Some("now I answer"),
            Some((120, 10)),
        ),
        text_reply(Some("second"), "ok", None, Some((150, 5))),
    ])
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let session = h.open(h.spec_with_mcp(&log_path(&log_dir))).await;
    assert_eq!(
        stop_reason(&turn(&*session, "first").await),
        StopReason::Completed
    );
    assert_eq!(
        stop_reason(&turn(&*session, "second").await),
        StopReason::Completed
    );

    let requests = h.chat();
    assert_eq!(requests.len(), 3);
    // Round two of the first turn: the assistant message of the tool call carries
    // its reasoning back (tools are offered).
    let assistant = &requests[1]["messages"][1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["reasoning_content"], "I should call echo");
    assert!(assistant["tool_calls"].is_array());
    // The next turn replays the transcript: the reasoning is still there.
    let replay = requests[2]["messages"].as_array().unwrap();
    let with_reasoning: Vec<&Value> = replay
        .iter()
        .filter(|m| m.get("reasoning_content").is_some())
        .collect();
    assert!(
        with_reasoning
            .iter()
            .any(|m| m["reasoning_content"] == "I should call echo"),
        "{replay:?}"
    );
    // The transcript itself keeps it.
    let events = turn(&*session, "first").await;
    let AgentEvent::Done {
        provider_session_id: Some(id),
        ..
    } = done(&events)
    else {
        panic!("done carries the transcript id");
    };
    let saved = h.store.load(id).unwrap().unwrap();
    assert!(
        saved
            .iter()
            .any(|m| m.reasoning.as_deref() == Some("I should call echo"))
    );
}

#[tokio::test]
async fn compaction_summarises_the_old_history_and_keeps_the_reasoning_of_the_recent_messages() {
    let h = Harness::with(
        vec![
            tool_reply(
                Some("go"),
                &[echo_call()],
                Some("old reasoning"),
                Some((100, 10)),
            ),
            tool_reply(
                Some(TOOL),
                &[("c2", "mcp__fake__echo", json!({"text": "two"}))],
                Some("kept reasoning"),
                Some((10_000, 10)),
            ),
            text_reply(
                Some("compacting the history"),
                "THE SUMMARY",
                None,
                Some((50, 20)),
            ),
            text_reply(Some(TOOL), "done", None, Some((300, 10))),
        ],
        |config| {
            config.context_window = Some(12_000);
            config.compaction.keep_recent = 2;
        },
    )
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let session = h.open(h.spec_with_mcp(&log_path(&log_dir))).await;
    let events = turn(&*session, "go").await;
    assert_eq!(stop_reason(&events), StopReason::Completed);

    let phases: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Compaction { phase, trigger, .. } => Some(format!("{phase:?}/{trigger:?}")),
            _ => None,
        })
        .collect();
    assert_eq!(phases, ["Started/Some(Auto)", "Completed/Some(Auto)"]);

    let requests = h.chat();
    assert_eq!(requests.len(), 4);
    // The summarisation call: the fixed prompt, no tools.
    let summary = &requests[2];
    assert!(
        summary["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("compacting the history of an agent session")
    );
    assert!(summary.get("tools").is_none());
    // The request after it: summary first, then the two recent messages, intact.
    let after = requests[3]["messages"].as_array().unwrap();
    assert!(
        after[0]["content"]
            .as_str()
            .unwrap()
            .contains("THE SUMMARY")
    );
    assert_eq!(after.len(), 3, "{after:?}");
    assert_eq!(after[1]["role"], "assistant");
    assert_eq!(after[1]["reasoning_content"], "kept reasoning");
    assert_eq!(after[2]["role"], "tool");
    // The old reasoning went into the summary, not back as a message.
    assert!(!requests[3].to_string().contains("old reasoning"));
}

#[tokio::test]
async fn an_unknown_window_never_compacts_by_itself() {
    // No configured window, and the catalogue answers none.
    let server = FakeOpenAi::start(json!([
        probe_route(false),
        json!({"method": "GET", "path": "/v1/models", "status": 200, "body": {"data": [{"id": "m"}]}}),
        text_reply(None, "ok", None, Some((900_000, 5))),
    ]));
    let mut config = NativeConfig::new("native-test");
    config.default_model = Some("m".to_owned());
    let provider = NativeProvider::new(
        config,
        endpoint(server.base_url(), EndpointQuirks::generic()),
    );
    let caps = provider.refresh_capabilities("m").await.unwrap();
    assert!(caps.context_window.is_none());
    let dir = tempfile::tempdir().unwrap();
    let mut spec = SessionSpec::new(dir.path());
    spec.model = Some("m".to_owned());
    let session = provider.open(spec).await.unwrap();
    let first = turn(&*session, "a").await;
    let second = turn(&*session, "b").await;
    for events in [&first, &second] {
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Compaction { .. }))
        );
    }
}

// ---------------------------------------------------------------------------
// Capabilities, honestly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn capabilities_say_what_was_probed_and_nothing_more() {
    let h = Harness::with(vec![text_reply(None, "x", None, None)], |config| {
        config.prices = PriceTable::new().with("m", price(1.0));
    })
    .await;
    // Before the probe: nothing is claimed about the model.
    let before = h.provider.capabilities(Some("m"));
    assert!(!before.tools && !before.thinking && before.context_window.is_none());
    assert!(before.resume && before.tool_cancel && before.interactive_permissions);
    assert_eq!(
        before.permission_scopes,
        vec![PermissionScope::Once, PermissionScope::Session]
    );
    let after = h.provider.refresh_capabilities("m").await.unwrap();
    assert!(after.tools && after.thinking);
    let window = after.context_window.unwrap();
    assert_eq!(window.value, 128_000);
    assert_eq!(format!("{:?}", window.source), "Probed");
    assert_eq!(after.cost, CostBasis::Priced);
    // What the native harness has not: stated as absent.
    assert!(!after.images && !after.native_question && !after.background_tasks);
    assert_eq!(format!("{:?}", after.subagents), "None");
    assert_eq!(format!("{:?}", after.sandbox), "None");
    assert!(after.secret_isolation && after.per_session_mcp);
    // What it has since the hooks run in its loop.
    assert_eq!(after.hooks, HookSupport::InProtocol);
}

#[tokio::test]
async fn a_configured_window_wins_and_free_or_unpriced_costs_are_not_invented() {
    let free = Harness::with(vec![], |config| {
        config.context_window = Some(4096);
        config.cost_basis = CostBasis::Free;
    })
    .await;
    let caps = free.provider.refresh_capabilities("m").await.unwrap();
    assert_eq!(caps.context_window.unwrap().value, 4096);
    assert_eq!(
        format!("{:?}", caps.context_window.unwrap().source),
        "Configured"
    );
    assert_eq!(caps.cost, CostBasis::Free);
    let unpriced = Harness::new(vec![]).await;
    let caps = unpriced.provider.refresh_capabilities("m").await.unwrap();
    assert_eq!(caps.cost, CostBasis::Unknown);
}

#[tokio::test]
async fn a_model_without_tools_refuses_mcp_servers_and_still_runs_plain_turns() {
    let server = FakeOpenAi::start(json!([
        text_reply(
            Some("Call the ping tool now"),
            "I cannot call tools",
            None,
            None
        ),
        models_route(8192),
        text_reply(None, "plain answer", None, Some((10, 5))),
    ]));
    let mut config = NativeConfig::new("native-test");
    config.default_model = Some("m".to_owned());
    let provider = NativeProvider::new(
        config,
        endpoint(server.base_url(), EndpointQuirks::generic()),
    );
    let caps = provider.refresh_capabilities("m").await.unwrap();
    assert!(!caps.tools);
    let dir = tempfile::tempdir().unwrap();
    let mut with_mcp = SessionSpec::new(dir.path());
    with_mcp.model = Some("m".to_owned());
    with_mcp.mcp_servers.insert("fake".into(), stdio_mcp(None));
    let refused = provider.open(with_mcp).await.err().expect("refused");
    assert_eq!(refused, ProviderError::ModelNoTools { model: "m".into() });
    let mut plain = SessionSpec::new(dir.path());
    plain.model = Some("m".to_owned());
    let session = provider.open(plain).await.unwrap();
    assert_eq!(
        stop_reason(&turn(&*session, "hi").await),
        StopReason::Completed
    );
    // No tool was offered to a model that cannot call any.
    let chat: Vec<Value> = server
        .requests_to("POST", "/v1/chat/completions")
        .into_iter()
        .map(|r| r["body"].clone())
        .collect();
    assert!(chat.last().unwrap().get("tools").is_none());
}

/// A reasoning model that runs out of budget on every probe attempt is not
/// recorded as "no tools": the probe is inconclusive, the session that needs tools
/// is refused for this opening only, and the next probe finds the tools.
#[tokio::test]
async fn an_inconclusive_probe_records_nothing_and_the_next_one_finds_the_tools() {
    let truncated = || {
        json!({"method": "POST", "path": "/v1/chat/completions", "status": 200,
            "body_contains": "Call the ping tool now",
            "sse": [delta(json!({"reasoning_content": "thinking"})), finish("length"), "[DONE]"]})
    };
    let server = FakeOpenAi::start(json!([
        truncated(),
        truncated(),
        truncated(),
        truncated(),
        probe_route(true),
        models_route(8192),
    ]));
    let mut config = NativeConfig::new("native-test");
    config.default_model = Some("m".to_owned());
    let provider = NativeProvider::new(
        config,
        endpoint(server.base_url(), EndpointQuirks::deepseek()),
    );
    let error = provider.refresh_capabilities("m").await.unwrap_err();
    assert!(
        matches!(&error, ProviderError::Protocol { detail } if detail.contains("inconclusive")),
        "{error:?}"
    );
    assert!(!provider.capabilities(Some("m")).tools, "nothing claimed");
    // A session that needs tools: refused for this opening, not as `ModelNoTools`.
    let dir = tempfile::tempdir().unwrap();
    let mut with_mcp = SessionSpec::new(dir.path());
    with_mcp.model = Some("m".to_owned());
    with_mcp.mcp_servers.insert("fake".into(), stdio_mcp(None));
    let refused = provider.open(with_mcp).await.err().expect("refused");
    assert!(
        matches!(&refused, ProviderError::Protocol { .. }),
        "{refused:?}"
    );
    // Nothing was recorded: the next probe asks again and finds the tools.
    let caps = provider.refresh_capabilities("m").await.unwrap();
    assert!(caps.tools);
    assert_eq!(server.requests_to("POST", "/v1/chat/completions").len(), 5);
}

// ---------------------------------------------------------------------------
// Budgets (A21)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_token_budget_works_without_any_price_and_stops_before_the_tools_run() {
    let h = Harness::new(vec![
        tool_reply(Some("spend"), &[echo_call()], None, Some((100, 60))),
        text_reply(None, "never reached", None, None),
    ])
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_path(&log_dir);
    let mut spec = h.spec_with_mcp(&log);
    spec.limits.max_tokens = Some(150);
    let session = h.open(spec).await;
    let events = turn(&*session, "spend").await;
    assert_eq!(stop_reason(&events), StopReason::BudgetExceeded);
    let AgentEvent::Done { cost, usage, .. } = done(&events) else {
        unreachable!()
    };
    // No price: no amount, and never a zero.
    assert_eq!(cost.usd, None);
    assert_eq!(cost.basis, CostBasis::Unknown);
    assert_eq!(usage.input_tokens, Some(100));
    assert_eq!(usage.output_tokens, Some(60));
    // The tools would have run past the budget: they did not.
    assert_eq!(mcp_calls(&log, "echo"), 0);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCall { .. }))
    );
    assert_eq!(h.chat().len(), 1);
    // Said as such, with the figures and the line the model reads at its next turn.
    let notice = budget_notice(&events);
    assert_eq!(notice["budget"], "tokens");
    assert_eq!(notice["limit"], 150);
    assert_eq!(notice["spent"], 160);
    assert_eq!(notice["requests_this_turn"], 1);
    let note = notice["next_turn_note"].as_str().unwrap();
    assert!(
        note.starts_with("[system] Your previous turn was stopped because the session token budget (150 tokens, cache reads not counted) was reached."),
        "{note}"
    );
    assert!(note.contains("not interrupted by the user"), "{note}");
    // The session budget is spent: the next turn does not even call the model (the
    // conversation is stopped until the budget is raised or the session resumed, which
    // counts anew) — and it says so, every time.
    let again = turn(&*session, "again").await;
    assert_eq!(stop_reason(&again), StopReason::BudgetExceeded);
    assert_eq!(h.chat().len(), 1);
    let refused = budget_notice(&again);
    assert_eq!(refused["requests_this_turn"], 0);
    assert!(
        refused["next_turn_note"]
            .as_str()
            .unwrap()
            .contains("did not run"),
        "{refused}"
    );
}

/// The `session_budget_reached` notice of a turn.
fn budget_notice(events: &[AgentEvent]) -> &Value {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ProviderNotice { kind, data } if kind == "session_budget_reached" => {
                Some(data)
            },
            _ => None,
        })
        .unwrap_or_else(|| panic!("a session_budget_reached notice: {events:?}"))
}

/// A final answer whose usage says `cached` of the `prompt` tokens came from the cache
/// (DeepSeek's field).
fn cached_reply(body_contains: &str, prompt: u64, cached: u64) -> Value {
    json!({"method": "POST", "path": "/v1/chat/completions", "status": 200,
        "body_contains": body_contains, "sse": [
        delta(json!({"content": "ok"})),
        finish("stop"),
        json!({"choices": [], "usage": {"prompt_tokens": prompt, "completion_tokens": 50,
            "total_tokens": prompt + 50, "prompt_cache_hit_tokens": cached}}),
        json!("[DONE]"),
    ]})
}

/// ISO5: on a long DeepSeek conversation, 96 % of the tokens were cache reads; counted at
/// full weight they spent a 10 M budget and stopped the conversation. They do not count.
#[tokio::test]
async fn cache_reads_do_not_spend_the_token_budget() {
    let h = Harness::with(
        vec![
            cached_reply("first", 6_000_000, 5_995_000),
            cached_reply("second", 6_000_000, 5_995_000),
            cached_reply("third", 6_000_000, 5_995_000),
        ],
        |config| config.limits.max_tokens = Some(1_000_000),
    )
    .await;
    let session = h.open(h.spec()).await;
    for text in ["first", "second", "third"] {
        let events = turn(&*session, text).await;
        assert_eq!(stop_reason(&events), StopReason::Completed, "{events:?}");
        let AgentEvent::Done { usage, .. } = done(&events) else {
            unreachable!()
        };
        // Reported as they are: the cache reads are in the usage, just not in the budget.
        assert_eq!(usage.cache_read_tokens, Some(5_995_000));
    }
    // 17.985 M tokens read from the cache, 15 150 counted: the budget of 1 M is far.
    assert_eq!(h.chat().len(), 3);
}

/// Without a configured budget, nothing stops a conversation however much it reads.
#[tokio::test]
async fn without_a_configured_budget_ten_million_cache_reads_stop_nothing() {
    let h = Harness::new(vec![
        cached_reply("first", 12_000_000, 11_000_000),
        cached_reply("second", 12_000_000, 11_000_000),
    ])
    .await;
    let session = h.open(h.spec()).await;
    for text in ["first", "second"] {
        let events = turn(&*session, text).await;
        assert_eq!(stop_reason(&events), StopReason::Completed, "{events:?}");
    }
    assert_eq!(h.chat().len(), 2);
}

#[tokio::test]
async fn a_usd_budget_with_a_price_is_enforced_and_the_cost_reported() {
    let h = Harness::with(
        vec![tool_reply(
            Some("spend"),
            &[echo_call()],
            None,
            Some((100, 60)),
        )],
        |config| config.prices = PriceTable::new().with("m", price(1000.0)),
    )
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let mut spec = h.spec_with_mcp(&log_path(&log_dir));
    spec.limits.max_cost_usd = Some(0.1);
    let session = h.open(spec).await;
    let events = turn(&*session, "spend").await;
    assert_eq!(stop_reason(&events), StopReason::BudgetExceeded);
    let AgentEvent::Done { cost, .. } = done(&events) else {
        unreachable!()
    };
    // 160 tokens at 1000 USD per million tokens.
    assert!((cost.usd.unwrap() - 0.16).abs() < 1e-9, "{cost:?}");
    assert_eq!(cost.basis, CostBasis::Priced);
}

#[tokio::test]
async fn a_usd_budget_without_a_price_is_refused_at_opening() {
    let h = Harness::new(vec![]).await;
    let mut spec = h.spec();
    spec.limits.max_cost_usd = Some(1.0);
    let refused = h.provider.open(spec).await.err().expect("refused");
    assert_eq!(refused, ProviderError::unsupported("cost"));
    // A free endpoint can honour it: it costs nothing.
    let free = Harness::with(vec![], |config| config.cost_basis = CostBasis::Free).await;
    let mut spec = free.spec();
    spec.limits.max_cost_usd = Some(1.0);
    assert!(free.provider.open(spec).await.is_ok());
}

#[tokio::test]
async fn without_a_price_the_cost_of_a_turn_is_unknown_not_zero() {
    let h = Harness::new(vec![text_reply(None, "ok", None, Some((100, 10)))]).await;
    let session = h.open(h.spec()).await;
    let events = turn(&*session, "hi").await;
    let AgentEvent::Done { cost, usage, .. } = done(&events) else {
        unreachable!()
    };
    assert_eq!((cost.usd, cost.basis), (None, CostBasis::Unknown));
    assert_eq!(usage.by_model[0].cost_usd, None);
    assert_eq!(usage.by_model[0].model, "m");
}

#[tokio::test]
async fn a_usd_budget_advances_on_an_estimate_when_the_endpoint_reports_no_usage() {
    // The endpoint never says what a request cost; the model has a price.
    let h = Harness::with(
        vec![tool_reply(Some("spend"), &[echo_call()], None, None)],
        |config| config.prices = PriceTable::new().with("m", price(1000.0)),
    )
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_path(&log_dir);
    let mut spec = h.spec_with_mcp(&log);
    spec.limits.max_cost_usd = Some(0.1);
    let session = h.open(spec).await;
    // About 4000 characters, so about 1000 prompt tokens: 1 USD at this price.
    let prompt = format!("spend {}", "x".repeat(4000));
    let events = turn(&*session, &prompt).await;
    assert_eq!(stop_reason(&events), StopReason::BudgetExceeded);
    // The tools would have run past the budget: they did not.
    assert_eq!(mcp_calls(&log, "echo"), 0);
    // The estimate is announced as such, with its basis.
    let notice = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ProviderNotice { kind, data } if kind == "usage_estimated" => Some(data),
            _ => None,
        })
        .expect("a usage_estimated notice");
    assert_eq!(notice["estimated"], true);
    assert_eq!(notice["chars_per_token"], 4);
    assert!(notice["tokens"].as_u64().unwrap() >= 1000, "{notice}");
    assert!(notice["usd"].as_f64().unwrap() >= 1.0, "{notice}");
    // The session budget is spent: the next turn does not call the model.
    let again = turn(&*session, "again").await;
    assert_eq!(stop_reason(&again), StopReason::BudgetExceeded);
    assert_eq!(h.chat().len(), 1);
}

#[tokio::test]
async fn a_report_from_the_endpoint_replaces_the_estimate_and_no_notice_is_made() {
    let h = Harness::with(
        vec![text_reply(None, "ok", None, Some((100, 10)))],
        |config| config.prices = PriceTable::new().with("m", price(1000.0)),
    )
    .await;
    let session = h.open(h.spec()).await;
    let events = turn(&*session, "hi").await;
    assert!(
        !events.iter().any(|event| matches!(
            event,
            AgentEvent::ProviderNotice { kind, .. } if kind == "usage_estimated"
        )),
        "{events:?}"
    );
}

#[test]
fn a_native_config_bounds_round_trips_but_sets_no_time_limit_and_no_budget() {
    let config = NativeConfig::new("x");
    assert_eq!(config.max_turns, Some(NativeConfig::DEFAULT_MAX_TURNS));
    // No wall-clock cap on a turn and no session budget (Claude Code has neither, ISO5):
    // the round trips, the interrupt and the endpoint's idle deadline bound a turn.
    assert_eq!(config.limits, SessionLimits::default());
    assert_eq!(config.limits.turn_timeout_ms, None);
    assert_eq!(config.limits.max_tokens, None);
    assert_eq!(config.limits.max_cost_usd, None);
    const {
        assert!(NativeConfig::DEFAULT_MAX_TURNS > 0);
    }
}

#[tokio::test]
async fn a_session_without_its_own_max_turns_gets_the_one_of_the_config() {
    let h = Harness::with(
        vec![
            tool_reply(Some("loop"), &[echo_call()], None, Some((10, 1))),
            tool_reply(Some(TOOL), &[echo_call()], None, Some((10, 1))),
        ],
        |config| config.max_turns = Some(2),
    )
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    // Neither `max_turns` nor any limit in the spec.
    let session = h.open(h.spec_with_mcp(&log_path(&log_dir))).await;
    let events = turn(&*session, "loop").await;
    assert_eq!(stop_reason(&events), StopReason::MaxTurns);
    let AgentEvent::Done { num_turns, .. } = done(&events) else {
        unreachable!()
    };
    assert_eq!(*num_turns, 2);
}

#[tokio::test]
async fn the_spec_limits_win_over_the_defaults_of_the_config() {
    let h = Harness::with(
        vec![
            tool_reply(Some("loop"), &[echo_call()], None, Some((10, 1))),
            tool_reply(Some(TOOL), &[echo_call()], None, Some((10, 1))),
            text_reply(Some("never"), "x", None, None),
        ],
        |config| config.max_turns = Some(1),
    )
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let mut spec = h.spec_with_mcp(&log_path(&log_dir));
    spec.max_turns = Some(3);
    let session = h.open(spec).await;
    let events = turn(&*session, "loop").await;
    let AgentEvent::Done { num_turns, .. } = done(&events) else {
        unreachable!()
    };
    assert_eq!(*num_turns, 3);
}

#[tokio::test]
async fn max_turns_bounds_the_round_trips_of_a_turn() {
    let h = Harness::new(vec![
        tool_reply(Some("loop"), &[echo_call()], None, Some((10, 1))),
        tool_reply(Some(TOOL), &[echo_call()], None, Some((10, 1))),
    ])
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let mut spec = h.spec_with_mcp(&log_path(&log_dir));
    spec.max_turns = Some(2);
    let session = h.open(spec).await;
    let events = turn(&*session, "loop").await;
    assert_eq!(stop_reason(&events), StopReason::MaxTurns);
    let AgentEvent::Done { num_turns, .. } = done(&events) else {
        unreachable!()
    };
    assert_eq!(*num_turns, 2);
}

// ---------------------------------------------------------------------------
// Policy (A8, A35)
// ---------------------------------------------------------------------------

fn write_call() -> (&'static str, &'static str, Value) {
    ("w1", "mcp__fake__write", json!({"text": "data"}))
}

#[tokio::test]
async fn ask_mode_asks_before_a_tool_that_is_not_read_only_and_runs_it_once_allowed() {
    let h = Harness::new(vec![
        tool_reply(Some("write"), &[write_call(), echo_call()], None, None),
        text_reply(Some(TOOL), "done", None, None),
    ])
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_path(&log_dir);
    let session = h.open(h.spec_with_mcp(&log)).await;
    let events = turn_deciding(&*session, "write", |_| PermissionDecision::allow_once()).await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    let asks: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::PermissionAsk { .. }))
        .collect();
    // `write` asks; `echo` is read-only and does not.
    assert_eq!(asks.len(), 1);
    let AgentEvent::PermissionAsk {
        tool_name,
        tool_call_id,
        scopes,
        input,
        ..
    } = asks[0]
    else {
        unreachable!()
    };
    assert_eq!(tool_name, "mcp__fake__write");
    assert_eq!(tool_call_id.as_deref(), Some("w1"));
    assert_eq!(scopes, &[PermissionScope::Once, PermissionScope::Session]);
    assert_eq!(input["text"], "data");
    assert_eq!(
        mcp_calls(&log, "write"),
        1,
        "{:?}",
        std::fs::read_to_string(&log)
    );
    assert_eq!(mcp_calls(&log, "echo"), 1);
}

#[tokio::test]
async fn a_denied_tool_does_not_run_and_the_model_is_told() {
    let h = Harness::new(vec![
        tool_reply(Some("write"), &[write_call()], None, None),
        text_reply(Some(TOOL), "understood", None, None),
    ])
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_path(&log_dir);
    let session = h.open(h.spec_with_mcp(&log)).await;
    let events = turn_deciding(&*session, "write", |_| PermissionDecision::Deny {
        message: Some("not on my watch".into()),
        interrupt: false,
    })
    .await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    assert_eq!(mcp_calls(&log, "write"), 0);
    let results = tool_results(&events);
    assert_eq!(results, vec![("w1".into(), true, "not on my watch".into())]);
    // The refusal went back to the model as the tool message.
    let second = &h.chat()[1];
    assert!(second.to_string().contains("not on my watch"));
}

#[tokio::test]
async fn a_session_scoped_approval_is_not_asked_again() {
    let h = Harness::new(vec![
        tool_reply(Some("one"), &[write_call()], None, None),
        text_reply(Some(TOOL), "done one", None, None),
        tool_reply(
            Some("two"),
            &[("w2", "mcp__fake__write", json!({}))],
            None,
            None,
        ),
        text_reply(Some("done one"), "done two", None, None),
    ])
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_path(&log_dir);
    let session = h.open(h.spec_with_mcp(&log)).await;
    let decide = |_: &str| PermissionDecision::Allow {
        scope: PermissionScope::Session,
        updated_input: None,
    };
    let first = turn_deciding(&*session, "one", decide).await;
    assert_eq!(
        first
            .iter()
            .filter(|e| matches!(e, AgentEvent::PermissionAsk { .. }))
            .count(),
        1
    );
    let second = turn_deciding(&*session, "two", decide).await;
    assert_eq!(
        second
            .iter()
            .filter(|e| matches!(e, AgentEvent::PermissionAsk { .. }))
            .count(),
        0
    );
    assert_eq!(mcp_calls(&log, "write"), 2);
    // `always` has nowhere to be kept: refused by name.
    let h2 = Harness::new(vec![
        tool_reply(Some("x"), &[write_call()], None, None),
        text_reply(Some(TOOL), "end", None, None),
    ])
    .await;
    let log2 = tempfile::tempdir().unwrap();
    let session = h2.open(h2.spec_with_mcp(&log_path(&log2))).await;
    let mut stream = session.send_turn(TurnInput::text("x")).await.unwrap();
    while let Some(event) = stream.next().await {
        if let AgentEvent::PermissionAsk { request_id, .. } = event {
            let always = PermissionDecision::Allow {
                scope: PermissionScope::Always,
                updated_input: None,
            };
            let refused = session
                .answer_permission(&request_id, always)
                .await
                .unwrap_err();
            assert_eq!(refused, ProviderError::unsupported("permission_scope"));
            session
                .answer_permission(&request_id, PermissionDecision::deny())
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn deny_patterns_and_allow_lists_decide_what_the_model_is_offered() {
    let h = Harness::new(vec![text_reply(None, "ok", None, None)]).await;
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_path(&log_dir);
    let offered = |policy: ToolPolicy| {
        let h = &h;
        let log = &log;
        async move {
            let mut spec = h.spec_with_mcp(log);
            spec.policy = policy;
            let session = h.open(spec).await;
            turn(&*session, "list").await;
            let last = h.chat().last().unwrap().clone();
            offered_tools(&last)
        }
    };
    let all = offered(ToolPolicy::new(PolicyMode::Ask)).await;
    assert_eq!(all.len(), 10, "{all:?}");
    // deny wins and hides the tool.
    let denied = offered(
        ToolPolicy::from_patterns(
            PolicyMode::Ask,
            &[] as &[&str],
            &["mcp__fake__write", "mcp__fake__d*"],
        )
        .unwrap(),
    )
    .await;
    assert!(!denied.contains(&"mcp__fake__write".to_owned()));
    assert!(!denied.contains(&"mcp__fake__die".to_owned()));
    assert_eq!(denied.len(), 8);
    // An allow list is an exposure list.
    let allowed = offered(
        ToolPolicy::from_patterns(PolicyMode::Ask, &["mcp__fake__echo"], &[] as &[&str]).unwrap(),
    )
    .await;
    assert_eq!(allowed, vec!["mcp__fake__echo".to_owned()]);
}

#[tokio::test]
async fn a_tool_that_was_not_offered_is_refused_without_reaching_the_server() {
    let h = Harness::new(vec![
        tool_reply(Some("sneak"), &[write_call()], None, None),
        text_reply(Some(TOOL), "ok", None, None),
    ])
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_path(&log_dir);
    let mut spec = h.spec_with_mcp(&log);
    spec.policy =
        ToolPolicy::from_patterns(PolicyMode::Ask, &[] as &[&str], &["mcp__fake__write"]).unwrap();
    let session = h.open(spec).await;
    let events = turn(&*session, "sneak").await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::PermissionAsk { .. }))
    );
    let results = tool_results(&events);
    assert!(
        results[0].1 && results[0].2.contains("not available"),
        "{results:?}"
    );
    assert_eq!(mcp_calls(&log, "write"), 0);
}

#[tokio::test]
async fn plan_only_offers_read_only_tools_and_refuses_the_others() {
    let h = Harness::new(vec![
        tool_reply(Some("plan"), &[write_call(), echo_call()], None, None),
        text_reply(Some(TOOL), "planned", None, None),
    ])
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_path(&log_dir);
    let mut spec = h.spec_with_mcp(&log);
    spec.policy = ToolPolicy::new(PolicyMode::PlanOnly);
    // An allow entry cannot lift plan mode.
    spec.policy.allow.push(ToolPattern::tool("mcp__fake__*"));
    let session = h.open(spec).await;
    let events = turn(&*session, "plan").await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    let offered = offered_tools(&h.chat()[0]);
    for read_only in ["echo", "readonly", "slow", "env", "argv", "big"] {
        assert!(
            offered.contains(&format!("mcp__fake__{read_only}")),
            "{offered:?}"
        );
    }
    for writes in ["write", "fail", "die"] {
        assert!(
            !offered.contains(&format!("mcp__fake__{writes}")),
            "{offered:?}"
        );
    }
    let results = tool_results(&events);
    let by_id = |id: &str| results.iter().find(|r| r.0 == id).unwrap().clone();
    assert!(by_id("w1").1, "write is refused in plan mode");
    assert!(!by_id("c1").1, "the read-only echo runs");
    assert_eq!(mcp_calls(&log, "write"), 0);
    assert_eq!(mcp_calls(&log, "echo"), 1);
}

#[tokio::test]
async fn trust_mode_opens_and_switches_live_whatever_the_sandbox() {
    // Decision of 2026-10-07: a third-party provider behaves like Claude Code. The sandbox
    // level is information for the user, never a gate on a mode.
    let h = Harness::new(vec![]).await;
    let mut spec = h.spec();
    spec.policy = ToolPolicy::new(PolicyMode::Trust);
    let trusted = h.provider.open(spec).await.expect("trust opens");
    assert!(trusted.capabilities().sandbox == nexus_claude::agent::SandboxLevel::None);
    trusted.close().await.unwrap();

    let session = h.open(h.spec()).await;
    session
        .set_policy_mode(PolicyMode::Trust, None)
        .await
        .expect("trust is applied live");
    session
        .set_policy_mode(PolicyMode::AutoEdits, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn in_trust_mode_a_writing_tool_runs_without_asking() {
    let h = Harness::new(vec![
        tool_reply(Some("go"), &[write_call()], None, None),
        text_reply(Some(TOOL), "done", None, None),
    ])
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_path(&log_dir);
    let mut spec = h.spec_with_mcp(&log);
    spec.policy = ToolPolicy::new(PolicyMode::Trust);
    let session = h.open(spec).await;
    let events = turn(&*session, "go").await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::PermissionAsk { .. })),
        "the user is never asked in trust mode"
    );
    assert_eq!(mcp_calls(&log, "write"), 1, "the write ran");
}

#[tokio::test]
async fn the_same_writing_tool_asks_in_ask_mode() {
    // The control of the test above: without it, "never asked" could be an accident of the
    // scripted turn.
    let h = Harness::new(vec![
        tool_reply(Some("go"), &[write_call()], None, None),
        text_reply(Some(TOOL), "done", None, None),
    ])
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_path(&log_dir);
    let mut spec = h.spec_with_mcp(&log);
    spec.policy = ToolPolicy::new(PolicyMode::Ask);
    let session = h.open(spec).await;
    let events = turn_deciding(&*session, "go", |_| PermissionDecision::allow_once()).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::PermissionAsk { .. })),
        "{events:?}"
    );
}

// ---------------------------------------------------------------------------
// Resume
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resume_reloads_the_transcript_by_id() {
    let h = Harness::new(vec![
        text_reply(Some("my number is 42"), "noted", Some("remember"), None),
        text_reply(Some("what was it"), "42", None, None),
    ])
    .await;
    let session = h.open(h.spec()).await;
    turn(&*session, "my number is 42").await;
    let token = session.resume_token().expect("a token");
    assert_eq!(token.kind(), ProviderKind::Native);
    let id = token.data()["transcript_id"].as_str().unwrap().to_owned();
    assert!(h.store.load(&id).unwrap().is_some());
    session.close().await.unwrap();

    let resumed = h.provider.resume(h.spec(), token.clone()).await.unwrap();
    assert_eq!(
        resumed.resume_token().unwrap().data()["transcript_id"],
        id.as_str()
    );
    turn(&*resumed, "what was it").await;
    let replay = h.chat().last().unwrap()["messages"].to_string();
    assert!(
        replay.contains("my number is 42") && replay.contains("noted"),
        "{replay}"
    );
    // Wire form round trip, as the backend persists it.
    let again = ResumeToken::from_wire(&token.to_wire()).unwrap();
    assert!(h.provider.resume(h.spec(), again).await.is_ok());
}

#[tokio::test]
async fn resume_refuses_an_unknown_id_and_a_foreign_token() {
    let h = Harness::new(vec![]).await;
    for token in [
        ResumeToken::new(ProviderKind::Native, 1, json!({"transcript_id": "nope"})),
        ResumeToken::new(ProviderKind::Native, 1, json!({})),
        ResumeToken::new(
            ProviderKind::Native,
            1,
            json!({"transcript_id": "../etc/passwd"}),
        ),
        ResumeToken::claude_code_session("abc"),
    ] {
        let refused = h
            .provider
            .resume(h.spec(), token)
            .await
            .err()
            .expect("refused");
        assert_eq!(refused.kind(), "invalid_request", "{refused}");
    }
}

#[tokio::test]
async fn a_file_transcript_survives_a_new_provider_and_holds_no_credential() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn TranscriptStore> = Arc::new(FileTranscriptStore::new(dir.path().join("t")));
    let routes = || {
        vec![
            probe_route(true),
            models_route(8192),
            text_reply(None, "ok", Some("quiet thought"), None),
        ]
    };
    let server = FakeOpenAi::start(json!(routes()));
    let mut config = NativeConfig::new("native-test");
    config.default_model = Some("m".to_owned());
    let first = NativeProvider::new(
        config.clone(),
        endpoint(server.base_url(), EndpointQuirks::deepseek()),
    )
    .with_transcript_store(store.clone());
    let cwd = tempfile::tempdir().unwrap();
    let mut spec = SessionSpec::new(cwd.path());
    spec.model = Some("m".to_owned());
    let session = first.open(spec.clone()).await.unwrap();
    turn(
        &*session,
        "use header Authorization: Bearer tok_SECRETSECRET1234 please",
    )
    .await;
    let token = session.resume_token().unwrap();
    let id = token.data()["transcript_id"].as_str().unwrap().to_owned();
    let file = dir.path().join("t").join(format!("{id}.json"));
    let raw = std::fs::read_to_string(&file).unwrap();
    assert!(
        !raw.contains("SECRETSECRET1234"),
        "a credential was persisted: {raw}"
    );
    assert!(raw.contains("quiet thought"), "the reasoning is persisted");

    // A new provider instance (a new process, in effect) resumes from the file.
    let second = NativeProvider::new(
        config,
        endpoint(server.base_url(), EndpointQuirks::deepseek()),
    )
    .with_transcript_store(Arc::new(FileTranscriptStore::new(dir.path().join("t"))));
    assert!(second.resume(spec, token).await.is_ok());
}

// ---------------------------------------------------------------------------
// One turn at a time, interruption, cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_turn_is_refused_while_one_runs_and_accepted_after_its_end() {
    let h = Harness::new(vec![
        busy_reply(Some("busy")),
        text_reply(Some("next"), "fine", None, None),
    ])
    .await;
    let session = h.open(h.spec()).await;
    let mut first = session.send_turn(TurnInput::text("busy")).await.unwrap();
    let refused = session
        .send_turn(TurnInput::text("next"))
        .await
        .err()
        .expect("refused");
    assert_eq!(refused, ProviderError::TurnInProgress);
    // Reading starts after the refusal: the turn was accepted all along.
    assert!(matches!(first.next().await, Some(AgentEvent::Delta { .. })));
    session.interrupt(InterruptScope::TurnOnly).await.unwrap();
    let rest = collect(first).await;
    assert_eq!(stop_reason(&rest), StopReason::Interrupted);
    assert_eq!(
        stop_reason(&turn(&*session, "next").await),
        StopReason::Completed
    );
}

#[tokio::test]
async fn an_interruption_in_the_middle_of_the_stream_ends_the_turn_promptly_and_keeps_what_was_said()
 {
    let h = Harness::new(vec![
        busy_reply(Some("busy")),
        text_reply(Some("again"), "ok", None, None),
    ])
    .await;
    let session = h.open(h.spec()).await;
    let mut stream = session.send_turn(TurnInput::text("busy")).await.unwrap();
    assert!(matches!(
        stream.next().await,
        Some(AgentEvent::Delta { .. })
    ));
    let started = Instant::now();
    let outcome = session
        .interrupt(InterruptScope::TurnAndTools)
        .await
        .unwrap();
    assert!(outcome.turn_interrupted);
    let rest = collect(stream).await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the stream was not cut"
    );
    assert_eq!(stop_reason(&rest), StopReason::Interrupted);
    // What the model had said is in the transcript and in the events.
    assert!(
        rest.iter()
            .any(|e| matches!(e, AgentEvent::Text { text, .. } if text == "start"))
    );
    turn(&*session, "again").await;
    assert!(
        h.chat().last().unwrap()["messages"]
            .to_string()
            .contains("start")
    );
}

async fn wait_for_events(log: &std::path::Path, name: &str, count: usize) {
    let wait = async {
        while log_events(log)
            .iter()
            .filter(|e| e["event"] == name)
            .count()
            < count
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .unwrap_or_else(|_| panic!("the MCP log never showed {count} `{name}`"));
}

#[tokio::test]
async fn cancel_tools_runs_parallel_calls_together_stops_them_and_the_model_hears_of_it() {
    let slow = |id: &'static str| (id, "mcp__fake__slow", json!({}));
    let h = Harness::new(vec![
        tool_reply(Some("two slow"), &[slow("s1"), slow("s2")], None, None),
        text_reply(Some(TOOL), "I carry on", None, None),
    ])
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_path(&log_dir);
    let session = h.open(h.spec_with_mcp(&log)).await;
    let stream = session
        .send_turn(TurnInput::text("two slow"))
        .await
        .unwrap();
    // Both started before either ends: the calls ran concurrently.
    wait_for_events(&log, "slow_started", 2).await;
    let outcome = session.cancel_tools(CancelScope::All).await.unwrap();
    assert_eq!(outcome.tools_cancelled, 2);
    let events = collect(stream).await;
    assert_eq!(
        stop_reason(&events),
        StopReason::Completed,
        "the turn was preserved"
    );
    let results = tool_results(&events);
    assert_eq!(results.len(), 2);
    assert!(
        results.iter().all(|r| r.1 && r.2.contains("cancelled")),
        "{results:?}"
    );
    // The server was told to stop, not just abandoned.
    wait_for_events(&log, "slow_cancelled", 2).await;
    // The model saw both results, in call order.
    let second = h.chat()[1]["messages"].clone();
    let tool_messages: Vec<&Value> = second
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(tool_messages.len(), 2);
    assert_eq!(tool_messages[0]["tool_call_id"], "s1");
    assert_eq!(tool_messages[1]["tool_call_id"], "s2");
    // Tasks are not a thing here.
    let task = session
        .cancel_tools(CancelScope::Task { id: "t".into() })
        .await
        .unwrap_err();
    assert_eq!(task, ProviderError::unsupported("background_tasks"));
}

#[tokio::test]
async fn an_interruption_stops_the_running_tool_on_the_server_too() {
    let h = Harness::new(vec![tool_reply(
        Some("slow"),
        &[("s1", "mcp__fake__slow", json!({}))],
        None,
        None,
    )])
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_path(&log_dir);
    let session = h.open(h.spec_with_mcp(&log)).await;
    let stream = session.send_turn(TurnInput::text("slow")).await.unwrap();
    wait_for_events(&log, "slow_started", 1).await;
    session
        .interrupt(InterruptScope::TurnAndTools)
        .await
        .unwrap();
    let events = collect(stream).await;
    assert_eq!(stop_reason(&events), StopReason::Interrupted);
    let results = tool_results(&events);
    assert!(results[0].1, "the cut tool ends in error: {results:?}");
    wait_for_events(&log, "slow_cancelled", 1).await;
}

#[tokio::test]
async fn a_turn_timeout_is_a_classified_failure_not_a_hang() {
    let h = Harness::new(vec![busy_reply(Some("busy"))]).await;
    let mut spec = h.spec();
    spec.limits.turn_timeout_ms = Some(400);
    let session = h.open(spec).await;
    let events = turn(&*session, "busy").await;
    match done(&events) {
        AgentEvent::Done {
            is_error: true,
            error: Some(ProviderError::Timeout { after_ms: 400 }),
            stop_reason: StopReason::Error,
            ..
        } => {},
        other => panic!("expected a timeout failure, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Turn wall-clock limit (ISO5): none by default; a configured one says why
// ---------------------------------------------------------------------------

/// An endpoint in the process whose one answer comes after `delay` of tokio time.
/// Under a paused clock a turn that "lasts" hours runs at once, and only a wall-clock
/// cap of the harness could cut it.
struct SlowEndpoint {
    delay: Duration,
}

#[async_trait::async_trait]
impl ModelEndpoint for SlowEndpoint {
    fn id(&self) -> &str {
        "slow"
    }

    async fn health(&self) -> ProviderHealth {
        ProviderHealth::ok(None)
    }

    async fn models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }

    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionStream, ProviderError> {
        let delay = self.delay;
        Ok(Box::pin(async_stream::stream! {
            tokio::time::sleep(delay).await;
            yield Ok(CompletionChunk::Text("done after a long while".to_owned()));
            yield Ok(CompletionChunk::Finish(FinishReason::Stop));
        }))
    }

    async fn probe(&self, _model: &str) -> Result<EndpointProbe, ProviderError> {
        Ok(EndpointProbe {
            tools: false,
            parallel_tools: None,
            reasoning_field: None,
            context_window: Some(128_000),
            images: None,
            checked_at_ms: 0,
        })
    }
}

/// A session of a default config over [`SlowEndpoint`], with `limit` as the session's cap.
async fn slow_session(
    delay: Duration,
    limit: Option<u64>,
) -> (Arc<dyn AgentSession>, tempfile::TempDir) {
    let provider = NativeProvider::new(NativeConfig::new("slow"), Arc::new(SlowEndpoint { delay }));
    let cwd = tempfile::tempdir().unwrap();
    let mut spec = SessionSpec::new(cwd.path());
    spec.model = Some("m".to_owned());
    spec.limits.turn_timeout_ms = limit;
    (provider.open(spec).await.expect("open"), cwd)
}

/// A turn read to its end with no deadline of its own (`collect` has one, which a
/// paused clock would reach first).
async fn whole_turn(session: &dyn AgentSession, text: &str) -> Vec<AgentEvent> {
    session
        .send_turn(TurnInput::text(text))
        .await
        .expect("send_turn")
        .collect()
        .await
}

#[tokio::test(start_paused = true)]
async fn without_a_configured_cap_a_turn_of_hours_is_not_cut() {
    let hours = Duration::from_secs(3 * 3600);
    let (session, _cwd) = slow_session(hours, None).await;
    let started = tokio::time::Instant::now();
    let events = whole_turn(&*session, "work for hours").await;
    match done(&events) {
        AgentEvent::Done {
            stop_reason: StopReason::Completed,
            is_error: false,
            result_text: Some(text),
            ..
        } => assert_eq!(text, "done after a long while"),
        other => panic!("a long turn must complete, got {other:?}"),
    }
    assert!(started.elapsed() >= hours, "the turn did run for hours");
    assert!(
        !events.iter().any(|event| matches!(
            event,
            AgentEvent::ProviderNotice { kind, .. } if kind == "turn_time_limit"
        )),
        "{events:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_explicit_cap_still_stops_a_long_turn_and_says_its_limit() {
    let limit = 25 * 60 * 1000;
    let (session, _cwd) = slow_session(Duration::from_secs(3 * 3600), Some(limit)).await;
    let started = tokio::time::Instant::now();
    let events = whole_turn(&*session, "work for hours").await;
    assert!(started.elapsed() < Duration::from_secs(3 * 3600));
    match done(&events) {
        AgentEvent::Done {
            stop_reason: StopReason::Error,
            subtype: Some(subtype),
            is_error: true,
            error: Some(ProviderError::Timeout { after_ms }),
            ..
        } => {
            assert_eq!(subtype, "error_timeout");
            assert_eq!(*after_ms, limit);
        },
        other => panic!("expected the cap to fire, got {other:?}"),
    }
    let notice = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ProviderNotice { kind, data } if kind == "turn_time_limit" => Some(data),
            _ => None,
        })
        .expect("a turn_time_limit notice before the terminal event");
    assert_eq!(notice["limit_ms"], json!(limit));
    assert!(notice["elapsed_ms"].is_u64(), "{notice}");
    assert!(
        notice["next_turn_note"]
            .as_str()
            .unwrap()
            .contains("(25 min)"),
        "{notice}"
    );
}

/// The text of the last user message of a chat request.
fn last_user_text(body: &Value) -> String {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn after_a_capped_turn_the_next_request_tells_the_model_why_it_stopped() {
    const NOTE: &str = "by the configured turn time limit";
    let h = Harness::new(vec![
        busy_reply(Some("busy")),
        text_reply(Some(NOTE), "understood", None, None),
        text_reply(Some("third"), "fine", None, None),
    ])
    .await;
    let mut spec = h.spec();
    spec.system_prompt = Some(SystemPromptSpec {
        text: "You are a careful agent.".to_owned(),
        mode: SystemPromptMode::Replace,
    });
    spec.limits.turn_timeout_ms = Some(400);
    let session = h.open(spec).await;

    let first = turn(&*session, "busy").await;
    assert!(
        matches!(
            done(&first),
            AgentEvent::Done {
                error: Some(ProviderError::Timeout { after_ms: 400 }),
                ..
            }
        ),
        "{first:?}"
    );
    let second = turn(&*session, "what happened?").await;
    assert_eq!(stop_reason(&second), StopReason::Completed, "{second:?}");
    let third = turn(&*session, "third").await;
    assert_eq!(stop_reason(&third), StopReason::Completed, "{third:?}");

    let chat = h.chat();
    assert_eq!(chat.len(), 3, "{chat:?}");
    // The note opens the next user message, then the person's own words.
    let asked = last_user_text(&chat[1]);
    assert!(
        asked.starts_with("[system] Your previous turn was stopped after "),
        "{asked}"
    );
    assert!(asked.contains(&format!("{NOTE} (400 ms)")), "{asked}");
    assert!(asked.contains("not interrupted by the user"), "{asked}");
    assert!(asked.ends_with("\n\nwhat happened?"), "{asked}");
    // The capped turn's work was kept: its user message and the words streamed so far.
    let history = chat[1]["messages"].to_string();
    assert!(history.contains("\"busy\""), "{history}");
    assert!(history.contains("start"), "{history}");
    // Cache-friendly: the system prompt is the same on every request, never the note's place.
    for body in &chat {
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "You are a careful agent.");
    }
    // Said once: the turn after is not told again (the note stays in the history only).
    assert_eq!(last_user_text(&chat[2]), "third");
    assert_eq!(chat[2]["messages"].to_string().matches(NOTE).count(), 1);
}

// ---------------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_mcp_tool_in_error_is_an_error_result_and_the_turn_goes_on() {
    let h = Harness::new(vec![
        tool_reply(
            Some("fail"),
            &[("f1", "mcp__fake__fail", json!({}))],
            None,
            None,
        ),
        text_reply(Some(TOOL), "it failed, noted", None, None),
    ])
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let session = h.open(h.spec_with_mcp(&log_path(&log_dir))).await;
    let events = turn_deciding(&*session, "fail", |_| PermissionDecision::allow_once()).await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    assert_eq!(
        tool_results(&events),
        vec![("f1".into(), true, "boom".into())]
    );
    assert!(h.chat()[1].to_string().contains("boom"));
}

#[tokio::test]
async fn an_unreachable_mcp_server_is_a_typed_error_at_opening() {
    let h = Harness::new(vec![]).await;
    let mut spec = h.spec();
    spec.mcp_servers.insert(
        "gone".into(),
        nexus_claude::agent::McpServerSpec::Http {
            url: "http://127.0.0.1:9/mcp-secret-path".into(),
            headers: [(
                "Authorization".to_owned(),
                "Bearer HEADERTOKEN-123456".to_owned(),
            )]
            .into(),
        },
    );
    let refused = h.provider.open(spec).await.err().expect("refused");
    assert_eq!(refused.kind(), "endpoint_unreachable", "{refused}");
    let text = format!(
        "{refused} {refused:?} {}",
        serde_json::to_string(&refused).unwrap()
    );
    assert!(
        !text.contains("HEADERTOKEN") && !text.contains("mcp-secret-path"),
        "{text}"
    );

    let mut missing = h.spec();
    missing.mcp_servers.insert(
        "nope".into(),
        nexus_claude::agent::McpServerSpec::stdio("/nonexistent/server"),
    );
    let refused = h.provider.open(missing).await.err().expect("refused");
    assert_eq!(refused.kind(), "cli_not_found");
}

#[tokio::test]
async fn an_endpoint_failure_is_a_classified_done_and_leaves_the_transcript_untouched() {
    let h = Harness::new(vec![
        status_reply(
            Some("SECRET-TRANSCRIPT-TEXT"),
            503,
            json!({"error": {"message": "overloaded"}}),
        ),
        text_reply(Some("retry"), "fine", None, None),
    ])
    .await;
    let session = h.open(h.spec()).await;
    let events = turn(&*session, "SECRET-TRANSCRIPT-TEXT").await;
    let AgentEvent::Done {
        is_error: true,
        error: Some(error),
        ..
    } = done(&events)
    else {
        panic!("expected a classified failure: {:?}", done(&events));
    };
    assert_eq!(error, &ProviderError::Overloaded);
    // The text of the conversation never travels in an error.
    let all = format!("{:?}", done(&events));
    assert!(!all.contains("SECRET-TRANSCRIPT-TEXT"), "{all}");
    // The failed turn left nothing in the transcript: a retry sends one message.
    turn(&*session, "retry").await;
    let retry = h.chat().last().unwrap()["messages"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(retry, 1);
}

#[tokio::test]
async fn an_mcp_server_that_dies_kills_the_session_with_process_exited() {
    let h = Harness::new(vec![tool_reply(
        Some("die"),
        &[("d1", "mcp__fake__die", json!({}))],
        None,
        None,
    )])
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let session = h.open(h.spec_with_mcp(&log_path(&log_dir))).await;
    let events = turn_deciding(&*session, "die", |_| PermissionDecision::allow_once()).await;
    match done(&events) {
        AgentEvent::Error {
            error: ProviderError::ProcessExited { code },
        } => assert_eq!(*code, Some(7)),
        other => panic!("expected a terminal process_exited, got {other:?}"),
    }
    let next = session
        .send_turn(TurnInput::text("again"))
        .await
        .err()
        .expect("dead");
    assert_eq!(next.kind(), "process_exited");
    session.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// MCP over HTTP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tools_work_over_streamable_http_in_json_and_in_sse_with_the_auth_header() {
    for sse in [false, true] {
        let mcp = HttpMcp::start(Some("MCP-TOKEN-abcdef123456"), sse);
        let h = Harness::new(vec![
            tool_reply(Some("hi"), &[echo_call()], None, None),
            text_reply(Some(TOOL), "echoed", None, None),
        ])
        .await;
        let mut spec = h.spec();
        spec.mcp_servers.insert(
            "fake".into(),
            mcp.spec(&[("Authorization", "Bearer MCP-TOKEN-abcdef123456")]),
        );
        let session = h.open(spec).await;
        let events = turn(&*session, "hi").await;
        assert_eq!(stop_reason(&events), StopReason::Completed, "sse={sse}");
        assert_eq!(
            tool_results(&events),
            vec![("c1".into(), false, "echo: hi".into())]
        );
        let http = mcp.events();
        assert!(http.iter().any(|e| e["event"] == "http"));
        assert!(
            http.iter()
                .filter(|e| e["event"] == "http")
                .all(|e| e["authorised"] == true && e["has_authorization"] == true),
            "{http:?}"
        );
        // The session id the server issued is sent back after `initialize`.
        assert!(
            http.iter()
                .any(|e| e["event"] == "http" && e["session"] == true)
        );
        // Nothing the user or the model saw holds the token.
        assert!(!format!("{events:?}").contains("MCP-TOKEN-abcdef123456"));
        session.close().await.unwrap();
    }
}

#[tokio::test]
async fn a_wrong_mcp_token_is_unauthorized_and_never_echoed() {
    let mcp = HttpMcp::start(Some("RIGHT-TOKEN-0123456789"), false);
    let h = Harness::new(vec![]).await;
    let mut spec = h.spec();
    spec.mcp_servers.insert(
        "fake".into(),
        mcp.spec(&[("Authorization", "Bearer WRONG-TOKEN-0123456789")]),
    );
    let refused = h.provider.open(spec).await.err().expect("refused");
    assert_eq!(refused, ProviderError::Unauthorized);
    assert!(!format!("{refused:?}").contains("TOKEN-0123456789"));
}

#[tokio::test]
async fn an_http_mcp_call_is_cancelled_on_the_server() {
    let mcp = HttpMcp::start(None, false);
    let h = Harness::new(vec![tool_reply(
        Some("slow"),
        &[("s1", "mcp__fake__slow", json!({}))],
        None,
        None,
    )])
    .await;
    let mut spec = h.spec();
    spec.mcp_servers.insert("fake".into(), mcp.spec(&[]));
    let session = h.open(spec).await;
    let stream = session.send_turn(TurnInput::text("slow")).await.unwrap();
    wait_for_events(&mcp.log, "slow_started", 1).await;
    session
        .interrupt(InterruptScope::TurnAndTools)
        .await
        .unwrap();
    assert_eq!(stop_reason(&collect(stream).await), StopReason::Interrupted);
    wait_for_events(&mcp.log, "slow_cancelled", 1).await;
}

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stdio_server_gets_an_allowlisted_environment_and_no_secret_on_argv() {
    let h = Harness::new(vec![
        tool_reply(
            Some("inspect"),
            &[
                ("e1", "mcp__fake__env", json!({"name": "MCP_EXPLICIT_VAR"})),
                ("a1", "mcp__fake__argv", json!({})),
                ("h1", "mcp__fake__env", json!({"name": "HOME"})),
            ],
            None,
            None,
        ),
        text_reply(Some(TOOL), "inspected", None, None),
    ])
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let mut spec = h.spec_with_mcp(&log_path(&log_dir));
    // The host's own environment holds plenty (cargo sets CARGO_*): none of it
    // may reach the server. What the spec sets explicitly does.
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
    if let Some(nexus_claude::agent::McpServerSpec::Stdio { env, .. }) =
        spec.mcp_servers.get_mut("fake")
    {
        env.insert("MCP_EXPLICIT_VAR".into(), "explicit-value-42".into());
    }
    spec.env
        .set
        .insert("SESSION_SET_VAR".into(), "set-by-session".into());
    let session = h.open(spec).await;
    let events = turn(&*session, "inspect").await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    let results = tool_results(&events);
    let env = &results.iter().find(|r| r.0 == "e1").unwrap().2;
    let names = env.lines().next().unwrap();
    for leaked in [
        "CARGO_MANIFEST_DIR",
        "CARGO_PKG_NAME",
        "RUSTUP_HOME",
        "CARGO_HOME",
    ] {
        assert!(
            !names.contains(leaked),
            "{leaked} reached the MCP server: {names}"
        );
    }
    assert!(
        names.contains("MCP_EXPLICIT_VAR") && names.contains("SESSION_SET_VAR"),
        "{names}"
    );
    assert!(env.contains("MCP_EXPLICIT_VAR=explicit-value-42"));
    let argv = &results.iter().find(|r| r.0 == "a1").unwrap().2;
    assert_eq!(argv, "", "the server was started with no argument at all");
    // The server gets a dedicated HOME, not the host user's.
    let home = results.iter().find(|r| r.0 == "h1").unwrap().2.clone();
    let host_home = std::env::var("HOME").unwrap_or_default();
    assert!(
        home.contains("\nHOME=") && home.contains("nexus-native-home-"),
        "{home}"
    );
    assert!(
        host_home.is_empty()
            || !home.contains(&format!("HOME={host_home}\n"))
                && !home.ends_with(&format!("HOME={host_home}")),
        "{home}"
    );
}

#[tokio::test]
async fn with_strict_exposure_off_an_allow_list_only_pre_approves() {
    let h = Harness::with(vec![text_reply(None, "ok", None, None)], |config| {
        config.strict_tool_exposure = false;
    })
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let mut spec = h.spec_with_mcp(&log_path(&log_dir));
    spec.policy =
        ToolPolicy::from_patterns(PolicyMode::Ask, &["mcp__fake__echo"], &[] as &[&str]).unwrap();
    let session = h.open(spec).await;
    turn(&*session, "list").await;
    assert_eq!(offered_tools(h.chat().last().unwrap()).len(), 10);
}

// ---------------------------------------------------------------------------
// Host hooks (SessionHooks): the graph's way into the chain
// ---------------------------------------------------------------------------

use nexus_claude::agent::{
    CompactionInfo, HookSupport, HookVerdict, SessionHooks, ToolCallInfo, ToolResultInfo,
};
use std::sync::Mutex;

#[derive(Default)]
struct TestHooks {
    verdict: Mutex<Option<HookVerdict>>,
    after: Option<String>,
    compaction: Option<String>,
    /// `before_tool` never answers.
    stall: bool,
    calls: Mutex<Vec<ToolCallInfo>>,
    results: Mutex<Vec<ToolResultInfo>>,
    compactions: Mutex<Vec<CompactionInfo>>,
}

#[async_trait::async_trait]
impl SessionHooks for TestHooks {
    async fn before_tool(&self, call: &ToolCallInfo) -> HookVerdict {
        self.calls.lock().unwrap().push(call.clone());
        if self.stall {
            std::future::pending::<()>().await;
        }
        self.verdict
            .lock()
            .unwrap()
            .take()
            .unwrap_or(HookVerdict::Continue)
    }

    async fn after_tool(&self, result: &ToolResultInfo) -> Option<String> {
        self.results.lock().unwrap().push(result.clone());
        self.after.clone()
    }

    async fn before_compaction(&self, info: &CompactionInfo) -> Option<String> {
        self.compactions.lock().unwrap().push(info.clone());
        self.compaction.clone()
    }
}

fn echo_routes() -> Vec<Value> {
    vec![
        tool_reply(Some("go"), &[echo_call()], None, None),
        text_reply(Some(TOOL), "done", None, None),
    ]
}

async fn hooked(
    h: &Harness,
    hooks: Arc<TestHooks>,
    log: &std::path::Path,
) -> Arc<dyn AgentSession> {
    let mut spec = h.spec_with_mcp(log);
    spec.hooks = Some(hooks);
    h.open(spec).await
}

#[tokio::test]
async fn the_native_harness_runs_the_hooks_in_protocol_and_says_nothing_about_ignoring_them() {
    let h = Harness::new(echo_routes()).await;
    assert_eq!(
        h.provider.capabilities(Some("m")).hooks,
        HookSupport::InProtocol
    );
    let log_dir = tempfile::tempdir().unwrap();
    let session = hooked(&h, Arc::new(TestHooks::default()), &log_path(&log_dir)).await;
    let events = turn(&*session, "go").await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::ProviderNotice { kind, .. } if kind == "hooks_not_supported"
        )),
        "{events:?}"
    );
}

#[tokio::test]
async fn after_tool_text_reaches_the_model_and_not_the_tool_result_event() {
    let h = Harness::new(echo_routes()).await;
    let log_dir = tempfile::tempdir().unwrap();
    let hooks = Arc::new(TestHooks {
        after: Some("GRAPH: src/chat/manager.rs co-changes with composer.rs".into()),
        ..TestHooks::default()
    });
    let session = hooked(&h, hooks.clone(), &log_path(&log_dir)).await;
    let events = turn(&*session, "go").await;
    assert_eq!(stop_reason(&events), StopReason::Completed);

    // The model reads the output, then the hook's text, in the same tool message.
    let second = &h.chat()[1];
    let tool = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap();
    let content = tool["content"].as_str().unwrap();
    assert!(content.starts_with("echo: hi"), "{content}");
    assert!(content.contains("GRAPH: src/chat/manager.rs"), "{content}");
    // The event the consumer sees is the tool's own output.
    assert_eq!(tool_results(&events)[0].2, "echo: hi");
    // The hook saw the call that ran, with its canonical identity and input.
    let results = hooks.results.lock().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].call.name, "mcp__fake__echo");
    assert_eq!(results[0].call.input, json!({"text": "hi"}));
    assert_eq!(results[0].output, json!("echo: hi"));
    assert!(!results[0].is_error);
}

#[tokio::test]
async fn a_denying_before_tool_stops_the_call_and_tells_the_model_why() {
    let h = Harness::new(echo_routes()).await;
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_path(&log_dir);
    let hooks = Arc::new(TestHooks {
        verdict: Mutex::new(Some(HookVerdict::Deny {
            reason: "blocked by the graph: hot bridge".into(),
        })),
        ..TestHooks::default()
    });
    let session = hooked(&h, hooks.clone(), &log).await;
    let events = turn(&*session, "go").await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    assert_eq!(mcp_calls(&log, "echo"), 0);
    assert_eq!(
        tool_results(&events),
        vec![("c1".into(), true, "blocked by the graph: hot bridge".into())]
    );
    assert!(h.chat()[1].to_string().contains("hot bridge"));
    // A refused call did not run: no after_tool.
    assert!(hooks.results.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_replaced_input_is_what_runs() {
    let h = Harness::new(echo_routes()).await;
    let log_dir = tempfile::tempdir().unwrap();
    let hooks = Arc::new(TestHooks {
        verdict: Mutex::new(Some(HookVerdict::ReplaceInput(
            json!({"text": "rewritten"}),
        ))),
        ..TestHooks::default()
    });
    let session = hooked(&h, hooks, &log_path(&log_dir)).await;
    let events = turn(&*session, "go").await;
    assert_eq!(tool_results(&events)[0].2, "echo: rewritten");
}

#[tokio::test]
async fn a_hook_verdict_is_not_a_way_around_the_tool_policy() {
    // The hook cannot launder a call past the tool policy: `write` asks, and the user says no.
    let h = Harness::new(vec![
        tool_reply(Some("write"), &[write_call()], None, None),
        text_reply(Some(TOOL), "ok", None, None),
    ])
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_path(&log_dir);
    let hooks = Arc::new(TestHooks {
        verdict: Mutex::new(Some(HookVerdict::ReplaceInput(json!({"text": "x"})))),
        ..TestHooks::default()
    });
    let session = hooked(&h, hooks.clone(), &log).await;
    let events = turn_deciding(&*session, "write", |_| PermissionDecision::Deny {
        message: Some("no".into()),
        interrupt: false,
    })
    .await;
    // The hook did speak (a hook-free harness would also refuse, so this is what makes it a test)...
    assert_eq!(hooks.calls.lock().unwrap().len(), 1);
    // ...and the call still went through the policy, which asked and was refused.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::PermissionAsk { .. }))
    );
    assert_eq!(mcp_calls(&log, "write"), 0);
    assert_eq!(tool_results(&events)[0].2, "no");
}

#[tokio::test]
async fn a_replacement_that_is_not_an_object_is_refused() {
    let h = Harness::new(echo_routes()).await;
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_path(&log_dir);
    let hooks = Arc::new(TestHooks {
        verdict: Mutex::new(Some(HookVerdict::ReplaceInput(json!("nope")))),
        ..TestHooks::default()
    });
    let session = hooked(&h, hooks, &log).await;
    let events = turn(&*session, "go").await;
    assert_eq!(mcp_calls(&log, "echo"), 0);
    assert!(tool_results(&events)[0].1);
}

#[tokio::test]
async fn context_added_before_the_tool_follows_its_result() {
    let h = Harness::new(echo_routes()).await;
    let log_dir = tempfile::tempdir().unwrap();
    let hooks = Arc::new(TestHooks {
        verdict: Mutex::new(Some(HookVerdict::AddContext("NOTE: gotcha on echo".into()))),
        ..TestHooks::default()
    });
    let session = hooked(&h, hooks, &log_path(&log_dir)).await;
    let events = turn(&*session, "go").await;
    assert_eq!(tool_results(&events)[0].2, "echo: hi");
    let second = h.chat()[1].to_string();
    assert!(second.contains("NOTE: gotcha on echo"), "{second}");
}

#[tokio::test]
async fn a_hook_that_never_answers_does_not_freeze_the_turn() {
    let h = Harness::with(echo_routes(), |config| {
        config.hook_timeout = Duration::from_millis(150);
    })
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let log = log_path(&log_dir);
    let hooks = Arc::new(TestHooks {
        stall: true,
        ..TestHooks::default()
    });
    let session = hooked(&h, hooks, &log).await;
    let started = Instant::now();
    let events = turn(&*session, "go").await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(stop_reason(&events), StopReason::Completed);
    // Skipped, not failed: the call went through the normal policy.
    assert_eq!(mcp_calls(&log, "echo"), 1);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ProviderNotice { kind, .. } if kind == "hook_timeout"
    )));
}

#[tokio::test]
async fn before_compaction_text_joins_the_summary_instructions() {
    let h = Harness::with(
        vec![
            tool_reply(Some("go"), &[echo_call()], Some("old"), Some((100, 10))),
            tool_reply(
                Some(TOOL),
                &[("c2", "mcp__fake__echo", json!({"text": "two"}))],
                Some("kept"),
                Some((10_000, 10)),
            ),
            text_reply(
                Some("compacting the history"),
                "THE SUMMARY",
                None,
                Some((50, 20)),
            ),
            text_reply(Some(TOOL), "done", None, Some((300, 10))),
        ],
        |config| {
            config.context_window = Some(12_000);
            config.compaction.keep_recent = 2;
        },
    )
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let hooks = Arc::new(TestHooks {
        compaction: Some("Keep the decision about the hot bridge.".into()),
        ..TestHooks::default()
    });
    let session = hooked(&h, hooks.clone(), &log_path(&log_dir)).await;
    let events = turn(&*session, "go").await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    let summary = &h.chat()[2];
    let body = summary["messages"][1]["content"].as_str().unwrap();
    assert!(
        body.contains("Additional instructions: Keep the decision about the hot bridge."),
        "{body}"
    );
    let seen = hooks.compactions.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].trigger, "auto");
}

// ---------------------------------------------------------------------------
// Active model: `set_model` and a `before_turn` directive make a model active;
// its window and its price govern the turn, not the opening snapshot's (R3)
// ---------------------------------------------------------------------------

use nexus_claude::agent::{TurnContext, TurnDirective};

/// Hooks that answer `before_turn` with a fixed directive and record what they saw.
#[derive(Default)]
struct ModelHooks {
    /// Model to ask for; `None` asks for nothing.
    model: Mutex<Option<String>>,
    /// `before_turn` never answers.
    stall: bool,
    contexts: Mutex<Vec<TurnContext>>,
}

#[async_trait::async_trait]
impl SessionHooks for ModelHooks {
    async fn before_turn(&self, ctx: &TurnContext) -> TurnDirective {
        self.contexts.lock().unwrap().push(ctx.clone());
        if self.stall {
            std::future::pending::<()>().await;
        }
        match self.model.lock().unwrap().clone() {
            Some(model) => TurnDirective::model(model),
            None => TurnDirective::none(),
        }
    }
}

/// Two models on the same endpoint: `m` (128k window, no price) and `small`
/// (2k window, priced). The opening model is `m`.
struct TwoModels {
    server: FakeOpenAi,
    provider: Arc<NativeProvider>,
    cwd: tempfile::TempDir,
}

impl TwoModels {
    async fn start(routes: Vec<Value>, tweak: impl FnOnce(&mut NativeConfig)) -> Self {
        let mut all = vec![
            probe_route(false),
            json!({"method": "GET", "path": "/v1/models", "status": 200, "body": {"object": "list",
                "data": [{"id": "m", "context_length": 128_000}, {"id": "small", "context_length": 2_000}]}}),
        ];
        all.extend(routes);
        let server = FakeOpenAi::start(json!(all));
        let mut config = NativeConfig::new("native-test");
        config.default_model = Some("m".to_owned());
        config.prices = PriceTable::new().with("small", price(10.0));
        tweak(&mut config);
        let provider = Arc::new(NativeProvider::new(
            config,
            endpoint(server.base_url(), EndpointQuirks::generic()),
        ));
        Self {
            server,
            provider,
            cwd: tempfile::tempdir().unwrap(),
        }
    }

    async fn open(&self, hooks: Option<Arc<ModelHooks>>) -> Arc<dyn AgentSession> {
        let mut spec = SessionSpec::new(self.cwd.path());
        spec.model = Some("m".to_owned());
        spec.hooks = hooks.map(|hooks| hooks as Arc<dyn SessionHooks>);
        self.provider.open(spec).await.expect("open")
    }

    /// The `model` field of every chat request, probes excluded.
    fn wire_models(&self) -> Vec<String> {
        self.server
            .requests_to("POST", "/v1/chat/completions")
            .into_iter()
            .map(|r| r["body"].clone())
            .filter(|body| !body.to_string().contains("Call the ping tool now"))
            .map(|body| body["model"].as_str().unwrap().to_owned())
            .collect()
    }
}

fn model_changes(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ModelChanged { model } => Some(model.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn set_model_sends_the_new_model_on_the_wire_and_prices_the_turn_with_it() {
    let t = TwoModels::start(
        vec![
            text_reply(Some("alpha"), "ok", None, Some((100, 10))),
            text_reply(Some("beta"), "ok", None, Some((100, 10))),
        ],
        |_| {},
    )
    .await;
    let session = t.open(None).await;
    let first = turn(&*session, "alpha").await;
    let AgentEvent::Done { cost, .. } = done(&first) else {
        unreachable!()
    };
    // The opening model has no price: unknown, never zero.
    assert_eq!((cost.usd, cost.basis), (None, CostBasis::Unknown));

    session.set_model("small").await.unwrap();
    let second = turn(&*session, "beta").await;
    assert_eq!(stop_reason(&second), StopReason::Completed);
    assert_eq!(
        t.wire_models(),
        ["m", "small"],
        "the wire carries the active model"
    );
    let AgentEvent::Done {
        cost, usage, model, ..
    } = done(&second)
    else {
        unreachable!()
    };
    // The active model is priced: its price, its basis, its window on the usage.
    assert_eq!(model.as_deref(), Some("small"));
    assert_eq!(cost.basis, CostBasis::Priced);
    assert!(cost.usd.is_some_and(|usd| usd > 0.0), "{cost:?}");
    assert_eq!(usage.by_model[0].model, "small");
    assert_eq!(usage.by_model[0].context_window, Some(2_000));
    // The snapshot of the session did not move (A4).
    assert_eq!(session.capabilities().cost, CostBasis::Unknown);
    assert_eq!(
        session.capabilities().context_window.unwrap().value,
        128_000
    );
}

#[tokio::test]
async fn a_smaller_active_model_compacts_where_the_opening_one_would_not() {
    let t = TwoModels::start(
        vec![
            // The first turn leaves 1 800 tokens of context: nothing for a 128k window.
            text_reply(Some("alpha"), "ok", None, Some((1_800, 5))),
            text_reply(
                Some("compacting the history"),
                "THE SUMMARY",
                None,
                Some((50, 20)),
            ),
            text_reply(Some("beta"), "done", None, Some((300, 10))),
        ],
        |config| config.compaction.keep_recent = 1,
    )
    .await;
    let session = t.open(None).await;
    let first = turn(&*session, "alpha").await;
    assert!(
        !first
            .iter()
            .any(|e| matches!(e, AgentEvent::Compaction { .. })),
        "128k window: no compaction"
    );
    // 1 800 ≥ 80 % of the 2k window of the model now active: the next turn compacts first.
    session.set_model("small").await.unwrap();
    let second = turn(&*session, "beta").await;
    assert_eq!(stop_reason(&second), StopReason::Completed);
    let phases: Vec<String> = second
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Compaction { phase, .. } => Some(format!("{phase:?}")),
            _ => None,
        })
        .collect();
    assert_eq!(phases, ["Started", "Completed"], "{second:?}");
    assert_eq!(t.wire_models(), ["m", "small", "small"]);
}

#[tokio::test]
async fn a_before_turn_directive_runs_the_turn_on_that_model_and_announces_it_first() {
    let t = TwoModels::start(
        vec![
            text_reply(Some("alpha"), "ok", None, Some((100, 10))),
            text_reply(Some("beta"), "ok", None, Some((100, 10))),
        ],
        |_| {},
    )
    .await;
    let hooks = Arc::new(ModelHooks {
        model: Mutex::new(Some("small".to_owned())),
        ..ModelHooks::default()
    });
    let session = t.open(Some(hooks.clone())).await;
    let first = turn(&*session, "alpha").await;
    assert_eq!(stop_reason(&first), StopReason::Completed);
    assert_eq!(model_changes(&first), ["small"]);
    assert!(
        matches!(first[0], AgentEvent::ModelChanged { .. }),
        "the change is announced before anything else of the turn: {first:?}"
    );
    let AgentEvent::Done { model, cost, .. } = done(&first) else {
        unreachable!()
    };
    assert_eq!(model.as_deref(), Some("small"));
    assert_eq!(cost.basis, CostBasis::Priced);

    // The hook says nothing on the next turn: the model stays, nothing is announced.
    *hooks.model.lock().unwrap() = None;
    let second = turn(&*session, "beta").await;
    assert!(model_changes(&second).is_empty());
    assert_eq!(t.wire_models(), ["small", "small"]);

    let contexts = hooks.contexts.lock().unwrap();
    assert_eq!(contexts.len(), 2);
    assert_eq!(
        (contexts[0].turn_index, contexts[0].current_model.as_str()),
        (0, "m")
    );
    assert_eq!(contexts[0].input_chars, "alpha".len());
    assert_eq!(
        (contexts[1].turn_index, contexts[1].current_model.as_str()),
        (1, "small")
    );
    assert_eq!(contexts[1].tokens_spent, 110);
    assert!(
        contexts[1].usd_spent.is_some(),
        "a priced active model reports what was spent"
    );
}

#[tokio::test]
async fn a_directive_naming_the_current_model_or_an_empty_one_changes_nothing() {
    let t = TwoModels::start(vec![text_reply(None, "ok", None, Some((100, 10)))], |_| {}).await;
    let hooks = Arc::new(ModelHooks {
        model: Mutex::new(Some("m".to_owned())),
        ..ModelHooks::default()
    });
    let session = t.open(Some(hooks.clone())).await;
    let same = turn(&*session, "alpha").await;
    assert!(model_changes(&same).is_empty());
    *hooks.model.lock().unwrap() = Some("  ".to_owned());
    let empty = turn(&*session, "beta").await;
    assert!(model_changes(&empty).is_empty());
    assert_eq!(t.wire_models(), ["m", "m"]);
}

#[tokio::test]
async fn a_before_turn_hook_that_never_answers_is_skipped_with_a_notice_and_the_turn_runs() {
    let t = TwoModels::start(
        vec![text_reply(None, "ok", None, Some((100, 10)))],
        |config| config.hook_timeout = Duration::from_millis(50),
    )
    .await;
    let hooks = Arc::new(ModelHooks {
        model: Mutex::new(Some("small".to_owned())),
        stall: true,
        ..ModelHooks::default()
    });
    let session = t.open(Some(hooks)).await;
    let events = turn(&*session, "alpha").await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ProviderNotice { kind, data } if kind == "hook_timeout" && data["hook"] == "before_turn"
    )), "{events:?}");
    assert!(model_changes(&events).is_empty());
    assert_eq!(t.wire_models(), ["m"]);
}

// ---------------------------------------------------------------------------
// Images: the vision of the MODEL, never a "no" of the harness
// ---------------------------------------------------------------------------

/// A 1×1 transparent PNG.
const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

fn image_turn(text: &str) -> TurnInput {
    let mut input = TurnInput::text(text);
    input.blocks.push(InputBlock::Image {
        media_type: "image/png".to_owned(),
        data_base64: PIXEL.to_owned(),
    });
    input
}

/// A catalogue that states the modality of `m` the way OpenRouter does.
fn modality_route(modality: &str) -> Value {
    json!({"method": "GET", "path": "/v1/models", "status": 200,
        "body": {"data": [{"id": "m", "context_length": 128_000, "architecture": {"modality": modality}}]}})
}

fn result_text(events: &[AgentEvent]) -> String {
    match done(events) {
        AgentEvent::Done { result_text, .. } => result_text.clone().unwrap_or_default(),
        other => panic!("expected done, got {other:?}"),
    }
}

fn image_parts(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|part| part["type"] == "image_url")
        .map(|part| part["image_url"]["url"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a_vision_model_sees_the_image_and_the_transcript_keeps_it() {
    let h = Harness::raw(
        vec![
            probe_route(true),
            modality_route("text+image->text"),
            // The model answers one way when the request carries the image, and
            // another when it does not: a dropped image is a different answer.
            text_reply(Some("image_url"), "I see one pixel", None, Some((100, 5))),
            text_reply(None, "no image here", None, Some((100, 5))),
        ],
        EndpointQuirks::deepseek(),
        |_| {},
    )
    .await;
    let caps = h.provider.refresh_capabilities("m").await.unwrap();
    assert!(caps.images, "the catalogue states the vision of the model");
    let session = h.open(h.spec()).await;
    assert!(session.capabilities().images);

    let events = collect(
        session
            .send_turn(image_turn("what is this?"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    assert_eq!(result_text(&events), "I see one pixel");
    let requests = h.chat();
    let parts = requests[0]["messages"][0]["content"].as_array().unwrap();
    assert_eq!(parts[0], json!({"type": "text", "text": "what is this?"}));
    assert_eq!(
        parts[1],
        json!({"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{PIXEL}")}})
    );
    // The transcript keeps the image whole, and the next turn replays it.
    let AgentEvent::Done {
        provider_session_id: Some(id),
        ..
    } = done(&events)
    else {
        panic!("done carries the transcript id");
    };
    let saved = h.store.load(id).unwrap().unwrap();
    assert_eq!(saved[0].images.len(), 1);
    assert_eq!(saved[0].images[0].data_base64, PIXEL);
    assert_eq!(saved[0].content.as_deref(), Some("what is this?"));
    let events = turn(&*session, "and now?").await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    let replay = &h.chat()[1];
    assert_eq!(image_parts(replay).len(), 1);
    assert_eq!(replay["messages"][2]["content"], "and now?");
}

#[tokio::test]
async fn a_model_without_vision_refuses_an_image_as_its_own_limit() {
    let h = Harness::raw(
        vec![
            probe_route(true),
            modality_route("text->text"),
            text_reply(None, "text only", None, Some((100, 5))),
        ],
        EndpointQuirks::deepseek(),
        |_| {},
    )
    .await;
    let caps = h.provider.refresh_capabilities("m").await.unwrap();
    assert!(!caps.images);
    let listed = h.provider.catalog().await.unwrap();
    assert_eq!(listed[0].supports_images, Some(false));
    let session = h.open(h.spec()).await;
    assert_eq!(
        session.send_turn(image_turn("look")).await.err(),
        Some(ProviderError::unsupported("images"))
    );
    // The refusal left no turn running, and nothing with an image reached the model.
    let events = turn(&*session, "plain").await;
    assert_eq!(result_text(&events), "text only");
    let requests = h.chat();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].to_string().contains("image_url"));
}

#[tokio::test]
async fn a_silent_catalogue_is_asked_with_one_pixel_only_when_the_instance_allows_it() {
    let probing = || {
        let mut quirks = EndpointQuirks::deepseek();
        quirks.vision_probe = true;
        quirks
    };
    // The model refuses the pixel, naming images: no vision.
    let h = Harness::raw(
        vec![
            probe_route(true),
            models_route(128_000),
            status_reply(
                Some("image_url"),
                400,
                json!({"error": {"message": "This model does not support image input"}}),
            ),
            text_reply(None, "ok", None, None),
        ],
        probing(),
        |_| {},
    )
    .await;
    assert!(!h.provider.refresh_capabilities("m").await.unwrap().images);
    assert_eq!(h.chat().len(), 1, "the pixel request alone");
    assert_eq!(image_parts(&h.chat()[0]).len(), 1);
    // The model answers the pixel: vision.
    let h = Harness::raw(
        vec![
            probe_route(true),
            models_route(128_000),
            text_reply(Some("image_url"), "a pixel", None, None),
            // The pixel probe takes the route above; the turn gets this one.
            text_reply(Some("look"), "a pixel", None, None),
            text_reply(None, "ok", None, None),
        ],
        probing(),
        |_| {},
    )
    .await;
    assert!(h.provider.refresh_capabilities("m").await.unwrap().images);
    let session = h.open(h.spec()).await;
    let events = collect(session.send_turn(image_turn("look")).await.unwrap()).await;
    assert_eq!(result_text(&events), "a pixel");
    // Without the probe nothing is asked and nothing is claimed...
    let h = Harness::new(vec![text_reply(None, "ok", None, None)]).await;
    assert!(!h.provider.refresh_capabilities("m").await.unwrap().images);
    assert!(h.chat().is_empty());
    // ...unless the instance declares the vision of its models.
    let mut declared = EndpointQuirks::deepseek();
    declared.vision = Some(true);
    let h = Harness::raw(
        vec![
            probe_route(true),
            models_route(128_000),
            text_reply(None, "ok", None, None),
        ],
        declared,
        |_| {},
    )
    .await;
    assert!(h.provider.refresh_capabilities("m").await.unwrap().images);
    assert!(h.chat().is_empty());
}

#[tokio::test]
async fn images_of_the_summarised_history_are_said_never_lost_in_silence() {
    let h = Harness::raw(
        vec![
            probe_route(true),
            modality_route("text+image->text"),
            tool_reply(Some("image_url"), &[echo_call()], None, Some((100, 10))),
            text_reply(Some(TOOL), "done one", None, Some((10_000, 10))),
            text_reply(
                Some("compacting the history"),
                "THE SUMMARY",
                None,
                Some((50, 20)),
            ),
            text_reply(Some("second"), "after compaction", None, Some((300, 10))),
        ],
        EndpointQuirks::deepseek(),
        |config| {
            config.context_window = Some(12_000);
            config.compaction.keep_recent = 2;
        },
    )
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let session = h.open(h.spec_with_mcp(&log_path(&log_dir))).await;
    let first = collect(session.send_turn(image_turn("look at this")).await.unwrap()).await;
    assert_eq!(result_text(&first), "done one");
    let second = turn(&*session, "second").await;
    assert_eq!(result_text(&second), "after compaction");

    // The compaction said what it did with the image.
    let notice = second
        .iter()
        .find_map(|event| match event {
            AgentEvent::ProviderNotice { kind, data } if kind == "images_compacted" => Some(data),
            _ => None,
        })
        .expect("a provider_notice images_compacted");
    assert_eq!(notice["count"], 1);
    let phases: Vec<&AgentEvent> = second
        .iter()
        .filter(|e| matches!(e, AgentEvent::Compaction { .. }))
        .collect();
    assert_eq!(phases.len(), 2);
    let requests = h.chat();
    assert_eq!(requests.len(), 4, "{requests:?}");
    // The summarisation prompt names the image and never carries its pixels.
    let summary = requests[2]["messages"][1]["content"].as_str().unwrap();
    assert!(summary.contains("[image image/png]"), "{summary}");
    assert!(!summary.contains(PIXEL));
    assert!(image_parts(&requests[2]).is_empty());
    // After compaction the image is gone from the wire, with the summary in its place.
    assert!(image_parts(&requests[3]).is_empty());
    assert!(
        requests[3]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("THE SUMMARY")
    );
}

#[tokio::test]
async fn an_image_block_is_checked_before_the_turn_starts() {
    let h = Harness::raw(
        vec![
            probe_route(true),
            modality_route("text+image->text"),
            text_reply(None, "ok", None, None),
        ],
        EndpointQuirks::deepseek(),
        |_| {},
    )
    .await;
    h.provider.refresh_capabilities("m").await.unwrap();
    let session = h.open(h.spec()).await;
    let mut input = TurnInput::text("look");
    input.blocks.push(InputBlock::Image {
        media_type: "application/pdf".to_owned(),
        data_base64: PIXEL.to_owned(),
    });
    assert_eq!(
        session
            .send_turn(input)
            .await
            .err()
            .map(|e| e.kind().to_owned()),
        Some("invalid_request".to_owned())
    );
    // Nothing was started: a plain turn runs.
    assert_eq!(result_text(&turn(&*session, "plain").await), "ok");
    assert_eq!(h.chat().len(), 1);
}

/// A catalogue with `m` (vision) and `text` (no vision), OpenRouter style.
fn vision_pair_route() -> Value {
    json!({"method": "GET", "path": "/v1/models", "status": 200, "body": {"data": [
        {"id": "m", "context_length": 128_000, "architecture": {"modality": "text+image->text"}},
        {"id": "text", "context_length": 128_000, "architecture": {"modality": "text->text"}},
    ]}})
}

#[tokio::test]
async fn the_active_model_decides_images_after_set_model_and_the_history_says_what_it_lost() {
    let h = Harness::raw(
        vec![
            probe_route(true),
            vision_pair_route(),
            text_reply(Some("image_url"), "I see one pixel", None, Some((100, 5))),
            text_reply(Some("plain"), "text only", None, Some((100, 5))),
            text_reply(None, "back on m", None, Some((100, 5))),
        ],
        EndpointQuirks::deepseek(),
        |_| {},
    )
    .await;
    let session = h.open(h.spec()).await;
    assert!(session.capabilities().images);
    let events = collect(session.send_turn(image_turn("look")).await.unwrap()).await;
    assert_eq!(result_text(&events), "I see one pixel");

    // `text` has no vision: an image turn is refused with the typed error, although
    // the snapshot of the session (A4) still says what `m` could do.
    session.set_model("text").await.unwrap();
    assert!(session.capabilities().images);
    assert_eq!(
        session.send_turn(image_turn("again")).await.err(),
        Some(ProviderError::unsupported("images"))
    );
    // A text turn on `text` replays the history without the pixels, with a marker.
    let events = turn(&*session, "plain").await;
    assert_eq!(result_text(&events), "text only");
    let requests = h.chat();
    let replay = requests.last().unwrap();
    assert_eq!(replay["model"], "text");
    assert!(image_parts(replay).is_empty(), "{replay}");
    let first = replay["messages"][0]["content"].as_str().unwrap();
    assert!(
        first.starts_with("look\n[image omitted: the active model has no vision]"),
        "{first}"
    );

    // Back on `m`: images are accepted again, and the history has its pixel back.
    session.set_model("m").await.unwrap();
    let events = collect(session.send_turn(image_turn("once more")).await.unwrap()).await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    let last = h.chat().last().unwrap().clone();
    assert_eq!(last["model"], "m");
    assert_eq!(image_parts(&last).len(), 2, "{last}");
}

#[tokio::test]
async fn a_before_turn_switch_to_a_model_without_vision_fails_the_image_turn_typed() {
    let h = Harness::raw(
        vec![
            probe_route(true),
            vision_pair_route(),
            text_reply(None, "should not be reached", None, Some((100, 5))),
        ],
        EndpointQuirks::deepseek(),
        |_| {},
    )
    .await;
    let hooks = Arc::new(ModelHooks {
        model: Mutex::new(Some("text".to_owned())),
        ..ModelHooks::default()
    });
    let mut spec = h.spec();
    spec.hooks = Some(hooks as Arc<dyn SessionHooks>);
    let session = h.open(spec).await;
    // Accepted on `m` (vision), then the directive makes `text` active.
    let events = collect(session.send_turn(image_turn("look")).await.unwrap()).await;
    assert_eq!(model_changes(&events), ["text"]);
    let AgentEvent::Done {
        stop_reason,
        is_error,
        error,
        ..
    } = done(&events)
    else {
        panic!("expected done");
    };
    assert_eq!(*stop_reason, StopReason::Error);
    assert!(*is_error);
    assert_eq!(error.clone(), Some(ProviderError::unsupported("images")));
    // Nothing reached the model.
    assert!(h.chat().is_empty(), "{:?}", h.chat());
}

/// A turn whose model calls the `picture` tool of `fake_mcp` (a text and an image).
fn picture_routes(modality: &str) -> Vec<Value> {
    vec![
        probe_route(true),
        modality_route(modality),
        tool_reply(
            Some("show me"),
            &[("c1", "mcp__fake__picture", json!({}))],
            None,
            Some((100, 10)),
        ),
        text_reply(Some(TOOL), "it is a pixel", None, Some((120, 10))),
    ]
}

#[tokio::test]
async fn a_tool_image_reaches_a_vision_model_in_a_user_message_after_the_results() {
    let h = Harness::raw(
        picture_routes("text+image->text"),
        EndpointQuirks::deepseek(),
        |_| {},
    )
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let session = h.open(h.spec_with_mcp(&log_path(&log_dir))).await;
    let events = turn(&*session, "show me").await;
    assert_eq!(result_text(&events), "it is a pixel");
    let requests = h.chat();
    assert_eq!(requests.len(), 2);
    let messages = requests[1]["messages"].as_array().unwrap();
    // user, assistant (tool call), tool result, then the user message with the image.
    assert_eq!(messages.len(), 4, "{messages:?}");
    assert_eq!(messages[2]["role"], "tool");
    let result = messages[2]["content"].as_str().unwrap();
    assert_eq!(
        result,
        "here is the picture\n[image: attached in the next message]"
    );
    assert_eq!(messages[3]["role"], "user");
    let parts = messages[3]["content"].as_array().unwrap();
    assert_eq!(
        parts[0],
        json!({"type": "text", "text": "[images returned by the tool call(s) c1]"})
    );
    assert_eq!(
        parts[1]["image_url"]["url"],
        format!("data:image/png;base64,{PIXEL}")
    );
}

#[tokio::test]
async fn a_tool_image_stays_a_marker_for_a_model_without_vision() {
    let h = Harness::raw(
        picture_routes("text->text"),
        EndpointQuirks::deepseek(),
        |_| {},
    )
    .await;
    let log_dir = tempfile::tempdir().unwrap();
    let session = h.open(h.spec_with_mcp(&log_path(&log_dir))).await;
    let events = turn(&*session, "show me").await;
    assert_eq!(result_text(&events), "it is a pixel");
    let requests = h.chat();
    let messages = requests[1]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3, "{messages:?}");
    assert_eq!(
        messages[2]["content"],
        "here is the picture\n[image content not shown]"
    );
    assert!(!requests[1].to_string().contains("image_url"));
}

// ---------------------------------------------------------------------------
// The harness's own tools (P18): AskUserQuestion and Agent
// ---------------------------------------------------------------------------

/// A host that runs sub-agents: answers every `Agent` call with `answer`, records the
/// requests, and notices when a sub-agent's future is dropped before its end.
struct SubagentHost {
    answer: Result<String, String>,
    /// How long a sub-agent takes.
    takes: Duration,
    asked: std::sync::Mutex<Vec<nexus_claude::agent::SubagentRequest>>,
    dropped: std::sync::atomic::AtomicBool,
    /// `before_tool` refuses every call.
    deny_before: bool,
}

impl SubagentHost {
    fn new(answer: Result<&str, &str>, takes: Duration) -> Arc<Self> {
        Arc::new(Self {
            answer: answer.map(str::to_owned).map_err(str::to_owned),
            takes,
            asked: std::sync::Mutex::new(Vec::new()),
            dropped: std::sync::atomic::AtomicBool::new(false),
            deny_before: false,
        })
    }

    fn denying() -> Arc<Self> {
        Arc::new(Self {
            answer: Ok("never".to_owned()),
            takes: Duration::ZERO,
            asked: std::sync::Mutex::new(Vec::new()),
            dropped: std::sync::atomic::AtomicBool::new(false),
            deny_before: true,
        })
    }
}

struct DropFlag<'a>(&'a std::sync::atomic::AtomicBool, bool);

impl Drop for DropFlag<'_> {
    fn drop(&mut self) {
        if !self.1 {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

#[async_trait::async_trait]
impl nexus_claude::agent::SessionHooks for SubagentHost {
    async fn before_tool(
        &self,
        _call: &nexus_claude::agent::ToolCallInfo,
    ) -> nexus_claude::agent::HookVerdict {
        if self.deny_before {
            nexus_claude::agent::HookVerdict::Deny {
                reason: "DENIED-BY-HOOK".to_owned(),
            }
        } else {
            nexus_claude::agent::HookVerdict::Continue
        }
    }

    fn runs_subagents(&self) -> bool {
        true
    }

    async fn run_subagent(
        &self,
        request: &nexus_claude::agent::SubagentRequest,
    ) -> Result<String, String> {
        self.asked.lock().unwrap().push(request.clone());
        let mut flag = DropFlag(&self.dropped, false);
        tokio::time::sleep(self.takes).await;
        flag.1 = true;
        self.answer.clone()
    }
}

fn agent_call(id: &'static str) -> (&'static str, &'static str, Value) {
    (
        id,
        "Agent",
        json!({"description": "find the spec", "prompt": "Find where the spec is built"}),
    )
}

#[tokio::test]
async fn a_host_that_runs_subagents_gives_the_model_an_agent_tool_whose_answer_is_the_result() {
    let h = Harness::new(vec![
        tool_reply(Some("delegate"), &[agent_call("a1")], None, None),
        text_reply(Some(TOOL), "the sub-agent found it", None, None),
    ])
    .await;
    let host = SubagentHost::new(Ok("SPEC-IS-IN-manager.rs"), Duration::ZERO);
    let mut spec = h.spec();
    spec.hooks = Some(host.clone());
    let session = h.open(spec).await;
    let events = turn(&*session, "delegate the search").await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    let call = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolCall { name, category, .. } if name == "Agent" => Some(*category),
            _ => None,
        })
        .expect("the Agent call is announced");
    assert_eq!(call, nexus_claude::agent::ToolCategory::Agent);
    assert_eq!(
        tool_results(&events),
        vec![("a1".to_owned(), false, "SPEC-IS-IN-manager.rs".to_owned())]
    );
    let asked = host.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].prompt, "Find where the spec is built");
    assert_eq!(asked[0].description.as_deref(), Some("find the spec"));
    assert_eq!(asked[0].tool_call_id, "a1");
    let chat = h.chat();
    assert!(offered_tools(&chat[0]).contains(&"Agent".to_owned()));
    assert!(chat[1].to_string().contains("SPEC-IS-IN-manager.rs"));
}

#[tokio::test]
async fn a_failed_subagent_is_an_error_result_and_the_turn_goes_on() {
    let h = Harness::new(vec![
        tool_reply(Some("delegate"), &[agent_call("a1")], None, None),
        text_reply(Some(TOOL), "carried on", None, None),
    ])
    .await;
    let host = SubagentHost::new(Err("too many sub-agents"), Duration::ZERO);
    let mut spec = h.spec();
    spec.hooks = Some(host);
    let session = h.open(spec).await;
    let events = turn(&*session, "delegate the search").await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    assert_eq!(
        tool_results(&events),
        vec![("a1".to_owned(), true, "too many sub-agents".to_owned())]
    );
}

#[tokio::test]
async fn no_agent_tool_without_a_host_that_runs_subagents_nor_for_a_child_session() {
    let h = Harness::new(vec![
        text_reply(Some("first"), "one", None, None),
        text_reply(Some("second"), "two", None, None),
        text_reply(Some("third"), "three", None, None),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    // No hooks: no Agent (the other tools are there).
    let session = h.open(h.spec_with_mcp(&log_path(&dir))).await;
    turn(&*session, "first").await;
    // A child session (depth 1) never gets it, even from a host that runs sub-agents (A17).
    let mut child = h.spec_with_mcp(&log_path(&dir));
    child.hooks = Some(SubagentHost::new(Ok("x"), Duration::ZERO));
    child.lineage = Some(nexus_claude::agent::Lineage {
        parent_session_id: "parent".to_owned(),
        root_session_id: "parent".to_owned(),
        depth: 1,
        spawned_by: "conversation".to_owned(),
    });
    let session = h.open(child).await;
    turn(&*session, "second").await;
    // The same host at depth 0 gives it: the guard is the lineage, nothing else.
    let mut top = h.spec_with_mcp(&log_path(&dir));
    top.hooks = Some(SubagentHost::new(Ok("x"), Duration::ZERO));
    let session = h.open(top).await;
    turn(&*session, "third").await;
    let chat = h.chat();
    assert_eq!(chat.len(), 3);
    for body in &chat[..2] {
        let offered = offered_tools(body);
        assert!(!offered.is_empty(), "the MCP tools are offered");
        assert!(!offered.contains(&"Agent".to_owned()), "{offered:?}");
    }
    assert!(offered_tools(&chat[2]).contains(&"Agent".to_owned()));
}

#[tokio::test]
async fn an_interruption_drops_the_running_subagent() {
    let h = Harness::new(vec![tool_reply(
        Some("delegate"),
        &[agent_call("a1")],
        None,
        None,
    )])
    .await;
    let host = SubagentHost::new(Ok("never"), Duration::from_secs(60));
    let mut spec = h.spec();
    spec.hooks = Some(host.clone());
    let session = h.open(spec).await;
    let mut stream = session
        .send_turn(TurnInput::text("delegate the search"))
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        if matches!(&event, AgentEvent::ToolCall { name, .. } if name == "Agent") {
            // Let the host start the sub-agent, then interrupt the turn.
            tokio::time::sleep(Duration::from_millis(100)).await;
            session
                .interrupt(InterruptScope::TurnAndTools)
                .await
                .unwrap();
        }
        let end = matches!(event, AgentEvent::Done { .. } | AgentEvent::Error { .. });
        events.push(event);
        if end {
            break;
        }
    }
    assert_eq!(stop_reason(&events), StopReason::Interrupted);
    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    assert!(results[0].1, "the cut call is an error: {results:?}");
    assert!(host.dropped.load(std::sync::atomic::Ordering::SeqCst));
}

fn question_call(id: &'static str) -> (&'static str, &'static str, Value) {
    (
        id,
        "AskUserQuestion",
        json!({"questions": [{"question": "Which database?", "header": "DB",
            "options": [{"label": "Postgres", "description": "the server"}, {"label": "SQLite"}]}]}),
    )
}

#[tokio::test]
async fn ask_user_question_puts_the_question_to_the_host_ends_the_turn_and_the_answer_is_the_next_turn()
 {
    let h = Harness::with(
        vec![
            tool_reply(Some("pick a database"), &[question_call("q1")], None, None),
            text_reply(Some("Postgres please"), "going with Postgres", None, None),
        ],
        |config| config.ask_user_question = true,
    )
    .await;
    assert!(
        h.provider
            .refresh_capabilities("m")
            .await
            .unwrap()
            .native_question,
        "a model with tools has the harness's question"
    );
    let session = h.open(h.spec()).await;
    let events = turn(&*session, "pick a database").await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    let (question_id, reply, questions, tool_call_id) = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Question {
                question_id,
                reply,
                questions,
                tool_call_id,
                ..
            } => Some((
                question_id.clone(),
                *reply,
                questions.clone(),
                tool_call_id.clone(),
            )),
            _ => None,
        })
        .expect("the question reaches the host");
    assert_eq!(reply, nexus_claude::agent::QuestionReply::Turn);
    assert_eq!(tool_call_id.as_deref(), Some("q1"));
    assert_eq!(questions[0].question, "Which database?");
    assert_eq!(questions[0].options.len(), 2);
    // Answered by a turn, never by a call.
    let refused = session
        .answer_question(&question_id, nexus_claude::agent::QuestionAnswer::Cancelled)
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, ProviderError::Unsupported { capability } if capability == "answer_question"),
        "{refused:?}"
    );
    // The turn ended on the question: one request only, the model did not go on alone.
    assert_eq!(h.chat().len(), 1);
    let (_, is_error, said) = &tool_results(&events)[0];
    assert!(!is_error && said.contains("next user message"));
    // The answer is the next turn, after the question and its result in the history.
    let events = turn(&*session, "Postgres please").await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    let chat = h.chat();
    assert_eq!(chat.len(), 2);
    let messages = chat[1]["messages"].as_array().unwrap();
    let roles: Vec<&str> = messages.iter().filter_map(|m| m["role"].as_str()).collect();
    assert_eq!(roles, vec!["user", "assistant", "tool", "user"]);
    assert!(offered_tools(&chat[0]).contains(&"AskUserQuestion".to_owned()));
}

#[tokio::test]
async fn ask_user_question_is_not_offered_unless_the_host_asks_for_it() {
    let h = Harness::new(vec![text_reply(Some("hello"), "hi", None, None)]).await;
    assert!(
        !h.provider
            .refresh_capabilities("m")
            .await
            .unwrap()
            .native_question
    );
    let session = h.open(h.spec()).await;
    turn(&*session, "hello").await;
    assert!(!offered_tools(&h.chat()[0]).contains(&"AskUserQuestion".to_owned()));
    // And a question call without the tool is refused, without any question event.
    let refused = session
        .answer_question("q", nexus_claude::agent::QuestionAnswer::Cancelled)
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, ProviderError::Unsupported { capability } if capability == "native_question")
    );
}

#[tokio::test]
async fn a_question_without_questions_is_an_error_result_and_the_turn_goes_on() {
    let h = Harness::with(
        vec![
            tool_reply(
                Some("pick"),
                &[("q1", "AskUserQuestion", json!({"questions": []}))],
                None,
                None,
            ),
            text_reply(Some(TOOL), "asked nothing", None, None),
        ],
        |config| config.ask_user_question = true,
    )
    .await;
    let session = h.open(h.spec()).await;
    let events = turn(&*session, "pick").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Question { .. }))
    );
    assert!(tool_results(&events)[0].1);
    assert_eq!(h.chat().len(), 2, "the turn went on to a second request");
}

fn typed_agent_call(id: &'static str, kind: Option<&str>) -> (&'static str, &'static str, Value) {
    let mut input = json!({"description": "look", "prompt": "Look around"});
    if let Some(kind) = kind {
        input["subagent_type"] = json!(kind);
    }
    (id, "Agent", input)
}

#[tokio::test]
async fn a_deny_with_an_argument_refuses_that_subagent_type_and_fails_closed_without_one() {
    let h = Harness::new(vec![
        tool_reply(
            Some("delegate"),
            &[
                typed_agent_call("a1", Some("Explore")),
                typed_agent_call("a3", None),
            ],
            None,
            None,
        ),
        tool_reply(
            Some(TOOL),
            &[typed_agent_call("a2", Some("general-purpose"))],
            None,
            None,
        ),
        text_reply(Some(TOOL), "done", None, None),
    ])
    .await;
    let host = SubagentHost::new(Ok("RAN"), Duration::ZERO);
    let mut spec = h.spec();
    spec.hooks = Some(host.clone());
    spec.policy =
        ToolPolicy::from_patterns(PolicyMode::Ask, &[] as &[&str], &["Agent(Explore)"]).unwrap();
    let session = h.open(spec).await;
    let events = turn(&*session, "delegate").await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    let results = tool_results(&events);
    let of = |id: &str| results.iter().find(|r| r.0 == id).cloned().unwrap();
    assert!(
        of("a1").1 && of("a1").2.contains("denied by policy"),
        "{results:?}"
    );
    assert!(
        of("a3").1,
        "no subagent_type: the deny applies (fail closed): {results:?}"
    );
    assert_eq!(of("a2"), ("a2".to_owned(), false, "RAN".to_owned()));
    let asked: Vec<Option<String>> = host
        .asked
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.subagent_type.clone())
        .collect();
    assert_eq!(asked, vec![Some("general-purpose".to_owned())]);
    // Still offered: a deny with an argument judges calls, it does not hide the tool.
    assert!(offered_tools(&h.chat()[0]).contains(&"Agent".to_owned()));
}

#[tokio::test]
async fn a_bare_deny_hides_the_tool_by_its_name_or_its_alias_and_an_allow_list_does_not() {
    let h = Harness::with(
        vec![
            text_reply(Some("one"), "1", None, None),
            text_reply(Some("two"), "2", None, None),
            text_reply(Some("three"), "3", None, None),
            text_reply(Some("four"), "4", None, None),
        ],
        |config| config.ask_user_question = true,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let open = |deny: &'static [&'static str], allow: &'static [&'static str], mode: PolicyMode| {
        let mut spec = h.spec_with_mcp(&log_path(&dir));
        spec.hooks = Some(SubagentHost::new(Ok("x"), Duration::ZERO));
        spec.policy = ToolPolicy::from_patterns(mode, allow, deny).unwrap();
        spec
    };
    for (spec, text) in [
        (
            open(&["Agent", "AskUserQuestion"], &[], PolicyMode::Ask),
            "one",
        ),
        (open(&["Task"], &[], PolicyMode::Ask), "two"),
        (open(&[], &["mcp__fake__echo"], PolicyMode::Ask), "three"),
        (open(&[], &[], PolicyMode::PlanOnly), "four"),
    ] {
        let session = h.open(spec).await;
        turn(&*session, text).await;
    }
    let chat = h.chat();
    let offered: Vec<Vec<String>> = chat.iter().map(offered_tools).collect();
    let has = |i: usize, tool: &str| offered[i].contains(&tool.to_owned());
    assert!(
        !has(0, "Agent") && !has(0, "AskUserQuestion"),
        "{:?}",
        offered[0]
    );
    assert!(
        !has(1, "Agent") && has(1, "AskUserQuestion"),
        "{:?}",
        offered[1]
    );
    // The allow-list exposes only echo of the MCP tools, and keeps the harness's own.
    assert!(
        has(2, "Agent") && has(2, "AskUserQuestion"),
        "{:?}",
        offered[2]
    );
    assert!(!has(2, "mcp__fake__write"));
    assert!(
        has(3, "Agent") && has(3, "AskUserQuestion"),
        "{:?}",
        offered[3]
    );
}

#[tokio::test]
async fn a_before_tool_deny_means_no_question_and_no_subagent() {
    let h = Harness::with(
        vec![
            tool_reply(
                Some("go"),
                &[question_call("q1"), agent_call("a1")],
                None,
                None,
            ),
            text_reply(Some(TOOL), "went on", None, None),
        ],
        |config| config.ask_user_question = true,
    )
    .await;
    let host = SubagentHost::denying();
    let mut spec = h.spec();
    spec.hooks = Some(host.clone());
    let session = h.open(spec).await;
    let events = turn(&*session, "go").await;
    assert_eq!(stop_reason(&events), StopReason::Completed);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Question { .. }))
    );
    assert!(host.asked.lock().unwrap().is_empty());
    let results = tool_results(&events);
    assert!(
        results
            .iter()
            .all(|r| r.1 && r.2.contains("DENIED-BY-HOOK")),
        "{results:?}"
    );
    // No question was asked, so the turn did not end on it.
    assert_eq!(h.chat().len(), 2);
}
