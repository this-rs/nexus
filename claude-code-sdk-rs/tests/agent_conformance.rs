//! Conformance of the agent contract, proven on the scripted provider.
//!
//! Five groups:
//!
//! - (a) the suite passes on a scripted target with every capability;
//! - (b) the suite passes on a scripted target with **no** capability: every
//!   capability scenario is then a verified fallback, none is skipped;
//! - (c) negative controls: each faulty provider must make the suite fail, with
//!   the failure named — a suite that cannot fail proves nothing;
//! - (d) replay: record → normalise → `Script::from_transcript` → record again
//!   gives the same bytes;
//! - (e) the concurrency rules of `ScriptedProvider` itself (contract §9, §10).
//!
//! Run with `cargo test -p nexus-claude --features testkit --test agent_conformance`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use nexus_claude::agent::*;
use nexus_claude::testkit::{
    ConformanceTarget, Prepared, RecordedCall, Recorder, Scenario, ScenarioOutcome, Script,
    ScriptedProvider, ScriptedTarget, Step, Transcript, done_event, full_capabilities, normalize,
    run_all, run_scenario, scripted_target, steps,
};
use serde_json::json;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);

async fn collect(stream: EventStream) -> Vec<AgentEvent> {
    timeout(WAIT, stream.collect())
        .await
        .expect("the stream ends")
}

// ---------------------------------------------------------------------------
// (a) every capability
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_the_suite_passes_on_a_scripted_target_with_every_capability() {
    let report = run_all(&scripted_target(full_capabilities())).await;
    report.assert_conformant();
    assert_eq!(report.results.len(), Scenario::ALL.len());
    for (scenario, outcome) in &report.results {
        assert_eq!(
            outcome,
            &ScenarioOutcome::Passed,
            "{scenario}: with every capability present nothing may be a fallback"
        );
    }
}

// ---------------------------------------------------------------------------
// (b) no capability: everything is a verified fallback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn b_the_suite_passes_on_a_scripted_target_with_no_capability() {
    let report = run_all(&scripted_target(Capabilities::none())).await;
    report.assert_conformant();
    for (scenario, outcome) in &report.results {
        let expected = match scenario.capability() {
            Some(_) => ScenarioOutcome::FallbackVerified,
            None => ScenarioOutcome::Passed,
        };
        assert_eq!(outcome, &expected, "{scenario}: {}", report.summary());
    }
    let fallbacks = report
        .results
        .iter()
        .filter(|(_, outcome)| *outcome == ScenarioOutcome::FallbackVerified)
        .count();
    assert_eq!(fallbacks, 16, "{}", report.summary());
}

#[tokio::test]
async fn b_the_suite_passes_on_a_partial_set_of_capabilities() {
    // What a third-party CLI looks like: permissions with one scope, sub-agents
    // on another thread, free local model, no hooks, no cancellation.
    let mut capabilities = Capabilities::none();
    capabilities.interactive_permissions = true;
    capabilities.permission_scopes = vec![PermissionScope::Session];
    capabilities.tools = true;
    capabilities.per_session_mcp = true;
    capabilities.subagents = SubagentSupport::SeparateThread;
    capabilities.thinking = true;
    capabilities.resume = true;
    capabilities.cost = CostBasis::Free;
    capabilities.hooks = HookSupport::Command;
    let report = run_all(&scripted_target(capabilities)).await;
    report.assert_conformant();
    assert_eq!(
        report.outcome(Scenario::PermissionAccordee),
        Some(&ScenarioOutcome::Passed)
    );
    assert_eq!(
        report.outcome(Scenario::AnnulationTourPreserve),
        Some(&ScenarioOutcome::FallbackVerified)
    );
}

// ---------------------------------------------------------------------------
// (c) negative controls
// ---------------------------------------------------------------------------

/// What a faulty provider gets wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// Declares capabilities it does not hold (the declaration is `declared`,
    /// the behaviour is the wrapped provider's).
    Lies,
    /// Emits a `text` after the terminal event of every turn.
    EmitsAfterTerminal,
}

