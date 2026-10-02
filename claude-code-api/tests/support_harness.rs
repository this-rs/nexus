//! Proof that the harness in `tests/support/` works, and the reference examples
//! downstream tests should copy.
//!
//! One test per facility. Every assertion is about real gateway behaviour, not
//! about the harness itself.

mod support;

use axum::http::StatusCode;
use claude_code_api::api::streaming_handler::handle_enhanced_streaming_response;
use claude_code_api::core::conversation::{ConversationConfig, ConversationManager};
use claude_code_api::core::memory::{ContextualMemoryProvider, UnifiedMemoryProvider};
use claude_code_api::core::storage::{CacheStore, ConversationStore, SessionStore};
use claude_code_api::models::openai::ChatCompletionResponse;
use serde_json::json;
use std::sync::Arc;
use support::fakes::{
    CacheOp, ConversationOp, FakeCacheStore, FakeConversationStore, FakeMemoryProvider,
    FakeSessionStore, SessionOp,
};
use support::{
    FakeClaudeCli, TestSettings, claude_output, http_mocks, openai, sse, test_app,
    test_app_with_cli, test_app_with_components, test_components,
};

// ===========================================================================
// test_app(): the production router
// ===========================================================================

/// `test_app` serves the routes declared in `claude_code_api::build_router`,
/// not a copy of them.
#[tokio::test]
async fn test_app_serves_the_production_route_table() {
    let server = test_app().await;

    server.get("/health").await.assert_text("OK");

    // Declared by build_router and therefore reachable.
    assert_eq!(server.get("/v1/models").await.status_code(), StatusCode::OK);
    assert_eq!(server.get("/stats").await.status_code(), StatusCode::OK);
    assert_eq!(
        server.get("/v1/conversations").await.status_code(),
        StatusCode::OK
    );

    // Not declared anywhere: `api::sessions` and `api::projects` exist as
    // handlers but build_router never mounts them.
    assert_eq!(
        server.get("/v1/sessions").await.status_code(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        server.get("/v1/projects").await.status_code(),
        StatusCode::NOT_FOUND
    );
}

/// The middleware stack travels with the router: `request_id::add_request_id`
/// echoes a caller-supplied id, and mints one otherwise.
#[tokio::test]
async fn request_id_middleware_is_part_of_the_router() {
    let server = test_app().await;

    let echoed = server
        .get("/health")
        .add_header("x-request-id", "nexus-harness-1")
        .await;
    assert_eq!(
        echoed.header("x-request-id").to_str().unwrap(),
        "nexus-harness-1"
    );

    let minted = server.get("/health").await;
    let generated = minted.header("x-request-id").to_str().unwrap().to_string();
    assert!(
        uuid::Uuid::parse_str(&generated).is_ok(),
        "a missing x-request-id must be replaced by a UUID, got {generated:?}"
    );
}

// ===========================================================================
// openai helpers + error shapes
// ===========================================================================

/// `openai::request()` builds the OpenAI payload; the gateway rejects an empty
/// `messages` array with the OpenAI error envelope.
#[tokio::test]
async fn empty_messages_is_an_invalid_request_error() {
    let server = test_app().await;

    let response = server
        .post("/v1/chat/completions")
        .json(&openai::request().build())
        .await;

    response.assert_status(StatusCode::BAD_REQUEST);
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(
        body["error"]["message"],
        "Bad request: Messages cannot be empty"
    );
}

/// With the default `TestSettings`, `claude.command` cannot be spawned, so the
/// handler returns the `claude_process_error` it advertises.
#[tokio::test]
async fn a_missing_cli_is_reported_as_claude_process_error() {
    let server = test_app().await;

    let response = server
        .post("/v1/chat/completions")
        .json(&openai::chat_request("Bonjour"))
        .await;

    response.assert_status(StatusCode::INTERNAL_SERVER_ERROR);
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"]["type"], "claude_process_error");
}

// ===========================================================================
// FakeClaudeCli: the happy path end to end
// ===========================================================================

/// The reference example: a scripted CLI drives a complete, non-streaming
/// chat completion through `ProcessPool` -> `ClaudeManager` -> the real
/// subprocess -> `handle_non_streaming_response`.
#[tokio::test]
async fn fake_cli_drives_a_full_chat_completion() {
    let cli = FakeClaudeCli::replying("Bonjour Nexus");
    let server = test_app_with_cli(&cli).await;

    let response = server
        .post("/v1/chat/completions")
        .json(&openai::chat_request("Dis bonjour"))
        .await;

    response.assert_status_ok();
    let body: ChatCompletionResponse = response.json();
    assert_eq!(openai::first_text(&body).as_deref(), Some("Bonjour Nexus"));
    assert_eq!(
        openai::first_choice(&body)
            .unwrap()
            .finish_reason
            .as_deref(),
        Some("stop")
    );
    assert_eq!(body.object, "chat.completion");
    assert!(
        body.conversation_id.is_some(),
        "the gateway must return the conversation it created"
    );
}

/// A CLI `tool_use` content block becomes an OpenAI `tool_calls` entry, and the
/// finish reason flips to `tool_calls`.
#[tokio::test]
async fn fake_cli_tool_use_becomes_an_openai_tool_call() {
    let cli = FakeClaudeCli::calling_tool("toolu_1", "get_weather", json!({"city": "Lyon"}));
    let server = test_app_with_cli(&cli).await;

    let response = server
        .post("/v1/chat/completions")
        .json(
            &openai::request()
                .message(openai::user("Quel temps à Lyon ?"))
                .tools(vec![openai::tool(
                    "get_weather",
                    "Current weather",
                    json!({"type": "object", "properties": {"city": {"type": "string"}}}),
                )])
                .build(),
        )
        .await;

    response.assert_status_ok();
    let body: ChatCompletionResponse = response.json();
    let choice = openai::first_choice(&body).expect("one choice");
    assert_eq!(choice.finish_reason.as_deref(), Some("tool_calls"));

    let calls = choice.message.tool_calls.as_ref().expect("tool_calls");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "toolu_1");
    assert_eq!(calls[0].tool_type, "function");
    assert_eq!(calls[0].function.name, "get_weather");
    assert_eq!(calls[0].function.arguments, r#"{"city":"Lyon"}"#);
}

/// A CLI that prints nothing still produces a well-formed `200`.
#[tokio::test]
async fn a_silent_cli_yields_an_empty_completion() {
    let cli = FakeClaudeCli::silent();
    let server = test_app_with_cli(&cli).await;

    let response = server
        .post("/v1/chat/completions")
        .json(&openai::chat_request("..."))
        .await;

    response.assert_status_ok();
    let body: ChatCompletionResponse = response.json();
    assert_eq!(openai::first_text(&body).as_deref(), Some(""));
    assert_eq!(body.usage.total_tokens, 0);
}

/// Sidechain messages (subagent `Task` output) never reach the client.
#[tokio::test]
async fn sidechain_output_is_excluded_from_the_completion() {
    let cli = FakeClaudeCli::new(&[
        claude_output::sidechain_text("toolu_sub", "SECRET SUBAGENT TEXT"),
        claude_output::assistant_text("réponse visible"),
        claude_output::result_success(),
    ]);
    let server = test_app_with_cli(&cli).await;

    let response = server
        .post("/v1/chat/completions")
        .json(&openai::chat_request("hello"))
        .await;

    response.assert_status_ok();
    let body: ChatCompletionResponse = response.json();
    assert_eq!(
        openai::first_text(&body).as_deref(),
        Some("réponse visible")
    );
}

/// The response cache in front of the handler: the second identical request is
/// served from `ResponseCache` without touching the CLI.
#[tokio::test]
async fn identical_requests_hit_the_response_cache() {
    let cli = FakeClaudeCli::replying("mis en cache");
    let server = test_app_with_cli(&cli).await;

    let first: ChatCompletionResponse = server
        .post("/v1/chat/completions")
        .json(&openai::chat_request("même question"))
        .await
        .json();
    let second: ChatCompletionResponse = server
        .post("/v1/chat/completions")
        .json(&openai::chat_request("même question"))
        .await
        .json();

    assert_eq!(
        first.id, second.id,
        "a cache hit must replay the stored response verbatim"
    );

    let stats: serde_json::Value = server.get("/stats").await.json();
    assert_eq!(stats["cache"]["total_hits"], 1);
    assert_eq!(stats["cache"]["enabled"], true);
}

// ===========================================================================
// SSE
// ===========================================================================

/// The HTTP-level streaming path: `create_sse_stream` frames every chunk as a
/// `data:` line, and `sse::chunks` parses them back.
#[tokio::test]
async fn streaming_completion_emits_sse_chunks() {
    let cli = FakeClaudeCli::replying("réponse diffusée");
    let server = test_app_with_cli(&cli).await;

    let response = server
        .post("/v1/chat/completions")
        .json(&openai::streaming_request("stream please"))
        .await;

    response.assert_status_ok();
    assert!(
        response
            .header("content-type")
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );

    let chunks = sse::chunks(&response.text());
    assert_eq!(
        chunks[0].choices[0].delta.role.as_deref(),
        Some("assistant"),
        "the first chunk announces the role"
    );
    assert_eq!(sse::text(&chunks), "réponse diffusée");
    assert_eq!(sse::finish_reasons(&chunks), vec!["stop".to_string()]);
}

/// The stream assembler can be driven without any subprocess: feed it a channel.
/// This is the cheapest way to cover `streaming_handler`.
#[tokio::test]
async fn streaming_handler_can_be_driven_from_a_channel() {
    let rx = claude_output::channel(&[
        claude_output::sidechain_text("toolu_sub", "ignoré"),
        claude_output::assistant_text("salut"),
        claude_output::assistant_tool_use("toolu_2", "search", json!({"q": "nexus"})),
        claude_output::unknown("system"),
        claude_output::result_success(),
    ]);

    let stream = handle_enhanced_streaming_response("claude-sonnet-5".to_string(), rx, None, None);
    let chunks = sse::collect(stream.await).await;

    assert_eq!(sse::text(&chunks), "salut");
    assert_eq!(
        sse::tool_calls(&chunks),
        vec![("search".to_string(), r#"{"q":"nexus"}"#.to_string())]
    );
    assert_eq!(sse::finish_reasons(&chunks), vec!["stop".to_string()]);
}

// ===========================================================================
// Conversations
// ===========================================================================

/// `POST`/`GET /v1/conversations` round-trip through the in-memory store that
/// `build_components` wires up.
#[tokio::test]
async fn conversation_routes_round_trip() {
    let server = test_app().await;

    let created: serde_json::Value = server
        .post("/v1/conversations")
        .json(&json!({"model": "claude-sonnet-5", "project_path": "/tmp/nexus"}))
        .await
        .json();
    let id = created["id"].as_str().expect("id").to_string();
    assert_eq!(created["message_count"], 0);
    assert_eq!(created["metadata"]["project_path"], "/tmp/nexus");

    let fetched: serde_json::Value = server.get(&format!("/v1/conversations/{id}")).await.json();
    assert_eq!(fetched["id"], id);

    let listed: serde_json::Value = server.get("/v1/conversations").await.json();
    let ids: Vec<&str> = listed["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&id.as_str()));

    server
        .get("/v1/conversations/unknown-id")
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

/// `POST /v1/sessions/:id/interrupt` answers `404` when no interactive session
/// exists for that conversation.
#[tokio::test]
async fn interrupting_an_unknown_session_is_a_404() {
    let server = test_app().await;

    let response = server.post("/v1/sessions/nope/interrupt").await;
    response.assert_status(StatusCode::NOT_FOUND);
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"], "session not found");
    assert_eq!(body["conversation_id"], "nope");
}

// ===========================================================================
// TestSettings
// ===========================================================================

/// The configuration builder produces a `Settings` without reading
/// `config/*.toml` or the environment, so concurrent tests cannot collide.
#[tokio::test]
async fn test_settings_are_built_in_memory() {
    let settings = TestSettings::new()
        .command("/opt/fake/claude")
        .interactive_sessions(true)
        .timeout_seconds(7)
        .process_pool(3, 0, 1)
        .build();

    assert_eq!(settings.claude.command, "/opt/fake/claude");
    assert!(settings.claude.use_interactive_sessions);
    assert_eq!(settings.claude.timeout_seconds, 7);
    // min_idle must stay 0: ProcessPool::new pre-warms min_idle CLI processes.
    assert_eq!(settings.process_pool.min_idle, 0);

    // And the settings actually reach the handler: ChatState stores them.
    let components = test_components(settings).await;
    assert_eq!(components.chat_state.settings.claude.timeout_seconds, 7);
    assert!(components.chat_state.use_interactive_sessions);
}

/// `EnvGuard` restores the environment, for the tests that must exercise
/// `Settings::new()` itself. Those tests have to be `#[serial]`.
#[test]
#[serial_test::serial]
fn env_guard_restores_the_previous_environment() {
    const KEY: &str = "NEXUS_HARNESS_PROBE";
    // SAFETY: the test is #[serial].
    unsafe { std::env::set_var(KEY, "before") };

    {
        let _guard = support::config::EnvGuard::new().set(KEY, "during");
        assert_eq!(std::env::var(KEY).unwrap(), "during");
    }

    assert_eq!(std::env::var(KEY).unwrap(), "before");
    // SAFETY: the test is #[serial].
    unsafe { std::env::remove_var(KEY) };
}

/// `Settings::new()` falls back to its built-in defaults when no config file and
/// no `CLAUDE_CODE__*` variables are present, and `CLAUDE_CODE__*` overrides win.
#[test]
#[serial_test::serial]
fn settings_new_reads_the_claude_code_prefix() {
    use claude_code_api::core::config::Settings;

    let _guard = support::config::EnvGuard::new()
        .set("RUN_MODE", "nexus-harness-absent")
        .set("CLAUDE_CODE__SERVER__PORT", "9731")
        .set("CLAUDE_CODE__CLAUDE__COMMAND", "claude-from-env");

    let settings = Settings::new().expect("defaults plus env must deserialize");
    assert_eq!(settings.server.port, 9731);
    assert_eq!(settings.claude.command, "claude-from-env");
    // Untouched defaults.
    assert_eq!(settings.claude.timeout_seconds, 300);
    assert!(!settings.auth.enabled);
}

// ===========================================================================
// Fakes: forcing the error paths
// ===========================================================================

/// `ConversationManager` is generic over its store, so a fake with an armed
/// fault reaches the error branches of every method.
#[tokio::test]
async fn fake_conversation_store_forces_the_manager_error_paths() {
    let store = FakeConversationStore::new();
    store.fail(ConversationOp::Create, "disk on fire");
    let manager = ConversationManager::new(store, ConversationConfig::default());

    let error = manager
        .create_conversation(None)
        .await
        .expect_err("an armed Create fault must surface");
    assert_eq!(error.to_string(), "disk on fire");

    // update_metadata on an absent conversation is an error, not a no-op.
    let error = manager
        .update_metadata("absent", |m| m.total_tokens = 1)
        .await
        .expect_err("unknown conversation");
    assert_eq!(error.to_string(), "Conversation not found");

    // list_active swallows store errors by design (`unwrap_or_default`).
    let store = Arc::new(FakeConversationStore::new());
    store.fail(ConversationOp::ListActive, "neo4j down");
    assert!(store.list_active().await.is_err());
    let manager =
        ConversationManager::new(FakeConversationStore::new(), ConversationConfig::default());
    assert!(manager.list_active_conversations().await.is_empty());
}

/// The fake session store records calls and fails the armed one.
#[tokio::test]
async fn fake_session_store_can_fail_any_operation() {
    let store = FakeSessionStore::new();
    let id = store.create(Some("/w".to_string())).await.unwrap();
    assert!(store.get(&id).await.unwrap().is_some());

    store.fail(SessionOp::List, "bolt handshake failed");
    assert_eq!(
        store.list().await.unwrap_err().to_string(),
        "bolt handshake failed"
    );

    store.heal(SessionOp::List);
    assert_eq!(store.list().await.unwrap().len(), 1);
    assert_eq!(store.calls(), vec!["create", "get", "list", "list"]);
}

/// The fake cache can be made to lose writes or read cold, which is how a
/// caller-side cache-miss branch is covered.
#[tokio::test]
async fn fake_cache_store_can_lose_writes_and_fail_cleanup() {
    let cache = FakeCacheStore::new();
    cache
        .put("k".to_string(), openai::response("r1", "claude-sonnet-5"))
        .await;
    assert_eq!(cache.get("k").await.unwrap().id, "r1");

    cache.fail(CacheOp::Get, "cold");
    assert!(cache.get("k").await.is_none());
    cache.heal(CacheOp::Get);

    cache.fail(CacheOp::Put, "dropped");
    cache
        .put("k2".to_string(), openai::response("r2", "claude-sonnet-5"))
        .await;
    assert!(cache.get("k2").await.is_none());

    cache.fail(CacheOp::Cleanup, "meilisearch gone");
    assert_eq!(
        cache.cleanup().await.unwrap_err().to_string(),
        "meilisearch gone"
    );
}

/// `UnifiedMemoryProvider` takes three boxed providers; the fake stands in for
/// any tier, including the two that would otherwise need live services.
#[tokio::test]
async fn fake_memory_providers_drive_the_unified_provider() {
    let unified = UnifiedMemoryProvider::new(
        Box::new(FakeMemoryProvider::with_texts(&[(
            "s1",
            "court terme",
            0.9,
        )])),
        Box::new(FakeMemoryProvider::with_texts(&[(
            "m1",
            "moyen terme",
            0.5,
        )])),
        Box::new(FakeMemoryProvider::with_texts(&[("l1", "long terme", 0.4)])),
    );

    let results = unified.query("auth", 10).await.expect("all tiers answered");
    let contents: Vec<&str> = results.iter().map(|r| r.content.as_str()).collect();
    assert_eq!(contents, vec!["court terme", "moyen terme", "long terme"]);

    // A dead tier is skipped, not fatal: `query` logs the failure and answers
    // with what the reachable tiers know. (It used to `?` on each level, so an
    // unreachable project-orchestrator also cost the caller its short-term
    // memory — see `core::memory::unified`.)
    let broken = UnifiedMemoryProvider::new(
        Box::new(FakeMemoryProvider::with_texts(&[(
            "s1",
            "court terme",
            0.9,
        )])),
        Box::new(FakeMemoryProvider::failing("orchestrator unreachable")),
        Box::new(FakeMemoryProvider::with_texts(&[("l1", "long terme", 0.4)])),
    );
    let degraded = broken
        .query("auth", 10)
        .await
        .expect("a dead tier is skipped, not fatal");
    let degraded_contents: Vec<&str> = degraded.iter().map(|r| r.content.as_str()).collect();
    assert_eq!(degraded_contents, vec!["court terme", "long terme"]);

    // Only a total outage is an error.
    let dead = UnifiedMemoryProvider::new(
        Box::new(FakeMemoryProvider::failing("store unreachable")),
        Box::new(FakeMemoryProvider::failing("orchestrator unreachable")),
        Box::new(FakeMemoryProvider::failing("meilisearch unreachable")),
    );
    assert!(
        dead.query("auth", 10)
            .await
            .unwrap_err()
            .to_string()
            .contains("every memory level failed")
    );
}

// ===========================================================================
// wiremock
// ===========================================================================

/// Without an API key the registry never calls the network and serves the static
/// catalog from `ClaudeModel::all()`.
#[tokio::test]
async fn models_route_serves_the_static_catalog_without_a_key() {
    let upstream = http_mocks::anthropic_models(&[]).await;
    let mut components = test_components(TestSettings::new().build()).await;
    components.model_registry = http_mocks::registry_for(&upstream, None);
    let server = test_app_with_components(components);

    let body: serde_json::Value = server.get("/v1/models").await.json();
    assert_eq!(body["object"], "list");
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"claude-opus-5"));
    assert_eq!(upstream.received_requests().await.unwrap().len(), 0);
}

/// With a key, `POST /v1/models/refresh` fetches the mocked Anthropic Models API
/// and the catalog becomes dynamic.
#[tokio::test]
async fn model_refresh_reads_the_mocked_anthropic_api() {
    let upstream = http_mocks::anthropic_models(&[
        ("claude-harness-1", "Claude Harness 1", Some(321_000)),
        ("claude-harness-2", "Claude Harness 2", None),
    ])
    .await;

    let mut components = test_components(TestSettings::new().build()).await;
    components.model_registry = http_mocks::registry_for(&upstream, Some("sk-test"));
    let server = test_app_with_components(components);

    let body: serde_json::Value = server.post("/v1/models/refresh").await.json();
    assert_eq!(body["refreshed"], true);
    assert_eq!(body["dynamic"], true);
    assert_eq!(body["detail"], "refreshed from Anthropic Models API");

    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["claude-harness-1", "claude-harness-2"]);
    assert!(!upstream.received_requests().await.unwrap().is_empty());
}

/// When the Models API fails, the registry keeps the static catalog and reports
/// the failure instead of returning an error to the client.
#[tokio::test]
async fn model_refresh_keeps_the_catalog_when_upstream_fails() {
    let upstream = http_mocks::anthropic_models_failing(503).await;
    let mut components = test_components(TestSettings::new().build()).await;
    components.model_registry = http_mocks::registry_for(&upstream, Some("sk-test"));
    let server = test_app_with_components(components);

    let response = server.post("/v1/models/refresh").await;
    response.assert_status_ok();
    let body: serde_json::Value = response.json();
    assert_eq!(body["refreshed"], false);
    assert_eq!(body["dynamic"], false);
    assert_eq!(
        body["detail"],
        "Models API returned HTTP 503 Service Unavailable"
    );
    assert!(!body["data"].as_array().unwrap().is_empty());
}

/// Meilisearch is reachable through `MeilisearchConfig::url`, so wiremock can
/// stand in for the whole service — including the bootstrap in
/// `MeilisearchClient::new`.
#[tokio::test]
async fn meilisearch_client_runs_against_a_mock_server() {
    let upstream = http_mocks::meilisearch(
        vec![http_mocks::message_hit(
            "m1",
            "conv-1",
            "user",
            "comment configurer l'auth",
        )],
        vec![http_mocks::conversation_hit(
            "conv-1",
            "claude-sonnet-5",
            "auth",
        )],
    )
    .await;

    let client = http_mocks::meilisearch_client(&upstream)
        .await
        .expect("bootstrap against the mock must succeed");

    let hits = client.search_messages("auth", None, 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "m1");
    assert_eq!(hits[0].content, "comment configurer l'auth");

    let scoped = client
        .search_messages("auth", Some("conv-1"), 10)
        .await
        .unwrap();
    assert_eq!(scoped.len(), 1);

    let conversations = client.search_conversations("auth", 10).await.unwrap();
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].id, "conv-1");

    let stats = client.get_stats().await.unwrap();
    assert!(!stats.is_indexing);
}

