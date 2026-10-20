//! Memory provider implementations for persistent storage.
//!
//! This module defines the [`MemoryProvider`] trait and its Meilisearch
//! implementation, which handles:
//! - Storing and retrieving messages
//! - Applying multi-factor relevance scoring
//! - Managing index settings
//! - Retrieving conversation history with pagination
//! - Raw full-text search with the engine's ranking score
//!
//! Two other implementations live next to it: [`InMemoryMemoryProvider`]
//! (process-local, for tests and hosts without a Meilisearch server) and
//! [`NoopMemoryProvider`] (what a disabled config yields).
//!
//! [`InMemoryMemoryProvider`]: super::InMemoryMemoryProvider
//! [`NoopMemoryProvider`]: super::NoopMemoryProvider

use super::{
    ConversationDocument, MemoryConfig, MessageDocument, RelevanceConfig, RelevanceScore,
    RelevanceScorer,
};
use async_trait::async_trait;
use chrono::Utc;
use meilisearch_sdk::client::Client;
use meilisearch_sdk::settings::Settings;
use serde::{Deserialize, Serialize};

/// Result type for memory operations.
pub type MemoryResult<T> = Result<T, MemoryError>;

/// Errors that can occur during memory operations.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    /// Meilisearch client error
    #[error("Meilisearch error: {0}")]
    Meilisearch(String),

    /// Serialization error
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Configuration error
    #[error("Configuration error: {0}")]
    Config(String),

    /// Memory is disabled
    #[error("Memory is disabled")]
    Disabled,
}

impl From<meilisearch_sdk::errors::Error> for MemoryError {
    fn from(e: meilisearch_sdk::errors::Error) -> Self {
        MemoryError::Meilisearch(e.to_string())
    }
}

/// Context for querying memory.
#[derive(Debug, Clone, Default)]
pub struct QueryContext {
    /// The search query text
    pub query: String,

    /// Current working directory
    pub cwd: Option<String>,

    /// Files in the current context
    pub files: Vec<String>,
}

impl QueryContext {
    /// Creates a new QueryContext with a query.
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            ..Default::default()
        }
    }

    /// Sets the working directory.
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Sets the files in context.
    pub fn with_files(mut self, files: Vec<String>) -> Self {
        self.files = files;
        self
    }
}

/// A memory result with relevance scoring.
#[derive(Debug, Clone)]
pub struct ScoredMemoryResult {
    /// The message document
    pub document: MessageDocument,

    /// The relevance score breakdown
    pub score: RelevanceScore,
}

/// The field a conversation read is ordered on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MessageSort {
    /// Order on `turn_index` (default). One user and one assistant message
    /// share a turn, so ties are resolved by the engine.
    #[default]
    TurnIndex,
    /// Order on `created_at`, the Unix timestamp of the message. What a host
    /// replaying a legacy session by wall-clock wants.
    CreatedAt,
}

/// Options for retrieving conversation messages with pagination.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetMessagesOptions {
    /// Maximum number of messages to return (default: 50)
    pub limit: Option<usize>,

    /// Number of messages to skip for pagination (default: 0)
    pub offset: Option<usize>,

    /// If true, return newest messages first (descending sort key).
    /// Default: true (newest first, like a chat UI)
    pub newest_first: Option<bool>,

    /// The field the page is sorted on (default: `turn_index`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_by: Option<MessageSort>,
}

impl GetMessagesOptions {
    /// Creates new options with defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the maximum number of messages to return.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Sets the offset for pagination.
    pub fn offset(mut self, offset: usize) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Sets the sort order. True = newest first (default).
    pub fn newest_first(mut self, newest: bool) -> Self {
        self.newest_first = Some(newest);
        self
    }

    /// Sets the field the page is sorted on (default: `turn_index`).
    pub fn sort_by(mut self, sort: MessageSort) -> Self {
        self.sort_by = Some(sort);
        self
    }

    /// Returns the effective limit (default: 50).
    pub fn effective_limit(&self) -> usize {
        self.limit.unwrap_or(50)
    }

    /// Returns the effective offset (default: 0).
    pub fn effective_offset(&self) -> usize {
        self.offset.unwrap_or(0)
    }

    /// Returns whether to sort newest first (default: true).
    pub fn is_newest_first(&self) -> bool {
        self.newest_first.unwrap_or(true)
    }

    /// Returns the effective sort key (default: `turn_index`).
    pub fn effective_sort(&self) -> MessageSort {
        self.sort_by.unwrap_or_default()
    }
}

/// Options for a raw full-text search over messages.
///
/// Unlike [`QueryContext`] + `retrieve_context`, nothing here is scored,
/// budgeted or thresholded: the hits come back as the engine ranked them,
/// which is what a search UI wants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSearchOptions {
    /// Maximum number of hits to return (default: 20).
    pub limit: usize,

    /// Number of hits to skip (default: 0).
    pub offset: usize,

    /// Restricts the search to one conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
}

impl Default for MessageSearchOptions {
    fn default() -> Self {
        Self {
            limit: 20,
            offset: 0,
            conversation_id: None,
        }
    }
}

impl MessageSearchOptions {
    /// Creates options with the defaults (20 hits, no filter).
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the maximum number of hits.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Sets the number of hits to skip.
    pub fn offset(mut self, offset: usize) -> Self {
        self.offset = offset;
        self
    }

    /// Restricts the search to one conversation.
    pub fn conversation_id(mut self, id: impl Into<String>) -> Self {
        self.conversation_id = Some(id.into());
        self
    }
}

/// One hit of a raw message search, with the score the engine assigned.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageSearchHit {
    /// The message document.
    pub document: MessageDocument,

    /// The engine's ranking score, normally in `0.0..=1.0`; `0.0` when the
    /// engine reported none.
    pub ranking_score: f64,
}

/// The hits of a raw message search that belong to one conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationSearchGroup {
    /// The conversation the hits belong to.
    pub conversation_id: String,

    /// The hits, in the order the engine ranked them.
    pub hits: Vec<MessageSearchHit>,

    /// The highest `ranking_score` among `hits`.
    pub best_score: f64,
}

