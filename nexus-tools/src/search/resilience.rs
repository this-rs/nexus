//! Circuit breaker and rate limiter, on an injected clock (N22).

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

/// Opens after `threshold` consecutive failures and stays open for `cooldown_ms`; then it lets
/// **one** probe through (half-open): success closes it, failure opens it again.
#[derive(Debug)]
pub struct Breaker {
    threshold: u32,
    cooldown_ms: u64,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    failures: u32,
    opened_at: Option<u64>,
    probing: bool,
}

impl Breaker {
    /// A closed breaker.
    pub fn new(threshold: u32, cooldown_ms: u64) -> Self {
        Self {
            threshold: threshold.max(1),
            cooldown_ms,
            state: Mutex::new(State::default()),
        }
    }

    /// Whether a call may go through now. When it returns `true` for a half-open breaker, the
    /// caller owes a `success` or `failure`.
    pub fn allow(&self, now_ms: u64) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match state.opened_at {
            None => true,
            Some(at) if now_ms.saturating_sub(at) >= self.cooldown_ms && !state.probing => {
                state.probing = true;
                true
            },
            Some(_) => false,
        }
    }

    /// The call worked: close.
    pub fn success(&self) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = State::default();
    }

    /// The call failed.
    pub fn failure(&self, now_ms: u64) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.probing = false;
        state.failures += 1;
        if state.failures >= self.threshold || state.opened_at.is_some() {
            state.opened_at = Some(now_ms);
        }
    }

    /// A call that was allowed ended in a way that says nothing about the engine's health (a
    /// rejected key, a quota): free the half-open probe without closing or re-opening.
    pub fn release(&self) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .probing = false;
    }

    /// Whether it is open right now.
    pub fn is_open(&self, now_ms: u64) -> bool {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .opened_at
            .is_some_and(|at| now_ms.saturating_sub(at) < self.cooldown_ms || state.probing)
    }
}

/// At most `max` calls in any window of `window_ms`.
#[derive(Debug)]
pub struct Limiter {
    max: usize,
    window_ms: u64,
    calls: Mutex<VecDeque<u64>>,
}

impl Limiter {
    /// A limiter.
    pub fn new(max: usize, window_ms: u64) -> Self {
        Self {
            max,
            window_ms,
            calls: Mutex::new(VecDeque::new()),
        }
    }

    /// Takes a slot, or says how long until one frees up.
    pub fn try_acquire(&self, now_ms: u64) -> Result<(), u64> {
        let mut calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
        while calls
            .front()
            .is_some_and(|t| now_ms.saturating_sub(*t) >= self.window_ms)
        {
            calls.pop_front();
        }
        if calls.len() >= self.max {
            let oldest = calls.front().copied().unwrap_or(now_ms);
            return Err(self.window_ms.saturating_sub(now_ms.saturating_sub(oldest)));
        }
        calls.push_back(now_ms);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_breaker_opens_after_n_failures_and_closes_after_a_good_probe() {
        let breaker = Breaker::new(3, 10_000);
        for t in [0, 1, 2] {
            assert!(breaker.allow(t));
            breaker.failure(t);
        }
        assert!(breaker.is_open(3));
        assert!(!breaker.allow(3), "open: not asked");
        assert!(!breaker.allow(9_999));
        // After the cooldown, one probe goes through, only one.
        assert!(breaker.allow(10_002));
        assert!(!breaker.allow(10_003), "a probe is already in flight");
        breaker.success();
        assert!(!breaker.is_open(10_004));
        assert!(breaker.allow(10_005));
    }

    #[test]
    fn a_failed_probe_opens_it_again_for_a_full_cooldown() {
        let breaker = Breaker::new(1, 1_000);
        breaker.failure(0);
        assert!(!breaker.allow(500));
        assert!(breaker.allow(1_000));
        breaker.failure(1_000);
        assert!(!breaker.allow(1_999));
        assert!(breaker.allow(2_000));
    }

    #[test]
    fn a_success_resets_the_count_so_failures_must_be_consecutive() {
        let breaker = Breaker::new(3, 1_000);
        breaker.failure(0);
        breaker.failure(1);
        breaker.success();
        breaker.failure(2);
        breaker.failure(3);
        assert!(breaker.allow(4), "two since the success: still closed");
        breaker.failure(4);
        assert!(!breaker.allow(5));
    }

    #[test]
    fn the_limiter_allows_n_per_window_and_says_when_to_retry() {
        let limiter = Limiter::new(2, 1_000);
        assert_eq!(limiter.try_acquire(0), Ok(()));
        assert_eq!(limiter.try_acquire(100), Ok(()));
        assert_eq!(limiter.try_acquire(200), Err(800));
        assert_eq!(limiter.try_acquire(999), Err(1));
        assert_eq!(
            limiter.try_acquire(1_000),
            Ok(()),
            "the first call left the window"
        );
        assert_eq!(
            limiter.try_acquire(1_050),
            Err(50),
            "the second leaves at 1100"
        );
    }
}
