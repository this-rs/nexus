//! Behaviour tests for the **L2 (Neo4j) half** of
//! [`claude_code_api::core::storage::TieredCache`].
//!
//! The L1 half is pure in-process logic and is covered inside the module itself
//! (`TieredCache::memory_only`). Everything here needs a `neo4rs::Graph`, which
//! `TieredCache::new` takes as `Option<Arc<Graph>>` and which no trait can stand
//! in for, so these tests drive the real code against the loopback Bolt server in
//! [`fake_bolt_s05`].
//!
//! Three things are worth asserting and only the first is about return values:
//!
//! 1. what a cached row does to `get` / `put` / `cleanup`;
//! 2. the **Cypher and parameters** the cache emits — the server re-reads them,
//!    so e.g. the TTL really being sent as seconds is assertable;
//! 3. where an L2 error is swallowed, and what the caller is told instead.
//!
//! What it cannot show: whether `MATCH (c:NexusCacheEntry) WHERE c.expires_at <
//! datetime() DELETE c RETURN count(c) as deleted` means what the code assumes.
//! The fake server does not interpret Cypher; that half needs a real Neo4j.

mod fake_bolt_s05;

use claude_code_api::core::storage::{CacheStore, TieredCache, TieredCacheConfig};
use claude_code_api::models::openai::{ChatCompletionResponse, Usage};
use fake_bolt_s05::{FakeBolt, pack_int, pack_null, pack_string};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

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

/// The exact `c.response` payload `write_l2` would have stored for `id`.
fn stored_response(id: &str) -> Vec<u8> {
    pack_string(&serde_json::to_string(&response(id)).expect("serializable"))
}

async fn cache_with_l2(bolt: &FakeBolt, config: TieredCacheConfig) -> TieredCache {
    let graph: Arc<neo4rs::Graph> = bolt.graph().await;
    TieredCache::new(config, Some(graph))
}

/// L1 entries are stale as soon as they are written, which makes the L2 path the
/// only one a `get` can take.
fn l1_expires_immediately() -> TieredCacheConfig {
    TieredCacheConfig {
        l1_ttl_seconds: 0,
        ..TieredCacheConfig::default()
    }
}

// ---------------------------------------------------------------------------
// get: the L2 read path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_l1_miss_is_served_from_l2_and_counted_as_an_l2_hit() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(&bolt, TieredCacheConfig::default()).await;
    bolt.returning("response", vec![stored_response("r-from-l2")]);

    let found = cache.get("k").await.expect("the row is in L2");

    assert_eq!(found.id, "r-from-l2");
    assert_eq!(
        found.usage.total_tokens, 33,
        "the whole value is rehydrated"
    );
    let stats = cache.extended_stats();
    assert_eq!(stats.l2_hits, 1);
    assert_eq!(stats.l1_hits, 0);
    assert_eq!(stats.misses, 0, "an L2 hit is not a miss");
    assert!(stats.l2_enabled);

    let run = bolt.run_matching("MATCH (c:NexusCacheEntry {key: $key})");
    assert_eq!(run.param("key").as_str(), Some("k"));
    assert!(
        run.cypher.contains("WHERE c.expires_at > datetime()"),
        "an expired row must not be served: {}",
        run.cypher
    );
    assert!(
        run.cypher.contains("RETURN c.response as response"),
        "{}",
        run.cypher
    );
}

/// The point of the two tiers: the row read from L2 is promoted, so the second
/// read touches no graph at all.
#[tokio::test]
async fn an_l2_hit_is_promoted_so_the_next_read_never_reaches_the_graph() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(&bolt, TieredCacheConfig::default()).await;
    bolt.returning("response", vec![stored_response("r-1")]);

    cache.get("k").await.expect("from L2");
    cache.get("k").await.expect("from L1 this time");

    assert_eq!(
        bolt.count_runs_matching("NexusCacheEntry"),
        1,
        "the second read was served from L1: {:?}",
        bolt.cypher()
    );
    let stats = cache.extended_stats();
    assert_eq!((stats.l1_hits, stats.l2_hits), (1, 1));
    assert_eq!(stats.l1_entries, 1, "promote_to_l1 inserted the row");
}

