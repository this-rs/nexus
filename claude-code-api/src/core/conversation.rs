use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use tracing::info;

use crate::core::storage::{ConversationStore, InMemoryConversationStore};
use crate::models::claude::ClaudeModel;
use crate::models::openai::{ChatMessage, MessageContent};

/// Error type for context preparation failures
#[derive(Debug)]
pub enum ContextError {
    /// A single message exceeds the model's context window
    SingleMessageTooLong {
        estimated_tokens: usize,
        max_tokens: usize,
    },
}

impl fmt::Display for ContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContextError::SingleMessageTooLong {
                estimated_tokens,
                max_tokens,
            } => write!(
                f,
                "The last message is too long (~{} tokens estimated). \
                 Maximum context length for this model is {} tokens.",
                estimated_tokens, max_tokens
            ),
        }
    }
}

/// Type alias for the default ConversationManager using in-memory storage
pub type DefaultConversationManager = ConversationManager<InMemoryConversationStore>;

/// Configuration for the conversation manager
#[derive(Clone)]
pub struct ConversationConfig {
    /// Legacy field — kept for backward compatibility. The actual token limit
    /// is now determined dynamically by `ClaudeModel::context_window_for_model()`.
    #[allow(dead_code)]
    pub max_context_tokens: usize,
    pub session_timeout_minutes: i64,
}

impl Default for ConversationConfig {
    fn default() -> Self {
        Self {
            max_context_tokens: 100000,
            session_timeout_minutes: 30,
        }
    }
}

/// A conversation with its messages and metadata
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub messages: Vec<ChatMessage>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub metadata: ConversationMetadata,
}

/// Metadata associated with a conversation
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ConversationMetadata {
    pub model: Option<String>,
    pub total_tokens: usize,
    pub turn_count: usize,
    pub project_path: Option<String>,
}

/// Manager for conversations that delegates storage to a ConversationStore implementation
#[derive(Clone)]
pub struct ConversationManager<S: ConversationStore> {
    store: Arc<S>,
    config: ConversationConfig,
}

impl<S: ConversationStore + 'static> ConversationManager<S> {
    /// Create a new ConversationManager with the given store and config
    pub fn new(store: S, config: ConversationConfig) -> Self {
        let manager = Self {
            store: Arc::new(store),
            config,
        };

        // Start cleanup task
        let store_clone = manager.store.clone();
        let timeout = manager.config.session_timeout_minutes;
        tokio::spawn(async move {
            Self::cleanup_loop(store_clone, timeout).await;
        });

        manager
    }

    /// Create a new conversation and return its ID
    pub async fn create_conversation(&self, model: Option<String>) -> Result<String> {
        self.store.create(model).await
    }

    /// Add a message to a conversation
    pub async fn add_message(&self, conversation_id: &str, message: ChatMessage) -> Result<()> {
        self.store.add_message(conversation_id, message).await
    }

    /// Get a conversation by ID
    pub async fn get_conversation(&self, conversation_id: &str) -> Option<Conversation> {
        self.store.get(conversation_id).await.ok().flatten()
    }

    /// Get context messages for a conversation, including new messages.
    /// Uses the model's context window (with safety factor) to determine limits.
    /// Returns an error if the last user message alone exceeds the context window.
    pub async fn get_context_messages(
        &self,
        conversation_id: &str,
        new_messages: &[ChatMessage],
        model_id: &str,
    ) -> std::result::Result<Vec<ChatMessage>, ContextError> {
        if let Some(conversation) = self.get_conversation(conversation_id).await {
            let mut context = conversation.messages;
            context.extend_from_slice(new_messages);
            self.trim_context(context, model_id)
        } else {
            self.trim_context(new_messages.to_vec(), model_id)
        }
    }

    /// Trim context to fit within token limits for the given model.
    /// Uses ClaudeModel::context_window_for_model() with a 90% safety factor.
    fn trim_context(
        &self,
        messages: Vec<ChatMessage>,
        model_id: &str,
    ) -> std::result::Result<Vec<ChatMessage>, ContextError> {
        let max_tokens = ClaudeModel::context_window_for_model(model_id);

        let mut system_messages = Vec::new();
        let mut other_messages = Vec::new();

        for msg in messages {
            if msg.role == "system" {
                system_messages.push(msg);
            } else {
                other_messages.push(msg);
            }
        }

        let mut result = system_messages;
        let mut token_count = estimate_tokens(&result);

        // Validate: if the last message alone exceeds the limit, we can't trim it
        if let Some(last_msg) = other_messages.last() {
            let last_msg_tokens = estimate_tokens(std::slice::from_ref(last_msg));
            let system_tokens = token_count;
            if system_tokens + last_msg_tokens > max_tokens {
                return Err(ContextError::SingleMessageTooLong {
                    estimated_tokens: last_msg_tokens,
                    max_tokens,
                });
            }
        }

        // Add messages from newest to oldest
        for msg in other_messages.into_iter().rev() {
            let msg_tokens = estimate_tokens(std::slice::from_ref(&msg));
            if token_count + msg_tokens > max_tokens {
                break;
            }
            result.push(msg);
            token_count += msg_tokens;
        }

        // Restore correct order
        if result.len() > 1 {
            let system_count = result.iter().filter(|m| m.role == "system").count();
            result[system_count..].reverse();
        }

        Ok(result)
    }

    /// Update conversation metadata
    pub async fn update_metadata(
        &self,
        conversation_id: &str,
        update_fn: impl FnOnce(&mut ConversationMetadata),
    ) -> Result<()> {
        if let Some(mut conversation) = self.get_conversation(conversation_id).await {
            update_fn(&mut conversation.metadata);
            self.store
                .update_metadata(conversation_id, conversation.metadata)
                .await
        } else {
            Err(anyhow::anyhow!("Conversation not found"))
        }
    }

    /// List all active conversations with their last update time
    pub async fn list_active_conversations(&self) -> Vec<(String, DateTime<Utc>)> {
        self.store.list_active().await.unwrap_or_default()
    }

    /// Background cleanup loop
    async fn cleanup_loop(store: Arc<S>, timeout_minutes: i64) {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(300)).await;

            match store.cleanup_expired(timeout_minutes).await {
                Ok(count) if count > 0 => {
                    info!("Cleaned up {} expired conversations", count);
                },
                Err(e) => {
                    tracing::error!("Failed to cleanup expired conversations: {}", e);
                },
                _ => {},
            }
        }
    }
}

