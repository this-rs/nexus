//! A keyless engine's HTML endpoint (DuckDuckGo's HTML page) (N22).
//!
//! **Fragile and off by default.** It parses a page meant for people, which can change or start
//! refusing automated requests at any time, and the engine's terms of use may not allow it.
//! It exists for installations with no key and no SearXNG instance, and only runs when the
//! operator asks for it by name (`--search-engine html`).

use std::sync::Arc;

use async_trait::async_trait;
use regex::Regex;
use url::Url;

use super::backend::{SearchBackend, SearchError, SearchHit, SearchQuery};
use super::util::{decode_entities, from_web, plain_text};
use crate::web::{Fetcher, Outcome, decode};

/// DuckDuckGo's HTML endpoint.
pub const DDG_HTML_ENDPOINT: &str = "https://html.duckduckgo.com/html/";

/// The HTML-scraping engine.
pub struct HtmlBackend {
    endpoint: String,
    fetcher: Arc<Fetcher>,
}

impl std::fmt::Debug for HtmlBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HtmlBackend")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl HtmlBackend {
    /// Creating one is an explicit act: nothing builds it by default.
    pub fn explicitly_enabled(endpoint: impl Into<String>, fetcher: Arc<Fetcher>) -> Self {
        Self {
            endpoint: endpoint.into(),
            fetcher,
        }
    }
}

/// The destination of a result link: DuckDuckGo wraps it as `//duckduckgo.com/l/?uddg=<url>`.
fn unwrap_redirect(href: &str) -> String {
    let href = decode_entities(href);
    let absolute = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href
    };
    if let Ok(parsed) = Url::parse(&absolute)
        && let Some((_, target)) = parsed.query_pairs().find(|(name, _)| name == "uddg")
    {
        return target.into_owned();
    }
    absolute
}

#[async_trait]
impl SearchBackend for HtmlBackend {
    fn id(&self) -> &str {
        "html"
    }

    async fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>, SearchError> {
        let mut url = Url::parse(&self.endpoint)
            .map_err(|e| SearchError::NotConfigured(format!("bad endpoint: {e}")))?;
        url.query_pairs_mut().append_pair("q", &query.text);
        let outcome = self
            .fetcher
            .get(url.as_str(), &[("Accept", "text/html")])
            .await
            .map_err(|e| from_web(e, false))?;
        let page = match outcome {
            Outcome::Page(page) => page,
            Outcome::Redirect { .. } => {
                return Err(SearchError::Unavailable(
                    "the endpoint redirected elsewhere".into(),
                ));
            },
        };
        let html = decode(&page.body, page.content_type.as_deref());
        let link = Regex::new(
            r#"(?s)<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#,
        )
        .map_err(|e| SearchError::BadResponse(e.to_string()))?;
        let snippet = Regex::new(r#"(?s)<a[^>]*class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</a>"#)
            .map_err(|e| SearchError::BadResponse(e.to_string()))?;
        let links: Vec<_> = link.captures_iter(&html).collect();
        if links.is_empty() && !html.contains("result") {
            // A page with no results markup at all is an interstitial (a captcha, a block).
            return Err(SearchError::BadResponse(
                "no results markup: the page may be a block page".into(),
            ));
        }
        let snippets: Vec<String> = snippet
            .captures_iter(&html)
            .map(|c| plain_text(&c[1]))
            .collect();
        Ok(links
            .iter()
            .enumerate()
            .map(|(i, c)| SearchHit {
                title: plain_text(&c[2]),
                url: unwrap_redirect(&c[1]),
                snippet: snippets.get(i).cloned().filter(|s| !s.is_empty()),
            })
            .take(query.limit)
            .collect())
    }
}
