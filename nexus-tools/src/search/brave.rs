//! A keyed JSON search API (Brave Search's shape) (N22).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use url::Url;

use super::backend::{SearchBackend, SearchError, SearchHit, SearchQuery};
use super::secret::KeySource;
use super::util::{from_web, plain_text};
use crate::web::{Fetcher, Outcome};

/// Brave Search's public endpoint.
pub const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";

/// The keyed API engine.
pub struct KeyedApiBackend {
    id: String,
    endpoint: String,
    key: KeySource,
    fetcher: Arc<Fetcher>,
}

impl std::fmt::Debug for KeyedApiBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyedApiBackend")
            .field("id", &self.id)
            .field("endpoint", &self.endpoint)
            .field("key", &self.key)
            .finish()
    }
}

impl KeyedApiBackend {
    /// An engine named `id` at `endpoint`, authenticated with the key `key` refers to.
    pub fn new(
        id: impl Into<String>,
        endpoint: impl Into<String>,
        key: KeySource,
        fetcher: Arc<Fetcher>,
    ) -> Self {
        Self {
            id: id.into(),
            endpoint: endpoint.into(),
            key,
            fetcher,
        }
    }
}

#[async_trait]
impl SearchBackend for KeyedApiBackend {
    fn id(&self) -> &str {
        &self.id
    }

    async fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>, SearchError> {
        let secret = self.key.resolve().map_err(SearchError::NotConfigured)?;
        let mut url = Url::parse(&self.endpoint)
            .map_err(|e| SearchError::NotConfigured(format!("bad endpoint: {e}")))?;
        url.query_pairs_mut()
            .append_pair("q", &query.text)
            .append_pair("count", &query.limit.min(20).to_string());
        // The key goes in a header, never in the URL: URLs end up in logs and error messages.
        let outcome = self
            .fetcher
            .get(
                url.as_str(),
                &[
                    ("X-Subscription-Token", secret.expose()),
                    ("Accept", "application/json"),
                ],
            )
            .await
            .map_err(|e| from_web(e, true))?;
        let page = match outcome {
            Outcome::Page(page) => page,
            Outcome::Redirect { .. } => {
                return Err(SearchError::Unavailable(
                    "the endpoint redirected elsewhere".into(),
                ));
            },
        };
        let document: Value = serde_json::from_slice(&page.body)
            .map_err(|_| SearchError::BadResponse("the answer is not JSON".into()))?;
        let results = document
            .pointer("/web/results")
            .and_then(Value::as_array)
            .ok_or_else(|| SearchError::BadResponse("no web.results in the answer".into()))?;
        Ok(results
            .iter()
            .filter_map(|r| {
                let url = r.get("url")?.as_str()?.to_owned();
                let title = plain_text(r.get("title")?.as_str()?);
                let snippet = r
                    .get("description")
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
