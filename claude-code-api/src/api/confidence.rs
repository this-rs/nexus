/// API endpoints for the self-evaluation confidence system.
///
/// Provides:
/// - `GET /api/projects/:slug/confidence` — aggregated SystemConfidence
/// - `POST /api/projects/:slug/confidence/feedback` — submit prediction feedback
///
/// # References
/// - ELL (2025) — "Experience-driven Lifelong Learning"
///   4th pillar: self-evaluation. Expose calibrated confidence to users and
///   accept feedback to recalibrate predictions over time.
use axum::{
    Json,
    extract::{Path, State},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::core::learning::confidence::{
    ConfidenceTracker, PredictionOutcome, SystemConfidence,
};
use crate::models::error::ApiResult;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Shared state for confidence endpoints.
/// Maps project slug → ConfidenceTracker.
#[derive(Clone)]
pub struct ConfidenceState {
    pub trackers: Arc<RwLock<HashMap<String, ConfidenceTracker>>>,
}

impl ConfidenceState {
    pub fn new() -> Self {
        Self {
            trackers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn get_or_create_tracker(&self, slug: &str) -> ConfidenceTracker {
        let read = self.trackers.read();
        if let Some(tracker) = read.get(slug) {
            return tracker.clone();
        }
        drop(read);

        let mut write = self.trackers.write();
        write.entry(slug.to_string())
            .or_insert_with(ConfidenceTracker::default)
            .clone()
    }
}

impl Default for ConfidenceState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

/// Request body for submitting prediction feedback.
///
/// # References
/// - ELL (2025) — feedback loop for calibration
#[derive(Debug, Deserialize, Serialize)]
pub struct FeedbackRequest {
    pub prediction_id: Uuid,
    pub actual_outcome: OutcomeInput,
}

/// User-facing outcome enum for the feedback endpoint.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OutcomeInput {
    Confirmed,
    Refuted,
}

impl From<OutcomeInput> for PredictionOutcome {
    fn from(input: OutcomeInput) -> Self {
        match input {
            OutcomeInput::Confirmed => PredictionOutcome::Confirmed,
            OutcomeInput::Refuted => PredictionOutcome::Refuted,
        }
    }
}

/// Response for the feedback endpoint.
#[derive(Debug, Serialize)]
pub struct FeedbackResponse {
    pub message: String,
    pub updated_system_confidence: SystemConfidence,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/projects/:slug/confidence
///
/// Returns the aggregated SystemConfidence for a project.
///
/// # References
/// - ELL (2025) — self-evaluation: expose system confidence to users
#[allow(dead_code)]
pub async fn get_project_confidence(
    Path(slug): Path<String>,
    State(state): State<ConfidenceState>,
) -> ApiResult<impl IntoResponse> {
    let tracker = state.get_or_create_tracker(&slug);
    let confidence = tracker.system_confidence();
    Ok(Json(confidence))
}

/// POST /api/projects/:slug/confidence/feedback
///
/// Submit feedback on a prediction to recalibrate the system.
///
/// # References
/// - ELL (2025) — meta-learning via calibration feedback loop
#[allow(dead_code)]
pub async fn post_prediction_feedback(
    Path(slug): Path<String>,
    State(state): State<ConfidenceState>,
    Json(body): Json<FeedbackRequest>,
) -> ApiResult<impl IntoResponse> {
    // We need the predicted confidence for the given prediction_id.
    // In a real system, we'd look this up from a stored prediction log.
    // For now, we use a default of 0.5 if no matching prediction is found,
    // but try to find a matching prediction in the tracker.
    let predicted_confidence = {
        let read = state.trackers.read();
        if let Some(tracker) = read.get(&slug) {
            // Try to find the prediction — in a full system this would be indexed
            // For now, use the last prediction's score as a proxy
            tracker.predictions().last().map(|p| p.score).unwrap_or(0.5)
        } else {
            0.5
        }
    };

    // Record the feedback
    {
        let mut write = state.trackers.write();
        let tracker = write.entry(slug.clone())
            .or_insert_with(ConfidenceTracker::default);
        tracker.record_feedback(
            body.prediction_id,
            predicted_confidence,
            body.actual_outcome.into(),
        );
    }

    // Compute updated confidence
    let tracker = state.get_or_create_tracker(&slug);
    let updated = tracker.system_confidence();

    Ok(Json(FeedbackResponse {
        message: "Feedback recorded, calibration updated".to_string(),
        updated_system_confidence: updated,
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::learning::confidence::RichConfidenceScore;
    use crate::core::learning::confidence::ConfidenceBasis;

    #[test]
    fn test_confidence_state_default() {
        let state = ConfidenceState::new();
        let tracker = state.get_or_create_tracker("test-project");
        let sc = tracker.system_confidence();
        assert_eq!(sc.score, 0.5); // neutral for empty
        assert_eq!(sc.sample_size, 0);
    }

    #[test]
    fn test_feedback_request_serde() {
        let req = FeedbackRequest {
            prediction_id: Uuid::new_v4(),
            actual_outcome: OutcomeInput::Confirmed,
        };
        let json = serde_json::to_string(&req).unwrap();
        let deserialized: FeedbackRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.actual_outcome, OutcomeInput::Confirmed);
    }

    #[test]
    fn test_feedback_refuted_lowers_confidence() {
        let state = ConfidenceState::new();

        // Record some high-confidence predictions
        {
            let mut write = state.trackers.write();
            let tracker = write.entry("proj".to_string())
                .or_insert_with(ConfidenceTracker::default);

            for _ in 0..5 {
                tracker.record_prediction(RichConfidenceScore::new(
                    0.9,
                    ConfidenceBasis::GraphDensity,
                    10,
                ));
            }
        }

        // Record 5 refuted feedbacks
        {
            let mut write = state.trackers.write();
            let tracker = write.get_mut("proj").unwrap();
            for _ in 0..5 {
                tracker.record_feedback(Uuid::new_v4(), 0.9, PredictionOutcome::Refuted);
            }
        }

        let tracker = state.get_or_create_tracker("proj");
        let sc = tracker.system_confidence();
        assert!(
            sc.score < 0.7,
            "After 5 refuted high-confidence predictions, score should be < 0.7, got {}",
            sc.score
        );
    }

    #[test]
    fn test_outcome_input_conversion() {
        assert_eq!(
            PredictionOutcome::from(OutcomeInput::Confirmed),
            PredictionOutcome::Confirmed
        );
        assert_eq!(
            PredictionOutcome::from(OutcomeInput::Refuted),
            PredictionOutcome::Refuted
        );
    }
}
