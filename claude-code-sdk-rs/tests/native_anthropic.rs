//! The native harness on the Anthropic Messages protocol (CTX5): a real `NativeProvider`
//! on a real `AnthropicEndpoint`, talking to a fake Anthropic API (`fake_openai` serving
//! `/v1/messages` with the real event framing) and to `fake_mcp` over stdio. No network
//! beyond 127.0.0.1, no real key.
//!
//! - the conformance suite of the agent contract, as for the OpenAI-compatible endpoint
//!   (`native_conformance.rs`), by hand and from a `ProviderRegistry` instance;
//! - the three `cache_control` points (tools, system, last turn), read IN THE REQUEST
//!   the fake received, request after request of a tool round;
//! - the thinking blocks sent back verbatim, signature and order kept, and dropped
//!   once a summary rewrote the history before them.

#[path = "support/fake_anthropic.rs"]
mod fake_anthropic;
#[path = "support/fake_openai.rs"]
mod fake_openai;
#[path = "support/native.rs"]
mod native;

use std::sync::Arc;

use async_trait::async_trait;
use fake_anthropic::*;
use fake_openai::FakeOpenAi;
use native::{collect, stdio_mcp, terminal};
use nexus_claude::agent::{
    AgentEvent, AgentProvider, AgentSession, Capabilities, CostBasis, CredentialRef,
    CredentialResolver, ModelInfo, ModelPrice, ProviderError, ProviderHealth,
    ProviderInstanceConfig, ProviderKind, ProviderRegistry, ResumeToken, Secret, SecurityGate,
    SessionSpec, StopReason, SystemPromptMode, SystemPromptSpec, TurnInput,
};
use nexus_claude::model::{ChatMessage, PriceTable};
use nexus_claude::providers::native::{
    MemoryTranscriptStore, NativeConfig, NativeProvider, TranscriptStore,
};
use nexus_claude::testkit::conformance::{
    ConformanceTarget, Prepared, Scenario, ScenarioOutcome, run_all, run_scenario,
};
use serde_json::{Value, json};

const KEY: &str = "sk-ant-test-0123456789";

/// Answers every reference with [`KEY`].
struct FixedKey;

#[async_trait]
impl CredentialResolver for FixedKey {
    async fn resolve(
        &self,
        _instance: &str,
        reference: &CredentialRef,
    ) -> Result<Option<Secret>, ProviderError> {
        Ok(match reference {
            CredentialRef::None => None,
            _ => Some(Secret::new(KEY)),
        })
    }
}

fn price() -> ModelPrice {
    ModelPrice {
        input_per_mtok: 1.0,
        output_per_mtok: 2.0,
        cache_read_per_mtok: None,
        cache_write_per_mtok: None,
    }
}

fn config(scenario: Option<Scenario>) -> NativeConfig {
    let mut config = NativeConfig::new("native-test");
    config.default_model = Some("m".to_owned());
    config.prices = PriceTable::new().with("m", price());
    if scenario == Some(Scenario::Compaction) {
        config.context_window = Some(12_000);
        config.compaction.keep_recent = 2;
    }
    config
}

fn echo(id: &'static str) -> (&'static str, &'static str, Value) {
    (id, "mcp__fake__echo", json!({"text": id}))
}

