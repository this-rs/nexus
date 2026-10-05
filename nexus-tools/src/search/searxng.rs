//! A self-hosted SearXNG instance (N22).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use url::Url;

use super::backend::{SearchBackend, SearchError, SearchHit, SearchQuery};
use super::util::{from_web, plain_text};
use crate::web::{Fetcher, Outcome, WebError};

/// A SearXNG instance, by URL. It needs the JSON output format enabled in its settings.
pub struct SearxngBackend {
    base: String,
    fetcher: Arc<Fetcher>,
}

impl std::fmt::Debug for SearxngBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearxngBackend")
            .field("base", &self.base)
            .finish()
    }
}

impl SearxngBackend {
    /// An engine at `base` (for example `http://searx.lan:8080`). An instance on a private
    /// address needs a `Fetcher` with `allow_private_network`: it is the operator's own service,
    /// configured by the operator, never chosen by the model.
    pub fn new(base: impl Into<String>, fetcher: Arc<Fetcher>) -> Self {
        Self {
            base: base.into(),
            fetcher,
        }
    }
}

#[async_trait]
impl SearchBackend for SearxngBackend {
    fn id(&self) -> &str {
        "searxng"
    }

    async fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>, SearchError> {
        let mut url = Url::parse(&format!("{}/search", self.base.trim_end_matches('/')))
            .map_err(|e| SearchError::NotConfigured(format!("bad instance URL: {e}")))?;
        url.query_pairs_mut()
            .append_pair("q", &query.text)
            .append_pair("format", "json");
        let outcome = match self
            .fetcher
            .get(url.as_str(), &[("Accept", "application/json")])
            .await
        {
            Ok(outcome) => outcome,
            // SearXNG answers 403 when the JSON format is not enabled in its settings.
            Err(WebError::HttpStatus { status: 403, .. }) => {
                return Err(SearchError::BadResponse(
                    "HTTP 403: the instance may have the json format disabled".into(),
                ));
            },
            Err(error) => return Err(from_web(error, false)),
        };
        let page = match outcome {
            Outcome::Page(page) => page,
            Outcome::Redirect { .. } => {
                return Err(SearchError::Unavailable(
                    "the instance redirected elsewhere".into(),
                ));
            },
        };
        let document: Value = serde_json::from_slice(&page.body)
            .map_err(|_| SearchError::BadResponse("the answer is not JSON".into()))?;
        let results = document
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| SearchError::BadResponse("no results in the answer".into()))?;
        Ok(results
            .iter()
            .filter_map(|r| {
                let url = r.get("url")?.as_str()?.to_owned();
                let title = plain_text(r.get("title")?.as_str()?);
                let snippet = r
                    .get("content")
                    .and_then(Value::as_str)
                    .map(plain_text)
                    .filter(|s| !s.is_empty());
                Some(SearchHit {
                    title,
                    url,
                    snippet,
                })
            })
            .take(query.limit)
            .collect())
    }
}
