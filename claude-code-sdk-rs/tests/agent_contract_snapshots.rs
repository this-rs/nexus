//! Frozen JSON shapes of the agent contract.
//!
//! `tests/snapshots/agent_contract_v<N>.json` holds one sample of every
//! serialised shape of `nexus_claude::agent`. The file name carries
//! `CONTRACT_VERSION`: changing a shape without bumping the version makes this
//! test fail, and bumping the version requires a new snapshot file, so a wire
//! change can never slip through as an incidental diff.
//!
//! Regenerate with `UPDATE_AGENT_SNAPSHOTS=1 cargo test -p nexus-claude --test agent_contract_snapshots`.
//! The backend and the frontend copy this file as their shared fixture (A44).

use std::collections::BTreeMap;
use std::path::PathBuf;

use nexus_claude::agent::*;
use serde_json::{Value, json};

fn event_samples() -> Vec<AgentEvent> {
    vec![
        AgentEvent::SessionStarted {
            provider_session_id: Some("sess-1".into()),
            model: Some("model-a".into()),
            policy_mode: Some(PolicyMode::AutoEdits),
            native_mode: Some("acceptEdits".into()),
            tools: vec!["Read".into(), "mcp__po__plan".into()],
            mcp_servers: vec![McpServerStatus {
                name: "po".into(),
                status: "connected".into(),
            }],
            cwd: Some("/work".into()),
        },
        AgentEvent::UserEcho {
            text: "hello".into(),
            seq: Some(1),
            parent: None,
        },
        AgentEvent::Text {
            text: "hi".into(),
            seq: Some(2),
            parent: Some("toolu_parent".into()),
        },
        AgentEvent::Thinking {
            text: "hmm".into(),
            signature: Some("sig".into()),
            seq: Some(2),
            parent: None,
        },
        AgentEvent::Delta {
            kind: DeltaKind::ToolInput,
            text: "{\"a\":".into(),
            index: Some(1),
            tool_call_id: Some("toolu_1".into()),
            parent: None,
        },
        AgentEvent::ToolCall {
            id: "toolu_1".into(),
            name: "Bash".into(),
            input: json!({"command": "ls"}),
            category: ToolCategory::Command,
            canonical: Some("Bash".into()),
            input_complete: true,
            seq: Some(2),
            parent: None,
        },
        AgentEvent::ToolResult {
            id: "toolu_1".into(),
            output: Some(ToolOutput::Text("a.txt".into())),
            is_error: false,
            seq: Some(3),
            parent: None,
        },
        AgentEvent::PermissionAsk {
            request_id: "req-1".into(),
            tool_name: "Bash".into(),
            input: json!({"command": "rm x"}),
            category: ToolCategory::Command,
            canonical: Some("Bash".into()),
            tool_call_id: Some("toolu_2".into()),
            scopes: vec![PermissionScope::Once, PermissionScope::Session],
            parent: None,
        },
        AgentEvent::Question {
            question_id: "req-2".into(),
            tool_call_id: Some("toolu_3".into()),
            reply: QuestionReply::Turn,
            questions: vec![QuestionSpec {
                question: "Which one?".into(),
                header: Some("Choice".into()),
                options: vec![QuestionOption {
                    label: "A".into(),
                    description: Some("the first".into()),
                }],
                multi_select: false,
            }],
            input: json!({"questions": []}),
            parent: None,
        },
        AgentEvent::Compaction {
            phase: CompactionPhase::Completed,
            trigger: Some(CompactionTrigger::Auto),
            pre_tokens: Some(150_000),
        },
        AgentEvent::BackgroundTasks {
            tasks: vec![BackgroundTask {
                id: "bg-1".into(),
                kind: BackgroundTaskKind::Shell,
                description: "cargo watch".into(),
                status: BackgroundTaskStatus::Running,
                started_at_ms: Some(1_700_000_000_000),
                tool_call_id: Some("toolu_4".into()),
                parent: None,
                pid: Some(4242),
            }],
        },
        AgentEvent::TaskUpdate {
            phase: TaskPhase::Progress,
            task_id: Some("task-1".into()),
            tool_call_id: Some("toolu_5".into()),
            description: Some("explore".into()),
            status: Some("running".into()),
            summary: None,
            event_id: Some("uuid-1".into()),
            data: json!({"uuid": "uuid-1"}),
        },
        AgentEvent::ModelChanged {
            model: "model-b".into(),
        },
        AgentEvent::PolicyModeChanged {
            mode: PolicyMode::PlanOnly,
            native_mode: Some("plan".into()),
        },
        AgentEvent::Done {
            stop_reason: StopReason::MaxTurns,
            subtype: Some("error_max_turns".into()),
            is_error: true,
            result_text: Some("stopped".into()),
            usage: Usage {
                input_tokens: Some(12),
                output_tokens: Some(3),
                cache_read_tokens: Some(100),
                cache_creation_tokens: None,
                reasoning_tokens: None,
                context_tokens: Some(115),
                by_model: vec![ModelUsage {
                    model: "model-a".into(),
                    input_tokens: Some(12),
                    output_tokens: Some(3),
                    cache_read_tokens: Some(100),
                    cache_creation_tokens: None,
                    cost_usd: Some(0.0004),
                    context_window: Some(200_000),
                }],
            },
            cost: Cost {
                usd: Some(0.0004),
                basis: CostBasis::Reported,
            },
            duration_ms: 812,
            duration_api_ms: Some(640),
            num_turns: 50,
            model: Some("model-a".into()),
            provider_session_id: Some("sess-1".into()),
            structured_output: None,
            error: None,
        },
        AgentEvent::Error {
            error: ProviderError::RateLimited {
                retry_after_ms: Some(1500),
            },
        },
        AgentEvent::ProviderNotice {
            kind: "status".into(),
            data: json!({"status": "compacting"}),
        },
    ]
}

