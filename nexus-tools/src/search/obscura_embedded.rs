//! The keyless `obscura` engine **embedded** (`obscura` feature): Obscura's SERP search run in
//! this process, on a browser thread of its own ([`crate::obscura::run_isolated`]).
//!
//! The same contract as the external engine ([`super::ObscuraBackend`], nexus #96): one fresh
//! browser per query (no cookies kept between queries), a 30 s limit, an answer over 2 MiB
//! refused, results cleaned by [`super::parse_serp`]. A cancelled call stops the browser:
//! dropping the search terminates its JavaScript and drops its page loads.

use std::time::Duration;

use async_trait::async_trait;

use super::backend::{SearchBackend, SearchError, SearchHit, SearchQuery};
use super::obscura::parse_serp;
use crate::obscura::{
    CALL_TIMEOUT, MAX_ANSWER_BYTES, Stopped, private_network_refusal, run_isolated,
};

/// DuckDuckGo's HTML results page, the one Obscura reads (its origin is what a project consents
/// to for this engine).
pub const OBSCURA_SERP_ENDPOINT: &str = "https://html.duckduckgo.com/html/?q=";

/// Obscura's SERP search, in process.
#[derive(Debug, Clone)]
pub struct EmbeddedObscuraBackend {
    /// The results page, the query appended (form-encoded).
    endpoint: String,
    timeout: Duration,
}

impl Default for EmbeddedObscuraBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl EmbeddedObscuraBackend {
    /// The engine against DuckDuckGo's HTML results page.
    pub fn new() -> Self {
        Self {
            endpoint: OBSCURA_SERP_ENDPOINT.to_owned(),
            timeout: CALL_TIMEOUT,
        }
    }

    /// Another results page, the query appended to it (tests: a fixed `data:` page).
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Another time limit (tests).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl SearchBackend for EmbeddedObscuraBackend {
    fn id(&self) -> &str {
        "obscura"
    }

    async fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>, SearchError> {
        if let Some(why) = private_network_refusal() {
            return Err(SearchError::NotConfigured(why));
        }
        let encoded: String = url::form_urlencoded::byte_serialize(query.text.as_bytes()).collect();
        let page = format!("{}{encoded}", self.endpoint);
        let limit = query.limit.max(1);
        let answer = run_isolated(
            move || async move {
                let mut state = obscura_mcp::BrowserState::new(None, None, false);
                obscura_mcp::serp_search_at(&mut state, &page, limit).await
            },
            self.timeout,
        )
        .await;
        let results = match answer {
            Ok(Ok(results)) => results,
            Ok(Err(why)) => return Err(SearchError::Unavailable(format!("obscura: {why}"))),
            Err(Stopped::TimedOut) => return Err(SearchError::Timeout),
            Err(Stopped::Failed(why)) => {
                return Err(SearchError::Unavailable(format!("obscura: {why}")));
            },
        };
        // Through the same reader as the external engine's printed answer: same cleaning, same
        // ceiling.
        let json = serde_json::to_vec(&results)
            .map_err(|e| SearchError::BadResponse(format!("obscura's answer: {e}")))?;
        if json.len() > MAX_ANSWER_BYTES {
            return Err(SearchError::BadResponse(format!(
                "obscura answered more than {} KiB: not a search answer",
                MAX_ANSWER_BYTES / 1024
            )));
        }
        parse_serp(&json, query.limit)
    }
}
