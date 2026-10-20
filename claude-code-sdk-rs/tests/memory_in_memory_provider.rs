//! Contract of the two network-free providers, and of `ContextInjector`
//! running on a provider the host built (`with_provider`).
//!
//! `InMemoryMemoryProvider` is what a host's tests drive instead of a
//! `wiremock` Meilisearch, so what is pinned here is what such a test may
//! rely on: upsert on `id`, lexical hits with a score, the conversation
//! filter and the two sort keys of a paginated read, the ranking pipeline
//! shared with the Meilisearch provider, and the no-op provider's silence.

#![cfg(feature = "memory")]

use nexus_claude::memory::{
    ContextInjector, ConversationDocument, ConversationSearchGroup, GetMessagesOptions,
    InMemoryMemoryProvider, MemoryConfig, MemoryProvider, MessageDocument, MessageSearchOptions,
    MessageSort, NoopMemoryProvider, QueryContext,
};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A config that reads no environment variable.
fn config() -> MemoryConfig {
    MemoryConfig {
        meilisearch_url: "http://127.0.0.1:1".to_string(),
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

fn msg(id: &str, conv: &str, turn: usize, created_at: i64, content: &str) -> MessageDocument {
    MessageDocument::new(id, conv, "user", content, turn, created_at)
}

/// `new` stamps `updated_at = created_at`, which is all these tests need.
fn conv(id: &str, updated_at: i64) -> ConversationDocument {
    ConversationDocument::new(id, "preview", "model", updated_at)
}

/// Three messages across two conversations, stored out of chronological
/// order on purpose.
async fn seeded() -> InMemoryMemoryProvider {
    let provider = InMemoryMemoryProvider::new(config());
    provider
        .store_messages(&[
            msg("m2", "conv-a", 1, 1_700_000_200, "la rotation des clés jwt"),
            msg("m1", "conv-a", 0, 1_700_000_100, "bonjour"),
            msg("m3", "conv-b", 0, 1_700_000_300, "jwt expiré"),
        ])
        .await
        .unwrap();
    provider
}

fn ids(messages: &[MessageDocument]) -> Vec<&str> {
    messages.iter().map(|m| m.id.as_str()).collect()
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn store_upserts_on_id_like_a_primary_key() {
    let provider = InMemoryMemoryProvider::new(config());
    provider
        .store_message(&msg("m1", "conv-a", 0, 1, "v1"))
        .await
        .unwrap();
    provider
        .store_message(&msg("m1", "conv-a", 0, 1, "v2"))
        .await
        .unwrap();

    let stored = provider.messages();
    assert_eq!(stored.len(), 1, "a second write on the same id replaces");
    assert_eq!(stored[0].content, "v2");
    assert_eq!(
        provider
            .count_conversation_messages("conv-a")
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn update_conversation_upserts_and_list_sorts_on_updated_at_desc() {
    let provider = InMemoryMemoryProvider::new(config());
    provider
        .update_conversation(&conv("c1", 100))
        .await
        .unwrap();
    provider
        .update_conversation(&conv("c2", 300))
        .await
        .unwrap();
    provider
        .update_conversation(&conv("c1", 200))
        .await
        .unwrap();

    let listed = provider.list_conversations(10, 0).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|c| (c.id.as_str(), c.updated_at))
            .collect::<Vec<_>>(),
        [("c2", 300), ("c1", 200)],
        "c1 was replaced, and the most recently updated comes first"
    );
    assert_eq!(provider.list_conversations(1, 1).await.unwrap()[0].id, "c1");
    assert!(provider.health_check().await.unwrap());
}

// ---------------------------------------------------------------------------
// Paginated reads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_conversation_messages_defaults_to_newest_turn_first_within_the_conversation() {
    let provider = seeded().await;

    let page = provider
        .get_conversation_messages("conv-a", None)
        .await
        .unwrap();

    assert_eq!(ids(&page.messages), ["m2", "m1"], "conv-b is filtered out");
    assert_eq!(page.total_count, 2);
    assert!(!page.has_more);
    assert_eq!((page.offset, page.limit), (0, 50));
}

#[tokio::test]
async fn get_conversation_messages_sorts_on_created_at_ascending_and_paginates() {
    let provider = seeded().await;
    provider
        .store_message(&msg(
            "m0",
            "conv-a",
            7,
            1_700_000_000,
            "le plus ancien par horloge",
        ))
        .await
        .unwrap();

    let opts = GetMessagesOptions::new()
        .sort_by(MessageSort::CreatedAt)
        .newest_first(false)
        .limit(2);
    let first = provider
        .get_conversation_messages("conv-a", Some(opts.clone()))
        .await
        .unwrap();
    assert_eq!(
        ids(&first.messages),
        ["m0", "m1"],
        "created_at, not turn_index, orders the page"
    );
    assert_eq!(first.total_count, 3);
    assert!(first.has_more);

    let second = provider
        .get_conversation_messages("conv-a", Some(opts.offset(2)))
        .await
        .unwrap();
    assert_eq!(ids(&second.messages), ["m2"]);
    assert!(!second.has_more);
}

#[tokio::test]
async fn get_conversation_messages_on_an_unknown_conversation_is_an_empty_page() {
    let provider = seeded().await;
    let page = provider
        .get_conversation_messages("nope", None)
        .await
        .unwrap();
    assert!(page.messages.is_empty());
    assert_eq!(page.total_count, 0);
    assert!(!page.has_more);
}

// ---------------------------------------------------------------------------
// search_messages
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_messages_scores_by_the_fraction_of_terms_found_and_groups_by_conversation() {
    let provider = seeded().await;

    let hits = provider
        .search_messages("jwt rotation", &MessageSearchOptions::new())
        .await
        .unwrap();

    assert_eq!(
        hits.iter()
            .map(|h| (h.document.id.as_str(), h.ranking_score))
            .collect::<Vec<_>>(),
        [("m2", 1.0), ("m3", 0.5)],
        "m1 matches no term and is not a hit"
    );

    let groups = ConversationSearchGroup::group(hits);
    assert_eq!(groups.len(), 2);
    assert_eq!(
        (groups[0].conversation_id.as_str(), groups[0].best_score),
        ("conv-a", 1.0)
    );
    assert_eq!(
        (groups[1].conversation_id.as_str(), groups[1].best_score),
        ("conv-b", 0.5)
    );
}

#[tokio::test]
async fn search_messages_honours_the_conversation_filter_offset_and_limit() {
    let provider = seeded().await;

    let only_b = provider
        .search_messages(
            "jwt",
            &MessageSearchOptions::new().conversation_id("conv-b"),
        )
        .await
        .unwrap();
    assert_eq!(only_b.len(), 1);
    assert_eq!(only_b[0].document.id, "m3");

    // An empty query matches everything with score 1.0, newest first.
    let all = provider
        .search_messages("", &MessageSearchOptions::new())
        .await
        .unwrap();
    assert_eq!(
        all.iter()
            .map(|h| h.document.id.as_str())
            .collect::<Vec<_>>(),
        ["m3", "m2", "m1"]
    );
    assert!(all.iter().all(|h| h.ranking_score == 1.0));

    let window = provider
        .search_messages("", &MessageSearchOptions::new().offset(1).limit(1))
        .await
        .unwrap();
    assert_eq!(window.len(), 1);
    assert_eq!(window[0].document.id, "m2");
}

// ---------------------------------------------------------------------------
// retrieve_context: the shared ranking pipeline
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retrieve_context_filters_on_cwd_then_ranks_with_the_relevance_scorer() {
    let provider = InMemoryMemoryProvider::new(config());
    provider
        .store_messages(&[
            msg("here", "conv-a", 0, 1_700_000_000, "jwt ici").with_cwd("/w"),
            msg("elsewhere", "conv-b", 0, 1_700_000_000, "jwt ailleurs").with_cwd("/other"),
            msg("nowhere", "conv-c", 0, 1_700_000_000, "jwt sans cwd"),
        ])
        .await
        .unwrap();

    let results = provider
        .retrieve_context(&QueryContext::new("jwt").with_cwd("/w"), 5)
        .await
        .unwrap();

    assert_eq!(
        results
            .iter()
            .map(|r| r.document.id.as_str())
            .collect::<Vec<_>>(),
        ["here"],
        "a cwd in the query is an exact filter, as with Meilisearch"
    );
    // The lexical score feeds the semantic factor through the same
    // normalisation as a Meilisearch score (`semantic_score` halves it).
    let score = &results[0].score;
    assert_eq!(score.semantic, 0.5);
    assert_eq!(score.cwd_match, 1.0);
    assert!(score.total >= 0.3, "{score:?}");
}

#[tokio::test]
async fn retrieve_context_applies_the_threshold() {
    // Without a cwd on either side and with old messages, a full lexical
    // match scores 0.4 * 0.5 = 0.2: under the default 0.3 threshold, so the
    // hit is found and then dropped — exactly as a Meilisearch hit would be.
    let provider = InMemoryMemoryProvider::new(config());
    provider
        .store_message(&msg("m1", "c", 0, 1_700_000_000, "jwt"))
        .await
        .unwrap();

    let dropped = provider
        .retrieve_context(&QueryContext::new("jwt"), 5)
        .await
        .unwrap();
    assert!(dropped.is_empty(), "{dropped:?}");

    let mut lenient = config();
    lenient.min_relevance_score = 0.0;
    let provider = InMemoryMemoryProvider::new(lenient);
    provider
        .store_message(&msg("m1", "c", 0, 1_700_000_000, "jwt"))
        .await
        .unwrap();
    let kept = provider
        .retrieve_context(&QueryContext::new("jwt"), 5)
        .await
        .unwrap();
    assert_eq!(kept.len(), 1);

    let none = provider
        .retrieve_context(&QueryContext::new("inexistant"), 5)
        .await
        .unwrap();
    assert!(none.is_empty(), "no lexical hit, nothing to rank");
}

#[tokio::test]
async fn retrieve_context_caps_to_max_context_items_then_fits_the_token_budget() {
    let mut cfg = config();
    cfg.min_relevance_score = 0.0;
    cfg.max_context_items = 2;
    cfg.token_budget = 5; // 20 characters
    let provider = InMemoryMemoryProvider::new(cfg);
    provider
        .store_messages(&[
            msg("short", "c", 0, 1_700_000_300, "jwt"),
            msg(
                "long",
                "c",
                1,
                1_700_000_200,
                "jwt jwt jwt jwt jwt jwt jwt jwt jwt jwt",
            ),
            msg("third", "c", 2, 1_700_000_100, "jwt aussi"),
        ])
        .await
        .unwrap();

    let results = provider
        .retrieve_context(&QueryContext::new("jwt"), 5)
        .await
        .unwrap();

    // Three equal lexical hits, newest first: the cap keeps `short` and
    // `long`; `short` fits the 20-char budget, then the 39-char `long` does
    // not and is skipped (something is already kept, so the "keep one anyway"
    // branch does not fire). `third` never reaches the budget.
    assert_eq!(
        results
            .iter()
            .map(|r| r.document.id.as_str())
            .collect::<Vec<_>>(),
        ["short"]
    );
}

// ---------------------------------------------------------------------------
// NoopMemoryProvider
// ---------------------------------------------------------------------------

#[tokio::test]
async fn noop_provider_accepts_every_write_and_answers_every_read_empty() {
    let provider = NoopMemoryProvider;
    provider
        .store_message(&msg("m1", "c", 0, 1, "x"))
        .await
        .unwrap();
    provider
        .store_messages(&[msg("m2", "c", 1, 2, "y")])
        .await
        .unwrap();
    provider.update_conversation(&conv("c", 1)).await.unwrap();

    assert!(provider.health_check().await.unwrap());
    assert!(
        provider
            .retrieve_context(&QueryContext::new("x"), 5)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        provider
            .search_messages("x", &MessageSearchOptions::new())
            .await
            .unwrap()
            .is_empty()
    );
    let page = provider
        .get_conversation_messages("c", Some(GetMessagesOptions::new().limit(3).offset(9)))
        .await
        .unwrap();
    assert!(page.messages.is_empty());
    assert_eq!(
        (page.total_count, page.has_more, page.offset, page.limit),
        (0, false, 9, 3)
    );
    assert_eq!(provider.count_conversation_messages("c").await.unwrap(), 0);
    assert!(provider.list_conversations(10, 0).await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// ContextInjector::with_provider
// ---------------------------------------------------------------------------

#[tokio::test]
async fn with_provider_runs_the_injector_on_the_host_provider() {
    let provider = Arc::new(InMemoryMemoryProvider::new(config()));
    let injector = ContextInjector::with_provider(provider.clone(), config());

    injector
        .store_messages(&[
            msg("m1", "conv-a", 0, 1_700_000_000, "la rotation des clés jwt").with_cwd("/w"),
        ])
        .await
        .unwrap();
    assert_eq!(
        provider.messages().len(),
        1,
        "the write went to the very provider the host handed over"
    );

    let prefix = injector
        .get_context_prefix("jwt", Some("/w"), &[])
        .await
        .unwrap()
        .expect("one relevant message yields a prefix");
    assert!(prefix.contains("la rotation des clés jwt"), "{prefix}");
    assert!(prefix.starts_with("## Contexte historique"), "{prefix}");

    let loaded = injector
        .load_conversation("conv-a", None, None)
        .await
        .unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(
        injector
            .count_conversation_messages("conv-a")
            .await
            .unwrap(),
        1
    );
    assert!(injector.provider().health_check().await.unwrap());
}

#[tokio::test]
async fn with_provider_still_honours_a_disabled_config() {
    let provider = Arc::new(InMemoryMemoryProvider::new(config()));
    let injector = ContextInjector::with_provider(provider.clone(), config().with_enabled(false));

    injector
        .store_messages(&[msg("m1", "conv-a", 0, 1, "jwt")])
        .await
        .unwrap();
    assert!(
        provider.messages().is_empty(),
        "the disabled guard runs before the provider"
    );
    assert_eq!(
        injector.get_context_prefix("jwt", None, &[]).await.unwrap(),
        None
    );
}
