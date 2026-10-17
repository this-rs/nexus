//! Context economy of the native harness: what a turn costs in tokens and in
//! cache, and whether the request a turn sends keeps the previous one as an
//! exact prefix — the condition of the automatic prompt cache of OpenAI-compatible
//! servers (OpenAI, DeepSeek, vLLM prefix caching), which needs no `cache_control`
//! marker but breaks at the first byte that differs.
//!
//! Four things, all offline (scripted `fake_openai`, real `fake_mcp`):
//!
//! 1. the cache reads an endpoint reports reach the `done` usage and its cost;
//! 2. over a multi-turn session with tool calls and a resume, every request
//!    starts with the previous one: same tool list in the same order, same
//!    system prompt, the history append-only. Two rewrites are declared: a
//!    compaction and a stub pass of old tool results (CTX3). The check itself is
//!    proven to catch a timestamp in the system prompt;
//! 3. the cheap tier measured where it matters: 40 tool rounds in ONE turn, under
//!    a window they would blow through without it, with no summary and a bounded
//!    number of cache breaks — and the control that says the measure is not empty
//!    (the same session, tier off, does reach the summary);
//! 4. a reference benchmark (`#[ignore]`), ten turns on PO-like tools, that
//!    prints per turn what is sent and how much of it a prefix cache could serve:
//!
//! ```text
//! cargo test -p nexus-claude --features provider-native --test native_context \
//!     -- --ignored --nocapture reference_benchmark
//! ```
//!
//! The token figures of the benchmark are an ESTIMATE, deterministic: the bytes of
//! the canonical JSON of what is sent, divided by 4. They compare runs of this
//! benchmark with each other (before / after a context change), not with an
//! invoice.

#[path = "support/fake_openai.rs"]
mod fake_openai;
#[path = "support/native.rs"]
mod native;

use std::sync::Arc;

use fake_openai::FakeOpenAi;
use native::*;
use nexus_claude::agent::{
    AgentEvent, AgentProvider, AgentSession, CostBasis, ModelPrice, PolicyMode, SessionSpec,
    StopReason, SystemPromptMode, SystemPromptSpec, ToolPolicy, TurnInput,
};
use nexus_claude::model::{EndpointQuirks, PriceTable};
use nexus_claude::providers::native::compaction::STUB_MARKER;
use nexus_claude::providers::native::{
    FileTranscriptStore, MemoryTranscriptStore, NativeConfig, NativeProvider, TranscriptStore,
};
use serde_json::{Value, json};

/// Bytes per token of the estimate.
const BYTES_PER_TOKEN: usize = 4;

/// Every scripted answer is matched on this, which the system prompt carries:
/// a capability probe (its own prompt) never consumes one.
const MARK: &str = "CTX-BENCH";

const SYSTEM: &str = "You are the CTX-BENCH agent of a project orchestrator. You plan, track \
and record work through the project tools: plans hold tasks, tasks hold steps, notes hold \
knowledge, decisions record why. Read before you write, keep answers short, cite the ids you \
touched.";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    server: FakeOpenAi,
    provider: Arc<NativeProvider>,
    cwd: tempfile::TempDir,
    tools_file: tempfile::NamedTempFile,
}

impl Harness {
    async fn new(routes: Vec<Value>, tweak: impl FnOnce(&mut NativeConfig)) -> Self {
        Self::with_store(routes, tweak, Arc::new(MemoryTranscriptStore::new())).await
    }

    async fn with_store(
        routes: Vec<Value>,
        tweak: impl FnOnce(&mut NativeConfig),
        store: Arc<dyn TranscriptStore>,
    ) -> Self {
        let mut all = vec![probe_route(true), models_route(128_000)];
        all.extend(routes);
        let server = FakeOpenAi::start(json!(all));
        let mut config = NativeConfig::new("native-test");
        config.default_model = Some("m".to_owned());
        tweak(&mut config);
        let provider = Arc::new(
            NativeProvider::new(
                config,
                endpoint(server.base_url(), EndpointQuirks::deepseek()),
            )
            .with_transcript_store(store),
        );
        let tools_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tools_file.path(), po_tools().to_string()).unwrap();
        Self {
            server,
            provider,
            cwd: tempfile::tempdir().unwrap(),
            tools_file,
        }
    }

    /// A session on the PO-like tools only (the built-in tools of `fake_mcp` are
    /// not offered), in trust mode, with `system` as its prompt.
    fn spec(&self, system: &str) -> SessionSpec {
        self.spec_with(system, &[])
    }

    /// [`Self::spec`], with the built-in `fake_mcp` tools of `extra` offered too.
    fn spec_with(&self, system: &str, extra: &[&str]) -> SessionSpec {
        let mut spec = SessionSpec::new(self.cwd.path());
        spec.model = Some("m".to_owned());
        spec.system_prompt = Some(SystemPromptSpec {
            text: system.to_owned(),
            mode: SystemPromptMode::Replace,
        });
        let mut server = stdio_mcp(None);
        if let nexus_claude::agent::McpServerSpec::Stdio { env, .. } = &mut server {
            env.insert(
                "FAKE_MCP_EXTRA_TOOLS".to_owned(),
                self.tools_file.path().display().to_string(),
            );
        }
        spec.mcp_servers.insert("po".to_owned(), server);
        let mut allowed: Vec<String> = po_tools()
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| format!("mcp__po__{}", tool["name"].as_str().unwrap()))
            .collect();
        allowed.extend(extra.iter().map(|name| format!("mcp__po__{name}")));
        spec.policy =
            ToolPolicy::from_patterns(PolicyMode::Trust, &allowed, &[] as &[String]).unwrap();
        spec
    }

    /// The chat requests of the turns (probes excluded), as JSON bodies.
    fn chat(&self) -> Vec<Value> {
        self.server
            .requests_to("POST", "/v1/chat/completions")
            .into_iter()
            .map(|r| r["body"].clone())
            .filter(|body| !body.to_string().contains("Call the ping tool now"))
            .collect()
    }
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