struct FaultyTarget {
    inner: ScriptedTarget,
    declared: Capabilities,
    fault: Fault,
}

struct FaultyProvider {
    inner: Arc<dyn AgentProvider>,
    declared: Capabilities,
    fault: Fault,
}

struct FaultySession {
    inner: Arc<dyn AgentSession>,
    declared: Capabilities,
    fault: Fault,
}

#[async_trait]
impl ConformanceTarget for FaultyTarget {
    fn name(&self) -> &str {
        "faulty"
    }

    fn provider(&self) -> Arc<dyn AgentProvider> {
        Arc::new(FaultyProvider {
            inner: self.inner.provider(),
            declared: self.declared.clone(),
            fault: self.fault,
        })
    }

    async fn prepare(&self, scenario: Scenario) -> Option<Prepared> {
        let mut prepared = self.inner.prepare(scenario).await?;
        prepared.provider = Arc::new(FaultyProvider {
            inner: prepared.provider,
            declared: self.declared.clone(),
            fault: self.fault,
        });
        Some(prepared)
    }
}

impl FaultyProvider {
    fn wrap(&self, inner: Arc<dyn AgentSession>) -> Arc<dyn AgentSession> {
        Arc::new(FaultySession {
            inner,
            declared: self.declared.clone(),
            fault: self.fault,
        })
    }
}

#[async_trait]
impl AgentProvider for FaultyProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn kind(&self) -> ProviderKind {
        self.inner.kind()
    }

    async fn health(&self) -> ProviderHealth {
        self.inner.health().await
    }

    async fn catalog(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.inner.catalog().await
    }

    fn capabilities(&self, _model: Option<&str>) -> Capabilities {
        self.declared.clone()
    }

    async fn open(&self, spec: SessionSpec) -> Result<Arc<dyn AgentSession>, ProviderError> {
        Ok(self.wrap(self.inner.open(spec).await?))
    }

    async fn resume(
        &self,
        spec: SessionSpec,
        token: ResumeToken,
    ) -> Result<Arc<dyn AgentSession>, ProviderError> {
        Ok(self.wrap(self.inner.resume(spec, token).await?))
    }
}

#[async_trait]
impl AgentSession for FaultySession {
    fn capabilities(&self) -> &Capabilities {
        &self.declared
    }

    fn resume_token(&self) -> Option<ResumeToken> {
        self.inner.resume_token()
    }

    async fn send_turn(&self, input: TurnInput) -> Result<EventStream, ProviderError> {
        let stream = self.inner.send_turn(input).await?;
        if self.fault != Fault::EmitsAfterTerminal {
            return Ok(stream);
        }
        let late = futures::stream::once(async {
            AgentEvent::Text {
                text: "one more thing".into(),
                seq: None,
                parent: None,
            }
        });
        Ok(Box::pin(stream.chain(late)))
    }

    async fn answer_permission(
        &self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), ProviderError> {
        self.inner.answer_permission(request_id, decision).await
    }

    async fn answer_question(
        &self,
        question_id: &str,
        answer: QuestionAnswer,
    ) -> Result<(), ProviderError> {
        self.inner.answer_question(question_id, answer).await
    }

    async fn interrupt(&self, scope: InterruptScope) -> Result<InterruptOutcome, ProviderError> {
        self.inner.interrupt(scope).await
    }

    async fn cancel_tools(&self, scope: CancelScope) -> Result<CancelOutcome, ProviderError> {
        self.inner.cancel_tools(scope).await
    }

    async fn set_model(&self, model: &str) -> Result<(), ProviderError> {
        self.inner.set_model(model).await
    }

    async fn set_policy_mode(
        &self,
        mode: PolicyMode,
        native: Option<&str>,
    ) -> Result<(), ProviderError> {
        self.inner.set_policy_mode(mode, native).await
    }

    fn out_of_band(&self) -> Option<EventStream> {
        self.inner.out_of_band()
    }

    async fn close(&self) -> Result<(), ProviderError> {
        self.inner.close().await
    }
}

