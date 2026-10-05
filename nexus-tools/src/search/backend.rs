//! What a search engine is, to the tool (N22).

use async_trait::async_trait;

/// One query to one engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    /// The words to search for.
    pub text: String,
    /// How many results to ask for.
    pub limit: usize,
}

/// One result. Untrusted data: a title or snippet is what a web page says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// The page title.
    pub title: String,
    /// The page address.
    pub url: String,
    /// A short extract, when the engine gives one.
    pub snippet: Option<String>,
}

/// Why an engine did not answer. No variant ever carries a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchError {
    /// The engine could not be reached or answered with a server error.
    Unavailable(String),
    /// It did not answer in time.
    Timeout,
    /// It refused the key it was given.
    KeyRejected,
    /// The key's quota is used up.
    QuotaExceeded,
    /// We are asking too often (our own limit, or the engine's).
    RateLimited,
    /// It answered, but not in a shape we understand.
    BadResponse(String),
    /// Its circuit is open after repeated failures: it was not asked.
    CircuitOpen,
    /// No key is configured or the key reference cannot be resolved.
    NotConfigured(String),
}

impl SearchError {
    /// A short stable name.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "unavailable",
            Self::Timeout => "timeout",
            Self::KeyRejected => "key_rejected",
            Self::QuotaExceeded => "quota_exceeded",
            Self::RateLimited => "rate_limited",
            Self::BadResponse(_) => "bad_response",
            Self::CircuitOpen => "circuit_open",
            Self::NotConfigured(_) => "not_configured",
        }
    }

    /// Whether this failure should count against the engine's circuit: a quota or our own rate
    /// limit does not mean the engine is broken.
    pub fn trips_circuit(&self) -> bool {
        matches!(
            self,
            Self::Unavailable(_) | Self::Timeout | Self::BadResponse(_)
        )
    }
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(why) => write!(f, "unavailable: {why}"),
            Self::Timeout => f.write_str("timed out"),
            Self::KeyRejected => f.write_str("the key was rejected"),
            Self::QuotaExceeded => f.write_str("the quota is used up"),
            Self::RateLimited => f.write_str("rate limited"),
            Self::BadResponse(why) => write!(f, "unreadable answer: {why}"),
            Self::CircuitOpen => f.write_str("skipped: too many recent failures"),
            Self::NotConfigured(why) => write!(f, "not configured: {why}"),
        }
    }
}

/// A search engine.
#[async_trait]
pub trait SearchBackend: Send + Sync {
    /// A short name for logs and error messages (`brave`, `searxng`…).
    fn id(&self) -> &str;

    /// Runs one query. At most `query.limit` hits; fewer is fine; none is a valid answer.
    async fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>, SearchError>;
}
