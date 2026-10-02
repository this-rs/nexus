//! In-memory storage implementations
//!
//! These implementations store data in memory using thread-safe data structures.
//! Data is lost when the process exits.

#![allow(dead_code)] // Public API - may not be used internally

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{debug, info};
use uuid::Uuid;

use crate::core::cache::CacheStats;
use crate::core::conversation::{Conversation, ConversationMetadata};
use crate::core::session_manager::Session;
use crate::models::openai::{ChatCompletionResponse, ChatMessage};

use super::traits::{CacheStore, ConversationStore, SessionStore};

/// Configuration for in-memory conversation storage
#[derive(Clone)]
pub struct InMemoryConversationConfig {
    pub max_history_messages: usize,
}

impl Default for InMemoryConversationConfig {
    fn default() -> Self {
        Self {
            max_history_messages: 20,
        }
    }
}

/// In-memory implementation of ConversationStore
///
/// Uses a HashMap protected by a RwLock for thread-safe access.
/// Suitable for development and single-instance deployments.
pub struct InMemoryConversationStore {
    conversations: RwLock<HashMap<String, Conversation>>,
    config: InMemoryConversationConfig,
}

impl InMemoryConversationStore {
    pub fn new(config: InMemoryConversationConfig) -> Self {
        Self {
            conversations: RwLock::new(HashMap::new()),
            config,
        }
    }
}

impl Default for InMemoryConversationStore {
    fn default() -> Self {
        Self::new(InMemoryConversationConfig::default())
    }
}

#[async_trait]
impl ConversationStore for InMemoryConversationStore {
    async fn create(&self, model: Option<String>) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();

        let conversation = Conversation {
            id: id.clone(),
            messages: Vec::new(),
            created_at: now,
            updated_at: now,
            metadata: ConversationMetadata {
                model,
                ..Default::default()
            },
        };

        self.conversations.write().insert(id.clone(), conversation);
        info!("Created new conversation: {}", id);

        Ok(id)
    }

    async fn get(&self, id: &str) -> Result<Option<Conversation>> {
        Ok(self.conversations.read().get(id).cloned())
    }

    async fn add_message(&self, id: &str, message: ChatMessage) -> Result<()> {
        let mut conversations = self.conversations.write();

        if let Some(conversation) = conversations.get_mut(id) {
            conversation.messages.push(message);
            conversation.updated_at = Utc::now();
            conversation.metadata.turn_count += 1;

            // Trim old messages if exceeding limit
            if conversation.messages.len() > self.config.max_history_messages {
                let remove_count = conversation.messages.len() - self.config.max_history_messages;
                conversation.messages.drain(0..remove_count);
                info!(
                    "Trimmed {} old messages from conversation {}",
                    remove_count, id
                );
            }

            Ok(())
        } else {
            Err(anyhow::anyhow!("Conversation not found: {}", id))
        }
    }

    async fn update_metadata(&self, id: &str, metadata: ConversationMetadata) -> Result<()> {
        let mut conversations = self.conversations.write();

        if let Some(conversation) = conversations.get_mut(id) {
            conversation.metadata = metadata;
            conversation.updated_at = Utc::now();
            Ok(())
        } else {
            Err(anyhow::anyhow!("Conversation not found: {}", id))
        }
    }

    async fn list_active(&self) -> Result<Vec<(String, DateTime<Utc>)>> {
        let conversations = self.conversations.read();
        Ok(conversations
            .iter()
            .map(|(id, conv)| (id.clone(), conv.updated_at))
            .collect())
    }

    async fn cleanup_expired(&self, timeout_minutes: i64) -> Result<usize> {
        let timeout = chrono::Duration::minutes(timeout_minutes);
        let now = Utc::now();
        let mut expired = Vec::new();

        {
            let conversations = self.conversations.read();
            for (id, conv) in conversations.iter() {
                if now - conv.updated_at > timeout {
                    expired.push(id.clone());
                }
            }
        }

        let count = expired.len();
        if !expired.is_empty() {
            let mut conversations = self.conversations.write();
            for id in expired {
                conversations.remove(&id);
                info!("Removed expired conversation: {}", id);
            }
        }

        Ok(count)
    }

    async fn delete(&self, id: &str) -> Result<bool> {
        Ok(self.conversations.write().remove(id).is_some())
    }
}

