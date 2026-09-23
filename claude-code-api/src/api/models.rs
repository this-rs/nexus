use std::sync::Arc;

use crate::core::model_registry::ModelRegistry;
use crate::models::{
    error::ApiResult,
    openai::{Model, ModelList},
};
use axum::{Json, extract::State, response::IntoResponse};
use chrono::Utc;

fn to_model_list(models: Vec<crate::models::claude::ClaudeModel>) -> ModelList {
    let models: Vec<Model> = models
        .into_iter()
        .map(|m| Model {
            id: m.id,
            object: "model".to_string(),
            created: Utc::now().timestamp(),
            owned_by: "anthropic".to_string(),
        })
        .collect();

    ModelList {
        object: "list".to_string(),
        data: models,
    }
}

/// GET /v1/models — serves the model catalog from the registry
/// (live Anthropic Models API data when available, static fallback otherwise).
pub async fn list_models(
    State(registry): State<Arc<ModelRegistry>>,
) -> ApiResult<impl IntoResponse> {
    let models = registry.get_models().await;
    Ok(Json(to_model_list(models)))
}

/// POST /v1/models/refresh — forces an immediate refresh from the
/// Anthropic Models API and returns the refreshed list.
pub async fn refresh_models(
    State(registry): State<Arc<ModelRegistry>>,
) -> ApiResult<impl IntoResponse> {
    let refreshed = registry.refresh().await;
    let models = registry.get_models().await;
    let list = to_model_list(models);

    let body = serde_json::json!({
        "object": list.object,
        "data": list.data,
        "refreshed": refreshed.is_ok(),
        "dynamic": registry.is_dynamic().await,
        "detail": match refreshed {
            Ok(true) => "refreshed from Anthropic Models API".to_string(),
            Ok(false) => "no ANTHROPIC_API_KEY configured; serving static catalog".to_string(),
            Err(e) => e,
        },
    });
    Ok(Json(body))
}