/// The routes of a scenario. Routes are consumed in order; the last one answers any
/// request left over with plain text (and a thinking block).
fn script(scenario: Scenario, vision: bool) -> Vec<Value> {
    let p = scenario.prompt();
    let p = Some(p.as_str());
    let tool = Some(TOOL_RESULT);
    let mut routes = vec![models_route(128_000, vision)];
    let tool_call = |name: &'static str| ("c1", name, json!({"text": "x"}));
    if vision && scenario == Scenario::MessageImages {
        routes.push(text_reply(
            Some(IMAGE),
            "the pixel is seen",
            None,
            Some((100, 10)),
        ));
    }
    match scenario {
        Scenario::AppelOutilResultat => {
            routes.push(tool_reply(
                p,
                &[echo("c1")],
                Some(("use echo", "sig-1")),
                Some((120, 12)),
            ));
            routes.push(text_reply(tool, "the tool said hi", None, Some((150, 8))));
        },
        Scenario::OutilsParalleles => {
            routes.push(tool_reply(
                p,
                &[echo("c1"), echo("c2")],
                None,
                Some((120, 20)),
            ));
            routes.push(text_reply(tool, "both done", None, Some((200, 8))));
        },
        Scenario::PermissionAccordee
        | Scenario::PermissionRefusee
        | Scenario::PermissionHorsTour => {
            routes.push(tool_reply(
                p,
                &[tool_call("mcp__fake__write")],
                None,
                Some((120, 12)),
            ));
            routes.push(text_reply(tool, "carried on", None, Some((150, 8))));
        },
        Scenario::InterruptionOutil => {
            routes.push(tool_reply(
                p,
                &[("c1", "mcp__fake__slow", json!({}))],
                None,
                None,
            ));
        },
        Scenario::AnnulationTourPreserve => {
            routes.push(tool_reply(
                p,
                &[("c1", "mcp__fake__slow", json!({}))],
                None,
                None,
            ));
            routes.push(text_reply(
                tool,
                "recovered after the cancellation",
                None,
                None,
            ));
        },
        Scenario::ProcessusMort => {
            routes.push(tool_reply(
                p,
                &[("c1", "mcp__fake__die", json!({}))],
                None,
                None,
            ));
        },
        Scenario::Compaction => {
            routes.push(tool_reply(p, &[echo("c1")], None, Some((100, 10))));
            routes.push(tool_reply(tool, &[echo("c2")], None, Some((10_000, 10))));
            routes.push(text_reply(
                Some("compacting the history"),
                "SUMMARY-TEXT",
                None,
                Some((50, 20)),
            ));
            routes.push(text_reply(
                tool,
                "all done after compaction",
                None,
                Some((300, 10)),
            ));
        },
        Scenario::InterruptionEnFlux | Scenario::FermetureIdempotente => {
            routes.push(busy_reply(p));
        },
        Scenario::TourConcurrent => {
            routes.push(busy_reply(p));
            routes.push(text_reply(p, "the second turn", None, Some((100, 10))));
        },
        Scenario::QuestionUtilisateur => {
            routes.push(tool_reply(
                p,
                &[(
                    "q1",
                    "AskUserQuestion",
                    json!({"questions": [{"question": "Which one?", "header": "Pick",
                        "options": [{"label": "A"}, {"label": "B"}]}]}),
                )],
                None,
                Some((100, 10)),
            ));
        },
        Scenario::ErreurRetryable => {
            routes.push(status_reply(p, 529, "overloaded_error", "Overloaded"));
            routes.push(text_reply(p, "recovered", None, Some((100, 10))));
        },
        _ => {},
    }
    routes.push(text_reply(
        None,
        "hello from the model",
        Some("thinking it over"),
        Some((100, 10)),
    ));
    routes
}

fn needs_tools(scenario: Scenario) -> bool {
    matches!(
        scenario,
        Scenario::AppelOutilResultat
            | Scenario::OutilsParalleles
            | Scenario::PermissionAccordee
            | Scenario::PermissionRefusee
            | Scenario::PermissionHorsTour
            | Scenario::InterruptionOutil
            | Scenario::AnnulationTourPreserve
            | Scenario::Compaction
            | Scenario::ProcessusMort
    )
}

struct Staging {
    _server: FakeOpenAi,
    _cwd: tempfile::TempDir,
}

#[derive(Clone, Copy, PartialEq)]
enum Route {
    /// `NativeProvider::new` on an `AnthropicEndpoint`, by hand.
    Direct,
    /// A `native` instance with `model_protocol: anthropic_messages`, from the registry.
    Registry,
}

fn direct(server: &FakeOpenAi, config: NativeConfig) -> NativeProvider {
    NativeProvider::new(
        config,
        anthropic_endpoint(
            server.base_url(),
            CredentialRef::Env("UNUSED".into()),
            Arc::new(FixedKey),
        ),
    )
}

