#![allow(dead_code)]

use anyhow::Result;
use std::future::Future;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    pub exponential_base: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay_ms: 1000,
            max_delay_ms: 30000,
            exponential_base: 2.0,
        }
    }
}

pub struct RetryPolicy {
    config: RetryConfig,
}

impl RetryPolicy {
    pub fn new(config: RetryConfig) -> Self {
        Self { config }
    }

    pub async fn execute<F, Fut, T, E>(
        &self,
        operation_name: &str,
        mut operation: F,
    ) -> Result<T, E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: std::fmt::Display,
    {
        let mut attempt = 0;
        let mut delay_ms = self.config.initial_delay_ms;

        loop {
            attempt += 1;

            match operation().await {
                Ok(result) => {
                    if attempt > 1 {
                        info!("{} succeeded after {} attempts", operation_name, attempt);
                    }
                    return Ok(result);
                },
                Err(err) => {
                    if attempt >= self.config.max_retries {
                        error!(
                            "{} failed after {} attempts: {}",
                            operation_name, attempt, err
                        );
                        return Err(err);
                    }

                    warn!(
                        "{} failed (attempt {}/{}): {}. Retrying in {}ms...",
                        operation_name, attempt, self.config.max_retries, err, delay_ms
                    );

                    sleep(Duration::from_millis(delay_ms)).await;

                    // Calculate next delay with exponential backoff
                    delay_ms = ((delay_ms as f64) * self.config.exponential_base) as u64;
                    delay_ms = delay_ms.min(self.config.max_delay_ms);
                },
            }
        }
    }

    pub fn should_retry<E: std::fmt::Display>(error: &E) -> bool {
        let error_str = error.to_string().to_lowercase();

        // Retry on these types of errors
        if error_str.contains("timeout")
            || error_str.contains("connection")
            || error_str.contains("temporarily unavailable")
            || error_str.contains("too many requests")
            || error_str.contains("overloaded")
        {
            return true;
        }

        // Don't retry on these
        if error_str.contains("invalid")
            || error_str.contains("unauthorized")
            || error_str.contains("forbidden")
            || error_str.contains("not found")
        {
            return false;
        }

        // Default to retry for unknown errors
        true
    }
}

#[derive(Clone)]
pub struct CircuitBreaker {
    failure_threshold: u32,
    recovery_timeout: Duration,
    failures: std::sync::Arc<std::sync::atomic::AtomicU32>,
    last_failure: std::sync::Arc<parking_lot::Mutex<Option<std::time::Instant>>>,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, recovery_timeout_secs: u64) -> Self {
        Self {
            failure_threshold,
            recovery_timeout: Duration::from_secs(recovery_timeout_secs),
            failures: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
            last_failure: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    pub fn is_open(&self) -> bool {
        let failures = self.failures.load(std::sync::atomic::Ordering::Relaxed);
        if failures < self.failure_threshold {
            return false;
        }

        // Check if we should reset
        if let Some(last_failure) = *self.last_failure.lock()
            && last_failure.elapsed() > self.recovery_timeout
        {
            self.reset();
            return false;
        }

        true
    }

    pub fn record_success(&self) {
        self.reset();
    }

    pub fn record_failure(&self) {
        self.failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *self.last_failure.lock() = Some(std::time::Instant::now());
    }

    fn reset(&self) {
        self.failures.store(0, std::sync::atomic::Ordering::Relaxed);
        *self.last_failure.lock() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ═══════════════════════════════════════════════════════════════
    //  RetryConfig defaults
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn test_retry_config_defaults() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.initial_delay_ms, 1000);
        assert_eq!(config.max_delay_ms, 30000);
        assert!((config.exponential_base - 2.0).abs() < f64::EPSILON);
    }

    // ═══════════════════════════════════════════════════════════════
    //  RetryPolicy::should_retry
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn test_should_retry_timeout() {
        assert!(RetryPolicy::should_retry(&"Connection timeout"));
    }

    #[test]
    fn test_should_retry_connection_error() {
        assert!(RetryPolicy::should_retry(&"connection refused"));
    }

    #[test]
    fn test_should_retry_temporarily_unavailable() {
        assert!(RetryPolicy::should_retry(
            &"Service temporarily unavailable"
        ));
    }

    #[test]
    fn test_should_retry_too_many_requests() {
        assert!(RetryPolicy::should_retry(&"too many requests"));
    }

    #[test]
    fn test_should_retry_overloaded() {
        assert!(RetryPolicy::should_retry(&"Server overloaded"));
    }

    #[test]
    fn test_should_not_retry_invalid() {
        assert!(!RetryPolicy::should_retry(&"invalid request body"));
    }

    #[test]
    fn test_should_not_retry_unauthorized() {
        assert!(!RetryPolicy::should_retry(&"unauthorized access"));
    }

    #[test]
    fn test_should_not_retry_forbidden() {
        assert!(!RetryPolicy::should_retry(&"forbidden resource"));
    }

    #[test]
    fn test_should_not_retry_not_found() {
        assert!(!RetryPolicy::should_retry(&"resource not found"));
    }

    #[test]
    fn test_should_retry_unknown_error() {
        // Unknown errors default to retryable
        assert!(RetryPolicy::should_retry(&"some obscure error"));
    }

    #[test]
    fn test_should_retry_case_insensitive() {
        assert!(RetryPolicy::should_retry(&"TIMEOUT occurred"));
        assert!(!RetryPolicy::should_retry(&"INVALID input"));
    }

    // ═══════════════════════════════════════════════════════════════
    //  CircuitBreaker
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn test_circuit_breaker_starts_closed() {
        let cb = CircuitBreaker::new(3, 60);
        assert!(!cb.is_open(), "Brand new circuit breaker should be closed");
    }

    #[test]
    fn test_circuit_breaker_opens_after_threshold() {
        let cb = CircuitBreaker::new(3, 60);
        cb.record_failure();
        cb.record_failure();
        assert!(!cb.is_open(), "Should still be closed below threshold");

        cb.record_failure();
        assert!(cb.is_open(), "Should be open at threshold");
    }

    #[test]
    fn test_circuit_breaker_success_resets() {
        let cb = CircuitBreaker::new(3, 60);
        cb.record_failure();
        cb.record_failure();
        cb.record_success();
        assert!(
            !cb.is_open(),
            "Should be closed after success resets failures"
        );

        // Need threshold failures again to open
        cb.record_failure();
        cb.record_failure();
        assert!(!cb.is_open());
        cb.record_failure();
        assert!(cb.is_open());
    }

    #[test]
    fn test_circuit_breaker_stays_open_before_recovery() {
        // Use a long recovery timeout — circuit should stay open
        let cb = CircuitBreaker::new(2, 3600); // 1 hour
        cb.record_failure();
        cb.record_failure();
        assert!(cb.is_open(), "Should be open after threshold failures");
        // Still open because recovery timeout hasn't elapsed
        assert!(cb.is_open(), "Should stay open before recovery timeout");
    }

    #[test]
    fn test_circuit_breaker_clone() {
        let cb = CircuitBreaker::new(2, 60);
        cb.record_failure();

        let cb2 = cb.clone();
        cb2.record_failure();

        // Both share the same Arc, so the second failure should open the circuit
        assert!(cb.is_open());
        assert!(cb2.is_open());
    }
}