/// A Meilisearch that rejects the settings calls makes `MeilisearchClient::new`
/// fail — the error path of the constructor, with no service running.
#[tokio::test]
async fn meilisearch_client_surfaces_bootstrap_failures() {
    let upstream = http_mocks::meilisearch_failing(500).await;
    // `MeilisearchClient` is not `Debug`, so unwrap the Result by hand.
    let error = match http_mocks::meilisearch_client(&upstream).await {
        Ok(_) => panic!("set_settings against a failing server must not succeed"),
        Err(e) => e,
    };
    assert!(
        error.to_string().contains("injected meilisearch failure")
            || error.to_string().contains("500"),
        "unexpected error: {error}"
    );
}

/// `MediumTermMemory` talks to project-orchestrator over plain HTTP, so wiremock
/// covers it too.
#[tokio::test]
async fn medium_term_memory_queries_the_mocked_orchestrator() {
    let upstream = http_mocks::project_orchestrator(
        vec![json!({
            "id": "plan-1",
            "title": "Harnais de test",
            "description": "couvrir claude-code-api",
            "status": "active",
            "created_at": "2026-01-01T00:00:00Z",
        })],
        vec![],
        vec![],
        vec![],
    )
    .await;

    let memory = http_mocks::medium_term_for(&upstream);
    let results = memory.query("harnais", 10).await.expect("mocked HTTP");

    assert!(
        results
            .iter()
            .any(|r| r.content.contains("claude-code-api")),
        "the plan from the mock must be surfaced: {results:#?}"
    );
    assert!(!upstream.received_requests().await.unwrap().is_empty());
}

