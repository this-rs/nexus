//! `ShortTermMemory` against a conversation store that fails.
//!
//! The test double lives here rather than in a `#[cfg(test)] mod tests` block
//! inside `src/core/memory/short_term.rs` for a measurement reason worth
//! recording: `ShortTermMemory<S>` is generic, so a store type declared under a
//! path segment named `tests` puts `tests` into the mangled symbol name of every
//! *monomorphised production method*. `scripts/coverage_logic_only.py` buckets
//! lines by symbol name, so it then files the whole body of
//! `ShortTermMemory::query` as test code — which quietly took the file's
//! logic-line denominator from 73 down to 13. Declaring the double in an
//! integration test keeps the production symbols production and keeps the
//! double's own `unimplemented!()` arms out of the report (`tests/` is ignored).

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use claude_code_api::core::conversation::{Conversation, ConversationMetadata};
use claude_code_api::core::memory::{ContextualMemoryProvider, ShortTermMemory};
use claude_code_api::core::storage::ConversationStore;
use claude_code_api::models::openai::ChatMessage;
use std::sync::Arc;

/// A store whose `get` always fails. `ShortTermMemory` only ever reads, so the
/// remaining methods are unreachable from it.
struct FailingStore;

#[async_trait]
impl ConversationStore for FailingStore {
    async fn create(&self, _model: Option<String>) -> Result<String> {
        unimplemented!("not called by ShortTermMemory")
    }

    async fn get(&self, _id: &str) -> Result<Option<Conversation>> {
        Err(anyhow::anyhow!("neo4j is down"))
    }

    async fn add_message(&self, _id: &str, _message: ChatMessage) -> Result<()> {
        unimplemented!("not called by ShortTermMemory")
    }

    async fn update_metadata(&self, _id: &str, _metadata: ConversationMetadata) -> Result<()> {
        unimplemented!("not called by ShortTermMemory")
    }

    async fn list_active(&self) -> Result<Vec<(String, DateTime<Utc>)>> {
        unimplemented!("not called by ShortTermMemory")
    }

    async fn cleanup_expired(&self, _timeout_minutes: i64) -> Result<usize> {
        unimplemented!("not called by ShortTermMemory")
    }

    async fn delete(&self, _id: &str) -> Result<bool> {
        unimplemented!("not called by ShortTermMemory")
    }
}

/// A store failure must surface as an error, not be flattened into "this
/// conversation has no memory" — which is what an unset conversation id and an
/// unknown conversation both legitimately produce.
#[tokio::test]
async fn query_propagates_a_store_failure() {
    let memory = ShortTermMemory::new(Arc::new(FailingStore)).with_conversation("c1".to_string());

    let error = memory
        .query("authentication", 10)
        .await
        .expect_err("a store failure must not be swallowed");
    assert_eq!(error.to_string(), "neo4j is down");
}

/// The same for `search_context`, which forwards to `query`.
#[tokio::test]
async fn search_context_propagates_a_store_failure() {
    let memory = ShortTermMemory::new(Arc::new(FailingStore)).with_conversation("c1".to_string());

    let error = memory
        .search_context("authentication", Some("conversation"), 10)
        .await
        .expect_err("a store failure must not be swallowed");
    assert_eq!(error.to_string(), "neo4j is down");
}

/// …but a filter the short-term level cannot serve short-circuits *before* the
/// store is touched, so a broken store is invisible there.
#[tokio::test]
async fn an_unservable_filter_answers_empty_without_touching_the_store() {
    let memory = ShortTermMemory::new(Arc::new(FailingStore)).with_conversation("c1".to_string());

    let results = memory
        .search_context("authentication", Some("plan"), 10)
        .await
        .expect("the store is never reached");
    assert!(results.is_empty());
}
