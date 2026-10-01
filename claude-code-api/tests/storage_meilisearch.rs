//! Behaviour of `core::storage::meilisearch` against a mock Meilisearch.
//!
//! `MeilisearchConfig.url` is the seam: the Meilisearch SDK speaks plain
//! HTTP/JSON, so a `wiremock::MockServer` can answer every call the wrapper
//! makes — including the `create_index` + `set_settings` bootstrap inside
//! `MeilisearchClient::new`. No service is started, and the assertions are made
//! on the **requests the wrapper sent**, which is the only way to tell a method
//! that targets the right index from one that quietly targets the wrong one.
//!
//! Contrast with `core::storage::neo4j`, which has no such seam: `neo4rs::Graph`
//! opens a binary Bolt handshake over TCP and exposes no injectable connection
//! trait. See `docs/diagrams/nexus-api-storage.mmd`.

mod support;

use claude_code_api::core::storage::meilisearch::{
    ConversationDocument, INDEX_CONVERSATIONS, INDEX_MESSAGES, MeilisearchClient,
    MeilisearchConfig, MeilisearchStats, MessageDocument,
};
use serde_json::{Value, json};
use support::http_mocks;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ===========================================================================
// Mock plumbing
// ===========================================================================

fn task(kind: &str) -> Value {
    json!({
        "taskUid": 1,
        "indexUid": "nexus_messages",
        "status": "enqueued",
        "type": kind,
        "details": null,
        "enqueuedAt": "2026-01-01T00:00:00Z",
    })
}

