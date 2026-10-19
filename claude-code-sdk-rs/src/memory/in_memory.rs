//! Memory providers that never touch the network.
//!
//! - [`InMemoryMemoryProvider`] keeps everything in process memory and
//!   answers searches with a lexical match. It is what tests of a host
//!   exercise, and what a host without a Meilisearch server can run on.
//! - [`NoopMemoryProvider`] stores nothing and finds nothing. It is what
//!   `ContextInjector::new` builds for a config with `enabled = false`.
//!
//! Both go through the same ranking pipeline as the Meilisearch provider
//! (`score_hits`, `apply_token_budget`), so a `ContextInjector` behaves the
//! same whichever one it holds — only the engine's own score differs.

use super::provider::{
    GetMessagesOptions, MemoryProvider, MemoryResult, MessageSearchHit, MessageSearchOptions,
    MessageSort, PaginatedMessages, QueryContext, ScoredMemoryResult, rank_for_context,
};
use super::{
    ConversationDocument, MemoryConfig, MessageDocument, RelevanceConfig, RelevanceScorer,
};
use async_trait::async_trait;
use std::sync::RwLock;

/// A process-local memory provider with a lexical search.
///
/// Messages and conversations are upserted on their `id`, as Meilisearch
/// does with `primaryKey = id`. A search tokenises the query on whitespace,
/// lower-cases it, and scores each message by the fraction of query tokens
/// found in its `display_content()`: a message matching none is not a hit,
/// an empty query matches every message with score `1.0` (what Meilisearch
/// does with `q = ""`).
pub struct InMemoryMemoryProvider {
    config: MemoryConfig,
    scorer: RelevanceScorer,
    messages: RwLock<Vec<MessageDocument>>,
    conversations: RwLock<Vec<ConversationDocument>>,
}

impl InMemoryMemoryProvider {
    /// Creates an empty provider. Only the ranking fields of `config` are
    /// read (`max_context_items`, `token_budget`, `min_relevance_score`); the
    /// Meilisearch fields and `enabled` are ignored.
    pub fn new(config: MemoryConfig) -> Self {
        Self {
            config,
            scorer: RelevanceScorer::new(RelevanceConfig::default()),
            messages: RwLock::new(Vec::new()),
            conversations: RwLock::new(Vec::new()),
        }
    }

