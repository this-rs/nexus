//! Behaviour tests for [`claude_code_api::core::storage::neo4j`].
//!
//! This module is the Neo4j backend for conversations and sessions. Every one of
//! its methods is a Cypher statement plus a hand-written decoding of the rows
//! that come back, so there are two things worth asserting and they are not the
//! same thing: **what it writes** (the statement and its parameter map) and
//! **what it does with what it reads** (a missing property, a null, a timestamp
//! in a shape it did not expect).
//!
//! The fake Bolt server in [`fake_bolt_s05`] re-reads the Cypher and its
//! parameters, which is what makes the first half assertable at all. See that
//! module for why a Bolt server rather than `wiremock`, and for the honest limit:
//! it does not *interpret* Cypher, so the semantics of
//! `DETACH DELETE c, m RETURN count(c) as deleted` stay out of reach.
//!
//! Several tests below are named after a defect rather than a feature. They are
//! deliberate: each one pins down what the code does today, with a comment
//! saying what a caller would have expected instead. The recurring one is
//! `Option::unwrap_or_default()` on the way in: `create(None)` stores the empty
//! string, not `null`, so "no model" and "no project path" never round-trip back
//! to `None`.
//!
//! One hypothesis this file *refutes*: writing `created_at: datetime($now)` and
//! reading it back with `node.get::<String>(..)` looks like a type error, and it
//! is not — `neo4rs` renders temporal values to RFC 3339 on demand. The
//! `pack_datetime` tests exist to keep that true across upgrades.

mod fake_bolt_s05;

use chrono::{DateTime, Utc};
use claude_code_api::core::conversation::ConversationMetadata;
use claude_code_api::core::storage::{
    ConversationStore, Neo4jClient, Neo4jConfig, Neo4jConversationStore, Neo4jSessionStore,
    SessionStore,
};
use claude_code_api::models::openai::{ChatMessage, ContentPart, ImageUrl, MessageContent};
use fake_bolt_s05::{
    FakeBolt, pack_datetime, pack_int, pack_list, pack_node, pack_null, pack_string,
};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// An RFC 3339 instant, i.e. the shape `parse_neo4j_datetime` can actually read.
const CREATED: &str = "2026-10-01T08:30:00+00:00";
const UPDATED: &str = "2026-10-01T09:45:00+00:00";

/// The user and password the fake server ignores — it performs no auth at all.
const FAKE_USER: &str = "neo4j";
const FAKE_AUTH: &str = "fake-bolt-has-no-auth";

fn config_for(bolt: &FakeBolt) -> Neo4jConfig {
    Neo4jConfig {
        uri: bolt.uri(),
        user: FAKE_USER.to_string(),
        password: FAKE_AUTH.to_string(),
        max_connections: 4,
    }
}

/// A client pointed at `bolt`, with the three `init_schema` statements already
/// forgotten so that a test's assertions only see its own traffic.
async fn client_for(bolt: &FakeBolt) -> Neo4jClient {
    let client = Neo4jClient::new(config_for(bolt))
        .await
        .expect("the neo4rs pool is lazy, so this cannot fail against a live listener");
    bolt.forget_runs();
    client
}

async fn conversations(bolt: &FakeBolt) -> Neo4jConversationStore {
    Neo4jConversationStore::new(client_for(bolt).await)
}

async fn sessions(bolt: &FakeBolt) -> Neo4jSessionStore {
    Neo4jSessionStore::new(client_for(bolt).await)
}

fn conversation_node(props: &[(&str, Vec<u8>)]) -> Vec<u8> {
    pack_node(1, &["NexusConversation"], props)
}

/// A conversation node with every property in the shape the decoder expects.
fn healthy_conversation_node() -> Vec<u8> {
    conversation_node(&[
        ("model", pack_string("claude-opus-5")),
        ("total_tokens", pack_int(1234)),
        ("turn_count", pack_int(2)),
        ("created_at", pack_string(CREATED)),
        ("updated_at", pack_string(UPDATED)),
    ])
}

fn message_node(role: Vec<u8>, content: Vec<u8>) -> Vec<u8> {
    pack_node(
        2,
        &["NexusMessage"],
        &[("role", role), ("content", content)],
    )
}

fn session_node(props: &[(&str, Vec<u8>)]) -> Vec<u8> {
    pack_node(3, &["NexusSession"], props)
}

fn healthy_session_node(id: &str, project_path: Vec<u8>) -> Vec<u8> {
    session_node(&[
        ("id", pack_string(id)),
        ("project_path", project_path),
        ("created_at", pack_string(CREATED)),
        ("updated_at", pack_string(UPDATED)),
    ])
}

fn text_of(message: &ChatMessage) -> Option<&str> {
    match &message.content {
        Some(MessageContent::Text(text)) => Some(text.as_str()),
        _ => None,
    }
}

fn user_text(text: &str) -> ChatMessage {
    ChatMessage {
        role: "user".to_string(),
        content: Some(MessageContent::Text(text.to_string())),
        name: None,
        tool_calls: None,
    }
}

fn rfc3339(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("fixture is rfc3339")
        .with_timezone(&Utc)
}

// ---------------------------------------------------------------------------
// Neo4jConfig
// ---------------------------------------------------------------------------