fn registry_instance(scenario: Option<Scenario>, url: String) -> ProviderInstanceConfig {
    let mut instance = ProviderInstanceConfig::native("native-test", url)
        .with_extension("model_protocol", json!("anthropic_messages"))
        .with_extension("anthropic_auth", json!("api_key"))
        .with_credential(CredentialRef::Vault("anthropic-test".into()))
        .with_default_model("m")
        .with_price("m", price());
    if scenario == Some(Scenario::Compaction) {
        instance = instance
            .with_context_window(12_000)
            .with_extension("compaction_keep_recent", json!(2));
    }
    instance
}

async fn from_registry(
    scenario: Option<Scenario>,
    url: String,
    store: Option<Arc<MemoryTranscriptStore>>,
) -> Option<Arc<dyn AgentProvider>> {
    let registry = ProviderRegistry::new(Arc::new(FixedKey));
    registry.activate_security_gate(SecurityGate::attest("native-anthropic"));
    if let Some(store) = store {
        registry.set_transcript_store(store);
    }
    registry.upsert(registry_instance(scenario, url)).ok()?;
    registry
        .refresh_capabilities("native-test", "m")
        .await
        .ok()?;
    registry.get("native-test").ok()
}

struct AnthropicTarget {
    route: Route,
    vision: bool,
    base: Arc<dyn AgentProvider>,
    _base_server: FakeOpenAi,
}

impl AnthropicTarget {
    async fn new(route: Route, vision: bool) -> Self {
        let server = FakeOpenAi::start(json!([models_route(128_000, vision)]));
        let provider: Arc<dyn AgentProvider> = match route {
            Route::Direct => {
                let provider = Arc::new(direct(&server, config(None)));
                provider
                    .refresh_capabilities("m")
                    .await
                    .expect("the probe of the base provider");
                provider
            },
            Route::Registry => from_registry(None, server.base_url(), None)
                .await
                .expect("the base provider from the registry"),
        };
        Self {
            route,
            vision,
            base: provider,
            _base_server: server,
        }
    }
}

/// Starts a turn when a session opens and drops its stream (`permission_hors_tour`).
struct DetachedTurn {
    inner: Arc<dyn AgentProvider>,
    prompt: String,
}

#[async_trait]
impl AgentProvider for DetachedTurn {
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

    fn capabilities(&self, model: Option<&str>) -> Capabilities {
        self.inner.capabilities(model)
    }

    async fn open(&self, spec: SessionSpec) -> Result<Arc<dyn AgentSession>, ProviderError> {
        let session = self.inner.open(spec).await?;
        drop(
            session
                .send_turn(TurnInput::text(self.prompt.clone()))
                .await?,
        );
        Ok(session)
    }

    async fn resume(
        &self,
        spec: SessionSpec,
        token: ResumeToken,
    ) -> Result<Arc<dyn AgentSession>, ProviderError> {
        self.inner.resume(spec, token).await
    }
}

#[async_trait]
impl ConformanceTarget for AnthropicTarget {
    fn name(&self) -> &str {
        match self.route {
            Route::Direct => "native (AnthropicEndpoint on a fake Messages API, fake_mcp)",
            Route::Registry => {
                "native anthropic_messages from ProviderRegistry (fake Messages API)"
            },
        }
    }

    fn provider(&self) -> Arc<dyn AgentProvider> {
        self.base.clone()
    }