fn error_samples() -> Vec<ProviderError> {
    vec![
        ProviderError::CliNotFound {
            program: "claude".into(),
        },
        ProviderError::AuthRequired {
            login_hint: Some("claude login".into()),
        },
        ProviderError::CredentialsLocked,
        ProviderError::Unauthorized,
        ProviderError::unreachable("connection refused"),
        ProviderError::ModelNoTools {
            model: "tiny".into(),
        },
        ProviderError::ContextTooSmall {
            needed: Some(40_000),
            available: Some(32_768),
        },
        ProviderError::RateLimited {
            retry_after_ms: None,
        },
        ProviderError::Overloaded,
        ProviderError::Timeout { after_ms: 30_000 },
        ProviderError::ProcessExited { code: Some(1) },
        ProviderError::protocol("unexpected frame"),
        ProviderError::unsupported("images"),
        ProviderError::TurnInProgress,
        ProviderError::invalid("unknown request id"),
        ProviderError::Closed,
        ProviderError::ModelProtocolMismatch(Box::new(ProtocolMismatch {
            harness: "codex-main".into(),
            provider: "deepseek".into(),
            protocol: "openai_chat".into(),
            accepts: vec!["openai_responses".into()],
        })),
    ]
}

fn claude_code_like_capabilities() -> Capabilities {
    let mut caps = Capabilities::none();
    caps.interactive_permissions = true;
    caps.permission_scopes = vec![
        PermissionScope::Once,
        PermissionScope::Session,
        PermissionScope::Always,
    ];
    caps.sandbox = SandboxLevel::None;
    caps.secret_isolation = true;
    caps.per_session_mcp = true;
    caps.hooks = HookSupport::InProtocol;
    caps.subagents = SubagentSupport::Nested;
    caps.compaction_signal = true;
    caps.thinking = true;
    caps.tools = true;
    caps.context_window = Some(ContextWindow {
        value: 200_000,
        source: ContextWindowSource::Reported,
    });
    caps.set_model_live = true;
    caps.native_question = true;
    caps.tool_cancel = true;
    caps.background_tasks = true;
    caps.resume = true;
    caps.cost = CostBasis::Reported;
    caps
}

fn current_snapshot() -> Value {
    let mut events: BTreeMap<&str, Value> = event_samples()
        .iter()
        .map(|event| (event.type_name(), serde_json::to_value(event).unwrap()))
        .collect();
    // `done` without `error` is the sample above; the classified failure of the
    // turn (contract v2, `done.error`) is its own entry so a renamed field shows.
    let mut done_with_error = event_samples()
        .into_iter()
        .find(|event| event.type_name() == "done")
        .expect("a done sample");
    if let AgentEvent::Done { error, .. } = &mut done_with_error {
        *error = Some(ProviderError::Overloaded);
    }
    events.insert(
        "done_with_error",
        serde_json::to_value(&done_with_error).unwrap(),
    );
    let errors: BTreeMap<&str, Value> = error_samples()
        .iter()
        .map(|error| (error.kind(), serde_json::to_value(error).unwrap()))
        .collect();
    json!({
        "contract_version": CONTRACT_VERSION,
        "events": events,
        "errors": errors,
        "capabilities": {
            "none": Capabilities::none(),
            "claude_code_like": claude_code_like_capabilities(),
        },
        "tool_policy": ToolPolicy {
            mode: PolicyMode::AutoEdits,
            native_mode: Some("acceptEdits".into()),
            allow: vec!["Read".parse().unwrap(), "Bash(git *)".parse().unwrap()],
            deny: vec!["mcp__po__admin".parse().unwrap()],
        },
        "policy_modes": [PolicyMode::PlanOnly, PolicyMode::Ask, PolicyMode::AutoEdits, PolicyMode::Trust],
        "resume_token": ResumeToken::claude_code_session("sess-1"),
        "permission_decisions": [
            PermissionDecision::allow_once(),
            PermissionDecision::Allow { scope: PermissionScope::Session, updated_input: Some(json!({"command": "ls"})) },
            PermissionDecision::Deny { message: Some("no".into()), interrupt: true },
        ],
        "question_answers": [
            QuestionAnswer::Answered { answers: vec![QuestionAnswerItem {
                question: "Which one?".into(), selected: vec!["A".into()], free_text: None,
            }] },
            QuestionAnswer::Cancelled,
        ],
        "interrupt_scopes": [InterruptScope::TurnAndTools, InterruptScope::TurnOnly],
        "cancel_scopes": [CancelScope::All, CancelScope::Task { id: "bg-1".into() }],
        "interrupt_outcome": InterruptOutcome {
            turn_interrupted: true,
            tools_cancelled: 2,
            diagnostic: Some(ProcessDiagnostic { pid: Some(10), killed_pids: vec![11, 12] }),
        },
        "provider_health": ProviderHealth {
            status: HealthStatus::Unavailable,
            version: Some("0.38.0".into()),
            detail: Some("app-server requires a newer codex".into()),
            error: Some(ProviderError::AuthRequired { login_hint: Some("codex login".into()) }),
            login_hint: Some("codex login".into()),
            checked_at_ms: 1_700_000_000_000,
        },
        "model_info": ModelInfo {
            id: "model-a".into(),
            display_name: Some("Model A".into()),
            context_window: Some(ContextWindow { value: 32_768, source: ContextWindowSource::Configured }),
            supports_tools: Some(true),
            supports_images: Some(false),
            supports_thinking: None,
            is_default: true,
            pricing: Some(ModelPrice { input_per_mtok: 0.28, output_per_mtok: 0.42, cache_read_per_mtok: Some(0.028), cache_write_per_mtok: None }),
        },
        "turn_input": TurnInput::text("hello"),
    })
}

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(format!("agent_contract_v{CONTRACT_VERSION}.json"))
}

