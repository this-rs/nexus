/// Self-evolving knowledge system with mutation critic, episodic memory replay,
/// self-evaluation confidence system, and privacy-preserving anonymization.
///
/// # References
/// - EvoFSM (2026) — "Controllable Self-Evolution for Deep Research with FSMs"
///   Separates Flow optimization and Skill optimization with a pre-evaluation critic.
/// - "A Neural Network Account of Memory Replay and Knowledge Consolidation" (2022)
///   — Generative replay for synthetic episode generation.
/// - "Elements of Episodic Memory" (2024)
///   — Stimulus/process/outcome episodic structure.
/// - ELL (2025) — "Experience-driven Lifelong Learning"
///   4th pillar: self-evaluation with calibrated confidence and feedback loops.
/// - "Episodic Memory in AI Agents Poses Risks That Should Be Studied and Mitigated" (2025)
///   — Privacy and manipulation risks in episodic memory; addressed by anonymization pipeline.
pub mod anonymize;
pub mod confidence;
pub mod critic;
pub mod episodes;
pub mod evolve;
pub mod types;

pub use confidence::{
    impact_confidence, link_prediction_confidence, pattern_detection_confidence,
    BasisBreakdown, ConfidenceBasis, ConfidenceTracker, PredictionFeedback,
    PredictionOutcome, RichConfidenceScore, SystemConfidence,
};
pub use critic::{GraphBasedCritic, MutationCritic};
pub use episodes::{
    generate_from_notes, generate_synthetic_episodes, DetectedPattern, EpisodeData,
    EpisodeOutcome, GateResult, KnowledgeNote, NoteType, OutcomeType,
};
pub use anonymize::{
    consent_gate_export, AnonymizationPipeline, AnonymizationStage, EntityGeneralizer,
    ExportError, MetricNoise, PathStripper, PipelineConfig, SecretDetector, SharingPolicy,
    StageConfig,
};
pub use evolve::EvolutionEngine;
pub use types::*;
