//! Conformance of `providers::claude_code` to the agent contract.
//!
//! The provider under test is the real [`ClaudeCodeProvider`]: it spawns an
//! executable, speaks stream-json with it and writes control JSON to its stdin.
//! The executable is `fake_claude`, scripted by one transcript per scenario — no
//! real `claude`, no network.
//!
//! Two parts:
//!
//! 1. the suite of `testkit::conformance`, the same one every provider runs;
//! 2. what only this adapter must prove: the lines written to the CLI's stdin,
//!    byte for byte (contract §15); the child's environment and command line
//!    (§11); the routing of an out-of-turn permission; the death of the process;
//!    the neutral hooks.

mod support;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use nexus_claude::agent::{
    AgentEvent, AgentProvider, AgentSession, CancelScope, CompactionInfo, CompactionPhase,
    CompactionTrigger, EventStream, HookVerdict, InterruptScope, McpServerSpec, ModelInfo,
    PermissionDecision, PermissionScope, PolicyMode, ProviderError, ProviderKind, QuestionAnswer,
    QuestionReply, ResumeToken, SessionHooks, SessionSpec, StopReason, ToolCallInfo, ToolCategory,
    ToolResultInfo, TurnInput,
};
use nexus_claude::providers::claude_code::{ClaudeCodeConfig, ClaudeCodeProvider};
use nexus_claude::testkit::{ConformanceTarget, Prepared, Scenario, ScenarioOutcome, run_all};
use serde_json::{Value, json};
use support::{FakeCli, Transcript, msg};

const WAIT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Staging
// ---------------------------------------------------------------------------

/// The instance under test: defaults (allowlist environment, MCP configuration
/// in a file, reported cost), a two-model catalogue, and the fake as CLI.
fn config(fake: Option<&FakeCli>) -> ClaudeCodeConfig {
    let mut default_model = ModelInfo::new("fake-claude");
    default_model.is_default = true;
    let mut config = ClaudeCodeConfig::default();
    config.models = vec![default_model, ModelInfo::new("fake-claude-2")];
    config.cli_path = fake.map(|fake| fake.cli_path().to_path_buf());
    config
}

/// A provider pointed at the fake, and a spec that hands the fake its
/// transcript and its recording files through `EnvSpec::set`.
fn stage(transcript: Transcript) -> (FakeCli, Arc<ClaudeCodeProvider>, SessionSpec) {
    let fake = transcript.build();
    let provider = Arc::new(ClaudeCodeProvider::new(config(Some(&fake))));
    let mut spec = SessionSpec::new(fake.dir());
    spec.env.set = fake.options().env.into_iter().collect();
    (fake, provider, spec)
}

fn assistant(content: Value, parent: Option<&str>) -> Value {
    json!({
        "type": "assistant",
        "message": {"id": "msg_fake", "type": "message", "role": "assistant",
                    "model": "fake-claude", "content": content},
        "parent_tool_use_id": parent,
    })
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    assistant(
        json!([{"type": "tool_use", "id": id, "name": name, "input": input}]),
        None,
    )
}

fn tool_result(id: &str, content: &str, is_error: bool) -> Value {
    json!({
        "type": "user",
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error}
        ]},
    })
}

fn can_use_tool(request_id: &str, tool: &str, input: Value, tool_use_id: Option<&str>) -> Value {
    let mut message = msg::permission_request(request_id, tool, input);
    if let Some(id) = tool_use_id {
        message["request"]["tool_use_id"] = json!(id);
    }
    message
}

fn stream_event(event: Value) -> Value {
    json!({"type": "stream_event", "session_id": "fake-session", "event": event})
}

fn hook_input(event: &str, extra: Value) -> Value {
    let mut input = json!({
        "hook_event_name": event,
        "session_id": "fake-session",
        "transcript_path": "/tmp/transcript.jsonl",
        "cwd": "/work",
    });
    for (key, value) in extra.as_object().into_iter().flatten() {
        input[key] = value.clone();
    }
    input
}

/// Waits for the prompt of a scenario, then opens the session like the CLI.
fn begin(scenario: Scenario) -> Transcript {
    Transcript::new()
        .await_stdin_containing(scenario.prompt())
        .init("fake-session")
}

/// A plain turn: text, then a successful result.
fn answer(transcript: Transcript, text: &str) -> Transcript {
    transcript.assistant_text(text).result_ok(text)
}

/// The fake stays alive until the session is closed.
fn until_closed(transcript: Transcript) -> Transcript {
    transcript.wait_eof_for(10_000)
}

fn interrupted_result() -> Value {
    msg::result("error_during_execution", "", true)
}

const INTERRUPT_MARK: &str = r#""type":"interrupt""#;