/// Estimate token count for a slice of messages.
/// Uses character count (Unicode-aware) with a conservative ratio of 0.3 tokens/char
/// which better approximates real tokenizer behavior for code and multilingual text.
/// For content arrays (images etc.), estimates 1600 tokens per item (one JPEG tile).
fn estimate_tokens(msgs: &[ChatMessage]) -> usize {
    msgs.iter()
        .map(|m| match &m.content {
            Some(MessageContent::Text(text)) => {
                // Use chars().count() for proper UTF-8 handling
                // Ratio of ~0.3 tokens per character is more conservative than len()/4
                (text.chars().count() as f64 * 0.3).ceil() as usize
            },
            Some(MessageContent::Array(parts)) => {
                // Estimate 1600 tokens per content part (one image tile)
                parts.len() * 1600
            },
            None => 50,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_text_msg(role: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: Some(MessageContent::Text(text.to_string())),
            name: None,
            tool_calls: None,
        }
    }

    fn make_manager() -> ConversationManager<InMemoryConversationStore> {
        ConversationManager {
            store: Arc::new(InMemoryConversationStore::default()),
            config: ConversationConfig::default(),
        }
    }

    // ── estimate_tokens tests ──────────────────────────────────────────

    #[test]
    fn test_estimate_tokens_ascii() {
        // 100 ASCII chars → 100 * 0.3 = 30 tokens
        let msgs = vec![make_text_msg("user", &"a".repeat(100))];
        assert_eq!(estimate_tokens(&msgs), 30);
    }

    #[test]
    fn test_estimate_tokens_utf8_cjk() {
        // 10 CJK characters: chars().count() = 10, but len() = 30 (3 bytes each)
        // With old formula: 30/4 = 7. With new: ceil(10*0.3) = 3
        let msgs = vec![make_text_msg("user", "你好世界测试中文字符")];
        let tokens = estimate_tokens(&msgs);
        assert_eq!(tokens, 3); // ceil(10 * 0.3)
    }

    #[test]
    fn test_estimate_tokens_emoji() {
        // Emojis: 5 emoji chars, each 4 bytes → len()=20, chars()=5
        // Old: 20/4=5. New: ceil(5*0.3) = 2
        let msgs = vec![make_text_msg("user", "😀🎉🚀💻🔥")];
        let tokens = estimate_tokens(&msgs);
        assert_eq!(tokens, 2); // ceil(5 * 0.3)
    }

    #[test]
    fn test_estimate_tokens_none_content() {
        let msgs = vec![ChatMessage {
            role: "assistant".to_string(),
            content: None,
            name: None,
            tool_calls: None,
        }];
        assert_eq!(estimate_tokens(&msgs), 50);
    }

    #[test]
    fn test_estimate_tokens_image_array() {
        use crate::models::openai::{ContentPart, ImageUrl};
        let msgs = vec![ChatMessage {
            role: "user".to_string(),
            content: Some(MessageContent::Array(vec![
                ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: "data:image/png;base64,abc".to_string(),
                        detail: None,
                    },
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: "data:image/png;base64,def".to_string(),
                        detail: None,
                    },
                },
            ])),
            name: None,
            tool_calls: None,
        }];
        assert_eq!(estimate_tokens(&msgs), 3200); // 2 * 1600
    }

    // ── trim_context tests ─────────────────────────────────────────────

    #[test]
    fn test_trim_context_within_limits() {
        let manager = make_manager();
        let msgs = vec![
            make_text_msg("system", "You are a helpful assistant."),
            make_text_msg("user", "Hello"),
            make_text_msg("assistant", "Hi there!"),
            make_text_msg("user", "How are you?"),
        ];

        let result = manager
            .trim_context(msgs.clone(), "claude-sonnet-4-20250514")
            .unwrap();
        // All messages should fit within 450K token limit
        assert_eq!(result.len(), 4);
        assert_eq!(result[0].role, "system");
        assert_eq!(result[1].role, "user");
    }

    #[test]
    fn test_trim_context_preserves_order() {
        let manager = make_manager();
        let msgs = vec![
            make_text_msg("user", "First"),
            make_text_msg("assistant", "Response 1"),
            make_text_msg("user", "Second"),
            make_text_msg("assistant", "Response 2"),
            make_text_msg("user", "Third"),
        ];

        let result = manager
            .trim_context(msgs, "claude-sonnet-4-20250514")
            .unwrap();
        // Verify correct chronological order
        assert_eq!(result.len(), 5);
        assert!(
            result[0]
                .content
                .as_ref()
                .unwrap()
                .to_string()
                .contains("First")
        );
        assert!(
            result[4]
                .content
                .as_ref()
                .unwrap()
                .to_string()
                .contains("Third")
        );
    }

    #[test]
    fn test_trim_context_single_message_too_long() {
        let manager = make_manager();
        // Create a message that would exceed even the largest model's window
        // 450K tokens at 0.3 tokens/char ≈ 1.5M chars needed
        let huge_text = "x".repeat(2_000_000);
        let msgs = vec![make_text_msg("user", &huge_text)];

        let result = manager.trim_context(msgs, "claude-sonnet-4-20250514");
        assert!(result.is_err());
        match result.unwrap_err() {
            ContextError::SingleMessageTooLong {
                estimated_tokens,
                max_tokens,
            } => {
                assert_eq!(max_tokens, 450_000);
                assert!(estimated_tokens > 450_000);
            },
        }
    }

    #[test]
    fn test_trim_context_drops_oldest_non_system() {
        let manager = make_manager();
        // Unknown model → fallback 200K * 0.9 = 180K tokens
        // At 0.3 tokens/char, we need > 180K tokens ≈ > 600K chars to fill the window
        // Two messages of 550K chars each (~165K tokens) can't both fit
        let big_msg_old = "y".repeat(550_000); // ~165K tokens — oldest
        let big_msg_new = "z".repeat(550_000); // ~165K tokens — newest
        let msgs = vec![
            make_text_msg("user", &big_msg_old), // ~165K tokens — oldest, should be dropped
            make_text_msg("user", &big_msg_new), // ~165K tokens — newest, must be kept
        ];

        let result = manager.trim_context(msgs, "unknown-small-model").unwrap();
        // Only the newest message should remain (the old one gets dropped)
        assert_eq!(result.len(), 1);
        // The kept message should be the newest (z's, not y's)
        if let Some(MessageContent::Text(ref text)) = result[0].content {
            assert!(text.starts_with('z'));
        } else {
            panic!("Expected text content");
        }
    }

    #[test]
    fn test_trim_context_keeps_system_messages() {
        let manager = make_manager();
        let big_msg = "z".repeat(500_000);
        let msgs = vec![
            make_text_msg("system", "Important system prompt"),
            make_text_msg("user", &big_msg), // big old message
            make_text_msg("user", "latest"), // small recent message
        ];

        let result = manager.trim_context(msgs, "unknown-small-model").unwrap();
        // System message should always be present
        assert_eq!(result[0].role, "system");
        assert!(
            result[0]
                .content
                .as_ref()
                .unwrap()
                .to_string()
                .contains("Important")
        );
    }

    #[test]
    fn test_trim_context_uses_model_specific_limit() {
        let manager = make_manager();
        // Claude 4 has 500K window → 450K after safety factor
        // Claude 3 has 200K window → 180K after safety factor
        // A message of ~200K tokens should fit in Claude 4 but not Claude 3
        let medium_msg = "m".repeat(650_000); // ~195K tokens

        let result_c4 = manager.trim_context(
            vec![make_text_msg("user", &medium_msg)],
            "claude-sonnet-4-20250514",
        );
        assert!(result_c4.is_ok(), "Should fit in Claude 4 (450K limit)");

        let result_c3 = manager.trim_context(
            vec![make_text_msg("user", &medium_msg)],
            "claude-3-7-sonnet-20250219",
        );
        assert!(
            result_c3.is_err(),
            "Should NOT fit in Claude 3.7 (180K limit)"
        );
    }

    // Helper for MessageContent display
    impl std::fmt::Display for MessageContent {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                MessageContent::Text(t) => write!(f, "{}", t),
                MessageContent::Array(parts) => write!(f, "[{} parts]", parts.len()),
            }
        }
    }
}