/// The failures of a scenario; panics when it did not fail.
fn failures_of(outcome: ScenarioOutcome) -> Vec<String> {
    match outcome {
        ScenarioOutcome::Failed(failures) => failures,
        other => panic!("the negative control was not detected: outcome is {other:?}"),
    }
}

fn assert_names(failures: &[String], expected: &str) {
    assert!(
        failures.iter().any(|failure| failure.contains(expected)),
        "no failure mentions `{expected}`: {failures:#?}"
    );
}

#[tokio::test]
async fn c_a_turn_without_terminal_event_fails_conformance() {
    let caps = full_capabilities();
    let broken =
        Script::new(caps.clone()).with_turn(vec![steps::text("no end"), Step::EndWithoutTerminal]);
    let target = scripted_target(caps).with_script(Scenario::TourTexteSimple, broken);
    let failures = failures_of(run_scenario(&target, Scenario::TourTexteSimple).await);
    assert_names(&failures, "without a terminal event");
    assert!(!run_all(&target).await.is_conformant());
}

#[tokio::test]
async fn c_an_orphan_tool_result_fails_conformance() {
    let caps = full_capabilities();
    let broken = Script::new(caps.clone()).with_turn(vec![
        steps::tool_call("tool-1", "Read", json!({})),
        steps::tool_result("tool-1", "ok"),
        steps::tool_result("tool-ghost", "from nowhere"),
        steps::text("done"),
        steps::done(&caps),
    ]);
    let target = scripted_target(caps).with_script(Scenario::AppelOutilResultat, broken);
    let failures = failures_of(run_scenario(&target, Scenario::AppelOutilResultat).await);
    assert_names(&failures, "orphan `tool_result`");
    assert_names(&failures, "tool-ghost");
}

#[tokio::test]
async fn c_a_declared_capability_that_is_not_held_fails_conformance() {
    // Declares `tool_cancel: true`; the provider underneath answers `Unsupported`.
    let mut held = full_capabilities();
    held.tool_cancel = false;
    let mut staged =
        scripted_target(full_capabilities()).script_for(Scenario::AnnulationTourPreserve);
    staged.capabilities = held.clone();
    let target = FaultyTarget {
        inner: scripted_target(held).with_script(Scenario::AnnulationTourPreserve, staged),
        declared: full_capabilities(),
        fault: Fault::Lies,
    };
    let failures = failures_of(run_scenario(&target, Scenario::AnnulationTourPreserve).await);
    assert_names(&failures, "cancel_tools(all) failed");
    assert_names(&failures, "unsupported");
}

#[tokio::test]
async fn c_an_event_after_the_terminal_event_fails_conformance() {
    let target = FaultyTarget {
        inner: scripted_target(full_capabilities()),
        declared: full_capabilities(),
        fault: Fault::EmitsAfterTerminal,
    };
    let failures = failures_of(run_scenario(&target, Scenario::TourTexteSimple).await);
    assert_names(&failures, "after the terminal event");
}

#[tokio::test]
async fn c_a_silent_success_fails_conformance() {
    // Declares `images: false` but accepts the image: the fallback (`Unsupported
    // { images }`) is not applied.
    let mut declared = full_capabilities();
    declared.images = false;
    let target = FaultyTarget {
        inner: scripted_target(full_capabilities()),
        declared,
        fault: Fault::Lies,
    };
    let failures = failures_of(run_scenario(&target, Scenario::MessageImages).await);
    assert_names(&failures, "accepted an image block silently");
}

#[tokio::test]
async fn c_an_event_of_an_absent_capability_fails_conformance() {
    // `thinking: false`, yet the plain turn carries reasoning.
    let caps = Capabilities::none();
    let broken = Script::new(caps.clone()).with_turn(vec![
        steps::thinking("I should not be here"),
        steps::text("done"),
        steps::done(&caps),
    ]);
    let target = scripted_target(caps).with_script(Scenario::Raisonnement, broken);
    let failures = failures_of(run_scenario(&target, Scenario::Raisonnement).await);
    assert_names(&failures, "`thinking` is false");
}