fn upstream_error() -> Value {
    json!({
        "message": "injected meilisearch failure",
        "code": "internal",
        "type": "internal",
        "link": "https://example.invalid",
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

fn index_stats(documents: usize, is_indexing: bool) -> Value {
    json!({
        "numberOfDocuments": documents,
        "numberOfEmbeddedDocuments": 0,
        "numberOfEmbeddings": 0,
        "rawDocumentDbSize": 0,
        "avgDocumentSize": 0,
        "isIndexing": is_indexing,
        "fieldDistribution": {},
    })
}

/// The two calls `MeilisearchClient::new` makes per index, both answered `202`.
async fn mount_bootstrap(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/indexes"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task("indexCreation")))
        .mount(server)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex(r"^/indexes/[^/]+/settings$"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task("settingsUpdate")))
        .mount(server)
        .await;
}

/// A `MockServer` that only answers the bootstrap: anything else is a `404`,
/// which the SDK surfaces as an error. Each test mounts the routes it wants.
async fn bootstrapped_server() -> MockServer {
    let server = MockServer::start().await;
    mount_bootstrap(&server).await;
    server
}

async fn connect(server: &MockServer) -> MeilisearchClient {
    MeilisearchClient::new(MeilisearchConfig {
        url: server.uri(),
        api_key: None,
    })
    .await
    .expect("bootstrap must succeed against a mock that answers 202")
}

/// Every request the server saw, as `("METHOD /path?query", body)`.
async fn trace(server: &MockServer) -> Vec<(String, Value)> {
    server
        .received_requests()
        .await
        .expect("wiremock records requests by default")
        .into_iter()
        .map(|request| {
            let query = request
                .url
                .query()
                .map(|q| format!("?{q}"))
                .unwrap_or_default();
            let line = format!("{} {}{}", request.method, request.url.path(), query);
            let body = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            (line, body)
        })
        .collect()
}

/// The request lines only, in the order the wrapper sent them.
async fn request_lines(server: &MockServer) -> Vec<String> {
    trace(server)
        .await
        .into_iter()
        .map(|(line, _)| line)
        .collect()
}

/// The body of the single request whose line starts with `prefix`.
async fn only_body(server: &MockServer, prefix: &str) -> Value {
    let matching: Vec<Value> = trace(server)
        .await
        .into_iter()
        .filter(|(line, _)| line.starts_with(prefix))
        .map(|(_, body)| body)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one `{prefix}` request, saw {}",
        matching.len()
    );
    matching.into_iter().next().unwrap()
}

/// How many `"` in `expression` are *not* preceded by a backslash — i.e. how many
/// of them Meilisearch's filter parser treats as string delimiters. A correctly
/// escaped `field = "value"` has exactly two.
fn unescaped_quotes(expression: &str) -> usize {
    let mut count = 0;
    let mut escaped = false;
    for character in expression.chars() {
        match character {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => count += 1,
            _ => {},
        }
    }
    count
}

fn message(id: &str, conversation_id: &str) -> MessageDocument {
    MessageDocument {
        id: id.to_string(),
        conversation_id: conversation_id.to_string(),
        role: "user".to_string(),
        content: "comment indexer un message".to_string(),
        turn_index: 3,
        created_at: 1_700_000_000,
    }
}

fn message_hit(id: &str) -> Value {
    json!({
        "id": id,
        "conversation_id": "conv-1",
        "role": "assistant",
        "content": "réponse",
        "turn_index": 1,
        "created_at": 42,
    })
}

// ===========================================================================
// MeilisearchConfig::default — the env-var contract
// ===========================================================================

/// `Default` is the only place the two environment variables are read, and it
/// reads them *once per call* (no caching), so a test can observe both branches.
#[test]
#[serial_test::serial(meilisearch_env)]
fn config_default_prefers_the_environment() {
    // SAFETY: serialised against the other test that touches these variables.
    unsafe {
        std::env::set_var("MEILISEARCH_URL", "http://meili.invalid:1234");
        std::env::set_var("MEILISEARCH_KEY", "not-a-real-key");
    }
    let config = MeilisearchConfig::default();
    unsafe {
        std::env::remove_var("MEILISEARCH_URL");
        std::env::remove_var("MEILISEARCH_KEY");
    }

    assert_eq!(config.url, "http://meili.invalid:1234");
    assert_eq!(config.api_key.as_deref(), Some("not-a-real-key"));
}

/// Without the variables the config points at `localhost:7700` with no key —
/// a *silent* default: nothing refuses to start when Meilisearch is unconfigured.
#[test]
#[serial_test::serial(meilisearch_env)]
fn config_default_falls_back_to_localhost_without_env() {
    // SAFETY: serialised against the other test that touches these variables.
    unsafe {
        std::env::remove_var("MEILISEARCH_URL");
        std::env::remove_var("MEILISEARCH_KEY");
    }
    let config = MeilisearchConfig::default();

    assert_eq!(config.url, "http://localhost:7700");
    assert_eq!(config.api_key, None);
}

/// The module doc-comment claims both index names are prefixed with `nexus_`
/// "to avoid conflicts with other applications".
#[test]
fn index_names_carry_the_nexus_prefix() {
    assert_eq!(INDEX_MESSAGES, "nexus_messages");
    assert_eq!(INDEX_CONVERSATIONS, "nexus_conversations");
}

// ===========================================================================
// MeilisearchClient::new / init_indexes
// ===========================================================================

/// `new` bootstraps both indexes: one `create_index` and one `set_settings`
/// each, in that order, with `id` as primary key and the attribute lists the
/// module header documents.
#[tokio::test]
async fn new_creates_and_configures_both_indexes() {
    let server = bootstrapped_server().await;
    let _client = connect(&server).await;

    assert_eq!(
        request_lines(&server).await,
        vec![
            "POST /indexes".to_string(),
            "PATCH /indexes/nexus_messages/settings".to_string(),
            "POST /indexes".to_string(),
            "PATCH /indexes/nexus_conversations/settings".to_string(),
        ]
    );

    let creations: Vec<Value> = trace(&server)
        .await
        .into_iter()
        .filter(|(line, _)| line == "POST /indexes")
        .map(|(_, body)| body)
        .collect();
    assert_eq!(
        creations,
        vec![
            json!({"uid": "nexus_messages", "primaryKey": "id"}),
            json!({"uid": "nexus_conversations", "primaryKey": "id"}),
        ]
    );

    let messages = only_body(&server, "PATCH /indexes/nexus_messages/settings").await;
    assert_eq!(messages["searchableAttributes"], json!(["content", "role"]));
    assert_eq!(
        messages["filterableAttributes"],
        json!(["conversation_id", "role", "created_at"])
    );
    assert_eq!(
        messages["sortableAttributes"],
        json!(["created_at", "turn_index"])
    );

    let conversations = only_body(&server, "PATCH /indexes/nexus_conversations/settings").await;
    assert_eq!(
        conversations["searchableAttributes"],
        json!(["content_preview", "model"])
    );
    assert_eq!(
        conversations["filterableAttributes"],
        json!(["model", "created_at", "updated_at"])
    );
    assert_eq!(
        conversations["sortableAttributes"],
        json!(["created_at", "updated_at", "message_count"])
    );
}

/// `init_indexes` drops the result of `create_index` with `.ok()` — the comment
/// says "Ignore if exists", but the discard is unconditional: a Meilisearch that
/// refuses index creation outright still yields a usable client, as long as the
/// settings calls succeed.
#[tokio::test]
async fn new_ignores_a_rejected_create_index() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/indexes"))
        .respond_with(ResponseTemplate::new(500).set_body_json(upstream_error()))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex(r"^/indexes/[^/]+/settings$"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task("settingsUpdate")))
        .mount(&server)
        .await;

    assert!(
        MeilisearchClient::new(MeilisearchConfig {
            url: server.uri(),
            api_key: None,
        })
        .await
        .is_ok(),
        "a create_index rejection is swallowed by `.ok()`"
    );
    // Both indexes were still configured, so the failure really was ignored
    // rather than short-circuiting the bootstrap.
    assert_eq!(request_lines(&server).await.len(), 4);
}