    async fn prepare(&self, scenario: Scenario) -> Option<Prepared> {
        let server = FakeOpenAi::start(json!(script(scenario, self.vision)));
        let store = Arc::new(MemoryTranscriptStore::new());
        let provider: Arc<dyn AgentProvider> = match self.route {
            Route::Direct => {
                let provider = Arc::new(
                    direct(&server, config(Some(scenario))).with_transcript_store(store.clone()),
                );
                provider.refresh_capabilities("m").await.ok()?;
                provider
            },
            Route::Registry => {
                from_registry(Some(scenario), server.base_url(), Some(store.clone())).await?
            },
        };
        let cwd = tempfile::tempdir().ok()?;
        let mut spec = SessionSpec::new(cwd.path());
        spec.model = Some("m".to_owned());
        if needs_tools(scenario) {
            spec.mcp_servers.insert("fake".to_owned(), stdio_mcp(None));
        }
        let mut resume = None;
        if scenario == Scenario::Reprise {
            store
                .save(
                    "seed1",
                    &[
                        ChatMessage::user("earlier"),
                        ChatMessage::assistant("earlier answer"),
                    ],
                )
                .ok()?;
            resume = Some(ResumeToken::new(
                ProviderKind::Native,
                1,
                json!({"transcript_id": "seed1"}),
            ));
        }
        let provider: Arc<dyn AgentProvider> = if scenario == Scenario::PermissionHorsTour {
            Arc::new(DetachedTurn {
                inner: provider,
                prompt: scenario.prompt(),
            })
        } else {
            provider
        };
        let mut prepared = Prepared::new(provider, spec);
        prepared.resume = resume;
        prepared.guard = Some(Box::new(Staging {
            _server: server,
            _cwd: cwd,
        }));
        Some(prepared)
    }
}

