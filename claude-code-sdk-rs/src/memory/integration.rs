//! Integration module for memory system with SDK conversation flow.
//!
//! This module provides the glue between the memory system and the
//! SDK's conversation management.

#[cfg(feature = "memory")]
use super::provider::GetMessagesOptions;
#[cfg(feature = "memory")]
use super::{ContextFormatter, MeilisearchMemoryProvider, MemoryProvider};
use super::{DefaultToolContextExtractor, MemoryConfig, MessageContextAggregator, MessageDocument};

#[cfg(feature = "memory")]
use chrono::Utc;
#[cfg(not(feature = "memory"))]
use std::time::SystemTime;

use serde_json::Value;
#[cfg(feature = "memory")]
use std::sync::Arc;
use uuid::Uuid;

/// Returns the current Unix timestamp in seconds.
fn current_timestamp() -> i64 {
    #[cfg(feature = "memory")]
    {
        Utc::now().timestamp()
    }
    #[cfg(not(feature = "memory"))]
    {
        unix_seconds(SystemTime::now())
    }
}

/// Converts a wall-clock instant into a Unix timestamp in seconds.
///
/// Instants before the epoch yield a **negative** timestamp.
/// `SystemTime::duration_since(UNIX_EPOCH)` reports them as an `Err` carrying
/// the distance travelled backwards; swallowing that error with
/// `unwrap_or(0)` would stamp every message of a machine whose clock sits
/// before 1970 with the epoch itself, and `RelevanceScorer::recency_score`
/// would then read those messages as more than half a century old.
#[cfg(any(not(feature = "memory"), test))]
fn unix_seconds(time: std::time::SystemTime) -> i64 {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(forward) => forward.as_secs() as i64,
        Err(backward) => -(backward.duration().as_secs() as i64),
    }
}

/// Query context for memory retrieval.
/// Only defined when memory feature is disabled (provider.rs defines it otherwise).
#[cfg(not(feature = "memory"))]
#[derive(Debug, Clone)]
pub struct QueryContext {
    /// The query text.
    pub query: String,
    /// Current working directory.
    pub cwd: Option<String>,
    /// Files currently being worked on.
    pub files: Vec<String>,
}

#[cfg(feature = "memory")]
use super::provider::QueryContext;

/// Manages conversation context for memory operations.
///
/// This struct tracks the current conversation state and provides
/// methods for capturing and injecting context.
pub struct ConversationMemoryManager {
    /// Current conversation ID
    conversation_id: String,

    /// Current working directory
    cwd: Option<String>,

    /// Context aggregator for the current turn
    aggregator: MessageContextAggregator,

    /// Tool context extractor (reserved for future use)
    #[allow(dead_code)]
    extractor: DefaultToolContextExtractor,

    /// Current turn index
    turn_index: usize,

    /// Messages pending storage
    pending_messages: Vec<MessageDocument>,

    /// Memory configuration
    config: MemoryConfig,
}

impl ConversationMemoryManager {
    /// Creates a new ConversationMemoryManager.
    pub fn new(config: MemoryConfig) -> Self {
        Self {
            conversation_id: format!("conv-{}", Uuid::new_v4()),
            cwd: None,
            aggregator: MessageContextAggregator::new(),
            extractor: DefaultToolContextExtractor::new(),
            turn_index: 0,
            pending_messages: Vec::new(),
            config,
        }
    }

    /// Creates a manager with a specific conversation ID.
    pub fn with_conversation_id(mut self, id: impl Into<String>) -> Self {
        self.conversation_id = id.into();
        self
    }

