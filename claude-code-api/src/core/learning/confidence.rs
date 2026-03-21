/// Self-evaluation confidence system for calibrated predictions.
///
/// Provides `RichConfidenceScore` with basis tracking, impact analysis confidence,
/// link prediction confidence, system-level aggregation, and feedback-based calibration.
///
/// # References
/// - ELL (2025) — "Experience-driven Lifelong Learning"
///   4th pillar: self-evaluation. The system must know *how confident* it is in its
///   predictions, expose that to users, and recalibrate via feedback loops.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// The basis on which a confidence score was computed.
///
/// # References
/// - ELL (2025) — self-evaluation requires transparent confidence provenance
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ConfidenceBasis {
    /// Confidence derived from local graph density (edges/nodes in neighborhood).
    GraphDensity,
    /// Confidence derived from converging independent signals (co-change, structural, community).
    SignalConvergence,
    /// Confidence derived from sample size and variance of observations.
    SampleVariance,
    /// Weighted combination of multiple bases.
    Composite,
}

/// A rich confidence score that carries its provenance metadata.
///
/// Unlike the simple `ConfidenceScore(f64)` wrapper used for mutation critics,
/// this struct tracks *why* the system is confident and how much data backs it.
///
/// # References
/// - ELL (2025) — "Experience-driven Lifelong Learning", 4th pillar: self-evaluation
///   A prediction without calibrated confidence is incomplete.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RichConfidenceScore {
    /// Confidence value in [0.0, 1.0].
    pub score: f64,
    /// What the confidence is based on.
    pub basis: ConfidenceBasis,
    /// Number of data points that contributed to this score.
    pub sample_size: usize,
}

impl RichConfidenceScore {
    /// Create a new score, clamping to [0.0, 1.0].
    pub fn new(score: f64, basis: ConfidenceBasis, sample_size: usize) -> Self {
        Self {
            score: score.clamp(0.0, 1.0),
            basis,
            sample_size,
        }
    }
}

// ---------------------------------------------------------------------------
// Impact analysis confidence (graph density based)
// ---------------------------------------------------------------------------

/// Compute confidence for `analyze_impact` based on local graph density.
///
/// density = edges / nodes in the k=2 neighborhood of the target node.
/// - Dense neighborhood (many edges per node) → high confidence.
/// - Sparse neighborhood → low confidence.
///
/// # References
/// - ELL (2025) — self-evaluation: confidence scales with information density
pub fn impact_confidence(neighborhood_edges: usize, neighborhood_nodes: usize) -> RichConfidenceScore {
    if neighborhood_nodes == 0 {
        return RichConfidenceScore::new(0.0, ConfidenceBasis::GraphDensity, 0);
    }

    let density = neighborhood_edges as f64 / neighborhood_nodes as f64;
    // Exponential saturation: density ~3 → ~0.85, density ~0.4 → ~0.25
    let score = 1.0 - (-density * 0.6).exp();

    RichConfidenceScore::new(score, ConfidenceBasis::GraphDensity, neighborhood_nodes)
}

// ---------------------------------------------------------------------------
// Link prediction confidence (signal convergence based)
// ---------------------------------------------------------------------------

/// Compute confidence for `predict_missing_links` based on converging signals.
///
/// Each independent signal (co-change, structural similarity, community membership)
/// adds evidence. More converging signals → higher confidence.
///
/// # References
/// - ELL (2025) — self-evaluation: triangulation of independent signals
pub fn link_prediction_confidence(
    co_change_signal: bool,
    structural_signal: bool,
    community_signal: bool,
) -> RichConfidenceScore {
    let signal_count = [co_change_signal, structural_signal, community_signal]
        .iter()
        .filter(|&&s| s)
        .count();

    let score = match signal_count {
        0 => 0.1,
        1 => 0.3,
        2 => 0.6,
        3 => 0.85,
        _ => 0.1,
    };

    RichConfidenceScore::new(score, ConfidenceBasis::SignalConvergence, signal_count)
}

// ---------------------------------------------------------------------------
// Pattern detection confidence (sample variance based)
// ---------------------------------------------------------------------------