/// `Neo4jConfig::default()` is the only place the three `NEO4J_*` variables are
/// read, so this is the only test that can show they are read at all.
#[tokio::test]
#[serial_test::serial]
async fn config_default_reads_the_three_neo4j_environment_variables() {
    // SAFETY: `#[serial]` keeps every other test in this binary off the
    // environment for the duration, and the variables are restored below.
    unsafe {
        std::env::set_var("NEO4J_URI", "bolt://graph.invalid:17687");
        std::env::set_var("NEO4J_USER", "nexus-reader");
        std::env::set_var("NEO4J_PASSWORD", "from-the-environment");
    }

    let config = Neo4jConfig::default();

    assert_eq!(config.uri, "bolt://graph.invalid:17687");
    assert_eq!(config.user, "nexus-reader");
    assert_eq!(config.password, "from-the-environment");
    // `max_connections` has no environment override at all.
    assert_eq!(config.max_connections, 10);

    unsafe {
        std::env::remove_var("NEO4J_URI");
        std::env::remove_var("NEO4J_USER");
        std::env::remove_var("NEO4J_PASSWORD");
    }
}

/// With no environment at all, `Neo4jConfig::default()` does not refuse and does
/// not leave the credential empty: it substitutes the upstream placeholder
/// `"password"`. A deployment that forgets `NEO4J_PASSWORD` therefore starts up
/// and tries to authenticate with a guessable value instead of failing loudly.
#[tokio::test]
#[serial_test::serial]
async fn config_default_silently_substitutes_a_placeholder_credential() {
    // SAFETY: see `config_default_reads_the_three_neo4j_environment_variables`.
    unsafe {
        std::env::remove_var("NEO4J_URI");
        std::env::remove_var("NEO4J_USER");
        std::env::remove_var("NEO4J_PASSWORD");
    }

    let config = Neo4jConfig::default();

    assert_eq!(config.uri, "bolt://localhost:7687");
    assert_eq!(config.user, "neo4j");
    assert_eq!(
        config.password, "password",
        "a missing NEO4J_PASSWORD should be refused, not defaulted"
    );
}

// ---------------------------------------------------------------------------
// Neo4jClient
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_new_sends_the_three_uniqueness_constraints_in_order() {
    let bolt = FakeBolt::start().await;

    let client = Neo4jClient::new(config_for(&bolt)).await.expect("client");

    let cypher = bolt.cypher();
    assert_eq!(cypher.len(), 3, "one statement per constraint: {cypher:?}");
    assert!(
        cypher[0].contains("CONSTRAINT nexus_session_id"),
        "{cypher:?}"
    );
    assert!(cypher[0].contains("s:NexusSession"), "{cypher:?}");
    assert!(
        cypher[1].contains("CONSTRAINT nexus_conversation_id"),
        "{cypher:?}"
    );
    assert!(
        cypher[2].contains("CONSTRAINT nexus_message_id"),
        "{cypher:?}"
    );
    assert!(
        cypher.iter().all(|c| c.contains("IF NOT EXISTS")),
        "re-running must stay a no-op: {cypher:?}"
    );

    // `graph()` hands out the very pool those statements went through.
    bolt.forget_runs();
    client
        .graph()
        .run(neo4rs::query("RETURN 1 AS probe"))
        .await
        .expect("probe");
    assert_eq!(bolt.cypher(), vec!["RETURN 1 AS probe".to_string()]);
}

/// A URI whose scheme `neo4rs` does not know is rejected eagerly, before any
/// socket is opened — the one failure mode `Neo4jClient::new` really reports.
#[tokio::test]
async fn client_new_rejects_a_uri_whose_scheme_is_not_bolt() {
    let config = Neo4jConfig {
        uri: "https://graph.invalid:7473".to_string(),
        user: FAKE_USER.to_string(),
        password: FAKE_AUTH.to_string(),
        max_connections: 1,
    };

    // `Neo4jClient` implements neither `Debug` nor `Display`, so `expect_err`
    // is unavailable and the error has to be taken out by hand.
    let error = match Neo4jClient::new(config).await {
        Ok(_) => panic!("https is not a bolt scheme and must be rejected"),
        Err(error) => error,
    };

    assert!(
        error.to_string().contains("https"),
        "the error should name the rejected scheme: {error}"
    );
}

/// `init_schema` funnels **every** error into `debug!` and then returns
/// `Ok(())`, so `Neo4jClient::new` logs "Connected to Neo4j successfully" for a
/// server that rejected all three statements. A caller that bootstraps the
/// store has no way to tell a healthy graph from a hostile one; the first real
/// symptom is a failed `create` much later.
#[tokio::test]
async fn client_new_reports_success_even_though_every_constraint_failed() {
    let bolt = FakeBolt::start().await;
    bolt.failing("this account may not create constraints");

    let client = Neo4jClient::new(config_for(&bolt)).await;

    assert!(
        client.is_ok(),
        "init_schema swallows the failure: {:?}",
        client.err()
    );
    assert_eq!(
        bolt.cypher().len(),
        3,
        "all three were attempted and all three failed"
    );
}

/// `Neo4jConfig::max_connections` is never forwarded: `Neo4jClient::new` calls
/// `Graph::new(uri, user, password)`, which builds the pool with `neo4rs`'
/// own default. `max_connections: 0` is the proof — `deadpool` would never hand
/// out a connection for a pool of size zero, so `init_schema`'s first statement
/// would block forever if the field were honoured. It completes instead.
#[tokio::test]
async fn client_new_ignores_the_configured_max_connections() {
    let bolt = FakeBolt::start().await;
    let config = Neo4jConfig {
        max_connections: 0,
        ..config_for(&bolt)
    };

    let client = tokio::time::timeout(Duration::from_secs(10), Neo4jClient::new(config))
        .await
        .expect("a pool of size 0 would have hung here");

    assert!(client.is_ok());
    assert_eq!(bolt.cypher().len(), 3, "the schema went out regardless");
}