impl ConversationSearchGroup {
    /// Groups search hits by conversation.
    ///
    /// Groups come back ordered by `best_score` descending (stable, so two
    /// conversations with the same best score keep the order of their first
    /// hit); within a group the hits keep the engine's order.
    pub fn group(hits: Vec<MessageSearchHit>) -> Vec<Self> {
        let mut groups: Vec<Self> = Vec::new();
        for hit in hits {
            match groups
                .iter_mut()
                .find(|g| g.conversation_id == hit.document.conversation_id)
            {
                Some(group) => {
                    group.best_score = group.best_score.max(hit.ranking_score);
                    group.hits.push(hit);
                },
                None => groups.push(Self {
                    conversation_id: hit.document.conversation_id.clone(),
                    best_score: hit.ranking_score,
                    hits: vec![hit],
                }),
            }
        }
        groups.sort_by(|a, b| {
            b.best_score
                .partial_cmp(&a.best_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        groups
    }
}

/// Result of fetching paginated messages.
#[derive(Debug, Clone)]
pub struct PaginatedMessages {
    /// The messages in this page
    pub messages: Vec<MessageDocument>,

    /// Total number of messages in the conversation
    pub total_count: usize,

    /// Whether there are more messages after this page
    pub has_more: bool,

    /// Current offset used for this query
    pub offset: usize,

    /// Limit used for this query
    pub limit: usize,
}

/// Trait for memory providers.
///
/// Object-safe: `ContextInjector` holds an `Arc<dyn MemoryProvider>`, so a
/// host may hand it any implementation — the Meilisearch one, the in-memory
/// one, or its own.
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    /// Stores a message in memory.
    async fn store_message(&self, message: &MessageDocument) -> MemoryResult<()>;

    /// Stores multiple messages in memory.
    async fn store_messages(&self, messages: &[MessageDocument]) -> MemoryResult<()>;

    /// Retrieves relevant context for a query.
    async fn retrieve_context(
        &self,
        context: &QueryContext,
        limit: usize,
    ) -> MemoryResult<Vec<ScoredMemoryResult>>;

    /// Updates a conversation document.
    async fn update_conversation(&self, conversation: &ConversationDocument) -> MemoryResult<()>;

    /// Checks if memory is healthy.
    async fn health_check(&self) -> MemoryResult<bool>;

    /// Retrieves all messages for a conversation with pagination.
    ///
    /// By default, messages are returned newest first (descending turn_index),
    /// which is suitable for chat UI display. `GetMessagesOptions::sort_by`
    /// switches the sort key to `created_at`, and `newest_first(false)` flips
    /// it ascending: `sort_by(MessageSort::CreatedAt).newest_first(false)` is
    /// the chronological read of one conversation.
    ///
    /// # Arguments
    ///
    /// * `conversation_id` - The ID of the conversation
    /// * `options` - Optional pagination and sorting options
    ///
    /// # Returns
    ///
    /// A paginated result containing messages and metadata.
    async fn get_conversation_messages(
        &self,
        conversation_id: &str,
        options: Option<GetMessagesOptions>,
    ) -> MemoryResult<PaginatedMessages>;

    /// Full-text search over messages, with the engine's ranking score.
    ///
    /// Nothing is scored, thresholded or budgeted on top: the hits come back
    /// in the engine's order with its score, which is what a search UI wants
    /// (`retrieve_context` is the prompt-injection path). Group them per
    /// conversation with [`ConversationSearchGroup::group`].
    async fn search_messages(
        &self,
        query: &str,
        options: &MessageSearchOptions,
    ) -> MemoryResult<Vec<MessageSearchHit>>;

    /// Returns the total count of messages in a conversation.
    ///
    /// Useful for displaying pagination info (e.g., "showing 1-50 of 142").
    async fn count_conversation_messages(&self, conversation_id: &str) -> MemoryResult<usize>;

    /// Lists all conversations with basic metadata.
    ///
    /// Returns conversations sorted by `updated_at` descending (most recent first).
    ///
    /// # Arguments
    ///
    /// * `limit` - Maximum number of conversations to return
    /// * `offset` - Number of conversations to skip for pagination
    async fn list_conversations(
        &self,
        limit: usize,
        offset: usize,
    ) -> MemoryResult<Vec<ConversationDocument>>;
}

/// Meilisearch-based memory provider.
pub struct MeilisearchMemoryProvider {
    client: Client,
    config: MemoryConfig,
    scorer: RelevanceScorer,
}

impl MeilisearchMemoryProvider {
    /// Creates a new MeilisearchMemoryProvider.
    pub async fn new(config: MemoryConfig) -> MemoryResult<Self> {
        if !config.enabled {
            return Err(MemoryError::Disabled);
        }

        let client = Client::new(&config.meilisearch_url, config.meilisearch_key.as_deref())
            .map_err(|e| MemoryError::Meilisearch(e.to_string()))?;

        let scorer = RelevanceScorer::new(RelevanceConfig::default());

        let provider = Self {
            client,
            config,
            scorer,
        };

        // Setup indexes
        provider.setup_indexes().await?;

        Ok(provider)
    }

    /// Sets up Meilisearch indexes with proper settings.
    async fn setup_indexes(&self) -> MemoryResult<()> {
        // Messages index
        let _ = self
            .client
            .create_index(&self.config.messages_index, Some("id"))
            .await;

        let messages_index = self.client.index(&self.config.messages_index);
        let messages_settings = Settings::new()
            .with_searchable_attributes(["content", "summary", "role"])
            .with_filterable_attributes(["conversation_id", "role", "cwd", "created_at"])
            .with_sortable_attributes(["created_at", "turn_index"]);

        messages_index.set_settings(&messages_settings).await?;

        // Conversations index
        let _ = self
            .client
            .create_index(&self.config.conversations_index, Some("id"))
            .await;

        let conversations_index = self.client.index(&self.config.conversations_index);
        let conversations_settings = Settings::new()
            .with_searchable_attributes(["content_preview", "model"])
            .with_filterable_attributes(["model", "cwd", "created_at", "updated_at"])
            .with_sortable_attributes(["created_at", "updated_at", "message_count"]);

        conversations_index
            .set_settings(&conversations_settings)
            .await?;

        Ok(())
    }

    /// Builds a filter string for Meilisearch based on the query context.
    fn build_filter(&self, context: &QueryContext) -> Option<String> {
        let mut filters = Vec::new();

        // Filter by cwd if provided (exact match)
        if let Some(ref cwd) = context.cwd {
            filters.push(filter_eq("cwd", cwd));
        }

        if filters.is_empty() {
            None
        } else {
            Some(filters.join(" AND "))
        }
    }
}

/// `field = "value"` for a Meilisearch filter, with the value escaped.
///
/// Meilisearch string literals escape `\` and `"` with a backslash. Without
/// this a conversation id (or a cwd) containing a double quote closed the
/// literal and the rest of it became filter syntax.
pub(super) fn filter_eq(field: &str, value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("{field} = \"{escaped}\"")
}

/// Computes the age in hours of a message created at `created_at`.
fn compute_age_hours(created_at: i64) -> f64 {
    let now = Utc::now().timestamp();
    let age_seconds = (now - created_at).max(0) as f64;
    age_seconds / 3600.0
}

/// Applies multi-factor relevance scoring to raw hits, sorts them by total
/// score descending and drops those under `config.min_relevance_score`.
///
/// Shared by every provider that feeds `retrieve_context`: the ranking is a
/// property of the memory domain, not of the engine the hits came from.
pub(super) fn score_hits(
    scorer: &RelevanceScorer,
    config: &MemoryConfig,
    hits: Vec<MessageSearchHit>,
    context: &QueryContext,
) -> Vec<ScoredMemoryResult> {
    let mut results: Vec<ScoredMemoryResult> = hits
        .into_iter()
        .map(|hit| {
            let age_hours = compute_age_hours(hit.document.created_at);

            let score = scorer.compute_score(
                hit.ranking_score,
                context.cwd.as_deref(),
                hit.document.cwd.as_deref(),
                &context.files,
                &hit.document.files_touched,
                age_hours,
            );

            ScoredMemoryResult {
                document: hit.document,
                score,
            }
        })
        .collect();

    // Sort by total score descending
    results.sort_by(|a, b| {
        b.score
            .total
            .partial_cmp(&a.score.total)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Filter by minimum relevance score
    results.retain(|r| r.score.total >= config.min_relevance_score);

    results
}

/// Keeps the best-ranked results that fit in `config.token_budget`
/// (~4 characters per token, measured on `display_content()`).
///
/// A result that does not fit is skipped and the loop goes on, so a
/// lower-ranked but shorter result may still get in; when nothing has been
/// kept yet the oversized result is kept anyway, so the prefix is never empty
/// just because the best hit is long.
pub(super) fn apply_token_budget(
    config: &MemoryConfig,
    results: Vec<ScoredMemoryResult>,
) -> Vec<ScoredMemoryResult> {
    let mut budget_remaining = config.token_budget * 4; // ~4 chars per token
    let mut selected = Vec::new();

    for result in results {
        let content_len = result.document.display_content().len();
        if content_len <= budget_remaining {
            budget_remaining -= content_len;
            selected.push(result);
        } else if selected.is_empty() {
            // Always include at least one result even if over budget
            selected.push(result);
            break;
        }
    }

    selected
}

/// The whole `retrieve_context` pipeline past the engine: score, threshold,
/// cap to `max_context_items.min(limit)`, then fit in the token budget.
pub(super) fn rank_for_context(
    scorer: &RelevanceScorer,
    config: &MemoryConfig,
    hits: Vec<MessageSearchHit>,
    context: &QueryContext,
    limit: usize,
) -> Vec<ScoredMemoryResult> {
    let scored = score_hits(scorer, config, hits, context);
    let limited: Vec<_> = scored
        .into_iter()
        .take(config.max_context_items.min(limit))
        .collect();
    apply_token_budget(config, limited)
}

#[async_trait]
impl MemoryProvider for MeilisearchMemoryProvider {
    async fn store_message(&self, message: &MessageDocument) -> MemoryResult<()> {
        let index = self.client.index(&self.config.messages_index);

        index.add_documents(&[message], Some("id")).await?;

        Ok(())
    }

    async fn store_messages(&self, messages: &[MessageDocument]) -> MemoryResult<()> {
        if messages.is_empty() {
            return Ok(());
        }

        let index = self.client.index(&self.config.messages_index);

        index.add_documents(messages, Some("id")).await?;

        Ok(())
    }

    async fn retrieve_context(
        &self,
        context: &QueryContext,
        limit: usize,
    ) -> MemoryResult<Vec<ScoredMemoryResult>> {
        let index = self.client.index(&self.config.messages_index);

        // Build filter first so it lives long enough
        let filter = self.build_filter(context);

        // Build search query
        let mut search = index.search();
        search.with_query(&context.query);
        search.with_limit(limit * 2); // Get more than needed for post-filtering
        search.with_show_ranking_score(true);

        // Apply filters
        if let Some(ref f) = filter {
            search.with_filter(f);
        }

        // Execute search
        let results: meilisearch_sdk::search::SearchResults<MessageDocument> =
            search.execute().await?;

        let hits: Vec<MessageSearchHit> = results
            .hits
            .into_iter()
            .map(|h| MessageSearchHit {
                document: h.result,
                ranking_score: h.ranking_score.unwrap_or(0.0),
            })
            .collect();

        Ok(rank_for_context(
            &self.scorer,
            &self.config,
            hits,
            context,
            limit,
        ))
    }

    async fn update_conversation(&self, conversation: &ConversationDocument) -> MemoryResult<()> {
        let index = self.client.index(&self.config.conversations_index);

        index.add_documents(&[conversation], Some("id")).await?;

        Ok(())
    }

    async fn health_check(&self) -> MemoryResult<bool> {
        match self.client.health().await {
            Ok(_) => Ok(true),
            Err(e) => Err(MemoryError::Meilisearch(e.to_string())),
        }
    }

    async fn get_conversation_messages(
        &self,
        conversation_id: &str,
        options: Option<GetMessagesOptions>,
    ) -> MemoryResult<PaginatedMessages> {
        let opts = options.unwrap_or_default();
        let limit = opts.effective_limit();
        let offset = opts.effective_offset();
        let newest_first = opts.is_newest_first();

        let index = self.client.index(&self.config.messages_index);
        let filter = filter_eq("conversation_id", conversation_id);

        // Sort order: desc for newest first, asc for oldest first
        let sort = match (opts.effective_sort(), newest_first) {
            (MessageSort::TurnIndex, true) => "turn_index:desc",
            (MessageSort::TurnIndex, false) => "turn_index:asc",
            (MessageSort::CreatedAt, true) => "created_at:desc",
            (MessageSort::CreatedAt, false) => "created_at:asc",
        };

        // Execute search with empty query to match all documents
        let results = index
            .search()
            .with_query("")
            .with_filter(&filter)
            .with_sort(&[sort])
            .with_limit(limit)
            .with_offset(offset)
            .execute::<MessageDocument>()
            .await?;

        let total_count = results.estimated_total_hits.unwrap_or(0);
        let messages: Vec<MessageDocument> = results.hits.into_iter().map(|h| h.result).collect();
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
        let index = self.client.index(&self.config.messages_index);
        let filter = options
            .conversation_id
            .as_deref()
            .map(|id| filter_eq("conversation_id", id));

        let mut search = index.search();
        search.with_query(query);
        search.with_limit(options.limit);
        search.with_offset(options.offset);
        search.with_show_ranking_score(true);
        if let Some(ref f) = filter {
            search.with_filter(f);
        }

        let results: meilisearch_sdk::search::SearchResults<MessageDocument> =
            search.execute().await?;

        Ok(results
            .hits
            .into_iter()
            .map(|h| MessageSearchHit {
                document: h.result,
                ranking_score: h.ranking_score.unwrap_or(0.0),
            })
            .collect())
    }

    async fn count_conversation_messages(&self, conversation_id: &str) -> MemoryResult<usize> {
        let index = self.client.index(&self.config.messages_index);
        let filter = filter_eq("conversation_id", conversation_id);

        // Execute search with limit 0 to just get the count
        let results = index
            .search()
            .with_query("")
            .with_filter(&filter)
            .with_limit(0)
            .execute::<MessageDocument>()
            .await?;

        Ok(results.estimated_total_hits.unwrap_or(0))
    }

    async fn list_conversations(
        &self,
        limit: usize,
        offset: usize,
    ) -> MemoryResult<Vec<ConversationDocument>> {
        let index = self.client.index(&self.config.conversations_index);

        // Sort by updated_at descending (most recent first)
        let results = index
            .search()
            .with_query("")
            .with_sort(&["updated_at:desc"])
            .with_limit(limit)
            .with_offset(offset)
            .execute::<ConversationDocument>()
            .await?;

        Ok(results.hits.into_iter().map(|h| h.result).collect())
    }
}

/// Builder for creating memory providers.
pub struct MemoryProviderBuilder {
    config: MemoryConfig,
}

impl MemoryProviderBuilder {
    /// Creates a new builder with default config.
    pub fn new() -> Self {
        Self {
            config: MemoryConfig::default(),
        }
    }

    /// Sets the Meilisearch URL.
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.config.meilisearch_url = url.into();
        self
    }

    /// Sets the API key.
    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.config.meilisearch_key = Some(key.into());
        self
    }

    /// Enables or disables memory.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.config.enabled = enabled;
        self
    }

    /// Sets the maximum context items.
    pub fn max_context_items(mut self, max: usize) -> Self {
        self.config.max_context_items = max;
        self
    }

    /// Sets the token budget.
    pub fn token_budget(mut self, budget: usize) -> Self {
        self.config.token_budget = budget;
        self
    }

    /// Sets the minimum relevance score.
    pub fn min_relevance_score(mut self, score: f64) -> Self {
        self.config.min_relevance_score = score;
        self
    }

    /// Sets the summary threshold.
    pub fn summary_threshold(mut self, threshold: usize) -> Self {
        self.config.summary_threshold = threshold;
        self
    }

    /// Builds the Meilisearch memory provider.
    pub async fn build(self) -> MemoryResult<MeilisearchMemoryProvider> {
        MeilisearchMemoryProvider::new(self.config).await
    }

    /// Builds the config without creating a provider.
    pub fn build_config(self) -> MemoryConfig {
        self.config
    }
}

impl Default for MemoryProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Formats context for injection into prompts.
pub struct ContextFormatter;

impl ContextFormatter {
    /// Formats scored results for injection into a prompt.
    pub fn format_for_prompt(results: &[ScoredMemoryResult]) -> String {
        if results.is_empty() {
            return String::new();
        }

        let mut output = String::new();
        output.push_str("## Contexte historique (pour référence)\n\n");
        output.push_str("Les informations suivantes proviennent de conversations précédentes et peuvent être pertinentes :\n\n");

        for (i, result) in results.iter().enumerate() {
            let age_desc = Self::format_age(result.document.created_at);
            let role = &result.document.role;
            let content = result.document.display_content();

            output.push_str(&format!(
                "{}. [{}] ({})\n   \"{}\"\n\n",
                i + 1,
                age_desc,
                role,
                Self::truncate(content, 200)
            ));
        }

        output.push_str("---\n\n");
        output.push_str("## Conversation actuelle (prioritaire)\n\n");

        output
    }