/// The transcript that stages a scenario as its documentation says. For the
/// three capabilities absent in this slice (`tool_cancel`, `background_tasks`,
/// `images`) the staging is a plain turn: the suite verifies the fallback.
fn transcript_for(scenario: Scenario) -> Transcript {
    let script = match scenario {
        Scenario::TourTexteSimple
        | Scenario::ChangementModele
        | Scenario::ChangementPolitique
        | Scenario::Reprise
        | Scenario::FinUsageCout
        | Scenario::MessageImages => answer(begin(scenario), "bonjour"),
        // A tool is a real child process of the fake; cancelling the tools
        // signals it, the tool ends in error and the turn goes on to its end.
        Scenario::AnnulationTourPreserve => answer(
            begin(scenario)
                // The tool's process exists before the call is announced: the
                // suite cancels as soon as it sees the complete call.
                .spawn_child()
                .json(tool_use("toolu_cut", "Bash", json!({"command": "sleep 600"})))
                .wait_children_exit(15_000)
                .json(tool_result("toolu_cut", "interrupted by cancel_tools", true)),
            "went on",
        ),
        // A background task: a `Bash` call with `run_in_background`, its process
        // appearing after the adapter has read the process table, then the turn
        // ends and the process lives on until the task is cancelled.
        Scenario::AnnulationTache => {
            let script = answer(
                begin(scenario)
                    .json(tool_use(
                        "toolu_bg",
                        "Bash",
                        json!({"command": "sleep 600", "run_in_background": true}),
                    ))
                    .sleep_ms(400)
                    .spawn_child()
                    .json(tool_result("toolu_bg", "started", false)),
                "started a task",
            )
            .wait_children_exit(15_000);
            return until_closed(script);
        },
        Scenario::FluxDeltas => begin(scenario)
            .json(stream_event(json!({"type": "message_start", "message": {}})))
            .json(stream_event(json!({
                "type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": ""}
            })))
            .text_delta("bon")
            .text_delta("jour")
            .json(stream_event(json!({"type": "content_block_stop", "index": 0})))
            .json(stream_event(json!({"type": "message_stop"})))
            .assistant_text("bonjour")
            .result_ok("bonjour"),
        Scenario::Raisonnement => begin(scenario)
            .json(assistant(
                json!([
                    {"type": "thinking", "thinking": "let me think", "signature": "sig"},
                    {"type": "text", "text": "42"}
                ]),
                None,
            ))
            .result_ok("42"),
        // The suite opens this scenario with hooks: the CLI calls them around
        // the tool, as the real one does.
        Scenario::AppelOutilResultat => answer(
            Transcript::new()
                .capture_hooks()
                .await_stdin_containing(scenario.prompt())
                .init("fake-session")
                .json(tool_use("toolu_1", "Read", json!({"file_path": "/work/a.rs"})))
                .emit_hook(
                    "PreToolUse",
                    "hook-pre",
                    hook_input(
                        "PreToolUse",
                        json!({"tool_name": "Read", "tool_input": {"file_path": "/work/a.rs"}}),
                    ),
                    Some("toolu_1"),
                )
                .json(tool_result("toolu_1", "fn main() {}", false))
                .emit_hook(
                    "PostToolUse",
                    "hook-post",
                    hook_input(
                        "PostToolUse",
                        json!({"tool_name": "Read", "tool_input": {"file_path": "/work/a.rs"},
                               "tool_response": {"content": "fn main() {}"}}),
                    ),
                    Some("toolu_1"),
                ),
            "read",
        ),
        Scenario::OutilsParalleles => answer(
            begin(scenario)
                .json(assistant(
                    json!([
                        {"type": "tool_use", "id": "toolu_a", "name": "Glob", "input": {"pattern": "*.rs"}},
                        {"type": "tool_use", "id": "toolu_b", "name": "Grep", "input": {"pattern": "fn"}}
                    ]),
                    None,
                ))
                .json(tool_result("toolu_a", "a.rs", false))
                .json(tool_result("toolu_b", "a.rs:1", false)),
            "found",
        ),
        Scenario::PermissionAccordee => answer(
            begin(scenario)
                .json(tool_use("toolu_p", "Bash", json!({"command": "ls"})))
                .json(can_use_tool(
                    "req-perm",
                    "Bash",
                    json!({"command": "ls"}),
                    Some("toolu_p"),
                ))
                .await_stdin_containing("req-perm")
                .json(tool_result("toolu_p", "a.rs", false)),
            "listed",
        ),
        Scenario::PermissionRefusee => answer(
            begin(scenario)
                .json(tool_use("toolu_p", "Bash", json!({"command": "rm -rf /"})))
                .json(can_use_tool(
                    "req-perm",
                    "Bash",
                    json!({"command": "rm -rf /"}),
                    Some("toolu_p"),
                ))
                .await_stdin_containing("req-perm")
                .json(tool_result("toolu_p", "permission denied", true)),
            "not done",
        ),
        // Reply mode `turn`: the adapter unblocks the tool itself, the turn ends,
        // and the answer comes as a second turn.
        Scenario::QuestionUtilisateur => {
            let question = json!({"questions": [{
                "question": "Which one?", "header": "Choice", "multiSelect": false,
                "options": [{"label": "A", "description": "first"}, {"label": "B"}]
            }]});
            answer(
                answer(
                    begin(scenario)
                        .json(tool_use("toolu_q", "AskUserQuestion", question.clone()))
                        .json(can_use_tool(
                            "req-question",
                            "AskUserQuestion",
                            question,
                            Some("toolu_q"),
                        ))
                        .await_stdin_containing("req-question")
                        .json(tool_result("toolu_q", "asked", false)),
                    "waiting for your answer",
                )
                .await_stdin_containing(scenario.prompt()),
                "thanks",
            )
        },
        Scenario::InterruptionEnFlux | Scenario::FermetureIdempotente => begin(scenario)
            .assistant_text("working")
            .await_stdin_containing(INTERRUPT_MARK)
            .json(interrupted_result()),
        Scenario::InterruptionOutil => begin(scenario)
            .json(tool_use("toolu_long", "Bash", json!({"command": "sleep 600"})))
            .await_stdin_containing(INTERRUPT_MARK)
            .json(tool_result("toolu_long", "interrupted", true))
            .json(interrupted_result()),
        Scenario::SousAgent => answer(
            begin(scenario)
                .json(tool_use("toolu_task", "Task", json!({"prompt": "explore"})))
                .json(assistant(
                    json!([{"type": "text", "text": "exploring"}]),
                    Some("toolu_task"),
                ))
                .json(assistant(
                    json!([{"type": "tool_use", "id": "toolu_child", "name": "Read", "input": {}}]),
                    Some("toolu_task"),
                ))
                .json(json!({
                    "type": "user",
                    "message": {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_child", "content": "x"}
                    ]},
                    "parent_tool_use_id": "toolu_task",
                }))
                .json(tool_result("toolu_task", "explored", false)),
            "done",
        ),
        Scenario::Compaction => answer(
            begin(scenario).system(
                "compact_boundary",
                json!({"compact_metadata": {"trigger": "auto", "pre_tokens": 150_000}}),
            ),
            "compacted",
        ),
        // The suite opens with hooks: the `initialize` request tells the fake
        // the session is listening, and the notice is printed with no turn running.
        Scenario::HorsTour => answer(
            Transcript::new()
                .capture_hooks()
                .system("status", json!({"status": "idle"}))
                .await_stdin_containing(scenario.prompt())
                .init("fake-session"),
            "bonjour",
        ),
        // A control request is buffered by the transport: nothing to wait for.
        Scenario::PermissionHorsTour => Transcript::new()
            .json(can_use_tool(
                "req-oob",
                "Bash",
                json!({"command": "npm test"}),
                None,
            ))
            .await_stdin_containing("req-oob"),
        Scenario::TourConcurrent => answer(
            begin(scenario)
                .assistant_text("working")
                .await_stdin_containing(INTERRUPT_MARK)
                .json(interrupted_result())
                .await_stdin_containing(scenario.prompt()),
            "second",
        ),
        Scenario::ErreurRetryable => answer(
            begin(scenario)
                .json(msg::result(
                    "error_during_execution",
                    r#"API Error: 529 {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
                    true,
                ))
                .await_stdin_containing(scenario.prompt()),
            "second",
        ),
        Scenario::ProcessusMort => {
            return begin(scenario).assistant_text("about to die").exit_with(3);
        },
        _ => answer(begin(scenario), "bonjour"),
    };
    until_closed(script)
}

