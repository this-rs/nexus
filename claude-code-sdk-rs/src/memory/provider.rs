//! Memory provider implementations for persistent storage.
//!
//! This module provides the Meilisearch-based memory provider that handles:
//! - Storing and retrieving messages
//! - Applying multi-factor relevance scoring
//! - Managing index settings
//! - Retrieving conversation history with pagination

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

/// Options for retrieving conversation messages with pagination.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetMessagesOptions {
    /// Maximum number of messages to return (default: 50)
    pub limit: Option<usize>,

    /// Number of messages to skip for pagination (default: 0)
    pub offset: Option<usize>,

    /// If true, return newest messages first (descending turn_index).
    /// Default: true (newest first, like a chat UI)
    pub newest_first: Option<bool>,
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
    /// which is suitable for chat UI display.
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

        // Filter by cwd if provided (exact match or prefix)
        if let Some(ref cwd) = context.cwd {
            // Use a STARTS_WITH-like filter for cwd matching
            filters.push(format!("cwd = \"{}\"", cwd));
        }

        if filters.is_empty() {
            None
        } else {
            Some(filters.join(" AND "))
        }
    }

    /// Computes the age in hours for a message.
    fn compute_age_hours(&self, created_at: i64) -> f64 {
        let now = Utc::now().timestamp();
        let age_seconds = (now - created_at).max(0) as f64;
        age_seconds / 3600.0
    }

    /// Applies relevance scoring to search results.
    fn score_results(
        &self,
        hits: Vec<SearchHit>,
        context: &QueryContext,
    ) -> Vec<ScoredMemoryResult> {
        let mut results: Vec<ScoredMemoryResult> = hits
            .into_iter()
            .map(|hit| {
                let age_hours = self.compute_age_hours(hit.document.created_at);

                let score = self.scorer.compute_score(
                    hit.score.unwrap_or(0.0),
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
        results.retain(|r| r.score.total >= self.config.min_relevance_score);

        results
    }

    /// Applies token budget to results.
    fn apply_token_budget(&self, results: Vec<ScoredMemoryResult>) -> Vec<ScoredMemoryResult> {
        let mut budget_remaining = self.config.token_budget * 4; // ~4 chars per token
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
}

/// Internal search hit structure.
#[derive(Debug, Deserialize)]
struct SearchHit {
    #[serde(flatten)]
    document: MessageDocument,
    #[serde(rename = "_rankingScore")]
    score: Option<f64>,
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

        // Convert to SearchHit format
        let hits: Vec<SearchHit> = results
            .hits
            .into_iter()
            .map(|h| SearchHit {
                document: h.result,
                score: h.ranking_score,
            })
            .collect();

        // Score and filter results
        let scored = self.score_results(hits, context);

        // Apply limits
        let limited: Vec<_> = scored
            .into_iter()
            .take(self.config.max_context_items.min(limit))
            .collect();

        // Apply token budget
        let budgeted = self.apply_token_budget(limited);

        Ok(budgeted)
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
        let filter = format!("conversation_id = \"{}\"", conversation_id);

        // Sort order: desc for newest first, asc for oldest first
        let sort = if newest_first {
            "turn_index:desc"
        } else {
            "turn_index:asc"
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

    async fn count_conversation_messages(&self, conversation_id: &str) -> MemoryResult<usize> {
        let index = self.client.index(&self.config.messages_index);
        let filter = format!("conversation_id = \"{}\"", conversation_id);

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
    fn truncate(s: &str, max_len: usize) -> String {
        if s.len() <= max_len {
            s.to_string()
        } else {
            format!("{}...", &s[..max_len])
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

        matches!(mem_err, MemoryError::Serialization(_));
    }
}
