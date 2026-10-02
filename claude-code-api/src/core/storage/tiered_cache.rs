//! Tiered caching with L1 (memory) and L2 (Neo4j)
//!
//! This module provides a two-level cache:
//! - L1: DashMap for fast, in-memory access
//! - L2: Neo4j for persistent, cross-restart storage
//!
//! Cache flow:
//! 1. Read: L1 hit → return | L1 miss → L2 lookup → populate L1 → return
//! 2. Write: Write to L1 → async write to L2

#![allow(dead_code)] // Public API - may not be used internally

use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use neo4rs::{Graph, query};
use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::core::cache::CacheStats;
use crate::models::openai::ChatCompletionResponse;

use super::traits::CacheStore;

/// Configuration for tiered cache
#[derive(Clone, Debug)]
pub struct TieredCacheConfig {
    /// Maximum entries in L1 cache
    pub l1_max_entries: usize,
    /// TTL for L1 cache entries in seconds
    pub l1_ttl_seconds: u64,
    /// Whether L2 (Neo4j) cache is enabled
    pub l2_enabled: bool,
    /// TTL for L2 cache entries in seconds
    pub l2_ttl_seconds: u64,
}

impl Default for TieredCacheConfig {
    fn default() -> Self {
        Self {
            l1_max_entries: 1000,
            l1_ttl_seconds: 3600, // 1 hour
            l2_enabled: true,
            l2_ttl_seconds: 86400, // 24 hours
        }
    }
}

/// L1 cache entry
#[derive(Clone)]
struct L1Entry {
    response: ChatCompletionResponse,
    created_at: Instant,
    hit_count: usize,
}

/// Tiered cache with L1 (DashMap) and L2 (Neo4j)
pub struct TieredCache {
    /// Shared with the background sweep task. This **must** stay behind an `Arc`:
    /// `DashMap::clone` deep-copies every shard into an independent map, so
    /// handing the task a bare `DashMap` gave it a detached snapshot and the
    /// sweep silently never touched the live cache.
    l1: Arc<DashMap<String, L1Entry>>,
    l2: Option<Arc<Graph>>,
    config: TieredCacheConfig,
    l1_hits: std::sync::atomic::AtomicUsize,
    l2_hits: std::sync::atomic::AtomicUsize,
    misses: std::sync::atomic::AtomicUsize,
}

impl TieredCache {
    /// Create a new tiered cache with optional Neo4j L2
    pub fn new(config: TieredCacheConfig, neo4j_graph: Option<Arc<Graph>>) -> Self {
        let cache = Self {
            l1: Arc::new(DashMap::new()),
            l2: neo4j_graph,
            config,
            l1_hits: std::sync::atomic::AtomicUsize::new(0),
            l2_hits: std::sync::atomic::AtomicUsize::new(0),
            misses: std::sync::atomic::AtomicUsize::new(0),
        };

        // Start L1 cleanup task, on a *shared* handle to the live map
        let l1_clone = Arc::clone(&cache.l1);
        let ttl = cache.config.l1_ttl_seconds;
        tokio::spawn(async move {
            Self::l1_cleanup_loop(l1_clone, ttl).await;
        });

        cache
    }

    /// Create a new tiered cache with only L1 (memory)
    pub fn memory_only(config: TieredCacheConfig) -> Self {
        Self::new(config, None)
    }

    /// L1 cleanup background task
    async fn l1_cleanup_loop(cache: Arc<DashMap<String, L1Entry>>, ttl_seconds: u64) {
        let ttl = Duration::from_secs(ttl_seconds);

        loop {
            tokio::time::sleep(Duration::from_secs(300)).await;

            let mut expired_keys = Vec::new();
            for entry in cache.iter() {
                if entry.value().created_at.elapsed() > ttl {
                    expired_keys.push(entry.key().clone());
                }
            }

            for key in expired_keys {
                cache.remove(&key);
            }

            debug!("L1 cache cleanup: {} entries remaining", cache.len());
        }
    }

    /// Get from L1 cache
    fn get_l1(&self, key: &str) -> Option<ChatCompletionResponse> {
        let mut entry = self.l1.get_mut(key)?;

        // Check TTL
        if entry.created_at.elapsed() > Duration::from_secs(self.config.l1_ttl_seconds) {
            drop(entry);
            self.l1.remove(key);
            return None;
        }

        entry.hit_count += 1;
        self.l1_hits
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Some(entry.response.clone())
    }

