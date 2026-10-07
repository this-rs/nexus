//! The native harness (`providers::native`) against the conformance suite of the
//! agent contract: a real `NativeProvider` on a real `OpenAiEndpoint`, talking to
//! the `fake_openai` server (one script per scenario) and to `fake_mcp` over stdio.
//! No network beyond 127.0.0.1.
//!
//! What the native harness declares absent is proven through its written
//! fallback (§5): `images`, `native_question`, `background_tasks`, `subagents`,
//! `hooks` and `sandbox`.
//!
//! `permission_hors_tour` needs a permission request with no turn running. The
//! native harness raises one only from a tool call, and a turn whose stream was
//! dropped sends the rest of itself out of band (§9): [`DetachedTurn`] stages
//! exactly that, by starting a turn at `open` and dropping its stream.

#[path = "support/fake_openai.rs"]
mod fake_openai;
#[path = "support/native.rs"]
mod native;

use std::sync::Arc;

use async_trait::async_trait;
use fake_openai::FakeOpenAi;
use native::*;
use nexus_claude::agent::{
    AgentProvider, AgentSession, Capabilities, CostBasis, EnvCredentialResolver, ModelInfo,
    ModelPrice, ProviderError, ProviderHealth, ProviderInstanceConfig, ProviderKind,
    ProviderRegistry, ResumeToken, SecurityGate, SessionSpec, TurnInput,
};
use nexus_claude::model::{ChatMessage, EndpointQuirks, PriceTable};
use nexus_claude::providers::native::{
    MemoryTranscriptStore, NativeConfig, NativeProvider, TranscriptStore,
};
use nexus_claude::testkit::conformance::{
    ConformanceTarget, Prepared, Scenario, ScenarioOutcome, run_all,
};
use serde_json::{Value, json};

const TOOL_MARKER: &str = "\"role\":\"tool\"";

fn config(scenario: Option<Scenario>) -> NativeConfig {
    let mut config = NativeConfig::new("native-test");
    config.default_model = Some("m".to_owned());
    config.prices = PriceTable::new().with(
        "m",
        ModelPrice {
            input_per_mtok: 1.0,
            output_per_mtok: 2.0,
            cache_read_per_mtok: None,
            cache_write_per_mtok: None,
        },
    );
    if scenario == Some(Scenario::Compaction) {
        // The window is configured, and the staged usage nears it.
        config.context_window = Some(12_000);
        config.compaction.keep_recent = 2;
    }
    config
}

fn echo(id: &'static str) -> (&'static str, &'static str, Value) {
    (id, "mcp__fake__echo", json!({"text": id}))
}

/// The `fake_openai` routes of a scenario. Routes are consumed in order; the
/// last one answers any request left over with plain text.
fn script(scenario: Scenario) -> Vec<Value> {
    let p = scenario.prompt();
    let p = Some(p.as_str());
    let tool = Some(TOOL_MARKER);
    let mut routes = vec![probe_route(true), models_route(128_000)];
    let tool_call = |name: &'static str| ("c1", name, json!({"text": "x"}));
    match scenario {
        Scenario::AppelOutilResultat => {
            routes.push(tool_reply(p, &[echo("c1")], None, Some((120, 12))));
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
            // The second answer reports a prompt near the 12 000-token window.
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
        Scenario::ErreurRetryable => {
            routes.push(status_reply(
                p,
                503,
                json!({"error": {"message": "overloaded"}}),
            ));
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

/// Everything a scenario keeps alive: the fake server and a working directory.
struct Staging {
    _server: FakeOpenAi,
    _cwd: tempfile::TempDir,
}

/// How the provider of a scenario is obtained.
#[derive(Clone, Copy, PartialEq)]
enum Route {
    /// `NativeProvider::new` on an `OpenAiEndpoint`, by hand.
    Direct,
    /// `ProviderRegistry::upsert` of an instance configuration, then `get`.
    Registry,
}

struct NativeTarget {
    route: Route,
    base: Arc<dyn AgentProvider>,
    _base_server: FakeOpenAi,
}

/// The instance the registry route builds: the same endpoint, dialect, price and
/// window as [`config`], described as configuration.
fn registry_instance(scenario: Option<Scenario>, url: String) -> ProviderInstanceConfig {
    let mut instance = ProviderInstanceConfig::native("native-test", url)
        .with_preset("deepseek")
        .with_default_model("m")
        .with_price(
            "m",
            ModelPrice {
                input_per_mtok: 1.0,
                output_per_mtok: 2.0,
                cache_read_per_mtok: None,
                cache_write_per_mtok: None,
            },
        );
    if scenario == Some(Scenario::Compaction) {
        instance = instance
            .with_context_window(12_000)
            .with_extension("compaction_keep_recent", json!(2));
    }
    instance
}

/// A provider obtained from a registry (gate open), probed for model `m`.
async fn from_registry(
    scenario: Option<Scenario>,
    url: String,
    store: Option<Arc<MemoryTranscriptStore>>,
) -> Option<Arc<dyn AgentProvider>> {
    let registry = ProviderRegistry::new(Arc::new(EnvCredentialResolver));
    registry.activate_security_gate(SecurityGate::attest("native-conformance"));
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

impl NativeTarget {
    async fn new(route: Route) -> Self {
        let server = FakeOpenAi::start(json!([probe_route(true), models_route(128_000)]));
        let provider: Arc<dyn AgentProvider> = match route {
            Route::Direct => {
                let provider = Arc::new(NativeProvider::new(
                    config(None),
                    endpoint(server.base_url(), EndpointQuirks::deepseek()),
                ));
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
            base: provider,
            _base_server: server,
        }
    }
}

/// Starts a turn when a session opens and drops its stream: the turn goes on and
/// everything it emits goes out of band (§9).
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
impl ConformanceTarget for NativeTarget {
    fn name(&self) -> &str {
        match self.route {
            Route::Direct => "native (OpenAiEndpoint on fake_openai, MCP over stdio on fake_mcp)",
            Route::Registry => {
                "native from ProviderRegistry (instance config, fake_openai, fake_mcp)"
            },
        }
    }

    fn provider(&self) -> Arc<dyn AgentProvider> {
        self.base.clone()
    }

    async fn prepare(&self, scenario: Scenario) -> Option<Prepared> {
        let server = FakeOpenAi::start(json!(script(scenario)));
        let store = Arc::new(MemoryTranscriptStore::new());
        let provider: Arc<dyn AgentProvider> = match self.route {
            Route::Direct => {
                let provider = Arc::new(
                    NativeProvider::new(
                        config(Some(scenario)),
                        endpoint(server.base_url(), EndpointQuirks::deepseek()),
                    )
                    .with_transcript_store(store.clone()),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_native_harness_passes_the_conformance_suite() {
    check_conformance(Route::Direct).await;
}

/// The same suite, with the provider of every scenario obtained from a
/// `ProviderRegistry` that was given an instance configuration.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_native_instance_built_by_the_registry_passes_the_conformance_suite() {
    check_conformance(Route::Registry).await;
}

async fn check_conformance(route: Route) {
    let target = NativeTarget::new(route).await;
    let report = run_all(&target).await;
    eprintln!("{}", report.summary());
    report.assert_conformant();

    // What the native harness does not have is proven through its fallback.
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
    // What it has is played, not skipped.
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
    assert!(caps.tool_cancel && caps.resume && caps.interactive_permissions);
    assert_eq!(caps.cost, CostBasis::Priced);
}