/// A rejected `set_settings`, by contrast, is propagated and no client is built.
#[tokio::test]
async fn new_fails_when_the_messages_settings_are_rejected() {
    let server = http_mocks::meilisearch_failing(500).await;
    let error = match MeilisearchClient::new(MeilisearchConfig {
        url: server.uri(),
        api_key: None,
    })
    .await
    {
        Ok(_) => panic!("a 500 on set_settings must fail `new`"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("injected meilisearch failure"),
        "the upstream message must reach the caller, got: {error}"
    );
    // It gave up on the messages index: the conversations index was never touched.
    let lines = request_lines(&server).await;
    assert_eq!(
        lines,
        vec![
            "POST /indexes".to_string(),
            "PATCH /indexes/nexus_messages/settings".to_string(),
        ]
    );
}

/// The failure can also come from the *second* index, after the first one is
/// already configured — the `conversations_index.set_settings` arm.
#[tokio::test]
async fn new_fails_when_the_conversations_settings_are_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/indexes"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task("indexCreation")))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/indexes/nexus_messages/settings"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task("settingsUpdate")))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/indexes/nexus_conversations/settings"))
        .respond_with(ResponseTemplate::new(400).set_body_json(upstream_error()))
        .mount(&server)
        .await;

    let error = match MeilisearchClient::new(MeilisearchConfig {
        url: server.uri(),
        api_key: None,
    })
    .await
    {
        Ok(_) => panic!("a 400 on the conversations settings must fail `new`"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("injected meilisearch failure"),
        "got: {error}"
    );
    assert_eq!(request_lines(&server).await.len(), 4);
}

/// The index accessors are pure name lookups: they issue no request at all, so
/// they cannot fail and cannot be the reason a later call is slow.
#[tokio::test]
async fn index_accessors_issue_no_request() {
    let server = bootstrapped_server().await;
    let client = connect(&server).await;
    let before = request_lines(&server).await.len();

    assert_eq!(client.messages_index().uid, INDEX_MESSAGES);
    assert_eq!(client.conversations_index().uid, INDEX_CONVERSATIONS);

    assert_eq!(request_lines(&server).await.len(), before);
}

// ===========================================================================
// Indexing
// ===========================================================================

#[tokio::test]
async fn index_message_posts_one_document_to_the_messages_index() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/documents"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task("documentAdditionOrUpdate")))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    client
        .index_message(message("msg-1", "conv-1"))
        .await
        .expect("a 202 means the document was accepted");

    let lines = request_lines(&server).await;
    assert_eq!(
        lines.last().map(String::as_str),
        Some("POST /indexes/nexus_messages/documents?primaryKey=id"),
        "the primary key must travel with the batch, got {lines:?}"
    );
    let body = only_body(&server, "POST /indexes/nexus_messages/documents").await;
    assert_eq!(
        body,
        json!([{
            "id": "msg-1",
            "conversation_id": "conv-1",
            "role": "user",
            "content": "comment indexer un message",
            "turn_index": 3,
            "created_at": 1_700_000_000i64,
        }]),
        "all six fields of MessageDocument must be serialised, snake_case"
    );
}

