//! Long-term memory: cross-conversation search via Meilisearch
//!
//! Provides semantic search across all past conversations and
//! knowledge notes.

#![allow(dead_code)] // Public API - may not be used internally

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use std::sync::Arc;
use tracing::debug;

use crate::core::storage::meilisearch::MeilisearchClient;

use super::traits::{ContextualMemoryProvider, MemoryResult, MemorySource, RelevanceScore};

/// Long-term memory backed by Meilisearch
pub struct LongTermMemory {
    meilisearch: Arc<MeilisearchClient>,
    current_conversation_id: Option<String>,
    scope: Option<String>,
}

impl LongTermMemory {
    /// Create a new long-term memory provider
    pub fn new(meilisearch: Arc<MeilisearchClient>) -> Self {
        Self {
            meilisearch,
            current_conversation_id: None,
            scope: None,
        }
    }

    /// Exclude the current conversation from search results
    pub fn with_current_conversation(mut self, conversation_id: String) -> Self {
        self.current_conversation_id = Some(conversation_id);
        self
    }

    /// Set the current conversation
    pub fn set_current_conversation(&mut self, conversation_id: Option<String>) {
        self.current_conversation_id = conversation_id;
    }

    /// Calculate recency score from timestamp
    ///
    /// Full score within 1 hour, decaying to ~0.5 after 24 hours and towards 0
    /// as the document ages.
    ///
    /// The age is floored at zero. A timestamp in the future — clock skew
    /// between whoever indexed the message and this process, or the
    /// `unwrap_or_else(Utc::now)` fallback in [`Self::query`] — makes
    /// `1.0 + hours / 24.0` negative once it is more than 24 hours ahead, and
    /// the raw formula then scores it `0.0` after clamping: the *worst* score,
    /// for the *freshest* document, with a discontinuity at exactly -24 h. With
    /// the floor the function is monotonically non-increasing in age, which is
    /// what "score decays over time" means.
    fn recency_score(&self, timestamp: DateTime<Utc>) -> f64 {
        let now = Utc::now();
        let age = now.signed_duration_since(timestamp);

        let hours = (age.num_minutes() as f64 / 60.0).max(0.0);
        let score = 1.0 / (1.0 + hours / 24.0);

        score.clamp(0.0, 1.0)
    }
}