    /// Get from L2 cache (Neo4j)
    async fn get_l2(&self, key: &str) -> Option<ChatCompletionResponse> {
        let graph = self.l2.as_ref()?;

        if !self.config.l2_enabled {
            return None;
        }

        let q = query(
            "MATCH (c:NexusCacheEntry {key: $key})
            WHERE c.expires_at > datetime()
            RETURN c.response as response",
        )
        .param("key", key);

        match graph.execute(q).await {
            Ok(mut result) => {
                if let Ok(Some(row)) = result.next().await
                    && let Ok(response_json) = row.get::<String>("response")
                    && let Ok(response) = serde_json::from_str(&response_json)
                {
                    self.l2_hits
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    debug!("L2 cache hit for key: {}", key);
                    return Some(response);
                }
            },
            Err(e) => {
                warn!("L2 cache read error: {}", e);
            },
        }

        None
    }

    /// Promote from L2 to L1
    fn promote_to_l1(&self, key: String, response: ChatCompletionResponse) {
        // Evict oldest if at capacity
        if self.l1.len() >= self.config.l1_max_entries {
            self.evict_oldest_l1();
        }

        self.l1.insert(
            key,
            L1Entry {
                response,
                created_at: Instant::now(),
                hit_count: 0,
            },
        );
    }

    /// Evict oldest L1 entry
    fn evict_oldest_l1(&self) {
        let mut oldest_key = None;
        let mut oldest_time = Instant::now();

        for entry in self.l1.iter() {
            if entry.value().created_at < oldest_time {
                oldest_time = entry.value().created_at;
                oldest_key = Some(entry.key().clone());
            }
        }

        if let Some(key) = oldest_key {
            self.l1.remove(&key);
        }
    }

    /// Write to L2 cache (async, non-blocking)
    async fn write_l2(&self, key: &str, response: &ChatCompletionResponse) {
        let Some(graph) = &self.l2 else { return };

        if !self.config.l2_enabled {
            return;
        }

        let response_json = match serde_json::to_string(response) {
            Ok(json) => json,
            Err(e) => {
                warn!("Failed to serialize response for L2 cache: {}", e);
                return;
            },
        };

        let q = query(
            "MERGE (c:NexusCacheEntry {key: $key})
            SET c.response = $response,
                c.created_at = datetime(),
                c.expires_at = datetime() + duration({seconds: $ttl})",
        )
        .param("key", key)
        .param("response", response_json)
        .param("ttl", self.config.l2_ttl_seconds as i64);

        if let Err(e) = graph.run(q).await {
            warn!("L2 cache write error: {}", e);
        } else {
            debug!("Wrote to L2 cache: {}", key);
        }
    }

    /// Initialize L2 cache schema
    pub async fn init_l2_schema(&self) -> Result<()> {
        let Some(graph) = &self.l2 else {
            return Ok(());
        };

        let constraint = "CREATE CONSTRAINT nexus_cache_key IF NOT EXISTS FOR (c:NexusCacheEntry) REQUIRE c.key IS UNIQUE";

        if let Err(e) = graph.run(query(constraint)).await {
            debug!("Cache constraint creation result: {:?}", e);
        }

        info!("L2 cache schema initialized");
        Ok(())
    }

    /// Get extended statistics
    pub fn extended_stats(&self) -> TieredCacheStats {
        let l1_hits = self.l1_hits.load(std::sync::atomic::Ordering::Relaxed);
        let l2_hits = self.l2_hits.load(std::sync::atomic::Ordering::Relaxed);
        let misses = self.misses.load(std::sync::atomic::Ordering::Relaxed);

        TieredCacheStats {
            l1_entries: self.l1.len(),
            l1_hits,
            l2_hits,
            misses,
            l2_enabled: self.l2.is_some() && self.config.l2_enabled,
            hit_rate: if l1_hits + l2_hits + misses > 0 {
                (l1_hits + l2_hits) as f64 / (l1_hits + l2_hits + misses) as f64
            } else {
                0.0
            },
        }
    }
}

#[async_trait]
impl CacheStore for TieredCache {
    async fn get(&self, key: &str) -> Option<ChatCompletionResponse> {
        // Try L1 first
        if let Some(response) = self.get_l1(key) {
            return Some(response);
        }

        // Try L2
        if let Some(response) = self.get_l2(key).await {
            // Promote to L1
            self.promote_to_l1(key.to_string(), response.clone());
            return Some(response);
        }

        self.misses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        None
    }