    /// Sets the working directory.
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        let cwd = cwd.into();
        self.cwd = Some(cwd.clone());
        self.aggregator = MessageContextAggregator::with_initial_cwd(cwd);
        self
    }

    /// Returns the current conversation ID.
    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    /// Returns the current working directory.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Returns the current turn index.
    pub fn turn_index(&self) -> usize {
        self.turn_index
    }

    /// Returns whether memory is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Processes a tool call and accumulates context.
    ///
    /// Call this for each tool use observed during a turn.
    pub fn process_tool_call(&mut self, tool_name: &str, input: &Value) {
        if !self.config.enabled {
            return;
        }

        self.aggregator.process_tool_call(tool_name, input);

        // Update cwd if changed
        if let Some(new_cwd) = self.aggregator.cwd() {
            self.cwd = Some(new_cwd.to_string());
        }
    }

    /// Records a user message.
    ///
    /// Call this when a user message is received.
    pub fn record_user_message(&mut self, content: &str) {
        if !self.config.enabled {
            return;
        }

        let timestamp = current_timestamp();
        let msg = MessageDocument::new(
            format!("msg-{}", Uuid::new_v4()),
            &self.conversation_id,
            "user",
            content,
            self.turn_index,
            timestamp,
        );

        let msg = if let Some(ref cwd) = self.cwd {
            msg.with_cwd(cwd.clone())
        } else {
            msg
        };

        self.pending_messages.push(msg);
    }

    /// Records an assistant message.
    ///
    /// Call this when an assistant response is complete.
    /// This also captures the accumulated tool context.
    pub fn record_assistant_message(&mut self, content: &str) {
        if !self.config.enabled {
            return;
        }

        let context = self.aggregator.finalize();
        let timestamp = current_timestamp();

        let msg = MessageDocument::new(
            format!("msg-{}", Uuid::new_v4()),
            &self.conversation_id,
            "assistant",
            content,
            self.turn_index,
            timestamp,
        )
        .with_files_touched(context.files);

        let msg = if let Some(ref cwd) = self.cwd {
            msg.with_cwd(cwd.clone())
        } else {
            msg
        };

        self.pending_messages.push(msg);

        // Prepare for next turn
        self.turn_index += 1;
        self.aggregator.reset();
    }

    /// Returns and clears pending messages for storage.
    pub fn take_pending_messages(&mut self) -> Vec<MessageDocument> {
        std::mem::take(&mut self.pending_messages)
    }

    /// Returns the current context for querying memory.
    pub fn current_context(&self, query: &str) -> QueryContext {
        QueryContext {
            query: query.to_string(),
            cwd: self.cwd.clone(),
            files: self.aggregator.files(),
        }
    }

    /// Returns the memory configuration.
    pub fn config(&self) -> &MemoryConfig {
        &self.config
    }

    /// Resumes a conversation from a loaded state.
    ///
    /// This sets up the manager to continue an existing conversation
    /// by restoring the conversation ID, working directory, and turn index.
    ///
    /// # Arguments
    ///
    /// * `conversation_id` - The ID of the conversation to resume
    /// * `cwd` - The working directory to restore (if any)
    /// * `turn_index` - The turn index to resume from (typically max_turn_index + 1)
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Load conversation from memory
    /// let loaded = injector.load_conversation("conv-123", Some(50), None).await?;
    ///
    /// // Resume the conversation
    /// let next_turn = loaded.max_turn_index().map(|i| i + 1).unwrap_or(0);
    /// manager.resume_conversation(
    ///     "conv-123",
    ///     loaded.messages.first().and_then(|m| m.cwd.as_deref()),
    ///     next_turn,
    /// );
    /// ```
    pub fn resume_conversation(
        &mut self,
        conversation_id: impl Into<String>,
        cwd: Option<&str>,
        turn_index: usize,
    ) {
        self.conversation_id = conversation_id.into();
        self.turn_index = turn_index;

        if let Some(cwd_str) = cwd {
            self.cwd = Some(cwd_str.to_string());
            self.aggregator = MessageContextAggregator::with_initial_cwd(cwd_str.to_string());
        }

        // Clear any pending messages from previous state
        self.pending_messages.clear();
    }

    /// Returns whether this manager has been resumed from an existing conversation.
    ///
    /// This is determined by checking if the turn index is greater than 0.
    pub fn is_resumed(&self) -> bool {
        self.turn_index > 0
    }
}

/// Builder for creating memory-enabled conversations.
pub struct MemoryIntegrationBuilder {
    config: MemoryConfig,
    conversation_id: Option<String>,
    cwd: Option<String>,
}

