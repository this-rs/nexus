//! `wiremock` servers for every outbound HTTP call the crate makes.
//!
//! Three components talk to the network:
//!
//! | component | endpoint | seam used here |
//! |---|---|---|
//! | `core::model_registry::ModelRegistry` | `GET https://api.anthropic.com/v1/models` | `ModelRegistry::with_endpoint` |
//! | `core::storage::meilisearch::MeilisearchClient` | Meilisearch REST API | `MeilisearchConfig::url` |
//! | `core::memory::MediumTermMemory` | project-orchestrator REST API | `McpConfig::url` |
//!
//! `core::storage::neo4j` is **not** in this table: `neo4rs::Graph` speaks the
//! binary Bolt protocol, which wiremock cannot serve. See `support_harness.rs`.

use std::sync::Arc;
use std::time::Duration;

use claude_code_api::core::memory::{McpConfig, MediumTermMemory};
use claude_code_api::core::model_registry::ModelRegistry;
use claude_code_api::core::storage::{MeilisearchClient, MeilisearchConfig};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ===========================================================================
// Anthropic Models API (core::model_registry)
// ===========================================================================

/// `(id, display_name, max_input_tokens)` — the three fields `RemoteModel` reads.
pub type RemoteModelSpec = (&'static str, &'static str, Option<i64>);

/// A mock `GET /v1/models` answering with one page of `models`.
pub async fn anthropic_models(models: &[RemoteModelSpec]) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(models_page(models, false, None)))
        .mount(&server)
        .await;
    server
}

/// A mock `GET /v1/models` that answers `status` with an empty body — the
/// `Models API returned HTTP <status>` branch of `ModelRegistry::fetch_remote`.
pub async fn anthropic_models_failing(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(status))
        .mount(&server)
        .await;
    server
}

/// A mock `GET /v1/models` that answers `200` with a body that is not a
/// `ModelsPage` — the `invalid response body` branch.
pub async fn anthropic_models_malformed() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{\"data\": \"not-a-list\"}"))
        .mount(&server)
        .await;
    server
}

/// The JSON body of one `ModelsPage`.
pub fn models_page(models: &[RemoteModelSpec], has_more: bool, last_id: Option<&str>) -> Value {
    json!({
        "data": models
            .iter()
            .map(|(id, display_name, max_input_tokens)| json!({
                "id": id,
                "display_name": display_name,
                "max_input_tokens": max_input_tokens,
            }))
            .collect::<Vec<_>>(),
        "has_more": has_more,
        "last_id": last_id,
    })
}

/// A registry pointed at `server`, with an API key so it actually fetches.
///
/// Pass `None` as the key to exercise the "no `ANTHROPIC_API_KEY`" path, where
/// `refresh()` returns `Ok(false)` and never touches the network.
pub fn registry_for(server: &MockServer, api_key: Option<&str>) -> Arc<ModelRegistry> {
    Arc::new(ModelRegistry::with_endpoint(
        format!("{}/v1/models", server.uri()),
        api_key.map(str::to_string),
        Duration::from_secs(3600),
    ))
}

/// A registry whose TTL has already expired, so the next `get_models()` refreshes.
pub fn registry_for_expired_ttl(server: &MockServer, api_key: Option<&str>) -> Arc<ModelRegistry> {
    Arc::new(ModelRegistry::with_endpoint(
        format!("{}/v1/models", server.uri()),
        api_key.map(str::to_string),
        Duration::from_secs(0),
    ))
}

// ===========================================================================
// Meilisearch (core::storage::meilisearch, core::memory::long_term)
// ===========================================================================