fn done_usage(events: &[AgentEvent]) -> (&nexus_claude::agent::Usage, f64, StopReason) {
    match terminal(events) {
        AgentEvent::Done {
            usage,
            cost,
            stop_reason,
            ..
        } => (usage, cost.usd.unwrap_or(f64::NAN), *stop_reason),
        other => panic!("expected done, got {other:?}"),
    }
}

fn chat_route(events: Vec<Value>) -> Value {
    json!({"method": "POST", "path": "/v1/chat/completions", "status": 200,
        "body_contains": MARK, "sse": events})
}

fn call_events(calls: &[(&str, &str, Value)], usage: Option<Value>) -> Vec<Value> {
    let mut events = vec![delta(json!({"reasoning_content": "I need the tools."}))];
    for (index, (id, name, arguments)) in calls.iter().enumerate() {
        events.push(delta(json!({"tool_calls": [{
            "index": index, "id": id, "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()}}]})));
    }
    events.push(finish("tool_calls"));
    events.extend(usage);
    events.push(json!("[DONE]"));
    events
}

fn answer_events(text: &str, usage: Option<Value>) -> Vec<Value> {
    let mut events = vec![delta(json!({"content": text})), finish("stop")];
    events.extend(usage);
    events.push(json!("[DONE]"));
    events
}

// ---------------------------------------------------------------------------
// 1. Usage of a native turn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_cache_reads_reported_by_the_endpoint_reach_the_done_usage_and_its_cost() {
    let h = Harness::new(
        vec![
            // OpenAI form: `prompt_tokens_details.cached_tokens`.
            chat_route(call_events(
                &[(
                    "c1",
                    "mcp__po__task",
                    json!({"action": "get", "task_id": "t1"}),
                )],
                Some(json!({"choices": [], "usage": {
                    "prompt_tokens": 1000, "completion_tokens": 40, "total_tokens": 1040,
                    "prompt_tokens_details": {"cached_tokens": 600}}})),
            )),
            // DeepSeek form: `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`.
            chat_route(answer_events(
                "done",
                Some(json!({"choices": [], "usage": {
                    "prompt_tokens": 1300, "completion_tokens": 10, "total_tokens": 1310,
                    "prompt_cache_hit_tokens": 1000, "prompt_cache_miss_tokens": 300}})),
            )),
        ],
        |config| {
            config.prices = PriceTable::new().with(
                "m",
                ModelPrice {
                    input_per_mtok: 1.0,
                    output_per_mtok: 2.0,
                    cache_read_per_mtok: Some(0.1),
                    cache_write_per_mtok: None,
                },
            );
        },
    )
    .await;
    let session = h.provider.open(h.spec(SYSTEM)).await.expect("open");
    let events = turn(&*session, "look at t1").await;
    let (usage, usd, stop) = done_usage(&events);
    assert_eq!(stop, StopReason::Completed);
    // Input is what the cache did NOT serve; the cache reads are apart.
    assert_eq!(usage.input_tokens, Some(400 + 300));
    assert_eq!(usage.cache_read_tokens, Some(600 + 1000));
    assert_eq!(usage.output_tokens, Some(50));
    // No OpenAI-compatible server reports a cache write: unknown, never a made-up zero.
    assert_eq!(usage.cache_creation_tokens, None);
    // The context after the turn is the last prompt, cached part included.
    assert_eq!(usage.context_tokens, Some(1300));
    let by_model = &usage.by_model[0];
    assert_eq!(
        (by_model.input_tokens, by_model.cache_read_tokens),
        (Some(700), Some(1600))
    );
    // The cache reads are priced at the cache price, not the input price.
    let expected = (700.0 * 1.0 + 50.0 * 2.0 + 1600.0 * 0.1) / 1_000_000.0;
    assert!((usd - expected).abs() < 1e-12, "{usd} != {expected}");
    assert_eq!(by_model.cost_usd, Some(usd));
    let AgentEvent::Done { cost, .. } = terminal(&events) else {
        unreachable!()
    };
    assert_eq!(cost.basis, CostBasis::Priced);
}

// ---------------------------------------------------------------------------
// 2. Prefix stability
// ---------------------------------------------------------------------------

/// The canonical bytes of one element of the cacheable prompt.
fn canon(value: &Value) -> String {
    serde_json::to_string(value).expect("serialisable")
}

/// Why `next` does not start with `prev`, or `None` when it does: the same tool
/// list in the same order, then every message of `prev`, byte for byte (the system
/// prompt is the first of them), and only new messages after.
fn prefix_break(prev: &Value, next: &Value) -> Option<String> {
    if canon(&prev["tools"]) != canon(&next["tools"]) {
        return Some("the tool list changed (content or order)".to_owned());
    }
    let before = prev["messages"].as_array().expect("messages");
    let after = next["messages"].as_array().expect("messages");
    for (index, message) in before.iter().enumerate() {
        match after.get(index) {
            None => return Some(format!("the history shrank at message {index}")),
            Some(other) if canon(other) != canon(message) => {
                return Some(format!(
                    "message {index} ({}) changed:\n  was: {}\n  now: {}",
                    message["role"],
                    canon(message),
                    canon(other)
                ));
            },
            Some(_) => {},
        }
    }
    None
}

fn is_summary_request(body: &Value) -> bool {
    body.get("tools").is_none() && body.to_string().contains("compacting the history")
}

/// How many messages a break of the prefix is allowed to touch **as a stub pass**
/// (CTX3), or `None` when this break is not one.
///
/// The stub tier rewrites the text of old tool results, on purpose and under
/// pressure: the message list keeps its length, its roles and its `tool_call_id`s,
/// and the ONLY difference a break may carry is a `tool` message whose content became
/// [`STUB_MARKER`]. Anything else is a real prefix break.
fn stub_break(prev: &Value, next: &Value) -> Option<usize> {
    if canon(&prev["tools"]) != canon(&next["tools"]) {
        return None;
    }
    let before = prev["messages"].as_array()?;
    let after = next["messages"].as_array()?;
    // A stub pass never removes nor reorders a message: it only shortens one.
    if after.len() < before.len() {
        return None;
    }
    let mut stubbed = 0;
    for (index, message) in before.iter().enumerate() {
        let other = after.get(index)?;
        if canon(other) == canon(message) {
            continue;
        }
        let is_tool = message["role"] == "tool" && other["role"] == "tool";
        let became_stub = other["content"].as_str() == Some(STUB_MARKER);
        let was_whole = message["content"].as_str() != Some(STUB_MARKER);
        // Everything but the content must be identical, ids included.
        let mut rest_before = message.clone();
        let mut rest_after = other.clone();
        rest_before["content"] = Value::Null;
        rest_after["content"] = Value::Null;
        if !is_tool || !became_stub || !was_whole || canon(&rest_before) != canon(&rest_after) {
            return None;
        }
        stubbed += 1;
    }
    (stubbed > 0).then_some(stubbed)
}

/// Asserts that every request starts with the one before it, a declared rewrite of the
/// history aside: a compaction (the summary request is not part of the conversation, and
/// the request that follows it starts a new prefix) or a stub pass (old tool results
/// only). A stub pass is counted and returned, so a test can say how many it cost.
fn assert_prefix_stable(requests: &[Value]) -> usize {
    let mut previous: Option<&Value> = None;
    let mut compacted = false;
    let mut passes = 0;
    for (index, request) in requests.iter().enumerate() {
        if is_summary_request(request) {
            compacted = true;
            continue;
        }
        if let Some(prev) = previous
            && !std::mem::take(&mut compacted)
            && let Some(why) = prefix_break(prev, request)
        {
            if stub_break(prev, request).is_some() {
                passes += 1;
            } else {
                panic!(
                    "prefix broken between request {} and {index}: {why}",
                    index - 1
                );
            }
        }
        previous = Some(request);
    }
    passes
}

/// A session of four turns with tool calls, a resume between turns two and
/// three (a backend reopens a session that way), and the system prompt each
/// opening is given by `system_of(opening)`.
async fn multi_turn_requests(system_of: impl Fn(usize) -> String) -> Vec<Value> {
    let task = (
        "c1",
        "mcp__po__task",
        json!({"action": "get", "task_id": "t1"}),
    );
    let step = (
        "c2",
        "mcp__po__step",
        json!({"action": "create", "task_id": "t1"}),
    );
    let note = (
        "c3",
        "mcp__po__note",
        json!({"action": "search", "query": "cache"}),
    );
    let h = Harness::new(
        vec![
            chat_route(call_events(std::slice::from_ref(&task), None)),
            chat_route(answer_events("t1 is about caching.", None)),
            chat_route(call_events(&[step, note], None)),
            chat_route(answer_events("Step created, two notes found.", None)),
            chat_route(answer_events("Nothing else to do.", None)),
            chat_route(call_events(&[task], None)),
            chat_route(answer_events("Still pending.", None)),
        ],
        |_| {},
    )
    .await;
    let session = h.provider.open(h.spec(&system_of(0))).await.expect("open");
    for text in ["look at t1", "add a step and find notes"] {
        let events = turn(&*session, text).await;
        assert_eq!(done_usage(&events).2, StopReason::Completed, "{events:?}");
    }
    let token = session.resume_token().expect("a token");
    session.close().await.unwrap();
    let session = h
        .provider
        .resume(h.spec(&system_of(1)), token)
        .await
        .expect("resume");
    for text in ["anything else?", "and t1 now?"] {
        let events = turn(&*session, text).await;
        assert_eq!(done_usage(&events).2, StopReason::Completed, "{events:?}");
    }
    let requests = h.chat();
    assert_eq!(requests.len(), 7);
    requests
}

#[tokio::test]
async fn every_request_starts_with_the_previous_one_across_turns_tools_and_a_resume() {
    let requests = multi_turn_requests(|_| SYSTEM.to_owned()).await;
    assert_prefix_stable(&requests);
    // And there is something to keep: the system prompt, then a growing history.
    assert_eq!(requests[0]["messages"][0]["role"], "system");
    assert!(requests[0]["tools"].as_array().unwrap().len() >= 10);
    let lengths: Vec<usize> = requests
        .iter()
        .map(|r| r["messages"].as_array().unwrap().len())
        .collect();
    assert!(lengths.windows(2).all(|w| w[0] < w[1]), "{lengths:?}");
}

/// The check is not vacuous: a system prompt that carries the time of the
/// opening (the classic `Current date: …` line) is caught at the resume.
#[tokio::test]
#[should_panic(expected = "prefix broken between request 3 and 4: message 0 (\"system\") changed")]
async fn the_prefix_check_fails_when_a_timestamp_is_injected_into_the_system_prompt() {
    let requests = multi_turn_requests(|opening| {
        format!(
            "{SYSTEM}\nCurrent time: 2026-10-12T09:{:02}:00Z",
            15 + opening
        )
    })
    .await;
    assert_prefix_stable(&requests);
}

/// Compaction rewrites the history on purpose: the one request after it starts
/// a new prefix, and the check says so only there.
#[tokio::test]
async fn compaction_is_the_declared_exception_and_the_prefix_holds_again_after_it() {
    let usage = |prompt: u64| {
        json!({"choices": [], "usage": {
        "prompt_tokens": prompt, "completion_tokens": 10, "total_tokens": prompt + 10}})
    };
    let h = Harness::new(
        vec![
            chat_route(call_events(
                &[("c1", "mcp__po__task", json!({"action": "get"}))],
                Some(usage(100)),
            )),
            chat_route(call_events(
                &[("c2", "mcp__po__note", json!({"action": "search"}))],
                Some(usage(10_000)),
            )),
            json!({"method": "POST", "path": "/v1/chat/completions", "status": 200,
                "body_contains": "compacting the history",
                "sse": answer_events("THE SUMMARY", Some(usage(50)))}),
            chat_route(answer_events("done", Some(usage(300)))),
            chat_route(answer_events("again", Some(usage(400)))),
        ],
        |config| {
            config.context_window = Some(12_000);
            config.compaction.keep_recent = 2;
        },
    )
    .await;
    let session = h.provider.open(h.spec(SYSTEM)).await.expect("open");
    turn(&*session, "go").await;
    turn(&*session, "once more").await;
    let requests = h.chat();
    assert_eq!(requests.len(), 5);
    assert!(is_summary_request(&requests[2]));
    // The compaction did break the prefix (the exception is real, not assumed) …
    assert!(prefix_break(&requests[1], &requests[3]).is_some());
    // … and the check, which allows it, holds everywhere else.
    assert_prefix_stable(&requests);
    assert!(prefix_break(&requests[3], &requests[4]).is_none());
}

// ---------------------------------------------------------------------------
// 2b. The cheap tier: old tool results become a stub (CTX3)
// ---------------------------------------------------------------------------

/// Rough size of a request's prompt in tokens, the way the harness estimates it: four
/// characters per token over what the model reads (system prompt, text, reasoning).
fn prompt_tokens(body: &Value) -> u64 {
    let mut chars = 0;
    for message in body["messages"].as_array().expect("messages") {
        for key in ["content", "reasoning_content"] {
            chars += message[key].as_str().map_or(0, str::len);
        }
    }
    (chars as u64).div_ceil(BYTES_PER_TOKEN as u64)
}

/// One turn of `rounds` sequential tool rounds, each result `result_chars` long (the
/// `big` tool: short arguments, big result — what a `Read` of a file looks like), then
/// the final answer.
///
/// The summary route comes last and matches nothing but a summary request: a request
/// count of `rounds + 1` therefore says that no summary was asked for.
async fn long_session(
    rounds: usize,
    result_chars: usize,
    tweak: impl FnOnce(&mut NativeConfig),
) -> (Vec<Value>, Vec<AgentEvent>) {
    let mut routes = Vec::new();
    for round in 0..rounds {
        let id = format!("c{round}");
        let calls: Vec<(&str, &str, Value)> =
            vec![(id.as_str(), "mcp__po__big", json!({ "n": result_chars }))];
        routes.push(chat_route(call_events(&calls, None)));
    }
    routes.push(chat_route(answer_events("done", None)));
    routes.push(
        json!({"method": "POST", "path": "/v1/chat/completions", "status": 200,
        "body_contains": "compacting the history",
        "sse": answer_events("THE SUMMARY", None)}),
    );
    let h = Harness::new(routes, tweak).await;
    let session = h
        .provider
        .open(h.spec_with(SYSTEM, &["big"]))
        .await
        .expect("open");
    let events = turn(&*session, "read the file in pieces").await;
    (h.chat(), events)
}

/// The measured session: 40 rounds of tool results in ONE turn, a window it would blow
/// through without the cheap tier (`0.8 × 8000` tokens).
const LONG_ROUNDS: usize = 40;
const LONG_WINDOW: u64 = 8_000;

#[tokio::test]
async fn forty_tool_rounds_stay_under_the_threshold_without_a_full_compaction() {
    let (requests, events) = long_session(LONG_ROUNDS, 800, |config| {
        config.context_window = Some(LONG_WINDOW);
    })
    .await;
    assert_eq!(done_usage(&events).2, StopReason::Completed, "{events:?}");
    // One request per round, then the answer: no summary was ever asked for.
    assert_eq!(requests.len(), LONG_ROUNDS + 1);
    assert!(
        requests.iter().all(|request| !is_summary_request(request)),
        "a full compaction ran although the cheap tier was enough"
    );

    // The prompt never comes near the threshold, round trip after round trip.
    let threshold = (LONG_WINDOW as f64 * 0.8) as u64;
    let sizes: Vec<u64> = requests.iter().map(prompt_tokens).collect();
    assert!(
        sizes.iter().all(|size| *size < threshold),
        "the prompt reached the threshold: {sizes:?}"
    );

    // What it DOES pay: a bounded number of cache breaks, each one a declared stub pass
    // — the CTX0 measure, over the same canonical bytes the prefix check uses.
    let passes = assert_prefix_stable(&requests);
    assert!(passes >= 1, "the cheap tier never ran: nothing to measure");
    // Never more than one pass per block of aged-out results (the default block).
    let block = nexus_claude::providers::native::compaction::CompactionConfig::default().stub_block;
    assert!(
        passes <= LONG_ROUNDS / block,
        "{passes} cache breaks for {LONG_ROUNDS} rounds, a block of {block}"
    );

    // The stubs really reach the model, and the old results really left the prompt.
    let last = canon(requests.last().expect("a last request"));
    assert!(last.contains(STUB_MARKER), "no stub in the last request");
    let elided = last.matches(STUB_MARKER).count();
    assert!(elided >= 4, "{elided} stubs, expected a whole block");
}

/// The control the test above is worth nothing without: the SAME session, with the
/// cheap tier off, reaches the summary — so the figures above really do measure the
/// tier and not a window nobody would have hit.
#[tokio::test]
async fn without_the_cheap_tier_the_same_session_reaches_the_full_compaction() {
    let (requests, events) = long_session(LONG_ROUNDS, 800, |config| {
        config.context_window = Some(LONG_WINDOW);
        config.compaction.keep_tool_rounds = 0;
    })
    .await;
    assert_eq!(done_usage(&events).2, StopReason::Completed, "{events:?}");
    assert!(
        requests.iter().any(is_summary_request),
        "the session never compacted: the 40-round window is not the one measured above"
    );
    assert!(!canon(&requests[requests.len() - 1]).contains(STUB_MARKER));
}

/// The cheap tier is a step, never a substitute: 120 rounds, and a window the tier can
/// no longer relieve — the kept rounds plus the stubs themselves fill it — still ends
/// in a summary. A pass never silences the tier below it.
#[tokio::test]
async fn a_window_the_cheap_tier_cannot_relieve_still_compacts() {
    let (requests, events) = long_session(120, 1_000, |config| {
        config.context_window = Some(5_000);
        // A turn of 120 round trips: the default bound of 50 would stop it first.
        config.max_turns = Some(200);
    })
    .await;
    assert_eq!(done_usage(&events).2, StopReason::Completed, "{events:?}");
    assert!(
        requests.iter().any(is_summary_request),
        "the prompt was never summarised although the tier cannot relieve it"
    );
    assert!(assert_prefix_stable(&requests) >= 1, "the tier never ran");
}

/// The request of the second turn of a session whose first turn called `echo`
/// with `text` (DeepSeek reasoning on the call): run live, or with the session
/// closed and resumed from a FILE transcript between the turns, as a host does
/// when it reopens a conversation.
async fn second_turn_request(resume: bool, text: &str) -> Value {
    let dir = tempfile::tempdir().unwrap();
    let h = Harness::with_store(
        vec![
            chat_route(call_events(
                &[("c1", "mcp__po__echo", json!({ "text": text }))],
                None,
            )),
            chat_route(answer_events("echoed", None)),
            chat_route(answer_events("again", None)),
        ],
        |_| {},
        Arc::new(FileTranscriptStore::new(dir.path())),
    )
    .await;
    let spec = || h.spec_with(SYSTEM, &["echo"]);
    let session = h.provider.open(spec()).await.expect("open");
    turn(&*session, "echo it").await;
    let session = if resume {
        let token = session.resume_token().expect("a token");
        session.close().await.unwrap();
        h.provider.resume(spec(), token).await.expect("resume")
    } else {
        session
    };
    turn(&*session, "once more").await;
    let requests = h.chat();
    assert_eq!(requests.len(), 3);
    requests[2].clone()
}

/// The first message where the resumed request departs from the live one.
fn first_difference(live: &Value, resumed: &Value) -> Option<(usize, String, String)> {
    let live = live["messages"].as_array().unwrap();
    let resumed = resumed["messages"].as_array().unwrap();
    live.iter()
        .zip(resumed)
        .enumerate()
        .find(|(_, (a, b))| canon(a) != canon(b))
        .map(|(index, (a, b))| (index, canon(a), canon(b)))
}

/// A plain history resumes byte for byte, reasoning of the tool call included:
/// a reopened session can be served from the cache the live one filled.
#[tokio::test]
async fn a_resumed_session_sends_the_same_request_as_the_live_one() {
    let live = second_turn_request(false, "a plain sentence").await;
    let resumed = second_turn_request(true, "a plain sentence").await;
    assert_eq!(first_difference(&live, &resumed), None);
    assert_eq!(canon(&live), canon(&resumed));
    assert!(
        canon(&resumed).contains("I need the tools."),
        "reasoning kept"
    );
}

/// The file transcript is the conversation (decision CTX6): what only LOOKS like a
/// credential — a field NAMED like one (`input_tokens`, `max_tokens`, `api_key`), a
/// word over 400 bytes, compact JSON included, in a tool-call argument or a tool
/// result — is replayed byte for byte, so a reopened session is served from the
/// cache the live one filled and every argument is still the JSON the model wrote.
/// (Before CTX6 this test pinned the divergence: `<redacted` from message 2 on.)
#[tokio::test]
async fn a_resumed_history_equals_the_live_one_even_where_the_text_looks_like_a_secret() {
    let long_json = json!({
        "api_key": "not-a-key",
        "max_tokens": 4096,
        "rows": (0..40).map(|i| format!("row-{i:03}")).collect::<Vec<_>>(),
    })
    .to_string();
    assert!(long_json.len() > 400, "{}", long_json.len());
    for text in [
        r#"{"input_tokens":12,"output_tokens":3}"#.to_owned(),
        format!("/{}", "segment/".repeat(60)),
        long_json,
    ] {
        let live = second_turn_request(false, &text).await;
        let resumed = second_turn_request(true, &text).await;
        assert_eq!(first_difference(&live, &resumed), None, "{text}");
        assert_eq!(canon(&live), canon(&resumed), "{text}");
        assert!(!canon(&resumed).contains("<redacted"), "{text}");
        // The replayed argument is still the JSON object the model sent.
        let arguments = resumed["messages"][2]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments");
        assert_eq!(
            serde_json::from_str::<Value>(arguments).expect("valid JSON"),
            json!({ "text": text })
        );
    }
}

// ---------------------------------------------------------------------------
// Real API (DeepSeek), by hand
// ---------------------------------------------------------------------------

/// A `MakeWriter` that appends every formatted event to a shared buffer.
#[derive(Clone, Default)]
struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);

impl CapturedLog {
    fn lines(&self, needle: &str) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|line| line.contains(needle))
            .map(str::to_owned)
            .collect()
    }
}

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for CapturedLog {
    type Writer = Self;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

fn log_field(line: &str, name: &str) -> Option<String> {
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(&format!("{name}=")).map(str::to_owned))
}

/// On the real DeepSeek API: a tool turn then a follow-up, and EVERY request after
/// the first is served in part from DeepSeek's automatic prefix cache (it caches in
/// blocks of 64 tokens: the system prompt and the tools are far above that). Read
/// from the per-request log line (`cached_tokens`). Needs `DEEPSEEK_API_KEY`;
/// `DEEPSEEK_MODEL` (default `deepseek-v4-flash`) and `DEEPSEEK_BASE_URL` (default
/// `https://api.deepseek.com/v1`) are optional. Costs a few cents.
#[tokio::test]
#[ignore = "real DeepSeek API: needs DEEPSEEK_API_KEY, run by hand"]
async fn deepseek_serves_every_request_after_the_first_from_its_prefix_cache() {
    use nexus_claude::agent::{CredentialRef, EnvCredentialResolver};
    use nexus_claude::model::{OpenAiEndpoint, OpenAiEndpointConfig};

    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|key| !key.is_empty()),
        "DEEPSEEK_API_KEY is not set"
    );
    let model = std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".into());
    let base =
        std::env::var("DEEPSEEK_BASE_URL").unwrap_or_else(|_| "https://api.deepseek.com/v1".into());
    let mut config = OpenAiEndpointConfig::new("deepseek-real", base);
    config.credential = CredentialRef::Env("DEEPSEEK_API_KEY".into());
    config.quirks = EndpointQuirks::deepseek();
    config.response_timeout = std::time::Duration::from_secs(180);
    config.idle_timeout = std::time::Duration::from_secs(180);
    let endpoint = Arc::new(OpenAiEndpoint::new(config, Arc::new(EnvCredentialResolver)));
    let mut native = NativeConfig::new("deepseek-real");
    native.default_model = Some(model.clone());
    native.max_tokens = Some(2048);
    let provider = NativeProvider::new(native, endpoint);

    // A harness only for its PO-like tools file and its spec; nothing is sent to its fake.
    let h = Harness::new(vec![], |_| {}).await;
    let mut spec = h.spec(SYSTEM);
    spec.model = Some(model.clone());

    let logs = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .without_time()
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let session = provider.open(spec).await.expect("open on DeepSeek");
    let first = turn(
        &*session,
        "Call the task tool once with action get and task_id t1, then answer in one short sentence.",
    )
    .await;
    assert!(
        first
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolCall { .. })),
        "the model did not call the tool: {first:?}"
    );
    let second = turn(&*session, "Thanks. Now answer with the single word: done.").await;
    for events in [&first, &second] {
        let (usage, _, stop) = done_usage(events);
        assert_eq!(stop, StopReason::Completed, "{events:?}");
        println!("turn usage: {usage:?}");
    }

    let lines = logs.lines("native model request");
    for line in &lines {
        println!("{line}");
    }
    assert!(lines.len() >= 3, "a tool turn and a follow-up: {lines:?}");
    for line in &lines[1..] {
        let cached: u64 = log_field(line, "cached_tokens")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        assert!(
            cached > 0,
            "a request after the first missed the cache: {line}"
        );
    }
    let hashes: std::collections::BTreeSet<_> = lines
        .iter()
        .map(|line| {
            (
                log_field(line, "system_hash"),
                log_field(line, "tools_hash"),
            )
        })
        .collect();
    assert_eq!(hashes.len(), 1, "the prefix fingerprint moved: {lines:?}");
}

