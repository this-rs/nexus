/// Synthetic episode generation via memory replay and knowledge consolidation.
///
/// Generates synthetic episodes from detected patterns and knowledge notes,
/// enabling bootstrap of new projects with synthesized experience.
///
/// # References
/// - "A Neural Network Account of Memory Replay and Knowledge Consolidation" (2022)
///   — Generative replay is more efficient than simple storage for generalization.
/// - "Elements of Episodic Memory" (2024)
///   — Episodic memory structure: stimulus → process → outcome.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// An episodic memory record, real or synthetic.
///
/// # References
/// - "Elements of Episodic Memory" (2024) — stimulus/process/outcome structure
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EpisodeData {
    pub id: Uuid,
    /// What triggered this episode (e.g., pattern type, error context).
    pub stimulus: String,
    /// What happened during the episode (e.g., affected files, actions taken).
    pub process: Vec<String>,
    /// The result: success/failure, recommendation, resolution.
    pub outcome: EpisodeOutcome,
    /// Gate results captured during this episode.
    pub gate_results: Vec<GateResult>,
    /// Whether this episode was synthetically generated via replay.
    ///
    /// Synthetic episodes are weighted at 0.5x in frequency/confidence calculations.
    ///
    /// # Neo4j Model
    /// ```cypher
    /// (:NexusEpisode {
    ///     id: String,
    ///     stimulus: String,
    ///     process: [String],
    ///     outcome_type: String,  // "positive" | "negative"
    ///     outcome_description: String,
    ///     synthetic: Boolean,
    ///     source_pattern: String?,
    ///     created_at: DateTime
    /// })
    /// ```
    pub synthetic: bool,
    /// If synthetic, the source pattern or note ID that generated this episode.
    pub source_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Outcome of an episode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EpisodeOutcome {
    pub outcome_type: OutcomeType,
    pub description: String,
    pub recommendation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum OutcomeType {
    Positive,
    Negative,
}

/// A gate check result within an episode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GateResult {
    pub gate_name: String,
    pub passed: bool,
    pub details: Option<String>,
}

/// A detected pattern from which synthetic episodes can be generated.
///
/// # References
/// - "A Neural Network Account of Memory Replay and Knowledge Consolidation" (2022)
///   — Pattern detection feeds the generative replay mechanism.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedPattern {
    pub pattern_type: String,
    pub affected_files: Vec<String>,
    pub recommendation: String,
    pub confidence: f64,
    pub gate: Option<String>,
    pub occurrence_count: u32,
}

/// A knowledge note (gotcha, pattern, tip) from which episodes can be generated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeNote {
    pub id: String,
    pub note_type: NoteType,
    pub context: String,
    pub resolution: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum NoteType {
    Gotcha,
    Pattern,
    Tip,
}

/// Generate synthetic episodes from a detected pattern.
///
/// Creates an episode where:
/// - stimulus = pattern_type
/// - process = affected_files
/// - outcome = recommendation (negative for failures, positive for optimizations)
///
/// # References
/// - "A Neural Network Account of Memory Replay and Knowledge Consolidation" (2022)
///   — Generative replay produces training episodes from compressed pattern representations.
pub fn generate_synthetic_episodes(pattern: &DetectedPattern) -> Vec<EpisodeData> {
    let outcome_type = if pattern.gate.is_some() {
        OutcomeType::Negative
    } else if pattern.pattern_type.to_lowercase().contains("failure")
        || pattern.pattern_type.to_lowercase().contains("error")
    {
        OutcomeType::Negative
    } else {
        OutcomeType::Positive
    };

    let gate_results = if let Some(ref gate) = pattern.gate {
        vec![GateResult {
            gate_name: gate.clone(),
            passed: false,
            details: Some(format!(
                "Recurring failure detected ({} occurrences)",
                pattern.occurrence_count
            )),
        }]
    } else {
        vec![]
    };

    let episode = EpisodeData {
        id: Uuid::new_v4(),
        stimulus: pattern.pattern_type.clone(),
        process: pattern.affected_files.clone(),
        outcome: EpisodeOutcome {
            outcome_type,
            description: format!(
                "Pattern '{}' detected with confidence {:.2} across {} files",
                pattern.pattern_type,
                pattern.confidence,
                pattern.affected_files.len()
            ),
            recommendation: Some(pattern.recommendation.clone()),
        },
        gate_results,
        synthetic: true,
        source_id: Some(format!("pattern:{}", pattern.pattern_type)),
        created_at: Utc::now(),
    };

    vec![episode]
}