/// A mock Meilisearch that answers every call `MeilisearchClient` makes.
///
/// `search_messages` / `search_conversations` return `hits`, which must be JSON
/// objects shaped like `MessageDocument` / `ConversationDocument`. Use
/// [`message_hit`] and [`conversation_hit`] to build them.
pub async fn meilisearch(message_hits: Vec<Value>, conversation_hits: Vec<Value>) -> MockServer {
    let server = MockServer::start().await;

    // MeilisearchClient::new -> init_indexes
    Mock::given(method("POST"))
        .and(path("/indexes"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task_info("indexCreation")))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex(r"^/indexes/[^/]+/settings$"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task_info("settingsUpdate")))
        .mount(&server)
        .await;

    // index_message / index_messages / index_conversation
    Mock::given(method("POST"))
        .and(path_regex(r"^/indexes/[^/]+/documents$"))
        .respond_with(
            ResponseTemplate::new(202).set_body_json(task_info("documentAdditionOrUpdate")),
        )
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/indexes/[^/]+/documents/.+$"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task_info("documentDeletion")))
        .mount(&server)
        .await;

    // search_messages / search_conversations
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_results(message_hits)))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_conversations/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_results(conversation_hits)))
        .mount(&server)
        .await;

    // get_stats
    Mock::given(method("GET"))
        .and(path_regex(r"^/indexes/[^/]+/stats$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(index_stats(0)))
        .mount(&server)
        .await;

    server
}

/// A Meilisearch that rejects everything with `status` — for the error paths of
/// `MeilisearchClient` (including `new`, which fails on `set_settings`).
pub async fn meilisearch_failing(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path_regex(r".*"))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({
            "message": "injected meilisearch failure",
            "code": "internal",
            "type": "internal",
            "link": "https://example.invalid",
        })))
        .mount(&server)
        .await;
    server
}

/// Connect a real [`MeilisearchClient`] to `server`.
pub async fn meilisearch_client(server: &MockServer) -> anyhow::Result<MeilisearchClient> {
    MeilisearchClient::new(MeilisearchConfig {
        url: server.uri(),
        api_key: Some("test-key".to_string()),
    })
    .await
}

pub fn message_hit(id: &str, conversation_id: &str, role: &str, content: &str) -> Value {
    json!({
        "id": id,
        "conversation_id": conversation_id,
        "role": role,
        "content": content,
        "turn_index": 0,
        "created_at": 0,
    })
}

pub fn conversation_hit(id: &str, model: &str, preview: &str) -> Value {
    json!({
        "id": id,
        "model": model,
        "message_count": 1,
        "total_tokens": 0,
        "created_at": 0,
        "updated_at": 0,
        "content_preview": preview,
    })
}

fn task_info(kind: &str) -> Value {
    json!({
        "taskUid": 1,
        "indexUid": "nexus_messages",
        "status": "enqueued",
        "type": kind,
        "details": null,
        "enqueuedAt": "2026-01-01T00:00:00Z",
    })
}

fn search_results(hits: Vec<Value>) -> Value {
    let total = hits.len();
    json!({
        "hits": hits,
        "offset": 0,
        "limit": 20,
        "estimatedTotalHits": total,
        "processingTimeMs": 1,
        "query": "",
    })
}

fn index_stats(documents: usize) -> Value {
    json!({
        "numberOfDocuments": documents,
        "numberOfEmbeddedDocuments": 0,
        "numberOfEmbeddings": 0,
        "rawDocumentDbSize": 0,
        "avgDocumentSize": 0,
        "isIndexing": false,
        "fieldDistribution": {},
    })
}

// ===========================================================================
// project-orchestrator (core::memory::medium_term)
// ===========================================================================

/// A mock project-orchestrator exposing the four endpoints `MediumTermMemory`
/// queries.
///
/// Note the shapes the crate actually expects, which are easy to get wrong:
/// the routes are `GET /plans`, `GET /tasks`, `GET /decisions/search` and
/// `GET /notes/search`, and each body must be an **object** wrapping the list
/// under `plans` / `tasks` / `decisions` / `notes`. Anything else is silently
/// read as "no results".
pub async fn project_orchestrator(
    plans: Vec<Value>,
    tasks: Vec<Value>,
    decisions: Vec<Value>,
    notes: Vec<Value>,
) -> MockServer {
    let server = MockServer::start().await;
    for (route, key, body) in [
        ("/plans", "plans", plans),
        ("/tasks", "tasks", tasks),
        ("/decisions/search", "decisions", decisions),
        ("/notes/search", "notes", notes),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({key: body})))
            .mount(&server)
            .await;
    }
    server
}

/// A project-orchestrator that fails every request with `status`.
pub async fn project_orchestrator_failing(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path_regex(r".*"))
        .respond_with(ResponseTemplate::new(status))
        .mount(&server)
        .await;
    server
}

/// A `MediumTermMemory` pointed at `server`.
pub fn medium_term_for(server: &MockServer) -> MediumTermMemory {
    MediumTermMemory::new(McpConfig {
        url: server.uri(),
        api_key: None,
    })
}