#[tokio::test]
async fn index_message_propagates_an_upstream_rejection() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/documents"))
        .respond_with(ResponseTemplate::new(413).set_body_json(upstream_error()))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    let error = client
        .index_message(message("msg-1", "conv-1"))
        .await
        .expect_err("a 413 must not be swallowed");
    assert!(error.to_string().contains("injected meilisearch failure"));
}

/// The `docs.is_empty()` guard short-circuits before any index lookup: an empty
/// batch is a no-op, not an empty `POST`. Proven by the request count.
#[tokio::test]
async fn index_messages_with_an_empty_batch_touches_no_index() {
    let server = bootstrapped_server().await;
    let client = connect(&server).await;
    let before = request_lines(&server).await;

    client
        .index_messages(Vec::new())
        .await
        .expect("an empty batch is Ok(())");

    assert_eq!(
        request_lines(&server).await,
        before,
        "an empty batch must not reach the network"
    );
}

#[tokio::test]
async fn index_messages_posts_the_whole_batch_in_one_request() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/documents"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task("documentAdditionOrUpdate")))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    client
        .index_messages(vec![
            message("msg-1", "conv-1"),
            message("msg-2", "conv-1"),
            message("msg-3", "conv-2"),
        ])
        .await
        .expect("a 202 means the batch was accepted");

    // `only_body` asserts there was exactly one such request: no per-document loop.
    let body = only_body(&server, "POST /indexes/nexus_messages/documents").await;
    let ids: Vec<&str> = body
        .as_array()
        .expect("the batch is a JSON array")
        .iter()
        .map(|doc| doc["id"].as_str().expect("id is a string"))
        .collect();
    assert_eq!(ids, vec!["msg-1", "msg-2", "msg-3"]);
}

#[tokio::test]
async fn index_messages_propagates_an_upstream_rejection() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/documents"))
        .respond_with(ResponseTemplate::new(500).set_body_json(upstream_error()))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    let error = client
        .index_messages(vec![message("msg-1", "conv-1")])
        .await
        .expect_err("a 500 must not be swallowed");
    assert!(error.to_string().contains("injected meilisearch failure"));
}

#[tokio::test]
async fn index_conversation_posts_to_the_conversations_index() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_conversations/documents"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task("documentAdditionOrUpdate")))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    client
        .index_conversation(ConversationDocument {
            id: "conv-1".to_string(),
            model: None,
            message_count: 2,
            total_tokens: 17,
            created_at: 1,
            updated_at: 2,
            content_preview: "aperçu".to_string(),
        })
        .await
        .expect("a 202 means the document was accepted");

    let body = only_body(&server, "POST /indexes/nexus_conversations/documents").await;
    assert_eq!(
        body,
        json!([{
            "id": "conv-1",
            "model": null,
            "message_count": 2,
            "total_tokens": 17,
            "created_at": 1,
            "updated_at": 2,
            "content_preview": "aperçu",
        }]),
        "a `None` model is sent as JSON null, not omitted"
    );
}

#[tokio::test]
async fn index_conversation_propagates_an_upstream_rejection() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_conversations/documents"))
        .respond_with(ResponseTemplate::new(500).set_body_json(upstream_error()))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    let error = client
        .index_conversation(ConversationDocument {
            id: "conv-1".to_string(),
            model: Some("claude-opus-4".to_string()),
            message_count: 0,
            total_tokens: 0,
            created_at: 0,
            updated_at: 0,
            content_preview: String::new(),
        })
        .await
        .expect_err("a 500 must not be swallowed");
    assert!(error.to_string().contains("injected meilisearch failure"));
}

// ===========================================================================
// Search
// ===========================================================================

#[tokio::test]
async fn search_messages_without_a_conversation_sends_no_filter() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_results(vec![])))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    let hits = client
        .search_messages("indexer", None, 5)
        .await
        .expect("an empty hit list is Ok");
    assert!(hits.is_empty());

    let body = only_body(&server, "POST /indexes/nexus_messages/search").await;
    assert_eq!(body["q"], json!("indexer"));
    assert_eq!(body["limit"], json!(5));
    assert!(
        body.get("filter").is_none_or(Value::is_null),
        "no conversation means no filter key, got {body}"
    );
}

