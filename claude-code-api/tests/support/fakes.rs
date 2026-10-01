//! Fake storage and memory backends.
//!
//! Every fake wraps the crate's own in-memory implementation (so the happy path
//! behaves exactly like production) and adds a *fault table*: a set of operations
//! that must return `Err` instead. That is how a test reaches the error branches
//! of `ConversationManager`, `ShortTermMemory` and `UnifiedMemoryProvider` without
//! a database.
//!
//! ```no_run
//! let store = Arc::new(FakeConversationStore::new());
//! store.fail(ConversationOp::Create, "disk on fire");
//! assert!(store.create(None).await.is_err());
//! ```
//!
//! # Reach of these fakes
//!
//! `ConversationManager<S>` and `ShortTermMemory<S>` are generic over the store,
//! so the fakes drop straight in. The HTTP layer is **not** generic:
//! `ChatState.conversation_manager` and `ConversationState.manager` are typed
//! `Arc<DefaultConversationManager>` = `ConversationManager<InMemoryConversationStore>`.
//! A fake store therefore cannot be injected into the router; see the notes in
//! `support_harness.rs`.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use claude_code_api::core::cache::CacheStats;
use claude_code_api::core::conversation::{Conversation, ConversationMetadata};
use claude_code_api::core::memory::{
    ContextualMemoryProvider, MemoryResult, MemorySource, RelevanceScore,
};
use claude_code_api::core::session_manager::Session;
use claude_code_api::core::storage::{
    CacheStore, ConversationStore, InMemoryCacheConfig, InMemoryCacheStore,
    InMemoryConversationConfig, InMemoryConversationStore, InMemorySessionStore, SessionStore,
};
use claude_code_api::models::openai::{ChatCompletionResponse, ChatMessage};
use parking_lot::Mutex;

// ===========================================================================
// Fault injection
// ===========================================================================

/// Operations of [`ConversationStore`] that can be forced to fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConversationOp {
    Create,
    Get,
    AddMessage,
    UpdateMetadata,
    ListActive,
    CleanupExpired,
    Delete,
}

/// Operations of [`SessionStore`] that can be forced to fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionOp {
    Create,
    Get,
    Update,
    Remove,
    List,
}

/// Operations of [`CacheStore`] that can be forced to misbehave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheOp {
    /// `get` returns `None` even for a stored key.
    Get,
    /// `put` silently drops the entry.
    Put,
    /// `cleanup` returns `Err`.
    Cleanup,
}

struct Faults<Op> {
    failing: Mutex<HashSet<Op>>,
    message: Mutex<String>,
    calls: Mutex<Vec<String>>,
}