/// Compute confidence for pattern detection based on sample size and variance.
///
/// - Large sample + low variance → high confidence
/// - Small sample or high variance → low confidence
///
/// # References
/// - ELL (2025) — self-evaluation: statistical grounding of pattern claims
pub fn pattern_detection_confidence(sample_size: usize, variance: f64) -> RichConfidenceScore {
    if sample_size == 0 {
        return RichConfidenceScore::new(0.0, ConfidenceBasis::SampleVariance, 0);
    }

    // Size factor: diminishing returns, 20+ samples → ~0.95
    let size_factor = 1.0 - 1.0 / (1.0 + sample_size as f64 / 8.0);
    // Variance penalty: variance > 1.0 sharply reduces confidence
    let variance_factor = 1.0 / (1.0 + variance);

    let score = size_factor * variance_factor;

    RichConfidenceScore::new(score, ConfidenceBasis::SampleVariance, sample_size)
}

// ---------------------------------------------------------------------------
// System-level confidence aggregation
// ---------------------------------------------------------------------------

/// Outcome of a prediction as reported by the user.
///
/// # References
/// - ELL (2025) — feedback loop for calibration adjustment
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PredictionOutcome {
    Confirmed,
    Refuted,
}

/// A single prediction feedback record.
///
/// # References
/// - ELL (2025) — meta-learning: recalibrate via user feedback
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionFeedback {
    pub id: Uuid,
    pub prediction_id: Uuid,
    pub predicted_confidence: f64,
    pub actual_outcome: PredictionOutcome,
    pub created_at: DateTime<Utc>,
}

/// Breakdown of basis contributions in the system confidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BasisBreakdown {
    pub graph_density_avg: Option<f64>,
    pub signal_convergence_avg: Option<f64>,
    pub sample_variance_avg: Option<f64>,
    pub composite_avg: Option<f64>,
}

/// Aggregated system confidence for a project.
///
/// Represents "how well the PO knows itself" — a meta-confidence that
/// combines recent prediction accuracy with rolling bias correction.
///
/// # References
/// - ELL (2025) — "Experience-driven Lifelong Learning", 4th pillar
///   SystemConfidence is the self-evaluation metric that tells users
///   when to trust the system and when to verify manually.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfidence {
    /// Aggregated confidence score [0.0, 1.0].
    pub score: f64,
    /// Breakdown by basis type.
    pub basis_breakdown: BasisBreakdown,
    /// Total number of predictions considered.
    pub sample_size: usize,
    /// Rolling bias correction (negative = system is overconfident).
    pub bias_correction: f64,
}

/// Tracker that maintains rolling confidence state and feedback for a project.
///
/// # References
/// - ELL (2025) — meta-learning via calibration feedback loop
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceTracker {
    /// Recent prediction scores (ring buffer, max `window_size`).
    predictions: Vec<RichConfidenceScore>,
    /// Feedback entries for calibration.
    feedbacks: Vec<PredictionFeedback>,
    /// Maximum number of predictions to track.
    window_size: usize,
}

impl Default for ConfidenceTracker {
    fn default() -> Self {
        Self::new(100)
    }
}

impl ConfidenceTracker {
    pub fn new(window_size: usize) -> Self {
        Self {
            predictions: Vec::new(),
            feedbacks: Vec::new(),
            window_size,
        }
    }

    /// Record a new prediction confidence.
    pub fn record_prediction(&mut self, score: RichConfidenceScore) {
        if self.predictions.len() >= self.window_size {
            self.predictions.remove(0);
        }
        self.predictions.push(score);
    }

    /// Record user feedback on a prediction.
    pub fn record_feedback(&mut self, prediction_id: Uuid, predicted_confidence: f64, outcome: PredictionOutcome) {
        if self.feedbacks.len() >= self.window_size {
            self.feedbacks.remove(0);
        }
        self.feedbacks.push(PredictionFeedback {
            id: Uuid::new_v4(),
            prediction_id,
            predicted_confidence,
            actual_outcome: outcome,
            created_at: Utc::now(),
        });
    }

