//! `WebSearch` (N22): a search **trait** with several engines, because Claude Code's search
//! rides on its publisher's service and there is none to ride on here.
//!
//! What the tool guarantees, whatever engine answers: domain filters are applied *after* the
//! engine (a blocked domain never appears even if the engine returns it), equivalent URLs are
//! merged, engines are tried in order with a circuit breaker and a rate limit each, keys are
//! never printed, results are labelled untrusted data (a page's own title is not an
//! instruction), and no model is called behind the model's back.

mod backend;
mod canon;
mod engine;
mod resilience;
mod tool;

pub use backend::{SearchBackend, SearchError, SearchHit, SearchQuery};
pub use canon::{canonical, permitted};
pub use engine::{Answer, Engine, EngineError, Protection};
pub use resilience::{Breaker, Limiter};
pub use tool::{MAX_EXTENDED_QUERIES, MAX_RESULTS, WebSearchTool};