#[tokio::test]
async fn search_messages_quotes_the_conversation_filter() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_results(vec![])))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    client
        .search_messages("indexer", Some("conv-1"), 5)
        .await
        .expect("an empty hit list is Ok");

    let body = only_body(&server, "POST /indexes/nexus_messages/search").await;
    assert_eq!(body["filter"], json!(r#"conversation_id = "conv-1""#));
}

/// Regression test for the filter injection: `conversation_id` reaches
/// `search_messages` straight from the HTTP layer, and it is interpolated into a
/// Meilisearch filter expression. Without escaping, an id carrying a `"` closes
/// the string literal and the rest of the id is parsed as filter syntax — which
/// `delete_conversation_messages` then turns into deletions.
///
/// Meilisearch's filter parser accepts `\"` and `\\` inside a quoted value, so
/// the escaped form below is both safe and still a plain equality test.
#[tokio::test]
async fn search_messages_escapes_quotes_in_the_conversation_id() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_results(vec![])))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    client
        .search_messages("", Some(r#"x" OR role = "user"#), 10)
        .await
        .expect("an empty hit list is Ok");

    let body = only_body(&server, "POST /indexes/nexus_messages/search").await;
    let filter = body["filter"].as_str().expect("filter is a string");
    assert_eq!(
        filter, r#"conversation_id = "x\" OR role = \"user""#,
        "the quotes must be escaped, not left to close the literal"
    );
    assert_eq!(
        unescaped_quotes(filter),
        2,
        "only the two delimiters may be unescaped quotes, otherwise the value \
         ends early and the rest becomes filter syntax: {filter}"
    );
}

/// A backslash in the id must not escape the closing quote either.
#[tokio::test]
async fn search_messages_escapes_backslashes_in_the_conversation_id() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_results(vec![])))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    client
        .search_messages("", Some(r"conv\"), 10)
        .await
        .expect("an empty hit list is Ok");

    let body = only_body(&server, "POST /indexes/nexus_messages/search").await;
    assert_eq!(
        body["filter"].as_str().expect("filter is a string"),
        r#"conversation_id = "conv\\""#
    );
}

#[tokio::test]
async fn search_messages_returns_the_hits_as_documents() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(search_results(vec![
                message_hit("msg-a"),
                message_hit("msg-b"),
            ])),
        )
        .mount(&server)
        .await;
    let client = connect(&server).await;

    let hits = client
        .search_messages("réponse", None, 2)
        .await
        .expect("well-formed hits deserialise");

    assert_eq!(
        hits.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["msg-a", "msg-b"],
        "hit order is preserved"
    );
    assert_eq!(hits[0].conversation_id, "conv-1");
    assert_eq!(hits[0].role, "assistant");
    assert_eq!(hits[0].content, "réponse");
    assert_eq!(hits[0].turn_index, 1);
    assert_eq!(hits[0].created_at, 42);
}

/// `MessageDocument` has no `#[serde(default)]`: a hit missing a field is a hard
/// error, not a document with a blank role. Documents indexed by anything other
/// than this wrapper therefore break the whole search, not just one hit.
#[tokio::test]
async fn search_messages_rejects_a_hit_missing_a_field() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(search_results(vec![json!({
                "id": "msg-a",
                "conversation_id": "conv-1",
                "content": "pas de rôle",
                "turn_index": 0,
                "created_at": 0,
            })])),
        )
        .mount(&server)
        .await;
    let client = connect(&server).await;

    let error = client
        .search_messages("", None, 1)
        .await
        .expect_err("a hit without `role` cannot become a MessageDocument");
    assert!(
        error.to_string().contains("role"),
        "the error must name the missing field, got: {error}"
    );
}