    /// Compute the rolling bias correction.
    ///
    /// bias = mean(predicted_confidence) - accuracy_rate
    /// Negative bias means system is overconfident.
    ///
    /// # References
    /// - ELL (2025) — calibration via rolling bias correction
    pub fn compute_bias_correction(&self) -> f64 {
        if self.feedbacks.is_empty() {
            return 0.0;
        }

        let mean_predicted: f64 = self.feedbacks.iter()
            .map(|f| f.predicted_confidence)
            .sum::<f64>() / self.feedbacks.len() as f64;

        let accuracy: f64 = self.feedbacks.iter()
            .filter(|f| f.actual_outcome == PredictionOutcome::Confirmed)
            .count() as f64 / self.feedbacks.len() as f64;

        // Positive = underconfident, Negative = overconfident
        accuracy - mean_predicted
    }

    /// Compute the aggregated `SystemConfidence` for this project.
    ///
    /// # References
    /// - ELL (2025) — system-level self-evaluation metric
    pub fn system_confidence(&self) -> SystemConfidence {
        let sample_size = self.predictions.len();
        let bias_correction = self.compute_bias_correction();

        if sample_size == 0 {
            return SystemConfidence {
                score: 0.5, // no data → neutral
                basis_breakdown: BasisBreakdown {
                    graph_density_avg: None,
                    signal_convergence_avg: None,
                    sample_variance_avg: None,
                    composite_avg: None,
                },
                sample_size: 0,
                bias_correction,
            };
        }

        // Group predictions by basis and compute averages
        let mut graph_scores = Vec::new();
        let mut signal_scores = Vec::new();
        let mut variance_scores = Vec::new();
        let mut composite_scores = Vec::new();

        for p in &self.predictions {
            match p.basis {
                ConfidenceBasis::GraphDensity => graph_scores.push(p.score),
                ConfidenceBasis::SignalConvergence => signal_scores.push(p.score),
                ConfidenceBasis::SampleVariance => variance_scores.push(p.score),
                ConfidenceBasis::Composite => composite_scores.push(p.score),
            }
        }

        let avg = |v: &[f64]| -> Option<f64> {
            if v.is_empty() { None } else { Some(v.iter().sum::<f64>() / v.len() as f64) }
        };

        let overall_avg: f64 = self.predictions.iter().map(|p| p.score).sum::<f64>() / sample_size as f64;

        // Apply bias correction (clamp result to [0, 1])
        let corrected_score = (overall_avg + bias_correction).clamp(0.0, 1.0);

        SystemConfidence {
            score: corrected_score,
            basis_breakdown: BasisBreakdown {
                graph_density_avg: avg(&graph_scores),
                signal_convergence_avg: avg(&signal_scores),
                sample_variance_avg: avg(&variance_scores),
                composite_avg: avg(&composite_scores),
            },
            sample_size,
            bias_correction,
        }
    }

    /// Get all feedbacks (for API serialization).
    pub fn feedbacks(&self) -> &[PredictionFeedback] {
        &self.feedbacks
    }

    /// Get all predictions (for API serialization).
    pub fn predictions(&self) -> &[RichConfidenceScore] {
        &self.predictions
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Step 1: ConfidenceScore types --

    #[test]
    fn test_rich_confidence_score_clamping() {
        let s = RichConfidenceScore::new(1.5, ConfidenceBasis::GraphDensity, 10);
        assert_eq!(s.score, 1.0);
        let s = RichConfidenceScore::new(-0.5, ConfidenceBasis::GraphDensity, 0);
        assert_eq!(s.score, 0.0);
    }

    #[test]
    fn test_rich_confidence_score_serde_roundtrip() {
        let s = RichConfidenceScore::new(0.75, ConfidenceBasis::SignalConvergence, 3);
        let json = serde_json::to_string(&s).unwrap();
        let deserialized: RichConfidenceScore = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.score, 0.75);
        assert_eq!(deserialized.basis, ConfidenceBasis::SignalConvergence);
        assert_eq!(deserialized.sample_size, 3);
    }

