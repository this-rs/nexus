//! `RetryPolicy::execute` under a paused clock.
//!
//! `execute` is the only part of `core::retry` that waits, and it waits through
//! `tokio::time::sleep`. That is the seam these tests use: with
//! `#[tokio::test(start_paused = true)]` the runtime's clock is virtual and
//! jumps straight to the next timer deadline whenever the runtime runs out of
//! work, so a 5-second backoff is asserted on in microseconds of wall time.
//! Nothing has to be injected into `RetryPolicy` for that, and no test in this
//! file sleeps for real.
//!
//! The tests live in `tests/` rather than in an inline `#[cfg(test)] mod tests`
//! on purpose. `scripts/coverage_logic_only.py` recognises test code from the
//! v0-mangled symbol names, and a closure defined inside a module named `tests`
//! can leak that path segment into the mangled name of the `execute::<..>`
//! monomorphisation — which would drop the whole function out of the logic
//! denominator instead of adding it to the covered set.

use claude_code_api::core::retry::{RetryConfig, RetryPolicy};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

fn cfg(max_retries: u32, initial_delay_ms: u64, max_delay_ms: u64, base: f64) -> RetryConfig {
    RetryConfig {
        max_retries,
        initial_delay_ms,
        max_delay_ms,
        exponential_base: base,
    }
}

/// Virtual milliseconds elapsed between consecutive attempts.
fn gaps_ms(marks: &[Duration]) -> Vec<u64> {
    marks
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).as_millis() as u64)
        .collect()
}

/// Drives `execute` with an operation that fails `failures` times and then
/// succeeds, recording the virtual instant at which each attempt ran.
///
/// Pass `u32::MAX` for an operation that never succeeds.
async fn attempts(
    config: RetryConfig,
    failures: u32,
) -> (Result<&'static str, String>, Vec<Duration>) {
    let marks = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::new(AtomicU32::new(0));
    let start = Instant::now();
    let policy = RetryPolicy::new(config);

    let result = policy
        .execute("probe", || {
            let marks = Arc::clone(&marks);
            let seen = Arc::clone(&seen);
            async move {
                let attempt = seen.fetch_add(1, Ordering::Relaxed) + 1;
                marks.lock().unwrap().push(start.elapsed());
                if attempt <= failures {
                    Err(format!("timeout on attempt {attempt}"))
                } else {
                    Ok("ok")
                }
            }
        })
        .await;

    let marks = marks.lock().unwrap().clone();
    (result, marks)
}

// ═══════════════════════════════════════════════════════════════
//  How many attempts actually run
// ═══════════════════════════════════════════════════════════════

#[tokio::test(start_paused = true)]
async fn succeeds_on_the_first_attempt_without_waiting() {
    let start = Instant::now();
    let (result, marks) = attempts(RetryConfig::default(), 0).await;

    assert_eq!(result.unwrap(), "ok");
    assert_eq!(marks.len(), 1, "a success must not be retried");
    assert_eq!(
        start.elapsed(),
        Duration::ZERO,
        "a first-attempt success must not wait at all"
    );
}

#[tokio::test(start_paused = true)]
async fn succeeds_after_two_failures_and_waits_in_between() {
    let (result, marks) = attempts(cfg(5, 100, 10_000, 2.0), 2).await;

    assert_eq!(result.unwrap(), "ok");
    assert_eq!(marks.len(), 3, "two failures then a success");
    assert_eq!(gaps_ms(&marks), vec![100, 200]);
}

/// `max_retries` bounds the number of *attempts*, not the number of retries:
/// the loop gives up as soon as `attempt >= max_retries`. The field name is a
/// misnomer, the code is self-consistent with it (the `error!` line it prints
/// says "after N attempts"), so this pins the behaviour rather than changing it
/// — `execute` has no caller in the workspace to arbitrate the intent.
#[tokio::test(start_paused = true)]
async fn max_retries_bounds_attempts_rather_than_retries() {
    let (result, marks) = attempts(cfg(3, 100, 10_000, 2.0), u32::MAX).await;

    assert!(result.is_err());
    assert_eq!(
        marks.len(),
        3,
        "`max_retries: 3` runs 3 attempts, i.e. only 2 retries"
    );
}

