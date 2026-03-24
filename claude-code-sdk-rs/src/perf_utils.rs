//! Performance utilities for the Claude Code SDK

use crate::{errors::Result, types::Message};
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use tracing::{debug, warn};

/// Configuration for retry logic
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts
    pub max_retries: u32,
    /// Initial delay between retries
    pub initial_delay: Duration,
    /// Maximum delay between retries
    pub max_delay: Duration,
    /// Multiplier for exponential backoff
    pub backoff_multiplier: f64,
    /// Jitter factor (0.0 to 1.0) to add randomness to delays
    pub jitter_factor: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(30),
            backoff_multiplier: 2.0,
            jitter_factor: 0.1,
        }
    }
}

impl RetryConfig {
    /// Execute a future with retry logic
    pub async fn retry<F, Fut, T>(&self, mut f: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut retries = 0;
        let mut delay = self.initial_delay;

        loop {
            match f().await {
                Ok(result) => return Ok(result),
                Err(e) if retries < self.max_retries => {
                    retries += 1;

                    // Add jitter to delay
                    let jitter = if self.jitter_factor > 0.0 {
                        let jitter_range = delay.as_secs_f64() * self.jitter_factor;
                        let jitter = rand::random::<f64>() * jitter_range - (jitter_range / 2.0);
                        Duration::from_secs_f64(jitter.abs())
                    } else {
                        Duration::ZERO
                    };

                    let actual_delay = delay + jitter;
                    warn!(
                        "Attempt {} failed, retrying in {:?}: {}",
                        retries, actual_delay, e
                    );

                    sleep(actual_delay).await;

                    // Calculate next delay with exponential backoff
                    delay = Duration::from_secs_f64(
                        (delay.as_secs_f64() * self.backoff_multiplier)
                            .min(self.max_delay.as_secs_f64()),
                    );
                },
                Err(e) => return Err(e),
            }
        }
    }
}

/// Message batcher for efficient processing
pub struct MessageBatcher {
    /// Buffer for messages
    buffer: VecDeque<Message>,
    /// Maximum batch size
    max_batch_size: usize,
    /// Maximum wait time for a batch
    max_wait_time: Duration,
    /// Channel for incoming messages
    input_rx: mpsc::Receiver<Message>,
    /// Channel for outgoing batches
    output_tx: mpsc::Sender<Vec<Message>>,
}

impl MessageBatcher {
    /// Create a new message batcher
    pub fn new(
        max_batch_size: usize,
        max_wait_time: Duration,
    ) -> (Self, mpsc::Sender<Message>, mpsc::Receiver<Vec<Message>>) {
        let (input_tx, input_rx) = mpsc::channel(100);
        let (output_tx, output_rx) = mpsc::channel(10);

        let batcher = Self {
            buffer: VecDeque::new(),
            max_batch_size,
            max_wait_time,
            input_rx,
            output_tx,
        };

        (batcher, input_tx, output_rx)
    }

    /// Run the batcher
    pub async fn run(mut self) {
        loop {
            // Wait for messages with timeout
            let timeout_result = timeout(self.max_wait_time, self.input_rx.recv()).await;

            match timeout_result {
                Ok(Some(msg)) => {
                    self.buffer.push_back(msg);

                    // Check if we should emit a batch
                    if self.buffer.len() >= self.max_batch_size {
                        self.emit_batch().await;
                    }
                },
                Ok(None) => {
                    // Channel closed, emit remaining messages and exit
                    if !self.buffer.is_empty() {
                        self.emit_batch().await;
                    }
                    break;
                },
                Err(_) => {
                    // Timeout, emit batch if we have messages
                    if !self.buffer.is_empty() {
                        self.emit_batch().await;
                    }
                },
            }
        }
    }

    /// Emit a batch of messages
    async fn emit_batch(&mut self) {
        if self.buffer.is_empty() {
            return;
        }

        let batch: Vec<Message> = self.buffer.drain(..).collect();
        debug!("Emitting batch of {} messages", batch.len());

        if self.output_tx.send(batch).await.is_err() {
            warn!("Failed to send batch, receiver dropped");
        }
    }
}

/// Performance metrics collector
#[derive(Debug, Default, Clone)]
pub struct PerformanceMetrics {
    /// Total number of requests
    pub total_requests: u64,
    /// Number of successful requests
    pub successful_requests: u64,
    /// Number of failed requests
    pub failed_requests: u64,
    /// Total latency in milliseconds
    pub total_latency_ms: u64,
    /// Maximum latency in milliseconds
    pub max_latency_ms: u64,
    /// Minimum latency in milliseconds
    pub min_latency_ms: u64,
}

impl PerformanceMetrics {
    /// Record a successful request
    pub fn record_success(&mut self, latency_ms: u64) {
        self.total_requests += 1;
        self.successful_requests += 1;
        self.total_latency_ms += latency_ms;
        self.max_latency_ms = self.max_latency_ms.max(latency_ms);
        self.min_latency_ms = if self.min_latency_ms == 0 {
            latency_ms
        } else {
            self.min_latency_ms.min(latency_ms)
        };
    }

    /// Record a failed request
    pub fn record_failure(&mut self) {
        self.total_requests += 1;
        self.failed_requests += 1;
    }