/// On the real DeepSeek API: a reasoned tool turn, then a PLAN-ONLY turn (no
/// tools). The earlier assistant message — reasoning included — must be re-sent
/// verbatim on the tool-less request: `extends_previous=true` proves the history
/// did not change under the cache. The tool-less request must also be ACCEPTED
/// (a completed turn, not a 400): DeepSeek takes `reasoning_content` back with or
/// without tools. Needs `DEEPSEEK_API_KEY`; `DEEPSEEK_MODEL` (default
/// `deepseek-v4-flash`) and `DEEPSEEK_BASE_URL` (default `https://api.deepseek.com/v1`)
/// are optional. Costs a few cents.
#[tokio::test]
#[ignore = "real DeepSeek API: needs DEEPSEEK_API_KEY, run by hand"]
async fn deepseek_accepts_reasoning_back_on_a_tool_less_turn_after_a_reasoned_one() {
    use nexus_claude::agent::{CredentialRef, EnvCredentialResolver};
    use nexus_claude::model::{OpenAiEndpoint, OpenAiEndpointConfig};

    assert!(
        std::env::var("DEEPSEEK_API_KEY").is_ok_and(|key| !key.is_empty()),
        "DEEPSEEK_API_KEY is not set"
    );
    let model = std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".into());
    let base =
        std::env::var("DEEPSEEK_BASE_URL").unwrap_or_else(|_| "https://api.deepseek.com/v1".into());
    let mut config = OpenAiEndpointConfig::new("deepseek-real", base);
    config.credential = CredentialRef::Env("DEEPSEEK_API_KEY".into());
    config.quirks = EndpointQuirks::deepseek();
    config.response_timeout = std::time::Duration::from_secs(180);
    config.idle_timeout = std::time::Duration::from_secs(180);
    let endpoint = Arc::new(OpenAiEndpoint::new(config, Arc::new(EnvCredentialResolver)));
    let mut native = NativeConfig::new("deepseek-real");
    native.default_model = Some(model.clone());
    native.max_tokens = Some(2048);
    let provider = NativeProvider::new(native, endpoint);

    // A harness only for its PO-like tools file and its spec; nothing is sent to its fake.
    let h = Harness::new(vec![], |_| {}).await;
    let mut spec = h.spec(SYSTEM);
    spec.model = Some(model.clone());

    let logs = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .without_time()
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let session = provider.open(spec).await.expect("open on DeepSeek");

    // A reasoned tool turn: the model thinks, calls the task tool, then answers.
    let first = turn(
        &*session,
        "Call the task tool once with action get and task_id t1, then answer in one short sentence.",
    )
    .await;
    assert!(
        first
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolCall { .. })),
        "the model did not call the tool: {first:?}"
    );
    let (usage, _, _) = done_usage(&first);
    assert!(
        usage.reasoning_tokens.is_some_and(|tokens| tokens > 0),
        "the tool turn did not reason: {usage:?}"
    );

    // A plan-only turn: the policy leaves no tool. DeepSeek accepts reasoning back
    // with or without tools, so the earlier assistant message must be re-sent
    // verbatim and the history still extends the previous request.
    session
        .set_policy_mode(PolicyMode::PlanOnly, None)
        .await
        .expect("plan only");
    let second = turn(&*session, "Thanks. Now answer with the single word: done.").await;
    let (_, _, stop) = done_usage(&second);
    assert_eq!(stop, StopReason::Completed, "{second:?}");

    let lines = logs.lines("native model request");
    for line in &lines {
        println!("{line}");
    }
    assert!(
        lines.len() >= 3,
        "a tool turn and a plan-only follow-up: {lines:?}"
    );
    let plan_only = lines.last().expect("the plan-only request");
    assert!(plan_only.contains("tools=0"), "{plan_only}");
    assert!(plan_only.contains("extends_previous=true"), "{plan_only}");
}

