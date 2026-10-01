//! Meilisearch integration for semantic search
//!
//! This module provides Meilisearch-backed search capabilities for conversations.
//! Index names are prefixed with "nexus_" to avoid conflicts with other applications.

#![allow(dead_code)] // Public API - may not be used internally
//!
//! ## Indexes
//!
//! - `nexus_messages`: Full-text search on message content
//!   - Searchable: content, role
//!   - Filterable: conversation_id, role, created_at
//!   - Sortable: created_at

use anyhow::Result;
use meilisearch_sdk::client::Client;
use meilisearch_sdk::indexes::Index;
use meilisearch_sdk::settings::Settings;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

/// Meilisearch index names (prefixed to avoid conflicts)
pub const INDEX_MESSAGES: &str = "nexus_messages";
pub const INDEX_CONVERSATIONS: &str = "nexus_conversations";

/// Configuration for Meilisearch connection
#[derive(Clone, Debug)]
pub struct MeilisearchConfig {
    pub url: String,
    pub api_key: Option<String>,
}

impl Default for MeilisearchConfig {
    fn default() -> Self {
        Self {
            url: std::env::var("MEILISEARCH_URL")
                .unwrap_or_else(|_| "http://localhost:7700".to_string()),
            api_key: std::env::var("MEILISEARCH_KEY").ok(),
        }
    }
}

/// Document structure for indexed messages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDocument {
    pub id: String,
    pub conversation_id: String,
    pub role: String,
    pub content: String,
    pub turn_index: usize,
    pub created_at: i64, // Unix timestamp for sorting
}

/// Document structure for indexed conversations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationDocument {
    pub id: String,
    pub model: Option<String>,
    pub message_count: usize,
    pub total_tokens: usize,
    pub created_at: i64,
    pub updated_at: i64,
    /// Concatenated preview of conversation content for search
    pub content_preview: String,
}

/// Escape a value so it can be embedded in a double-quoted Meilisearch filter.
///
/// Filter values reach this module straight from the HTTP layer (a
/// `conversation_id` chosen by the caller). Interpolating one raw into
/// `conversation_id = "..."` lets a `"` close the string literal and the rest of
/// the value be parsed as filter syntax — which `delete_conversation_messages`
/// would then turn into deletions of other conversations' messages.
///
/// Meilisearch's filter parser accepts `\\` and `\"` inside a quoted value, so
/// escaping both keeps the expression a plain equality test. Values without a
/// backslash or a double quote — every id this crate generates — are unchanged.
fn escape_filter_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Meilisearch client wrapper for Nexus
#[derive(Clone)]
pub struct MeilisearchClient {
    client: Client,
}

impl MeilisearchClient {
    /// Create a new Meilisearch client
    pub async fn new(config: MeilisearchConfig) -> Result<Self> {
        info!("Connecting to Meilisearch at {}", config.url);

        let client = Client::new(&config.url, config.api_key.as_deref())?;

        let ms = Self { client };

        // Initialize indexes
        ms.init_indexes().await?;

        info!("Connected to Meilisearch successfully");
        Ok(ms)
    }

    /// Initialize Meilisearch indexes with proper settings
    async fn init_indexes(&self) -> Result<()> {
        // Create messages index
        self.client
            .create_index(INDEX_MESSAGES, Some("id"))
            .await
            .ok(); // Ignore if exists

        let messages_index = self.client.index(INDEX_MESSAGES);
        let messages_settings = Settings::new()
            .with_searchable_attributes(["content", "role"])
            .with_filterable_attributes(["conversation_id", "role", "created_at"])
            .with_sortable_attributes(["created_at", "turn_index"]);

        messages_index.set_settings(&messages_settings).await?;

        // Create conversations index
        self.client
            .create_index(INDEX_CONVERSATIONS, Some("id"))
            .await
            .ok(); // Ignore if exists

        let conversations_index = self.client.index(INDEX_CONVERSATIONS);
        let conversations_settings = Settings::new()
            .with_searchable_attributes(["content_preview", "model"])
            .with_filterable_attributes(["model", "created_at", "updated_at"])
            .with_sortable_attributes(["created_at", "updated_at", "message_count"]);

        conversations_index
            .set_settings(&conversations_settings)
            .await?;

        info!("Meilisearch indexes initialized for Nexus");
        Ok(())
    }

    /// Get the messages index
    pub fn messages_index(&self) -> Index {
        self.client.index(INDEX_MESSAGES)
    }

    /// Get the conversations index
    pub fn conversations_index(&self) -> Index {
        self.client.index(INDEX_CONVERSATIONS)
    }

    /// Index a message for search
    pub async fn index_message(&self, doc: MessageDocument) -> Result<()> {
        let index = self.messages_index();
        index.add_documents(&[doc], Some("id")).await?;
        debug!("Indexed message");
        Ok(())
    }

    /// Index multiple messages
    pub async fn index_messages(&self, docs: Vec<MessageDocument>) -> Result<()> {
        if docs.is_empty() {
            return Ok(());
        }

        let index = self.messages_index();
        index.add_documents(&docs, Some("id")).await?;
        debug!("Indexed {} messages", docs.len());
        Ok(())
    }

