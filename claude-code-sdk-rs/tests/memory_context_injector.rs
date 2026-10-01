//! HTTP contract of `ContextInjector`, the façade the SDK hands to a
//! conversation.
//!
//! `ContextInjector` owns a `MeilisearchMemoryProvider`, so every test here
//! drives it against a `wiremock` server bound on `127.0.0.1` with an
//! ephemeral port: no real Meilisearch, no outbound network, no shared state.
//! The config is built field by field so the `MEILISEARCH_URL` /
//! `MEILISEARCH_KEY` environment variables cannot leak in. What is asserted is
//! what the injector adds on top of the provider: the prompt prefix it builds,
//! the pagination defaults it chooses, and the calls it decides not to make.

#![cfg(feature = "memory")]

use nexus_claude::memory::{
    ContextInjector, MemoryConfig, MemoryError, MemoryProvider, MessageDocument,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A config that depends on nothing but the mock server's address.
fn config(url: &str) -> MemoryConfig {
    MemoryConfig {
        meilisearch_url: url.to_string(),
        meilisearch_key: None,
        messages_index: "nexus_messages".to_string(),
        conversations_index: "nexus_conversations".to_string(),
        summary_threshold: 500,
        max_context_items: 5,
        token_budget: 2000,
        min_relevance_score: 0.3,
        enabled: true,
    }
}

fn task_info(task_type: &str) -> Value {
    json!({
        "taskUid": 0,
        "indexUid": "nexus_messages",
        "status": "enqueued",
        "type": task_type,
        "enqueuedAt": "2026-10-01T00:00:00Z",
    })
}

/// Mounts the two `POST /indexes` + two `PATCH .../settings` calls that
/// `MeilisearchMemoryProvider::setup_indexes` performs, so that `new()`
/// succeeds.
async fn mount_bootstrap(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/indexes"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task_info("indexCreation")))
        .mount(server)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex(r"^/indexes/[^/]+/settings$"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task_info("settingsUpdate")))
        .mount(server)
        .await;
}

/// Boots an injector, then wipes the mocks and the request log so each test
/// asserts only on its own traffic.
async fn booted_injector(server: &MockServer) -> ContextInjector {
    booted_injector_with(server, config(&server.uri())).await
}

async fn booted_injector_with(server: &MockServer, cfg: MemoryConfig) -> ContextInjector {
    mount_bootstrap(server).await;
    let injector = ContextInjector::new(cfg)
        .await
        .expect("bootstrap against the mock server must succeed");
    server.reset().await;
    injector
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server
        .received_requests()
        .await
        .expect("the mock server records every request")
}

fn body_json(request: &Request) -> Value {
    serde_json::from_slice(&request.body).expect("the SDK always sends JSON")
}

fn search_response(hits: Vec<Value>, estimated_total_hits: usize) -> Value {
    json!({
        "hits": hits,
        "offset": 0,
        "limit": 20,
        "estimatedTotalHits": estimated_total_hits,
        "processingTimeMs": 1,
        "query": "",
    })
}

fn message_hit(id: &str, turn_index: usize, cwd: Option<&str>, ranking_score: f64) -> Value {
    let mut hit = json!({
        "id": id,
        "conversation_id": "conv-1",
        "role": "assistant",
        "content": format!("contenu de {id}"),
        "turn_index": turn_index,
        "created_at": 1_700_000_000i64,
        "_rankingScore": ranking_score,
    });
    if let Some(cwd) = cwd {
        hit["cwd"] = json!(cwd);
    }
    hit
}

async fn mount_messages_search(server: &MockServer, body: Value) {
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// new()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn new_refuses_a_disabled_config_without_calling_the_server() {
    let server = MockServer::start().await;
    mount_bootstrap(&server).await;

    let error = match ContextInjector::new(config(&server.uri()).with_enabled(false)).await {
        Err(error) => error,
        Ok(_) => panic!("a disabled config must not yield an injector"),
    };

    // The refusal comes from the provider, before any index is touched: the
    // `enabled` flag is therefore always true inside a live `ContextInjector`,
    // which makes its own `if !self.config.enabled` guards unreachable.
    assert!(matches!(error, MemoryError::Disabled), "got {error:?}");
    assert!(
        requests(&server).await.is_empty(),
        "nothing is sent for a disabled config"
    );
}

