//! Short-term memory: current conversation
//!
//! Wraps ConversationStore to provide access to recent messages
//! in the current conversation.

#![allow(dead_code)] // Public API - may not be used internally

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tracing::debug;

use crate::core::storage::ConversationStore;
use crate::models::openai::MessageContent;

use super::traits::{ContextualMemoryProvider, MemoryResult, MemorySource, RelevanceScore};

/// Short-term memory backed by ConversationStore
pub struct ShortTermMemory<S: ConversationStore> {
    store: Arc<S>,
    conversation_id: Option<String>,
    scope: Option<String>,
}

impl<S: ConversationStore> ShortTermMemory<S> {
    /// Create a new short-term memory provider
    pub fn new(store: Arc<S>) -> Self {
        Self {
            store,
            conversation_id: None,
            scope: None,
        }
    }

    /// Set the current conversation ID
    pub fn with_conversation(mut self, conversation_id: String) -> Self {
        self.conversation_id = Some(conversation_id);
        self
    }

    /// Set the conversation ID
    pub fn set_conversation(&mut self, conversation_id: Option<String>) {
        self.conversation_id = conversation_id;
    }

    /// Calculate recency score based on message index
    fn recency_score(&self, message_index: usize, total_messages: usize) -> f64 {
        if total_messages == 0 {
            return 0.0;
        }
        // More recent messages get higher scores
        (message_index as f64 + 1.0) / total_messages as f64
    }

    /// Simple keyword matching for semantic score (placeholder for real semantic search)
    fn keyword_match_score(&self, query: &str, content: &str) -> f64 {
        let query_lower = query.to_lowercase();
        let query_words: Vec<&str> = query_lower.split_whitespace().collect();
        let content_lower = content.to_lowercase();

        if query_words.is_empty() {
            return 0.0;
        }

        let matches = query_words
            .iter()
            .filter(|word| content_lower.contains(*word))
            .count();

        matches as f64 / query_words.len() as f64
    }
}

