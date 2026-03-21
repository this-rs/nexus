/// Evolution engine that applies mutations with critic pre-evaluation.
///
/// # References
/// - EvoFSM (2026) — "Controllable Self-Evolution for Deep Research with FSMs"
///   The engine implements the mutation→critic→apply/reject pipeline,
///   separating Flow optimization (structural) from Skill optimization (behavioral).
use chrono::Utc;
use tracing::{info, warn};
use uuid::Uuid;

use super::critic::MutationCritic;
use super::types::*;

/// Result of applying mutations through the evolution engine.
#[derive(Debug, Clone)]
pub struct EvolutionResult {
    pub applied: Vec<MutationCandidate>,
    pub rejected: Vec<MutationCandidate>,
    pub decisions: Vec<Decision>,
}

/// Engine that orchestrates mutation evaluation and application.
///
/// # References
/// - EvoFSM (2026) — controllable self-evolution with configurable thresholds
pub struct EvolutionEngine {
    config: LearningConfig,
}

impl EvolutionEngine {
    pub fn new(config: LearningConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &LearningConfig {
        &self.config
    }

    /// Apply mutations with critic pre-evaluation.
    ///
    /// For each candidate:
    /// 1. Score via the critic
    /// 2. If score >= threshold AND mode is Apply → accept
    /// 3. If score >= threshold AND mode is SuggestOnly → suggest (don't apply)
    /// 4. If score < threshold → reject
    /// 5. Log all decisions with rationale
    ///
    /// # References
    /// - EvoFSM (2026) — critic-gated mutation pipeline
    pub fn apply_mutations(
        &self,
        candidates: Vec<MutationCandidate>,
        critic: &dyn MutationCritic,
    ) -> EvolutionResult {
        let mut applied = Vec::new();
        let mut rejected = Vec::new();
        let mut decisions = Vec::new();

        let max = self.config.max_mutations_per_cycle;
        let mut applied_count = 0;

        for candidate in candidates {
            // Skip if below minimum pattern confidence
            if candidate.confidence.value() < self.config.min_pattern_confidence {
                warn!(
                    pattern = %candidate.pattern,
                    confidence = %candidate.confidence.value(),
                    "Skipping candidate below min_pattern_confidence"
                );
                let decision = Decision {
                    id: Uuid::new_v4(),
                    mutation_id: candidate.id,
                    status: DecisionStatus::Rejected,
                    critic_score: candidate.confidence,
                    rationale: format!(
                        "Pattern confidence {:.2} below minimum {:.2}",
                        candidate.confidence.value(),
                        self.config.min_pattern_confidence
                    ),
                    created_at: Utc::now(),
                };
                decisions.push(decision);
                rejected.push(candidate);
                continue;
            }

            // Score via critic
            let result = critic.score_mutation(&candidate);

            if result.score.value() >= self.config.critic_threshold {
                match self.config.critic_mode {
                    CriticMode::Apply => {
                        if applied_count >= max {
                            info!(
                                pattern = %candidate.pattern,
                                "Max mutations per cycle reached, skipping"
                            );
                            let decision = Decision {
                                id: Uuid::new_v4(),
                                mutation_id: candidate.id,
                                status: DecisionStatus::Rejected,
                                critic_score: result.score,
                                rationale: format!(
                                    "Max mutations per cycle ({}) reached. critic_score={:.3}: {}",
                                    max, result.score.value(), result.rationale
                                ),
                                created_at: Utc::now(),
                            };
                            decisions.push(decision);
                            rejected.push(candidate);
                            continue;
                        }

                        info!(
                            pattern = %candidate.pattern,
                            score = %result.score.value(),
                            "Mutation accepted by critic"
                        );
                        let decision = Decision {
                            id: Uuid::new_v4(),
                            mutation_id: candidate.id,
                            status: DecisionStatus::Accepted,
                            critic_score: result.score,
                            rationale: format!(
                                "Accepted: critic_score={:.3} >= threshold={:.2}. {}",
                                result.score.value(),
                                self.config.critic_threshold,
                                result.rationale
                            ),
                            created_at: Utc::now(),
                        };
                        decisions.push(decision);
                        applied.push(candidate);
                        applied_count += 1;
                    }
                    CriticMode::SuggestOnly => {
                        info!(
                            pattern = %candidate.pattern,
                            score = %result.score.value(),
                            "Mutation suggested (SuggestOnly mode)"
                        );
                        let decision = Decision {
                            id: Uuid::new_v4(),
                            mutation_id: candidate.id,
                            status: DecisionStatus::Suggested,
                            critic_score: result.score,
                            rationale: format!(
                                "Suggested (SuggestOnly mode): critic_score={:.3}. {}",
                                result.score.value(),
                                result.rationale
                            ),
                            created_at: Utc::now(),
                        };
                        decisions.push(decision);
                        // In SuggestOnly, we don't apply — treat as "not applied"
                        rejected.push(candidate);
                    }
                }
            } else {
                warn!(
                    pattern = %candidate.pattern,
                    score = %result.score.value(),
                    threshold = %self.config.critic_threshold,
                    "Mutation rejected by critic"
                );
                let decision = Decision {
                    id: Uuid::new_v4(),
                    mutation_id: candidate.id,
                    status: DecisionStatus::Rejected,
                    critic_score: result.score,
                    rationale: format!(
                        "Rejected: critic_score={:.3} < threshold={:.2}. {}",
                        result.score.value(),
                        self.config.critic_threshold,
                        result.rationale
                    ),
                    created_at: Utc::now(),
                };
                decisions.push(decision);
                rejected.push(candidate);
            }
        }

        EvolutionResult {
            applied,
            rejected,
            decisions,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::learning::critic::GraphBasedCritic;

    fn make_high_confidence_candidate() -> MutationCandidate {
        MutationCandidate::new(
            "proven_pattern".to_string(),
            vec![ProposedState {
                name: "optimized".to_string(),
                description: "Optimized state".to_string(),
            }],
            vec![ProposedTransition {
                from_state: "running".to_string(),
                to_state: "optimized".to_string(),
                trigger: "on_optimize".to_string(),
            }],
            ProtocolContext {
                existing_states: vec![
                    "init".to_string(),
                    "running".to_string(),
                    "done".to_string(),
                ],
                existing_transitions: vec![
                    ("init".to_string(), "running".to_string()),
                    ("running".to_string(), "done".to_string()),
                    ("init".to_string(), "done".to_string()),
                ],
                co_change_history: vec![("proven_pattern".to_string(), 20)],
            },
            ConfidenceScore::new(0.95),
        )
    }

    fn make_low_confidence_candidate() -> MutationCandidate {
        MutationCandidate::new(
            "weak_pattern".to_string(),
            vec![ProposedState {
                name: "speculative".to_string(),
                description: "Speculative state".to_string(),
            }],
            vec![ProposedTransition {
                from_state: "init".to_string(),
                to_state: "speculative".to_string(),
                trigger: "on_guess".to_string(),
            }],
            ProtocolContext {
                existing_states: vec!["init".to_string()],
                existing_transitions: vec![],
                co_change_history: vec![],
            },
            ConfidenceScore::new(0.55),
        )
    }

    #[test]
    fn test_mutations_below_threshold_are_rejected() {
        let config = LearningConfig::default(); // threshold = 0.7
        let engine = EvolutionEngine::new(config);
        let critic = GraphBasedCritic::default();

        let candidates = vec![make_low_confidence_candidate()];
        let result = engine.apply_mutations(candidates, &critic);

        assert_eq!(result.applied.len(), 0);
        assert_eq!(result.rejected.len(), 1);
        assert_eq!(result.decisions.len(), 1);
        assert_eq!(result.decisions[0].status, DecisionStatus::Rejected);
    }

    #[test]
    fn test_mutations_above_threshold_are_accepted() {
        let config = LearningConfig::default(); // threshold = 0.7
        let engine = EvolutionEngine::new(config);
        let critic = GraphBasedCritic::default();

        let candidates = vec![make_high_confidence_candidate()];
        let result = engine.apply_mutations(candidates, &critic);

        assert_eq!(result.applied.len(), 1);
        assert_eq!(result.rejected.len(), 0);
        assert_eq!(result.decisions.len(), 1);
        assert_eq!(result.decisions[0].status, DecisionStatus::Accepted);
    }

    #[test]
    fn test_suggest_only_mode_does_not_apply() {
        let config = LearningConfig {
            critic_mode: CriticMode::SuggestOnly,
            ..LearningConfig::default()
        };
        let engine = EvolutionEngine::new(config);
        let critic = GraphBasedCritic::default();

        let candidates = vec![make_high_confidence_candidate()];
        let result = engine.apply_mutations(candidates, &critic);

        assert_eq!(result.applied.len(), 0, "SuggestOnly should not apply");
        assert_eq!(result.rejected.len(), 1);
        assert_eq!(result.decisions[0].status, DecisionStatus::Suggested);
    }

    #[test]
    fn test_decision_created_for_rejected_mutation() {
        let config = LearningConfig::default();
        let engine = EvolutionEngine::new(config);
        let critic = GraphBasedCritic::default();

        let candidate = make_low_confidence_candidate();
        let candidate_id = candidate.id;
        let result = engine.apply_mutations(vec![candidate], &critic);

        assert_eq!(result.decisions.len(), 1);
        let decision = &result.decisions[0];
        assert_eq!(decision.mutation_id, candidate_id);
        assert_eq!(decision.status, DecisionStatus::Rejected);
        assert!(!decision.rationale.is_empty());
    }

    #[test]
    fn test_max_mutations_per_cycle_respected() {
        let config = LearningConfig {
            max_mutations_per_cycle: 1,
            ..LearningConfig::default()
        };
        let engine = EvolutionEngine::new(config);
        let critic = GraphBasedCritic::default();

        let candidates = vec![
            make_high_confidence_candidate(),
            make_high_confidence_candidate(),
        ];
        let result = engine.apply_mutations(candidates, &critic);

        assert_eq!(result.applied.len(), 1, "Should apply at most 1");
        assert_eq!(result.rejected.len(), 1, "Second should be rejected");
    }

    #[test]
    fn test_below_min_pattern_confidence_skipped() {
        let config = LearningConfig {
            min_pattern_confidence: 0.6,
            ..LearningConfig::default()
        };
        let engine = EvolutionEngine::new(config);
        let critic = GraphBasedCritic::default();

        let candidate = MutationCandidate::new(
            "too_weak".to_string(),
            vec![],
            vec![],
            ProtocolContext {
                existing_states: vec![],
                existing_transitions: vec![],
                co_change_history: vec![],
            },
            ConfidenceScore::new(0.4), // Below 0.6 threshold
        );

        let result = engine.apply_mutations(vec![candidate], &critic);
        assert_eq!(result.rejected.len(), 1);
        assert!(result.decisions[0]
            .rationale
            .contains("below minimum"));
    }

    #[test]
    fn test_mixed_candidates() {
        let config = LearningConfig::default();
        let engine = EvolutionEngine::new(config);
        let critic = GraphBasedCritic::default();

        let candidates = vec![
            make_high_confidence_candidate(),
            make_low_confidence_candidate(),
        ];
        let result = engine.apply_mutations(candidates, &critic);

        assert_eq!(result.applied.len(), 1);
        assert_eq!(result.rejected.len(), 1);
        assert_eq!(result.decisions.len(), 2);
    }
}