/// A target that stages nothing for one scenario.
struct Unstaged {
    inner: ScriptedTarget,
    missing: Scenario,
}

#[async_trait]
impl ConformanceTarget for Unstaged {
    fn name(&self) -> &str {
        "unstaged"
    }

    fn provider(&self) -> Arc<dyn AgentProvider> {
        self.inner.provider()
    }

    async fn prepare(&self, scenario: Scenario) -> Option<Prepared> {
        if scenario == self.missing {
            return None;
        }
        self.inner.prepare(scenario).await
    }
}

#[tokio::test]
async fn c_a_declared_capability_left_unstaged_is_refused() {
    let target = Unstaged {
        inner: scripted_target(full_capabilities()),
        missing: Scenario::Reprise,
    };
    let report = run_all(&target).await;
    assert!(matches!(
        report.outcome(Scenario::Reprise),
        Some(ScenarioOutcome::NotStaged(_))
    ));
    let problems = report.problems();
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(problems[0].contains("a declared capability must be proven"));
    let panic = std::panic::catch_unwind(|| report.assert_conformant());
    assert!(panic.is_err(), "assert_conformant must panic");
}

#[tokio::test]
async fn c_an_absent_capability_left_unstaged_still_has_its_fallback_verified() {
    // The target stages nothing for `reprise`, and `resume` is absent: the suite
    // falls back to the plain-turn staging and verifies `Unsupported { resume }`.
    let target = Unstaged {
        inner: scripted_target(Capabilities::none()),
        missing: Scenario::Reprise,
    };
    assert_eq!(
        run_scenario(&target, Scenario::Reprise).await,
        ScenarioOutcome::FallbackVerified
    );
}

#[tokio::test]
async fn c_a_turn_that_hangs_fails_instead_of_hanging_the_test() {
    let caps = full_capabilities();
    let stuck = Script::new(caps.clone()).with_turn(vec![steps::text("…"), Step::AwaitInterrupt]);
    let target = scripted_target(caps).with_script(Scenario::TourTexteSimple, stuck);
    let outcome = timeout(
        Duration::from_secs(30),
        run_scenario(&target, Scenario::TourTexteSimple),
    )
    .await
    .expect("the suite bounds its own waits");
    assert_names(&failures_of(outcome), "it hangs");
}

// ---------------------------------------------------------------------------
// (d) record, normalise, replay
// ---------------------------------------------------------------------------

/// A scenario with everything that varies between two runs: identifiers, the
/// working directory, durations.
fn volatile_script(cwd: &str) -> Script {
    let caps = full_capabilities();
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let session = format!("sess-{unique}");
    let tool = format!("toolu_{unique}");
    let request = format!("req-{unique}");
    let mut done = done_event(&caps, StopReason::Completed, None);
    if let AgentEvent::Done {
        duration_ms,
        duration_api_ms,
        provider_session_id,
        ..
    } = &mut done
    {
        *duration_ms = u64::from(unique.as_bytes()[0]) * 7 + 1;
        *duration_api_ms = Some(u64::from(unique.as_bytes()[1]) + 1);
        *provider_session_id = Some(session.clone());
    }
    Script::builder()
        .turn(vec![
            Step::Emit(AgentEvent::SessionStarted {
                provider_session_id: Some(session),
                model: Some("model-a".into()),
                policy_mode: Some(PolicyMode::Ask),
                native_mode: None,
                tools: vec!["Bash".into()],
                mcp_servers: Vec::new(),
                cwd: Some(cwd.into()),
            }),
            steps::tool_call(&tool, "Bash", json!({ "command": format!("ls {cwd}/src") })),
            steps::permission_ask(&request, "Bash", Some(&tool), &[PermissionScope::Once]),
            steps::await_permission(
                &request,
                vec![steps::tool_result(&tool, format!("{cwd}/src/lib.rs"))],
                vec![steps::tool_error(&tool, "denied")],
            ),
            steps::text(format!("one file under {cwd}")),
            Step::Emit(done),
        ])
        .text_turn("second turn")
        .out_of_band(vec![steps::notice("status", json!({ "cwd": cwd }))])
        .build()
}