#[test]
fn every_variant_has_a_sample() {
    let mut event_types: Vec<&str> = event_samples().iter().map(AgentEvent::type_name).collect();
    event_types.sort_unstable();
    let mut expected = AgentEvent::TYPE_NAMES.to_vec();
    expected.sort_unstable();
    assert_eq!(event_types, expected, "one sample per AgentEvent variant");

    let kinds: Vec<&str> = error_samples().iter().map(ProviderError::kind).collect();
    let unique: std::collections::BTreeSet<&str> = kinds.iter().copied().collect();
    assert_eq!(kinds.len(), unique.len());
    assert_eq!(kinds.len(), 17, "one sample per ProviderError variant");
}

#[test]
fn samples_round_trip_and_tags_match_the_accessors() {
    for event in event_samples() {
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], event.type_name());
        let back: AgentEvent = serde_json::from_value(value).unwrap();
        assert_eq!(back, event);
    }
    for error in error_samples() {
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(value["kind"], error.kind());
        let back: ProviderError = serde_json::from_value(value).unwrap();
        assert_eq!(back, error);
    }
    let caps = claude_code_like_capabilities();
    let back: Capabilities = serde_json::from_value(serde_json::to_value(&caps).unwrap()).unwrap();
    assert_eq!(back, caps);
}

#[test]
fn optional_fields_may_be_absent_on_the_wire() {
    // What a minimal provider (or an older peer) sends must still parse.
    let minimal = [
        json!({"type": "session_started"}),
        json!({"type": "text", "text": "x"}),
        json!({"type": "tool_call", "id": "1", "name": "T", "input": {}, "input_complete": true}),
        json!({"type": "tool_result", "id": "1"}),
        json!({"type": "done", "stop_reason": "completed"}),
        json!({"type": "provider_notice", "kind": "k"}),
    ];
    for value in minimal {
        serde_json::from_value::<AgentEvent>(value.clone())
            .unwrap_or_else(|e| panic!("{value} must parse: {e}"));
    }
    let done: AgentEvent =
        serde_json::from_value(json!({"type": "done", "stop_reason": "completed"})).unwrap();
    match done {
        AgentEvent::Done { cost, usage, .. } => {
            assert_eq!(cost.usd, None, "an absent cost is unknown, not zero");
            assert_eq!(cost.basis, CostBasis::Unknown);
            assert_eq!(
                usage.input_tokens, None,
                "an absent counter is unknown, not zero"
            );
        },
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn retryable_is_exactly_the_documented_set() {
    let retryable: Vec<&str> = error_samples()
        .iter()
        .filter(|error| error.retryable())
        .map(ProviderError::kind)
        .collect();
    assert_eq!(
        retryable,
        [
            "endpoint_unreachable",
            "rate_limited",
            "overloaded",
            "timeout"
        ]
    );
}

#[test]
fn shapes_match_the_committed_snapshot_of_this_contract_version() {
    let current = serde_json::to_string_pretty(&current_snapshot()).unwrap() + "\n";
    let path = snapshot_path();
    if std::env::var_os("UPDATE_AGENT_SNAPSHOTS").is_some() {
        std::fs::write(&path, &current).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "{} is missing: CONTRACT_VERSION changed (or this is the first run). \
             Review the shapes, then regenerate with UPDATE_AGENT_SNAPSHOTS=1.",
            path.display()
        )
    });
    assert_eq!(
        committed.replace("\r\n", "\n"),
        current,
        "a serialised shape of the agent contract changed: bump agent::CONTRACT_VERSION, \
         regenerate the snapshot (UPDATE_AGENT_SNAPSHOTS=1) and update docs/agent-contract.md"
    );
}
