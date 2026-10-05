//! `WebSearch` (N22): a search **trait** with several engines, because Claude Code's search
//! rides on its publisher's service and there is none to ride on here.
//!
//! What the tool guarantees, whatever engine answers: domain filters are applied *after* the
//! engine (a blocked domain never appears even if the engine returns it), equivalent URLs are
//! merged, engines are tried in order with a circuit breaker and a rate limit each, keys are
//! never printed, results are labelled untrusted data (a page's own title is not an
//! instruction), and no model is called behind the model's back.

mod backend;
mod brave;
mod canon;
mod engine;
mod html;
mod resilience;
mod searxng;
mod secret;
mod tool;
mod util;

pub use backend::{SearchBackend, SearchError, SearchHit, SearchQuery};
pub use brave::{BRAVE_ENDPOINT, KeyedApiBackend};
pub use canon::{canonical, permitted};
pub use engine::{Answer, Engine, EngineError, Protection};
pub use html::{DDG_HTML_ENDPOINT, HtmlBackend};
pub use resilience::{Breaker, Limiter};
pub use searxng::SearxngBackend;
pub use secret::{KeySource, Secret};
pub use tool::{MAX_EXTENDED_QUERIES, MAX_RESULTS, WebSearchTool};

/// Adds `WebSearch` to a registry — only when at least one engine is configured: a tool that can
/// only ever say "no engine" is worse than no tool.
pub fn register(
    registry: crate::registry::ToolRegistry,
    engine: Engine,
) -> crate::registry::ToolRegistry {
    if engine.is_empty() {
        registry
    } else {
        registry.with(WebSearchTool::new(engine))
    }
}
