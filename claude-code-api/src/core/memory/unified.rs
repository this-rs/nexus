//! Unified memory provider that aggregates all memory levels
//!
//! Combines short-term, medium-term, and long-term memory with
//! intelligent scoring and deduplication.

#![allow(dead_code, unused_imports)] // Public API - may not be used internally

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashSet;
use tracing::{debug, warn};

use super::traits::{ContextualMemoryProvider, MemoryResult, MemorySource};

/// Configuration for the unified memory provider
#[derive(Debug, Clone)]
pub struct UnifiedMemoryConfig {
    /// Weight for short-term results (0.0 - 1.0)
    pub short_term_weight: f64,
    /// Weight for medium-term results (0.0 - 1.0)
    pub medium_term_weight: f64,
    /// Weight for long-term results (0.0 - 1.0)
    pub long_term_weight: f64,
    /// Minimum score threshold to include in results
    pub min_score_threshold: f64,
}

impl Default for UnifiedMemoryConfig {
    fn default() -> Self {
        Self {
            short_term_weight: 1.2, // Boost current conversation
            medium_term_weight: 1.0,
            long_term_weight: 0.8, // Slight penalty for old context
            min_score_threshold: 0.1,
        }
    }
}

/// Unified memory provider that aggregates all memory levels
pub struct UnifiedMemoryProvider {
    short_term: Box<dyn ContextualMemoryProvider>,
    medium_term: Box<dyn ContextualMemoryProvider>,
    long_term: Box<dyn ContextualMemoryProvider>,
    config: UnifiedMemoryConfig,
    scope: Option<String>,
}

impl UnifiedMemoryProvider {
    /// Create a new unified memory provider
    pub fn new(
        short_term: Box<dyn ContextualMemoryProvider>,
        medium_term: Box<dyn ContextualMemoryProvider>,
        long_term: Box<dyn ContextualMemoryProvider>,
    ) -> Self {
        Self {
            short_term,
            medium_term,
            long_term,
            config: UnifiedMemoryConfig::default(),
            scope: None,
        }
    }

    /// Create with custom configuration
    pub fn with_config(mut self, config: UnifiedMemoryConfig) -> Self {
        self.config = config;
        self
    }

    /// Apply level-based weight boost to a result
    ///
    /// The match is on the source variant rather than on `MemorySource::level()`
    /// so that it stays exhaustive: keyed on the `u8` level it needed a `_ => 1.0`
    /// arm which no input could reach, and which would have silently given a
    /// future memory level a neutral weight instead of failing to compile.
    fn apply_weight(&self, result: &mut MemoryResult) {
        let weight = match result.source {
            MemorySource::Conversation { .. } => self.config.short_term_weight,
            MemorySource::ProjectOrchestrator { .. } => self.config.medium_term_weight,
            MemorySource::CrossConversation { .. } | MemorySource::KnowledgeNote { .. } => {
                self.config.long_term_weight
            },
        };

        result.score.combined *= weight;
    }

    /// Fold one level's answer into the aggregate.
    ///
    /// A level that fails is recorded in `failures` and logged, never
    /// propagated: a unified provider answers with what the reachable levels
    /// know. The caller decides what to do when *every* level failed.
    fn merge_level(
        &self,
        level: &str,
        answer: Result<Vec<MemoryResult>>,
        into: &mut Vec<MemoryResult>,
        failures: &mut Vec<String>,
    ) {
        match answer {
            Ok(results) => into.extend(results),
            Err(error) => {
                warn!("UnifiedMemory: {} level unavailable: {}", level, error);
                failures.push(format!("{level}: {error}"));
            },
        }
    }

    /// Deduplicate results based on content similarity
    fn deduplicate(&self, results: Vec<MemoryResult>) -> Vec<MemoryResult> {
        let mut seen_content: HashSet<String> = HashSet::new();
        let mut deduped = Vec::new();

        for result in results {
            // Create a normalized version for comparison
            let normalized: String = result
                .content
                .chars()
                .filter(|c| c.is_alphanumeric() || c.is_whitespace())
                .collect::<String>()
                .to_lowercase()
                .split_whitespace()
                .take(20)  // Compare first 20 words
                .collect::<Vec<_>>()
                .join(" ");

            if !seen_content.contains(&normalized) {
                seen_content.insert(normalized);
                deduped.push(result);
            }
        }

        deduped
    }
}