struct FakeClaudeTarget;

#[async_trait]
impl ConformanceTarget for FakeClaudeTarget {
    fn name(&self) -> &str {
        "claude_code (fake_claude)"
    }

    fn provider(&self) -> Arc<dyn AgentProvider> {
        Arc::new(ClaudeCodeProvider::new(config(None)))
    }

    async fn prepare(&self, scenario: Scenario) -> Option<Prepared> {
        let (fake, provider, spec) = stage(transcript_for(scenario));
        let mut prepared = Prepared::new(provider, spec);
        if scenario == Scenario::Reprise {
            prepared.resume = Some(ResumeToken::claude_code_session("session-to-resume"));
        }
        prepared.guard = Some(Box::new(fake));
        Some(prepared)
    }
}

// ---------------------------------------------------------------------------
// 1. The suite
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claude_code_passes_the_conformance_suite() {
    let report = run_all(&FakeClaudeTarget).await;
    println!("{}", report.summary());
    report.assert_conformant();

    // The capability this slice declares absent (images) had its FALLBACK
    // verified; everything else played out for real.
    for (scenario, outcome) in &report.results {
        let expected = match scenario {
            Scenario::MessageImages => ScenarioOutcome::FallbackVerified,
            _ => ScenarioOutcome::Passed,
        };
        assert_eq!(outcome, &expected, "{scenario}");
    }
}

#[test]
fn the_declared_capabilities_are_the_ones_of_this_slice() {
    let provider = ClaudeCodeProvider::new(config(None));
    assert_eq!(provider.kind(), ProviderKind::ClaudeCode);
    assert_eq!(provider.id(), "claude-code");
    let capabilities = provider.capabilities(None);
    assert!(capabilities.secret_isolation);
    assert!(capabilities.tool_cancel);
    assert!(capabilities.background_tasks);
    assert!(!capabilities.images);
    assert_eq!(
        capabilities.permission_scopes,
        [
            PermissionScope::Once,
            PermissionScope::Session,
            PermissionScope::Always
        ]
    );
}

// ---------------------------------------------------------------------------
// Driving a session by hand
// ---------------------------------------------------------------------------

async fn open(provider: &ClaudeCodeProvider, spec: SessionSpec) -> Arc<dyn AgentSession> {
    tokio::time::timeout(WAIT, provider.open(spec))
        .await
        .expect("open() answers")
        .expect("open() succeeds")
}

async fn next(stream: &mut EventStream) -> Option<AgentEvent> {
    tokio::time::timeout(WAIT, stream.next())
        .await
        .expect("the stream yields or closes in time")
}

/// Reads a turn to its end; `on_event` reacts to each event.
async fn read_turn<F, Fut>(mut stream: EventStream, mut on_event: F) -> Vec<AgentEvent>
where
    F: FnMut(AgentEvent) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut events = Vec::new();
    while let Some(event) = next(&mut stream).await {
        events.push(event.clone());
        on_event(event).await;
    }
    events
}