/// Plays two turns on a fresh session and records everything it emitted.
async fn record(script: Script) -> Transcript {
    let provider = ScriptedProvider::new("recorded", script);
    let session = provider.open(SessionSpec::new("/work")).await.unwrap();
    let out_of_band = session.out_of_band().unwrap();
    let mut recorder = Recorder::new(provider.kind());
    for prompt in ["first", "second"] {
        timeout(
            WAIT,
            recorder.drive_turn(&*session, TurnInput::text(prompt)),
        )
        .await
        .expect("the turn ends")
        .expect("the turn starts");
    }
    session.close().await.unwrap();
    recorder.push_out_of_band(collect(out_of_band).await);
    recorder.finish()
}

#[tokio::test]
async fn d_two_recordings_of_one_scenario_are_identical_once_normalised() {
    let mut first = record(volatile_script("/Users/alice/project")).await;
    let mut second = record(volatile_script("/home/bob/src/project")).await;
    assert_ne!(first.to_json_string(), second.to_json_string());
    normalize(&mut first);
    normalize(&mut second);
    assert_eq!(first.to_json_string(), second.to_json_string());
    let json = first.to_json_string();
    assert!(json.contains("<tool-1>") && json.contains("<req-1>") && json.contains("<cwd>/src"));
    assert!(!json.contains("alice"), "{json}");
}

#[tokio::test]
async fn d_a_replayed_transcript_records_to_the_same_bytes() {
    let mut recorded = record(volatile_script("/Users/alice/project")).await;
    normalize(&mut recorded);
    let expected = recorded.to_json_string();
    assert_eq!(recorded.turns.len(), 2);
    assert!(
        recorded.turns[0]
            .iter()
            .any(|event| matches!(event, AgentEvent::PermissionAsk { .. }))
    );

    // Through the file form, as a host fixture would be.
    let path = std::env::temp_dir().join(format!(
        "nexus-conformance-replay-{}.json",
        uuid::Uuid::new_v4()
    ));
    recorded.save(&path).unwrap();
    let loaded = Transcript::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(loaded, recorded);

    let replay = Script::from_transcript(&loaded);
    assert!(
        replay.turns[0]
            .iter()
            .any(|step| matches!(step, Step::AwaitPermission { .. })),
        "a recorded permission_ask is replayed as a wait for its answer"
    );
    let mut replayed = record(replay).await;
    normalize(&mut replayed);
    assert_eq!(replayed.to_json_string(), expected);
}

// ---------------------------------------------------------------------------
// (e) ScriptedProvider: concurrency and journal
// ---------------------------------------------------------------------------

fn busy_script() -> Script {
    Script::builder()
        .turn(vec![steps::text("working"), Step::AwaitInterrupt])
        .build()
}

#[tokio::test]
async fn e_one_turn_at_a_time_until_the_terminal_event_is_emitted() {
    let provider = ScriptedProvider::new("fake", busy_script());
    let session = provider.open(SessionSpec::new("/work")).await.unwrap();
    let mut first = session.send_turn(TurnInput::text("one")).await.unwrap();
    assert!(matches!(
        timeout(WAIT, first.next()).await.unwrap(),
        Some(AgentEvent::Text { .. })
    ));
    assert_eq!(
        session.send_turn(TurnInput::text("two")).await.err(),
        Some(ProviderError::TurnInProgress)
    );

    let outcome = session.interrupt(InterruptScope::TurnOnly).await.unwrap();
    assert!(outcome.turn_interrupted);
    // The terminal event is EMITTED, not yet read: the session already takes a turn.
    let second = session.send_turn(TurnInput::text("three")).await.unwrap();
    let rest = collect(first).await;
    assert!(matches!(
        rest.as_slice(),
        [AgentEvent::Done {
            stop_reason: StopReason::Interrupted,
            ..
        }]
    ));
    let events = collect(second).await;
    assert!(matches!(&events[0], AgentEvent::Text { text, .. } if text == "echo: three"));

    // Outside a turn an interruption is a no-op that says so.
    let outcome = session
        .interrupt(InterruptScope::TurnAndTools)
        .await
        .unwrap();
    assert_eq!(outcome, InterruptOutcome::default());
}