    /// Index a conversation for search
    pub async fn index_conversation(&self, doc: ConversationDocument) -> Result<()> {
        let index = self.conversations_index();
        let id = doc.id.clone();
        index.add_documents(&[doc], Some("id")).await?;
        debug!("Indexed conversation {}", id);
        Ok(())
    }

    /// Search messages by content
    ///
    /// `conversation_id` is escaped before it enters the filter expression, so a
    /// caller-supplied id cannot widen the filter (see [`escape_filter_value`]).
    pub async fn search_messages(
        &self,
        query: &str,
        conversation_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MessageDocument>> {
        let index = self.messages_index();

        let filter =
            conversation_id.map(|id| format!("conversation_id = \"{}\"", escape_filter_value(id)));

        let mut search = index.search();
        search.with_query(query).with_limit(limit);

        if let Some(ref f) = filter {
            search.with_filter(f);
        }

        let results = search.execute::<MessageDocument>().await?;

        Ok(results.hits.into_iter().map(|h| h.result).collect())
    }

    /// Search conversations by content preview
    pub async fn search_conversations(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ConversationDocument>> {
        let index = self.conversations_index();

        let results = index
            .search()
            .with_query(query)
            .with_limit(limit)
            .execute::<ConversationDocument>()
            .await?;

        Ok(results.hits.into_iter().map(|h| h.result).collect())
    }

    /// Delete a message from the index
    pub async fn delete_message(&self, message_id: &str) -> Result<()> {
        let index = self.messages_index();
        index.delete_document(message_id).await?;
        Ok(())
    }

    /// Delete all messages for a conversation
    pub async fn delete_conversation_messages(&self, conversation_id: &str) -> Result<()> {
        // Search for all messages in this conversation and delete them one by one
        // Note: Meilisearch SDK v0.27 doesn't have filter-based deletion
        let messages = self
            .search_messages("", Some(conversation_id), 1000)
            .await?;

        let index = self.messages_index();
        for msg in messages {
            let _ = index.delete_document(&msg.id).await;
        }

        Ok(())
    }

    /// Delete a conversation from the index
    pub async fn delete_conversation(&self, conversation_id: &str) -> Result<()> {
        let index = self.conversations_index();
        index.delete_document(conversation_id).await?;

        // Also delete all messages
        self.delete_conversation_messages(conversation_id).await?;

        Ok(())
    }

    /// Get index statistics
    pub async fn get_stats(&self) -> Result<MeilisearchStats> {
        let messages_stats = self.messages_index().get_stats().await?;
        let conversations_stats = self.conversations_index().get_stats().await?;

        Ok(MeilisearchStats {
            messages_count: messages_stats.number_of_documents,
            conversations_count: conversations_stats.number_of_documents,
            is_indexing: messages_stats.is_indexing || conversations_stats.is_indexing,
        })
    }
}

/// Statistics for Nexus Meilisearch indexes
#[derive(Debug, Clone, Serialize)]
pub struct MeilisearchStats {
    pub messages_count: usize,
    pub conversations_count: usize,
    pub is_indexing: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An id that cannot close the quoted literal passes through untouched: the
    /// escaping must not change the filter for any id this crate produces.
    #[test]
    fn escape_filter_value_leaves_ordinary_ids_alone() {
        for id in [
            "conv-1",
            "550e8400-e29b-41d4-a716-446655440000",
            "conv with spaces",
            "conv'avec-apostrophe",
        ] {
            assert_eq!(escape_filter_value(id), id, "id {id:?} must be unchanged");
        }
    }

    /// Both characters that can escape out of a double-quoted value are doubled,
    /// and the backslash is handled first so an escape is never re-escaped.
    #[test]
    fn escape_filter_value_neutralises_quotes_and_backslashes() {
        assert_eq!(
            escape_filter_value(r#"x" OR role = "user"#),
            r#"x\" OR role = \"user"#
        );
        assert_eq!(escape_filter_value(r"conv\"), r"conv\\");
        // A value that already looks escaped must come out double-escaped, not
        // collapsed: `\"` is two characters, both of which need escaping.
        assert_eq!(escape_filter_value(r#"\""#), r#"\\\""#);
    }

    #[tokio::test]
    #[ignore]
    async fn test_meilisearch_connection() {
        let config = MeilisearchConfig::default();
        let client = MeilisearchClient::new(config).await.unwrap();

        let stats = client.get_stats().await.unwrap();
        println!("Stats: {:?}", stats);
    }

    #[tokio::test]
    #[ignore]
    async fn test_index_and_search_message() {
        let config = MeilisearchConfig::default();
        let client = MeilisearchClient::new(config).await.unwrap();

        let doc = MessageDocument {
            id: "test-msg-1".to_string(),
            conversation_id: "test-conv-1".to_string(),
            role: "user".to_string(),
            content: "Hello, how can I help you today?".to_string(),
            turn_index: 0,
            created_at: chrono::Utc::now().timestamp(),
        };

        client.index_message(doc).await.unwrap();

        // Wait for indexing
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let results = client.search_messages("help", None, 10).await.unwrap();
        assert!(!results.is_empty());

        // Cleanup
        client.delete_message("test-msg-1").await.unwrap();
    }
}