#[async_trait]
impl ContextualMemoryProvider for UnifiedMemoryProvider {
    /// Query every level and merge the answers.
    ///
    /// A level that is down is **skipped**, not fatal. Awaiting each level with
    /// `?` — as this did — meant one unreachable service (Meilisearch for
    /// long-term, project-orchestrator for medium-term) took the whole
    /// aggregated query down, so the gateway lost the current conversation's own
    /// memory because a remote index was unavailable.
    ///
    /// An error is still returned when *every* level failed: a total outage must
    /// not be indistinguishable from "nothing relevant was found".
    async fn query(&self, query: &str, limit: usize) -> Result<Vec<MemoryResult>> {
        // Query all levels in parallel (conceptually - we do it sequentially for simplicity)
        let per_level_limit = limit.max(5);

        let mut all_results = Vec::new();
        let mut failures: Vec<String> = Vec::new();

        let levels: [(&str, &dyn ContextualMemoryProvider); 3] = [
            ("short-term", self.short_term.as_ref()),
            ("medium-term", self.medium_term.as_ref()),
            ("long-term", self.long_term.as_ref()),
        ];
        let level_count = levels.len();

        for (level, provider) in levels {
            let answer = provider.query(query, per_level_limit).await;
            self.merge_level(level, answer, &mut all_results, &mut failures);
        }

        if failures.len() == level_count {
            anyhow::bail!("every memory level failed ({})", failures.join("; "));
        }

        for r in &mut all_results {
            self.apply_weight(r);
        }

        // Filter by minimum score
        all_results.retain(|r| r.score.combined >= self.config.min_score_threshold);

        // Sort by combined score (descending)
        all_results.sort_by(|a, b| {
            b.score
                .combined
                .partial_cmp(&a.score.combined)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Deduplicate similar content
        let deduped = self.deduplicate(all_results);

        let final_results: Vec<MemoryResult> = deduped.into_iter().take(limit).collect();

        debug!(
            "UnifiedMemory: returning {} results for query '{}'",
            final_results.len(),
            query
        );

        Ok(final_results)
    }

    async fn search_context(
        &self,
        query: &str,
        source_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryResult>> {
        match source_filter {
            Some("conversation") => {
                self.short_term
                    .search_context(query, source_filter, limit)
                    .await
            },
            Some("plan") | Some("task") | Some("decision") | Some("note") => {
                self.medium_term
                    .search_context(query, source_filter, limit)
                    .await
            },
            Some("cross_conversation") => {
                self.long_term
                    .search_context(query, source_filter, limit)
                    .await
            },
            _ => self.query(query, limit).await,
        }
    }

    /// Decisions, merged from the two levels that hold any.
    ///
    /// Degrades the same way as [`Self::query`]: a level that fails is logged
    /// and skipped, and only a failure of *both* is reported.
    async fn get_relevant_decisions(&self, topic: &str, limit: usize) -> Result<Vec<MemoryResult>> {
        let mut results = Vec::new();
        let mut failures: Vec<String> = Vec::new();

        // Decisions primarily come from medium-term
        let medium = self.medium_term.get_relevant_decisions(topic, limit).await;
        self.merge_level("medium-term", medium, &mut results, &mut failures);

        // Also check long-term for decision-related discussions
        let long = self
            .long_term
            .query(&format!("decision {}", topic), limit / 2)
            .await;
        self.merge_level("long-term", long, &mut results, &mut failures);

        if failures.len() == 2 {
            anyhow::bail!("every memory level failed ({})", failures.join("; "));
        }

        // Sort and deduplicate
        results.sort_by(|a, b| {
            b.score
                .combined
                .partial_cmp(&a.score.combined)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let deduped = self.deduplicate(results);
        Ok(deduped.into_iter().take(limit).collect())
    }

    fn current_scope(&self) -> Option<String> {
        self.scope.clone()
    }

    fn set_scope(&mut self, scope: Option<String>) {
        self.scope = scope;
        // Propagate to all levels (if they support it)
        // Note: Can't call set_scope on trait objects directly without &mut self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::memory::traits::RelevanceScore;
    use chrono::Utc;
    use std::sync::{Arc, Mutex};

    /// One call recorded by [`MockMemoryProvider`]: method, query, limit.
    type Call = (&'static str, String, usize);

    /// Mock memory provider for testing
    ///
    /// Records every call so a test can assert *which* level the unified
    /// provider dispatched to, and with what query and limit.
    struct MockMemoryProvider {
        results: Vec<MemoryResult>,
        decisions: Vec<MemoryResult>,
        fail_with: Option<&'static str>,
        calls: Arc<Mutex<Vec<Call>>>,
    }

    impl MockMemoryProvider {
        fn new(results: Vec<MemoryResult>) -> Self {
            Self {
                results,
                decisions: Vec::new(),
                fail_with: None,
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_decisions(mut self, decisions: Vec<MemoryResult>) -> Self {
            self.decisions = decisions;
            self
        }

        /// A level that is down — every method returns `Err`.
        fn failing(message: &'static str) -> Self {
            Self {
                results: Vec::new(),
                decisions: Vec::new(),
                fail_with: Some(message),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// A call log handle that survives boxing the provider.
        fn log(&self) -> Arc<Mutex<Vec<Call>>> {
            self.calls.clone()
        }

        fn record(&self, method: &'static str, query: &str, limit: usize) -> Result<()> {
            self.calls.lock().expect("call log is not poisoned").push((
                method,
                query.to_string(),
                limit,
            ));
            match self.fail_with {
                Some(message) => Err(anyhow::anyhow!("{message}")),
                None => Ok(()),
            }
        }
    }

    fn calls_of(log: &Arc<Mutex<Vec<Call>>>) -> Vec<Call> {
        log.lock().expect("call log is not poisoned").clone()
    }

    #[async_trait]
    impl ContextualMemoryProvider for MockMemoryProvider {
        async fn query(&self, query: &str, limit: usize) -> Result<Vec<MemoryResult>> {
            self.record("query", query, limit)?;
            Ok(self.results.iter().take(limit).cloned().collect())
        }

        async fn search_context(
            &self,
            query: &str,
            _source_filter: Option<&str>,
            limit: usize,
        ) -> Result<Vec<MemoryResult>> {
            self.record("search_context", query, limit)?;
            Ok(self.results.iter().take(limit).cloned().collect())
        }

        async fn get_relevant_decisions(
            &self,
            topic: &str,
            limit: usize,
        ) -> Result<Vec<MemoryResult>> {
            self.record("get_relevant_decisions", topic, limit)?;
            Ok(self.decisions.iter().take(limit).cloned().collect())
        }

        fn current_scope(&self) -> Option<String> {
            None
        }

        fn set_scope(&mut self, _scope: Option<String>) {}
    }

    /// `RelevanceScore::new(s, s, s).combined == s`, which keeps the arithmetic
    /// of the weight boosts readable in the assertions below.
    fn scored(id: &str, source: MemorySource, content: &str, score: f64) -> MemoryResult {
        MemoryResult::new(
            id.to_string(),
            source,
            content.to_string(),
            RelevanceScore::new(score, score, score),
            Utc::now(),
        )
    }

    fn conversation_source() -> MemorySource {
        MemorySource::Conversation {
            conversation_id: "c1".to_string(),
            message_index: 0,
        }
    }

    fn orchestrator_source(entity_type: &str) -> MemorySource {
        MemorySource::ProjectOrchestrator {
            entity_type: entity_type.to_string(),
            entity_id: "e1".to_string(),
        }
    }

    fn cross_conversation_source() -> MemorySource {
        MemorySource::CrossConversation {
            conversation_id: "c9".to_string(),
            message_id: "m9".to_string(),
        }
    }

    fn knowledge_note_source() -> MemorySource {
        MemorySource::KnowledgeNote {
            note_id: "n1".to_string(),
            project_id: None,
        }
    }

    // =======================================================================
    // query: weighting, threshold, dedup, limits
    // =======================================================================

    /// The per-level weights reorder the merged list: a short-term hit scoring
    /// 0.50 outranks a long-term hit at 0.70 and a medium-term hit at 0.55.
    #[tokio::test]
    async fn test_unified_memory_query() {
        let short = MockMemoryProvider::new(vec![scored(
            "s1",
            conversation_source(),
            "Short-term result about auth",
            0.50,
        )]);
        let medium = MockMemoryProvider::new(vec![scored(
            "m1",
            orchestrator_source("decision"),
            "Decision about auth: use JWT",
            0.55,
        )]);
        let long = MockMemoryProvider::new(vec![
            scored(
                "l1",
                cross_conversation_source(),
                "Auth came up before",
                0.70,
            ),
            scored("n1", knowledge_note_source(), "Auth runbook", 0.60),
        ]);

        let unified = UnifiedMemoryProvider::new(Box::new(short), Box::new(medium), Box::new(long));

        let results = unified.query("authentication", 10).await.unwrap();
        let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["s1", "l1", "m1", "n1"]);

        // 0.50 * 1.2 (short-term boost), 0.70 * 0.8, 0.55 * 1.0, 0.60 * 0.8.
        let combined: Vec<f64> = results.iter().map(|r| r.score.combined).collect();
        for (got, want) in combined.iter().zip([0.60, 0.56, 0.55, 0.48]) {
            assert!((got - want).abs() < 1e-9, "{combined:?}");
        }
        // The un-weighted components are untouched.
        assert!((results[0].score.semantic - 0.50).abs() < 1e-9);
    }

    #[tokio::test]
    async fn test_unified_memory_deduplication() {
        let short = MockMemoryProvider::new(vec![scored(
            "s1",
            conversation_source(),
            "We should use JWT for authentication",
            0.8,
        )]);
        let medium = MockMemoryProvider::new(vec![scored(
            "m1",
            orchestrator_source("note"),
            // Same words, different punctuation and case: still a duplicate,
            // because `deduplicate` strips non-alphanumerics and lowercases.
            "we should USE jwt, for authentication!",
            0.9,
        )]);
        let long = MockMemoryProvider::new(vec![]);

        let unified = UnifiedMemoryProvider::new(Box::new(short), Box::new(medium), Box::new(long));

        let results = unified.query("JWT", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        // Dedup runs *after* sorting, so the higher-scoring copy is the survivor:
        // 0.9 * 1.0 (medium) < 0.8 * 1.2 (short-term boost) = 0.96.
        assert_eq!(results[0].id, "s1");
    }

    /// `deduplicate` compares only the first twenty words, so two genuinely
    /// different documents that share a twenty-word opening collapse into one.
    /// Pinned as current behaviour, not endorsed: the loser is dropped silently.
    #[tokio::test]
    async fn deduplicate_only_compares_the_first_twenty_words() {
        let prefix = (1..=20)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let short = MockMemoryProvider::new(vec![scored(
            "s1",
            conversation_source(),
            &format!("{prefix} authentication uses JWT"),
            0.8,
        )]);
        let medium = MockMemoryProvider::new(vec![scored(
            "m1",
            orchestrator_source("note"),
            &format!("{prefix} authentication uses mutual TLS"),
            0.9,
        )]);
        let long = MockMemoryProvider::new(vec![]);

        let unified = UnifiedMemoryProvider::new(Box::new(short), Box::new(medium), Box::new(long));

        let results = unified.query("authentication", 10).await.unwrap();
        assert_eq!(results.len(), 1, "the 21st word onwards is never compared");
        assert_eq!(results[0].id, "s1");
    }

    /// `min_score_threshold` is applied to the *weighted* score, and
    /// `with_config` replaces the defaults wholesale.
    #[tokio::test]
    async fn with_config_changes_the_weights_and_the_threshold() {
        let short =
            MockMemoryProvider::new(vec![scored("s1", conversation_source(), "short", 0.40)]);
        let medium = MockMemoryProvider::new(vec![scored(
            "m1",
            orchestrator_source("plan"),
            "medium",
            0.40,
        )]);
        let long = MockMemoryProvider::new(vec![scored(
            "l1",
            cross_conversation_source(),
            "long",
            0.40,
        )]);

        let unified = UnifiedMemoryProvider::new(Box::new(short), Box::new(medium), Box::new(long))
            .with_config(UnifiedMemoryConfig {
                short_term_weight: 0.1,
                medium_term_weight: 1.0,
                long_term_weight: 2.0,
                // 0.40 * 0.1 = 0.04 is below this, the other two are above.
                min_score_threshold: 0.3,
            });

        let results = unified.query("anything", 10).await.unwrap();
        let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["l1", "m1"], "the short-term hit fell below 0.3");
    }

    /// Each level is asked for at least five results whatever `limit` is, so the
    /// aggregate has something to rank before truncating.
    #[tokio::test]
    async fn each_level_is_asked_for_at_least_five_results() {
        let short = MockMemoryProvider::new(vec![
            scored("s1", conversation_source(), "alpha", 0.9),
            scored("s2", conversation_source(), "beta", 0.8),
        ]);
        let log = short.log();
        let unified = UnifiedMemoryProvider::new(
            Box::new(short),
            Box::new(MockMemoryProvider::new(vec![])),
            Box::new(MockMemoryProvider::new(vec![])),
        );

        let results = unified.query("alpha", 1).await.unwrap();
        assert_eq!(results.len(), 1, "the final answer still honours the limit");
        assert_eq!(calls_of(&log), vec![("query", "alpha".to_string(), 5)]);
    }

    // =======================================================================
    // Graceful degradation
    // =======================================================================

    /// A dead level must not take the aggregated query down with it.
    ///
    /// This is the regression test for the `?`-per-level shape: an unreachable
    /// project-orchestrator used to make `query` return `Err`, so the gateway
    /// also lost the short-term memory of the conversation in front of it.
    #[tokio::test]
    async fn one_dead_level_still_yields_the_others() {
        let unified = UnifiedMemoryProvider::new(
            Box::new(MockMemoryProvider::new(vec![scored(
                "s1",
                conversation_source(),
                "court terme",
                0.9,
            )])),
            Box::new(MockMemoryProvider::failing("orchestrator unreachable")),
            Box::new(MockMemoryProvider::new(vec![scored(
                "l1",
                cross_conversation_source(),
                "long terme",
                0.4,
            )])),
        );

        let results = unified
            .query("auth", 10)
            .await
            .expect("a dead level is skipped, not fatal");
        let contents: Vec<&str> = results.iter().map(|r| r.content.as_str()).collect();
        assert_eq!(contents, vec!["court terme", "long terme"]);
    }

    /// The *first* level failing is just as survivable as the middle one.
    #[tokio::test]
    async fn a_dead_short_term_level_does_not_hide_the_remote_levels() {
        let unified = UnifiedMemoryProvider::new(
            Box::new(MockMemoryProvider::failing("conversation store down")),
            Box::new(MockMemoryProvider::new(vec![scored(
                "m1",
                orchestrator_source("task"),
                "moyen terme",
                0.5,
            )])),
            Box::new(MockMemoryProvider::failing("meilisearch down")),
        );

        let results = unified
            .query("auth", 10)
            .await
            .expect("one surviving level is enough");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "m1");
    }

    /// A *total* outage is still an error: an empty answer must not be
    /// indistinguishable from "every backing service is down".
    #[tokio::test]
    async fn every_level_failing_is_reported_as_an_error() {
        let unified = UnifiedMemoryProvider::new(
            Box::new(MockMemoryProvider::failing("store down")),
            Box::new(MockMemoryProvider::failing("orchestrator down")),
            Box::new(MockMemoryProvider::failing("meilisearch down")),
        );

        let error = unified
            .query("auth", 10)
            .await
            .expect_err("a total outage is not an empty result set");
        let message = error.to_string();
        assert!(message.contains("every memory level failed"), "{message}");
        for expected in [
            "short-term: store down",
            "medium-term: orchestrator down",
            "long-term: meilisearch down",
        ] {
            assert!(message.contains(expected), "{message} lacks {expected}");
        }
    }

    /// A level that answers with nothing is not a failure.
    #[tokio::test]
    async fn three_empty_levels_yield_an_empty_answer_not_an_error() {
        let unified = UnifiedMemoryProvider::new(
            Box::new(MockMemoryProvider::new(vec![])),
            Box::new(MockMemoryProvider::new(vec![])),
            Box::new(MockMemoryProvider::new(vec![])),
        );

        assert!(unified.query("auth", 10).await.unwrap().is_empty());
    }

    // =======================================================================
    // search_context dispatch
    // =======================================================================

    #[tokio::test]
    async fn search_context_dispatches_each_filter_to_one_level() {
        let short =
            MockMemoryProvider::new(vec![scored("s1", conversation_source(), "short", 0.9)]);
        let medium = MockMemoryProvider::new(vec![scored(
            "m1",
            orchestrator_source("plan"),
            "medium",
            0.9,
        )]);
        let long =
            MockMemoryProvider::new(vec![scored("l1", cross_conversation_source(), "long", 0.9)]);
        let (short_log, medium_log, long_log) = (short.log(), medium.log(), long.log());

        let unified = UnifiedMemoryProvider::new(Box::new(short), Box::new(medium), Box::new(long));

        assert_eq!(
            unified
                .search_context("q", Some("conversation"), 3)
                .await
                .unwrap()[0]
                .id,
            "s1"
        );
        for filter in ["plan", "task", "decision", "note"] {
            assert_eq!(
                unified.search_context("q", Some(filter), 3).await.unwrap()[0].id,
                "m1"
            );
        }
        assert_eq!(
            unified
                .search_context("q", Some("cross_conversation"), 3)
                .await
                .unwrap()[0]
                .id,
            "l1"
        );

        // `search_context` forwards, it does not weight: the scores are raw.
        assert!(
            (unified
                .search_context("q", Some("conversation"), 3)
                .await
                .unwrap()[0]
                .score
                .combined
                - 0.9)
                .abs()
                < 1e-9
        );

        assert_eq!(
            calls_of(&short_log)
                .iter()
                .filter(|(m, _, _)| *m == "search_context")
                .count(),
            2
        );
        assert_eq!(calls_of(&medium_log).len(), 4);
        assert_eq!(calls_of(&long_log).len(), 1);
    }

    /// An unknown filter — and `None` — falls through to the full aggregation,
    /// which *is* weighted and therefore reorders.
    #[tokio::test]
    async fn an_unknown_filter_falls_back_to_the_full_query() {
        let short =
            MockMemoryProvider::new(vec![scored("s1", conversation_source(), "short", 0.5)]);
        let medium = MockMemoryProvider::new(vec![scored(
            "m1",
            orchestrator_source("plan"),
            "medium",
            0.55,
        )]);
        let long = MockMemoryProvider::new(vec![]);
        let long_log = long.log();

        let unified = UnifiedMemoryProvider::new(Box::new(short), Box::new(medium), Box::new(long));

        for filter in [None, Some("knowledge_note"), Some("tool")] {
            let results = unified.search_context("q", filter, 10).await.unwrap();
            let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
            assert_eq!(ids, vec!["s1", "m1"], "filter {filter:?}");
        }
        // The fallback goes through `query`, so even the long-term level is asked.
        assert_eq!(calls_of(&long_log).len(), 3);
    }

    // =======================================================================
    // get_relevant_decisions
    // =======================================================================

    /// Medium-term is asked for decisions, long-term for `"decision <topic>"`
    /// with **half** the limit.
    #[tokio::test]
    async fn get_relevant_decisions_merges_both_levels() {
        let medium = MockMemoryProvider::new(vec![]).with_decisions(vec![scored(
            "d1",
            orchestrator_source("decision"),
            "We chose JWT",
            0.9,
        )]);
        let long = MockMemoryProvider::new(vec![scored(
            "l1",
            cross_conversation_source(),
            "Earlier chat about tokens",
            0.4,
        )]);
        let (medium_log, long_log) = (medium.log(), long.log());

        let unified = UnifiedMemoryProvider::new(
            Box::new(MockMemoryProvider::new(vec![])),
            Box::new(medium),
            Box::new(long),
        );

        let results = unified.get_relevant_decisions("auth", 6).await.unwrap();
        let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["d1", "l1"]);
        // No weighting here, unlike `query`.
        assert!((results[0].score.combined - 0.9).abs() < 1e-9);

        assert_eq!(
            calls_of(&medium_log),
            vec![("get_relevant_decisions", "auth".to_string(), 6)]
        );
        assert_eq!(
            calls_of(&long_log),
            vec![("query", "decision auth".to_string(), 3)]
        );
    }

    /// `limit / 2` is integer division: a limit of 1 asks long-term for zero
    /// results, so the supplementary level contributes nothing at all.
    #[tokio::test]
    async fn get_relevant_decisions_asks_long_term_for_nothing_when_the_limit_is_one() {
        let long = MockMemoryProvider::new(vec![scored(
            "l1",
            cross_conversation_source(),
            "Earlier chat",
            0.9,
        )]);
        let long_log = long.log();
        let unified = UnifiedMemoryProvider::new(
            Box::new(MockMemoryProvider::new(vec![])),
            Box::new(MockMemoryProvider::new(vec![]).with_decisions(vec![scored(
                "d1",
                orchestrator_source("decision"),
                "We chose JWT",
                0.5,
            )])),
            Box::new(long),
        );

        let results = unified.get_relevant_decisions("auth", 1).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "d1");
        assert_eq!(
            calls_of(&long_log),
            vec![("query", "decision auth".to_string(), 0)]
        );
    }

    #[tokio::test]
    async fn get_relevant_decisions_survives_one_dead_level() {
        let unified = UnifiedMemoryProvider::new(
            Box::new(MockMemoryProvider::new(vec![])),
            Box::new(MockMemoryProvider::new(vec![]).with_decisions(vec![scored(
                "d1",
                orchestrator_source("decision"),
                "We chose JWT",
                0.9,
            )])),
            Box::new(MockMemoryProvider::failing("meilisearch down")),
        );

        let results = unified
            .get_relevant_decisions("auth", 6)
            .await
            .expect("long-term is supplementary, not required");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "d1");
    }

    #[tokio::test]
    async fn get_relevant_decisions_fails_only_when_both_levels_fail() {
        let unified = UnifiedMemoryProvider::new(
            Box::new(MockMemoryProvider::new(vec![])),
            Box::new(MockMemoryProvider::failing("orchestrator down")),
            Box::new(MockMemoryProvider::failing("meilisearch down")),
        );

        let message = unified
            .get_relevant_decisions("auth", 6)
            .await
            .expect_err("no level could answer")
            .to_string();
        assert!(
            message.contains("medium-term: orchestrator down"),
            "{message}"
        );
        assert!(message.contains("long-term: meilisearch down"), "{message}");
    }

    // =======================================================================
    // config / scope
    // =======================================================================

    #[test]
    fn test_config_default() {
        let config = UnifiedMemoryConfig::default();
        assert!(config.short_term_weight > config.medium_term_weight);
        assert!(config.medium_term_weight > config.long_term_weight);
        assert!((config.short_term_weight - 1.2).abs() < f64::EPSILON);
        assert!((config.min_score_threshold - 0.1).abs() < f64::EPSILON);
    }

    /// `set_scope` stores the scope and — contrary to what a "unified" provider
    /// suggests — does **not** propagate it to the three levels: the trait takes
    /// `&mut self` and the levels are held as boxed trait objects behind `&self`.
    #[tokio::test]
    async fn set_scope_is_stored_but_never_propagated() {
        let short = MockMemoryProvider::new(vec![]);
        let short_log = short.log();
        let mut unified = UnifiedMemoryProvider::new(
            Box::new(short),
            Box::new(MockMemoryProvider::new(vec![])),
            Box::new(MockMemoryProvider::new(vec![])),
        );

        assert_eq!(unified.current_scope(), None);
        unified.set_scope(Some("project-nexus".to_string()));
        assert_eq!(unified.current_scope(), Some("project-nexus".to_string()));
        unified.set_scope(None);
        assert_eq!(unified.current_scope(), None);

        // Nothing reached the levels, and the scope never enters their queries.
        assert!(calls_of(&short_log).is_empty());
        unified.query("auth", 10).await.unwrap();
        assert_eq!(
            calls_of(&short_log),
            vec![("query", "auth".to_string(), 10)]
        );
    }
}
