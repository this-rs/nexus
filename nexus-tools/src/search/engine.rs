//! Several engines tried in order, each behind its own breaker and rate limit (N22).

use std::sync::Arc;

use super::backend::{SearchBackend, SearchError, SearchHit, SearchQuery};
use super::resilience::{Breaker, Limiter};
use crate::web::Clock;

/// How an engine is protected.
#[derive(Debug, Clone, Copy)]
pub struct Protection {
    /// Consecutive failures that open the circuit.
    pub breaker_threshold: u32,
    /// How long it stays open.
    pub breaker_cooldown_ms: u64,
    /// At most this many calls per window, if set: `(calls, window_ms)`.
    pub rate: Option<(usize, u64)>,
}

impl Default for Protection {
    fn default() -> Self {
        Self {
            breaker_threshold: 3,
            breaker_cooldown_ms: 60_000,
            rate: None,
        }
    }
}

struct Slot {
    backend: Box<dyn SearchBackend>,
    breaker: Breaker,
    limiter: Option<Limiter>,
}

/// What an answered query looked like.
#[derive(Debug)]
pub struct Answer {
    /// The engine that answered.
    pub backend: String,
    /// Its results.
    pub hits: Vec<SearchHit>,
    /// Engines tried before it, and why they did not answer.
    pub skipped: Vec<(String, SearchError)>,
}

/// Every engine failed (or there are none).
#[derive(Debug)]
pub struct EngineError {
    /// Each engine and its failure, in the order they were tried.
    pub failures: Vec<(String, SearchError)>,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.failures.is_empty() {
            return f.write_str("no search engine is configured");
        }
        f.write_str("no search engine could answer:")?;
        for (id, error) in &self.failures {
            write!(f, "\n- {id}: {} ({error})", error.kind())?;
        }
        Ok(())
    }
}

/// The ordered engines.
pub struct Engine {
    slots: Vec<Slot>,
    clock: Arc<dyn Clock>,
}

impl Engine {
    /// An engine set with no engines yet.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            slots: Vec::new(),
            clock,
        }
    }

    /// Adds an engine after the ones already there.
    #[must_use]
    pub fn with_backend(
        mut self,
        backend: impl SearchBackend + 'static,
        protection: Protection,
    ) -> Self {
        self.slots.push(Slot {
            backend: Box::new(backend),
            breaker: Breaker::new(protection.breaker_threshold, protection.breaker_cooldown_ms),
            limiter: protection
                .rate
                .map(|(calls, window)| Limiter::new(calls, window)),
        });
        self
    }

    /// The clock the engines are timed by.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Whether any engine is configured.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Asks the engines in order until one answers.
    pub async fn search(&self, query: &SearchQuery) -> Result<Answer, EngineError> {
        let mut skipped = Vec::new();
        for slot in &self.slots {
            let id = slot.backend.id().to_owned();
            let now = self.clock.now_ms();
            if !slot.breaker.allow(now) {
                skipped.push((id, SearchError::CircuitOpen));
                continue;
            }
            if let Some(limiter) = &slot.limiter
                && limiter.try_acquire(now).is_err()
            {
                slot.breaker.release();
                skipped.push((id, SearchError::RateLimited));
                continue;
            }
            match slot.backend.search(query).await {
                Ok(hits) => {
                    slot.breaker.success();
                    return Ok(Answer {
                        backend: id,
                        hits,
                        skipped,
                    });
                },
                Err(error) => {
                    if error.trips_circuit() {
                        slot.breaker.failure(self.clock.now_ms());
                    } else {
                        slot.breaker.release();
                    }
                    skipped.push((id, error));
                },
            }
        }
        Err(EngineError { failures: skipped })
    }
}