#[async_trait]
impl ContextualMemoryProvider for LongTermMemory {
    async fn query(&self, query: &str, limit: usize) -> Result<Vec<MemoryResult>> {
        // Search messages across all conversations
        let messages = self
            .meilisearch
            .search_messages(query, None, limit * 2) // Get more to filter
            .await?;

        let mut results: Vec<MemoryResult> = messages
            .into_iter()
            .filter(|msg| {
                // Exclude current conversation if set
                if let Some(ref current) = self.current_conversation_id {
                    return &msg.conversation_id != current;
                }
                true
            })
            .map(|msg| {
                let timestamp = Utc
                    .timestamp_opt(msg.created_at, 0)
                    .single()
                    .unwrap_or_else(Utc::now);
                let recency = self.recency_score(timestamp);

                // Meilisearch already did semantic search, so semantic score is high
                let score = RelevanceScore::new(0.8, recency, 0.5);

                MemoryResult::new(
                    msg.id.clone(),
                    MemorySource::CrossConversation {
                        conversation_id: msg.conversation_id.clone(),
                        message_id: msg.id,
                    },
                    msg.content,
                    score,
                    timestamp,
                )
                .with_metadata(serde_json::json!({
                    "role": msg.role,
                    "turn_index": msg.turn_index,
                    "conversation_id": msg.conversation_id,
                }))
            })
            .collect();

        // Sort by combined score
        results.sort_by(|a, b| {
            b.score
                .combined
                .partial_cmp(&a.score.combined)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        results.truncate(limit);
        debug!("LongTermMemory: found {} results for query", results.len());

        Ok(results)
    }

    async fn search_context(
        &self,
        query: &str,
        source_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryResult>> {
        match source_filter {
            Some("conversation") | Some("cross_conversation") => self.query(query, limit).await,
            Some("note") | Some("knowledge_note") => {
                // For now, notes come from medium-term (project-orchestrator)
                // This could be extended to search Meilisearch for notes
                Ok(vec![])
            },
            _ => self.query(query, limit).await,
        }
    }

    async fn get_relevant_decisions(
        &self,
        _topic: &str,
        _limit: usize,
    ) -> Result<Vec<MemoryResult>> {
        // Decisions are stored in project-orchestrator (medium-term)
        // Long-term only has conversation messages
        Ok(vec![])
    }

    fn current_scope(&self) -> Option<String> {
        self.scope.clone()
    }

    fn set_scope(&mut self, scope: Option<String>) {
        self.scope = scope;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::storage::meilisearch::MeilisearchConfig;
    use chrono::Duration;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn task_info() -> Value {
        json!({
            "taskUid": 1,
            "indexUid": "nexus_messages",
            "status": "enqueued",
            "type": "settingsUpdate",
            "details": null,
            "enqueuedAt": "2026-01-01T00:00:00Z",
        })
    }

    /// One `MessageDocument`-shaped Meilisearch hit.
    fn hit(id: &str, conversation_id: &str, created_at: i64) -> Value {
        json!({
            "id": id,
            "conversation_id": conversation_id,
            "role": "assistant",
            "content": format!("content of {id}"),
            "turn_index": 3,
            "created_at": created_at,
        })
    }

    /// A Meilisearch answering `MeilisearchClient::new` and every message search
    /// with `hits`. `LongTermMemory` only ever calls `search_messages`, so the
    /// conversations index is deliberately absent.
    async fn meili_with_hits(hits: Vec<Value>) -> MockServer {
        let server = MockServer::start().await;
        let total = hits.len();

        Mock::given(method("POST"))
            .and(path("/indexes"))
            .respond_with(ResponseTemplate::new(202).set_body_json(task_info()))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path_regex(r"^/indexes/[^/]+/settings$"))
            .respond_with(ResponseTemplate::new(202).set_body_json(task_info()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/indexes/nexus_messages/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "hits": hits,
                "offset": 0,
                "limit": 20,
                "estimatedTotalHits": total,
                "processingTimeMs": 1,
                "query": "",
            })))
            .mount(&server)
            .await;

        server
    }

    /// A Meilisearch that rejects everything — including the handshake, hence the
    /// separate constructor: the client has to be built against a healthy server
    /// first and then pointed at a broken one, which we cannot do. So this one
    /// lets `new` through and only fails the search.
    async fn meili_failing_search(status: u16) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/indexes"))
            .respond_with(ResponseTemplate::new(202).set_body_json(task_info()))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path_regex(r"^/indexes/[^/]+/settings$"))
            .respond_with(ResponseTemplate::new(202).set_body_json(task_info()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/indexes/nexus_messages/search"))
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

    async fn memory_for(server: &MockServer) -> LongTermMemory {
        let client = MeilisearchClient::new(MeilisearchConfig {
            url: server.uri(),
            api_key: Some("test-key".to_string()),
        })
        .await
        .expect("the mock answered the index handshake");
        LongTermMemory::new(Arc::new(client))
    }

    /// The `limit` every `search_messages` request carried, in order.
    async fn search_limits(server: &MockServer) -> Vec<u64> {
        server
            .received_requests()
            .await
            .expect("the mock server records requests")
            .iter()
            .filter(|r| r.url.path() == "/indexes/nexus_messages/search")
            .map(|r| {
                serde_json::from_slice::<Value>(&r.body).expect("search bodies are JSON")["limit"]
                    .as_u64()
                    .expect("search bodies carry a numeric limit")
            })
            .collect()
    }

    // =======================================================================
    // recency_score
    // =======================================================================

    /// The doc-comment promises "full score within 1 hour, decays to ~0.5 after
    /// 24 hours". This calls the function instead of restating its formula.
    #[tokio::test]
    async fn recency_score_decays_with_age() {
        let server = meili_with_hits(vec![]).await;
        let memory = memory_for(&server).await;
        let now = Utc::now();

        assert!((memory.recency_score(now) - 1.0).abs() < 0.001);
        assert!((memory.recency_score(now - Duration::minutes(5)) - 0.9965).abs() < 0.001);
        assert!((memory.recency_score(now - Duration::hours(24)) - 0.5).abs() < 0.001);
        assert!((memory.recency_score(now - Duration::hours(48)) - 1.0 / 3.0).abs() < 0.001);
    }

    /// A timestamp in the future is not "less recent than everything else".
    ///
    /// `1.0 / (1.0 + hours / 24.0)` goes negative as soon as `hours < -24`, and
    /// `clamp` then turns it into `0.0` — the worst possible score. Clock skew
    /// between the indexer and the gateway, or the `unwrap_or_else(Utc::now)`
    /// fallback below, is enough to produce such a timestamp. `recency_score`
    /// now floors the age at zero, so a future document is simply "fresh".
    #[tokio::test]
    async fn recency_score_treats_a_future_timestamp_as_fresh() {
        let server = meili_with_hits(vec![]).await;
        let memory = memory_for(&server).await;
        let now = Utc::now();

        // The pathological region: 25h ahead used to score 0.0, 23h ahead 1.0.
        assert!((memory.recency_score(now + Duration::hours(48)) - 1.0).abs() < 0.001);
        assert!((memory.recency_score(now + Duration::hours(25)) - 1.0).abs() < 0.001);
        assert!((memory.recency_score(now + Duration::hours(23)) - 1.0).abs() < 0.001);

        // And the score is monotonically non-increasing in age across the seam.
        let ages = [-48i64, -25, -23, -1, 0, 1, 24, 48];
        let scores: Vec<f64> = ages
            .iter()
            .map(|h| memory.recency_score(now - Duration::hours(*h)))
            .collect();
        for pair in scores.windows(2) {
            assert!(
                pair[0] >= pair[1],
                "recency must not rise with age: {pair:?}"
            );
        }
    }

    // =======================================================================
    // query
    // =======================================================================

    #[tokio::test]
    async fn query_maps_hits_to_cross_conversation_results_ranked_by_recency() {
        let fresh = Utc::now().timestamp();
        let stale = fresh - 30 * 24 * 3600;
        let server = meili_with_hits(vec![
            hit("old", "conv-a", stale),
            hit("new", "conv-b", fresh),
        ])
        .await;
        let memory = memory_for(&server).await;

        let results = memory
            .query("auth", 10)
            .await
            .expect("meilisearch answered");

        // Semantic (0.8) and scope (0.5) are constants here, so recency decides.
        let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["new", "old"]);

        let top = &results[0];
        assert_eq!(
            top.source,
            MemorySource::CrossConversation {
                conversation_id: "conv-b".to_string(),
                message_id: "new".to_string(),
            }
        );
        assert_eq!(top.content, "content of new");
        assert_eq!(top.title, None);
        assert!((top.score.semantic - 0.8).abs() < f64::EPSILON);
        assert!((top.score.scope - 0.5).abs() < f64::EPSILON);
        assert!((top.score.recency - 1.0).abs() < 0.001);
        // 0.8 * 0.5 + 1.0 * 0.3 + 0.5 * 0.2
        assert!((top.score.combined - 0.8).abs() < 0.001);
        assert_eq!(top.metadata["role"], json!("assistant"));
        assert_eq!(top.metadata["turn_index"], json!(3));
        assert_eq!(top.metadata["conversation_id"], json!("conv-b"));

        // The stale hit is ~30 days old: 1 / (1 + 720/24) = 0.0323.
        assert!(results[1].score.recency < 0.05);
    }

    #[tokio::test]
    async fn query_excludes_the_current_conversation() {
        let now = Utc::now().timestamp();
        let server = meili_with_hits(vec![
            hit("mine", "conv-self", now),
            hit("theirs", "conv-other", now),
        ])
        .await;
        let memory = memory_for(&server)
            .await
            .with_current_conversation("conv-self".to_string());

        let results = memory
            .query("auth", 10)
            .await
            .expect("meilisearch answered");
        let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["theirs"]);
    }