async fn check_conformance(route: Route) {
    let target = AnthropicTarget::new(route, false).await;
    let report = run_all(&target).await;
    eprintln!("{}", report.summary());
    report.assert_conformant();
    for absent in [
        Scenario::QuestionUtilisateur,
        Scenario::AnnulationTache,
        Scenario::MessageImages,
        Scenario::SousAgent,
    ] {
        assert_eq!(
            report.outcome(absent),
            Some(&ScenarioOutcome::FallbackVerified),
            "{absent} must verify its fallback\n{}",
            report.summary()
        );
    }
    for present in [
        Scenario::AppelOutilResultat,
        Scenario::OutilsParalleles,
        Scenario::PermissionAccordee,
        Scenario::PermissionRefusee,
        Scenario::PermissionHorsTour,
        Scenario::InterruptionOutil,
        Scenario::AnnulationTourPreserve,
        Scenario::Compaction,
        Scenario::Reprise,
        Scenario::ChangementModele,
        Scenario::DirectiveModele,
        Scenario::Raisonnement,
        Scenario::FinUsageCout,
        Scenario::HorsTour,
        Scenario::ProcessusMort,
    ] {
        assert_eq!(
            report.outcome(present),
            Some(&ScenarioOutcome::Passed),
            "{present} must pass for real\n{}",
            report.summary()
        );
    }
    let caps = &report.capabilities;
    assert!(caps.tool_cancel && caps.resume && caps.interactive_permissions && caps.thinking);
    assert_eq!(caps.cost, CostBasis::Priced);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_native_harness_on_anthropic_messages_passes_the_conformance_suite() {
    check_conformance(Route::Direct).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_native_anthropic_instance_built_by_the_registry_passes_the_conformance_suite() {
    check_conformance(Route::Registry).await;
}

/// A model the catalogue says reads images plays `message_images` for real: the image
/// reaches the fake as a base64 `image` block.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_vision_model_plays_message_images_for_real_on_anthropic_messages() {
    let target = AnthropicTarget::new(Route::Direct, true).await;
    assert!(target.provider().capabilities(Some("m")).images);
    let outcome = run_scenario(&target, Scenario::MessageImages).await;
    assert_eq!(outcome, ScenarioOutcome::Passed, "{outcome:?}");
}

// ---------------------------------------------------------------------------
// What the requests carry
// ---------------------------------------------------------------------------

/// A session with a system prompt and the `fake_mcp` tools, on the fake at `server`.
async fn open_session(server: &FakeOpenAi) -> Arc<dyn AgentSession> {
    let provider = Arc::new(direct(server, config(None)));
    provider.refresh_capabilities("m").await.expect("probe");
    let mut spec = SessionSpec::new(std::env::temp_dir());
    spec.model = Some("m".to_owned());
    spec.system_prompt = Some(SystemPromptSpec {
        text: "You are terse.".to_owned(),
        mode: SystemPromptMode::Replace,
    });
    spec.mcp_servers.insert("fake".to_owned(), stdio_mcp(None));
    provider.open(spec).await.expect("open")
}

/// Every place of a request that carries a `cache_control`, as a JSON path.
fn cache_points(body: &Value) -> Vec<String> {
    fn walk(value: &Value, path: String, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, inner) in map {
                    if key == "cache_control" {
                        assert_eq!(*inner, json!({"type": "ephemeral"}), "at {path}");
                        out.push(path.clone());
                    } else {
                        walk(inner, format!("{path}.{key}"), out);
                    }
                }
            },
            Value::Array(items) => {
                for (index, inner) in items.iter().enumerate() {
                    walk(inner, format!("{path}[{index}]"), out);
                }
            },
            _ => {},
        }
    }
    let mut out = Vec::new();
    walk(body, String::new(), &mut out);
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_three_cache_points_are_in_every_request_of_a_tool_round() {
    let server = FakeOpenAi::start(json!([
        models_route(128_000, false),
        tool_reply(Some("go"), &[echo("c1")], Some(("use echo", "sig-1")), None),
        text_reply(Some(TOOL_RESULT), "done", None, None),
    ]));
    let session = open_session(&server).await;
    let events = collect(session.send_turn(TurnInput::text("go")).await.unwrap()).await;
    assert!(
        matches!(
            terminal(&events),
            AgentEvent::Done {
                stop_reason: StopReason::Completed,
                ..
            }
        ),
        "{events:?}"
    );

    let requests = server.requests();
    let posts: Vec<&Value> = requests
        .iter()
        .filter(|r| r["path"] == "/v1/messages")
        .collect();
    assert_eq!(posts.len(), 2, "the tool call, then the answer");
    for post in &posts {
        // The protocol's own headers, and the key as `x-api-key`, never as a Bearer.
        assert_eq!(post["header_values"]["anthropic-version"], "2023-06-01");
        assert_eq!(post["x_api_key_present"], true);
        assert_eq!(post["authorization_present"], false);
        assert!(
            !post.to_string().contains(KEY),
            "the key value is never logged"
        );
    }
    for body in message_bodies(&requests) {
        let tools = body["tools"].as_array().expect("tools offered");
        let last_tool = tools.len() - 1;
        let messages = body["messages"].as_array().unwrap();
        let last = messages.len() - 1;
        let last_block = messages[last]["content"].as_array().unwrap().len() - 1;
        assert_eq!(
            cache_points(&body),
            vec![
                format!(".messages[{last}].content[{last_block}]"),
                ".system[0]".to_owned(),
                format!(".tools[{last_tool}]"),
            ],
            "exactly the three points: the last turn, the system, the tools\n{body}"
        );
        assert_eq!(body["system"][0]["text"], "You are terse.");
        assert_eq!(body["stream"], true);
        assert!(body["max_tokens"].as_u64().is_some(), "the API requires it");
    }
    // The second request's last turn is the tool result: that is where the marker moved.
    let second = &message_bodies(&requests)[1];
    let last = second["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"][0]["type"], "tool_result");
    assert_eq!(last["content"][0]["tool_use_id"], "c1");
    assert!(last["content"][0].get("cache_control").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_thinking_block_goes_back_verbatim_before_its_tool_call() {
    let server = FakeOpenAi::start(json!([
        models_route(128_000, false),
        tool_reply(
            Some("go"),
            &[echo("c1")],
            Some(("I will echo", "sig-XYZ")),
            None
        ),
        text_reply(Some(TOOL_RESULT), "done", Some("all good"), None),
        text_reply(Some("again"), "second turn", None, None),
    ]));
    let session = open_session(&server).await;
    let events = collect(session.send_turn(TurnInput::text("go")).await.unwrap()).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Thinking { text, .. } if text == "I will echo")),
        "the reasoning reaches the host: {events:?}"
    );
    collect(session.send_turn(TurnInput::text("again")).await.unwrap()).await;

    let bodies = message_bodies(&server.requests());
    assert_eq!(bodies.len(), 3);
    let assistant = &bodies[1]["messages"][1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(
        assistant["content"],
        json!([
            {"type": "thinking", "thinking": "I will echo", "signature": "sig-XYZ"},
            {"type": "tool_use", "id": "c1", "name": "mcp__fake__echo", "input": {"text": "c1"}},
        ]),
        "signature and order kept, nothing added"
    );
    // The next turn replays both assistant messages unchanged: the history only grows.
    let third = &bodies[2]["messages"];
    assert_eq!(third[1], bodies[1]["messages"][1]);
    assert_eq!(
        third[3]["content"],
        json!([
            {"type": "thinking", "thinking": "all good", "signature": "sig-final"},
            {"type": "text", "text": "done"},
        ])
    );
    // Strip the moving marker: the second request is a prefix of the third.
    let strip = |messages: &Value| {
        let mut messages = messages.clone();
        for message in messages.as_array_mut().unwrap() {
            for block in message["content"].as_array_mut().unwrap() {
                block.as_object_mut().unwrap().remove("cache_control");
            }
        }
        messages
    };
    let (second, third) = (strip(&bodies[1]["messages"]), strip(third));
    assert_eq!(
        second.as_array().unwrap()[..],
        third.as_array().unwrap()[..second.as_array().unwrap().len()]
    );
}