#[tokio::test]
async fn search_conversations_sends_query_and_limit_and_maps_hits() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_conversations/search"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(search_results(vec![json!({
                "id": "conv-9",
                "model": "claude-opus-4",
                "message_count": 4,
                "total_tokens": 120,
                "created_at": 7,
                "updated_at": 8,
                "content_preview": "aperçu de la conversation",
            })])),
        )
        .mount(&server)
        .await;
    let client = connect(&server).await;

    let hits = client
        .search_conversations("aperçu", 3)
        .await
        .expect("well-formed hits deserialise");

    let body = only_body(&server, "POST /indexes/nexus_conversations/search").await;
    assert_eq!(body["q"], json!("aperçu"));
    assert_eq!(body["limit"], json!(3));
    assert!(
        body.get("filter").is_none_or(Value::is_null),
        "`search_conversations` has no filter parameter at all"
    );

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "conv-9");
    assert_eq!(hits[0].model.as_deref(), Some("claude-opus-4"));
    assert_eq!(hits[0].message_count, 4);
    assert_eq!(hits[0].total_tokens, 120);
    assert_eq!(hits[0].created_at, 7);
    assert_eq!(hits[0].updated_at, 8);
    assert_eq!(hits[0].content_preview, "aperçu de la conversation");
}

#[tokio::test]
async fn search_conversations_propagates_an_upstream_rejection() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_conversations/search"))
        .respond_with(ResponseTemplate::new(503).set_body_json(upstream_error()))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    let error = client
        .search_conversations("aperçu", 3)
        .await
        .expect_err("a 503 must not become an empty hit list");
    assert!(error.to_string().contains("injected meilisearch failure"));
}

// ===========================================================================
// Deletion
// ===========================================================================

#[tokio::test]
async fn delete_message_targets_the_messages_index() {
    let server = bootstrapped_server().await;
    Mock::given(method("DELETE"))
        .and(path("/indexes/nexus_messages/documents/msg-1"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task("documentDeletion")))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    client.delete_message("msg-1").await.expect("202 is Ok");

    assert_eq!(
        request_lines(&server).await.last().map(String::as_str),
        Some("DELETE /indexes/nexus_messages/documents/msg-1")
    );
}

#[tokio::test]
async fn delete_message_propagates_an_upstream_rejection() {
    let server = bootstrapped_server().await;
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/indexes/nexus_messages/documents/.+$"))
        .respond_with(ResponseTemplate::new(500).set_body_json(upstream_error()))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    let error = client
        .delete_message("msg-1")
        .await
        .expect_err("a 500 must not be swallowed");
    assert!(error.to_string().contains("injected meilisearch failure"));
}

/// `delete_conversation_messages` is a search-then-delete loop with a hard-coded
/// `limit` of 1000 and no pagination: it asks for at most 1000 ids, once.
#[tokio::test]
async fn delete_conversation_messages_asks_for_at_most_1000_messages() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_results(vec![])))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    client
        .delete_conversation_messages("conv-1")
        .await
        .expect("no hits means nothing to delete");

    let body = only_body(&server, "POST /indexes/nexus_messages/search").await;
    assert_eq!(body["limit"], json!(1000));
    assert_eq!(
        body["q"],
        json!(""),
        "it matches on the filter, not on text"
    );
    assert_eq!(body["filter"], json!(r#"conversation_id = "conv-1""#));
    assert_eq!(
        trace(&server)
            .await
            .iter()
            .filter(|(line, _)| line.starts_with("POST /indexes/nexus_messages/search"))
            .count(),
        1,
        "there is no second page: messages past the 1000th stay in the index"
    );
}

#[tokio::test]
async fn delete_conversation_messages_deletes_every_hit_by_id() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(search_results(vec![
                message_hit("msg-a"),
                message_hit("msg-b"),
            ])),
        )
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/indexes/nexus_messages/documents/.+$"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task("documentDeletion")))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    client
        .delete_conversation_messages("conv-1")
        .await
        .expect("202 on each delete is Ok");

    let lines = request_lines(&server).await;
    assert_eq!(
        &lines[lines.len() - 2..],
        [
            "DELETE /indexes/nexus_messages/documents/msg-a".to_string(),
            "DELETE /indexes/nexus_messages/documents/msg-b".to_string(),
        ]
    );
}