/// `promote_to_l1` evicts before inserting, so promoting into a full L1 costs an
/// existing entry — the same age-based eviction `put` uses.
#[tokio::test]
async fn promotion_into_a_full_l1_evicts_the_oldest_entry() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(
        &bolt,
        TieredCacheConfig {
            l1_max_entries: 1,
            ..TieredCacheConfig::default()
        },
    )
    .await;
    cache
        .put("resident".to_string(), response("r-resident"))
        .await;
    bolt.forget_runs();
    bolt.returning("response", vec![stored_response("r-promoted")]);

    let promoted = cache.get("newcomer").await.expect("from L2");

    assert_eq!(promoted.id, "r-promoted");
    assert_eq!(cache.extended_stats().l1_entries, 1, "the cap held");
    assert_eq!(
        cache.get("newcomer").await.expect("still cached").id,
        "r-promoted",
        "the promoted entry is the one that stayed"
    );
}

#[tokio::test]
async fn an_absent_row_is_a_plain_miss() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(&bolt, TieredCacheConfig::default()).await;
    bolt.returning("response", Vec::new());

    assert!(cache.get("k").await.is_none());

    assert_eq!(cache.extended_stats().misses, 1);
    assert_eq!(bolt.count_runs_matching("NexusCacheEntry"), 1);
}

/// A refused L2 read is logged at `warn!` and then reported as a cache miss. That
/// is the right call for a cache — a degraded L2 must not fail the request — but
/// it means the caller cannot distinguish "not cached" from "the graph is down",
/// and the only signal is a log line.
#[tokio::test]
async fn a_refused_l2_read_degrades_into_a_miss() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(&bolt, TieredCacheConfig::default()).await;
    bolt.failing("the database is unavailable");

    assert!(cache.get("k").await.is_none());

    let stats = cache.extended_stats();
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.l2_hits, 0);
    assert_eq!(stats.l1_entries, 0, "nothing was promoted");
}

/// A row whose `response` is not valid JSON falls through the `&&`-chain and is
/// reported as a miss — and nothing deletes or overwrites it. The poisoned row
/// keeps matching `expires_at > datetime()`, so every subsequent read pays for
/// the round trip and still misses, until the TTL runs out. Deleting the row (or
/// logging it) would turn an invisible, permanent cost into a one-off.
#[tokio::test]
async fn an_unparsable_l2_row_is_a_silent_miss_and_is_never_repaired() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(&bolt, l1_expires_immediately()).await;
    bolt.returning("response", vec![pack_string("{not json at all")]);

    assert!(cache.get("k").await.is_none(), "first read");
    assert!(cache.get("k").await.is_none(), "and the one after it");

    assert_eq!(
        bolt.count_runs_matching("MATCH (c:NexusCacheEntry {key: $key})"),
        2,
        "the bad row is re-read every time"
    );
    assert_eq!(
        bolt.count_runs_matching("MERGE"),
        0,
        "nothing tries to repair or evict it: {:?}",
        bolt.cypher()
    );
    assert_eq!(cache.extended_stats().misses, 2);
}

/// Same silent fall-through when `c.response` is not a string at all — a schema
/// drift in whatever wrote the row is indistinguishable from a cold cache.
#[tokio::test]
async fn an_l2_row_whose_response_is_not_a_string_is_a_silent_miss() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(&bolt, TieredCacheConfig::default()).await;
    bolt.returning("response", vec![pack_null()]);

    assert!(cache.get("k").await.is_none());

    assert_eq!(cache.extended_stats().misses, 1);
}

/// `l2_enabled: false` with a `Graph` present: the guard is inside `get_l2`,
/// after the `Option` check, so the test has to prove *no statement was sent*
/// rather than just that the answer was `None`.
#[tokio::test]
async fn a_disabled_l2_is_never_queried_even_though_the_graph_is_there() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(
        &bolt,
        TieredCacheConfig {
            l2_enabled: false,
            ..TieredCacheConfig::default()
        },
    )
    .await;
    bolt.returning("response", vec![stored_response("r-1")]);

    assert!(cache.get("k").await.is_none());

    assert!(
        bolt.cypher().is_empty(),
        "not a single statement should go out: {:?}",
        bolt.cypher()
    );
    assert_eq!(cache.extended_stats().misses, 1);
    assert!(
        !cache.extended_stats().l2_enabled,
        "and the stats agree it is off"
    );
}