// ===========================================================================
// Bugs found while building the harness — evidence, not fixes
// ===========================================================================

/// BUG (api::chat::chat_completions): an unknown `conversation_id` is reported
/// as `500 internal_error` instead of `404 not_found_error`.
///
/// `chat_completions` uses the client-supplied `conversation_id` verbatim, never
/// checks that it exists, and only discovers the problem when
/// `conversation_manager.add_message` fails — after the CLI has already run and
/// billed a turn. The `.map_err(|e| ApiError::Internal(...))` on that call turns
/// "you asked for a conversation that does not exist" into a server error.
///
/// Expected: `404` with `"type": "not_found_error"` (and ideally the check before
/// spawning the CLI). Actual: `500` with `"type": "internal_error"` and the
/// message `Internal server error: Conversation not found: nope`.
#[tokio::test]
#[ignore = "documents a bug: unknown conversation_id answers 500 instead of 404"]
async fn unknown_conversation_id_should_be_a_404() {
    let cli = FakeClaudeCli::replying("peu importe");
    let server = test_app_with_cli(&cli).await;

    let response = server
        .post("/v1/chat/completions")
        .json(
            &openai::request()
                .message(openai::user("salut"))
                .conversation_id("nope")
                .build(),
        )
        .await;

    response.assert_status(StatusCode::NOT_FOUND);
}