impl<Op: Eq + std::hash::Hash> Faults<Op> {
    fn new() -> Self {
        Self {
            failing: Mutex::new(HashSet::new()),
            message: Mutex::new("injected fault".to_string()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn arm(&self, op: Op, message: &str) {
        self.failing.lock().insert(op);
        *self.message.lock() = message.to_string();
    }

    fn disarm(&self, op: &Op) {
        self.failing.lock().remove(op);
    }

    fn armed(&self, op: &Op) -> bool {
        self.failing.lock().contains(op)
    }

    fn error(&self) -> anyhow::Error {
        anyhow::anyhow!("{}", self.message.lock().clone())
    }

    fn record(&self, name: &str) {
        self.calls.lock().push(name.to_string());
    }

    fn recorded(&self) -> Vec<String> {
        self.calls.lock().clone()
    }
}

// ===========================================================================
// FakeConversationStore
// ===========================================================================

/// [`ConversationStore`] that delegates to the real in-memory store and can be
/// forced to fail any single operation.
pub struct FakeConversationStore {
    inner: InMemoryConversationStore,
    faults: Faults<ConversationOp>,
}

impl FakeConversationStore {
    pub fn new() -> Self {
        Self::with_config(InMemoryConversationConfig::default())
    }

    pub fn with_config(config: InMemoryConversationConfig) -> Self {
        Self {
            inner: InMemoryConversationStore::new(config),
            faults: Faults::new(),
        }
    }

    /// Make `op` return `Err(message)` until [`Self::heal`] is called.
    pub fn fail(&self, op: ConversationOp, message: &str) {
        self.faults.arm(op, message);
    }

    pub fn heal(&self, op: ConversationOp) {
        self.faults.disarm(&op);
    }

    /// Names of the trait methods called so far, in order.
    pub fn calls(&self) -> Vec<String> {
        self.faults.recorded()
    }
}

impl Default for FakeConversationStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ConversationStore for FakeConversationStore {
    async fn create(&self, model: Option<String>) -> Result<String> {
        self.faults.record("create");
        if self.faults.armed(&ConversationOp::Create) {
            return Err(self.faults.error());
        }
        self.inner.create(model).await
    }

    async fn get(&self, id: &str) -> Result<Option<Conversation>> {
        self.faults.record("get");
        if self.faults.armed(&ConversationOp::Get) {
            return Err(self.faults.error());
        }
        self.inner.get(id).await
    }

    async fn add_message(&self, id: &str, message: ChatMessage) -> Result<()> {
        self.faults.record("add_message");
        if self.faults.armed(&ConversationOp::AddMessage) {
            return Err(self.faults.error());
        }
        self.inner.add_message(id, message).await
    }

    async fn update_metadata(&self, id: &str, metadata: ConversationMetadata) -> Result<()> {
        self.faults.record("update_metadata");
        if self.faults.armed(&ConversationOp::UpdateMetadata) {
            return Err(self.faults.error());
        }
        self.inner.update_metadata(id, metadata).await
    }

    async fn list_active(&self) -> Result<Vec<(String, DateTime<Utc>)>> {
        self.faults.record("list_active");
        if self.faults.armed(&ConversationOp::ListActive) {
            return Err(self.faults.error());
        }
        self.inner.list_active().await
    }

    async fn cleanup_expired(&self, timeout_minutes: i64) -> Result<usize> {
        self.faults.record("cleanup_expired");
        if self.faults.armed(&ConversationOp::CleanupExpired) {
            return Err(self.faults.error());
        }
        self.inner.cleanup_expired(timeout_minutes).await
    }

    async fn delete(&self, id: &str) -> Result<bool> {
        self.faults.record("delete");
        if self.faults.armed(&ConversationOp::Delete) {
            return Err(self.faults.error());
        }
        self.inner.delete(id).await
    }
}

// ===========================================================================
// FakeSessionStore
// ===========================================================================

/// [`SessionStore`] with the same fault table as [`FakeConversationStore`].
pub struct FakeSessionStore {
    inner: InMemorySessionStore,
    faults: Faults<SessionOp>,
}

impl FakeSessionStore {
    pub fn new() -> Self {
        Self {
            inner: InMemorySessionStore::new(),
            faults: Faults::new(),
        }
    }

    pub fn fail(&self, op: SessionOp, message: &str) {
        self.faults.arm(op, message);
    }

    pub fn heal(&self, op: SessionOp) {
        self.faults.disarm(&op);
    }

    pub fn calls(&self) -> Vec<String> {
        self.faults.recorded()
    }
}

impl Default for FakeSessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionStore for FakeSessionStore {
    async fn create(&self, project_path: Option<String>) -> Result<String> {
        self.faults.record("create");
        if self.faults.armed(&SessionOp::Create) {
            return Err(self.faults.error());
        }
        self.inner.create(project_path).await
    }

    async fn get(&self, id: &str) -> Result<Option<Session>> {
        self.faults.record("get");
        if self.faults.armed(&SessionOp::Get) {
            return Err(self.faults.error());
        }
        self.inner.get(id).await
    }

    async fn update(&self, id: &str) -> Result<()> {
        self.faults.record("update");
        if self.faults.armed(&SessionOp::Update) {
            return Err(self.faults.error());
        }
        self.inner.update(id).await
    }

    async fn remove(&self, id: &str) -> Result<Option<Session>> {
        self.faults.record("remove");
        if self.faults.armed(&SessionOp::Remove) {
            return Err(self.faults.error());
        }
        self.inner.remove(id).await
    }

    async fn list(&self) -> Result<Vec<Session>> {
        self.faults.record("list");
        if self.faults.armed(&SessionOp::List) {
            return Err(self.faults.error());
        }
        self.inner.list().await
    }
}

// ===========================================================================
// FakeCacheStore
// ===========================================================================

/// [`CacheStore`] that can be made to lose writes or fail cleanup.
///
/// `CacheStore::get` and `put` return no `Result`, so the fault modes are
/// "behave as a cold cache" ([`CacheOp::Get`]), "drop the write"
/// ([`CacheOp::Put`]) and "fail cleanup" ([`CacheOp::Cleanup`]).
pub struct FakeCacheStore {
    inner: InMemoryCacheStore,
    faults: Faults<CacheOp>,
}

impl FakeCacheStore {
    pub fn new() -> Self {
        Self::with_config(InMemoryCacheConfig::default())
    }

    pub fn with_config(config: InMemoryCacheConfig) -> Self {
        Self {
            inner: InMemoryCacheStore::new(config),
            faults: Faults::new(),
        }
    }

    pub fn fail(&self, op: CacheOp, message: &str) {
        self.faults.arm(op, message);
    }

    pub fn heal(&self, op: CacheOp) {
        self.faults.disarm(&op);
    }

    pub fn calls(&self) -> Vec<String> {
        self.faults.recorded()
    }
}

impl Default for FakeCacheStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CacheStore for FakeCacheStore {
    async fn get(&self, key: &str) -> Option<ChatCompletionResponse> {
        self.faults.record("get");
        if self.faults.armed(&CacheOp::Get) {
            return None;
        }
        self.inner.get(key).await
    }

    async fn put(&self, key: String, response: ChatCompletionResponse) {
        self.faults.record("put");
        if self.faults.armed(&CacheOp::Put) {
            return;
        }
        self.inner.put(key, response).await
    }

    async fn stats(&self) -> CacheStats {
        self.faults.record("stats");
        self.inner.stats().await
    }

    async fn cleanup(&self) -> Result<usize> {
        self.faults.record("cleanup");
        if self.faults.armed(&CacheOp::Cleanup) {
            return Err(self.faults.error());
        }
        self.inner.cleanup().await
    }
}

// ===========================================================================
// FakeMemoryProvider
// ===========================================================================

/// [`ContextualMemoryProvider`] returning a scripted answer.
///
/// `UnifiedMemoryProvider::new` takes three boxed providers; this fake stands in
/// for any of the three tiers, including the two (`MediumTermMemory`,
/// `LongTermMemory`) that would otherwise need project-orchestrator or
/// Meilisearch.
pub struct FakeMemoryProvider {
    results: Vec<MemoryResult>,
    fail_with: Option<String>,
    scope: Option<String>,
    queries: Arc<Mutex<Vec<String>>>,
}

impl FakeMemoryProvider {
    /// A provider answering every query with `results`.
    pub fn returning(results: Vec<MemoryResult>) -> Self {
        Self {
            results,
            fail_with: None,
            scope: None,
            queries: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A provider answering with one result per `(id, content, score)` triple.
    pub fn with_texts(texts: &[(&str, &str, f64)]) -> Self {
        Self::returning(
            texts
                .iter()
                .map(|(id, content, score)| {
                    MemoryResult::new(
                        (*id).to_string(),
                        MemorySource::KnowledgeNote {
                            note_id: (*id).to_string(),
                            project_id: None,
                        },
                        (*content).to_string(),
                        RelevanceScore::new(*score, *score, *score),
                        Utc::now(),
                    )
                })
                .collect(),
        )
    }

    /// A provider that fails every call — the way to cover the
    /// "one tier is down" branches of `UnifiedMemoryProvider`.
    pub fn failing(message: &str) -> Self {
        Self {
            results: Vec::new(),
            fail_with: Some(message.to_string()),
            scope: None,
            queries: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Queries this provider was asked, in order.
    pub fn queries(&self) -> Vec<String> {
        self.queries.lock().clone()
    }

    /// A handle to the query log that survives boxing the provider.
    pub fn query_log(&self) -> Arc<Mutex<Vec<String>>> {
        self.queries.clone()
    }

    fn answer(&self, query: &str) -> Result<Vec<MemoryResult>> {
        self.queries.lock().push(query.to_string());
        match &self.fail_with {
            Some(message) => Err(anyhow::anyhow!("{message}")),
            None => Ok(self.results.clone()),
        }
    }
}

#[async_trait]
impl ContextualMemoryProvider for FakeMemoryProvider {
    async fn query(&self, query: &str, limit: usize) -> Result<Vec<MemoryResult>> {
        let mut results = self.answer(query)?;
        results.truncate(limit);
        Ok(results)
    }

    async fn search_context(
        &self,
        query: &str,
        _source_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryResult>> {
        let mut results = self.answer(query)?;
        results.truncate(limit);
        Ok(results)
    }

    async fn get_relevant_decisions(&self, topic: &str, limit: usize) -> Result<Vec<MemoryResult>> {
        let mut results = self.answer(topic)?;
        results.truncate(limit);
        Ok(results)
    }

    fn current_scope(&self) -> Option<String> {
        self.scope.clone()
    }

    fn set_scope(&mut self, scope: Option<String>) {
        self.scope = scope;
    }
}