// ---------------------------------------------------------------------------
// Neo4jConversationStore::create
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conversation_create_writes_the_node_and_returns_the_id_it_generated() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;

    let id = store
        .create(Some("claude-opus-5".to_string()))
        .await
        .expect("create");

    let run = bolt.run_matching("CREATE (c:NexusConversation");
    assert!(run.cypher.contains("total_tokens: 0"), "{}", run.cypher);
    assert!(run.cypher.contains("turn_count: 0"), "{}", run.cypher);
    assert_eq!(run.param("id").as_str(), Some(id.as_str()));
    assert_eq!(run.param("model").as_str(), Some("claude-opus-5"));
    assert!(
        uuid::Uuid::parse_str(&id).is_ok(),
        "the id must be a uuid: {id}"
    );
    // `now` is handed over as a string and wrapped in `datetime(...)` by Cypher.
    let now = run.param("now").as_str().expect("now is a string");
    assert!(
        DateTime::parse_from_rfc3339(now).is_ok(),
        "now must be rfc3339: {now}"
    );
    assert!(run.cypher.contains("datetime($now)"), "{}", run.cypher);
}

/// `create(None)` means "this conversation has no model". The code writes
/// `model.unwrap_or_default()`, so the property becomes the **empty string**
/// rather than `null`. `get` then reads it back with `node.get("model").ok()`,
/// which succeeds on `""`, so the absence is turned into `Some("")` — see
/// [`conversation_get_cannot_distinguish_a_missing_model_from_an_empty_one`].
#[tokio::test]
async fn conversation_create_writes_an_empty_string_instead_of_null_for_no_model() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;

    store.create(None).await.expect("create");

    let run = bolt.run_matching("CREATE (c:NexusConversation");
    assert_eq!(
        run.param("model"),
        &serde_json::Value::String(String::new()),
        "None should have been sent as null"
    );
    assert!(
        !run.param("model").is_null(),
        "documenting today's behaviour, not endorsing it"
    );
}

#[tokio::test]
async fn conversation_create_propagates_a_rejected_statement() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.failing("constraint violation on c.id");

    let error = store.create(None).await.expect_err("the graph refused");

    assert!(
        error.to_string().contains("constraint violation on c.id"),
        "the server's message must survive: {error}"
    );
}

// ---------------------------------------------------------------------------
// Neo4jConversationStore::get
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conversation_get_returns_none_when_no_row_matches() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning_rows(&["c", "messages"], Vec::new());

    let found = store.get("absent").await.expect("get");

    assert!(found.is_none());
    let run = bolt.run_matching("MATCH (c:NexusConversation {id: $id})");
    assert_eq!(run.param("id").as_str(), Some("absent"));
    assert!(
        run.cypher.contains("ORDER BY m.turn_index"),
        "messages must come back in turn order: {}",
        run.cypher
    );
}

#[tokio::test]
async fn conversation_get_decodes_the_node_and_its_messages_in_order() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning_rows(
        &["c", "messages"],
        vec![vec![
            healthy_conversation_node(),
            pack_list(&[
                message_node(pack_string("user"), pack_string("salut")),
                message_node(pack_string("assistant"), pack_string("bonjour")),
            ]),
        ]],
    );

    let conversation = store.get("conv-1").await.expect("get").expect("some");

    assert_eq!(conversation.id, "conv-1", "the id comes from the argument");
    assert_eq!(conversation.created_at, rfc3339(CREATED));
    assert_eq!(conversation.updated_at, rfc3339(UPDATED));
    assert_eq!(
        conversation.metadata.model.as_deref(),
        Some("claude-opus-5")
    );
    assert_eq!(conversation.metadata.total_tokens, 1234);
    assert_eq!(conversation.metadata.turn_count, 2);
    let roles: Vec<&str> = conversation
        .messages
        .iter()
        .map(|m| m.role.as_str())
        .collect();
    assert_eq!(roles, vec!["user", "assistant"]);
    let texts: Vec<Option<&str>> = conversation.messages.iter().map(text_of).collect();
    assert_eq!(texts, vec![Some("salut"), Some("bonjour")]);
    // Neither `name` nor `tool_calls` is persisted, so they come back empty even
    // if the caller had supplied them.
    assert!(conversation.messages.iter().all(|m| m.name.is_none()));
    assert!(conversation.messages.iter().all(|m| m.tool_calls.is_none()));
    // `project_path` is hard-coded to `None` here: the conversation node has no
    // such property and `update_metadata` never writes one.
    assert!(conversation.metadata.project_path.is_none());
}