    /// Formats the age of a message in human-readable form.
    fn format_age(created_at: i64) -> String {
        let now = Utc::now().timestamp();
        let age_seconds = (now - created_at).max(0);

        if age_seconds < 3600 {
            let minutes = age_seconds / 60;
            format!("Il y a {} min", minutes.max(1))
        } else if age_seconds < 86400 {
            let hours = age_seconds / 3600;
            format!("Il y a {} h", hours)
        } else {
            let days = age_seconds / 86400;
            if days == 1 {
                "Hier".to_string()
            } else {
                format!("Il y a {} jours", days)
            }
        }
    }

    /// Truncates text with ellipsis.
    ///
    /// `max_len` is a **byte** budget. When it falls inside a multi-byte UTF-8
    /// character the cut is moved back to the preceding character boundary:
    /// slicing on a byte index that is not a boundary would panic, and the
    /// content stored in memory is routinely non-ASCII.
    fn truncate(s: &str, max_len: usize) -> String {
        if s.len() <= max_len {
            s.to_string()
        } else {
            let mut end = max_len;
            while end > 0 && !s.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}...", &s[..end])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_context_builder() {
        let ctx = QueryContext::new("test query")
            .with_cwd("/projects/app")
            .with_files(vec!["/src/main.rs".to_string()]);

        assert_eq!(ctx.query, "test query");
        assert_eq!(ctx.cwd, Some("/projects/app".to_string()));
        assert_eq!(ctx.files.len(), 1);
    }

