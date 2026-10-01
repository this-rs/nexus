//! Traits for the contextual memory system

#![allow(dead_code)] // Public API - may not be used internally

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Source of a memory result
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemorySource {
    /// Current conversation (short-term)
    Conversation {
        conversation_id: String,
        message_index: usize,
    },
    /// Plan/task/decision from project-orchestrator (medium-term)
    ProjectOrchestrator {
        entity_type: String, // "plan", "task", "decision", "note"
        entity_id: String,
    },
    /// Cross-conversation search result (long-term)
    CrossConversation {
        conversation_id: String,
        message_id: String,
    },
    /// Knowledge note (long-term)
    KnowledgeNote {
        note_id: String,
        project_id: Option<String>,
    },
}

impl MemorySource {
    /// Get the memory level (1=short, 2=medium, 3=long)
    pub fn level(&self) -> u8 {
        match self {
            MemorySource::Conversation { .. } => 1,
            MemorySource::ProjectOrchestrator { .. } => 2,
            MemorySource::CrossConversation { .. } | MemorySource::KnowledgeNote { .. } => 3,
        }
    }
}

/// Relevance score for ranking memory results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelevanceScore {
    /// Semantic similarity score (0.0 - 1.0)
    pub semantic: f64,
    /// Recency score (0.0 - 1.0, higher = more recent)
    pub recency: f64,
    /// Scope match score (0.0 - 1.0, higher = more specific scope)
    pub scope: f64,
    /// Combined weighted score
    pub combined: f64,
}

impl RelevanceScore {
    /// Create a new relevance score with default weights
    pub fn new(semantic: f64, recency: f64, scope: f64) -> Self {
        // Default weights: semantic 50%, recency 30%, scope 20%
        let combined = semantic * 0.5 + recency * 0.3 + scope * 0.2;
        Self {
            semantic,
            recency,
            scope,
            combined,
        }
    }

    /// Create with custom weights
    pub fn with_weights(semantic: f64, recency: f64, scope: f64, weights: (f64, f64, f64)) -> Self {
        let combined = semantic * weights.0 + recency * weights.1 + scope * weights.2;
        Self {
            semantic,
            recency,
            scope,
            combined,
        }
    }
}

impl Default for RelevanceScore {
    fn default() -> Self {
        Self {
            semantic: 0.0,
            recency: 0.0,
            scope: 0.0,
            combined: 0.0,
        }
    }
}

/// A memory result from any level
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryResult {
    /// Unique identifier
    pub id: String,
    /// Source of this result
    pub source: MemorySource,
    /// Content text
    pub content: String,
    /// Optional title or summary
    pub title: Option<String>,
    /// Relevance score
    pub score: RelevanceScore,
    /// When this was created/updated
    pub timestamp: DateTime<Utc>,
    /// Additional metadata
    pub metadata: serde_json::Value,
}

impl MemoryResult {
    /// Create a new memory result
    pub fn new(
        id: String,
        source: MemorySource,
        content: String,
        score: RelevanceScore,
        timestamp: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            source,
            content,
            title: None,
            score,
            timestamp,
            metadata: serde_json::Value::Null,
        }
    }

    /// Add a title
    pub fn with_title(mut self, title: String) -> Self {
        self.title = Some(title);
        self
    }

    /// Add metadata
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Trait for contextual memory providers
///
/// Implementations provide access to different memory levels:
/// - Short-term: current conversation
/// - Medium-term: plans/tasks/decisions
/// - Long-term: cross-conversation search + knowledge notes
#[async_trait]
pub trait ContextualMemoryProvider: Send + Sync {
    /// Query the memory for relevant context
    ///
    /// # Arguments
    ///
    /// * `query` - Natural language query (e.g., "What did we decide about auth?")
    /// * `limit` - Maximum number of results to return
    ///
    /// # Returns
    ///
    /// Vector of memory results sorted by relevance score (descending)
    async fn query(&self, query: &str, limit: usize) -> Result<Vec<MemoryResult>>;