// ============================================================================
// InMemorySessionStore
// ============================================================================

/// In-memory implementation of SessionStore
pub struct InMemorySessionStore {
    sessions: RwLock<HashMap<String, Session>>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionStore for InMemorySessionStore {
    async fn create(&self, project_path: Option<String>) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();

        let session = Session {
            id: id.clone(),
            project_path,
            created_at: now,
            updated_at: now,
        };

        self.sessions.write().insert(id.clone(), session);
        info!("Created new session: {}", id);

        Ok(id)
    }

    async fn get(&self, id: &str) -> Result<Option<Session>> {
        Ok(self.sessions.read().get(id).cloned())
    }

    async fn update(&self, id: &str) -> Result<()> {
        if let Some(session) = self.sessions.write().get_mut(id) {
            session.updated_at = Utc::now();
            Ok(())
        } else {
            Err(anyhow::anyhow!("Session not found: {}", id))
        }
    }

    async fn remove(&self, id: &str) -> Result<Option<Session>> {
        Ok(self.sessions.write().remove(id))
    }

    async fn list(&self) -> Result<Vec<Session>> {
        Ok(self.sessions.read().values().cloned().collect())
    }
}

// ============================================================================
// InMemoryCacheStore
// ============================================================================

/// Cache entry with metadata
#[derive(Clone)]
struct CacheEntry {
    response: ChatCompletionResponse,
    created_at: Instant,
    hit_count: usize,
}

/// Configuration for in-memory cache
#[derive(Clone)]
pub struct InMemoryCacheConfig {
    pub max_entries: usize,
    pub ttl_seconds: u64,
    pub enabled: bool,
}

impl Default for InMemoryCacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 1000,
            ttl_seconds: 3600,
            enabled: true,
        }
    }
}

/// In-memory implementation of CacheStore using DashMap
pub struct InMemoryCacheStore {
    cache: DashMap<String, CacheEntry>,
    config: InMemoryCacheConfig,
}

impl InMemoryCacheStore {
    pub fn new(config: InMemoryCacheConfig) -> Self {
        Self {
            cache: DashMap::new(),
            config,
        }
    }

    fn evict_oldest(&self) {
        let mut oldest_key = None;
        let mut oldest_time = Instant::now();

        for entry in self.cache.iter() {
            if entry.value().created_at < oldest_time {
                oldest_time = entry.value().created_at;
                oldest_key = Some(entry.key().clone());
            }
        }

        if let Some(key) = oldest_key {
            self.cache.remove(&key);
            debug!("Evicted oldest cache entry: {}", key);
        }
    }
}

impl Default for InMemoryCacheStore {
    fn default() -> Self {
        Self::new(InMemoryCacheConfig::default())
    }
}

#[async_trait]
impl CacheStore for InMemoryCacheStore {
    async fn get(&self, key: &str) -> Option<ChatCompletionResponse> {
        if !self.config.enabled {
            return None;
        }

        let mut entry = self.cache.get_mut(key)?;

        // Check if expired
        if entry.created_at.elapsed() > Duration::from_secs(self.config.ttl_seconds) {
            drop(entry);
            self.cache.remove(key);
            debug!("Cache entry expired: {}", key);
            return None;
        }

        entry.hit_count += 1;
        let hit_count = entry.hit_count;
        let response = entry.response.clone();

        info!("Cache hit for key: {} (hits: {})", key, hit_count);
        Some(response)
    }

    async fn put(&self, key: String, response: ChatCompletionResponse) {
        if !self.config.enabled {
            return;
        }

        if self.cache.len() >= self.config.max_entries {
            self.evict_oldest();
        }

        let entry = CacheEntry {
            response,
            created_at: Instant::now(),
            hit_count: 0,
        };

        self.cache.insert(key.clone(), entry);
        debug!("Cached response for key: {}", key);
    }

    async fn stats(&self) -> CacheStats {
        let mut total_hits = 0;
        let mut total_entries = 0;

        for entry in self.cache.iter() {
            total_entries += 1;
            total_hits += entry.value().hit_count;
        }

        CacheStats {
            total_entries,
            total_hits,
            enabled: self.config.enabled,
        }
    }

