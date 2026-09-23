//! Dynamic model registry backed by the Anthropic Models API.
//!
//! Fetches the live model list from `GET https://api.anthropic.com/v1/models`
//! when an `ANTHROPIC_API_KEY` is available, caches it with a configurable
//! TTL (`MODEL_REFRESH_TTL_SECS`, default 6h), and falls back to the static
//! `ClaudeModel::all()` catalog when the API is unreachable or no key is set.

use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::models::claude::ClaudeModel;

const ANTHROPIC_MODELS_URL: &str = "https://api.anthropic.com/v1/models";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_TTL_SECS: u64 = 21_600; // 6 hours
const DEFAULT_CONTEXT_WINDOW: i32 = 200_000;

#[derive(Debug, Deserialize)]
struct ModelsPage {
    data: Vec<RemoteModel>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RemoteModel {
    id: String,
    display_name: String,
    /// Context window; present on the Models API since Mar 2026.
    #[serde(default)]
    max_input_tokens: Option<i64>,
}

struct CacheState {
    models: Vec<ClaudeModel>,
    last_refresh: Option<Instant>,
    /// True when `models` came from the live API (vs the static fallback).
    from_remote: bool,
}

pub struct ModelRegistry {
    cache: RwLock<CacheState>,
    http: reqwest::Client,
    api_key: Option<String>,
    ttl: Duration,
}

impl ModelRegistry {
    pub fn new() -> Self {
        let ttl_secs = std::env::var("MODEL_REFRESH_TTL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_TTL_SECS);
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty());

        if api_key.is_none() {
            info!("ANTHROPIC_API_KEY not set — /v1/models will serve the static model catalog");
        }

        Self {
            cache: RwLock::new(CacheState {
                models: ClaudeModel::all(),
                last_refresh: None,
                from_remote: false,
            }),
            http: reqwest::Client::new(),
            api_key,
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// Returns the cached model list, lazily refreshing from the Anthropic
    /// Models API when the TTL has expired.
    pub async fn get_models(&self) -> Vec<ClaudeModel> {
        if self.needs_refresh().await {
            // Best-effort refresh; on failure we keep serving the cache.
            let _ = self.refresh().await;
        }
        self.cache.read().await.models.clone()
    }

    /// Forces an immediate refresh from the Anthropic Models API.
    /// Returns Ok(true) if the list was refreshed from the API,
    /// Ok(false) if no API key is configured (static list kept).
    pub async fn refresh(&self) -> Result<bool, String> {
        let Some(api_key) = self.api_key.as_deref() else {
            // No key: mark refresh attempt so we don't retry on every call.
            let mut cache = self.cache.write().await;
            cache.last_refresh = Some(Instant::now());
            return Ok(false);
        };

        match self.fetch_remote(api_key).await {
            Ok(models) if !models.is_empty() => {
                let count = models.len();
                let mut cache = self.cache.write().await;
                cache.models = models;
                cache.last_refresh = Some(Instant::now());
                cache.from_remote = true;
                info!("Model registry refreshed from Anthropic Models API ({count} models)");
                Ok(true)
            },
            Ok(_) => {
                warn!("Anthropic Models API returned an empty list; keeping current catalog");
                let mut cache = self.cache.write().await;
                cache.last_refresh = Some(Instant::now());
                Err("Models API returned an empty list".to_string())
            },
            Err(e) => {
                warn!("Failed to refresh models from Anthropic API: {e}; keeping current catalog");
                let mut cache = self.cache.write().await;
                cache.last_refresh = Some(Instant::now());
                Err(e)
            },
        }
    }

    /// Whether the cache came from the live API.
    pub async fn is_dynamic(&self) -> bool {
        self.cache.read().await.from_remote
    }

    async fn needs_refresh(&self) -> bool {
        let cache = self.cache.read().await;
        match cache.last_refresh {
            None => true,
            Some(at) => at.elapsed() >= self.ttl,
        }
    }

    async fn fetch_remote(&self, api_key: &str) -> Result<Vec<ClaudeModel>, String> {
        let mut models = Vec::new();
        let mut after_id: Option<String> = None;

        loop {
            let mut req = self
                .http
                .get(ANTHROPIC_MODELS_URL)
                .header("x-api-key", api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .query(&[("limit", "100")]);
            if let Some(ref cursor) = after_id {
                req = req.query(&[("after_id", cursor.as_str())]);
            }

            let resp = req
                .send()
                .await
                .map_err(|e| format!("request failed: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("Models API returned HTTP {}", resp.status()));
            }
            let page: ModelsPage = resp
                .json()
                .await
                .map_err(|e| format!("invalid response body: {e}"))?;

            debug!(
                "Fetched {} models from Anthropic Models API",
                page.data.len()
            );
            models.extend(page.data.into_iter().map(|m| {
                ClaudeModel {
                    id: m.id,
                    display_name: m.display_name,
                    context_window: m
                        .max_input_tokens
                        .map(|v| v.clamp(0, i32::MAX as i64) as i32)
                        .unwrap_or(DEFAULT_CONTEXT_WINDOW),
                }
            }));

            if page.has_more {
                match page.last_id {
                    Some(id) => after_id = Some(id),
                    None => break,
                }
            } else {
                break;
            }
        }

        Ok(models)
    }
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}