/// The round trip nothing else verified. `create` writes
/// `created_at: datetime($now)`, so a real server stores a **temporal** value
/// and sends back a Bolt `DateTime` struct (`0xB3 0x46`) — not the string that
/// `parse_neo4j_datetime`'s comment claims ("Neo4j datetime is returned as a
/// string in ISO format"). Asking `neo4rs` for a `String` nevertheless works,
/// because it renders temporal values to RFC 3339 on demand. The comment is
/// wrong about the wire format and the code is right by accident; this test pins
/// the behaviour down so that a `neo4rs` upgrade cannot break the read path of
/// everything this module writes without a test going red.
#[tokio::test]
async fn conversation_get_decodes_the_bolt_datetime_that_create_actually_writes() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning_rows(
        &["c", "messages"],
        vec![vec![
            conversation_node(&[
                ("model", pack_string("claude-opus-5")),
                ("total_tokens", pack_int(7)),
                ("turn_count", pack_int(1)),
                ("created_at", pack_datetime(1_790_000_000, 0, 0)),
                ("updated_at", pack_datetime(1_790_000_100, 0, 0)),
            ]),
            pack_list(&[]),
        ]],
    );

    let conversation = store.get("conv-1").await.expect("get").expect("some");

    assert_eq!(
        conversation.created_at,
        rfc3339("2026-09-21T14:13:20+00:00")
    );
    assert_eq!(
        conversation.updated_at,
        rfc3339("2026-09-21T14:15:00+00:00")
    );
    assert_eq!(conversation.metadata.total_tokens, 7);
}

/// A message whose `role` or `content` property is missing is dropped by
/// `filter_map` without a log line and without affecting the returned
/// `turn_count`. A caller that replays `messages` to rebuild a prompt silently
/// loses a turn, and `metadata.turn_count` still claims it is there.
#[tokio::test]
async fn conversation_get_silently_drops_messages_with_a_missing_property() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning_rows(
        &["c", "messages"],
        vec![vec![
            conversation_node(&[
                ("model", pack_string("claude-opus-5")),
                ("total_tokens", pack_int(0)),
                ("turn_count", pack_int(3)),
                ("created_at", pack_string(CREATED)),
                ("updated_at", pack_string(UPDATED)),
            ]),
            pack_list(&[
                message_node(pack_string("user"), pack_string("gardé")),
                // no `content`
                pack_node(2, &["NexusMessage"], &[("role", pack_string("assistant"))]),
                // `role` is null rather than absent
                message_node(pack_null(), pack_string("perdu aussi")),
            ]),
        ]],
    );

    let conversation = store.get("conv-1").await.expect("get").expect("some");

    assert_eq!(
        conversation.messages.len(),
        1,
        "two of the three rows vanished without a trace"
    );
    assert_eq!(text_of(&conversation.messages[0]), Some("gardé"));
    assert_eq!(
        conversation.metadata.turn_count, 3,
        "turn_count still advertises the turns that were dropped"
    );
}

/// Round trip of [`conversation_create_writes_an_empty_string_instead_of_null_for_no_model`]:
/// an empty `model` is indistinguishable from a model that was set to `""`,
/// while a true `null` does come back as `None`. Missing counters fall back to 0.
#[tokio::test]
async fn conversation_get_cannot_distinguish_a_missing_model_from_an_empty_one() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning_rows(
        &["c", "messages"],
        vec![vec![
            // what `create(None)` actually stores: "" and no counters at all
            conversation_node(&[
                ("model", pack_string("")),
                ("created_at", pack_string(CREATED)),
                ("updated_at", pack_string(UPDATED)),
            ]),
            pack_list(&[]),
        ]],
    );

    let conversation = store.get("conv-1").await.expect("get").expect("some");

    assert_eq!(
        conversation.metadata.model.as_deref(),
        Some(""),
        "`create(None)` round-trips to Some(\"\"), never back to None"
    );
    assert_eq!(conversation.metadata.total_tokens, 0, "unwrap_or(0)");
    assert_eq!(conversation.metadata.turn_count, 0, "unwrap_or(0)");
    assert!(conversation.messages.is_empty());
}

#[tokio::test]
async fn conversation_get_turns_a_null_model_into_none() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning_rows(
        &["c", "messages"],
        vec![vec![
            conversation_node(&[
                ("model", pack_null()),
                ("total_tokens", pack_int(5)),
                ("turn_count", pack_int(1)),
                ("created_at", pack_string(CREATED)),
                ("updated_at", pack_string(UPDATED)),
            ]),
            pack_list(&[]),
        ]],
    );

    let conversation = store.get("conv-1").await.expect("get").expect("some");

    assert!(conversation.metadata.model.is_none());
}

#[tokio::test]
async fn conversation_get_propagates_a_rejected_statement() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.failing("unknown label NexusConversation");

    let error = store.get("conv-1").await.expect_err("the graph refused");

    assert!(
        error
            .to_string()
            .contains("unknown label NexusConversation"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// Neo4jConversationStore::add_message
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conversation_add_message_links_the_node_and_bumps_the_counter() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning("id", vec![pack_string("conv-1")]);

    store
        .add_message("conv-1", user_text("salut"))
        .await
        .expect("add_message");

    let run = bolt.run_matching("CREATE (m:NexusMessage");
    assert_eq!(run.param("conv_id").as_str(), Some("conv-1"));
    assert_eq!(run.param("role").as_str(), Some("user"));
    assert_eq!(run.param("content").as_str(), Some("salut"));
    assert!(
        uuid::Uuid::parse_str(run.param("msg_id").as_str().expect("msg_id")).is_ok(),
        "msg_id must be a uuid: {}",
        run.param("msg_id")
    );
    assert!(
        run.cypher.contains("turn_index: c.turn_count"),
        "{}",
        run.cypher
    );
    assert!(
        run.cypher.contains("c.turn_count = c.turn_count + 1"),
        "{}",
        run.cypher
    );
    assert!(
        run.cypher.contains("CREATE (c)-[:HAS_MESSAGE]->(m)"),
        "{}",
        run.cypher
    );
}

#[tokio::test]
async fn conversation_add_message_joins_array_text_parts_with_newlines() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning("id", vec![pack_string("conv-1")]);

    store
        .add_message(
            "conv-1",
            ChatMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Array(vec![
                    ContentPart::Text {
                        text: "première".to_string(),
                    },
                    ContentPart::Text {
                        text: "seconde".to_string(),
                    },
                ])),
                name: None,
                tool_calls: None,
            },
        )
        .await
        .expect("add_message");

    let run = bolt.run_matching("CREATE (m:NexusMessage");
    assert_eq!(
        run.param("content").as_str(),
        Some("première\nseconde"),
        "the two text parts are concatenated with a newline"
    );
}