    async fn cleanup(&self) -> Result<usize> {
        let ttl = Duration::from_secs(self.config.ttl_seconds);
        let mut expired_keys = Vec::new();

        for entry in self.cache.iter() {
            if entry.value().created_at.elapsed() > ttl {
                expired_keys.push(entry.key().clone());
            }
        }

        let count = expired_keys.len();
        for key in expired_keys {
            self.cache.remove(&key);
            debug!("Removed expired cache entry: {}", key);
        }

        info!(
            "Cache cleanup: removed {} entries, {} remaining",
            count,
            self.cache.len()
        );
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_conversation() {
        let store = InMemoryConversationStore::default();
        let id = store.create(Some("claude-3".to_string())).await.unwrap();

        assert!(!id.is_empty());

        let conv = store.get(&id).await.unwrap();
        assert!(conv.is_some());

        let conv = conv.unwrap();
        assert_eq!(conv.metadata.model, Some("claude-3".to_string()));
        assert!(conv.messages.is_empty());
    }

    #[tokio::test]
    async fn test_add_message() {
        let store = InMemoryConversationStore::default();
        let id = store.create(None).await.unwrap();

        let message = ChatMessage {
            role: "user".to_string(),
            content: Some(crate::models::openai::MessageContent::Text(
                "Hello".to_string(),
            )),
            name: None,
            tool_calls: None,
        };

        store.add_message(&id, message).await.unwrap();

        let conv = store.get(&id).await.unwrap().unwrap();
        assert_eq!(conv.messages.len(), 1);
        assert_eq!(conv.metadata.turn_count, 1);
    }

    #[tokio::test]
    async fn test_message_not_found() {
        let store = InMemoryConversationStore::default();

        let message = ChatMessage {
            role: "user".to_string(),
            content: Some(crate::models::openai::MessageContent::Text(
                "Hello".to_string(),
            )),
            name: None,
            tool_calls: None,
        };

        let result = store.add_message("nonexistent", message).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_delete_conversation() {
        let store = InMemoryConversationStore::default();
        let id = store.create(None).await.unwrap();

        assert!(store.get(&id).await.unwrap().is_some());

        let deleted = store.delete(&id).await.unwrap();
        assert!(deleted);

        assert!(store.get(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_list_active() {
        let store = InMemoryConversationStore::default();

        let id1 = store.create(None).await.unwrap();
        let id2 = store.create(None).await.unwrap();

        let active = store.list_active().await.unwrap();
        assert_eq!(active.len(), 2);

        let ids: Vec<_> = active.iter().map(|(id, _)| id.clone()).collect();
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
    }

    // ========================================================================
    // SessionStore tests
    // ========================================================================

    #[tokio::test]
    async fn test_session_create_and_get() {
        let store = InMemorySessionStore::default();
        let id = store
            .create(Some("/path/to/project".to_string()))
            .await
            .unwrap();

        assert!(!id.is_empty());

        let session = store.get(&id).await.unwrap();
        assert!(session.is_some());

        let session = session.unwrap();
        assert_eq!(session.project_path, Some("/path/to/project".to_string()));
    }

    #[tokio::test]
    async fn test_session_list() {
        let store = InMemorySessionStore::default();

        store.create(None).await.unwrap();
        store.create(Some("/path".to_string())).await.unwrap();

        let sessions = store.list().await.unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[tokio::test]
    async fn test_session_remove() {
        let store = InMemorySessionStore::default();
        let id = store.create(None).await.unwrap();

        let removed = store.remove(&id).await.unwrap();
        assert!(removed.is_some());

        let session = store.get(&id).await.unwrap();
        assert!(session.is_none());
    }

    // ========================================================================
    // CacheStore tests
    // ========================================================================

    #[tokio::test]
    async fn test_cache_put_and_get() {
        let store = InMemoryCacheStore::default();

        let response = crate::models::openai::ChatCompletionResponse {
            id: "test-id".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "test-model".to_string(),
            choices: vec![],
            usage: crate::models::openai::Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            },
            conversation_id: None,
        };

        store.put("test-key".to_string(), response.clone()).await;

        let cached = store.get("test-key").await;
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().id, "test-id");
    }

    #[tokio::test]
    async fn test_cache_stats() {
        let store = InMemoryCacheStore::default();

        let response = crate::models::openai::ChatCompletionResponse {
            id: "test".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "test".to_string(),
            choices: vec![],
            usage: crate::models::openai::Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            },
            conversation_id: None,
        };

        store.put("key1".to_string(), response.clone()).await;
        store.put("key2".to_string(), response).await;

        let stats = store.stats().await;
        assert_eq!(stats.total_entries, 2);
        assert!(stats.enabled);
    }

    // ========================================================================
    // Trimming, expiry and eviction — the paths that *remove* data
    // ========================================================================

    fn message(text: &str) -> ChatMessage {
        ChatMessage {
            role: "user".to_string(),
            content: Some(crate::models::openai::MessageContent::Text(
                text.to_string(),
            )),
            name: None,
            tool_calls: None,
        }
    }

    fn response(id: &str) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: id.to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "test-model".to_string(),
            choices: vec![],
            usage: crate::models::openai::Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            },
            conversation_id: None,
        }
    }