    /// `set_current_conversation(None)` must put the excluded hit back.
    #[tokio::test]
    async fn set_current_conversation_clears_the_exclusion() {
        let now = Utc::now().timestamp();
        let server = meili_with_hits(vec![hit("mine", "conv-self", now)]).await;
        let mut memory = memory_for(&server).await;

        memory.set_current_conversation(Some("conv-self".to_string()));
        assert!(
            memory
                .query("auth", 10)
                .await
                .expect("meilisearch answered")
                .is_empty()
        );

        memory.set_current_conversation(None);
        assert_eq!(
            memory
                .query("auth", 10)
                .await
                .expect("meilisearch answered")
                .len(),
            1
        );
    }

    /// `search_messages(query, None, limit * 2)` over-fetches so the current
    /// conversation can be filtered out without shrinking the page, then
    /// `truncate(limit)` cuts back.
    #[tokio::test]
    async fn query_over_fetches_twice_the_limit_then_truncates() {
        let now = Utc::now().timestamp();
        let hits: Vec<Value> = (0..5)
            .map(|i| hit(&format!("m{i}"), "conv-a", now - i))
            .collect();
        let server = meili_with_hits(hits).await;
        let memory = memory_for(&server).await;

        let results = memory.query("auth", 2).await.expect("meilisearch answered");
        assert_eq!(results.len(), 2);
        assert_eq!(search_limits(&server).await, vec![4]);
    }