/// A summary rewrites the history before the kept messages: their thinking blocks are no
/// longer valid (the API checks that nothing before a block changed) and are not replayed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rewritten_history_replays_no_thinking_block() {
    let server = FakeOpenAi::start(json!([
        models_route(128_000, false),
        tool_reply(
            Some("go"),
            &[echo("c1")],
            Some(("first", "sig-1")),
            Some((100, 10))
        ),
        // The second answer reports a prompt near the 12 000-token window.
        tool_reply(
            Some(TOOL_RESULT),
            &[echo("c2")],
            Some(("second", "sig-2")),
            Some((10_000, 10))
        ),
        text_reply(
            Some("compacting the history"),
            "SUMMARY-TEXT",
            None,
            Some((50, 20))
        ),
        text_reply(Some(TOOL_RESULT), "all done", None, Some((300, 10))),
    ]));
    let mut config = config(Some(Scenario::Compaction));
    config.compaction.keep_recent = 2;
    let provider = Arc::new(direct(&server, config));
    provider.refresh_capabilities("m").await.expect("probe");
    let mut spec = SessionSpec::new(std::env::temp_dir());
    spec.model = Some("m".to_owned());
    spec.mcp_servers.insert("fake".to_owned(), stdio_mcp(None));
    let session = provider.open(spec).await.expect("open");
    let events = collect(session.send_turn(TurnInput::text("go")).await.unwrap()).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compaction { .. })),
        "the history was compacted: {events:?}"
    );
    let bodies = message_bodies(&server.requests());
    // Before the rewrite, the thinking of the first step went back with it.
    assert!(bodies[1].to_string().contains("sig-1"), "{}", bodies[1]);
    let last = bodies.last().unwrap();
    assert!(
        last.to_string().contains("SUMMARY-TEXT"),
        "the request after the summary: {last}"
    );
    assert!(
        !last.to_string().contains("\"signature\""),
        "no thinking block replayed after the summary: {last}"
    );
}

/// An error event inside the stream ends the turn with the typed error, the key masked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_error_event_in_the_stream_ends_the_turn_with_its_type() {
    let server = FakeOpenAi::start(json!([
        models_route(128_000, false),
        stream_error_reply(None, "invalid_request_error", &format!("bad key {KEY}")),
    ]));
    let session = open_session(&server).await;
    let events = collect(session.send_turn(TurnInput::text("go")).await.unwrap()).await;
    let text = format!("{events:?}");
    assert!(!text.contains(KEY), "{text}");
    assert!(
        matches!(
            terminal(&events),
            AgentEvent::Error { .. } | AgentEvent::Done { .. }
        ),
        "{events:?}"
    );
}

