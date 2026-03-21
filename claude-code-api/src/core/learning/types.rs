/// Core types for the self-evolving knowledge system.
///
/// # References
/// - EvoFSM (2026) — "Controllable Self-Evolution for Deep Research with FSMs"
///   MutationCandidate maps to the "proposed skill mutation" concept in EvoFSM.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Confidence score wrapper with clamping to [0.0, 1.0].
///
/// # References
/// - EvoFSM (2026) — confidence-based gating for mutation application
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ConfidenceScore(f64);

impl ConfidenceScore {
    /// Create a new confidence score, clamped to [0.0, 1.0].
    pub fn new(value: f64) -> Self {
        Self(value.clamp(0.0, 1.0))
    }

    pub fn value(&self) -> f64 {
        self.0
    }
}

impl From<f64> for ConfidenceScore {
    fn from(v: f64) -> Self {
        Self::new(v)
    }
}

/// A proposed state in the protocol FSM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedState {
    pub name: String,
    pub description: String,
}

/// A proposed transition between states.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedTransition {
    pub from_state: String,
    pub to_state: String,
    pub trigger: String,
}

/// Context about the current protocol graph, used by the critic for scoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolContext {
    /// Existing state names in the protocol.
    pub existing_states: Vec<String>,
    /// Existing transitions as (from, to) pairs.
    pub existing_transitions: Vec<(String, String)>,
    /// Historical co-change counts: (pattern, count).
    pub co_change_history: Vec<(String, u32)>,
}

/// A candidate mutation proposed by the evolution engine.
///
/// # References
/// - EvoFSM (2026) — "Controllable Self-Evolution for Deep Research with FSMs"
///   Represents a single proposed mutation before critic evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationCandidate {
    pub id: Uuid,
    pub pattern: String,
    pub proposed_states: Vec<ProposedState>,
    pub proposed_transitions: Vec<ProposedTransition>,
    pub protocol_context: ProtocolContext,
    pub confidence: ConfidenceScore,
    pub created_at: DateTime<Utc>,
}

impl MutationCandidate {
    pub fn new(
        pattern: String,
        proposed_states: Vec<ProposedState>,
        proposed_transitions: Vec<ProposedTransition>,
        protocol_context: ProtocolContext,
        confidence: ConfidenceScore,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            pattern,
            proposed_states,
            proposed_transitions,
            protocol_context,
            confidence,
            created_at: Utc::now(),
        }
    }
}

/// Result of critic evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriticResult {
    pub score: ConfidenceScore,
    pub rationale: String,
    pub breakdown: CriticBreakdown,
}

/// Detailed scoring breakdown from the critic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriticBreakdown {
    /// Score from pattern history similarity [0, 1].
    pub history_score: f64,
    /// Score from structural impact analysis [0, 1].
    pub structural_score: f64,
    /// Score from transition coherence check [0, 1].
    pub coherence_score: f64,
}

/// Decision record for rejected/accepted mutations.
///
/// # References
/// - EvoFSM (2026) — traceability of mutation decisions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: Uuid,
    pub mutation_id: Uuid,
    pub status: DecisionStatus,
    pub critic_score: ConfidenceScore,
    pub rationale: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DecisionStatus {
    Accepted,
    Rejected,
    Suggested,
}

/// Critic operating mode.
///
/// # References
/// - EvoFSM (2026) — "suggest only" mode for human-in-the-loop review
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CriticMode {
    /// Apply mutations that pass the critic threshold.
    Apply,
    /// Only suggest mutations, never apply automatically.
    SuggestOnly,
}

impl Default for CriticMode {
    fn default() -> Self {
        Self::Apply
    }
}

/// Configuration for the learning/evolution subsystem.
///
/// # References
/// - EvoFSM (2026) — configurable thresholds for controllable self-evolution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningConfig {
    /// Minimum critic score to accept a mutation (default: 0.7).
    pub critic_threshold: f64,
    /// Critic operating mode (default: Apply).
    pub critic_mode: CriticMode,
    /// Minimum pattern confidence to even consider a mutation (default: 0.5).
    pub min_pattern_confidence: f64,
    /// Maximum number of mutations per evolution cycle (default: 5).
    pub max_mutations_per_cycle: usize,
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            critic_threshold: 0.7,
            critic_mode: CriticMode::default(),
            min_pattern_confidence: 0.5,
            max_mutations_per_cycle: 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_confidence_score_clamping() {
        assert_eq!(ConfidenceScore::new(1.5).value(), 1.0);
        assert_eq!(ConfidenceScore::new(-0.5).value(), 0.0);
        assert_eq!(ConfidenceScore::new(0.75).value(), 0.75);
    }

    #[test]
    fn test_learning_config_serde_roundtrip() {
        let config = LearningConfig {
            critic_threshold: 0.8,
            critic_mode: CriticMode::SuggestOnly,
            min_pattern_confidence: 0.6,
            max_mutations_per_cycle: 10,
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: LearningConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.critic_threshold, 0.8);
        assert_eq!(deserialized.critic_mode, CriticMode::SuggestOnly);
        assert_eq!(deserialized.min_pattern_confidence, 0.6);
        assert_eq!(deserialized.max_mutations_per_cycle, 10);
    }

    #[test]
    fn test_learning_config_default() {
        let config = LearningConfig::default();
        assert_eq!(config.critic_threshold, 0.7);
        assert_eq!(config.critic_mode, CriticMode::Apply);
    }

    #[test]
    fn test_mutation_candidate_serde_roundtrip() {
        let candidate = MutationCandidate::new(
            "test_pattern".to_string(),
            vec![ProposedState {
                name: "new_state".to_string(),
                description: "A test state".to_string(),
            }],
            vec![ProposedTransition {
                from_state: "init".to_string(),
                to_state: "new_state".to_string(),
                trigger: "on_event".to_string(),
            }],
            ProtocolContext {
                existing_states: vec!["init".to_string()],
                existing_transitions: vec![],
                co_change_history: vec![("test_pattern".to_string(), 5)],
            },
            ConfidenceScore::new(0.85),
        );
        let json = serde_json::to_string(&candidate).unwrap();
        let deserialized: MutationCandidate = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.pattern, "test_pattern");
        assert_eq!(deserialized.confidence.value(), 0.85);
    }

    #[test]
    fn test_decision_serde_roundtrip() {
        let decision = Decision {
            id: Uuid::new_v4(),
            mutation_id: Uuid::new_v4(),
            status: DecisionStatus::Rejected,
            critic_score: ConfidenceScore::new(0.45),
            rationale: "Low history score".to_string(),
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&decision).unwrap();
        let deserialized: Decision = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.status, DecisionStatus::Rejected);
    }
}