#[tokio::test]
async fn new_surfaces_a_rejected_settings_update() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/indexes"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task_info("indexCreation")))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex(r"^/indexes/[^/]+/settings$"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "message": "invalid settings",
            "code": "invalid_settings_filterable_attributes",
            "type": "invalid_request",
            "link": "https://docs.meilisearch.com/errors",
        })))
        .mount(&server)
        .await;

    let error = match ContextInjector::new(config(&server.uri())).await {
        Err(error) => error,
        Ok(_) => panic!("a rejected bootstrap must not yield an injector"),
    };

    assert!(
        matches!(error, MemoryError::Meilisearch(_)),
        "got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// get_context_prefix()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_context_prefix_wraps_the_retrieved_messages_in_a_prompt_header() {
    let server = MockServer::start().await;
    let injector = booted_injector(&server).await;
    mount_messages_search(
        &server,
        search_response(vec![message_hit("m-1", 0, Some("/w"), 2.0)], 1),
    )
    .await;

    let prefix = injector
        .get_context_prefix("jwt", Some("/w"), &["/w/a.rs".to_string()])
        .await
        .expect("the search succeeds")
        .expect("a hit above min_relevance_score produces a prefix");

    // The query travels as-is and the cwd becomes a Meilisearch filter; the
    // files only feed the local scorer.
    let body = body_json(&requests(&server).await[0]);
    assert_eq!(body["q"], "jwt");
    assert_eq!(body["filter"], r#"cwd = "/w""#);
    assert_eq!(
        body["limit"], 10,
        "max_context_items (5) is doubled for post-filtering"
    );

    assert!(
        prefix.starts_with("## Contexte historique (pour référence)"),
        "{prefix}"
    );
    assert!(prefix.contains("(assistant)"), "{prefix}");
    assert!(prefix.contains("contenu de m-1"), "{prefix}");
    assert!(
        prefix.ends_with("## Conversation actuelle (prioritaire)\n\n"),
        "the prefix hands the floor back to the live conversation: {prefix}"
    );
}

#[tokio::test]
async fn get_context_prefix_returns_none_when_the_index_is_empty() {
    let server = MockServer::start().await;
    let injector = booted_injector(&server).await;
    mount_messages_search(&server, search_response(Vec::new(), 0)).await;

    let prefix = injector
        .get_context_prefix("jwt", None, &[])
        .await
        .expect("the search succeeds");

    assert_eq!(prefix, None, "an empty result set injects nothing at all");
    assert!(
        body_json(&requests(&server).await[0])
            .get("filter")
            .is_none(),
        "no cwd means no filter"
    );
}

#[tokio::test]
async fn get_context_prefix_returns_none_when_every_hit_scores_too_low() {
    let server = MockServer::start().await;
    let injector = booted_injector(&server).await;
    // Ranking score 0.2 normalises to 0.1 semantic, and the hit carries no cwd
    // and no files: total 0.04, below the 0.3 floor. The server did answer
    // with a hit, yet nothing survives the local scoring.
    mount_messages_search(
        &server,
        search_response(vec![message_hit("m-1", 0, None, 0.2)], 1),
    )
    .await;

    let prefix = injector
        .get_context_prefix("jwt", Some("/w"), &[])
        .await
        .expect("the search succeeds");

    assert_eq!(prefix, None);
}

#[tokio::test]
async fn get_context_prefix_surfaces_a_server_rejection() {
    let server = MockServer::start().await;
    let injector = booted_injector(&server).await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "message": "cwd is not filterable",
            "code": "invalid_search_filter",
            "type": "invalid_request",
            "link": "https://docs.meilisearch.com/errors",
        })))
        .mount(&server)
        .await;

    let error = injector
        .get_context_prefix("jwt", Some("/w"), &[])
        .await
        .expect_err("a rejected search must not be swallowed into None");

    assert!(
        matches!(error, MemoryError::Meilisearch(_)),
        "got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// store_messages()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn store_messages_skips_the_round_trip_for_an_empty_batch() {
    let server = MockServer::start().await;
    let injector = booted_injector(&server).await;

    injector
        .store_messages(&[])
        .await
        .expect("an empty batch is a no-op");

    assert!(
        requests(&server).await.is_empty(),
        "an empty batch must not reach the network"
    );
}