    #[test]
    fn test_memory_provider_builder() {
        let config = MemoryProviderBuilder::new()
            .url("http://localhost:7700")
            .key("test-key")
            .enabled(true)
            .max_context_items(10)
            .token_budget(3000)
            .min_relevance_score(0.5)
            .build_config();

        assert_eq!(config.meilisearch_url, "http://localhost:7700");
        assert_eq!(config.meilisearch_key, Some("test-key".to_string()));
        assert!(config.enabled);
        assert_eq!(config.max_context_items, 10);
        assert_eq!(config.token_budget, 3000);
        assert_eq!(config.min_relevance_score, 0.5);
    }

    #[test]
    fn test_context_formatter_format_age() {
        let now = Utc::now().timestamp();

        // A few minutes ago
        assert!(ContextFormatter::format_age(now - 300).contains("min"));

        // A few hours ago
        assert!(ContextFormatter::format_age(now - 7200).contains("h"));

        // Yesterday
        assert_eq!(ContextFormatter::format_age(now - 86400), "Hier");

        // Several days ago
        assert!(ContextFormatter::format_age(now - 259200).contains("jours"));
    }

    #[test]
    fn test_context_formatter_truncate() {
        assert_eq!(ContextFormatter::truncate("short", 100), "short");
        assert_eq!(
            ContextFormatter::truncate("this is a long text", 10),
            "this is a ..."
        );
    }