/// The `_ => None` arm of the `ContentPart` match. `ContentPart` has exactly two
/// variants, `Text` and `ImageUrl`, and both come straight from an OpenAI client
/// request, so the arm can only ever discard an **image**. An image-only message
/// is persisted with an empty body: nothing records that the turn carried an
/// attachment, nothing logs, and `get` later replays a blank user turn. A
/// placeholder (`[image: <url>]`) or a `warn!` would both be defensible; silence
/// is not.
#[tokio::test]
async fn conversation_add_message_discards_image_parts_without_a_trace() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning("id", vec![pack_string("conv-1")]);

    store
        .add_message(
            "conv-1",
            ChatMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Array(vec![ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: "data:image/png;base64,iVBORw0KGgo=".to_string(),
                        detail: Some("high".to_string()),
                    },
                }])),
                name: None,
                tool_calls: None,
            },
        )
        .await
        .expect("add_message");

    let run = bolt.run_matching("CREATE (m:NexusMessage");
    assert_eq!(
        run.param("content").as_str(),
        Some(""),
        "the image url is dropped and the message body is empty"
    );
    assert_eq!(
        run.param("role").as_str(),
        Some("user"),
        "the empty turn is still written, so it cannot be spotted downstream"
    );
}

/// A mixed array keeps the text and drops the image, which is the same loss with
/// a survivor to hide it.
#[tokio::test]
async fn conversation_add_message_keeps_text_and_drops_the_image_beside_it() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning("id", vec![pack_string("conv-1")]);

    store
        .add_message(
            "conv-1",
            ChatMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Array(vec![
                    ContentPart::Text {
                        text: "décris cette image".to_string(),
                    },
                    ContentPart::ImageUrl {
                        image_url: ImageUrl {
                            url: "https://example.invalid/cat.png".to_string(),
                            detail: None,
                        },
                    },
                ])),
                name: None,
                tool_calls: None,
            },
        )
        .await
        .expect("add_message");

    let content = bolt
        .run_matching("CREATE (m:NexusMessage")
        .param("content")
        .as_str()
        .expect("content")
        .to_string();
    assert_eq!(content, "décris cette image");
    assert!(
        !content.contains("cat.png"),
        "nothing recalls the attachment: {content}"
    );
}

#[tokio::test]
async fn conversation_add_message_stores_an_empty_body_for_a_contentless_message() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning("id", vec![pack_string("conv-1")]);

    store
        .add_message(
            "conv-1",
            ChatMessage {
                role: "assistant".to_string(),
                content: None,
                name: None,
                tool_calls: None,
            },
        )
        .await
        .expect("add_message");

    assert_eq!(
        bolt.run_matching("CREATE (m:NexusMessage")
            .param("content")
            .as_str(),
        Some("")
    );
}

/// The `MATCH` found nothing, so no row comes back and the store reports it.
/// This is the one place in the module where "no row" is treated as an error
/// rather than as an absence.
#[tokio::test]
async fn conversation_add_message_reports_a_missing_conversation() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning_nothing();

    let error = store
        .add_message("absent", user_text("salut"))
        .await
        .expect_err("no conversation to attach to");

    assert_eq!(error.to_string(), "Conversation not found: absent");
}

// ---------------------------------------------------------------------------
// Neo4jConversationStore::update_metadata
// ---------------------------------------------------------------------------

/// `ConversationMetadata` carries four fields and `update_metadata` writes three
/// of them: `project_path` is accepted and then dropped. There is no
/// `project_path` property on `:NexusConversation` at all, and `get` hard-codes
/// `None`, so a caller can set it, get `Ok(())`, and never see it again.
#[tokio::test]
async fn conversation_update_metadata_accepts_project_path_and_never_writes_it() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning("id", vec![pack_string("conv-1")]);

    store
        .update_metadata(
            "conv-1",
            ConversationMetadata {
                model: Some("claude-opus-5".to_string()),
                total_tokens: 4096,
                turn_count: 7,
                project_path: Some("/srv/nexus".to_string()),
            },
        )
        .await
        .expect("update_metadata");

    let run = bolt.run_matching("MATCH (c:NexusConversation {id: $id})");
    assert_eq!(run.param("id").as_str(), Some("conv-1"));
    assert_eq!(run.param("model").as_str(), Some("claude-opus-5"));
    assert_eq!(run.param("total_tokens").as_i64(), Some(4096));
    assert_eq!(run.param("turn_count").as_i64(), Some(7));
    assert!(
        !run.cypher.contains("project_path"),
        "project_path is silently discarded: {}",
        run.cypher
    );
    assert!(
        run.params.get("project_path").is_none(),
        "it is not even sent as a parameter: {}",
        run.params
    );
}