#[tokio::test]
async fn e_an_interruption_cuts_any_turn_and_errors_its_running_tools() {
    let script = Script::builder()
        .turn(vec![
            steps::tool_call("tool-1", "Bash", json!({})),
            steps::sleep(60_000),
            steps::text("never"),
        ])
        .build();
    let provider = ScriptedProvider::new("fake", script);
    let session = provider.open(SessionSpec::new("/work")).await.unwrap();
    let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    let first = timeout(WAIT, stream.next()).await.unwrap();
    assert!(matches!(first, Some(AgentEvent::ToolCall { .. })));
    let outcome = session
        .interrupt(InterruptScope::TurnAndTools)
        .await
        .unwrap();
    assert_eq!(outcome.tools_cancelled, 1);
    let rest = collect(stream).await;
    assert!(matches!(
        rest.as_slice(),
        [
            AgentEvent::ToolResult { is_error: true, .. },
            AgentEvent::Done {
                stop_reason: StopReason::Interrupted,
                ..
            }
        ]
    ));
}

#[tokio::test]
async fn e_cancel_tools_preserves_the_turn() {
    let caps = full_capabilities();
    let script = Script::builder()
        .turn(vec![
            steps::tool_call("tool-1", "Bash", json!({})),
            Step::AwaitCancel,
            steps::text("still here"),
            steps::done(&caps),
        ])
        .build();
    let provider = ScriptedProvider::new("fake", script);
    let session = provider.open(SessionSpec::new("/work")).await.unwrap();
    let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    timeout(WAIT, stream.next()).await.unwrap();
    let outcome = session.cancel_tools(CancelScope::All).await.unwrap();
    assert_eq!(outcome.tools_cancelled, 1);
    let rest = collect(stream).await;
    assert!(matches!(
        rest.as_slice(),
        [
            AgentEvent::ToolResult { is_error: true, .. },
            AgentEvent::Text { .. },
            AgentEvent::Done {
                stop_reason: StopReason::Completed,
                ..
            }
        ]
    ));
}

#[tokio::test]
async fn e_out_of_band_is_taken_once_and_receives_what_happens_outside_a_turn() {
    let script = Script::builder()
        .out_of_band(vec![steps::notice("status", json!({ "n": 1 }))])
        .build();
    let provider = ScriptedProvider::new("fake", script);
    let session = provider.open(SessionSpec::new("/work")).await.unwrap();
    let mut out_of_band = session.out_of_band().expect("first call");
    assert!(session.out_of_band().is_none(), "second call");
    assert!(matches!(
        timeout(WAIT, out_of_band.next()).await.unwrap(),
        Some(AgentEvent::ProviderNotice { kind, .. }) if kind == "status"
    ));
    // A turn's events go to the turn stream, not out of band.
    let events = collect(session.send_turn(TurnInput::text("hi")).await.unwrap()).await;
    assert_eq!(events.len(), 2);
    session.close().await.unwrap();
    assert_eq!(collect(out_of_band).await, Vec::new());
}

#[tokio::test]
async fn e_close_is_idempotent_and_final() {
    let provider = ScriptedProvider::new("fake", busy_script());
    let session = provider.open(SessionSpec::new("/work")).await.unwrap();
    let stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    assert_eq!(session.close().await, Ok(()));
    assert_eq!(session.close().await, Ok(()));
    let events = collect(stream).await;
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Error {
            error: ProviderError::Closed
        })
    );
    assert_eq!(
        session.send_turn(TurnInput::text("again")).await.err(),
        Some(ProviderError::Closed)
    );
    assert_eq!(
        session.interrupt(InterruptScope::TurnOnly).await,
        Err(ProviderError::Closed)
    );
    assert_eq!(
        session.cancel_tools(CancelScope::All).await,
        Err(ProviderError::Closed)
    );
    assert_eq!(session.set_model("m").await, Err(ProviderError::Closed));
    assert_eq!(
        session.set_policy_mode(PolicyMode::Ask, None).await,
        Err(ProviderError::Closed)
    );
    assert_eq!(
        session
            .answer_permission("r", PermissionDecision::deny())
            .await,
        Err(ProviderError::Closed)
    );
    assert_eq!(
        session
            .answer_question("q", QuestionAnswer::Cancelled)
            .await,
        Err(ProviderError::Closed)
    );
}