/// **Swallowed errors.** The loop body is `let _ = index.delete_document(..)`:
/// every individual deletion may fail and the function still returns `Ok(())`.
/// A caller that reports "conversation deleted" is therefore lying whenever
/// Meilisearch is degraded, and the orphaned messages stay searchable.
#[tokio::test]
async fn delete_conversation_messages_reports_success_when_every_delete_fails() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(search_results(vec![
                message_hit("msg-a"),
                message_hit("msg-b"),
            ])),
        )
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/indexes/nexus_messages/documents/.+$"))
        .respond_with(ResponseTemplate::new(500).set_body_json(upstream_error()))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    let outcome = client.delete_conversation_messages("conv-1").await;

    assert!(
        outcome.is_ok(),
        "today `let _ =` hides the failures; this pins that behaviour"
    );
    assert_eq!(
        trace(&server)
            .await
            .iter()
            .filter(|(line, _)| line.starts_with("DELETE /indexes/nexus_messages/documents/"))
            .count(),
        2,
        "it does keep going after the first failure, rather than stopping"
    );
}

/// The search *is* propagated, though: a conversation whose messages cannot be
/// listed is not reported as cleaned up.
#[tokio::test]
async fn delete_conversation_messages_propagates_a_search_failure() {
    let server = bootstrapped_server().await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(ResponseTemplate::new(500).set_body_json(upstream_error()))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    let error = client
        .delete_conversation_messages("conv-1")
        .await
        .expect_err("a failed listing must surface");
    assert!(error.to_string().contains("injected meilisearch failure"));
}