/// Generate synthetic episodes from knowledge notes (gotcha, pattern, tip).
///
/// Creates an episode that "tells the story" of the gotcha/pattern:
/// - stimulus = context (what triggered the problem)
/// - process = tags (the domain/area affected)
/// - outcome = resolution (how it was resolved)
///
/// # References
/// - "Elements of Episodic Memory" (2024) — encoding semantic knowledge as episodic traces
///   for improved retrieval and generalization.
pub fn generate_from_notes(notes: &[KnowledgeNote]) -> Vec<EpisodeData> {
    notes
        .iter()
        .map(|note| {
            let outcome_type = match note.note_type {
                NoteType::Gotcha => OutcomeType::Negative,
                NoteType::Pattern => OutcomeType::Positive,
                NoteType::Tip => OutcomeType::Positive,
            };

            EpisodeData {
                id: Uuid::new_v4(),
                stimulus: note.context.clone(),
                process: note.tags.clone(),
                outcome: EpisodeOutcome {
                    outcome_type,
                    description: format!("[{}] {}", format!("{:?}", note.note_type), note.context),
                    recommendation: Some(note.resolution.clone()),
                },
                gate_results: vec![],
                synthetic: true,
                source_id: Some(format!("note:{}", note.id)),
                created_at: Utc::now(),
            }
        })
        .collect()
}

/// Feedback analysis that weights synthetic episodes at 0.5x.
///
/// # References
/// - "A Neural Network Account of Memory Replay and Knowledge Consolidation" (2022)
///   — Replay-generated memories contribute less certainty than direct experience.
pub mod feedback {
    use super::*;