/// Same misnomer at its degenerate end: anything below 2 disables retrying,
/// because the very first attempt already satisfies `attempt >= max_retries`.
#[tokio::test(start_paused = true)]
async fn max_retries_below_two_disables_retrying() {
    for max_retries in [0, 1] {
        let (result, marks) = attempts(cfg(max_retries, 100, 10_000, 2.0), u32::MAX).await;

        assert_eq!(result.unwrap_err(), "timeout on attempt 1");
        assert_eq!(
            marks.len(),
            1,
            "max_retries = {max_retries} must leave a single attempt"
        );
    }
}

/// The classic silent bug in a retry loop is to keep the *first* error and
/// return it after exhaustion, hiding what actually went wrong last.
#[tokio::test(start_paused = true)]
async fn returns_the_error_of_the_last_attempt_not_the_first() {
    let (result, marks) = attempts(cfg(3, 100, 10_000, 2.0), u32::MAX).await;

    assert_eq!(marks.len(), 3);
    assert_eq!(
        result.unwrap_err(),
        "timeout on attempt 3",
        "exhaustion must surface the last failure, not the first"
    );
}

// ═══════════════════════════════════════════════════════════════
//  How the delay progresses
// ═══════════════════════════════════════════════════════════════

/// Exact equality, attempt after attempt, is also the proof that no jitter is
/// applied: the backoff is fully deterministic.
#[tokio::test(start_paused = true)]
async fn delays_grow_exponentially_and_carry_no_jitter() {
    let (_, marks) = attempts(cfg(5, 100, 10_000, 2.0), u32::MAX).await;

    assert_eq!(gaps_ms(&marks), vec![100, 200, 400, 800]);
}

#[tokio::test(start_paused = true)]
async fn an_exponential_base_of_one_keeps_the_delay_constant() {
    let (_, marks) = attempts(cfg(4, 100, 10_000, 1.0), u32::MAX).await;

    assert_eq!(gaps_ms(&marks), vec![100, 100, 100]);
}

#[tokio::test(start_paused = true)]
async fn the_growing_delay_is_clamped_to_max_delay_ms() {
    let (_, marks) = attempts(cfg(6, 100, 250, 2.0), u32::MAX).await;

    assert_eq!(gaps_ms(&marks), vec![100, 200, 250, 250, 250]);
}

/// Regression: the ceiling used to be applied only from the *second* delay on,
/// because `delay_ms` started at `initial_delay_ms` unclamped and was only
/// passed through `min(max_delay_ms)` after the first `sleep`. A config whose
/// initial delay exceeded its own ceiling therefore waited 5 s before the first
/// retry of a policy that promised at most 100 ms.
#[tokio::test(start_paused = true)]
async fn the_first_delay_is_clamped_to_max_delay_ms_too() {
    let (_, marks) = attempts(cfg(3, 5_000, 100, 2.0), u32::MAX).await;

    assert_eq!(
        gaps_ms(&marks),
        vec![100, 100],
        "max_delay_ms must cap the first wait as well"
    );
}

// ═══════════════════════════════════════════════════════════════
//  Retryable vs definitive
// ═══════════════════════════════════════════════════════════════

/// `should_retry` is a public helper that `execute` never consults: a
/// definitive error is retried exactly like a transient one. Pinned rather than
/// fixed — wiring the predicate into the loop would change what `execute` does
/// for every error it sees, which is the owner's call, not a test's.
#[tokio::test(start_paused = true)]
async fn execute_retries_errors_that_should_retry_rejects() {
    assert!(
        !RetryPolicy::should_retry(&"unauthorized"),
        "precondition: the helper classifies this as definitive"
    );

    let seen = Arc::new(AtomicU32::new(0));
    let policy = RetryPolicy::new(cfg(3, 100, 10_000, 2.0));
    let result: Result<(), &str> = policy
        .execute("definitive", || {
            let seen = Arc::clone(&seen);
            async move {
                seen.fetch_add(1, Ordering::Relaxed);
                Err("unauthorized")
            }
        })
        .await;

    assert_eq!(result.unwrap_err(), "unauthorized");
    assert_eq!(
        seen.load(Ordering::Relaxed),
        3,
        "execute retries a definitive error up to its attempt budget"
    );
}