    #[test]
    fn test_context_formatter_empty_results() {
        let results: Vec<ScoredMemoryResult> = vec![];
        let formatted = ContextFormatter::format_for_prompt(&results);
        assert!(formatted.is_empty());
    }

    // ========================================================================
    // GetMessagesOptions tests
    // ========================================================================

    #[test]
    fn test_get_messages_options_default() {
        let opts = GetMessagesOptions::default();

        assert_eq!(opts.limit, None);
        assert_eq!(opts.offset, None);
        assert_eq!(opts.newest_first, None);

        // Test effective defaults
        assert_eq!(opts.effective_limit(), 50);
        assert_eq!(opts.effective_offset(), 0);
        assert!(opts.is_newest_first());
    }

    #[test]
    fn test_get_messages_options_builder() {
        let opts = GetMessagesOptions::new()
            .limit(25)
            .offset(10)
            .newest_first(false);

        assert_eq!(opts.limit, Some(25));
        assert_eq!(opts.offset, Some(10));
        assert_eq!(opts.newest_first, Some(false));

        assert_eq!(opts.effective_limit(), 25);
        assert_eq!(opts.effective_offset(), 10);
        assert!(!opts.is_newest_first());
    }

    #[test]
    fn test_get_messages_options_partial_builder() {
        // Only set limit, others should use defaults
        let opts = GetMessagesOptions::new().limit(100);

        assert_eq!(opts.effective_limit(), 100);
        assert_eq!(opts.effective_offset(), 0); // default
        assert!(opts.is_newest_first()); // default true
    }

    #[test]
    fn test_get_messages_options_serialization() {
        let opts = GetMessagesOptions::new().limit(50).offset(10);

        let json = serde_json::to_string(&opts).unwrap();
        let parsed: GetMessagesOptions = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.limit, Some(50));
        assert_eq!(parsed.offset, Some(10));
    }

    // ========================================================================
    // PaginatedMessages tests
    // ========================================================================

    #[test]
    fn test_paginated_messages_has_more() {
        // Page with more results available
        let paginated = PaginatedMessages {
            messages: vec![],
            total_count: 100,
            has_more: true,
            offset: 0,
            limit: 50,
        };

        assert!(paginated.has_more);
        assert_eq!(paginated.total_count, 100);
    }

    #[test]
    fn test_paginated_messages_last_page() {
        // Last page - no more results
        let paginated = PaginatedMessages {
            messages: vec![],
            total_count: 75,
            has_more: false,
            offset: 50,
            limit: 50,
        };

        assert!(!paginated.has_more);
    }

    #[test]
    fn test_paginated_messages_with_documents() {
        let messages = vec![
            MessageDocument::new("msg-1", "conv-1", "user", "Hello", 0, 1700000000),
            MessageDocument::new("msg-2", "conv-1", "assistant", "Hi!", 1, 1700000001),
        ];

        let paginated = PaginatedMessages {
            messages,
            total_count: 2,
            has_more: false,
            offset: 0,
            limit: 50,
        };

        assert_eq!(paginated.messages.len(), 2);
        assert_eq!(paginated.messages[0].role, "user");
        assert_eq!(paginated.messages[1].role, "assistant");
    }

    // ========================================================================
    // MemoryError tests
    // ========================================================================

    #[test]
    fn test_memory_error_display() {
        let err = MemoryError::Meilisearch("connection failed".to_string());
        assert!(err.to_string().contains("connection failed"));

        let err = MemoryError::Config("invalid config".to_string());
        assert!(err.to_string().contains("invalid config"));

        let err = MemoryError::Disabled;
        assert!(err.to_string().contains("disabled"));
    }