    /// Result of analyzing a set of episodes for pattern frequency and confidence.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct AnalysisResult {
        /// Effective count: real episodes count as 1.0, synthetic as 0.5.
        pub effective_count: f64,
        /// Confidence based on effective count.
        pub confidence: f64,
        /// Number of real episodes.
        pub real_count: usize,
        /// Number of synthetic episodes.
        pub synthetic_count: usize,
    }

    /// Analyze episodes matching a given pattern stimulus.
    ///
    /// Synthetic episodes are weighted at 0.5x in frequency and confidence calculations.
    /// This prevents synthetic replay from inflating pattern certainty beyond what
    /// direct experience supports.
    ///
    /// # References
    /// - "A Neural Network Account of Memory Replay and Knowledge Consolidation" (2022)
    pub fn analyze(episodes: &[EpisodeData], pattern_stimulus: &str) -> AnalysisResult {
        let matching: Vec<&EpisodeData> = episodes
            .iter()
            .filter(|e| e.stimulus == pattern_stimulus)
            .collect();

        let real_count = matching.iter().filter(|e| !e.synthetic).count();
        let synthetic_count = matching.iter().filter(|e| e.synthetic).count();

        // Synthetic episodes weighted at 0.5x
        let effective_count = real_count as f64 + synthetic_count as f64 * 0.5;

        // Confidence: sigmoid-like curve, 10+ effective episodes = ~1.0
        let confidence = if effective_count == 0.0 {
            0.0
        } else {
            (effective_count / 10.0).min(1.0)
        };

        AnalysisResult {
            effective_count,
            confidence,
            real_count,
            synthetic_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_episode_data_serde_roundtrip_synthetic_true() {
        let episode = EpisodeData {
            id: Uuid::new_v4(),
            stimulus: "RecurringFailure".to_string(),
            process: vec!["src/main.rs".to_string()],
            outcome: EpisodeOutcome {
                outcome_type: OutcomeType::Negative,
                description: "Test failure".to_string(),
                recommendation: Some("Fix it".to_string()),
            },
            gate_results: vec![GateResult {
                gate_name: "cargo test".to_string(),
                passed: false,
                details: Some("3 failures".to_string()),
            }],
            synthetic: true,
            source_id: Some("pattern:RecurringFailure".to_string()),
            created_at: Utc::now(),
        };

        let json = serde_json::to_string(&episode).unwrap();
        let deserialized: EpisodeData = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.synthetic, true);
        assert_eq!(deserialized.stimulus, "RecurringFailure");
        assert_eq!(deserialized.outcome.outcome_type, OutcomeType::Negative);
        assert_eq!(deserialized.gate_results.len(), 1);
        assert_eq!(deserialized.gate_results[0].gate_name, "cargo test");
    }

    #[test]
    fn test_episode_data_serde_roundtrip_synthetic_false() {
        let episode = EpisodeData {
            id: Uuid::new_v4(),
            stimulus: "RealEvent".to_string(),
            process: vec![],
            outcome: EpisodeOutcome {
                outcome_type: OutcomeType::Positive,
                description: "Success".to_string(),
                recommendation: None,
            },
            gate_results: vec![],
            synthetic: false,
            source_id: None,
            created_at: Utc::now(),
        };

        let json = serde_json::to_string(&episode).unwrap();
        let deserialized: EpisodeData = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.synthetic, false);
        assert_eq!(deserialized.source_id, None);
    }

    #[test]
    fn test_generate_from_recurring_failure_produces_negative_with_gates() {
        let pattern = DetectedPattern {
            pattern_type: "RecurringFailure".to_string(),
            affected_files: vec!["src/lib.rs".to_string(), "src/main.rs".to_string()],
            recommendation: "Add error handling for edge case".to_string(),
            confidence: 0.85,
            gate: Some("cargo test".to_string()),
            occurrence_count: 5,
        };

        let episodes = generate_synthetic_episodes(&pattern);

        assert_eq!(episodes.len(), 1);
        let ep = &episodes[0];
        assert!(ep.synthetic);
        assert_eq!(ep.stimulus, "RecurringFailure");
        assert_eq!(ep.process, vec!["src/lib.rs", "src/main.rs"]);
        assert_eq!(ep.outcome.outcome_type, OutcomeType::Negative);
        assert!(ep
            .outcome
            .recommendation
            .as_ref()
            .unwrap()
            .contains("error handling"));
        assert_eq!(ep.gate_results.len(), 1);
        assert_eq!(ep.gate_results[0].gate_name, "cargo test");
        assert!(!ep.gate_results[0].passed);
    }

    #[test]
    fn test_generate_from_gotcha_note_produces_episode_with_context_and_resolution() {
        let notes = vec![KnowledgeNote {
            id: "note-001".to_string(),
            note_type: NoteType::Gotcha,
            context: "Async drop causes panic in tokio runtime".to_string(),
            resolution: "Use tokio::task::spawn_blocking for cleanup".to_string(),
            tags: vec!["async".to_string(), "tokio".to_string()],
        }];

        let episodes = generate_from_notes(&notes);

        assert_eq!(episodes.len(), 1);
        let ep = &episodes[0];
        assert!(ep.synthetic);
        assert_eq!(ep.stimulus, "Async drop causes panic in tokio runtime");
        assert_eq!(ep.process, vec!["async", "tokio"]);
        assert_eq!(ep.outcome.outcome_type, OutcomeType::Negative);
        assert_eq!(
            ep.outcome.recommendation.as_ref().unwrap(),
            "Use tokio::task::spawn_blocking for cleanup"
        );
        assert_eq!(ep.source_id.as_ref().unwrap(), "note:note-001");
    }

    #[test]
    fn test_generate_from_pattern_note_produces_positive_episode() {
        let notes = vec![KnowledgeNote {
            id: "note-002".to_string(),
            note_type: NoteType::Pattern,
            context: "Use builder pattern for complex configs".to_string(),
            resolution: "Implement Default + builder methods".to_string(),
            tags: vec!["design-pattern".to_string()],
        }];

        let episodes = generate_from_notes(&notes);

        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].outcome.outcome_type, OutcomeType::Positive);
    }

    #[test]
    fn test_feedback_analyze_weights_synthetic_at_half() {
        let make_episode = |stimulus: &str, synthetic: bool| EpisodeData {
            id: Uuid::new_v4(),
            stimulus: stimulus.to_string(),
            process: vec![],
            outcome: EpisodeOutcome {
                outcome_type: OutcomeType::Negative,
                description: "test".to_string(),
                recommendation: None,
            },
            gate_results: vec![],
            synthetic,
            source_id: if synthetic {
                Some("synth".to_string())
            } else {
                None
            },
            created_at: Utc::now(),
        };

        // 3 real + 3 synthetic = 3 + 1.5 = 4.5 effective
        let mixed: Vec<EpisodeData> = (0..3)
            .map(|_| make_episode("TestPattern", false))
            .chain((0..3).map(|_| make_episode("TestPattern", true)))
            .collect();

        let result = feedback::analyze(&mixed, "TestPattern");
        assert_eq!(result.real_count, 3);
        assert_eq!(result.synthetic_count, 3);
        assert_eq!(result.effective_count, 4.5);
        assert_eq!(result.confidence, 0.45);

        // 6 real = 6.0 effective → higher confidence
        let all_real: Vec<EpisodeData> =
            (0..6).map(|_| make_episode("TestPattern", false)).collect();

        let result_real = feedback::analyze(&all_real, "TestPattern");
        assert_eq!(result_real.effective_count, 6.0);
        assert_eq!(result_real.confidence, 0.6);

        // 3 real + 3 synthetic (4.5) < 6 real (6.0) in confidence
        assert!(
            result.confidence < result_real.confidence,
            "Mixed (3 real + 3 synthetic) should have lower confidence than 6 real: {} vs {}",
            result.confidence,
            result_real.confidence
        );
    }

    #[test]
    fn test_feedback_analyze_empty() {
        let result = feedback::analyze(&[], "anything");
        assert_eq!(result.effective_count, 0.0);
        assert_eq!(result.confidence, 0.0);
        assert_eq!(result.real_count, 0);
        assert_eq!(result.synthetic_count, 0);
    }

    #[test]
    fn test_feedback_analyze_filters_by_stimulus() {
        let make_episode = |stimulus: &str| EpisodeData {
            id: Uuid::new_v4(),
            stimulus: stimulus.to_string(),
            process: vec![],
            outcome: EpisodeOutcome {
                outcome_type: OutcomeType::Positive,
                description: "ok".to_string(),
                recommendation: None,
            },
            gate_results: vec![],
            synthetic: false,
            source_id: None,
            created_at: Utc::now(),
        };

        let episodes = vec![
            make_episode("PatternA"),
            make_episode("PatternA"),
            make_episode("PatternB"),
        ];

        let result = feedback::analyze(&episodes, "PatternA");
        assert_eq!(result.real_count, 2);
        assert_eq!(result.effective_count, 2.0);
    }
}