// ---------------------------------------------------------------------------
// put: the L2 write path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn put_merges_the_entry_into_l2_with_the_configured_ttl() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(
        &bolt,
        TieredCacheConfig {
            l2_ttl_seconds: 900,
            ..TieredCacheConfig::default()
        },
    )
    .await;

    cache.put("k".to_string(), response("r-1")).await;

    let run = bolt.run_matching("MERGE (c:NexusCacheEntry {key: $key})");
    assert_eq!(run.param("key").as_str(), Some("k"));
    assert_eq!(
        run.param("ttl").as_i64(),
        Some(900),
        "the TTL goes out as an integer number of seconds"
    );
    assert!(
        run.cypher
            .contains("c.expires_at = datetime() + duration({seconds: $ttl})"),
        "{}",
        run.cypher
    );
    // The stored payload must be exactly what `get_l2` knows how to read back.
    let stored: ChatCompletionResponse =
        serde_json::from_str(run.param("response").as_str().expect("a json string"))
            .expect("write_l2 stores what get_l2 parses");
    assert_eq!(stored.id, "r-1");
    assert_eq!(stored.usage.completion_tokens, 22);
}

/// `write_l2` is documented "(async, non-blocking)" and `put` calls it under the
/// comment "Async write to L2", but it is plainly `await`ed: `put` does not
/// return until the graph has answered. The proof is that the `MERGE` is already
/// in the server's log the instant `put` returns, with nothing awaited in
/// between — a detached `tokio::spawn` would not give that guarantee. The naming
/// matters because `put` sits on the request path: a slow Neo4j slows every
/// cached completion.
#[tokio::test]
async fn put_waits_for_the_l2_write_despite_being_documented_as_non_blocking() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(&bolt, TieredCacheConfig::default()).await;

    cache.put("k".to_string(), response("r-1")).await;

    assert_eq!(
        bolt.count_runs_matching("MERGE (c:NexusCacheEntry {key: $key})"),
        1,
        "the round trip had already completed when put() returned: {:?}",
        bolt.cypher()
    );
}

/// A refused L2 write is logged and dropped. `put` returns `()`, so the caller is
/// told nothing; L1 still holds the value, which is why the next read succeeds
/// and the lost persistence only shows up after a restart.
#[tokio::test]
async fn a_refused_l2_write_is_swallowed_and_l1_keeps_the_value() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(&bolt, TieredCacheConfig::default()).await;
    bolt.failing("the database is read only");

    cache.put("k".to_string(), response("r-1")).await;

    assert_eq!(
        cache.get("k").await.expect("L1 still has it").id,
        "r-1",
        "the failure is invisible to the caller"
    );
    assert_eq!(cache.extended_stats().l1_hits, 1);
    assert_eq!(bolt.count_runs_matching("MERGE"), 1, "it was attempted");
}

#[tokio::test]
async fn a_disabled_l2_is_never_written_to() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(
        &bolt,
        TieredCacheConfig {
            l2_enabled: false,
            ..TieredCacheConfig::default()
        },
    )
    .await;

    cache.put("k".to_string(), response("r-1")).await;

    assert!(bolt.cypher().is_empty(), "{:?}", bolt.cypher());
    assert_eq!(cache.extended_stats().l1_entries, 1, "L1 still took it");
}

// ---------------------------------------------------------------------------
// init_l2_schema
// ---------------------------------------------------------------------------

#[tokio::test]
async fn init_l2_schema_creates_the_cache_key_constraint() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(&bolt, TieredCacheConfig::default()).await;

    cache.init_l2_schema().await.expect("schema");

    let cypher = bolt.cypher();
    assert_eq!(cypher.len(), 1, "{cypher:?}");
    assert!(
        cypher[0].contains("CONSTRAINT nexus_cache_key"),
        "{cypher:?}"
    );
    assert!(cypher[0].contains("c.key IS UNIQUE"), "{cypher:?}");
    assert!(
        cypher[0].contains("IF NOT EXISTS"),
        "re-running must stay a no-op: {cypher:?}"
    );
}