    #[test]
    fn test_memory_error_from_serde() {
        let json_err = serde_json::from_str::<i32>("invalid").unwrap_err();
        let mem_err: MemoryError = json_err.into();

        assert!(
            matches!(mem_err, MemoryError::Serialization(_)),
            "a serde failure must stay a Serialization error, got {mem_err:?}"
        );
        assert!(mem_err.to_string().starts_with("Serialization error: "));
    }

    #[test]
    fn test_memory_error_from_meilisearch_error_flattens_to_a_string() {
        // The `From<meilisearch_sdk::errors::Error>` impl drops the structured
        // cause and keeps only `Display`, so the variant is unrecoverable
        // downstream: assert exactly what callers can still read.
        let mem_err: MemoryError = meilisearch_sdk::errors::Error::Timeout.into();

        match mem_err {
            MemoryError::Meilisearch(msg) => {
                assert_eq!(msg, "A task did not succeed in time.");
            },
            other => panic!("expected MemoryError::Meilisearch, got {other:?}"),
        }
    }

    // ========================================================================
    // Pure helpers of MeilisearchMemoryProvider
    //
    // `Client::new` only assembles a reqwest client; no request leaves the
    // process until a method is called. Building the struct by hand therefore
    // exercises the private helpers without a server and without `new()`'s
    // index bootstrap.
    // ========================================================================

    fn unreachable_config() -> MemoryConfig {
        // Port 1 is privileged and closed: if one of these tests ever issued a
        // request, it would fail loudly instead of reaching a real Meilisearch.
        MemoryConfig::default().with_url("http://127.0.0.1:1")
    }

    fn offline_provider(config: MemoryConfig) -> MeilisearchMemoryProvider {
        let client = Client::new(&config.meilisearch_url, config.meilisearch_key.as_deref())
            .expect("assembling the reqwest client does not touch the network");
        MeilisearchMemoryProvider {
            client,
            config,
            scorer: RelevanceScorer::new(RelevanceConfig::default()),
        }
    }

    fn ids(results: &[ScoredMemoryResult]) -> Vec<&str> {
        results.iter().map(|r| r.document.id.as_str()).collect()
    }

    fn scored(id: &str, content: impl Into<String>) -> ScoredMemoryResult {
        ScoredMemoryResult {
            document: MessageDocument::new(id, "conv-1", "user", content, 0, 1_700_000_000),
            score: RelevanceScore::zero(),
        }
    }

    #[test]
    fn test_build_filter_ignores_files_and_returns_none_without_cwd() {
        let provider = offline_provider(unreachable_config());

        // `files` is part of the query context but deliberately never reaches
        // the filter: it only feeds the Jaccard component of the score.
        let ctx = QueryContext::new("jwt").with_files(vec!["/src/auth.rs".to_string()]);

        assert_eq!(provider.build_filter(&ctx), None);
    }