/// A native instance of this protocol needs no endpoint (Anthropic's own API is the
/// default) and takes no OpenAI dialect.
#[test]
fn a_native_anthropic_instance_needs_no_endpoint_and_refuses_openai_dialects() {
    let first_party = ProviderInstanceConfig::new("claude-api", ProviderKind::Native)
        .with_extension("model_protocol", json!("anthropic_messages"))
        .with_credential(CredentialRef::Vault("anthropic-key".into()));
    assert!(first_party.validate().is_ok());
    let side = first_party.model_provider().expect("a model side");
    assert_eq!(side.protocol.as_str(), "anthropic_messages");
    assert!(side.is_first_party());
    assert!(
        first_party
            .clone()
            .with_preset("deepseek")
            .validate()
            .is_err(),
        "an OpenAI dialect means nothing on this wire"
    );
    for bad in [
        ("model_protocol", json!("openai_responses")),
        ("anthropic_auth", json!("basic")),
        ("anthropic_thinking", json!("adaptive")),
        ("anthropic_prompt_cache", json!("yes")),
    ] {
        assert!(
            first_party
                .clone()
                .with_extension(bad.0, bad.1.clone())
                .validate()
                .is_err(),
            "{bad:?}"
        );
    }
    // Without the extension, a native instance is OpenAI-compatible and needs an endpoint.
    assert!(
        ProviderInstanceConfig::new("x", ProviderKind::Native)
            .validate()
            .is_err()
    );
}

/// What a `before_turn` hook is told, recorded.
#[derive(Default)]
struct Recorder {
    contexts: std::sync::Mutex<Vec<nexus_claude::agent::TurnContext>>,
}

#[async_trait]
impl nexus_claude::agent::SessionHooks for Recorder {
    async fn before_turn(
        &self,
        ctx: &nexus_claude::agent::TurnContext,
    ) -> nexus_claude::agent::TurnDirective {
        self.contexts.lock().unwrap().push(ctx.clone());
        nexus_claude::agent::TurnDirective::none()
    }
}

/// T3 (mutation testing): Anthropic reports the cache writes apart. They are in the prompt
/// (its size is input + cache reads + cache writes) and they spend the token budget (input
/// + cache writes + output; the cache reads do not).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cache_writes_count_in_the_prompt_size_and_in_the_tokens_spent_but_cache_reads_do_not() {
    let mut first = text_reply(Some("alpha"), "ok", None, Some((100, 10)));
    first["sse"][0]["message"]["usage"]["cache_creation_input_tokens"] = json!(50);
    first["sse"][0]["message"]["usage"]["cache_read_input_tokens"] = json!(40);
    let server = FakeOpenAi::start(json!([
        models_route(128_000, false),
        first,
        text_reply(Some("beta"), "ok", None, Some((100, 10))),
    ]));
    let provider = Arc::new(direct(&server, config(None)));
    provider.refresh_capabilities("m").await.expect("probe");
    let hooks = Arc::new(Recorder::default());
    let mut spec = SessionSpec::new(std::env::temp_dir());
    spec.model = Some("m".to_owned());
    spec.hooks = Some(hooks.clone());
    let session = provider.open(spec).await.expect("open");
    for text in ["alpha", "beta"] {
        let events = collect(session.send_turn(TurnInput::text(text)).await.unwrap()).await;
        assert!(
            matches!(
                terminal(&events),
                AgentEvent::Done {
                    stop_reason: StopReason::Completed,
                    ..
                }
            ),
            "{events:?}"
        );
    }
    let contexts = hooks.contexts.lock().unwrap();
    assert_eq!(
        contexts[1].context_tokens,
        Some(190),
        "100 + 40 read + 50 written"
    );
    assert_eq!(
        contexts[1].tokens_spent, 160,
        "100 + 50 written + 10 output"
    );
}
