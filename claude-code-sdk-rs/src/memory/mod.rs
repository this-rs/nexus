//! # Memory Module for Claude Code SDK
//!
//! This module provides persistent memory capabilities for conversations,
//! enabling context retrieval across sessions.
//!
//! ## Architecture
//!
//! The memory system uses a multi-factor scoring approach:
//! - **Semantic**: Text similarity via Meilisearch
//! - **CWD Match**: Same working directory bonus
//! - **Files Overlap**: Common files between conversations
//! - **Recency**: Exponential time decay
//!
//! ## Components
//!
//! - `MessageDocument`: Persistent message storage format
//! - `ToolContextExtractor`: Extracts context from tool calls
//! - `RelevanceScorer`: Multi-factor relevance scoring
//! - `MemoryProvider`: Unified memory access trait, implemented by
//!   `MeilisearchMemoryProvider`, `InMemoryMemoryProvider` and
//!   `NoopMemoryProvider`; `ContextInjector` holds it as `Arc<dyn MemoryProvider>`
//!
//! ## Feature `memory`
//!
//! Everything provider-side (the trait, its error type, the three
//! implementations, `ContextInjector`) is gated by the `memory` cargo feature,
//! which pulls `meilisearch-sdk` and `chrono`. The in-memory and no-op
//! providers need no Meilisearch *server*, only the feature.

mod integration;
mod message_document;
mod scoring;
mod tool_context;

pub use integration::{ConversationMemoryManager, MemoryIntegrationBuilder, SummaryGenerator};
pub use message_document::{ConversationDocument, MemoryConfig, MessageDocument};
pub use scoring::{RelevanceConfig, RelevanceScore, RelevanceScorer};
pub use tool_context::{
    DefaultToolContextExtractor, MessageContextAggregator, ToolContext, ToolContextExtractor,
};

#[cfg(not(feature = "memory"))]
pub use integration::QueryContext;

#[cfg(feature = "memory")]
mod in_memory;
#[cfg(feature = "memory")]
mod provider;

#[cfg(feature = "memory")]
pub use in_memory::{InMemoryMemoryProvider, NoopMemoryProvider};
#[cfg(feature = "memory")]
pub use provider::{
    ContextFormatter, ConversationSearchGroup, GetMessagesOptions, MeilisearchMemoryProvider,
    MemoryError, MemoryProvider, MemoryProviderBuilder, MemoryResult, MessageSearchHit,
    MessageSearchOptions, MessageSort, PaginatedMessages, QueryContext, ScoredMemoryResult,
};

#[cfg(feature = "memory")]
pub use integration::{ContextInjector, LoadedConversation};