    #[test]
    fn test_build_filter_emits_a_strict_equality_not_a_prefix() {
        let provider = offline_provider(unreachable_config());
        let ctx = QueryContext::new("jwt").with_cwd("/projects/app");

        // The doc comment announces "exact match or prefix" and the inline
        // comment a "STARTS_WITH-like filter"; the code emits plain equality,
        // so a message stored in a sub-directory is excluded.
        assert_eq!(
            provider.build_filter(&ctx).as_deref(),
            Some(r#"cwd = "/projects/app""#)
        );
    }

    #[test]
    fn test_build_filter_escapes_the_cwd() {
        let provider = offline_provider(unreachable_config());
        let ctx = QueryContext::new("jwt").with_cwd(r#"/a" OR role = "user"#);

        // A double quote inside the cwd stays inside the literal: the
        // remainder is part of the value, not filter syntax for the server.
        assert_eq!(
            provider.build_filter(&ctx).as_deref(),
            Some(r#"cwd = "/a\" OR role = \"user""#)
        );
    }

    #[test]
    fn test_compute_age_hours_converts_seconds_to_hours() {
        let now = Utc::now().timestamp();

        let age = compute_age_hours(now - 7200);

        assert!((age - 2.0).abs() < 0.05, "expected ~2 hours, got {age}");
    }

    #[test]
    fn test_compute_age_hours_clamps_future_timestamps_to_zero() {
        let now = Utc::now().timestamp();

        // A clock skew that puts a message in the future must not produce a
        // negative age (which `recency_score` would turn into a full score).
        assert_eq!(compute_age_hours(now + 86_400), 0.0);
    }

    fn default_scorer() -> RelevanceScorer {
        RelevanceScorer::new(RelevanceConfig::default())
    }

    #[test]
    fn test_score_hits_sorts_descending_and_drops_below_min_relevance() {
        let now = Utc::now().timestamp();
        let config = unreachable_config().with_min_relevance_score(0.3);

        let strong = MessageSearchHit {
            document: MessageDocument::new("strong", "conv-1", "user", "jwt", 0, now)
                .with_cwd("/w")
                .with_files_touched(vec!["/w/a.rs".to_string()]),
            ranking_score: 2.0,
        };
        // A hit without `_rankingScore` reads 0.0: the semantic component is
        // 0.0 instead of failing, and only recency keeps this hit alive.
        let weak = MessageSearchHit {
            document: MessageDocument::new("weak", "conv-1", "user", "jwt", 1, now),
            ranking_score: 0.0,
        };
        let ctx = QueryContext::new("jwt")
            .with_cwd("/w")
            .with_files(vec!["/w/a.rs".to_string()]);

        let results = score_hits(&default_scorer(), &config, vec![weak, strong], &ctx);

        assert_eq!(ids(&results), vec!["strong"]);
        assert_eq!(results[0].score.semantic, 1.0);
        assert!((results[0].score.total - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_score_hits_keeps_weak_hits_when_the_threshold_is_zero() {
        let now = Utc::now().timestamp();
        let config = unreachable_config().with_min_relevance_score(0.0);

        let strong = MessageSearchHit {
            document: MessageDocument::new("strong", "conv-1", "user", "jwt", 0, now)
                .with_cwd("/w"),
            ranking_score: 2.0,
        };
        let weak = MessageSearchHit {
            document: MessageDocument::new("weak", "conv-1", "user", "jwt", 1, now),
            ranking_score: 0.0,
        };
        let ctx = QueryContext::new("jwt").with_cwd("/w");

        let results = score_hits(&default_scorer(), &config, vec![weak, strong], &ctx);

        assert_eq!(ids(&results), vec!["strong", "weak"]);
        assert_eq!(results[1].score.semantic, 0.0);
        assert_eq!(results[1].score.cwd_match, 0.0);
    }

    #[test]
    fn test_score_hits_on_no_hits() {
        let results = score_hits(
            &default_scorer(),
            &unreachable_config(),
            vec![],
            &QueryContext::new("jwt"),
        );

        assert!(results.is_empty());
    }

    #[test]
    fn test_rank_for_context_caps_to_max_context_items_min_limit() {
        let now = Utc::now().timestamp();
        let config = unreachable_config()
            .with_min_relevance_score(0.0)
            .with_max_context_items(5);
        let hits = (0..4)
            .map(|i| MessageSearchHit {
                document: MessageDocument::new(format!("m{i}"), "conv-1", "user", "jwt", i, now),
                ranking_score: 2.0 - i as f64 * 0.1,
            })
            .collect();

        let results = rank_for_context(
            &default_scorer(),
            &config,
            hits,
            &QueryContext::new("jwt"),
            2,
        );

        assert_eq!(
            ids(&results),
            vec!["m0", "m1"],
            "limit = 2 wins over max_context_items = 5"
        );
    }

    #[test]
    fn test_apply_token_budget_skips_an_oversized_result_but_keeps_filling() {
        // 10 tokens * 4 chars = 40 characters of budget.
        let config = unreachable_config().with_token_budget(10);

        let results = vec![
            scored("a", "x".repeat(30)),
            scored("b", "x".repeat(20)), // does not fit in the 10 chars left
            scored("c", "x".repeat(5)),  // still fits: the loop does not stop
        ];

        // The budget is a filter, not a prefix: "b" is dropped and the lower
        // ranked "c" takes its place.
        assert_eq!(ids(&apply_token_budget(&config, results)), vec!["a", "c"]);
    }

    #[test]
    fn test_apply_token_budget_always_keeps_one_result_even_over_budget() {
        let config = unreachable_config().with_token_budget(0);

        let results = vec![scored("a", "x".repeat(100)), scored("b", "y")];

        // With a zero budget the first result is kept anyway and the loop
        // breaks, so "b" never gets a chance.
        assert_eq!(ids(&apply_token_budget(&config, results)), vec!["a"]);
    }

    #[test]
    fn test_apply_token_budget_on_empty_input() {
        assert!(apply_token_budget(&unreachable_config(), Vec::new()).is_empty());
    }

    #[test]
    fn test_apply_token_budget_measures_the_summary_not_the_content() {
        // 2 tokens * 4 chars = 8 characters of budget.
        let config = unreachable_config().with_token_budget(2);

        let summarized = ScoredMemoryResult {
            document: MessageDocument::new(
                "a",
                "conv-1",
                "user",
                "x".repeat(1000),
                0,
                1_700_000_000,
            )
            .with_summary("court"),
            score: RelevanceScore::zero(),
        };

        // Had the 1000-char content been measured, the "always keep one"
        // branch would have fired and "b" would be missing.
        assert_eq!(
            ids(&apply_token_budget(
                &config,
                vec![summarized, scored("b", "zz")]
            )),
            vec!["a", "b"]
        );
    }

    // ========================================================================
    // ContextFormatter
    // ========================================================================

    #[test]
    fn test_format_for_prompt_numbers_entries_and_truncates_at_200_bytes() {
        let now = Utc::now().timestamp();
        let result = ScoredMemoryResult {
            document: MessageDocument::new(
                "m1",
                "conv-1",
                "assistant",
                "x".repeat(250),
                0,
                now - 7200,
            ),
            score: RelevanceScore::zero(),
        };

        let out = ContextFormatter::format_for_prompt(std::slice::from_ref(&result));

        assert!(out.starts_with("## Contexte historique (pour référence)\n\n"));
        assert!(out.contains("1. [Il y a 2 h] (assistant)"), "got: {out}");
        assert!(out.contains(&format!("\"{}...\"", "x".repeat(200))));
        assert!(
            out.trim_end()
                .ends_with("## Conversation actuelle (prioritaire)")
        );
    }

    #[test]
    fn test_format_for_prompt_prefers_the_summary_over_the_content() {
        let now = Utc::now().timestamp();
        let result = ScoredMemoryResult {
            document: MessageDocument::new("m1", "conv-1", "user", "contenu complet", 0, now)
                .with_summary("résumé"),
            score: RelevanceScore::zero(),
        };

        let out = ContextFormatter::format_for_prompt(std::slice::from_ref(&result));

        assert!(out.contains("\"résumé\""));
        assert!(!out.contains("contenu complet"));
    }

    #[test]
    fn test_format_for_prompt_does_not_panic_on_accented_content() {
        // 201 bytes where byte 200 lands inside a two-byte character: the
        // naive `&s[..200]` of `truncate` panicked on exactly this input.
        let content = format!("a{}", "é".repeat(100));
        assert_eq!(content.len(), 201);

        let result = ScoredMemoryResult {
            document: MessageDocument::new("m1", "conv-1", "user", content, 0, 1_700_000_000),
            score: RelevanceScore::zero(),
        };

        let out = ContextFormatter::format_for_prompt(std::slice::from_ref(&result));

        // 199 bytes kept (the last whole character before the cut), then "...".
        assert!(out.contains(&format!("\"a{}...\"", "é".repeat(99))));
    }

    #[test]
    fn test_truncate_moves_the_cut_back_to_a_char_boundary() {
        // "ééé" is 6 bytes; byte 5 is the middle of the third character.
        assert_eq!(ContextFormatter::truncate("ééé", 5), "éé...");
        // A budget that lands exactly on a boundary is used as is.
        assert_eq!(ContextFormatter::truncate("ééé", 4), "éé...");
        // No boundary at all below the budget: everything is dropped.
        assert_eq!(ContextFormatter::truncate("é", 1), "...");
    }

    #[test]
    fn test_format_age_never_reports_zero_minutes() {
        let now = Utc::now().timestamp();

        assert_eq!(ContextFormatter::format_age(now), "Il y a 1 min");
        assert_eq!(ContextFormatter::format_age(now - 59), "Il y a 1 min");
        assert_eq!(ContextFormatter::format_age(now - 3599), "Il y a 59 min");
        assert_eq!(ContextFormatter::format_age(now - 3600), "Il y a 1 h");
        assert_eq!(ContextFormatter::format_age(now - 86_399), "Il y a 23 h");
        assert_eq!(
            ContextFormatter::format_age(now - 172_800),
            "Il y a 2 jours"
        );
    }

    #[test]
    fn test_format_age_clamps_future_timestamps() {
        let now = Utc::now().timestamp();

        // A message dated in the future is reported as one minute old rather
        // than producing a negative duration.
        assert_eq!(ContextFormatter::format_age(now + 86_400), "Il y a 1 min");
    }

    #[tokio::test]
    async fn test_provider_new_refuses_a_disabled_config() {
        let config = MemoryConfig::default()
            .with_url("http://127.0.0.1:1")
            .with_enabled(false);

        // `MeilisearchMemoryProvider` is not `Debug`, hence `matches!` rather
        // than `expect_err`.
        let result = MeilisearchMemoryProvider::new(config).await;

        assert!(
            matches!(result, Err(MemoryError::Disabled)),
            "a disabled config must not build a provider"
        );
    }

    #[tokio::test]
    async fn test_builder_build_refuses_a_disabled_config() {
        let result = MemoryProviderBuilder::new()
            .url("http://127.0.0.1:1")
            .enabled(false)
            .build()
            .await;

        assert!(
            matches!(result, Err(MemoryError::Disabled)),
            "the builder must relay the Disabled error"
        );
    }

    #[test]
    fn test_builder_summary_threshold_is_carried_into_the_config() {
        let config = MemoryProviderBuilder::default()
            .summary_threshold(1234)
            .build_config();

        assert_eq!(config.summary_threshold, 1234);
    }

    // ========================================================================
    // filter_eq, MessageSort, MessageSearchOptions, ConversationSearchGroup
    // ========================================================================

    #[test]
    fn test_filter_eq_escapes_quotes_and_backslashes() {
        assert_eq!(
            filter_eq("conversation_id", "conv-1"),
            r#"conversation_id = "conv-1""#
        );
        assert_eq!(
            filter_eq("conversation_id", r#"x" OR role = "user"#),
            r#"conversation_id = "x\" OR role = \"user""#
        );
        assert_eq!(filter_eq("cwd", r"C:\w"), r#"cwd = "C:\\w""#);
    }

    #[test]
    fn test_get_messages_options_default_sort_is_turn_index() {
        let opts = GetMessagesOptions::default();
        assert_eq!(opts.sort_by, None);
        assert_eq!(opts.effective_sort(), MessageSort::TurnIndex);

        let opts = GetMessagesOptions::new().sort_by(MessageSort::CreatedAt);
        assert_eq!(opts.effective_sort(), MessageSort::CreatedAt);
    }

    #[test]
    fn test_get_messages_options_sort_by_round_trips_through_serde() {
        let json = serde_json::to_value(GetMessagesOptions::new().sort_by(MessageSort::CreatedAt))
            .unwrap();
        assert_eq!(json["sort_by"], "created_at");

        // An options payload written before `sort_by` existed still parses.
        let legacy: GetMessagesOptions = serde_json::from_str(r#"{"limit": 3}"#).unwrap();
        assert_eq!(legacy.effective_sort(), MessageSort::TurnIndex);
    }

    #[test]
    fn test_message_search_options_defaults_and_builder() {
        let opts = MessageSearchOptions::default();
        assert_eq!(opts.limit, 20);
        assert_eq!(opts.offset, 0);
        assert_eq!(opts.conversation_id, None);

        let opts = MessageSearchOptions::new()
            .limit(7)
            .offset(2)
            .conversation_id("conv-9");
        assert_eq!(opts.limit, 7);
        assert_eq!(opts.offset, 2);
        assert_eq!(opts.conversation_id.as_deref(), Some("conv-9"));
    }

    fn hit(id: &str, conv: &str, score: f64) -> MessageSearchHit {
        MessageSearchHit {
            document: MessageDocument::new(id, conv, "user", id, 0, 1_700_000_000),
            ranking_score: score,
        }
    }

    #[test]
    fn test_group_orders_conversations_by_best_score_and_keeps_engine_order() {
        let groups = ConversationSearchGroup::group(vec![
            hit("m1", "conv-a", 0.4),
            hit("m2", "conv-b", 0.9),
            hit("m3", "conv-a", 0.7),
            hit("m4", "conv-b", 0.1),
        ]);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].conversation_id, "conv-b");
        assert_eq!(groups[0].best_score, 0.9);
        assert_eq!(
            groups[0]
                .hits
                .iter()
                .map(|h| h.document.id.as_str())
                .collect::<Vec<_>>(),
            ["m2", "m4"],
            "hits keep the engine's order inside a group"
        );
        assert_eq!(groups[1].conversation_id, "conv-a");
        assert_eq!(groups[1].best_score, 0.7);
    }

    #[test]
    fn test_group_is_stable_on_equal_best_scores_and_empty_on_no_hit() {
        let groups = ConversationSearchGroup::group(vec![
            hit("m1", "conv-a", 0.5),
            hit("m2", "conv-b", 0.5),
        ]);
        assert_eq!(groups[0].conversation_id, "conv-a");
        assert_eq!(groups[1].conversation_id, "conv-b");

        assert!(ConversationSearchGroup::group(Vec::new()).is_empty());
    }
}