    /// Trimming drops the *oldest* messages and keeps the newest ones, but
    /// `turn_count` keeps counting every message ever added — a caller that
    /// derives an index from `turn_count` must not expect `messages[turn_count]`
    /// to exist.
    #[tokio::test]
    async fn add_message_trims_the_oldest_messages_but_not_the_turn_count() {
        let store = InMemoryConversationStore::new(InMemoryConversationConfig {
            max_history_messages: 2,
        });
        let id = store.create(None).await.unwrap();

        for text in ["un", "deux", "trois"] {
            store.add_message(&id, message(text)).await.unwrap();
        }

        let conv = store.get(&id).await.unwrap().unwrap();
        let texts: Vec<String> = conv
            .messages
            .iter()
            .map(|m| match &m.content {
                Some(crate::models::openai::MessageContent::Text(t)) => t.clone(),
                other => panic!("unexpected content {other:?}"),
            })
            .collect();
        assert_eq!(texts, vec!["deux".to_string(), "trois".to_string()]);
        assert_eq!(
            conv.metadata.turn_count, 3,
            "turn_count counts additions, not stored messages"
        );
    }

    /// `update_metadata` replaces the whole metadata value, so a field the
    /// caller leaves at its default silently overwrites what was stored.
    #[tokio::test]
    async fn update_metadata_replaces_rather_than_merges() {
        let store = InMemoryConversationStore::default();
        let id = store.create(Some("claude-3".to_string())).await.unwrap();
        store.add_message(&id, message("bonjour")).await.unwrap();
        assert_eq!(
            store.get(&id).await.unwrap().unwrap().metadata.turn_count,
            1
        );

        store
            .update_metadata(
                &id,
                ConversationMetadata {
                    total_tokens: 42,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let meta = store.get(&id).await.unwrap().unwrap().metadata;
        assert_eq!(meta.total_tokens, 42);
        assert_eq!(meta.model, None, "the model stored at create is gone");
        assert_eq!(meta.turn_count, 0, "the turn count is reset to zero");
    }

    /// A fresh conversation is not expired, and the no-op path returns `0`
    /// without taking the write lock.
    #[tokio::test]
    async fn cleanup_expired_keeps_fresh_conversations_and_reports_zero() {
        let store = InMemoryConversationStore::default();
        let id = store.create(None).await.unwrap();

        assert_eq!(store.cleanup_expired(60).await.unwrap(), 0);
        assert!(store.get(&id).await.unwrap().is_some());
    }

    /// `timeout_minutes` is not validated. A negative value makes the
    /// comparison `now - updated_at > Duration::minutes(-1)` true for every
    /// conversation, including one created microseconds ago, so a timeout
    /// computed from configuration can wipe the whole store.
    #[tokio::test]
    async fn cleanup_expired_with_a_negative_timeout_removes_every_conversation() {
        let store = InMemoryConversationStore::default();
        let kept_alive = store.create(None).await.unwrap();
        store.create(None).await.unwrap();

        assert_eq!(store.cleanup_expired(-1).await.unwrap(), 2);
        assert!(store.get(&kept_alive).await.unwrap().is_none());
        assert!(store.list_active().await.unwrap().is_empty());
    }

    /// Only the conversations past the timeout go; the others stay. The count
    /// returned is the number of ids selected under the *read* lock, which is
    /// also the number removed as long as nothing else writes meanwhile.
    #[tokio::test]
    async fn cleanup_expired_removes_only_what_is_past_the_timeout() {
        let store = InMemoryConversationStore::default();
        let stale = store.create(None).await.unwrap();
        let fresh = store.create(None).await.unwrap();

        // Age one conversation by hand: `updated_at` is the only expiry input.
        store
            .conversations
            .write()
            .get_mut(&stale)
            .unwrap()
            .updated_at = Utc::now() - chrono::Duration::hours(3);

        assert_eq!(store.cleanup_expired(60).await.unwrap(), 1);
        assert!(store.get(&stale).await.unwrap().is_none());
        assert!(store.get(&fresh).await.unwrap().is_some());
    }

    /// `update` moves `updated_at` forward and leaves `created_at` alone.
    #[tokio::test]
    async fn session_update_moves_updated_at_forward_only() {
        let store = InMemorySessionStore::default();
        let id = store.create(Some("/projet".to_string())).await.unwrap();
        let before = store.get(&id).await.unwrap().unwrap();

        tokio::time::sleep(Duration::from_millis(5)).await;
        store.update(&id).await.unwrap();

        let after = store.get(&id).await.unwrap().unwrap();
        assert!(
            after.updated_at > before.updated_at,
            "updated_at must advance: {} -> {}",
            before.updated_at,
            after.updated_at
        );
        assert_eq!(after.created_at, before.created_at);
        assert_eq!(after.project_path, Some("/projet".to_string()));
    }

    /// Touching a session that was never created is an error, not a silent
    /// insert.
    #[tokio::test]
    async fn session_update_on_an_unknown_id_is_an_error() {
        let store = InMemorySessionStore::default();

        let err = store.update("jamais-cree").await.unwrap_err();
        assert_eq!(err.to_string(), "Session not found: jamais-cree");
        assert!(store.list().await.unwrap().is_empty());
    }

    /// Removing an unknown session is `Ok(None)`, not an error — the caller
    /// cannot tell "removed nothing" from "removed something" without the
    /// return value.
    #[tokio::test]
    async fn session_remove_of_an_unknown_id_is_ok_none() {
        let store = InMemorySessionStore::default();
        assert!(store.remove("jamais-cree").await.unwrap().is_none());
    }

    /// At capacity, `put` evicts the entry with the oldest `created_at` and
    /// keeps the newer ones.
    #[tokio::test]
    async fn cache_put_evicts_the_oldest_entry_when_full() {
        let store = InMemoryCacheStore::new(InMemoryCacheConfig {
            max_entries: 2,
            ..Default::default()
        });

        store.put("un".to_string(), response("un")).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        store.put("deux".to_string(), response("deux")).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        store.put("trois".to_string(), response("trois")).await;

        assert!(store.get("un").await.is_none(), "the oldest was evicted");
        assert_eq!(store.get("deux").await.unwrap().id, "deux");
        assert_eq!(store.get("trois").await.unwrap().id, "trois");
        assert_eq!(store.stats().await.total_entries, 2);
    }

    /// The eviction scan keeps the smallest `created_at` whichever order
    /// `DashMap` hands the entries over in — the order is seeded at random per
    /// map, so the result must not depend on it. Twenty entries, only the first
    /// of which is separated from the rest in time, pin both halves: the victim
    /// is always that first entry, and a scan of twenty entries necessarily
    /// compares at least one that is *not* older than the running minimum.
    #[tokio::test]
    async fn cache_eviction_picks_the_oldest_entry_whatever_the_scan_order() {
        let store = InMemoryCacheStore::new(InMemoryCacheConfig {
            max_entries: 20,
            ..Default::default()
        });

        store.put("victime".to_string(), response("victime")).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        for n in 0..19 {
            store.put(format!("k{n}"), response("garnissage")).await;
        }
        assert_eq!(store.stats().await.total_entries, 20);

        store
            .put("derniere".to_string(), response("derniere"))
            .await;

        assert!(
            store.get("victime").await.is_none(),
            "the entry with the smallest created_at is the one that goes"
        );
        assert_eq!(store.get("derniere").await.unwrap().id, "derniere");
        for n in 0..19 {
            assert!(
                store.get(&format!("k{n}")).await.is_some(),
                "k{n} was newer than the victim and must survive"
            );
        }
        assert_eq!(store.stats().await.total_entries, 20);
    }

    /// `max_entries: 0` does not disable the cache: `put` runs `evict_oldest`
    /// on an empty map and then inserts anyway, so the store keeps exactly one
    /// entry. Only `enabled: false` really turns it off.
    #[tokio::test]
    async fn cache_with_max_entries_zero_still_stores_one_entry() {
        let store = InMemoryCacheStore::new(InMemoryCacheConfig {
            max_entries: 0,
            ..Default::default()
        });

        store.put("un".to_string(), response("un")).await;
        assert_eq!(store.stats().await.total_entries, 1);

        store.put("deux".to_string(), response("deux")).await;
        assert_eq!(store.stats().await.total_entries, 1);
        assert!(store.get("un").await.is_none());
        assert_eq!(store.get("deux").await.unwrap().id, "deux");
    }

    /// A hit increments `hit_count`, which is what `stats()` sums.
    #[tokio::test]
    async fn cache_stats_count_every_hit_not_every_entry() {
        let store = InMemoryCacheStore::default();
        store.put("cle".to_string(), response("r")).await;

        store.get("cle").await.unwrap();
        store.get("cle").await.unwrap();
        assert!(store.get("absente").await.is_none());

        let stats = store.stats().await;
        assert_eq!(stats.total_entries, 1);
        assert_eq!(stats.total_hits, 2);
    }

    /// An expired entry is not merely hidden by `get`: it is dropped from the
    /// map on the way out.
    #[tokio::test]
    async fn cache_get_evicts_the_entry_it_finds_expired() {
        let store = InMemoryCacheStore::new(InMemoryCacheConfig {
            ttl_seconds: 0,
            ..Default::default()
        });
        store.put("cle".to_string(), response("r")).await;
        assert_eq!(store.stats().await.total_entries, 1);

        assert!(store.get("cle").await.is_none());
        assert_eq!(
            store.stats().await.total_entries,
            0,
            "the expired entry must be removed, not just skipped"
        );
    }

    /// `cleanup` reports how many entries it removed and leaves the live ones.
    #[tokio::test]
    async fn cache_cleanup_removes_only_expired_entries() {
        let live = InMemoryCacheStore::default();
        live.put("cle".to_string(), response("r")).await;
        assert_eq!(live.cleanup().await.unwrap(), 0);
        assert_eq!(live.stats().await.total_entries, 1);

        let stale = InMemoryCacheStore::new(InMemoryCacheConfig {
            ttl_seconds: 0,
            ..Default::default()
        });
        stale.put("un".to_string(), response("un")).await;
        stale.put("deux".to_string(), response("deux")).await;

        assert_eq!(stale.cleanup().await.unwrap(), 2);
        assert_eq!(stale.stats().await.total_entries, 0);
    }

    /// A disabled cache refuses to read *and* to write, so `cleanup` and
    /// `stats` see an empty map rather than entries nobody can reach.
    #[tokio::test]
    async fn a_disabled_cache_stores_nothing_at_all() {
        let store = InMemoryCacheStore::new(InMemoryCacheConfig {
            enabled: false,
            ..Default::default()
        });
        store.put("cle".to_string(), response("r")).await;

        let stats = store.stats().await;
        assert_eq!(stats.total_entries, 0);
        assert!(!stats.enabled);
        assert_eq!(store.cleanup().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_cache_disabled() {
        let config = InMemoryCacheConfig {
            enabled: false,
            ..Default::default()
        };
        let store = InMemoryCacheStore::new(config);

        let response = crate::models::openai::ChatCompletionResponse {
            id: "test".to_string(),
            object: "chat.completion".to_string(),
            created: 0,
            model: "test".to_string(),
            choices: vec![],
            usage: crate::models::openai::Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            },
            conversation_id: None,
        };

        store.put("key".to_string(), response).await;

        let cached = store.get("key").await;
        assert!(cached.is_none());
    }
}