// ---------------------------------------------------------------------------
// 4. Reference benchmark
// ---------------------------------------------------------------------------

fn tokens(bytes: usize) -> usize {
    bytes.div_ceil(BYTES_PER_TOKEN)
}

/// A filler of `bytes` bytes, deterministic, word-like (a tool result).
fn filler(seed: &str, bytes: usize) -> String {
    let words = [
        "task",
        "step",
        "plan",
        "note",
        "pending",
        "completed",
        "cache",
        "prefix",
        "turn",
        "decision",
        "commit",
        "context",
    ];
    let mut text = format!("{seed}:");
    let mut i = seed.len();
    while text.len() < bytes {
        text.push(' ');
        text.push_str(words[i % words.len()]);
        i = i.wrapping_mul(31).wrapping_add(7);
    }
    text.truncate(bytes);
    text
}

/// Twelve mega-tools shaped like the project orchestrator's MCP surface: an
/// `action` enum, a dozen documented parameters each. Deterministic.
fn po_tools() -> Value {
    let tools: [(&str, &[&str]); 12] = [
        (
            "project",
            &["list", "get", "create", "update", "sync", "get_roadmap"],
        ),
        (
            "plan",
            &[
                "list",
                "get",
                "create",
                "update_status",
                "get_dependency_graph",
                "get_critical_path",
            ],
        ),
        (
            "task",
            &[
                "list",
                "get",
                "create",
                "update",
                "get_next",
                "get_context",
                "get_blockers",
            ],
        ),
        (
            "step",
            &["list", "create", "update", "get", "delete", "get_progress"],
        ),
        (
            "note",
            &[
                "list",
                "get",
                "create",
                "update",
                "search",
                "search_semantic",
                "link_to_entity",
            ],
        ),
        (
            "decision",
            &[
                "add",
                "get",
                "update",
                "search",
                "search_semantic",
                "get_affecting",
            ],
        ),
        ("constraint", &["list", "add", "get", "update", "delete"]),
        (
            "commit",
            &[
                "create",
                "link_to_task",
                "link_to_plan",
                "get_task_commits",
                "get_file_history",
            ],
        ),
        (
            "code",
            &[
                "search",
                "get_file_symbols",
                "find_references",
                "get_call_graph",
                "analyze_impact",
            ],
        ),
        (
            "workspace",
            &[
                "list",
                "get",
                "get_overview",
                "list_projects",
                "add_project",
            ],
        ),
        (
            "milestone",
            &[
                "list",
                "get",
                "create",
                "update",
                "get_progress",
                "add_task",
            ],
        ),
        (
            "skill",
            &["list", "get", "create", "activate", "get_members"],
        ),
    ];
    let params = [
        ("project_id", "string", "Project UUID"),
        ("plan_id", "string", "Plan UUID"),
        ("task_id", "string", "Task UUID"),
        ("step_id", "string", "Step UUID"),
        ("title", "string", "Title (create)"),
        (
            "description",
            "string",
            "Free text description (create/update)",
        ),
        (
            "status",
            "string",
            "Status: pending, in_progress, blocked, completed, failed",
        ),
        ("query", "string", "Search query (search actions)"),
        ("limit", "integer", "Maximum number of items returned"),
        (
            "offset",
            "integer",
            "Items skipped before the first one returned",
        ),
        ("tags", "array", "Tags attached to the entity"),
        ("priority", "integer", "Priority, higher runs first"),
    ];
    let list: Vec<Value> = tools
        .iter()
        .map(|(name, actions)| {
            let mut properties = serde_json::Map::new();
            properties.insert(
                "action".to_owned(),
                json!({"type": "string", "enum": actions, "description": "Operation to perform"}),
            );
            for (param, kind, about) in params {
                let mut schema = json!({"type": kind, "description": about});
                if kind == "array" {
                    schema["items"] = json!({"type": "string"});
                }
                properties.insert(param.to_owned(), schema);
            }
            json!({
                "name": name,
                "description": format!(
                    "Manage {name}s of the project orchestrator. Actions: {}.",
                    actions.join(", ")
                ),
                "inputSchema": {"type": "object", "properties": properties, "required": ["action"]},
                "annotations": {"readOnlyHint": false},
                "reply": filler(name, 1_800),
            })
        })
        .collect();
    Value::Array(list)
}