impl MemoryIntegrationBuilder {
    /// Creates a new builder with default config.
    pub fn new() -> Self {
        Self {
            config: MemoryConfig::default(),
            conversation_id: None,
            cwd: None,
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

    /// Sets the conversation ID.
    pub fn conversation_id(mut self, id: impl Into<String>) -> Self {
        self.conversation_id = Some(id.into());
        self
    }

    /// Sets the working directory.
    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Sets the minimum relevance score.
    pub fn min_relevance_score(mut self, score: f64) -> Self {
        self.config.min_relevance_score = score;
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

    /// Builds the ConversationMemoryManager.
    pub fn build(self) -> ConversationMemoryManager {
        let mut manager = ConversationMemoryManager::new(self.config);

        if let Some(id) = self.conversation_id {
            manager = manager.with_conversation_id(id);
        }

        if let Some(cwd) = self.cwd {
            manager = manager.with_cwd(cwd);
        }

        manager
    }
}

impl Default for MemoryIntegrationBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Context injector for formatting and injecting memory context into prompts.
#[cfg(feature = "memory")]
pub struct ContextInjector {
    provider: Arc<MeilisearchMemoryProvider>,
    config: MemoryConfig,
}

#[cfg(feature = "memory")]
impl ContextInjector {
    /// Creates a new ContextInjector.
    pub async fn new(config: MemoryConfig) -> Result<Self, super::provider::MemoryError> {
        let provider = MeilisearchMemoryProvider::new(config.clone()).await?;
        Ok(Self {
            provider: Arc::new(provider),
            config,
        })
    }

    /// Retrieves and formats context for injection into a prompt.
    ///
    /// Returns a formatted string to prepend to the conversation,
    /// or None if no relevant context was found.
    pub async fn get_context_prefix(
        &self,
        query: &str,
        cwd: Option<&str>,
        files: &[String],
    ) -> Result<Option<String>, super::provider::MemoryError> {
        if !self.config.enabled {
            return Ok(None);
        }

        let context = QueryContext {
            query: query.to_string(),
            cwd: cwd.map(String::from),
            files: files.to_vec(),
        };

        let results = self
            .provider
            .retrieve_context(&context, self.config.max_context_items)
            .await?;

        if results.is_empty() {
            return Ok(None);
        }

        let formatted = ContextFormatter::format_for_prompt(&results);
        Ok(Some(formatted))
    }

    /// Stores messages in the memory system.
    pub async fn store_messages(
        &self,
        messages: &[MessageDocument],
    ) -> Result<(), super::provider::MemoryError> {
        if !self.config.enabled || messages.is_empty() {
            return Ok(());
        }

        self.provider.store_messages(messages).await
    }

    /// Returns a reference to the underlying provider.
    pub fn provider(&self) -> &MeilisearchMemoryProvider {
        &self.provider
    }

    /// Loads all messages from a conversation with pagination support.
    ///
    /// This method retrieves the complete message history for a conversation,
    /// useful for re-displaying messages when resuming a session.
    ///
    /// # Arguments
    ///
    /// * `conversation_id` - The ID of the conversation to load
    /// * `limit` - Maximum number of messages to retrieve (default: 50)
    /// * `offset` - Number of messages to skip for pagination (default: 0)
    ///
    /// # Returns
    ///
    /// A `LoadedConversation` containing messages ordered from newest to oldest,
    /// along with pagination info.
    pub async fn load_conversation(
        &self,
        conversation_id: &str,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<LoadedConversation, super::provider::MemoryError> {
        let options = GetMessagesOptions::new()
            .limit(limit.unwrap_or(50))
            .offset(offset.unwrap_or(0))
            .newest_first(true);

        let paginated = self
            .provider
            .get_conversation_messages(conversation_id, Some(options))
            .await?;

        Ok(LoadedConversation {
            messages: paginated.messages,
            total_count: paginated.total_count,
            has_more: paginated.has_more,
            offset: paginated.offset,
            limit: paginated.limit,
        })
    }

    /// Lists all available conversations with pagination.
    ///
    /// Returns conversations ordered by most recently updated.
    pub async fn list_conversations(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<super::ConversationDocument>, super::provider::MemoryError> {
        self.provider.list_conversations(limit, offset).await
    }

    /// Counts the total number of messages in a conversation.
    pub async fn count_conversation_messages(
        &self,
        conversation_id: &str,
    ) -> Result<usize, super::provider::MemoryError> {
        self.provider
            .count_conversation_messages(conversation_id)
            .await
    }
}

/// Result of loading a conversation's message history.
///
/// Contains the messages along with pagination information for
/// implementing infinite scroll or paged UIs.
#[cfg(feature = "memory")]
#[derive(Debug, Clone)]
pub struct LoadedConversation {
    /// The messages in this page, ordered from newest to oldest.
    pub messages: Vec<MessageDocument>,

    /// Total number of messages in the conversation.
    pub total_count: usize,

    /// Whether there are more messages beyond this page.
    pub has_more: bool,

    /// The offset used for this query.
    pub offset: usize,

    /// The limit used for this query.
    pub limit: usize,
}

#[cfg(feature = "memory")]
impl LoadedConversation {
    /// Returns true if this is an empty conversation.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Returns the number of messages in this page.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Returns the offset for the next page of messages.
    pub fn next_offset(&self) -> usize {
        self.offset + self.messages.len()
    }

    /// Returns messages in chronological order (oldest first).
    ///
    /// This is useful for displaying messages in a chat UI where
    /// older messages appear at the top.
    pub fn messages_chronological(&self) -> Vec<&MessageDocument> {
        let mut msgs: Vec<_> = self.messages.iter().collect();
        msgs.reverse();
        msgs
    }

    /// Returns the highest turn index in this page, if any.
    pub fn max_turn_index(&self) -> Option<usize> {
        self.messages.iter().map(|m| m.turn_index).max()
    }
}

/// Summary generator for long messages.
///
/// This is a placeholder that can be extended to use an LLM
/// for generating summaries.
pub struct SummaryGenerator {
    /// Minimum content length to trigger summarization
    threshold: usize,
}

impl SummaryGenerator {
    /// Creates a new SummaryGenerator.
    pub fn new(threshold: usize) -> Self {
        Self { threshold }
    }

    /// Creates with default threshold (500 chars).
    pub fn default_threshold() -> Self {
        Self::new(500)
    }

    /// Checks if content needs summarization.
    pub fn needs_summary(&self, content: &str) -> bool {
        content.len() > self.threshold
    }

    /// Generates a simple extractive summary.
    ///
    /// This is a basic implementation that extracts the first
    /// and last sentences. For production use, consider using
    /// an LLM for abstractive summarization.
    ///
    /// When the content holds no sentence at all (only delimiters and
    /// whitespace) the fallback keeps a prefix of `threshold` **bytes**: the
    /// cut is moved back to the preceding character boundary, because slicing
    /// in the middle of a multi-byte character would panic.
    pub fn generate_simple_summary(&self, content: &str) -> String {
        if !self.needs_summary(content) {
            return content.to_string();
        }

        // Simple extractive summary: first sentence + "..." + last sentence
        let sentences: Vec<&str> = content
            .split(['.', '!', '?'])
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        match sentences.len() {
            0 => {
                let mut end = self.threshold.min(content.len());
                while end > 0 && !content.is_char_boundary(end) {
                    end -= 1;
                }
                content[..end].to_string() + "..."
            },
            1 => sentences[0].to_string(),
            2 => format!("{}. ... {}", sentences[0], sentences[1]),
            _ => format!("{}. ... {}", sentences[0], sentences[sentences.len() - 1]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conversation_memory_manager_new() {
        let config = MemoryConfig::default().with_enabled(true);
        let manager = ConversationMemoryManager::new(config);

        assert!(manager.conversation_id().starts_with("conv-"));
        assert_eq!(manager.turn_index(), 0);
        assert!(manager.is_enabled());
    }

    #[test]
    fn test_conversation_memory_manager_with_cwd() {
        let config = MemoryConfig::default().with_enabled(true);
        let manager = ConversationMemoryManager::new(config).with_cwd("/projects/test");

        assert_eq!(manager.cwd(), Some("/projects/test"));
    }

    #[test]
    fn test_record_messages() {
        let config = MemoryConfig::default().with_enabled(true);
        let mut manager = ConversationMemoryManager::new(config).with_cwd("/projects/test");

        manager.record_user_message("Hello");
        manager.record_assistant_message("Hi there!");

        let messages = manager.take_pending_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[0].cwd, Some("/projects/test".to_string()));
    }

    #[test]
    fn test_process_tool_call() {
        let config = MemoryConfig::default().with_enabled(true);
        let mut manager = ConversationMemoryManager::new(config);

        manager.process_tool_call(
            "Read",
            &serde_json::json!({
                "file_path": "/src/main.rs"
            }),
        );
        manager.process_tool_call(
            "Bash",
            &serde_json::json!({
                "command": "cd /projects/app && cargo build"
            }),
        );

        assert_eq!(manager.cwd(), Some("/projects/app"));

        let ctx = manager.current_context("test query");
        assert!(ctx.files.contains(&"/src/main.rs".to_string()));
    }

    #[test]
    fn test_disabled_memory_does_nothing() {
        let config = MemoryConfig::default().with_enabled(false);
        let mut manager = ConversationMemoryManager::new(config);

        manager.record_user_message("Hello");
        manager.record_assistant_message("Hi!");

        let messages = manager.take_pending_messages();
        assert!(messages.is_empty());
    }

    #[test]
    fn test_memory_integration_builder() {
        let manager = MemoryIntegrationBuilder::new()
            .enabled(true)
            .cwd("/projects/test")
            .conversation_id("test-conv-1")
            .min_relevance_score(0.5)
            .max_context_items(10)
            .build();

        assert_eq!(manager.conversation_id(), "test-conv-1");
        assert_eq!(manager.cwd(), Some("/projects/test"));
        assert!(manager.is_enabled());
        assert_eq!(manager.config().min_relevance_score, 0.5);
        assert_eq!(manager.config().max_context_items, 10);
    }

    #[test]
    fn test_summary_generator() {
        let generator = SummaryGenerator::new(50);

        // Short content doesn't need summary
        assert!(!generator.needs_summary("Short text."));

        // Long content needs summary
        let long_content = "First sentence. Second sentence. Third sentence. Fourth sentence. Fifth sentence. Sixth sentence.";
        assert!(generator.needs_summary(long_content));

        let summary = generator.generate_simple_summary(long_content);
        assert!(summary.contains("First sentence"));
        assert!(summary.contains("Sixth sentence"));
        assert!(summary.contains("..."));
    }

    #[test]
    fn test_turn_index_increments() {
        let config = MemoryConfig::default().with_enabled(true);
        let mut manager = ConversationMemoryManager::new(config);

        assert_eq!(manager.turn_index(), 0);

        manager.record_user_message("Q1");
        manager.record_assistant_message("A1");
        assert_eq!(manager.turn_index(), 1);

        manager.record_user_message("Q2");
        manager.record_assistant_message("A2");
        assert_eq!(manager.turn_index(), 2);
    }

    #[test]
    fn test_resume_conversation() {
        let config = MemoryConfig::default().with_enabled(true);
        let mut manager = ConversationMemoryManager::new(config);

        // Initially not resumed
        assert!(!manager.is_resumed());
        assert_eq!(manager.turn_index(), 0);

        // Resume from an existing conversation
        manager.resume_conversation("existing-conv-123", Some("/projects/test"), 5);

        assert!(manager.is_resumed());
        assert_eq!(manager.conversation_id(), "existing-conv-123");
        assert_eq!(manager.cwd(), Some("/projects/test"));
        assert_eq!(manager.turn_index(), 5);
    }

    #[test]
    fn test_resume_conversation_without_cwd() {
        let config = MemoryConfig::default().with_enabled(true);
        let mut manager = ConversationMemoryManager::new(config).with_cwd("/old/path");

        manager.resume_conversation("conv-456", None, 3);

        assert_eq!(manager.conversation_id(), "conv-456");
        // CWD should remain from before since we passed None
        assert_eq!(manager.cwd(), Some("/old/path"));
        assert_eq!(manager.turn_index(), 3);
    }

    #[test]
    fn test_resume_clears_pending_messages() {
        let config = MemoryConfig::default().with_enabled(true);
        let mut manager = ConversationMemoryManager::new(config);

        // Add a message without taking it
        manager.record_user_message("Hello");

        // Resume should clear pending messages
        manager.resume_conversation("new-conv", Some("/new/path"), 10);

        let pending = manager.take_pending_messages();
        assert!(
            pending.is_empty(),
            "Expected pending messages to be cleared after resume"
        );
    }

    #[test]
    fn test_record_after_resume() {
        let config = MemoryConfig::default().with_enabled(true);
        let mut manager = ConversationMemoryManager::new(config);

        // Resume at turn 5
        manager.resume_conversation("conv-789", Some("/projects/app"), 5);

        // Record new messages
        manager.record_user_message("Continued question");
        manager.record_assistant_message("Continued answer");

        let messages = manager.take_pending_messages();
        assert_eq!(messages.len(), 2);

        // Messages should have the resumed conversation ID
        assert_eq!(messages[0].conversation_id, "conv-789");
        assert_eq!(messages[1].conversation_id, "conv-789");

        // Turn indices should continue from 5
        assert_eq!(messages[0].turn_index, 5);
        assert_eq!(messages[1].turn_index, 5);

        // Next turn should be 6
        assert_eq!(manager.turn_index(), 6);
    }

    // -----------------------------------------------------------------------
    // current_timestamp / unix_seconds
    // -----------------------------------------------------------------------

    #[test]
    fn unix_seconds_keeps_the_sign_of_a_pre_epoch_clock() {
        use std::time::{Duration, UNIX_EPOCH};

        assert_eq!(unix_seconds(UNIX_EPOCH), 0);
        assert_eq!(
            unix_seconds(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            1_700_000_000
        );
        // A clock standing before 1970 used to be flattened onto the epoch by
        // `unwrap_or(0)`, which made every message look 55 years old to the
        // recency scorer instead of brand new.
        assert_eq!(
            unix_seconds(UNIX_EPOCH - Duration::from_secs(86_400)),
            -86_400
        );
    }

    #[test]
    fn recorded_messages_carry_a_plausible_timestamp() {
        let config = MemoryConfig::default().with_enabled(true);
        let mut manager = ConversationMemoryManager::new(config);

        manager.record_user_message("Q");
        let messages = manager.take_pending_messages();

        // 2020-01-01: the only thing worth asserting on a real clock is that
        // it is not the silent epoch-zero fallback.
        assert!(
            messages[0].created_at > 1_577_836_800,
            "created_at fell back to the epoch: {}",
            messages[0].created_at
        );
    }

    // -----------------------------------------------------------------------
    // Builder
    // -----------------------------------------------------------------------

    #[test]
    fn builder_carries_url_key_and_token_budget_into_the_config() {
        let manager = MemoryIntegrationBuilder::default()
            .url("http://127.0.0.1:7700")
            .key("cle-factice")
            .token_budget(123)
            .build();

        let config = manager.config();
        assert_eq!(config.meilisearch_url, "http://127.0.0.1:7700");
        assert_eq!(config.meilisearch_key.as_deref(), Some("cle-factice"));
        assert_eq!(config.token_budget, 123);

        // Neither a conversation id nor a cwd was given: `build` leaves the
        // generated id in place and no cwd at all.
        assert!(manager.conversation_id().starts_with("conv-"));
        assert_eq!(manager.cwd(), None);
    }

    #[test]
    fn a_disabled_manager_ignores_tool_calls() {
        let mut manager = MemoryIntegrationBuilder::new().enabled(false).build();

        manager.process_tool_call("Bash", &serde_json::json!({"command": "cd /ailleurs"}));

        assert!(!manager.is_enabled());
        assert_eq!(manager.cwd(), None, "a disabled manager must not track cwd");
        assert!(manager.current_context("requête").files.is_empty());
    }

    #[test]
    fn is_resumed_is_true_for_a_brand_new_conversation_after_one_turn() {
        let config = MemoryConfig::default().with_enabled(true);
        let mut manager = ConversationMemoryManager::new(config);
        assert!(!manager.is_resumed());

        manager.record_user_message("Q1");
        manager.record_assistant_message("A1");

        // Nothing was resumed: `resume_conversation` was never called and the
        // conversation id is still the one generated by `new`. `is_resumed()`
        // only looks at `turn_index > 0`, so it answers "yes" as soon as a
        // first turn completes — the name promises more than the code does.
        assert!(manager.conversation_id().starts_with("conv-"));
        assert!(
            manager.is_resumed(),
            "documents the false positive of is_resumed()"
        );
    }

    // -----------------------------------------------------------------------
    // SummaryGenerator
    // -----------------------------------------------------------------------

    #[test]
    fn default_threshold_counts_bytes_not_characters() {
        let generator = SummaryGenerator::default_threshold();

        assert!(!generator.needs_summary(&"x".repeat(500)));
        assert!(generator.needs_summary(&"x".repeat(501)));
        // The doc-comment advertises "500 chars" but the comparison is on
        // `str::len()`, i.e. bytes: 251 two-byte characters already trip it.
        assert!(generator.needs_summary(&"é".repeat(251)));
        assert!(!generator.needs_summary(&"é".repeat(250)));
    }

    #[test]
    fn summary_of_delimiters_only_stops_on_a_char_boundary() {
        // `split(['.', '!', '?'])` yields nothing but blanks here — `trim()`
        // eats the em space — so the `0 =>` fallback runs. Threshold 2 lands
        // inside the em space (bytes 1..4), which used to panic with
        // "byte index 2 is not a char boundary".
        let generator = SummaryGenerator::new(2);
        assert!(generator.needs_summary(".\u{2003}."));

        assert_eq!(
            generator.generate_simple_summary(".\u{2003}."),
            "....",
            "the cut must fall back to byte 1, keeping the leading '.'"
        );
    }

    #[test]
    fn summary_without_sentences_keeps_the_byte_prefix() {
        let generator = SummaryGenerator::new(2);

        // All-ASCII: the threshold is already a boundary, nothing moves.
        assert_eq!(generator.generate_simple_summary("!!!!!!"), "!!...");
        // A no-break space is whitespace too, so still zero sentences; the
        // threshold lands inside its two bytes and walks back to 1.
        assert_eq!(generator.generate_simple_summary("!\u{00A0}?"), "!...");
    }

    #[test]
    fn summary_of_a_single_sentence_returns_it_untouched() {
        let generator = SummaryGenerator::new(10);
        let content = "une seule phrase sans ponctuation finale";
        assert!(generator.needs_summary(content));

        // The `1 =>` arm hands back the whole sentence: no shortening, no
        // ellipsis. A content above the threshold can come out unsummarised.
        assert_eq!(generator.generate_simple_summary(content), content);
    }

    #[test]
    fn summary_of_two_sentences_rewrites_the_first_terminator() {
        let generator = SummaryGenerator::new(10);

        // Both original terminators are lost: the first is replaced by a '.',
        // the second disappears with the trailing empty split.
        assert_eq!(
            generator.generate_simple_summary("Première ! Seconde ?"),
            "Première. ... Seconde"
        );
    }

    #[test]
    fn short_content_is_returned_verbatim() {
        let generator = SummaryGenerator::new(500);

        assert_eq!(generator.generate_simple_summary("Court."), "Court.");
    }

    /// A config that depends on nothing but the mock server's address, so the
    /// `MEILISEARCH_URL` / `MEILISEARCH_KEY` environment variables cannot leak
    /// into the test.
    #[cfg(feature = "memory")]
    fn mock_config(url: &str) -> MemoryConfig {
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

    /// `ContextInjector::new` goes through `MeilisearchMemoryProvider::new`,
    /// which refuses a disabled config with `MemoryError::Disabled`. The two
    /// `if !self.config.enabled` guards inside `get_context_prefix` and
    /// `store_messages` are therefore unreachable through the only public
    /// constructor; the struct is assembled by hand here to pin what they
    /// promise — a silent short-circuit, with no request at all.
    #[cfg(feature = "memory")]
    #[tokio::test]
    async fn a_disabled_injector_short_circuits_without_calling_the_server() {
        use wiremock::matchers::{method, path, path_regex};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let task = serde_json::json!({
            "taskUid": 0,
            "indexUid": "nexus_messages",
            "status": "enqueued",
            "type": "indexCreation",
            "enqueuedAt": "2026-10-01T00:00:00Z",
        });
        Mock::given(method("POST"))
            .and(path("/indexes"))
            .respond_with(ResponseTemplate::new(202).set_body_json(task.clone()))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path_regex(r"^/indexes/[^/]+/settings$"))
            .respond_with(ResponseTemplate::new(202).set_body_json(task))
            .mount(&server)
            .await;

        let provider = MeilisearchMemoryProvider::new(mock_config(&server.uri()))
            .await
            .expect("the mock server answers the bootstrap");
        // Drops the bootstrap mocks and the request log: from here on any
        // outgoing request would both fail and be visible.
        server.reset().await;

        let injector = ContextInjector {
            provider: Arc::new(provider),
            config: mock_config(&server.uri()).with_enabled(false),
        };

        assert_eq!(
            injector
                .get_context_prefix("jwt", Some("/w"), &["/w/a.rs".to_string()])
                .await
                .unwrap(),
            None,
            "a disabled injector injects nothing"
        );
        injector
            .store_messages(&[MessageDocument::new(
                "m-1",
                "conv-1",
                "user",
                "bonjour",
                0,
                1_700_000_000,
            )])
            .await
            .expect("a disabled injector swallows the write silently");

        assert!(
            server
                .received_requests()
                .await
                .expect("the mock server records every request")
                .is_empty(),
            "neither guard may reach the network"
        );
    }
}

#[cfg(all(test, feature = "memory"))]
mod memory_tests {
    use super::*;

    #[test]
    fn test_loaded_conversation_basic() {
        let messages = vec![
            MessageDocument::new("msg-3", "conv-1", "assistant", "Reply 2", 2, 1700002000),
            MessageDocument::new("msg-2", "conv-1", "user", "Question 2", 1, 1700001000),
            MessageDocument::new("msg-1", "conv-1", "user", "Question 1", 0, 1700000000),
        ];

        let loaded = LoadedConversation {
            messages,
            total_count: 5,
            has_more: true,
            offset: 0,
            limit: 3,
        };

        assert_eq!(loaded.len(), 3);
        assert!(!loaded.is_empty());
        assert!(loaded.has_more);
        assert_eq!(loaded.total_count, 5);
    }

    #[test]
    fn test_loaded_conversation_empty() {
        let loaded = LoadedConversation {
            messages: vec![],
            total_count: 0,
            has_more: false,
            offset: 0,
            limit: 50,
        };

        assert!(loaded.is_empty());
        assert_eq!(loaded.len(), 0);
        assert!(!loaded.has_more);
        assert_eq!(loaded.max_turn_index(), None);
    }

    #[test]
    fn test_loaded_conversation_next_offset() {
        let messages = vec![
            MessageDocument::new("msg-1", "conv-1", "user", "Q1", 0, 1700000000),
            MessageDocument::new("msg-2", "conv-1", "assistant", "A1", 1, 1700001000),
        ];

        let loaded = LoadedConversation {
            messages,
            total_count: 10,
            has_more: true,
            offset: 2,
            limit: 2,
        };

        assert_eq!(loaded.next_offset(), 4); // offset (2) + len (2)
    }

    #[test]
    fn test_loaded_conversation_max_turn_index() {
        let messages = vec![
            MessageDocument::new("msg-3", "conv-1", "assistant", "A2", 4, 1700003000),
            MessageDocument::new("msg-2", "conv-1", "user", "Q2", 3, 1700002000),
            MessageDocument::new("msg-1", "conv-1", "assistant", "A1", 2, 1700001000),
        ];

        let loaded = LoadedConversation {
            messages,
            total_count: 5,
            has_more: true,
            offset: 0,
            limit: 3,
        };

        assert_eq!(loaded.max_turn_index(), Some(4));
    }

    #[test]
    fn test_loaded_conversation_chronological_order() {
        let messages = vec![
            MessageDocument::new("msg-3", "conv-1", "assistant", "Latest", 2, 1700002000),
            MessageDocument::new("msg-2", "conv-1", "user", "Middle", 1, 1700001000),
            MessageDocument::new("msg-1", "conv-1", "user", "Oldest", 0, 1700000000),
        ];

        let loaded = LoadedConversation {
            messages,
            total_count: 3,
            has_more: false,
            offset: 0,
            limit: 50,
        };

        let chronological = loaded.messages_chronological();
        assert_eq!(chronological.len(), 3);
        assert_eq!(chronological[0].content, "Oldest");
        assert_eq!(chronological[1].content, "Middle");
        assert_eq!(chronological[2].content, "Latest");
    }

    #[test]
    fn test_resume_and_continue_workflow() {
        // This test simulates the full workflow of:
        // 1. Creating a loaded conversation (as if from injector.load_conversation)
        // 2. Resuming the manager
        // 3. Continuing to record messages

        let config = MemoryConfig::default().with_enabled(true);
        let mut manager = ConversationMemoryManager::new(config);

        // Simulate loaded conversation with messages at turns 0, 1, 2
        let loaded = LoadedConversation {
            messages: vec![
                MessageDocument::new("msg-3", "conv-xyz", "assistant", "A2", 2, 1700002000)
                    .with_cwd("/projects/app"),
                MessageDocument::new("msg-2", "conv-xyz", "user", "Q2", 1, 1700001000)
                    .with_cwd("/projects/app"),
                MessageDocument::new("msg-1", "conv-xyz", "user", "Q1", 0, 1700000000)
                    .with_cwd("/projects/app"),
            ],
            total_count: 3,
            has_more: false,
            offset: 0,
            limit: 50,
        };

        // Calculate next turn index
        let next_turn = loaded.max_turn_index().map(|i| i + 1).unwrap_or(0);
        assert_eq!(next_turn, 3);

        // Get CWD from most recent message
        let cwd = loaded.messages.first().and_then(|m| m.cwd.as_deref());

        // Resume the conversation
        manager.resume_conversation("conv-xyz", cwd, next_turn);

        assert_eq!(manager.conversation_id(), "conv-xyz");
        assert_eq!(manager.cwd(), Some("/projects/app"));
        assert_eq!(manager.turn_index(), 3);
        assert!(manager.is_resumed());

        // Continue the conversation
        manager.record_user_message("Q3");
        manager.record_assistant_message("A3");

        let new_messages = manager.take_pending_messages();
        assert_eq!(new_messages.len(), 2);
        assert_eq!(new_messages[0].turn_index, 3);
        assert_eq!(new_messages[1].turn_index, 3);
        assert_eq!(manager.turn_index(), 4);
    }
}