#[tokio::test]
async fn e_permission_answers_are_checked() {
    let mut caps = full_capabilities();
    caps.permission_scopes = vec![PermissionScope::Once];
    let script = Script::builder()
        .capabilities(caps.clone())
        .turn(vec![
            steps::permission_ask("r1", "Bash", None, &caps.permission_scopes),
            steps::await_permission("r1", vec![steps::text("allowed")], vec![]),
        ])
        .build();
    let provider = ScriptedProvider::new("fake", script);
    let session = provider.open(SessionSpec::new("/work")).await.unwrap();
    let mut stream = session.send_turn(TurnInput::text("go")).await.unwrap();
    // A request can only be answered once it has been asked.
    let asked = timeout(WAIT, stream.next())
        .await
        .expect("the request arrives");
    assert!(matches!(asked, Some(AgentEvent::PermissionAsk { .. })));
    assert!(matches!(
        session
            .answer_permission("nope", PermissionDecision::allow_once())
            .await,
        Err(ProviderError::InvalidRequest { .. })
    ));
    let always = PermissionDecision::Allow {
        scope: PermissionScope::Always,
        updated_input: None,
    };
    assert_eq!(
        session.answer_permission("r1", always).await,
        Err(ProviderError::unsupported("permission_scope"))
    );
    assert_eq!(
        session
            .answer_permission("r1", PermissionDecision::allow_once())
            .await,
        Ok(())
    );
    assert!(matches!(
        session
            .answer_permission("r1", PermissionDecision::allow_once())
            .await,
        Err(ProviderError::InvalidRequest { .. })
    ));
    let events = collect(stream).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Text { text, .. } if text == "allowed"))
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            stop_reason: StopReason::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn e_absent_capabilities_answer_unsupported_never_a_silent_success() {
    let provider = ScriptedProvider::new("fake", Script::new(Capabilities::none()));
    let unsupported = |capability: &str| Some(ProviderError::unsupported(capability));

    let mut with_mcp = SessionSpec::new("/work");
    with_mcp
        .mcp_servers
        .insert("po".into(), McpServerSpec::stdio("mcp"));
    assert_eq!(
        provider.open(with_mcp.clone()).await.err(),
        unsupported("per_session_mcp")
    );
    // `trust` is a mode like the others: with no sandbox it still opens (decision of
    // 2026-10-07; the sandbox level is information, not a gate).
    let mut trust = SessionSpec::new("/work");
    trust.policy = ToolPolicy::new(PolicyMode::Trust);
    assert!(provider.open(trust).await.is_ok());
    let mut above = SessionSpec::new("/work");
    above.policy = ToolPolicy::new(PolicyMode::AutoEdits);
    above.policy_ceiling = Some(ToolPolicy::new(PolicyMode::Ask));
    assert_eq!(
        provider.open(above).await.err(),
        unsupported("policy_ceiling")
    );
    let token = ResumeToken::new(ProviderKind::Scripted, 1, json!({}));
    assert_eq!(
        provider
            .resume(SessionSpec::new("/work"), token)
            .await
            .err(),
        unsupported("resume")
    );

    let mut mcp_without_tools = Capabilities::none();
    mcp_without_tools.per_session_mcp = true;
    let no_tools = ScriptedProvider::new("fake", Script::new(mcp_without_tools));
    assert!(matches!(
        no_tools.open(with_mcp).await.err(),
        Some(ProviderError::ModelNoTools { .. })
    ));

    let session = provider.open(SessionSpec::new("/work")).await.unwrap();
    assert_eq!(session.resume_token(), None);
    let mut image = TurnInput::text("look");
    image.blocks.push(InputBlock::Image {
        media_type: "image/png".into(),
        data_base64: "AAAA".into(),
    });
    assert_eq!(session.send_turn(image).await.err(), unsupported("images"));
    assert_eq!(
        session.set_model("other").await.err(),
        unsupported("set_model_live")
    );
    assert_eq!(
        session.cancel_tools(CancelScope::All).await.err(),
        unsupported("tool_cancel")
    );
    assert_eq!(
        session
            .cancel_tools(CancelScope::Task { id: "t".into() })
            .await
            .err(),
        unsupported("background_tasks")
    );
    assert_eq!(
        session
            .answer_permission("r", PermissionDecision::allow_once())
            .await
            .err(),
        unsupported("interactive_permissions")
    );
    assert_eq!(
        session
            .answer_question("q", QuestionAnswer::Cancelled)
            .await
            .err(),
        unsupported("native_question")
    );
    // The refused image left no turn running.
    let events = collect(session.send_turn(TurnInput::text("hi")).await.unwrap()).await;
    assert!(events.last().unwrap().is_terminal());
}