#[tokio::test]
async fn store_messages_posts_the_batch_keyed_on_id() {
    let server = MockServer::start().await;
    let injector = booted_injector(&server).await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/documents"))
        .respond_with(
            ResponseTemplate::new(202).set_body_json(task_info("documentAdditionOrUpdate")),
        )
        .mount(&server)
        .await;

    injector
        .store_messages(&[
            MessageDocument::new("m-1", "conv-1", "user", "question", 0, 1_700_000_000),
            MessageDocument::new("m-2", "conv-1", "assistant", "réponse", 0, 1_700_000_100)
                .with_cwd("/w")
                .with_files_touched(vec!["/w/a.rs".to_string()]),
        ])
        .await
        .expect("the server accepts the batch");

    let reqs = requests(&server).await;
    assert_eq!(reqs.len(), 1, "one batched call, not one call per message");
    assert_eq!(reqs[0].url.query(), Some("primaryKey=id"));

    let documents = body_json(&reqs[0]);
    assert_eq!(documents.as_array().unwrap().len(), 2);
    assert_eq!(documents[0]["id"], "m-1");
    assert!(
        documents[0].get("cwd").is_none(),
        "an absent cwd is left out of the payload entirely"
    );
    assert_eq!(documents[1]["cwd"], "/w");
    assert_eq!(documents[1]["files_touched"], json!(["/w/a.rs"]));
}

#[tokio::test]
async fn store_messages_surfaces_a_server_rejection() {
    let server = MockServer::start().await;
    let injector = booted_injector(&server).await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/documents"))
        .respond_with(ResponseTemplate::new(413).set_body_json(json!({
            "message": "payload too large",
            "code": "payload_too_large",
            "type": "invalid_request",
            "link": "https://docs.meilisearch.com/errors",
        })))
        .mount(&server)
        .await;

    let error = injector
        .store_messages(&[MessageDocument::new(
            "m-1",
            "conv-1",
            "user",
            "question",
            0,
            1_700_000_000,
        )])
        .await
        .expect_err("a rejected write must be reported");

    assert!(
        matches!(error, MemoryError::Meilisearch(_)),
        "got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// provider()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_exposes_the_underlying_meilisearch_client() {
    let server = MockServer::start().await;
    let injector = booted_injector(&server).await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "available"})))
        .mount(&server)
        .await;

    // The accessor hands out the very provider the injector was built with:
    // calling through it reaches the same mock server.
    assert!(injector.provider().health_check().await.unwrap());
    assert_eq!(requests(&server).await[0].url.path(), "/health");
}

// ---------------------------------------------------------------------------
// load_conversation()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn load_conversation_defaults_to_fifty_newest_first() {
    let server = MockServer::start().await;
    let injector = booted_injector(&server).await;
    mount_messages_search(&server, search_response(Vec::new(), 0)).await;

    let loaded = injector
        .load_conversation("conv-1", None, None)
        .await
        .expect("the search succeeds");

    let body = body_json(&requests(&server).await[0]);
    assert_eq!(body["limit"], 50, "the documented default limit");
    assert_eq!(body["offset"], 0);
    assert_eq!(body["sort"], json!(["turn_index:desc"]));
    assert_eq!(body["filter"], r#"conversation_id = "conv-1""#);

    assert!(loaded.is_empty());
    assert_eq!(loaded.len(), 0);
    assert_eq!(loaded.limit, 50);
    assert!(!loaded.has_more);
    assert_eq!(loaded.max_turn_index(), None);
    assert_eq!(loaded.next_offset(), 0);
}

#[tokio::test]
async fn load_conversation_reports_pagination_and_chronological_order() {
    let server = MockServer::start().await;
    let injector = booted_injector(&server).await;
    mount_messages_search(
        &server,
        search_response(
            vec![
                message_hit("m-3", 2, Some("/w"), 1.0),
                message_hit("m-2", 1, Some("/w"), 1.0),
            ],
            7,
        ),
    )
    .await;

    let loaded = injector
        .load_conversation("conv-1", Some(2), Some(3))
        .await
        .expect("the search succeeds");

    let body = body_json(&requests(&server).await[0]);
    assert_eq!(body["limit"], 2);
    assert_eq!(body["offset"], 3);

    // The page arrives newest first; `has_more` is derived from the server's
    // estimated total, not from the page being full.
    assert_eq!(loaded.total_count, 7);
    assert!(loaded.has_more, "3 + 2 < 7");
    assert_eq!(loaded.offset, 3);
    assert_eq!(loaded.limit, 2);
    assert_eq!(loaded.next_offset(), 5);
    assert_eq!(loaded.max_turn_index(), Some(2));

    let ids: Vec<&str> = loaded
        .messages_chronological()
        .iter()
        .map(|m| m.id.as_str())
        .collect();
    assert_eq!(ids, vec!["m-2", "m-3"], "oldest first for a chat UI");
}

