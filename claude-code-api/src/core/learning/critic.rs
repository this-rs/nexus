/// Mutation critic for pre-evaluating evolution candidates.
///
/// # References
/// - EvoFSM (2026) — "Controllable Self-Evolution for Deep Research with FSMs"
///   The critic implements the pre-evaluation gate that separates Flow optimization
///   from Skill optimization, scoring candidates before application.
use super::types::{ConfidenceScore, CriticBreakdown, CriticResult, MutationCandidate};

/// Trait for evaluating mutation candidates before application.
///
/// # References
/// - EvoFSM (2026) — critic-based gating for controllable self-evolution
pub trait MutationCritic: Send + Sync {
    /// Score a mutation candidate. Returns a CriticResult with score and rationale.
    fn score_mutation(&self, candidate: &MutationCandidate) -> CriticResult;
}

/// Graph-based critic that scores mutations using:
/// - Historical pattern similarity (co-change frequency)
/// - Structural impact (number of existing transitions affected)
/// - Coherence (no duplicate transitions)
///
/// # References
/// - EvoFSM (2026) — "Controllable Self-Evolution for Deep Research with FSMs"
///   Implements the betweenness/structural analysis from the paper's Flow optimization.
pub struct GraphBasedCritic {
    /// Weight for history score component.
    pub history_weight: f64,
    /// Weight for structural score component.
    pub structural_weight: f64,
    /// Weight for coherence score component.
    pub coherence_weight: f64,
}

impl Default for GraphBasedCritic {
    fn default() -> Self {
        Self {
            history_weight: 0.4,
            structural_weight: 0.3,
            coherence_weight: 0.3,
        }
    }
}

impl GraphBasedCritic {
    pub fn new(history_weight: f64, structural_weight: f64, coherence_weight: f64) -> Self {
        Self {
            history_weight,
            structural_weight,
            coherence_weight,
        }
    }

    /// Compute history score: how often the pattern co-changed with existing protocols.
    /// High co-change count → high confidence the mutation is well-founded.
    fn compute_history_score(&self, candidate: &MutationCandidate) -> f64 {
        let history = &candidate.protocol_context.co_change_history;
        if history.is_empty() {
            return 0.0;
        }

        // Find co-change count for this specific pattern
        let pattern_count = history
            .iter()
            .filter(|(p, _)| p == &candidate.pattern)
            .map(|(_, count)| *count)
            .sum::<u32>();

        // Normalize: 10+ co-changes = 1.0, sigmoid-like curve
        let normalized = (pattern_count as f64 / 10.0).min(1.0);
        // Also factor in overall pattern confidence
        normalized * candidate.confidence.value()
    }

    /// Compute structural impact score.
    /// More existing transitions = more established protocol = safer to extend.
    /// Empty protocol = risky to mutate.
    fn compute_structural_score(&self, candidate: &MutationCandidate) -> f64 {
        let existing_count = candidate.protocol_context.existing_transitions.len();
        let proposed_count = candidate.proposed_transitions.len();

        if existing_count == 0 && proposed_count > 0 {
            // Adding to an empty protocol: moderate risk
            return 0.3;
        }

        if proposed_count == 0 {
            // No transitions proposed: just states, low impact
            return 0.8;
        }

        // Ratio of proposed to existing — lower ratio = less disruptive
        let ratio = proposed_count as f64 / (existing_count as f64 + 1.0);
        // Inverse: smaller changes score higher
        (1.0 / (1.0 + ratio)).min(1.0)
    }

    /// Compute coherence score: check for duplicate transitions.
    /// Duplicate transitions → incoherent mutation → low score.
    fn compute_coherence_score(&self, candidate: &MutationCandidate) -> f64 {
        let existing = &candidate.protocol_context.existing_transitions;
        let proposed = &candidate.proposed_transitions;

        let mut duplicate_count = 0;
        for t in proposed {
            let pair = (t.from_state.clone(), t.to_state.clone());
            if existing.contains(&pair) {
                duplicate_count += 1;
            }
        }

        if proposed.is_empty() {
            return 1.0;
        }

        let duplicate_ratio = duplicate_count as f64 / proposed.len() as f64;
        1.0 - duplicate_ratio
    }
}