    /// Get average latency
    pub fn average_latency_ms(&self) -> f64 {
        if self.successful_requests == 0 {
            0.0
        } else {
            self.total_latency_ms as f64 / self.successful_requests as f64
        }
    }

    /// Get success rate
    pub fn success_rate(&self) -> f64 {
        if self.total_requests == 0 {
            0.0
        } else {
            self.successful_requests as f64 / self.total_requests as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.initial_delay, Duration::from_millis(100));
        assert_eq!(config.backoff_multiplier, 2.0);
    }

    #[test]
    fn test_performance_metrics() {
        let mut metrics = PerformanceMetrics::default();

        metrics.record_success(100);
        metrics.record_success(200);
        metrics.record_failure();

        assert_eq!(metrics.total_requests, 3);
        assert_eq!(metrics.successful_requests, 2);
        assert_eq!(metrics.failed_requests, 1);
        assert_eq!(metrics.average_latency_ms(), 150.0);
        assert!((metrics.success_rate() - 0.666).abs() < 0.01);
    }

    #[test]
    fn test_record_success_min_latency_set_on_first_call() {
        let mut metrics = PerformanceMetrics::default();
        assert_eq!(metrics.min_latency_ms, 0);
        metrics.record_success(42);
        assert_eq!(metrics.min_latency_ms, 42);
    }

    #[test]
    fn test_record_success_min_latency_updates_correctly() {
        let mut metrics = PerformanceMetrics::default();
        metrics.record_success(100);
        metrics.record_success(50);
        metrics.record_success(200);
        assert_eq!(metrics.min_latency_ms, 50);
    }

    #[test]
    fn test_record_failure_counting() {
        let mut metrics = PerformanceMetrics::default();
        metrics.record_failure();
        metrics.record_failure();
        metrics.record_failure();
        assert_eq!(metrics.failed_requests, 3);
        assert_eq!(metrics.total_requests, 3);
        assert_eq!(metrics.successful_requests, 0);
    }

    #[test]
    fn test_average_latency_ms_zero_successful_requests() {
        let metrics = PerformanceMetrics::default();
        assert_eq!(metrics.average_latency_ms(), 0.0);
    }

    #[test]
    fn test_success_rate_zero_total_requests() {
        let metrics = PerformanceMetrics::default();
        assert_eq!(metrics.success_rate(), 0.0);
    }

    #[test]
    fn test_max_latency_ms_tracks_correctly() {
        let mut metrics = PerformanceMetrics::default();
        metrics.record_success(10);
        metrics.record_success(500);
        metrics.record_success(200);
        assert_eq!(metrics.max_latency_ms, 500);
    }

    #[tokio::test]
    async fn test_retry_succeeds_on_first_try() {
        let config = RetryConfig {
            max_retries: 3,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
            backoff_multiplier: 2.0,
            jitter_factor: 0.0,
        };
        let result = config
            .retry(|| async { Ok::<_, crate::errors::SdkError>(42) })
            .await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_retry_fails_then_succeeds() {
        let config = RetryConfig {
            max_retries: 3,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
            backoff_multiplier: 2.0,
            jitter_factor: 0.0,
        };
        let attempt = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let attempt_clone = attempt.clone();
        let result = config
            .retry(move || {
                let attempt = attempt_clone.clone();
                async move {
                    let n = attempt.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if n < 2 {
                        Err(crate::errors::SdkError::ConnectionError("transient".into()))
                    } else {
                        Ok(99)
                    }
                }
            })
            .await;
        assert_eq!(result.unwrap(), 99);
        assert_eq!(attempt.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_max_retries_exhausted() {
        let config = RetryConfig {
            max_retries: 2,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
            backoff_multiplier: 2.0,
            jitter_factor: 0.0,
        };
        let result: crate::errors::Result<i32> = config
            .retry(|| async {
                Err(crate::errors::SdkError::ConnectionError(
                    "always fails".into(),
                ))
            })
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn test_message_batcher_new() {
        let (batcher, _tx, _rx) = MessageBatcher::new(10, Duration::from_millis(100));
        assert_eq!(batcher.max_batch_size, 10);
        assert_eq!(batcher.max_wait_time, Duration::from_millis(100));
        assert!(batcher.buffer.is_empty());
    }

    #[tokio::test]
    async fn test_message_batcher_emits_batch_on_channel_close() {
        let (batcher, tx, mut rx) = MessageBatcher::new(10, Duration::from_secs(5));

        let msg = Message::System {
            subtype: "test".into(),
            data: serde_json::json!({}),
        };
        tx.send(msg).await.unwrap();
        drop(tx); // close the channel

        tokio::spawn(async move { batcher.run().await });

        let batch = rx.recv().await.unwrap();
        assert_eq!(batch.len(), 1);
    }

    #[tokio::test]
    async fn test_message_batcher_emits_batch_when_max_size_reached() {
        let (batcher, tx, mut rx) = MessageBatcher::new(2, Duration::from_secs(5));

        tokio::spawn(async move { batcher.run().await });

        for _ in 0..2 {
            let msg = Message::System {
                subtype: "test".into(),
                data: serde_json::json!({}),
            };
            tx.send(msg).await.unwrap();
        }

        let batch = rx.recv().await.unwrap();
        assert_eq!(batch.len(), 2);
    }
}
