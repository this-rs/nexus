//! `WebFetch` (N21): a page as Markdown, fetched without being a way into the user's network.
//!
//! What it promises: only public addresses (every address a name resolves to is checked, the
//! connection is pinned to the address that was checked, redirects are re-checked), a redirect
//! to another host is reported rather than followed, bodies and text are capped and the cut is
//! said, binary content is refused with a typed error, no credentials in URLs, a 15-minute
//! cache. It makes **no hidden model call**: the instruction (`prompt`) is for the session's
//! model, which reads the Markdown.
//!
//! `https` needs the `tls` feature; without it `PlainConnector` refuses it.

mod cache;
mod convert;
mod fetch;
mod ssrf;
#[cfg(feature = "tls")]
mod tls;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

pub use cache::{Clock, PageCache, SystemClock};
pub use convert::{decode, html_to_markdown};
pub use fetch::{
    Connect, FetchConfig, Fetcher, Io, Outcome, Page, PlainConnector, Resolve, SystemResolver,
    Target, WebError,
};
pub use ssrf::blocked_reason;
#[cfg(feature = "tls")]
pub use tls::TlsConnector;

use crate::limits::truncate;
use crate::registry::ToolRegistry;
use crate::tool::{Annotations, CallContext, Tool, ToolResult};

/// Characters of Markdown returned at most.
pub const MAX_TEXT_CHARS: usize = 100_000;
/// How long a page stays cached.
pub const CACHE_TTL: Duration = Duration::from_secs(15 * 60);

/// The `WebFetch` tool.
pub struct WebFetchTool {
    fetcher: Fetcher,
    cache: PageCache,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for WebFetchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebFetchTool")
    }
}

impl WebFetchTool {
    /// A `WebFetch` over `fetcher`.
    pub fn new(fetcher: Fetcher) -> Self {
        Self {
            fetcher,
            cache: PageCache::new(CACHE_TTL, 64),
            clock: Arc::new(SystemClock::default()),
        }
    }

    /// Replaces the clock (tests move it by hand).
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
}

/// Adds `WebFetch` to a registry.
pub fn register(registry: ToolRegistry, fetcher: Fetcher) -> ToolRegistry {
    registry.with(WebFetchTool::new(fetcher))
}

fn failure(error: &WebError) -> ToolResult {
    ToolResult::error(format!("WebFetch failed ({}): {error}", error.kind()))
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "WebFetch"
    }

    fn description(&self) -> &str {
        "Fetches a web page and returns it as Markdown (headings, lists, tables, code, links made \
         absolute). Only public http(s) addresses; http is upgraded to https. A redirect to another \
         host is reported, not followed: call again with the new URL. `prompt` says what you want \
         from the page; you apply it to the Markdown returned (no model is called for you). Pages are \
         cached for 15 minutes."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "format": "uri"},
                "prompt": {"type": "string", "description": "What to extract from the page"}
            },
            "required": ["url"]
        })
    }

    fn annotations(&self) -> Annotations {
        Annotations {
            read_only: true,
            idempotent: true,
            open_world: true,
            destructive: false,
        }
    }

    async fn call(&self, _context: &CallContext, arguments: Value) -> ToolResult {
        let Some(given) = arguments.get("url").and_then(Value::as_str) else {
            return ToolResult::error("`url` is required and must be a string");
        };
        let prompt = arguments
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let key = given.trim().to_owned();
        let now = self.clock.now_ms();
        if let Some(page) = self.cache.get(&key, now) {
            return ToolResult::ok(page);
        }
        match self.fetcher.fetch(given).await {
            Err(error) => failure(&error),
            Ok(Outcome::Redirect { from, to, status }) => ToolResult::ok(format!(
                "REDIRECT DETECTED: the URL redirects somewhere this tool does not follow on its own (another host, or a downgrade from https to http).\n\nOriginal URL: {from}\nRedirect URL: {to}\nStatus: {status}\n\nTo get the content, call WebFetch again with:\n- url: \"{to}\"\n- prompt: \"{prompt}\""
            )),
            Ok(Outcome::Page(page)) => match render(&page) {
                Err(error) => failure(&error),
                Ok(text) => {
                    self.cache.put(&key, text.clone(), now);
                    ToolResult::ok(text)
                },
            },
        }
    }
}

/// A fetched page as the text the model reads.
fn render(page: &Page) -> Result<String, WebError> {
    let kind = page.content_type.as_deref().map(|t| {
        t.split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
    });
    let text = decode(&page.body, page.content_type.as_deref());
    let is_html = match kind.as_deref() {
        Some(k) => k == "text/html" || k == "application/xhtml+xml",
        // No type given: sniff, and refuse what is plainly binary.
        None => {
            if page.body.iter().take(8192).any(|b| *b == 0) {
                return Err(WebError::UnsupportedContentType("unknown (binary)".into()));
            }
            let head = text
                .chars()
                .take(512)
                .collect::<String>()
                .to_ascii_lowercase();
            head.contains("<html") || head.contains("<!doctype html")
        },
    };
    let mut out = if is_html {
        html_to_markdown(&text, &page.url)
    } else {
        text
    };
    out = truncate(&out, MAX_TEXT_CHARS);
    if page.truncated {
        out.push_str("\n[the page was larger than the download cap and was cut]");
    }
    if out.trim().is_empty() {
        out = "(the page has no text content)".to_owned();
    }
    Ok(out)
}