#[tokio::test]
async fn e_the_journal_records_every_call_of_the_provider_and_its_sessions() {
    let provider = ScriptedProvider::new("fake", Script::text_turn("hello"));
    let mut spec = SessionSpec::new("/work/project");
    spec.model = Some("model-a".into());
    spec.policy = ToolPolicy::new(PolicyMode::AutoEdits);
    let session = provider.open(spec).await.unwrap();
    collect(session.send_turn(TurnInput::text("hi")).await.unwrap()).await;
    session.set_model("model-b").await.unwrap();
    session
        .set_policy_mode(PolicyMode::PlanOnly, Some("plan"))
        .await
        .unwrap();
    session.interrupt(InterruptScope::TurnOnly).await.unwrap();
    session.cancel_tools(CancelScope::All).await.unwrap();
    let _ = session
        .answer_permission("r1", PermissionDecision::deny())
        .await;
    let _ = session
        .answer_question("q1", QuestionAnswer::Cancelled)
        .await;
    session.close().await.unwrap();
    let token = session.resume_token().expect("resume is declared");
    provider
        .resume(SessionSpec::new("/work/project"), token.clone())
        .await
        .unwrap();

    assert_eq!(
        provider.calls(),
        vec![
            RecordedCall::Open {
                model: Some("model-a".into()),
                cwd: PathBuf::from("/work/project"),
                policy: ToolPolicy::new(PolicyMode::AutoEdits),
            },
            RecordedCall::SendTurn(TurnInput::text("hi")),
            RecordedCall::SetModel("model-b".into()),
            RecordedCall::SetPolicyMode {
                mode: PolicyMode::PlanOnly,
                native: Some("plan".into()),
            },
            RecordedCall::Interrupt(InterruptScope::TurnOnly),
            RecordedCall::CancelTools(CancelScope::All),
            RecordedCall::AnswerPermission {
                request_id: "r1".into(),
                decision: PermissionDecision::deny(),
            },
            RecordedCall::AnswerQuestion {
                question_id: "q1".into(),
                answer: QuestionAnswer::Cancelled,
            },
            RecordedCall::Close,
            RecordedCall::Resume { token },
        ]
    );
    provider.clear_calls();
    assert!(provider.calls().is_empty());
}

#[tokio::test]
async fn e_a_script_is_a_json_fixture() {
    let script = volatile_script("/work");
    let json = serde_json::to_string(&script).unwrap();
    let back: Script = serde_json::from_str(&json).unwrap();
    assert_eq!(back, script);
    // A hand-written fixture needs only what it uses.
    let minimal: Script = serde_json::from_value(json!({
        "capabilities": Capabilities::none(),
        "turns": [[
            { "step": "emit", "type": "text", "text": "hi" },
            { "step": "await_interrupt" }
        ]]
    }))
    .unwrap();
    assert_eq!(minimal.turns[0].len(), 2);
}