/// BUG (utils::streaming::create_sse_stream): the SSE stream never terminates
/// with `data: [DONE]`.
///
/// Every OpenAI client (openai-python, openai-node, LangChain) treats
/// `data: [DONE]` as the end-of-stream sentinel. `create_sse_stream` maps each
/// chunk to a `data:` line and ends the HTTP body with nothing else, so clients
/// either hang until the socket closes or report a truncated stream.
///
/// Expected: a final frame whose payload is exactly `[DONE]`.
/// Actual: the last frame is the `finish_reason: "stop"` chunk.
#[tokio::test]
#[ignore = "documents a bug: the SSE stream has no [DONE] sentinel"]
async fn streaming_response_should_end_with_done() {
    let cli = FakeClaudeCli::replying("court");
    let server = test_app_with_cli(&cli).await;

    let body = server
        .post("/v1/chat/completions")
        .json(&openai::streaming_request("stream"))
        .await
        .text();

    assert_eq!(
        sse::frames(&body).last().map(String::as_str),
        Some("[DONE]"),
        "OpenAI clients need the [DONE] sentinel"
    );
}

// ---------------------------------------------------------------------------
// Findings that the harness cannot turn into a test
// ---------------------------------------------------------------------------
//
// * `api::chat::handle_non_streaming_response` only honours
//   `claude.timeout_seconds` in 5-second steps: the receive loop is
//   `timeout(Duration::from_secs(5), rx.recv())` and the configured budget is
//   compared only *after* a 5-second slice has elapsed. A gateway configured with
//   `timeout_seconds = 1` still holds the connection for ~5 seconds. Reproducing
//   it needs a CLI that keeps stdout open, i.e. a long-lived subprocess, which
//   this harness deliberately never creates.
//
// * `core::claude_manager::ClaudeManager::create_session_with_message` and
//   `core::interactive_session::InteractiveSessionManager::create_session` both
//   log the whole `Command` with `info!("... with command: {:?}", cmd)`. The
//   `Debug` of `tokio::process::Command` includes every argument, so an
//   `--mcp-config` passed inline as JSON (`mcp.config_json`) is written to the
//   log verbatim — credentials included.
//
// * The unit tests in `core::interactive_session` spawn `true` and `sleep 60`.
//   Neither exists on `windows-latest`, so those tests cannot pass on the Windows
//   leg of CI, and `sleep 60` leaves a process running for a minute.