/// Same empty-string-for-`None` as `create`: clearing the model stores `""`.
#[tokio::test]
async fn conversation_update_metadata_writes_an_empty_string_to_clear_the_model() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning("id", vec![pack_string("conv-1")]);

    store
        .update_metadata("conv-1", ConversationMetadata::default())
        .await
        .expect("update_metadata");

    let run = bolt.run_matching("MATCH (c:NexusConversation {id: $id})");
    assert_eq!(run.param("model").as_str(), Some(""));
    assert_eq!(run.param("total_tokens").as_i64(), Some(0));
}

#[tokio::test]
async fn conversation_update_metadata_reports_a_missing_conversation() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning_nothing();

    let error = store
        .update_metadata("absent", ConversationMetadata::default())
        .await
        .expect_err("nothing to update");

    assert_eq!(error.to_string(), "Conversation not found: absent");
}

// ---------------------------------------------------------------------------
// Neo4jConversationStore::list_active
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conversation_list_active_returns_each_row_with_its_timestamp() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning_rows(
        &["id", "updated_at"],
        vec![
            vec![pack_string("conv-2"), pack_string(UPDATED)],
            vec![pack_string("conv-1"), pack_string(CREATED)],
        ],
    );

    let listed = store.list_active().await.expect("list_active");

    assert_eq!(
        listed,
        vec![
            ("conv-2".to_string(), rfc3339(UPDATED)),
            ("conv-1".to_string(), rfc3339(CREATED)),
        ]
    );
    let run = bolt.run_matching("MATCH (c:NexusConversation)");
    assert!(
        run.cypher.contains("ORDER BY c.updated_at DESC"),
        "{}",
        run.cypher
    );
    assert!(
        run.cypher.contains("LIMIT 100"),
        "the cap is silent and not a parameter: {}",
        run.cypher
    );
}

#[tokio::test]
async fn conversation_list_active_is_empty_when_there_is_nothing_to_list() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning_rows(&["id", "updated_at"], Vec::new());

    assert!(store.list_active().await.expect("list_active").is_empty());
}

/// `if let Ok(updated_at) = DateTime::parse_from_rfc3339(..)` has no `else`: a
/// row whose timestamp does not parse is dropped from the result, with no error
/// and no log. A conversation that exists in the graph simply stops appearing in
/// the active list, and the caller sees a shorter `Vec` rather than a failure.
#[tokio::test]
async fn conversation_list_active_silently_drops_a_row_it_cannot_parse() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning_rows(
        &["id", "updated_at"],
        vec![
            vec![pack_string("conv-ok"), pack_string(UPDATED)],
            vec![pack_string("conv-broken"), pack_string("hier soir")],
        ],
    );

    let listed = store.list_active().await.expect("list_active");

    assert_eq!(listed.len(), 1, "conv-broken disappeared: {listed:?}");
    assert_eq!(listed[0].0, "conv-ok");
}

/// `list_active` reads `c.updated_at` — a temporal property — straight into a
/// `String`, which works for the same reason as
/// [`conversation_get_decodes_the_bolt_datetime_that_create_actually_writes`].
#[tokio::test]
async fn conversation_list_active_accepts_the_temporal_updated_at_it_wrote() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning_rows(
        &["id", "updated_at"],
        vec![vec![
            pack_string("conv-1"),
            pack_datetime(1_790_000_000, 0, 0),
        ]],
    );

    let listed = store.list_active().await.expect("list_active");

    assert_eq!(
        listed,
        vec![("conv-1".to_string(), rfc3339("2026-09-21T14:13:20+00:00"))]
    );
}

#[tokio::test]
async fn conversation_list_active_propagates_a_rejected_statement() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.failing("the database is read only");

    let error = store.list_active().await.expect_err("the graph refused");

    assert!(
        error.to_string().contains("the database is read only"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// Neo4jConversationStore::cleanup_expired / delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn conversation_cleanup_expired_passes_the_timeout_and_returns_the_count() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning("deleted", vec![pack_int(3)]);

    let deleted = store.cleanup_expired(42).await.expect("cleanup_expired");

    assert_eq!(deleted, 3);
    let run = bolt.run_matching("DETACH DELETE c, m");
    assert_eq!(run.param("timeout").as_i64(), Some(42));
    assert!(
        run.cypher.contains("duration({minutes: $timeout})"),
        "the timeout is in minutes: {}",
        run.cypher
    );
}

#[tokio::test]
async fn conversation_cleanup_expired_returns_zero_when_nothing_was_expired() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning("deleted", vec![pack_int(0)]);

    assert_eq!(store.cleanup_expired(5).await.expect("cleanup_expired"), 0);
}

/// No row at all — the `Ok(0)` after the `if let`. A caller cannot tell this
/// apart from "zero conversations were expired".
#[tokio::test]
async fn conversation_cleanup_expired_returns_zero_when_the_query_returns_no_row() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning_nothing();

    assert_eq!(store.cleanup_expired(5).await.expect("cleanup_expired"), 0);
}

/// A negative count would be nonsense, but `deleted as usize` on `-1` wraps to
/// `usize::MAX` instead of refusing. The fake server is the only way to feed it,
/// and it shows the cast is unchecked.
#[tokio::test]
async fn conversation_cleanup_expired_wraps_a_negative_count_instead_of_refusing() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning("deleted", vec![pack_int(-1)]);

    let deleted = store.cleanup_expired(5).await.expect("cleanup_expired");

    assert_eq!(
        deleted,
        usize::MAX,
        "`deleted as usize` is an unchecked cast"
    );
}