    /// `limit = 0` still hits the network — `0 * 2 == 0` — and returns nothing.
    #[tokio::test]
    async fn query_with_a_zero_limit_returns_nothing() {
        let server = meili_with_hits(vec![hit("m0", "conv-a", 0)]).await;
        let memory = memory_for(&server).await;

        assert!(
            memory
                .query("auth", 0)
                .await
                .expect("meilisearch answered")
                .is_empty()
        );
        assert_eq!(search_limits(&server).await, vec![0]);
    }

    #[tokio::test]
    async fn query_propagates_a_meilisearch_failure() {
        let server = meili_failing_search(500).await;
        let memory = memory_for(&server).await;

        let error = memory
            .query("auth", 10)
            .await
            .expect_err("a 500 from Meilisearch must not be swallowed");
        assert!(
            error.to_string().contains("injected meilisearch failure"),
            "unexpected error: {error}"
        );
    }

    /// A `created_at` that `timestamp_opt` cannot represent falls back to
    /// `Utc::now()`, which hands the corrupt document the *best* recency score
    /// and therefore the top of the ranking. Pinned, not endorsed: the fallback
    /// is in `query`, where returning no timestamp at all is not an option, but
    /// a caller reading `timestamp` gets a date the message never had.
    #[tokio::test]
    async fn query_dates_an_unrepresentable_timestamp_to_now_and_ranks_it_first() {
        let legit = Utc::now().timestamp() - 3600;
        let server = meili_with_hits(vec![
            hit("legit", "conv-a", legit),
            hit("corrupt", "conv-b", i64::MAX),
        ])
        .await;
        let memory = memory_for(&server).await;

        let results = memory
            .query("auth", 10)
            .await
            .expect("meilisearch answered");
        let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["corrupt", "legit"]);
        assert!((Utc::now() - results[0].timestamp).num_seconds().abs() < 5);
    }

    // =======================================================================
    // search_context / get_relevant_decisions / scope
    // =======================================================================

    /// Every arm of the `source_filter` match, including the `_` fallback.
    #[tokio::test]
    async fn search_context_routes_note_filters_to_an_empty_answer() {
        let server = meili_with_hits(vec![hit("m0", "conv-a", Utc::now().timestamp())]).await;
        let memory = memory_for(&server).await;

        for filter in ["conversation", "cross_conversation"] {
            let results = memory
                .search_context("auth", Some(filter), 10)
                .await
                .expect("meilisearch answered");
            assert_eq!(results.len(), 1, "filter {filter} must reach Meilisearch");
        }

        // Notes live in medium-term, so long-term answers without a round trip.
        for filter in ["note", "knowledge_note"] {
            let results = memory
                .search_context("auth", Some(filter), 10)
                .await
                .expect("no network needed");
            assert!(results.is_empty(), "filter {filter} must answer empty");
        }

        // Unknown filter and no filter both fall through to `query`.
        for filter in [None, Some("plan")] {
            let results = memory
                .search_context("auth", filter, 10)
                .await
                .expect("meilisearch answered");
            assert_eq!(results.len(), 1, "filter {filter:?} must reach Meilisearch");
        }

        // Two filters answered locally, four went to the network.
        assert_eq!(search_limits(&server).await.len(), 4);
    }

    #[tokio::test]
    async fn get_relevant_decisions_never_calls_meilisearch() {
        let server = meili_with_hits(vec![hit("m0", "conv-a", 0)]).await;
        let memory = memory_for(&server).await;

        let results = memory
            .get_relevant_decisions("auth", 10)
            .await
            .expect("no network needed");
        assert!(results.is_empty());
        assert!(search_limits(&server).await.is_empty());
    }

    #[tokio::test]
    async fn scope_round_trips_without_reaching_the_levels() {
        let server = meili_with_hits(vec![]).await;
        let mut memory = memory_for(&server).await;

        assert_eq!(memory.current_scope(), None);
        memory.set_scope(Some("project-nexus".to_string()));
        assert_eq!(memory.current_scope(), Some("project-nexus".to_string()));
        memory.set_scope(None);
        assert_eq!(memory.current_scope(), None);
    }
}