    async fn put(&self, key: String, response: ChatCompletionResponse) {
        // Write to L1
        if self.l1.len() >= self.config.l1_max_entries {
            self.evict_oldest_l1();
        }

        self.l1.insert(
            key.clone(),
            L1Entry {
                response: response.clone(),
                created_at: Instant::now(),
                hit_count: 0,
            },
        );

        // Async write to L2
        self.write_l2(&key, &response).await;
    }

    async fn stats(&self) -> CacheStats {
        let extended = self.extended_stats();
        CacheStats {
            total_entries: extended.l1_entries,
            total_hits: extended.l1_hits + extended.l2_hits,
            enabled: true,
        }
    }

    async fn cleanup(&self) -> Result<usize> {
        let mut count = 0;

        // Cleanup L1
        let ttl = Duration::from_secs(self.config.l1_ttl_seconds);
        let mut expired = Vec::new();

        for entry in self.l1.iter() {
            if entry.value().created_at.elapsed() > ttl {
                expired.push(entry.key().clone());
            }
        }

        for key in expired {
            self.l1.remove(&key);
            count += 1;
        }

        // Cleanup L2
        if let Some(graph) = &self.l2 {
            let q = query(
                "MATCH (c:NexusCacheEntry)
                WHERE c.expires_at < datetime()
                DELETE c
                RETURN count(c) as deleted",
            );

            if let Ok(mut result) = graph.execute(q).await
                && let Ok(Some(row)) = result.next().await
                && let Ok(deleted) = row.get::<i64>("deleted")
            {
                count += deleted as usize;
            }
        }

        info!("Cache cleanup: removed {} entries", count);
        Ok(count)
    }
}

/// Extended statistics for tiered cache
#[derive(Debug, Clone, Serialize)]
pub struct TieredCacheStats {
    pub l1_entries: usize,
    pub l1_hits: usize,
    pub l2_hits: usize,
    pub misses: usize,
    pub l2_enabled: bool,
    pub hit_rate: f64,
}

#[cfg(test)]
mod tests {
    //! L1-only behaviour. Everything here goes through `TieredCache::memory_only`,
    //! so no `Graph` is involved and the tests are pure in-process logic.
    //!
    //! The L2 half (`get_l2`, `write_l2`, the Neo4j branch of `cleanup`,
    //! `init_l2_schema`) needs a `neo4rs::Graph`, which cannot be faked with a
    //! trait; it is covered in `tests/storage_tiered_cache_l2_s05.rs` against a
    //! loopback Bolt server.

    use super::*;
    use crate::models::openai::Usage;