async fn stdin_line(fake: &FakeCli, containing: &str) -> String {
    let found = support::poll_until(WAIT, || {
        fake.stdin_lines()
            .iter()
            .any(|line| line.contains(containing))
    })
    .await;
    assert!(
        found,
        "no stdin line containing {containing:?}; got {:#?}",
        fake.stdin_lines()
    );
    fake.stdin_lines()
        .into_iter()
        .find(|line| line.contains(containing))
        .unwrap()
}

/// Replaces the random `request_id` of a control request with `<uuid>`, after
/// checking that it is one.
fn mask_uuid(line: &str) -> String {
    let value: Value = serde_json::from_str(line).expect("a JSON line");
    let id = value
        .get("request_id")
        .or_else(|| value["request"].get("request_id"))
        .and_then(Value::as_str)
        .expect("a control request carries a request_id")
        .to_owned();
    assert_eq!(id.len(), 36, "{id} is not a UUID");
    line.replace(&id, "<uuid>")
}

fn stop_reason(events: &[AgentEvent]) -> Option<StopReason> {
    match events.last() {
        Some(AgentEvent::Done { stop_reason, .. }) => Some(*stop_reason),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 2a. stdin, byte for byte (contract §15)
// ---------------------------------------------------------------------------

fn permission_transcript(request_id: &str, input: Value) -> Transcript {
    until_closed(
        Transcript::new()
            .await_stdin_containing("run it")
            .init("fake-session")
            .json(tool_use("toolu_1", "Bash", input.clone()))
            .json(can_use_tool(request_id, "Bash", input, Some("toolu_1")))
            .await_stdin_containing(request_id)
            .json(tool_result("toolu_1", "ok", false))
            .result_ok("ok"),
    )
}

#[tokio::test]
async fn a_granted_permission_replays_the_original_input_byte_for_byte() {
    let input = json!({"command": "ls -la", "description": "list"});
    let (fake, provider, spec) = stage(permission_transcript("req-allow", input.clone()));
    let session = open(&provider, spec).await;
    let stream = session.send_turn(TurnInput::text("run it")).await.unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let events = read_turn(stream, |event| {
        let session = Arc::clone(&session);
        let asked = Arc::clone(&asked);
        async move {
            if let AgentEvent::PermissionAsk { request_id, .. } = &event {
                // No `updated_input`: the adapter kept the original one.
                session
                    .answer_permission(request_id, PermissionDecision::allow_once())
                    .await
                    .unwrap();
                asked.lock().unwrap().push(event);
            }
        }
    })
    .await;
    assert_eq!(stop_reason(&events), Some(StopReason::Completed));
    assert_eq!(
        asked.lock().unwrap().as_slice(),
        [AgentEvent::PermissionAsk {
            request_id: "req-allow".into(),
            tool_name: "Bash".into(),
            input,
            category: ToolCategory::Command,
            canonical: Some("Bash".into()),
            tool_call_id: Some("toolu_1".into()),
            scopes: vec![
                PermissionScope::Once,
                PermissionScope::Session,
                PermissionScope::Always
            ],
            parent: None,
        }]
    );
    let lines = fake.stdin_lines();
    // The input message is the one `InteractiveClient::send_message` writes.
    assert_eq!(
        lines[0],
        r#"{"type":"user","message":{"content":"run it","role":"user"},"parent_tool_use_id":null,"session_id":"default"}"#
    );
    assert_eq!(
        lines[1],
        r#"{"response":{"request_id":"req-allow","response":{"behavior":"allow","updatedInput":{"command":"ls -la","description":"list"}},"subtype":"success"},"type":"control_response"}"#
    );
    assert_eq!(lines.len(), 2, "nothing else was written: {lines:#?}");
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_replaced_input_and_a_lasting_scope_are_written_as_documented() {
    let mut ask = can_use_tool("req-scope", "Bash", json!({"command": "ls"}), None);
    ask["request"]["permission_suggestions"] = json!([
        {"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "ls"}],
         "behavior": "allow", "destination": "localSettings"}
    ]);
    let (fake, provider, spec) = stage(until_closed(
        Transcript::new()
            .json(ask)
            .await_stdin_containing("req-scope"),
    ));
    let session = open(&provider, spec).await;
    let mut out_of_band = session.out_of_band().unwrap();
    assert!(matches!(
        next(&mut out_of_band).await,
        Some(AgentEvent::PermissionAsk { .. })
    ));
    session
        .answer_permission(
            "req-scope",
            PermissionDecision::Allow {
                scope: PermissionScope::Session,
                updated_input: Some(json!({"command": "ls -1"})),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        stdin_line(&fake, "req-scope").await,
        r#"{"response":{"request_id":"req-scope","response":{"behavior":"allow","updatedInput":{"command":"ls -1"},"updatedPermissions":[{"behavior":"allow","destination":"session","rules":[{"ruleContent":"ls","toolName":"Bash"}],"type":"addRules"}]},"subtype":"success"},"type":"control_response"}"#
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_denied_permission_is_written_byte_for_byte() {
    let (fake, provider, spec) = stage(until_closed(
        Transcript::new()
            .json(can_use_tool(
                "req-deny-1",
                "Bash",
                json!({"command": "x"}),
                None,
            ))
            .json(can_use_tool(
                "req-deny-2",
                "Bash",
                json!({"command": "y"}),
                None,
            ))
            .await_stdin_containing("req-deny-2"),
    ));
    let session = open(&provider, spec).await;
    // A request can only be answered once it has been asked.
    let mut out_of_band = session.out_of_band().unwrap();
    for expected in ["req-deny-1", "req-deny-2"] {
        assert!(matches!(
            next(&mut out_of_band).await,
            Some(AgentEvent::PermissionAsk { request_id, .. }) if request_id == expected
        ));
    }
    session
        .answer_permission("req-deny-1", PermissionDecision::deny())
        .await
        .unwrap();
    session
        .answer_permission(
            "req-deny-2",
            PermissionDecision::Deny {
                message: Some("not on my watch".into()),
                interrupt: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        stdin_line(&fake, "req-deny-1").await,
        r#"{"response":{"request_id":"req-deny-1","response":{"behavior":"deny","message":"User denied the permission request"},"subtype":"success"},"type":"control_response"}"#
    );
    assert_eq!(
        stdin_line(&fake, "req-deny-2").await,
        r#"{"response":{"request_id":"req-deny-2","response":{"behavior":"deny","message":"not on my watch"},"subtype":"success"},"type":"control_response"}"#
    );
    // Answered once: the identifier is gone.
    assert!(matches!(
        session
            .answer_permission("req-deny-1", PermissionDecision::deny())
            .await,
        Err(ProviderError::InvalidRequest { .. })
    ));
    session.close().await.unwrap();
}

#[tokio::test]
async fn set_policy_mode_and_set_model_are_written_byte_for_byte() {
    let (fake, provider, spec) = stage(until_closed(Transcript::new()));
    let session = open(&provider, spec).await;
    session
        .set_policy_mode(PolicyMode::AutoEdits, None)
        .await
        .unwrap();
    // The native mode of the caller, when it is one of the same neutral mode.
    session
        .set_policy_mode(PolicyMode::AutoEdits, Some("auto"))
        .await
        .unwrap();
    session
        .set_policy_mode(PolicyMode::Ask, Some("bypassPermissions"))
        .await
        .unwrap();
    session.set_model("fake-claude-2").await.unwrap();
    let lines = fake.wait_for_stdin_lines(4, WAIT).await;
    let masked: Vec<String> = lines.iter().map(|line| mask_uuid(line)).collect();
    assert_eq!(
        masked,
        [
            r#"{"request":{"mode":"acceptEdits","subtype":"set_permission_mode"},"request_id":"<uuid>","type":"control_request"}"#,
            r#"{"request":{"mode":"auto","subtype":"set_permission_mode"},"request_id":"<uuid>","type":"control_request"}"#,
            r#"{"request":{"mode":"default","subtype":"set_permission_mode"},"request_id":"<uuid>","type":"control_request"}"#,
            r#"{"request":{"model":"fake-claude-2","subtype":"set_model"},"request_id":"<uuid>","type":"control_request"}"#,
        ]
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn an_interruption_is_written_byte_for_byte_and_ends_the_turn_interrupted() {
    let (fake, provider, spec) = stage(until_closed(
        Transcript::new()
            .await_stdin_containing("work")
            .init("fake-session")
            .assistant_text("working")
            .await_stdin_containing(INTERRUPT_MARK)
            .json(interrupted_result()),
    ));
    let session = open(&provider, spec).await;
    // Outside a turn: nothing is written.
    let outside = session.interrupt(InterruptScope::TurnOnly).await.unwrap();
    assert!(!outside.turn_interrupted);

    let stream = session.send_turn(TurnInput::text("work")).await.unwrap();
    let outcome = Arc::new(Mutex::new(None));
    let events = read_turn(stream, |event| {
        let session = Arc::clone(&session);
        let outcome = Arc::clone(&outcome);
        async move {
            if matches!(event, AgentEvent::Text { .. }) {
                let interrupted = session.interrupt(InterruptScope::TurnAndTools).await;
                *outcome.lock().unwrap() = Some(interrupted.unwrap());
            }
        }
    })
    .await;
    let outcome = outcome
        .lock()
        .unwrap()
        .clone()
        .expect("the turn was interrupted");
    assert!(outcome.turn_interrupted);
    assert_eq!(outcome.tools_cancelled, 0);
    assert!(
        outcome
            .diagnostic
            .is_some_and(|diagnostic| diagnostic.pid.is_some()),
        "the CLI's pid is reported for diagnostic"
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            stop_reason: StopReason::Interrupted,
            subtype: Some(subtype),
            is_error: true,
            ..
        }) if subtype == "error_during_execution"
    ));
    let lines = fake.stdin_lines();
    assert_eq!(lines.len(), 2, "the prompt and one interrupt: {lines:#?}");
    assert_eq!(
        mask_uuid(&lines[1]),
        r#"{"request":{"request_id":"<uuid>","type":"interrupt"},"type":"control_request"}"#
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn ask_user_question_is_unblocked_by_the_adapter_byte_for_byte() {
    let question = json!({"questions": [{
        "question": "Which one?", "header": "Choice", "multiSelect": true,
        "options": [{"label": "A", "description": "first"}, {"label": "B"}]
    }]});
    let (fake, provider, spec) = stage(until_closed(
        Transcript::new()
            .await_stdin_containing("ask me")
            .init("fake-session")
            .json(tool_use("toolu_q", "AskUserQuestion", question.clone()))
            .json(can_use_tool(
                "req-question",
                "AskUserQuestion",
                question.clone(),
                Some("toolu_q"),
            ))
            .await_stdin_containing("req-question")
            .json(tool_result("toolu_q", "asked", false))
            .result_ok("asked"),
    ));
    let session = open(&provider, spec).await;
    let stream = session.send_turn(TurnInput::text("ask me")).await.unwrap();
    let events = read_turn(stream, |_| async {}).await;
    let asked = events
        .iter()
        .find(|event| matches!(event, AgentEvent::Question { .. }))
        .expect("the question reaches the host");
    let AgentEvent::Question {
        question_id,
        tool_call_id,
        reply,
        questions,
        input,
        parent,
    } = asked
    else {
        unreachable!()
    };
    assert_eq!(question_id, "req-question");
    assert_eq!(tool_call_id.as_deref(), Some("toolu_q"));
    assert_eq!(*reply, QuestionReply::Turn);
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].question, "Which one?");
    assert!(questions[0].multi_select);
    assert_eq!(questions[0].options[1].label, "B");
    assert_eq!(input, &question);
    assert_eq!(parent, &None);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::PermissionAsk { .. })),
        "a question is not a permission"
    );
    assert_eq!(stop_reason(&events), Some(StopReason::Completed));
    assert_eq!(
        fake.stdin_lines()[1],
        r#"{"response":{"request_id":"req-question","response":{"behavior":"allow","updatedInput":{"questions":[{"header":"Choice","multiSelect":true,"options":[{"description":"first","label":"A"},{"label":"B"}],"question":"Which one?"}]}},"subtype":"success"},"type":"control_response"}"#
    );
    // The answer is a regular turn; `answer_question` and an answer to the
    // request as a permission are both refused.
    assert_eq!(
        session
            .answer_question("req-question", QuestionAnswer::Cancelled)
            .await,
        Err(ProviderError::unsupported("answer_question"))
    );
    assert!(matches!(
        session
            .answer_permission("req-question", PermissionDecision::allow_once())
            .await,
        Err(ProviderError::InvalidRequest { .. })
    ));
    session.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 2b. The child's environment and command line (contract §11)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_cli_gets_an_allowlisted_environment_and_no_mcp_secret_on_argv() {
    // Not vacuous: the variable is in the environment the child would inherit.
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());

    let (fake, provider, mut spec) = stage(until_closed(Transcript::new()));
    spec.model = Some("fake-claude".into());
    spec.mcp_servers.insert(
        "po".into(),
        McpServerSpec::Stdio {
            command: "/bin/po-mcp".into(),
            args: vec!["serve".into()],
            env: [(
                "NEO4J_PASSWORD".to_string(),
                "s3cr3t-db-password".to_string(),
            )]
            .into(),
        },
    );
    let session = open(&provider, spec).await;
    let invocation = fake.wait_for_invocation(WAIT).await;

    assert!(
        !invocation.has_env("CARGO_MANIFEST_DIR"),
        "the default environment policy of the provider is an allowlist"
    );
    assert!(
        invocation.has_env("PATH"),
        "the base allowlist is inherited"
    );
    assert!(
        invocation.has_env("FAKE_CLAUDE_TRANSCRIPT"),
        "`EnvSpec::set` reaches the child"
    );

    let args = invocation.args();
    assert!(
        !args.iter().any(|arg| arg.contains("s3cr3t-db-password")),
        "an MCP credential is on the command line: {args:?}"
    );
    let mcp_config = invocation
        .flag_value("--mcp-config")
        .expect("the MCP configuration is passed");
    assert!(
        !mcp_config.trim_start().starts_with('{'),
        "the MCP configuration is passed by path, not inline: {mcp_config}"
    );
    // The same command line as the orchestrator's `build_options`.
    assert_eq!(
        invocation.flag_value("--permission-prompt-tool").as_deref(),
        Some("stdio")
    );
    assert_eq!(
        invocation.flag_value("--permission-mode").as_deref(),
        Some("default")
    );
    assert_eq!(
        invocation.flag_value("--model").as_deref(),
        Some("fake-claude")
    );
    assert!(invocation.has_flag("--include-partial-messages"));
    assert!(!invocation.has_flag("--resume"));
    assert!(!invocation.has_flag("--allowedTools"));
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_resumed_session_passes_the_tokens_session_to_the_cli() {
    let (fake, provider, spec) = stage(until_closed(Transcript::new()));
    let foreign = ResumeToken::new(ProviderKind::Codex, 1, json!({"thread_id": "t"}));
    assert!(matches!(
        provider.resume(spec.clone(), foreign).await,
        Err(ProviderError::InvalidRequest { .. })
    ));
    let empty = ResumeToken::new(ProviderKind::ClaudeCode, 1, json!({}));
    assert!(matches!(
        provider.resume(spec.clone(), empty).await,
        Err(ProviderError::InvalidRequest { .. })
    ));
    let session = provider
        .resume(spec, ResumeToken::claude_code_session("cli-session-42"))
        .await
        .unwrap();
    let invocation = fake.wait_for_invocation(WAIT).await;
    assert_eq!(
        invocation.flag_value("--resume").as_deref(),
        Some("cli-session-42")
    );
    // Known from the token, before the CLI said anything.
    assert_eq!(
        session.resume_token(),
        Some(ResumeToken::claude_code_session("cli-session-42"))
    );
    session.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 2c. A permission arriving with no turn running
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_permission_arriving_out_of_turn_goes_out_of_band_then_the_turn_is_clean() {
    let (fake, provider, spec) = stage(until_closed(
        Transcript::new()
            .json(can_use_tool(
                "req-oob",
                "Bash",
                json!({"command": "npm test"}),
                Some("toolu_unseen"),
            ))
            .await_stdin_containing("req-oob")
            .await_stdin_containing("hello")
            .init("fake-session")
            .assistant_text("hi")
            .result_ok("hi"),
    ));
    let session = open(&provider, spec).await;
    let mut out_of_band = session.out_of_band().expect("first call");
    assert!(session.out_of_band().is_none(), "a single consumer");
    let asked = next(&mut out_of_band).await.expect("an out-of-band event");
    let AgentEvent::PermissionAsk {
        request_id,
        tool_call_id,
        ..
    } = &asked
    else {
        panic!("expected a permission_ask, got {asked:?}");
    };
    assert_eq!(request_id, "req-oob");
    assert_eq!(
        tool_call_id, &None,
        "a tool call never emitted is not named (invariant 2 of §4)"
    );
    session
        .answer_permission("req-oob", PermissionDecision::allow_once())
        .await
        .unwrap();
    assert!(
        stdin_line(&fake, "req-oob")
            .await
            .contains(r#""behavior":"allow""#)
    );

    // The turn that follows carries its own events only.
    let stream = session.send_turn(TurnInput::text("hello")).await.unwrap();
    let events = read_turn(stream, |_| async {}).await;
    assert_eq!(
        events.iter().map(AgentEvent::type_name).collect::<Vec<_>>(),
        ["session_started", "text", "done"]
    );
    assert_eq!(
        session.resume_token(),
        Some(ResumeToken::claude_code_session("fake-session"))
    );
    session.close().await.unwrap();
    assert_eq!(
        next(&mut out_of_band).await,
        None,
        "closed with the session"
    );
}

// ---------------------------------------------------------------------------
// 2d. The process dies during a turn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cli_dying_mid_turn_ends_the_turn_with_process_exited() {
    let (_fake, provider, spec) = stage(
        Transcript::new()
            .await_stdin_containing("go")
            .init("fake-session")
            .assistant_text("about to die")
            .exit_with(3),
    );
    let session = open(&provider, spec).await;
    let stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    let events = read_turn(stream, |_| async {}).await;
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Error {
            error: ProviderError::ProcessExited { code: None }
        })
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Text { text, .. } if text == "about to die")),
        "what was said before the death is not lost"
    );
    let dead = ProviderError::ProcessExited { code: None };
    assert_eq!(
        session.send_turn(TurnInput::text("again")).await.err(),
        Some(dead.clone())
    );
    assert_eq!(session.set_model("x").await, Err(dead.clone()));
    assert_eq!(
        session.cancel_tools(CancelScope::All).await.err(),
        Some(dead)
    );
    session.close().await.unwrap();
    assert_eq!(
        session.send_turn(TurnInput::text("again")).await.err(),
        Some(ProviderError::Closed)
    );
}