#[tokio::test]
async fn conversation_delete_reports_true_when_a_node_was_removed() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.returning("deleted", vec![pack_int(1)]);

    assert!(store.delete("conv-1").await.expect("delete"));
    let run = bolt.run_matching("MATCH (c:NexusConversation {id: $id})");
    assert_eq!(run.param("id").as_str(), Some("conv-1"));
    assert!(run.cypher.contains("DETACH DELETE c, m"), "{}", run.cypher);
}

#[tokio::test]
async fn conversation_delete_reports_false_for_a_zero_count_and_for_no_row() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;

    bolt.returning("deleted", vec![pack_int(0)]);
    assert!(!store.delete("conv-1").await.expect("delete"));

    bolt.returning_nothing();
    assert!(!store.delete("conv-1").await.expect("delete"));
}

#[tokio::test]
async fn conversation_delete_propagates_a_rejected_statement() {
    let bolt = FakeBolt::start().await;
    let store = conversations(&bolt).await;
    bolt.failing("cannot delete node with relationships");

    let error = store.delete("conv-1").await.expect_err("the graph refused");

    assert!(
        error
            .to_string()
            .contains("cannot delete node with relationships"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// Neo4jSessionStore
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_create_writes_the_node_and_returns_the_id_it_generated() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;

    let id = store
        .create(Some("/srv/nexus".to_string()))
        .await
        .expect("create");

    let run = bolt.run_matching("CREATE (s:NexusSession");
    assert_eq!(run.param("id").as_str(), Some(id.as_str()));
    assert_eq!(run.param("project_path").as_str(), Some("/srv/nexus"));
    assert!(uuid::Uuid::parse_str(&id).is_ok(), "{id}");
    assert!(
        !run.cypher.contains("cli_session_id"),
        "the schema doc-comment promises cli_session_id for --resume, but \
         nothing writes it: {}",
        run.cypher
    );
}

/// Same defect as the conversation store: `None` becomes `""`, so a session with
/// no project path is stored as a session whose project path is the empty
/// string, and `get` reads it back as `Some("")`.
#[tokio::test]
async fn session_create_writes_an_empty_string_instead_of_null_for_no_project_path() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;

    store.create(None).await.expect("create");

    let run = bolt.run_matching("CREATE (s:NexusSession");
    assert_eq!(run.param("project_path").as_str(), Some(""));
    assert!(!run.param("project_path").is_null());
}

#[tokio::test]
async fn session_create_propagates_a_rejected_statement() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;
    bolt.failing("disk full");

    let error = store.create(None).await.expect_err("the graph refused");

    assert!(error.to_string().contains("disk full"), "{error}");
}

#[tokio::test]
async fn session_get_returns_none_when_no_row_matches() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;
    bolt.returning_nothing();

    assert!(store.get("absent").await.expect("get").is_none());
    assert_eq!(
        bolt.run_matching("MATCH (s:NexusSession {id: $id})")
            .param("id")
            .as_str(),
        Some("absent")
    );
}

#[tokio::test]
async fn session_get_decodes_the_node() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;
    bolt.returning(
        "s",
        vec![healthy_session_node("stored-id", pack_string("/srv/nexus"))],
    );

    let session = store.get("asked-id").await.expect("get").expect("some");

    assert_eq!(
        session.id, "asked-id",
        "the id comes from the argument, not from the node"
    );
    assert_eq!(session.project_path.as_deref(), Some("/srv/nexus"));
    assert_eq!(session.created_at, rfc3339(CREATED));
    assert_eq!(session.updated_at, rfc3339(UPDATED));
}

#[tokio::test]
async fn session_get_turns_a_null_project_path_into_none() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;
    bolt.returning("s", vec![healthy_session_node("s-1", pack_null())]);

    let session = store.get("s-1").await.expect("get").expect("some");

    assert!(
        session.project_path.is_none(),
        "only a true null round-trips to None, which `create` never writes"
    );
}

/// The session half of the same round trip: `create` writes `datetime($now)`,
/// `get` reads it back through `parse_neo4j_datetime`.
#[tokio::test]
async fn session_get_decodes_the_bolt_datetime_that_create_actually_writes() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;
    bolt.returning(
        "s",
        vec![session_node(&[
            ("id", pack_string("s-1")),
            ("project_path", pack_string("/srv/nexus")),
            ("created_at", pack_datetime(1_790_000_000, 0, 0)),
            ("updated_at", pack_datetime(1_790_000_100, 0, 0)),
        ])],
    );

    let session = store.get("s-1").await.expect("get").expect("some");

    assert_eq!(session.created_at, rfc3339("2026-09-21T14:13:20+00:00"));
    assert_eq!(session.updated_at, rfc3339("2026-09-21T14:15:00+00:00"));
}

#[tokio::test]
async fn session_update_touches_only_updated_at() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;
    bolt.returning("id", vec![pack_string("s-1")]);

    store.update("s-1").await.expect("update");

    let run = bolt.run_matching("MATCH (s:NexusSession {id: $id})");
    assert_eq!(run.param("id").as_str(), Some("s-1"));
    assert!(
        run.cypher.contains("SET s.updated_at = datetime($now)"),
        "{}",
        run.cypher
    );
    assert!(
        !run.cypher.contains("created_at"),
        "created_at must not be rewritten: {}",
        run.cypher
    );
    let now = run.param("now").as_str().expect("now");
    assert!(DateTime::parse_from_rfc3339(now).is_ok(), "{now}");
}