    fn response(id: &str) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: id.to_string(),
            object: "chat.completion".to_string(),
            created: 1_790_000_000,
            model: "claude-opus-5".to_string(),
            choices: vec![],
            usage: Usage {
                prompt_tokens: 11,
                completion_tokens: 22,
                total_tokens: 33,
            },
            conversation_id: None,
        }
    }

    /// Entries inserted with this config are already expired by the time they are
    /// read back: any non-zero elapsed time is `> Duration::from_secs(0)`.
    fn already_expired() -> TieredCacheConfig {
        TieredCacheConfig {
            l1_ttl_seconds: 0,
            ..TieredCacheConfig::default()
        }
    }

    #[test]
    fn default_config_is_an_hour_in_l1_and_a_day_in_l2() {
        let config = TieredCacheConfig::default();

        assert_eq!(config.l1_max_entries, 1000);
        assert_eq!(config.l1_ttl_seconds, 3600);
        assert_eq!(config.l2_ttl_seconds, 86400);
        assert!(
            config.l2_enabled,
            "L2 is opt-out in the config, and only the absent Graph disables it"
        );
    }

    #[tokio::test]
    async fn a_put_entry_comes_back_from_l1_and_is_counted_as_an_l1_hit() {
        let cache = TieredCache::memory_only(TieredCacheConfig::default());

        cache.put("k".to_string(), response("r-1")).await;
        let cached = cache.get("k").await.expect("just written");

        assert_eq!(cached.id, "r-1");
        assert_eq!(cached.usage.total_tokens, 33, "the whole value round-trips");
        let stats = cache.extended_stats();
        assert_eq!(stats.l1_entries, 1);
        assert_eq!(stats.l1_hits, 1);
        assert_eq!(stats.l2_hits, 0);
        assert_eq!(stats.misses, 0);
        assert!((stats.hit_rate - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn an_unknown_key_is_a_miss_and_nothing_else() {
        let cache = TieredCache::memory_only(TieredCacheConfig::default());

        assert!(cache.get("nonexistent").await.is_none());

        let stats = cache.extended_stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.l1_hits, 0);
        assert_eq!(stats.l1_entries, 0);
        assert!(
            stats.hit_rate.abs() < f64::EPSILON,
            "a miss-only cache has a 0.0 hit rate, not a division by zero"
        );
    }

    /// `memory_only` keeps `config.l2_enabled` at its default `true`, and
    /// `extended_stats` reports `l2_enabled: false` anyway because there is no
    /// `Graph`. The reported flag is the effective one, which is the useful one.
    #[tokio::test]
    async fn extended_stats_reports_l2_as_disabled_when_there_is_no_graph() {
        let cache = TieredCache::memory_only(TieredCacheConfig::default());

        assert!(!cache.extended_stats().l2_enabled);
    }

    #[tokio::test]
    async fn the_hit_rate_mixes_hits_and_misses() {
        let cache = TieredCache::memory_only(TieredCacheConfig::default());

        cache.put("k".to_string(), response("r-1")).await;
        cache.get("k").await.expect("hit");
        assert!(cache.get("absent").await.is_none());

        let stats = cache.extended_stats();
        assert_eq!((stats.l1_hits, stats.misses), (1, 1));
        assert!((stats.hit_rate - 0.5).abs() < f64::EPSILON);
    }

    /// `get_l1` checks the TTL before answering, and removes the entry it just
    /// refused, so an expired key costs one lookup and then behaves as absent.
    #[tokio::test]
    async fn an_expired_l1_entry_is_dropped_on_read_rather_than_served() {
        let cache = TieredCache::memory_only(already_expired());
        cache.put("k".to_string(), response("r-1")).await;
        assert_eq!(cache.extended_stats().l1_entries, 1, "written, then stale");

        assert!(cache.get("k").await.is_none(), "the TTL has passed");

        let stats = cache.extended_stats();
        assert_eq!(stats.l1_entries, 0, "the stale entry is evicted on read");
        assert_eq!(stats.l1_hits, 0, "an expired read is not a hit");
        assert_eq!(stats.misses, 1);
    }

    /// Two reads of the same key count twice. `L1Entry::hit_count` is bumped on
    /// each one but is never read anywhere in the crate: eviction is purely by
    /// age (`evict_oldest_l1`), so the per-entry counter buys nothing.
    #[tokio::test]
    async fn repeated_reads_each_count_as_a_hit() {
        let cache = TieredCache::memory_only(TieredCacheConfig::default());
        cache.put("k".to_string(), response("r-1")).await;

        for _ in 0..3 {
            cache.get("k").await.expect("hit");
        }

        assert_eq!(cache.extended_stats().l1_hits, 3);
    }

    #[tokio::test]
    async fn put_evicts_the_oldest_entry_once_l1_is_full() {
        let cache = TieredCache::memory_only(TieredCacheConfig {
            l1_max_entries: 2,
            ..TieredCacheConfig::default()
        });

        cache.put("first".to_string(), response("r-1")).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        cache.put("second".to_string(), response("r-2")).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        cache.put("third".to_string(), response("r-3")).await;

        assert_eq!(cache.extended_stats().l1_entries, 2, "the cap is honoured");
        assert!(
            cache.get("first").await.is_none(),
            "the oldest key is the one that goes"
        );
        assert_eq!(cache.get("second").await.expect("kept").id, "r-2");
        assert_eq!(cache.get("third").await.expect("kept").id, "r-3");
    }

    /// Re-putting the same key evicts *another* entry before overwriting it, so a
    /// full cache loses one unrelated entry on every refresh of a key it already
    /// holds. `put` checks the length before knowing whether the insert will add
    /// an entry or replace one.
    #[tokio::test]
    async fn refreshing_a_key_in_a_full_l1_still_evicts_a_different_key() {
        let cache = TieredCache::memory_only(TieredCacheConfig {
            l1_max_entries: 2,
            ..TieredCacheConfig::default()
        });
        cache.put("a".to_string(), response("r-a")).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        cache.put("b".to_string(), response("r-b")).await;

        cache.put("b".to_string(), response("r-b-again")).await;

        assert_eq!(
            cache.extended_stats().l1_entries,
            1,
            "`a` was evicted to make room for a key that was already there"
        );
        assert_eq!(cache.get("b").await.expect("kept").id, "r-b-again");
    }

    /// `l1_max_entries: 0` does not disable L1: `evict_oldest_l1` finds nothing
    /// to evict on the first `put` and the insert happens anyway, so the cache
    /// holds one entry above its stated maximum. A cap of zero should either
    /// refuse to store or be rejected by the config.
    #[tokio::test]
    async fn a_zero_sized_l1_still_stores_one_entry() {
        let cache = TieredCache::memory_only(TieredCacheConfig {
            l1_max_entries: 0,
            ..TieredCacheConfig::default()
        });

        cache.put("k".to_string(), response("r-1")).await;

        assert_eq!(cache.extended_stats().l1_entries, 1);
        assert_eq!(cache.get("k").await.expect("served anyway").id, "r-1");
    }

    #[tokio::test]
    async fn cleanup_removes_only_the_expired_l1_entries_and_counts_them() {
        let cache = TieredCache::memory_only(already_expired());
        cache.put("a".to_string(), response("r-a")).await;
        cache.put("b".to_string(), response("r-b")).await;

        assert_eq!(cache.cleanup().await.expect("cleanup"), 2);

        assert_eq!(cache.extended_stats().l1_entries, 0);
    }

    #[tokio::test]
    async fn cleanup_keeps_fresh_entries_and_reports_zero() {
        let cache = TieredCache::memory_only(TieredCacheConfig::default());
        cache.put("a".to_string(), response("r-a")).await;

        assert_eq!(cache.cleanup().await.expect("cleanup"), 0);

        assert_eq!(cache.extended_stats().l1_entries, 1);
    }

    /// `CacheStats` is the shape the `/health`-style endpoints report. It folds
    /// L1 and L2 hits into one number, counts only L1 entries as `total_entries`
    /// — an L2-only key is invisible — and hard-codes `enabled: true`, so it says
    /// "enabled" for a cache whose L2 is off and whose L1 cap is zero.
    #[tokio::test]
    async fn stats_flattens_both_tiers_into_the_shared_shape() {
        let cache = TieredCache::memory_only(TieredCacheConfig::default());
        cache.put("k".to_string(), response("r-1")).await;
        cache.get("k").await.expect("hit");
        assert!(cache.get("absent").await.is_none());

        let stats = cache.stats().await;

        assert_eq!(stats.total_entries, 1);
        assert_eq!(stats.total_hits, 1, "l1_hits + l2_hits");
        assert!(stats.enabled, "hard-coded, never derived from the config");
    }

    /// Without a `Graph`, `init_l2_schema` is a no-op that still returns `Ok`.
    #[tokio::test]
    async fn init_l2_schema_is_a_no_op_without_a_graph() {
        let cache = TieredCache::memory_only(TieredCacheConfig::default());

        cache.init_l2_schema().await.expect("no-op");
    }

    /// The background task `TieredCache::new` spawns. It wakes every 300 s and
    /// drops expired entries, which is the only way an untouched key ever leaves
    /// L1 — `get` only evicts the key it was asked for. Driven here on a paused
    /// clock so nothing actually waits.
    #[tokio::test(start_paused = true)]
    async fn the_background_task_sweeps_expired_entries_without_a_read() {
        let cache = TieredCache::new(already_expired(), None);
        cache.put("a".to_string(), response("r-a")).await;
        cache.put("b".to_string(), response("r-b")).await;
        assert_eq!(cache.extended_stats().l1_entries, 2);

        // On a paused clock tokio auto-advances to the next deadline, so this
        // sleep lets the task register its 300 s timer, fires it, and comes back
        // without any wall-clock wait.
        tokio::time::sleep(Duration::from_secs(301)).await;
        for _ in 0..50 {
            if cache.extended_stats().l1_entries == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert_eq!(
            cache.extended_stats().l1_entries,
            0,
            "the sweep ran on its own, with no get() in between"
        );
        assert_eq!(
            cache.extended_stats().misses,
            0,
            "and it is not accounted as a miss"
        );
    }

    /// The same task must leave fresh entries alone when it wakes up.
    #[tokio::test(start_paused = true)]
    async fn the_background_task_leaves_unexpired_entries_in_place() {
        let cache = TieredCache::new(TieredCacheConfig::default(), None);
        cache.put("a".to_string(), response("r-a")).await;

        tokio::time::sleep(Duration::from_secs(301)).await;
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        assert_eq!(cache.extended_stats().l1_entries, 1);
        assert_eq!(cache.get("a").await.expect("still there").id, "r-a");
    }
}