impl MutationCritic for GraphBasedCritic {
    fn score_mutation(&self, candidate: &MutationCandidate) -> CriticResult {
        let history_score = self.compute_history_score(candidate);
        let structural_score = self.compute_structural_score(candidate);
        let coherence_score = self.compute_coherence_score(candidate);

        let weighted_score = history_score * self.history_weight
            + structural_score * self.structural_weight
            + coherence_score * self.coherence_weight;

        let score = ConfidenceScore::new(weighted_score);

        let rationale = format!(
            "history={:.2} (w={:.1}), structural={:.2} (w={:.1}), coherence={:.2} (w={:.1}) → total={:.3}",
            history_score, self.history_weight,
            structural_score, self.structural_weight,
            coherence_score, self.coherence_weight,
            weighted_score,
        );

        CriticResult {
            score,
            rationale,
            breakdown: CriticBreakdown {
                history_score,
                structural_score,
                coherence_score,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::learning::types::*;

    fn make_candidate(
        pattern: &str,
        co_changes: Vec<(String, u32)>,
        existing_transitions: Vec<(String, String)>,
        proposed_transitions: Vec<ProposedTransition>,
        confidence: f64,
    ) -> MutationCandidate {
        MutationCandidate::new(
            pattern.to_string(),
            vec![ProposedState {
                name: "new_state".to_string(),
                description: "test".to_string(),
            }],
            proposed_transitions,
            ProtocolContext {
                existing_states: vec!["init".to_string(), "running".to_string()],
                existing_transitions,
                co_change_history: co_changes,
            },
            ConfidenceScore::new(confidence),
        )
    }

    #[test]
    fn test_high_confidence_pattern_scores_above_threshold() {
        let critic = GraphBasedCritic::default();

        // High confidence, strong history, established protocol, no duplicates
        let candidate = make_candidate(
            "well_known_pattern",
            vec![("well_known_pattern".to_string(), 15)],
            vec![
                ("init".to_string(), "running".to_string()),
                ("running".to_string(), "done".to_string()),
                ("init".to_string(), "error".to_string()),
            ],
            vec![ProposedTransition {
                from_state: "running".to_string(),
                to_state: "new_state".to_string(),
                trigger: "on_event".to_string(),
            }],
            0.9,
        );

        let result = critic.score_mutation(&candidate);
        assert!(
            result.score.value() > 0.7,
            "Expected score > 0.7 for high-confidence pattern, got {:.3}: {}",
            result.score.value(),
            result.rationale
        );
    }

    #[test]
    fn test_isolated_pattern_scores_below_threshold() {
        let critic = GraphBasedCritic::default();

        // Low confidence, no history, empty protocol, no transitions proposed
        let candidate = make_candidate(
            "unknown_pattern",
            vec![], // no co-change history
            vec![], // empty protocol
            vec![ProposedTransition {
                from_state: "init".to_string(),
                to_state: "new_state".to_string(),
                trigger: "guess".to_string(),
            }],
            0.3,
        );

        let result = critic.score_mutation(&candidate);
        assert!(
            result.score.value() < 0.5,
            "Expected score < 0.5 for isolated pattern, got {:.3}: {}",
            result.score.value(),
            result.rationale
        );
    }

    #[test]
    fn test_duplicate_transitions_reduce_score() {
        let critic = GraphBasedCritic::default();

        let candidate = make_candidate(
            "dup_pattern",
            vec![("dup_pattern".to_string(), 10)],
            vec![("init".to_string(), "running".to_string())],
            vec![ProposedTransition {
                from_state: "init".to_string(),
                to_state: "running".to_string(), // duplicate!
                trigger: "dup_trigger".to_string(),
            }],
            0.8,
        );

        let result = critic.score_mutation(&candidate);
        // Coherence score should be 0 due to full duplication
        assert_eq!(result.breakdown.coherence_score, 0.0);
    }

    #[test]
    fn test_no_proposed_transitions_high_coherence() {
        let critic = GraphBasedCritic::default();

        let candidate = make_candidate(
            "state_only",
            vec![("state_only".to_string(), 5)],
            vec![("init".to_string(), "running".to_string())],
            vec![], // no transitions
            0.7,
        );

        let result = critic.score_mutation(&candidate);
        assert_eq!(result.breakdown.coherence_score, 1.0);
        assert!(result.breakdown.structural_score > 0.7);
    }
}