#[tokio::test]
async fn delete_conversation_deletes_the_document_then_its_messages() {
    let server = bootstrapped_server().await;
    Mock::given(method("DELETE"))
        .and(path("/indexes/nexus_conversations/documents/conv-1"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task("documentDeletion")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(search_results(vec![message_hit("msg-a")])),
        )
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/indexes/nexus_messages/documents/msg-a"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task("documentDeletion")))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    client
        .delete_conversation("conv-1")
        .await
        .expect("202 is Ok");

    let lines = request_lines(&server).await;
    assert_eq!(
        &lines[lines.len() - 3..],
        [
            "DELETE /indexes/nexus_conversations/documents/conv-1".to_string(),
            "POST /indexes/nexus_messages/search".to_string(),
            "DELETE /indexes/nexus_messages/documents/msg-a".to_string(),
        ],
        "the conversation document goes first, then its messages"
    );
}

/// Because the conversation document is deleted *first* and its `?` aborts the
/// function, a failure there leaves every message of the conversation in the
/// messages index — the cleanup is not atomic and has no compensating path.
#[tokio::test]
async fn delete_conversation_orphans_messages_when_the_document_delete_fails() {
    let server = bootstrapped_server().await;
    Mock::given(method("DELETE"))
        .and(path("/indexes/nexus_conversations/documents/conv-1"))
        .respond_with(ResponseTemplate::new(500).set_body_json(upstream_error()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(search_results(vec![])))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    let error = client
        .delete_conversation("conv-1")
        .await
        .expect_err("a 500 on the conversation document must surface");
    assert!(error.to_string().contains("injected meilisearch failure"));
    assert!(
        !request_lines(&server)
            .await
            .iter()
            .any(|line| line.starts_with("POST /indexes/nexus_messages/search")),
        "the messages were never even listed, so they stay indexed"
    );
}

// ===========================================================================
// Stats
// ===========================================================================

/// Each count must come from its own index. A mock with different numbers per
/// index is the only way to catch the two being swapped.
#[tokio::test]
async fn get_stats_keeps_each_count_on_its_own_field() {
    let server = bootstrapped_server().await;
    Mock::given(method("GET"))
        .and(path("/indexes/nexus_messages/stats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(index_stats(7, false)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/indexes/nexus_conversations/stats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(index_stats(3, false)))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    let stats = client.get_stats().await.expect("both indexes answered");

    assert_eq!(stats.messages_count, 7);
    assert_eq!(stats.conversations_count, 3);
    assert!(!stats.is_indexing);
}

/// `is_indexing` is an OR: either index being busy makes the whole wrapper
/// report "indexing".
#[tokio::test]
async fn get_stats_reports_indexing_when_only_conversations_are_busy() {
    let server = bootstrapped_server().await;
    Mock::given(method("GET"))
        .and(path("/indexes/nexus_messages/stats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(index_stats(0, false)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/indexes/nexus_conversations/stats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(index_stats(0, true)))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    let stats = client.get_stats().await.expect("both indexes answered");
    assert!(stats.is_indexing);
}

/// The messages index is queried first and its `?` aborts: the conversations
/// index is never asked.
#[tokio::test]
async fn get_stats_propagates_an_upstream_rejection() {
    let server = bootstrapped_server().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/indexes/[^/]+/stats$"))
        .respond_with(ResponseTemplate::new(500).set_body_json(upstream_error()))
        .mount(&server)
        .await;
    let client = connect(&server).await;

    let error = client
        .get_stats()
        .await
        .expect_err("a 500 must not become zeroed stats");
    assert!(error.to_string().contains("injected meilisearch failure"));
    assert_eq!(
        trace(&server)
            .await
            .iter()
            .filter(|(line, _)| line.ends_with("/stats"))
            .count(),
        1,
        "it short-circuits on the messages index"
    );
}

/// `MeilisearchStats` is `Serialize` because it is handed to HTTP callers: pin
/// the wire field names.
#[test]
fn stats_serialise_with_snake_case_field_names() {
    let json = serde_json::to_value(MeilisearchStats {
        messages_count: 12,
        conversations_count: 4,
        is_indexing: true,
    })
    .expect("MeilisearchStats is Serialize");
    assert_eq!(
        json,
        json!({"messages_count": 12, "conversations_count": 4, "is_indexing": true})
    );
}

// ===========================================================================
// Domain findings outside this file
//
// `docs/diagrams/nexus-api-storage.mmd` is owned here and covers the whole
// `core::storage` domain, so the divergences it records get their evidence here.
// The fixes belong to the owners of those files.
// ===========================================================================

/// `TieredCache::new` spawns `l1_cleanup_loop` with `cache.l1.clone()`.
/// `dashmap::DashMap` implements `Clone` by *copying every shard into fresh
/// locks*, not by sharing a handle — so the background loop is handed a detached
/// snapshot of an L1 that is still empty, and it prunes that copy forever while
/// the real `self.l1` is never touched. This pins the premise; the ignored test
/// below is the end-to-end reproduction.
#[test]
fn dashmap_clone_detaches_the_map_instead_of_sharing_it() {
    let original: dashmap::DashMap<String, u8> = dashmap::DashMap::new();
    let handed_to_the_background_task = original.clone();

    original.insert("entrée".to_string(), 1);

    assert_eq!(
        handed_to_the_background_task.len(),
        0,
        "if this ever becomes 1, DashMap started sharing and the l1_cleanup_loop \
         bug in storage/tiered_cache.rs is fixed by that alone"
    );
    assert_eq!(original.len(), 1);
}

/// 🔴 `core::storage::tiered_cache::TieredCache::l1_cleanup_loop` never evicts
/// anything from the live L1.
///
/// Trigger: `TieredCache::memory_only(TieredCacheConfig { l1_ttl_seconds: 1, .. })`,
/// one `put`, then wait past the loop's hard-coded 300-second tick.
/// Today the entry is still in L1; `extended_stats().l1_entries` stays at 1,
/// because `TieredCache::new` passed the loop a `DashMap::clone()` of the L1
/// (see the test above). Entries only ever leave L1 through the lazy TTL check
/// in `get_l1` or through an explicit `CacheStore::cleanup`.
///
/// Ignored: the fix is in `storage/tiered_cache.rs`, not in this agent's scope,
/// and the loop's tick is a hard-coded `sleep(300)` with no injectable clock, so
/// the reproduction costs five minutes of wall time. Run with
/// `cargo test -p claude-code-api --test storage_meilisearch -- --ignored`.
#[tokio::test]
#[ignore = "waits out the hard-coded 300s tick of l1_cleanup_loop"]
async fn tiered_cache_cleanup_loop_never_prunes_the_live_l1() {
    use claude_code_api::core::storage::{CacheStore, TieredCache, TieredCacheConfig};

    let cache = TieredCache::memory_only(TieredCacheConfig {
        l1_ttl_seconds: 1,
        l2_enabled: false,
        ..TieredCacheConfig::default()
    });
    cache
        .put("k".to_string(), support::openai::response("r", "m"))
        .await;

    // One full tick of the loop, plus slack.
    tokio::time::sleep(std::time::Duration::from_secs(305)).await;

    assert_eq!(
        cache.extended_stats().l1_entries,
        0,
        "an entry 305s past a 1s TTL must have been pruned by l1_cleanup_loop"
    );
}