#[tokio::test]
async fn load_conversation_surfaces_a_server_rejection() {
    let server = MockServer::start().await;
    let injector = booted_injector(&server).await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "message": "turn_index is not sortable",
            "code": "invalid_search_sort",
            "type": "invalid_request",
            "link": "https://docs.meilisearch.com/errors",
        })))
        .mount(&server)
        .await;

    let error = injector
        .load_conversation("conv-1", None, None)
        .await
        .expect_err("a rejected search must be reported");

    assert!(
        matches!(error, MemoryError::Meilisearch(_)),
        "got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// list_conversations() / count_conversation_messages()
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_conversations_forwards_the_window_to_the_conversations_index() {
    let server = MockServer::start().await;
    let injector = booted_injector(&server).await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_conversations/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_response(
            vec![json!({
                "id": "conv-2",
                "content_preview": "récent",
                "model": "claude-opus-5",
                "created_at": 1_700_000_000i64,
                "updated_at": 1_700_009_000i64,
                "message_count": 4,
                "files_summary": ["/w/a.rs"],
            })],
            1,
        )))
        .mount(&server)
        .await;

    let conversations = injector
        .list_conversations(10, 20)
        .await
        .expect("the search succeeds");

    let body = body_json(&requests(&server).await[0]);
    assert_eq!(body["sort"], json!(["updated_at:desc"]));
    assert_eq!(body["limit"], 10);
    assert_eq!(body["offset"], 20);

    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].id, "conv-2");
    assert_eq!(conversations[0].message_count, 4);
}

#[tokio::test]
async fn count_conversation_messages_asks_for_no_hit_at_all() {
    let server = MockServer::start().await;
    let injector = booted_injector(&server).await;
    mount_messages_search(&server, search_response(Vec::new(), 142)).await;

    let count = injector
        .count_conversation_messages("conv-1")
        .await
        .expect("the search succeeds");

    let body = body_json(&requests(&server).await[0]);
    assert_eq!(body["limit"], 0, "only the estimated total is wanted");
    assert_eq!(body["filter"], r#"conversation_id = "conv-1""#);
    assert_eq!(count, 142);
}

#[tokio::test]
async fn count_conversation_messages_surfaces_a_server_rejection() {
    let server = MockServer::start().await;
    let injector = booted_injector(&server).await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "message": "conversation_id is not filterable",
            "code": "invalid_search_filter",
            "type": "invalid_request",
            "link": "https://docs.meilisearch.com/errors",
        })))
        .mount(&server)
        .await;

    let error = injector
        .count_conversation_messages("conv-1")
        .await
        .expect_err("a rejected search must be reported");

    assert!(
        matches!(error, MemoryError::Meilisearch(_)),
        "got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// Token budget, seen from the injector
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_context_prefix_keeps_one_message_even_over_the_token_budget() {
    let server = MockServer::start().await;
    let mut cfg = config(&server.uri());
    cfg.token_budget = 1; // 4 chars of budget
    cfg.min_relevance_score = 0.0;
    let injector = booted_injector_with(&server, cfg).await;
    mount_messages_search(
        &server,
        search_response(
            vec![
                message_hit("m-1", 0, Some("/w"), 2.0),
                message_hit("m-2", 1, Some("/w"), 2.0),
            ],
            2,
        ),
    )
    .await;

    let prefix = injector
        .get_context_prefix("jwt", Some("/w"), &[])
        .await
        .expect("the search succeeds")
        .expect("one message always survives the budget");

    // "contenu de m-1" is 14 chars against a 4-char budget: the first result
    // is kept anyway, the second is dropped.
    assert!(prefix.contains("contenu de m-1"), "{prefix}");
    assert!(
        !prefix.contains("contenu de m-2"),
        "the budget must stop after the first: {prefix}"
    );
}