    /// Search for specific context by type
    ///
    /// # Arguments
    ///
    /// * `query` - Search query
    /// * `source_filter` - Optional filter by source type
    /// * `limit` - Maximum results
    async fn search_context(
        &self,
        query: &str,
        source_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryResult>>;

    /// Get relevant decisions for a topic
    ///
    /// Specifically searches for architectural decisions and choices
    async fn get_relevant_decisions(&self, topic: &str, limit: usize) -> Result<Vec<MemoryResult>>;

    /// Get the current context scope (project, workspace, etc.)
    fn current_scope(&self) -> Option<String>;

    /// Set the current context scope
    fn set_scope(&mut self, scope: Option<String>);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_source_level() {
        assert_eq!(
            MemorySource::Conversation {
                conversation_id: "c1".to_string(),
                message_index: 0
            }
            .level(),
            1
        );
        assert_eq!(
            MemorySource::ProjectOrchestrator {
                entity_type: "task".to_string(),
                entity_id: "t1".to_string()
            }
            .level(),
            2
        );
        assert_eq!(
            MemorySource::KnowledgeNote {
                note_id: "n1".to_string(),
                project_id: None
            }
            .level(),
            3
        );
    }

    /// `CrossConversation` and `KnowledgeNote` share the level-3 arm.
    #[test]
    fn cross_conversation_is_also_level_three() {
        assert_eq!(
            MemorySource::CrossConversation {
                conversation_id: "c1".to_string(),
                message_id: "m1".to_string(),
            }
            .level(),
            3
        );
    }

    /// `MemorySource` is externally tagged, so the variant name is part of the
    /// wire format — renaming one would silently break stored results.
    #[test]
    fn memory_source_round_trips_through_json() {
        let source = MemorySource::KnowledgeNote {
            note_id: "n1".to_string(),
            project_id: Some("p1".to_string()),
        };
        let json = serde_json::to_value(&source).expect("serializable");
        assert_eq!(
            json,
            serde_json::json!({"KnowledgeNote": {"note_id": "n1", "project_id": "p1"}})
        );
        let back: MemorySource = serde_json::from_value(json).expect("deserializable");
        assert_eq!(back, source);
    }

    /// The default weights are 50 / 30 / 20 and they sum to 1, so `combined`
    /// stays inside the range of its components.
    #[test]
    fn test_relevance_score() {
        let score = RelevanceScore::new(0.8, 0.6, 0.4);
        // 0.8 * 0.5 + 0.6 * 0.3 + 0.4 * 0.2 = 0.4 + 0.18 + 0.08 = 0.66
        assert!((score.combined - 0.66).abs() < 0.001);
        assert!((score.semantic - 0.8).abs() < f64::EPSILON);
        assert!((score.recency - 0.6).abs() < f64::EPSILON);
        assert!((score.scope - 0.4).abs() < f64::EPSILON);

        let perfect = RelevanceScore::new(1.0, 1.0, 1.0);
        assert!((perfect.combined - 1.0).abs() < 1e-9);
    }

    /// `with_weights` applies the triple verbatim — it neither normalises it nor
    /// clamps the product, so weights that do not sum to 1 push `combined`
    /// outside `0.0..=1.0`. Pinned because `UnifiedMemoryProvider` relies on
    /// exactly that to boost a level above 1.0.
    #[test]
    fn with_weights_applies_the_triple_verbatim() {
        let score = RelevanceScore::with_weights(0.8, 0.6, 0.4, (1.0, 1.0, 1.0));
        assert!((score.combined - 1.8).abs() < 1e-9);
        assert!((score.semantic - 0.8).abs() < f64::EPSILON);

        // All the weight on recency.
        let recency_only = RelevanceScore::with_weights(1.0, 0.25, 1.0, (0.0, 1.0, 0.0));
        assert!((recency_only.combined - 0.25).abs() < 1e-9);

        // Zero weights give a zero score without touching the components.
        let muted = RelevanceScore::with_weights(0.9, 0.9, 0.9, (0.0, 0.0, 0.0));
        assert!(muted.combined.abs() < f64::EPSILON);
        assert!((muted.scope - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn default_relevance_score_is_all_zeros() {
        let score = RelevanceScore::default();
        assert!(score.semantic.abs() < f64::EPSILON);
        assert!(score.recency.abs() < f64::EPSILON);
        assert!(score.scope.abs() < f64::EPSILON);
        assert!(score.combined.abs() < f64::EPSILON);
    }

    /// `Deserialize` is derived, so `combined` is read from the wire rather than
    /// recomputed: a stored score can disagree with its own components. Worth
    /// knowing before trusting a `combined` that did not come from `new`.
    #[test]
    fn a_deserialized_score_keeps_an_inconsistent_combined() {
        let score: RelevanceScore = serde_json::from_value(serde_json::json!({
            "semantic": 0.0,
            "recency": 0.0,
            "scope": 0.0,
            "combined": 99.0,
        }))
        .expect("deserializable");
        assert!((score.combined - 99.0).abs() < f64::EPSILON);
    }

    #[test]
    fn memory_result_starts_untitled_with_null_metadata() {
        let result = MemoryResult::new(
            "r1".to_string(),
            MemorySource::Conversation {
                conversation_id: "c1".to_string(),
                message_index: 2,
            },
            "content".to_string(),
            RelevanceScore::default(),
            Utc::now(),
        );

        assert_eq!(result.title, None);
        assert_eq!(result.metadata, serde_json::Value::Null);

        let decorated = result
            .with_title("A title".to_string())
            .with_metadata(serde_json::json!({"role": "user"}));
        assert_eq!(decorated.title, Some("A title".to_string()));
        assert_eq!(decorated.metadata["role"], serde_json::json!("user"));
        // The builders keep everything else intact.
        assert_eq!(decorated.id, "r1");
        assert_eq!(decorated.content, "content");
        assert_eq!(decorated.source.level(), 1);
    }

    /// The last call wins — `with_metadata` replaces, it does not merge.
    #[test]
    fn with_metadata_replaces_rather_than_merges() {
        let result = MemoryResult::new(
            "r1".to_string(),
            MemorySource::ProjectOrchestrator {
                entity_type: "task".to_string(),
                entity_id: "t1".to_string(),
            },
            "content".to_string(),
            RelevanceScore::default(),
            Utc::now(),
        )
        .with_metadata(serde_json::json!({"a": 1}))
        .with_metadata(serde_json::json!({"b": 2}));

        assert_eq!(result.metadata, serde_json::json!({"b": 2}));
    }
}