#[async_trait]
impl<S: ConversationStore + 'static> ContextualMemoryProvider for ShortTermMemory<S> {
    async fn query(&self, query: &str, limit: usize) -> Result<Vec<MemoryResult>> {
        let Some(ref conv_id) = self.conversation_id else {
            return Ok(vec![]);
        };

        let Some(conversation) = self.store.get(conv_id).await? else {
            return Ok(vec![]);
        };

        let total_messages = conversation.messages.len();
        let mut results: Vec<MemoryResult> = conversation
            .messages
            .iter()
            .enumerate()
            .filter_map(|(idx, msg)| {
                let content = match &msg.content {
                    Some(MessageContent::Text(text)) => text.clone(),
                    // The non-text arm is spelled out rather than `_` so that a
                    // future `ContentPart` carrying text cannot be dropped from
                    // the search corpus without the compiler saying so. An
                    // `image_url` genuinely has nothing a keyword score can use.
                    Some(MessageContent::Array(parts)) => parts
                        .iter()
                        .filter_map(|p| match p {
                            crate::models::openai::ContentPart::Text { text } => Some(text.clone()),
                            crate::models::openai::ContentPart::ImageUrl { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                    None => return None,
                };

                let semantic = self.keyword_match_score(query, &content);
                let recency = self.recency_score(idx, total_messages);

                // Only include if there's some relevance
                if semantic < 0.1 {
                    return None;
                }

                let score = RelevanceScore::new(semantic, recency, 1.0); // Max scope for current conv

                Some(
                    MemoryResult::new(
                        format!("{}-{}", conv_id, idx),
                        MemorySource::Conversation {
                            conversation_id: conv_id.clone(),
                            message_index: idx,
                        },
                        content,
                        score,
                        conversation.updated_at,
                    )
                    .with_metadata(serde_json::json!({
                        "role": msg.role,
                        "turn_index": idx,
                    })),
                )
            })
            .collect();

        // Sort by combined score descending
        results.sort_by(|a, b| {
            b.score
                .combined
                .partial_cmp(&a.score.combined)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        results.truncate(limit);
        debug!("ShortTermMemory: found {} results for query", results.len());

        Ok(results)
    }

    async fn search_context(
        &self,
        query: &str,
        source_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryResult>> {
        // Short-term only has conversation source
        if let Some(filter) = source_filter
            && filter != "conversation"
        {
            return Ok(vec![]);
        }

        self.query(query, limit).await
    }

    async fn get_relevant_decisions(
        &self,
        _topic: &str,
        _limit: usize,
    ) -> Result<Vec<MemoryResult>> {
        // Short-term memory doesn't track decisions
        // Decisions come from medium-term (project-orchestrator)
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
    use crate::core::storage::InMemoryConversationStore;
    use crate::models::openai::ChatMessage;

    #[tokio::test]
    async fn test_short_term_query() {
        let store: Arc<InMemoryConversationStore> = Arc::new(InMemoryConversationStore::default());

        // Create conversation with messages
        let conv_id = store.create(None).await.unwrap();
        store
            .add_message(
                &conv_id,
                ChatMessage {
                    role: "user".to_string(),
                    content: Some(MessageContent::Text(
                        "How should we implement authentication?".to_string(),
                    )),
                    name: None,
                    tool_calls: None,
                },
            )
            .await
            .unwrap();
        store
            .add_message(
                &conv_id,
                ChatMessage {
                    role: "assistant".to_string(),
                    content: Some(MessageContent::Text(
                        "I recommend using JWT tokens for authentication.".to_string(),
                    )),
                    name: None,
                    tool_calls: None,
                },
            )
            .await
            .unwrap();

        let memory = ShortTermMemory::new(store).with_conversation(conv_id);

        let results = memory.query("authentication", 10).await.unwrap();
        assert!(!results.is_empty());
        assert!(
            results[0].content.contains("authentication") || results[0].content.contains("JWT")
        );
    }

    #[tokio::test]
    async fn test_short_term_no_conversation() {
        let store: Arc<InMemoryConversationStore> = Arc::new(InMemoryConversationStore::default());
        let memory = ShortTermMemory::new(store);

        let results = memory.query("anything", 10).await.unwrap();
        assert!(results.is_empty());
    }

    // =======================================================================
    // recency_score / keyword_match_score (private, and one branch that `query`
    // can never reach)
    // =======================================================================

    fn empty_memory() -> ShortTermMemory<InMemoryConversationStore> {
        ShortTermMemory::new(Arc::new(InMemoryConversationStore::default()))
    }

    /// The `total_messages == 0` guard is unreachable through `query` — with no
    /// messages the iterator never runs — so it is pinned directly. Without it
    /// the expression would be a division by zero yielding `NaN`, which the
    /// `sort_by` comparator would then silently treat as `Ordering::Equal`.
    #[test]
    fn recency_score_is_zero_for_an_empty_conversation() {
        let score = empty_memory().recency_score(0, 0);
        assert!(score.abs() < f64::EPSILON, "expected 0.0, got {score}");
    }

    #[test]
    fn recency_score_ranks_later_messages_higher() {
        let memory = empty_memory();
        assert!((memory.recency_score(0, 4) - 0.25).abs() < f64::EPSILON);
        assert!((memory.recency_score(3, 4) - 1.0).abs() < f64::EPSILON);
        assert!(memory.recency_score(1, 4) < memory.recency_score(2, 4));
    }

    #[test]
    fn keyword_match_score_is_a_case_insensitive_word_fraction() {
        let memory = empty_memory();
        assert!((memory.keyword_match_score("JWT auth", "jwt for AUTH") - 1.0).abs() < 1e-9);
        assert!((memory.keyword_match_score("jwt auth", "only jwt here") - 0.5).abs() < 1e-9);
        assert!(memory.keyword_match_score("jwt", "nothing here").abs() < f64::EPSILON);
    }

    /// A query made only of whitespace has no words, so the score is 0.0 rather
    /// than a division by zero.
    #[test]
    fn keyword_match_score_is_zero_for_a_blank_query() {
        let memory = empty_memory();
        assert!(
            memory
                .keyword_match_score("   \t\n ", "anything at all")
                .abs()
                < f64::EPSILON
        );
    }

    // =======================================================================
    // query
    // =======================================================================

    async fn store_with(messages: Vec<ChatMessage>) -> (Arc<InMemoryConversationStore>, String) {
        let store: Arc<InMemoryConversationStore> = Arc::new(InMemoryConversationStore::default());
        let conv_id = store.create(None).await.expect("in-memory create");
        for message in messages {
            store
                .add_message(&conv_id, message)
                .await
                .expect("in-memory add_message");
        }
        (store, conv_id)
    }

    fn text_message(role: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: Some(MessageContent::Text(text.to_string())),
            name: None,
            tool_calls: None,
        }
    }

    fn parts_message(role: &str, parts: Vec<crate::models::openai::ContentPart>) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: Some(MessageContent::Array(parts)),
            name: None,
            tool_calls: None,
        }
    }

    fn image_part(url: &str) -> crate::models::openai::ContentPart {
        crate::models::openai::ContentPart::ImageUrl {
            image_url: crate::models::openai::ImageUrl {
                url: url.to_string(),
                detail: None,
            },
        }
    }

    fn text_part(text: &str) -> crate::models::openai::ContentPart {
        crate::models::openai::ContentPart::Text {
            text: text.to_string(),
        }
    }

    /// A blank query matches nothing, so every message falls under the 0.1
    /// relevance floor and the answer is empty — not "everything".
    #[tokio::test]
    async fn query_with_a_blank_query_returns_nothing() {
        let (store, conv_id) = store_with(vec![text_message("user", "anything")]).await;
        let memory = ShortTermMemory::new(store).with_conversation(conv_id);

        assert!(
            memory
                .query("   ", 10)
                .await
                .expect("in-memory store")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn query_drops_messages_below_the_relevance_floor() {
        let (store, conv_id) = store_with(vec![
            text_message("user", "let us talk about authentication"),
            text_message("assistant", "the weather is nice"),
        ])
        .await;
        let memory = ShortTermMemory::new(store).with_conversation(conv_id);

        let results = memory
            .query("authentication", 10)
            .await
            .expect("in-memory store");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "let us talk about authentication");
        assert_eq!(
            results[0].source,
            MemorySource::Conversation {
                conversation_id: results[0]
                    .id
                    .rsplit_once('-')
                    .expect("id is conv-idx")
                    .0
                    .to_string(),
                message_index: 0,
            }
        );
        assert_eq!(results[0].metadata["role"], serde_json::json!("user"));
        assert_eq!(results[0].metadata["turn_index"], serde_json::json!(0));
    }

    /// A content array keeps its text parts (joined by a space) and silently
    /// drops anything else — today that is only `ContentPart::ImageUrl`, which
    /// carries no text a keyword score could use.
    #[tokio::test]
    async fn query_joins_text_parts_and_drops_image_parts() {
        let (store, conv_id) = store_with(vec![parts_message(
            "user",
            vec![
                text_part("authentication"),
                image_part("https://example.invalid/diagram.png"),
                text_part("design"),
            ],
        )])
        .await;
        let memory = ShortTermMemory::new(store).with_conversation(conv_id);

        let results = memory
            .query("authentication design", 10)
            .await
            .expect("in-memory store");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "authentication design");
        assert!((results[0].score.semantic - 1.0).abs() < 1e-9);
    }

    /// An image-only message becomes the empty string, scores 0.0 and is dropped.
    #[tokio::test]
    async fn query_skips_an_image_only_message() {
        let (store, conv_id) = store_with(vec![
            parts_message("user", vec![image_part("https://example.invalid/a.png")]),
            text_message("assistant", "authentication is handled by JWT"),
        ])
        .await;
        let memory = ShortTermMemory::new(store).with_conversation(conv_id);

        let results = memory
            .query("authentication", 10)
            .await
            .expect("in-memory store");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "authentication is handled by JWT");
    }

    /// `content: None` (a pure tool-call turn) is skipped before scoring.
    #[tokio::test]
    async fn query_skips_a_message_without_content() {
        let (store, conv_id) = store_with(vec![
            ChatMessage {
                role: "assistant".to_string(),
                content: None,
                name: None,
                tool_calls: None,
            },
            text_message("user", "authentication please"),
        ])
        .await;
        let memory = ShortTermMemory::new(store).with_conversation(conv_id);

        let results = memory
            .query("authentication", 10)
            .await
            .expect("in-memory store");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "authentication please");
    }

    /// Equal keyword scores are broken by recency, and `limit` truncates after
    /// sorting — so the most recent matches survive.
    #[tokio::test]
    async fn query_sorts_by_recency_then_truncates_to_the_limit() {
        let (store, conv_id) = store_with(vec![
            text_message("user", "authentication one"),
            text_message("assistant", "authentication two"),
            text_message("user", "authentication three"),
        ])
        .await;
        let memory = ShortTermMemory::new(store).with_conversation(conv_id);

        let results = memory
            .query("authentication", 2)
            .await
            .expect("in-memory store");
        let contents: Vec<&str> = results.iter().map(|r| r.content.as_str()).collect();
        assert_eq!(contents, vec!["authentication three", "authentication two"]);
    }

    #[tokio::test]
    async fn query_of_an_unknown_conversation_is_empty_not_an_error() {
        let store: Arc<InMemoryConversationStore> = Arc::new(InMemoryConversationStore::default());
        let memory = ShortTermMemory::new(store).with_conversation("no-such-conv".to_string());

        assert!(
            memory
                .query("authentication", 10)
                .await
                .expect("a missing conversation is not an error")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn set_conversation_switches_and_clears_the_target() {
        let (store, conv_id) = store_with(vec![text_message("user", "authentication")]).await;
        let mut memory = ShortTermMemory::new(store);

        // No conversation set: nothing to search.
        assert!(
            memory
                .query("authentication", 10)
                .await
                .expect("in-memory store")
                .is_empty()
        );

        memory.set_conversation(Some(conv_id));
        assert_eq!(
            memory
                .query("authentication", 10)
                .await
                .expect("in-memory store")
                .len(),
            1
        );

        memory.set_conversation(None);
        assert!(
            memory
                .query("authentication", 10)
                .await
                .expect("in-memory store")
                .is_empty()
        );
    }

    // =======================================================================
    // search_context / get_relevant_decisions / scope
    // =======================================================================

    #[tokio::test]
    async fn search_context_answers_only_for_the_conversation_source() {
        let (store, conv_id) = store_with(vec![text_message("user", "authentication")]).await;
        let memory = ShortTermMemory::new(store).with_conversation(conv_id);

        for filter in [None, Some("conversation")] {
            let results = memory
                .search_context("authentication", filter, 10)
                .await
                .expect("in-memory store");
            assert_eq!(results.len(), 1, "filter {filter:?} must search");
        }

        for filter in ["plan", "task", "decision", "note", "cross_conversation"] {
            let results = memory
                .search_context("authentication", Some(filter), 10)
                .await
                .expect("in-memory store");
            assert!(results.is_empty(), "filter {filter} must answer empty");
        }
    }

    #[tokio::test]
    async fn get_relevant_decisions_is_always_empty() {
        let (store, conv_id) = store_with(vec![text_message(
            "user",
            "we decided to use JWT for authentication",
        )])
        .await;
        let memory = ShortTermMemory::new(store).with_conversation(conv_id);

        assert!(
            memory
                .get_relevant_decisions("authentication", 10)
                .await
                .expect("short-term tracks no decisions")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn scope_round_trips() {
        let mut memory = empty_memory();

        assert_eq!(memory.current_scope(), None);
        memory.set_scope(Some("project-nexus".to_string()));
        assert_eq!(memory.current_scope(), Some("project-nexus".to_string()));
        memory.set_scope(None);
        assert_eq!(memory.current_scope(), None);
    }
}