/// The ten turns: `(user text, rounds of tool calls, final answer)`.
/// One tool call of the scenario: `(id, tool, arguments)`.
type Call = (String, &'static str, Value);

/// One turn of the scenario: `(user text, rounds of tool calls, final answer)`.
type Turn = (&'static str, Vec<Vec<Call>>, String);

fn scenario() -> Vec<Turn> {
    let call = |id: &str, tool: &'static str, args: Value| (id.to_owned(), tool, args);
    vec![
        (
            "What plans are in progress?",
            vec![vec![call(
                "a1",
                "mcp__po__plan",
                json!({"action": "list", "status": "in_progress"}),
            )]],
            filler("answer-1", 400),
        ),
        (
            "Give me the context of the next task of the first plan.",
            vec![
                vec![call(
                    "b1",
                    "mcp__po__task",
                    json!({"action": "get_next", "plan_id": "p1"}),
                )],
                vec![call(
                    "b2",
                    "mcp__po__task",
                    json!({"action": "get_context", "task_id": "t1"}),
                )],
            ],
            filler("answer-2", 600),
        ),
        (
            "Split it in two steps.",
            vec![vec![
                call(
                    "c1",
                    "mcp__po__step",
                    json!({"action": "create", "task_id": "t1", "description": "measure"}),
                ),
                call(
                    "c2",
                    "mcp__po__step",
                    json!({"action": "create", "task_id": "t1", "description": "compare"}),
                ),
            ]],
            filler("answer-3", 200),
        ),
        (
            "Any notes about prompt caching?",
            vec![vec![call(
                "d1",
                "mcp__po__note",
                json!({"action": "search_semantic", "query": "prompt cache"}),
            )]],
            filler("answer-4", 500),
        ),
        (
            "Where is the usage parsed in the code?",
            vec![vec![call(
                "e1",
                "mcp__po__code",
                json!({"action": "search", "query": "parse_usage"}),
            )]],
            filler("answer-5", 400),
        ),
        (
            "Explain the difference between input and cached tokens.",
            vec![],
            filler("answer-6", 900),
        ),
        (
            "Mark the task in progress.",
            vec![vec![call(
                "g1",
                "mcp__po__task",
                json!({"action": "update", "task_id": "t1", "status": "in_progress"}),
            )]],
            filler("answer-7", 120),
        ),
        (
            "Record the commit abc123 and link it to the task.",
            vec![
                vec![call(
                    "h1",
                    "mcp__po__commit",
                    json!({"action": "create", "query": "abc123"}),
                )],
                vec![call(
                    "h2",
                    "mcp__po__commit",
                    json!({"action": "link_to_task", "task_id": "t1"}),
                )],
            ],
            filler("answer-8", 150),
        ),
        (
            "Record the decision to measure before optimising.",
            vec![vec![call(
                "i1",
                "mcp__po__decision",
                json!({"action": "add", "description": "measure first"}),
            )]],
            filler("answer-9", 200),
        ),
        ("Summarise what we did.", vec![], filler("answer-10", 700)),
    ]
}

struct Row {
    turn: usize,
    requests: usize,
    input: usize,
    cached: usize,
    tools: usize,
    system: usize,
    history: usize,
}

/// Bytes `next` shares with `prev` at its start, in the order a server lays the
/// prompt out: tools first, then the messages.
fn common_prefix_bytes(prev: &Value, next: &Value) -> usize {
    let tools = canon(&next["tools"]);
    if canon(&prev["tools"]) != tools {
        return 0;
    }
    let mut shared = tools.len();
    let before = prev["messages"].as_array().unwrap();
    let after = next["messages"].as_array().unwrap();
    for (old, new) in before.iter().zip(after) {
        let (old, new) = (canon(old), canon(new));
        if old != new {
            shared += old
                .bytes()
                .zip(new.bytes())
                .take_while(|(a, b)| a == b)
                .count();
            break;
        }
        shared += new.len();
    }
    shared
}

fn prompt_bytes(body: &Value) -> (usize, usize, usize) {
    let tools = canon(&body["tools"]).len();
    let messages = body["messages"].as_array().unwrap();
    let system = messages
        .iter()
        .filter(|m| m["role"] == "system")
        .map(|m| canon(m).len())
        .sum::<usize>();
    let history = messages
        .iter()
        .filter(|m| m["role"] != "system")
        .map(|m| canon(m).len())
        .sum::<usize>();
    (tools, system, history)
}

#[tokio::test]
#[ignore = "reference benchmark: run with --ignored --nocapture (see the module doc)"]
async fn reference_benchmark() {
    let script = scenario();
    let mut routes = Vec::new();
    for (_, rounds, answer) in &script {
        for round in rounds {
            let calls: Vec<(&str, &str, Value)> = round
                .iter()
                .map(|(id, tool, args)| (id.as_str(), *tool, args.clone()))
                .collect();
            routes.push(chat_route(call_events(&calls, None)));
        }
        routes.push(chat_route(answer_events(answer, None)));
    }
    let h = Harness::new(routes, |_| {}).await;
    let session = h.provider.open(h.spec(SYSTEM)).await.expect("open");
    let mut per_turn = Vec::new();
    for (text, rounds, _) in &script {
        let events = turn(&*session, text).await;
        assert_eq!(done_usage(&events).2, StopReason::Completed, "{events:?}");
        per_turn.push(rounds.len() + 1);
    }
    let requests = h.chat();
    assert_eq!(requests.len(), per_turn.iter().sum::<usize>());
    assert_prefix_stable(&requests);

    let mut rows = Vec::new();
    let mut at = 0;
    let mut previous: Option<&Value> = None;
    for (index, count) in per_turn.iter().enumerate() {
        let mut row = Row {
            turn: index + 1,
            requests: *count,
            input: 0,
            cached: 0,
            tools: 0,
            system: 0,
            history: 0,
        };
        for request in &requests[at..at + count] {
            let (tools, system, history) = prompt_bytes(request);
            row.input += tokens(tools + system + history);
            row.cached += previous.map_or(0, |prev| tokens(common_prefix_bytes(prev, request)));
            (row.tools, row.system, row.history) = (tokens(tools), tokens(system), tokens(history));
            previous = Some(request);
        }
        at += count;
        rows.push(row);
    }

    println!(
        "\nnative context reference benchmark — estimate: canonical JSON bytes / {BYTES_PER_TOKEN}"
    );
    println!(
        "input/cached: summed over the requests of the turn; tools/system/history: last request of the turn"
    );
    println!(
        "{:>4} {:>4} {:>8} {:>8} {:>6} {:>6} {:>6} {:>8}",
        "turn", "reqs", "input", "cached", "hit%", "tools", "system", "history"
    );
    let (mut input, mut cached) = (0, 0);
    for row in &rows {
        input += row.input;
        cached += row.cached;
        println!(
            "{:>4} {:>4} {:>8} {:>8} {:>5.1}% {:>6} {:>6} {:>8}",
            row.turn,
            row.requests,
            row.input,
            row.cached,
            100.0 * row.cached as f64 / row.input as f64,
            row.tools,
            row.system,
            row.history
        );
    }
    println!(
        "total {} requests, input {input}, cached {cached} ({:.1}%), uncached {}",
        requests.len(),
        100.0 * cached as f64 / input as f64,
        input - cached
    );
}
