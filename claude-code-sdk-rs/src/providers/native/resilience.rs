//! What a turn does when the model's endpoint fails **for a reason that passes**.
//!
//! A turn is long and expensive: a dropped connection, a `503`, a rate limit or a vault
//! that was locked for a few minutes must not throw it away. Before this module, any
//! error from the endpoint ended the turn with `error_during_execution` (a response body
//! that failed to decode mid-stream, `endpoint_unreachable`, killed a delegated agent
//! that had been working for two minutes).
//!
//! | Error | Recovery |
//! |---|---|
//! | `CredentialsLocked` | **wait for the unlock**: the credential is resolved for each request, so the next request after the unlock succeeds. Polled with a bounded interval, capped by `unlock_wait_max` for the whole turn (all its round trips together, default 30 min, `retry_wait_unlock_max_secs`): past it the turn ends with `credentials_locked`, naming the time waited; the user's interrupt, the turn time limit and the session closing end the wait sooner |
//! | `EndpointUnreachable`, `Timeout`, `Overloaded` | exponential backoff, `max_attempts` times |
//! | `RateLimited` | the delay the endpoint asked for (capped), else the same backoff |
//! | anything else | not retried: the same request would fail the same way |
//!
//! Retrying is safe because a failed round trip commits nothing: `model_step` works on a
//! copy of the history and the transcript is saved only when the turn ends well.

use std::time::Duration;

use crate::agent::ProviderError;

/// How the native harness recovers from an endpoint failure that can pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first failure of one round trip (0 turns the backoff off).
    pub max_attempts: u32,
    /// First backoff delay; it doubles at each attempt.
    pub base_delay: Duration,
    /// Longest backoff delay, and the longest wait a rate-limit answer can impose.
    pub max_delay: Duration,
    /// Wait for a locked credential vault to be unlocked instead of failing the turn.
    pub wait_for_unlock: bool,
    /// Interval between two looks at a locked vault (never below [`MIN_UNLOCK_POLL`]).
    pub unlock_poll: Duration,
    /// Longest total wait for the unlock in one TURN (every round trip of the turn counts, measured
    /// on the clock). Long enough for a human round trip
    /// to the vault page (type the code, send again), short enough that a forgotten session
    /// does not spin for hours; past it the turn ends with `credentials_locked`.
    pub unlock_wait_max: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            wait_for_unlock: true,
            unlock_poll: Duration::from_secs(5),
            unlock_wait_max: Duration::from_secs(30 * 60),
        }
    }
}

impl RetryPolicy {
    /// No recovery: the first failure ends the turn (the behaviour before this module).
    pub const fn none() -> Self {
        Self {
            max_attempts: 0,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            wait_for_unlock: false,
            unlock_poll: Duration::ZERO,
            unlock_wait_max: Duration::ZERO,
        }
    }
}

/// Floor of the interval between two looks at a locked vault: a zero `unlock_poll` would spin
/// without ever letting the clock reach the cap.
pub(crate) const MIN_UNLOCK_POLL: Duration = Duration::from_millis(250);

/// What to do about one failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recovery {
    /// Ask again after the delay.
    Retry(Duration),
    /// The vault is locked: look again after the delay (until the turn's `unlock_wait_max` is spent).
    AwaitUnlock(Duration),
    /// Not recoverable here (or out of attempts): the turn ends with this error.
    GiveUp,
}

/// Decides the recovery for `error`, which is the failure number `attempt` (0 for the first
/// one) of this round trip. Pure, so the table above is tested without a clock.
pub(crate) fn recovery(
    policy: &RetryPolicy,
    error: &ProviderError,
    attempt: u32,
    unlock_waited: Duration, // already waited for the unlock by this TURN
) -> Recovery {
    match error {
        // `unlock_waited` counts only the wait for the unlock: a plain `attempt` cap would
        // end the wait on a poll count the user cannot see, a time cap ends it on time.
        ProviderError::CredentialsLocked if unlock_waited >= policy.unlock_wait_max => {
            Recovery::GiveUp
        },
        ProviderError::CredentialsLocked if policy.wait_for_unlock => {
            Recovery::AwaitUnlock(policy.unlock_poll.max(MIN_UNLOCK_POLL))
        },
        _ if attempt >= policy.max_attempts => Recovery::GiveUp,
        ProviderError::EndpointUnreachable { .. }
        | ProviderError::Timeout { .. }
        | ProviderError::Overloaded => Recovery::Retry(backoff(policy, attempt)),
        ProviderError::RateLimited { retry_after_ms, .. } => Recovery::Retry(
            retry_after_ms
                .map(|ms| Duration::from_millis(ms).min(policy.max_delay))
                .unwrap_or_else(|| backoff(policy, attempt)),
        ),
        _ => Recovery::GiveUp,
    }
}