    #[test]
    fn test_confidence_basis_serde_all_variants() {
        for basis in [
            ConfidenceBasis::GraphDensity,
            ConfidenceBasis::SignalConvergence,
            ConfidenceBasis::SampleVariance,
            ConfidenceBasis::Composite,
        ] {
            let json = serde_json::to_string(&basis).unwrap();
            let deserialized: ConfidenceBasis = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, basis);
        }
    }

    // -- Step 2: impact_confidence (graph density) --

    #[test]
    fn test_impact_confidence_dense_graph() {
        // Dense graph: 15 edges around 5 nodes → density=3 → confidence should be > 0.8
        let c = impact_confidence(15, 5);
        assert!(
            c.score > 0.8,
            "Dense graph should yield confidence > 0.8, got {}",
            c.score
        );
        assert_eq!(c.basis, ConfidenceBasis::GraphDensity);
    }

    #[test]
    fn test_impact_confidence_sparse_graph() {
        // Sparse graph: 2 edges around 5 nodes → density=0.4 → confidence should be < 0.4
        let c = impact_confidence(2, 5);
        assert!(
            c.score < 0.4,
            "Sparse graph should yield confidence < 0.4, got {}",
            c.score
        );
    }

    #[test]
    fn test_impact_confidence_empty_graph() {
        let c = impact_confidence(0, 0);
        assert_eq!(c.score, 0.0);
        assert_eq!(c.sample_size, 0);
    }

    #[test]
    fn test_impact_confidence_very_dense() {
        // Very dense: 50 edges, 3 nodes
        let c = impact_confidence(50, 3);
        assert!(c.score > 0.8);
    }

    // -- Step 3: link_prediction_confidence (signal convergence) --

    #[test]
    fn test_link_confidence_three_signals() {
        let c = link_prediction_confidence(true, true, true);
        assert!(
            c.score > 0.7,
            "3 signals should yield confidence > 0.7, got {}",
            c.score
        );
        assert_eq!(c.basis, ConfidenceBasis::SignalConvergence);
    }

    #[test]
    fn test_link_confidence_one_signal() {
        let c = link_prediction_confidence(true, false, false);
        assert!(
            c.score < 0.4,
            "1 signal should yield confidence < 0.4, got {}",
            c.score
        );
    }

    #[test]
    fn test_link_confidence_no_signals() {
        let c = link_prediction_confidence(false, false, false);
        assert!(c.score < 0.2);
    }

    #[test]
    fn test_link_confidence_two_signals() {
        let c = link_prediction_confidence(true, true, false);
        assert!(c.score > 0.5 && c.score < 0.8);
    }

    // -- Pattern detection confidence --

    #[test]
    fn test_pattern_confidence_large_sample_low_variance() {
        let c = pattern_detection_confidence(30, 0.1);
        assert!(c.score > 0.7, "Large sample + low variance should be confident, got {}", c.score);
    }

    #[test]
    fn test_pattern_confidence_small_sample() {
        let c = pattern_detection_confidence(2, 0.1);
        assert!(c.score < 0.5, "Small sample should be less confident, got {}", c.score);
    }

    #[test]
    fn test_pattern_confidence_high_variance() {
        let c = pattern_detection_confidence(20, 5.0);
        assert!(c.score < 0.5, "High variance should reduce confidence, got {}", c.score);
    }

    #[test]
    fn test_pattern_confidence_zero_samples() {
        let c = pattern_detection_confidence(0, 0.0);
        assert_eq!(c.score, 0.0);
    }

    // -- Step 4 & 5: SystemConfidence + feedback loop --

    #[test]
    fn test_system_confidence_no_data() {
        let tracker = ConfidenceTracker::default();
        let sc = tracker.system_confidence();
        assert_eq!(sc.score, 0.5); // neutral
        assert_eq!(sc.sample_size, 0);
    }

    #[test]
    fn test_system_confidence_aggregation() {
        let mut tracker = ConfidenceTracker::new(100);
        tracker.record_prediction(RichConfidenceScore::new(0.9, ConfidenceBasis::GraphDensity, 10));
        tracker.record_prediction(RichConfidenceScore::new(0.8, ConfidenceBasis::SignalConvergence, 3));
        tracker.record_prediction(RichConfidenceScore::new(0.7, ConfidenceBasis::SampleVariance, 20));

        let sc = tracker.system_confidence();
        assert_eq!(sc.sample_size, 3);
        // Average: (0.9 + 0.8 + 0.7) / 3 = 0.8
        assert!((sc.score - 0.8).abs() < 0.01);
        assert!(sc.basis_breakdown.graph_density_avg.is_some());
        assert!(sc.basis_breakdown.signal_convergence_avg.is_some());
        assert!(sc.basis_breakdown.sample_variance_avg.is_some());
    }

    #[test]
    fn test_feedback_bias_correction_overconfident() {
        let mut tracker = ConfidenceTracker::new(100);

        // Record 5 predictions at high confidence
        for _ in 0..5 {
            tracker.record_prediction(RichConfidenceScore::new(0.9, ConfidenceBasis::GraphDensity, 10));
        }

        // All 5 feedbacks refute the predictions → system was overconfident
        for _ in 0..5 {
            tracker.record_feedback(
                Uuid::new_v4(),
                0.9,
                PredictionOutcome::Refuted,
            );
        }

        let sc = tracker.system_confidence();
        // Bias correction should be negative (accuracy=0, mean_predicted=0.9 → correction = -0.9)
        assert!(sc.bias_correction < 0.0, "Bias should be negative for overconfident system");
        // System confidence should drop significantly below 0.7
        assert!(
            sc.score < 0.7,
            "After 5 refuted high-confidence predictions, system confidence should be < 0.7, got {}",
            sc.score
        );
    }

    #[test]
    fn test_feedback_bias_correction_well_calibrated() {
        let mut tracker = ConfidenceTracker::new(100);

        for _ in 0..10 {
            tracker.record_prediction(RichConfidenceScore::new(0.7, ConfidenceBasis::GraphDensity, 10));
        }

        // 7 out of 10 confirmed → accuracy matches predicted confidence
        for i in 0..10 {
            let outcome = if i < 7 { PredictionOutcome::Confirmed } else { PredictionOutcome::Refuted };
            tracker.record_feedback(Uuid::new_v4(), 0.7, outcome);
        }

        let sc = tracker.system_confidence();
        // Bias correction should be near 0 (accuracy=0.7, predicted=0.7)
        assert!(
            sc.bias_correction.abs() < 0.05,
            "Well-calibrated system should have near-zero bias, got {}",
            sc.bias_correction
        );
    }

    #[test]
    fn test_tracker_window_size_respected() {
        let mut tracker = ConfidenceTracker::new(3);

        for i in 0..5 {
            tracker.record_prediction(RichConfidenceScore::new(i as f64 * 0.2, ConfidenceBasis::Composite, 1));
        }

        assert_eq!(tracker.predictions().len(), 3);
    }

    #[test]
    fn test_system_confidence_serde_roundtrip() {
        let sc = SystemConfidence {
            score: 0.75,
            basis_breakdown: BasisBreakdown {
                graph_density_avg: Some(0.8),
                signal_convergence_avg: Some(0.7),
                sample_variance_avg: None,
                composite_avg: None,
            },
            sample_size: 10,
            bias_correction: -0.05,
        };
        let json = serde_json::to_string(&sc).unwrap();
        let deserialized: SystemConfidence = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.score, 0.75);
        assert_eq!(deserialized.sample_size, 10);
    }

    #[test]
    fn test_prediction_feedback_serde_roundtrip() {
        let fb = PredictionFeedback {
            id: Uuid::new_v4(),
            prediction_id: Uuid::new_v4(),
            predicted_confidence: 0.85,
            actual_outcome: PredictionOutcome::Confirmed,
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&fb).unwrap();
        let deserialized: PredictionFeedback = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.actual_outcome, PredictionOutcome::Confirmed);
    }
}