#[tokio::test]
async fn session_update_reports_a_missing_session() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;
    bolt.returning_nothing();

    let error = store.update("absent").await.expect_err("nothing to touch");

    assert_eq!(error.to_string(), "Session not found: absent");
}

/// `remove` reads first and deletes second, and returns the snapshot it read.
#[tokio::test]
async fn session_remove_returns_the_snapshot_it_read_before_deleting() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;
    bolt.returning("s", vec![healthy_session_node("s-1", pack_string("/srv"))]);

    let removed = store.remove("s-1").await.expect("remove").expect("some");

    assert_eq!(removed.id, "s-1");
    assert_eq!(removed.project_path.as_deref(), Some("/srv"));
    assert_eq!(
        bolt.count_runs_matching("DETACH DELETE s"),
        1,
        "exactly one delete: {:?}",
        bolt.cypher()
    );
}

/// When the read finds nothing, no `DELETE` is sent at all. That is also the
/// read-then-write race: between the `MATCH` and the `DETACH DELETE` the session
/// may be recreated, and the second statement then deletes a *different*
/// session than the one reported back.
#[tokio::test]
async fn session_remove_sends_no_delete_when_the_session_is_absent() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;
    bolt.returning_nothing();

    assert!(store.remove("absent").await.expect("remove").is_none());
    assert_eq!(
        bolt.count_runs_matching("DETACH DELETE"),
        0,
        "{:?}",
        bolt.cypher()
    );
}

#[tokio::test]
async fn session_remove_propagates_a_failure_from_the_delete_itself() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;
    bolt.returning("s", vec![healthy_session_node("s-1", pack_string("/srv"))]);
    bolt.failing_only("DETACH DELETE s", "the session is pinned");

    let error = store.remove("s-1").await.expect_err("the delete refused");

    assert!(
        error.to_string().contains("the session is pinned"),
        "{error}"
    );
}

#[tokio::test]
async fn session_list_returns_every_row() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;
    bolt.returning(
        "s",
        vec![
            healthy_session_node("s-2", pack_string("/srv/b")),
            healthy_session_node("s-1", pack_null()),
        ],
    );

    let listed = store.list().await.expect("list");

    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, "s-2");
    assert_eq!(listed[0].project_path.as_deref(), Some("/srv/b"));
    assert_eq!(listed[1].id, "s-1");
    assert!(listed[1].project_path.is_none());
    assert_eq!(listed[0].created_at, rfc3339(CREATED));
    let run = bolt.run_matching("MATCH (s:NexusSession)");
    assert!(
        run.cypher.contains("ORDER BY s.updated_at DESC"),
        "{}",
        run.cypher
    );
    assert!(run.cypher.contains("LIMIT 100"), "{}", run.cypher);
}

/// `list` and `get` disagree about the same decoding failure: `get` propagates
/// it, `list` does `unwrap_or_else(|_| Utc::now())`. A session whose timestamps
/// cannot be read is therefore listed as if it had just been created — the one
/// value a caller sorting or expiring by age must not be given. `list` is also
/// the path where the Bolt-`DateTime` mismatch is invisible, so the defect
/// `get` reports loudly is hidden here.
#[tokio::test]
async fn session_list_invents_a_fresh_timestamp_when_it_cannot_decode_one() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;
    let before = Utc::now();
    bolt.returning(
        "s",
        vec![session_node(&[
            ("id", pack_string("s-1")),
            ("project_path", pack_string("/srv")),
            // not RFC 3339: `DateTime::parse_from_rfc3339` refuses it
            ("created_at", pack_string("hier soir")),
            // not a string at all: `node.get::<String>` refuses it
            ("updated_at", pack_int(1_790_000_000)),
        ])],
    );

    let listed = store.list().await.expect("list");

    assert_eq!(listed.len(), 1, "the row is kept, unlike in `get`");
    assert!(
        listed[0].created_at >= before,
        "an unparsable string was replaced by now(): {}",
        listed[0].created_at
    );
    assert!(
        listed[0].updated_at >= before,
        "so was a property that is not a string at all: {}",
        listed[0].updated_at
    );
}

/// `node.get("id")?` is the one property `list` does not forgive, so a node
/// without an `id` fails the whole listing.
#[tokio::test]
async fn session_list_fails_when_a_node_has_no_id() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;
    bolt.returning(
        "s",
        vec![session_node(&[
            ("project_path", pack_string("/srv")),
            ("created_at", pack_string(CREATED)),
            ("updated_at", pack_string(UPDATED)),
        ])],
    );

    let error = store.list().await.expect_err("no id to decode");

    assert!(!error.to_string().is_empty(), "{error}");
}

#[tokio::test]
async fn session_list_propagates_a_rejected_statement() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;
    bolt.failing("the database is unavailable");

    let error = store.list().await.expect_err("the graph refused");

    assert!(
        error.to_string().contains("the database is unavailable"),
        "{error}"
    );
}

#[tokio::test]
async fn session_list_is_empty_when_there_are_no_sessions() {
    let bolt = FakeBolt::start().await;
    let store = sessions(&bolt).await;
    bolt.returning_nothing();

    assert!(store.list().await.expect("list").is_empty());
}