fn backoff(policy: &RetryPolicy, attempt: u32) -> Duration {
    policy
        .base_delay
        .saturating_mul(1u32.checked_shl(attempt).unwrap_or(u32::MAX))
        .min(policy.max_delay)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unreachable() -> ProviderError {
        ProviderError::EndpointUnreachable {
            detail: "error decoding response body".into(),
        }
    }

    #[test]
    fn a_dropped_stream_is_retried_with_a_doubling_delay_then_given_up() {
        let policy = RetryPolicy::default();
        let delays: Vec<_> = (0..6)
            .map(|n| recovery(&policy, &unreachable(), n, Duration::ZERO))
            .collect();
        assert_eq!(
            delays,
            [
                Recovery::Retry(Duration::from_secs(1)),
                Recovery::Retry(Duration::from_secs(2)),
                Recovery::Retry(Duration::from_secs(4)),
                Recovery::Retry(Duration::from_secs(8)),
                Recovery::Retry(Duration::from_secs(16)),
                Recovery::GiveUp,
            ]
        );
    }

    #[test]
    fn the_backoff_never_exceeds_its_cap() {
        let policy = RetryPolicy {
            max_attempts: 100,
            ..RetryPolicy::default()
        };
        assert_eq!(
            recovery(&policy, &ProviderError::Overloaded, 90, Duration::ZERO),
            Recovery::Retry(policy.max_delay)
        );
    }

    #[test]
    fn a_locked_vault_is_waited_for_without_an_attempt_cap_but_not_past_the_time_cap() {
        let policy = RetryPolicy::default();
        for attempt in [0, 5, 10_000] {
            assert_eq!(
                recovery(
                    &policy,
                    &ProviderError::CredentialsLocked,
                    attempt,
                    Duration::ZERO
                ),
                Recovery::AwaitUnlock(policy.unlock_poll)
            );
        }
        let almost = policy.unlock_wait_max - policy.unlock_poll;
        assert_eq!(
            recovery(&policy, &ProviderError::CredentialsLocked, 0, almost),
            Recovery::AwaitUnlock(policy.unlock_poll)
        );
        for waited in [policy.unlock_wait_max, policy.unlock_wait_max * 2] {
            assert_eq!(
                recovery(&policy, &ProviderError::CredentialsLocked, 0, waited),
                Recovery::GiveUp
            );
        }
    }

    #[test]
    fn a_zero_poll_interval_is_raised_to_the_floor() {
        let policy = RetryPolicy {
            unlock_poll: Duration::ZERO,
            ..RetryPolicy::default()
        };
        assert_eq!(
            recovery(
                &policy,
                &ProviderError::CredentialsLocked,
                0,
                Duration::ZERO
            ),
            Recovery::AwaitUnlock(MIN_UNLOCK_POLL)
        );
    }

    #[test]
    fn the_default_unlock_wait_cap_is_a_human_round_trip_not_hours() {
        let cap = RetryPolicy::default().unlock_wait_max;
        assert!(cap >= Duration::from_secs(10 * 60) && cap <= Duration::from_secs(60 * 60));
    }

    #[test]
    fn a_rate_limit_waits_what_the_endpoint_asked_up_to_the_cap() {
        let policy = RetryPolicy::default();
        let limited = |ms| ProviderError::RateLimited {
            retry_after_ms: ms,
            resets_at_ms: None,
        };
        assert_eq!(
            recovery(&policy, &limited(Some(7_000)), 0, Duration::ZERO),
            Recovery::Retry(Duration::from_secs(7))
        );
        assert_eq!(
            recovery(&policy, &limited(Some(3_600_000)), 0, Duration::ZERO),
            Recovery::Retry(policy.max_delay)
        );
        assert_eq!(
            recovery(&policy, &limited(None), 1, Duration::ZERO),
            Recovery::Retry(Duration::from_secs(2))
        );
    }

    #[test]
    fn what_a_retry_cannot_fix_is_not_retried() {
        let policy = RetryPolicy::default();
        for error in [
            ProviderError::Unauthorized,
            ProviderError::invalid("bad request"),
            ProviderError::protocol("garbage"),
            ProviderError::Closed,
        ] {
            assert_eq!(
                recovery(&policy, &error, 0, Duration::ZERO),
                Recovery::GiveUp,
                "{error:?}"
            );
        }
    }

    #[test]
    fn the_none_policy_recovers_from_nothing() {
        let policy = RetryPolicy::none();
        assert_eq!(
            recovery(&policy, &unreachable(), 0, Duration::ZERO),
            Recovery::GiveUp
        );
        assert_eq!(
            recovery(
                &policy,
                &ProviderError::CredentialsLocked,
                0,
                Duration::ZERO
            ),
            Recovery::GiveUp
        );
    }
}