// ---------------------------------------------------------------------------
// 2e. Neutral hooks
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingHooks {
    before: Mutex<Vec<ToolCallInfo>>,
    after: Mutex<Vec<ToolResultInfo>>,
    compactions: Mutex<Vec<CompactionInfo>>,
    calls: AtomicU32,
}

#[async_trait]
impl SessionHooks for RecordingHooks {
    async fn before_tool(&self, call: &ToolCallInfo) -> HookVerdict {
        self.before.lock().unwrap().push(call.clone());
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => HookVerdict::Deny {
                reason: "not allowed here".into(),
            },
            1 => HookVerdict::ReplaceInput(json!({"file_path": "/work/safe.rs"})),
            _ => HookVerdict::AddContext("mind the tests".into()),
        }
    }

    async fn after_tool(&self, result: &ToolResultInfo) -> Option<String> {
        self.after.lock().unwrap().push(result.clone());
        Some("the file was read".into())
    }

    async fn before_compaction(&self, info: &CompactionInfo) -> Option<String> {
        self.compactions.lock().unwrap().push(info.clone());
        None
    }
}

#[tokio::test]
async fn the_neutral_hooks_are_called_and_their_verdicts_written_to_the_cli() {
    let pre = |request_id: &str| {
        (
            request_id.to_owned(),
            hook_input(
                "PreToolUse",
                json!({"tool_name": "Read", "tool_input": {"file_path": "/etc/passwd"}}),
            ),
        )
    };
    let mut transcript = Transcript::new()
        .capture_hooks()
        .await_stdin_containing("read it")
        .init("fake-session");
    for (request_id, input) in [pre("hook-deny"), pre("hook-replace"), pre("hook-context")] {
        transcript = transcript.emit_hook("PreToolUse", &request_id, input, Some("toolu_1"));
    }
    let (fake, provider, mut spec) = stage(until_closed(
        transcript
            .emit_hook(
                "PostToolUse",
                "hook-post",
                hook_input(
                    "PostToolUse",
                    json!({"tool_name": "Read", "tool_input": {"file_path": "/work/safe.rs"},
                           "tool_response": {"content": "fn main() {}", "is_error": false}}),
                ),
                Some("toolu_1"),
            )
            .emit_hook(
                "PreCompact",
                "hook-compact",
                hook_input("PreCompact", json!({"trigger": "manual"})),
                None,
            )
            .system(
                "compact_boundary",
                json!({"compact_metadata": {"trigger": "manual", "pre_tokens": 9}}),
            )
            .result_ok("done"),
    ));
    let hooks = Arc::new(RecordingHooks::default());
    spec.hooks = Some(hooks.clone());
    let session = open(&provider, spec).await;
    let stream = session.send_turn(TurnInput::text("read it")).await.unwrap();
    let events = read_turn(stream, |_| async {}).await;
    assert_eq!(stop_reason(&events), Some(StopReason::Completed));

    // The registration names the three events, one matcher without criteria each.
    let init = fake
        .stdin_json()
        .into_iter()
        .find(|line| line["request"]["subtype"] == "initialize")
        .expect("the hooks are registered with the CLI");
    for event in ["PreToolUse", "PostToolUse", "PreCompact"] {
        let matchers = init["request"]["hooks"][event].as_array().unwrap();
        assert_eq!(matchers.len(), 1, "{event}");
        assert_eq!(matchers[0]["matcher"], Value::Null, "{event}");
        assert_eq!(matchers[0]["hookCallbackIds"].as_array().unwrap().len(), 1);
    }

    // What the host's hooks were given.
    let before = hooks.before.lock().unwrap().clone();
    assert_eq!(before.len(), 3);
    assert_eq!(
        before[0],
        ToolCallInfo {
            id: Some("toolu_1".into()),
            name: "Read".into(),
            canonical: Some("Read".into()),
            category: ToolCategory::Read,
            input: json!({"file_path": "/etc/passwd"}),
        }
    );
    let after = hooks.after.lock().unwrap().clone();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].call.name, "Read");
    assert_eq!(after[0].output["content"], "fn main() {}");
    assert!(!after[0].is_error);
    assert_eq!(
        hooks.compactions.lock().unwrap().as_slice(),
        [CompactionInfo {
            trigger: "manual".into(),
            custom_instructions: None
        }]
    );

    // What the CLI was answered, byte for byte.
    assert_eq!(
        stdin_line(&fake, "hook-deny").await,
        r#"{"response":{"request_id":"hook-deny","response":{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"not allowed here"}},"subtype":"success"},"type":"control_response"}"#
    );
    assert_eq!(
        stdin_line(&fake, "hook-replace").await,
        r#"{"response":{"request_id":"hook-replace","response":{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"file_path":"/work/safe.rs"}}},"subtype":"success"},"type":"control_response"}"#
    );
    assert_eq!(
        stdin_line(&fake, "hook-context").await,
        r#"{"response":{"request_id":"hook-context","response":{"hookSpecificOutput":{"additionalContext":"mind the tests","hookEventName":"PreToolUse"}},"subtype":"success"},"type":"control_response"}"#
    );
    assert_eq!(
        stdin_line(&fake, "hook-post").await,
        r#"{"response":{"request_id":"hook-post","response":{"hookSpecificOutput":{"additionalContext":"the file was read","hookEventName":"PostToolUse"}},"subtype":"success"},"type":"control_response"}"#
    );
    assert_eq!(
        stdin_line(&fake, "hook-compact").await,
        r#"{"response":{"request_id":"hook-compact","response":{},"subtype":"success"},"type":"control_response"}"#
    );

    // `PreCompact` is also the `compaction started` of the contract.
    let compactions: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Compaction {
                phase,
                trigger,
                pre_tokens,
            } => Some((*phase, *trigger, *pre_tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(
        compactions,
        [
            (
                CompactionPhase::Started,
                Some(CompactionTrigger::Manual),
                None
            ),
            (
                CompactionPhase::Completed,
                Some(CompactionTrigger::Manual),
                Some(9)
            ),
        ]
    );
    session.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Concurrency rules the suite checks from the outside, seen on the wire
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_refused_turn_writes_nothing_and_close_ends_the_running_turn() {
    let (fake, provider, spec) = stage(until_closed(
        Transcript::new()
            .await_stdin_containing("first")
            .init("fake-session")
            .assistant_text("working"),
    ));
    let session = open(&provider, spec).await;
    let mut stream = session.send_turn(TurnInput::text("first")).await.unwrap();
    assert!(matches!(
        next(&mut stream).await,
        Some(AgentEvent::SessionStarted { .. })
    ));
    assert_eq!(
        session.send_turn(TurnInput::text("second")).await.err(),
        Some(ProviderError::TurnInProgress)
    );
    let mut image = TurnInput::text("look");
    image.blocks.push(nexus_claude::agent::InputBlock::Image {
        media_type: "image/png".into(),
        data_base64: "AAAA".into(),
    });
    // The turn in progress is refused first; the image would be refused next.
    assert_eq!(
        session.send_turn(image).await.err(),
        Some(ProviderError::TurnInProgress)
    );
    assert!(matches!(
        next(&mut stream).await,
        Some(AgentEvent::Text { .. })
    ));
    session.close().await.unwrap();
    assert_eq!(
        next(&mut stream).await,
        Some(AgentEvent::Error {
            error: ProviderError::Closed
        })
    );
    assert_eq!(next(&mut stream).await, None);
    session.close().await.unwrap();
    assert_eq!(
        fake.stdin_lines().len(),
        1,
        "only the accepted turn reached the CLI"
    );
}