/// Like `Neo4jClient::init_schema`, this one turns any failure into a `debug!`
/// and still returns `Ok(())`, then logs "L2 cache schema initialized". A caller
/// that bootstraps the cache cannot tell that the uniqueness constraint is
/// missing — and without it, `MERGE` on `key` can duplicate rows.
#[tokio::test]
async fn init_l2_schema_reports_success_even_when_the_constraint_was_refused() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(&bolt, TieredCacheConfig::default()).await;
    bolt.failing("this account may not create constraints");

    cache
        .init_l2_schema()
        .await
        .expect("the error is swallowed");

    assert_eq!(bolt.cypher().len(), 1, "it was attempted");
}

// ---------------------------------------------------------------------------
// cleanup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cleanup_adds_the_l2_deletions_to_the_l1_ones() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(&bolt, l1_expires_immediately()).await;
    cache.put("a".to_string(), response("r-a")).await;
    cache.put("b".to_string(), response("r-b")).await;
    bolt.forget_runs();
    bolt.returning("deleted", vec![pack_int(4)]);

    let removed = cache.cleanup().await.expect("cleanup");

    assert_eq!(removed, 6, "2 expired in L1 + 4 reported by L2");
    assert_eq!(cache.extended_stats().l1_entries, 0);
    let run = bolt.run_matching("MATCH (c:NexusCacheEntry)");
    assert!(
        run.cypher.contains("WHERE c.expires_at < datetime()"),
        "only expired rows: {}",
        run.cypher
    );
    assert!(
        run.cypher.contains("RETURN count(c) as deleted"),
        "{}",
        run.cypher
    );
}

/// The whole L2 branch of `cleanup` is one `if let Ok(..) && let Ok(..) && let
/// Ok(..)` chain, so a refused delete is not merely unlogged — it is invisible.
/// `cleanup` returns the L1 count and `Ok(())`, and a caller watching that number
/// concludes the L2 tier is empty of expired rows when nothing was even deleted.
#[tokio::test]
async fn cleanup_hides_a_refused_l2_delete_behind_the_l1_count() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(&bolt, l1_expires_immediately()).await;
    cache.put("a".to_string(), response("r-a")).await;
    bolt.forget_runs();
    bolt.failing("the database is read only");

    let removed = cache.cleanup().await.expect("the L2 error is swallowed");

    assert_eq!(removed, 1, "only the single L1 entry is accounted for");
    assert_eq!(
        bolt.count_runs_matching("MATCH (c:NexusCacheEntry)"),
        1,
        "the delete was attempted and refused"
    );
}

/// A `DELETE` that returns no row at all takes the same silent path.
#[tokio::test]
async fn cleanup_counts_nothing_from_l2_when_the_delete_returns_no_row() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(&bolt, TieredCacheConfig::default()).await;
    bolt.returning_nothing();

    assert_eq!(cache.cleanup().await.expect("cleanup"), 0);
    assert_eq!(bolt.count_runs_matching("MATCH (c:NexusCacheEntry)"), 1);
}

// ---------------------------------------------------------------------------
// stats
// ---------------------------------------------------------------------------

/// `stats()` folds both tiers into one `total_hits`, and `total_entries` counts
/// L1 only — a key that lives in L2 and was never read is simply not reported.
#[tokio::test]
async fn stats_counts_l2_hits_towards_total_hits_but_not_l2_entries() {
    let bolt = FakeBolt::start().await;
    let cache = cache_with_l2(&bolt, l1_expires_immediately()).await;
    bolt.returning("response", vec![stored_response("r-1")]);

    cache.get("k").await.expect("from L2");
    cache
        .get("k")
        .await
        .expect("from L2 again, L1 expired at once");

    let stats = cache.stats().await;
    assert_eq!(stats.total_hits, 2, "both were L2 hits");
    assert_eq!(cache.extended_stats().l2_hits, 2);
    assert_eq!(cache.extended_stats().l1_hits, 0);
    assert!(stats.enabled);
}