    /// Snapshot of every stored message, in insertion order.
    pub fn messages(&self) -> Vec<MessageDocument> {
        self.messages
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Snapshot of every stored conversation, in insertion order.
    pub fn conversations(&self) -> Vec<ConversationDocument> {
        self.conversations
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Lexical hits for `query`, in score order (ties: newest first), before
    /// any offset or limit.
    fn lexical_hits(&self, query: &str, conversation_id: Option<&str>) -> Vec<MessageSearchHit> {
        let terms: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();

        let mut hits: Vec<MessageSearchHit> = self
            .messages()
            .into_iter()
            .filter(|m| conversation_id.is_none_or(|id| m.conversation_id == id))
            .filter_map(|document| {
                let score = lexical_score(&terms, document.display_content());
                (score > 0.0).then_some(MessageSearchHit {
                    document,
                    ranking_score: score,
                })
            })
            .collect();

        hits.sort_by(|a, b| {
            b.ranking_score
                .partial_cmp(&a.ranking_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.document.created_at.cmp(&a.document.created_at))
        });
        hits
    }
}

impl Default for InMemoryMemoryProvider {
    fn default() -> Self {
        Self::new(MemoryConfig::default())
    }
}

/// Fraction of `terms` found in `content` (case-insensitive substring);
/// `1.0` for no term at all.
fn lexical_score(terms: &[String], content: &str) -> f64 {
    if terms.is_empty() {
        return 1.0;
    }
    let haystack = content.to_lowercase();
    let matched = terms
        .iter()
        .filter(|t| haystack.contains(t.as_str()))
        .count();
    matched as f64 / terms.len() as f64
}

/// Replaces the element with the same `id`, or appends.
fn upsert<T: Clone>(store: &RwLock<Vec<T>>, item: &T, id: impl Fn(&T) -> &str) {
    let mut guard = store.write().unwrap_or_else(|e| e.into_inner());
    match guard.iter_mut().find(|existing| id(existing) == id(item)) {
        Some(existing) => *existing = item.clone(),
        None => guard.push(item.clone()),
    }
}

#[async_trait]
impl MemoryProvider for InMemoryMemoryProvider {
    async fn store_message(&self, message: &MessageDocument) -> MemoryResult<()> {
        upsert(&self.messages, message, |m| &m.id);
        Ok(())
    }

    async fn store_messages(&self, messages: &[MessageDocument]) -> MemoryResult<()> {
        for message in messages {
            upsert(&self.messages, message, |m| &m.id);
        }
        Ok(())
    }

    async fn retrieve_context(
        &self,
        context: &QueryContext,
        limit: usize,
    ) -> MemoryResult<Vec<ScoredMemoryResult>> {
        let mut hits = self.lexical_hits(&context.query, None);
        if let Some(cwd) = context.cwd.as_deref() {
            hits.retain(|h| h.document.cwd.as_deref() == Some(cwd));
        }
        // Same over-fetch as the Meilisearch provider, so the two providers
        // see the same candidates before scoring.
        hits.truncate(limit * 2);

        Ok(rank_for_context(
            &self.scorer,
            &self.config,
            hits,
            context,
            limit,
        ))
    }

    async fn update_conversation(&self, conversation: &ConversationDocument) -> MemoryResult<()> {
        upsert(&self.conversations, conversation, |c| &c.id);
        Ok(())
    }

    async fn health_check(&self) -> MemoryResult<bool> {
        Ok(true)
    }

    async fn get_conversation_messages(
        &self,
        conversation_id: &str,
        options: Option<GetMessagesOptions>,
    ) -> MemoryResult<PaginatedMessages> {
        let opts = options.unwrap_or_default();
        let limit = opts.effective_limit();
        let offset = opts.effective_offset();

        let mut all: Vec<MessageDocument> = self
            .messages()
            .into_iter()
            .filter(|m| m.conversation_id == conversation_id)
            .collect();
        let total_count = all.len();

        match opts.effective_sort() {
            MessageSort::TurnIndex => all.sort_by_key(|m| m.turn_index),
            MessageSort::CreatedAt => all.sort_by_key(|m| m.created_at),
        }
        if opts.is_newest_first() {
            all.reverse();
        }

        let messages: Vec<MessageDocument> = all.into_iter().skip(offset).take(limit).collect();
        let has_more = offset + messages.len() < total_count;

        Ok(PaginatedMessages {
            messages,
            total_count,
            has_more,
            offset,
            limit,
        })
    }

    async fn search_messages(
        &self,
        query: &str,
        options: &MessageSearchOptions,
    ) -> MemoryResult<Vec<MessageSearchHit>> {
        Ok(self
            .lexical_hits(query, options.conversation_id.as_deref())
            .into_iter()
            .skip(options.offset)
            .take(options.limit)
            .collect())
    }

    async fn count_conversation_messages(&self, conversation_id: &str) -> MemoryResult<usize> {
        Ok(self
            .messages()
            .iter()
            .filter(|m| m.conversation_id == conversation_id)
            .count())
    }

    async fn list_conversations(
        &self,
        limit: usize,
        offset: usize,
    ) -> MemoryResult<Vec<ConversationDocument>> {
        let mut all = self.conversations();
        all.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
        Ok(all.into_iter().skip(offset).take(limit).collect())
    }
}

/// A provider that stores nothing and finds nothing.
///
/// Every write succeeds without effect, every read is empty, and
/// `health_check` is `Ok(true)`: there is nothing to be unhealthy. It is what
/// a `ContextInjector` holds when memory is disabled, so the disabled path
/// never reaches the network.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopMemoryProvider;

#[async_trait]
impl MemoryProvider for NoopMemoryProvider {
    async fn store_message(&self, _message: &MessageDocument) -> MemoryResult<()> {
        Ok(())
    }

    async fn store_messages(&self, _messages: &[MessageDocument]) -> MemoryResult<()> {
        Ok(())
    }

    async fn retrieve_context(
        &self,
        _context: &QueryContext,
        _limit: usize,
    ) -> MemoryResult<Vec<ScoredMemoryResult>> {
        Ok(Vec::new())
    }

    async fn update_conversation(&self, _conversation: &ConversationDocument) -> MemoryResult<()> {
        Ok(())
    }

    async fn health_check(&self) -> MemoryResult<bool> {
        Ok(true)
    }

    async fn get_conversation_messages(
        &self,
        _conversation_id: &str,
        options: Option<GetMessagesOptions>,
    ) -> MemoryResult<PaginatedMessages> {
        let opts = options.unwrap_or_default();
        Ok(PaginatedMessages {
            messages: Vec::new(),
            total_count: 0,
            has_more: false,
            offset: opts.effective_offset(),
            limit: opts.effective_limit(),
        })
    }

    async fn search_messages(
        &self,
        _query: &str,
        _options: &MessageSearchOptions,
    ) -> MemoryResult<Vec<MessageSearchHit>> {
        Ok(Vec::new())
    }

    async fn count_conversation_messages(&self, _conversation_id: &str) -> MemoryResult<usize> {
        Ok(0)
    }

    async fn list_conversations(
        &self,
        _limit: usize,
        _offset: usize,
    ) -> MemoryResult<Vec<ConversationDocument>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_score_is_the_fraction_of_terms_found_case_insensitively() {
        let terms = vec!["jwt".to_string(), "rotation".to_string()];
        assert_eq!(lexical_score(&terms, "JWT keys"), 0.5);
        assert_eq!(lexical_score(&terms, "jwt key rotation"), 1.0);
        assert_eq!(lexical_score(&terms, "nothing here"), 0.0);
        assert_eq!(
            lexical_score(&[], "anything"),
            1.0,
            "an empty query matches all"
        );
    }

    #[test]
    fn upsert_replaces_on_id_and_appends_otherwise() {
        let store = RwLock::new(Vec::new());
        let m1 = MessageDocument::new("m1", "c", "user", "v1", 0, 1);
        upsert(&store, &m1, |m| &m.id);
        upsert(
            &store,
            &MessageDocument::new("m1", "c", "user", "v2", 0, 1),
            |m| &m.id,
        );
        upsert(
            &store,
            &MessageDocument::new("m2", "c", "user", "x", 1, 2),
            |m| &m.id,
        );

        let all = store.read().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].content, "v2", "the second write replaced the first");
        assert_eq!(all[1].id, "m2");
    }
}
