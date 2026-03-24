//! Token usage tracking and budget management
//!
//! This module provides utilities for monitoring token consumption and managing budgets
//! to help control costs when using Claude Code.

use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::warn;

/// Token usage statistics tracker
#[derive(Debug, Clone, Default)]
pub struct TokenUsageTracker {
    /// Total input tokens consumed
    pub total_input_tokens: u64,
    /// Total output tokens consumed
    pub total_output_tokens: u64,
    /// Total cost in USD
    pub total_cost_usd: f64,
    /// Number of sessions/queries completed
    pub session_count: usize,
}

impl TokenUsageTracker {
    /// Create a new empty tracker
    pub fn new() -> Self {
        Self::default()
    }

    /// Get total tokens (input + output)
    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens + self.total_output_tokens
    }

    /// Get average tokens per session
    pub fn avg_tokens_per_session(&self) -> f64 {
        if self.session_count == 0 {
            0.0
        } else {
            self.total_tokens() as f64 / self.session_count as f64
        }
    }

    /// Get average cost per session
    pub fn avg_cost_per_session(&self) -> f64 {
        if self.session_count == 0 {
            0.0
        } else {
            self.total_cost_usd / self.session_count as f64
        }
    }

    /// Update statistics with new usage data
    pub fn update(&mut self, input_tokens: u64, output_tokens: u64, cost_usd: f64) {
        self.total_input_tokens += input_tokens;
        self.total_output_tokens += output_tokens;
        self.total_cost_usd += cost_usd;
        self.session_count += 1;
    }

    /// Reset all statistics to zero
    pub fn reset(&mut self) {
        self.total_input_tokens = 0;
        self.total_output_tokens = 0;
        self.total_cost_usd = 0.0;
        self.session_count = 0;
    }
}

/// Budget limits and alerts
#[derive(Debug, Clone)]
pub struct BudgetLimit {
    /// Maximum cost in USD (None = unlimited)
    pub max_cost_usd: Option<f64>,
    /// Maximum total tokens (None = unlimited)
    pub max_tokens: Option<u64>,
    /// Threshold percentage for warning (0.0-1.0, default 0.8 for 80%)
    pub warning_threshold: f64,
}

impl Default for BudgetLimit {
    fn default() -> Self {
        Self {
            max_cost_usd: None,
            max_tokens: None,
            warning_threshold: 0.8,
        }
    }
}

impl BudgetLimit {
    /// Create a new budget limit with cost cap
    pub fn with_cost(max_cost_usd: f64) -> Self {
        Self {
            max_cost_usd: Some(max_cost_usd),
            ..Default::default()
        }
    }

    /// Create a new budget limit with token cap
    pub fn with_tokens(max_tokens: u64) -> Self {
        Self {
            max_tokens: Some(max_tokens),
            ..Default::default()
        }
    }

    /// Create a new budget limit with both caps
    pub fn with_both(max_cost_usd: f64, max_tokens: u64) -> Self {
        Self {
            max_cost_usd: Some(max_cost_usd),
            max_tokens: Some(max_tokens),
            warning_threshold: 0.8,
        }
    }

    /// Set warning threshold (0.0-1.0)
    pub fn with_warning_threshold(mut self, threshold: f64) -> Self {
        self.warning_threshold = threshold.clamp(0.0, 1.0);
        self
    }

    /// Check if usage exceeds limits
    pub fn check_limits(&self, usage: &TokenUsageTracker) -> BudgetStatus {
        let mut status = BudgetStatus::Ok;

        // Check cost limit
        if let Some(max_cost) = self.max_cost_usd {
            let cost_ratio = usage.total_cost_usd / max_cost;

            if cost_ratio >= 1.0 {
                status = BudgetStatus::Exceeded;
            } else if cost_ratio >= self.warning_threshold {
                status = BudgetStatus::Warning {
                    current_ratio: cost_ratio,
                    message: format!(
                        "Cost usage at {:.1}% (${:.2}/${:.2})",
                        cost_ratio * 100.0,
                        usage.total_cost_usd,
                        max_cost
                    ),
                };
            }
        }

        // Check token limit
        if let Some(max_tokens) = self.max_tokens {
            let token_ratio = usage.total_tokens() as f64 / max_tokens as f64;

            if token_ratio >= 1.0 {
                status = BudgetStatus::Exceeded;
            } else if token_ratio >= self.warning_threshold {
                // If already warning from cost, keep the exceeded state
                if !matches!(status, BudgetStatus::Exceeded) {
                    status = BudgetStatus::Warning {
                        current_ratio: token_ratio,
                        message: format!(
                            "Token usage at {:.1}% ({}/{})",
                            token_ratio * 100.0,
                            usage.total_tokens(),
                            max_tokens
                        ),
                    };
                }
            }
        }

        status
    }
}

/// Budget status result
#[derive(Debug, Clone, PartialEq)]
pub enum BudgetStatus {
    /// Usage is within limits
    Ok,
    /// Usage exceeds warning threshold
    Warning {
        /// Current usage ratio (0.0-1.0)
        current_ratio: f64,
        /// Warning message
        message: String,
    },
    /// Usage exceeds limits
    Exceeded,
}

/// Callback type for budget warnings
pub type BudgetWarningCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Budget manager that combines tracker and limits
#[derive(Clone)]
pub struct BudgetManager {
    tracker: Arc<RwLock<TokenUsageTracker>>,
    limit: Arc<RwLock<Option<BudgetLimit>>>,
    on_warning: Arc<RwLock<Option<BudgetWarningCallback>>>,
    warning_fired: Arc<RwLock<bool>>,
}

impl BudgetManager {
    /// Create a new budget manager
    pub fn new() -> Self {
        Self {
            tracker: Arc::new(RwLock::new(TokenUsageTracker::new())),
            limit: Arc::new(RwLock::new(None)),
            on_warning: Arc::new(RwLock::new(None)),
            warning_fired: Arc::new(RwLock::new(false)),
        }
    }

    /// Set budget limit
    pub async fn set_limit(&self, limit: BudgetLimit) {
        *self.limit.write().await = Some(limit);
        *self.warning_fired.write().await = false;
    }

    /// Set warning callback
    pub async fn set_warning_callback(&self, callback: BudgetWarningCallback) {
        *self.on_warning.write().await = Some(callback);
    }

    /// Clear budget limit
    pub async fn clear_limit(&self) {
        *self.limit.write().await = None;
        *self.warning_fired.write().await = false;
    }

    /// Get current usage statistics
    pub async fn get_usage(&self) -> TokenUsageTracker {
        self.tracker.read().await.clone()
    }

    /// Update usage and check limits
    pub async fn update_usage(&self, input_tokens: u64, output_tokens: u64, cost_usd: f64) {
        // Update tracker
        self.tracker
            .write()
            .await
            .update(input_tokens, output_tokens, cost_usd);

        // Check limits
        if let Some(limit) = self.limit.read().await.as_ref() {
            let usage = self.tracker.read().await.clone();
            let status = limit.check_limits(&usage);

            match status {
                BudgetStatus::Warning { message, .. } => {
                    let mut fired = self.warning_fired.write().await;
                    if !*fired {
                        *fired = true;
                        warn!("Budget warning: {}", message);

                        if let Some(callback) = self.on_warning.read().await.as_ref() {
                            callback(&message);
                        }
                    }
                },
                BudgetStatus::Exceeded => {
                    warn!(
                        "Budget exceeded! Current usage: {} tokens, ${:.2}",
                        usage.total_tokens(),
                        usage.total_cost_usd
                    );

                    if let Some(callback) = self.on_warning.read().await.as_ref() {
                        callback("Budget limit exceeded");
                    }
                },
                BudgetStatus::Ok => {
                    // Reset warning flag if usage dropped below threshold
                    *self.warning_fired.write().await = false;
                },
            }
        }
    }

    /// Reset usage statistics
    pub async fn reset_usage(&self) {
        self.tracker.write().await.reset();
        *self.warning_fired.write().await = false;
    }

    /// Check if budget is exceeded
    pub async fn is_exceeded(&self) -> bool {
        if let Some(limit) = self.limit.read().await.as_ref() {
            let usage = self.tracker.read().await.clone();
            matches!(limit.check_limits(&usage), BudgetStatus::Exceeded)
        } else {
            false
        }
    }
}

impl Default for BudgetManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tracker_basics() {
        let mut tracker = TokenUsageTracker::new();
        assert_eq!(tracker.total_tokens(), 0);
        assert_eq!(tracker.total_cost_usd, 0.0);

        tracker.update(100, 200, 0.05);
        assert_eq!(tracker.total_input_tokens, 100);
        assert_eq!(tracker.total_output_tokens, 200);
        assert_eq!(tracker.total_tokens(), 300);
        assert_eq!(tracker.total_cost_usd, 0.05);
        assert_eq!(tracker.session_count, 1);

        tracker.update(50, 100, 0.02);
        assert_eq!(tracker.total_tokens(), 450);
        assert_eq!(tracker.total_cost_usd, 0.07);
        assert_eq!(tracker.session_count, 2);
    }

    #[test]
    fn test_budget_limits() {
        let limit = BudgetLimit::with_cost(1.0).with_warning_threshold(0.8);

        let mut tracker = TokenUsageTracker::new();
        tracker.update(100, 200, 0.5);
        assert!(matches!(limit.check_limits(&tracker), BudgetStatus::Ok));

        tracker.update(100, 200, 0.35);
        assert!(matches!(
            limit.check_limits(&tracker),
            BudgetStatus::Warning { .. }
        ));

        tracker.update(100, 200, 0.2);
        assert!(matches!(
            limit.check_limits(&tracker),
            BudgetStatus::Exceeded
        ));
    }

    #[tokio::test]
    async fn test_budget_manager() {
        let manager = BudgetManager::new();

        manager.set_limit(BudgetLimit::with_tokens(1000)).await;
        manager.update_usage(300, 200, 0.05).await;

        let usage = manager.get_usage().await;
        assert_eq!(usage.total_tokens(), 500);

        assert!(!manager.is_exceeded().await);

        manager.update_usage(300, 300, 0.05).await;
        assert!(manager.is_exceeded().await);
    }

    #[test]
    fn test_avg_tokens_per_session_zero_sessions() {
        let tracker = TokenUsageTracker::new();
        assert_eq!(tracker.avg_tokens_per_session(), 0.0);
    }

    #[test]
    fn test_avg_tokens_per_session_with_sessions() {
        let mut tracker = TokenUsageTracker::new();
        tracker.update(100, 200, 0.05); // 300 tokens
        tracker.update(200, 100, 0.03); // 300 tokens
        // Total: 600 tokens over 2 sessions = 300.0 avg
        assert_eq!(tracker.avg_tokens_per_session(), 300.0);
    }

    #[test]
    fn test_avg_cost_per_session_zero_sessions() {
        let tracker = TokenUsageTracker::new();
        assert_eq!(tracker.avg_cost_per_session(), 0.0);
    }

    #[test]
    fn test_avg_cost_per_session_with_sessions() {
        let mut tracker = TokenUsageTracker::new();
        tracker.update(100, 200, 0.10);
        tracker.update(200, 100, 0.20);
        // Total: $0.30 over 2 sessions = $0.15 avg
        assert!((tracker.avg_cost_per_session() - 0.15).abs() < f64::EPSILON);
    }

    #[test]
    fn test_reset() {
        let mut tracker = TokenUsageTracker::new();
        tracker.update(100, 200, 0.05);
        tracker.update(50, 50, 0.02);
        assert_eq!(tracker.session_count, 2);

        tracker.reset();
        assert_eq!(tracker.total_input_tokens, 0);
        assert_eq!(tracker.total_output_tokens, 0);
        assert_eq!(tracker.total_cost_usd, 0.0);
        assert_eq!(tracker.session_count, 0);
        assert_eq!(tracker.total_tokens(), 0);
    }

    #[test]
    fn test_budget_limit_with_tokens_exceeded() {
        let limit = BudgetLimit::with_tokens(500);
        let mut tracker = TokenUsageTracker::new();
        tracker.update(300, 300, 0.05); // 600 tokens > 500 limit
        assert!(matches!(
            limit.check_limits(&tracker),
            BudgetStatus::Exceeded
        ));
    }

    #[test]
    fn test_budget_limit_with_both_token_exceeds_first() {
        // Cost well under limit, but tokens exceed
        let limit = BudgetLimit::with_both(10.0, 500);
        let mut tracker = TokenUsageTracker::new();
        tracker.update(300, 300, 0.01); // 600 tokens > 500, cost $0.01 < $10
        assert!(matches!(
            limit.check_limits(&tracker),
            BudgetStatus::Exceeded
        ));
    }

    #[test]
    fn test_budget_limit_with_warning_threshold_custom() {
        let limit = BudgetLimit::with_cost(1.0).with_warning_threshold(0.5);
        assert_eq!(limit.warning_threshold, 0.5);

        let mut tracker = TokenUsageTracker::new();
        tracker.update(100, 100, 0.55); // 55% > 50% threshold
        assert!(matches!(
            limit.check_limits(&tracker),
            BudgetStatus::Warning { .. }
        ));
    }

    #[test]
    fn test_budget_limit_token_warning() {
        let limit = BudgetLimit::with_tokens(1000).with_warning_threshold(0.8);
        let mut tracker = TokenUsageTracker::new();
        tracker.update(450, 400, 0.0); // 850 tokens = 85% > 80% threshold
        match limit.check_limits(&tracker) {
            BudgetStatus::Warning {
                current_ratio,
                message,
            } => {
                assert!((current_ratio - 0.85).abs() < 0.001);
                assert!(message.contains("Token usage"));
            }
            other => panic!("Expected Warning, got {:?}", other),
        }
    }

    #[test]
    fn test_budget_limit_cost_exceeded_overrides_token_warning() {
        // Cost exceeds, tokens only at warning level
        let limit = BudgetLimit::with_both(1.0, 1000).with_warning_threshold(0.8);
        let mut tracker = TokenUsageTracker::new();
        // cost $1.50 >= $1.0 (exceeded), tokens 850/1000 = 85% (warning)
        tracker.update(450, 400, 1.50);
        // Cost is checked first and sets Exceeded; token check keeps Exceeded
        assert!(matches!(
            limit.check_limits(&tracker),
            BudgetStatus::Exceeded
        ));
    }

    #[tokio::test]
    async fn test_budget_manager_set_warning_callback_fires_on_warning() {
        let manager = BudgetManager::new();
        let called = Arc::new(std::sync::Mutex::new(false));
        let called_clone = called.clone();

        manager
            .set_limit(BudgetLimit::with_cost(1.0).with_warning_threshold(0.8))
            .await;
        manager
            .set_warning_callback(Arc::new(move |_msg| {
                *called_clone.lock().unwrap() = true;
            }))
            .await;

        // Push past 80% threshold
        manager.update_usage(100, 100, 0.85).await;
        assert!(*called.lock().unwrap());
    }

    #[tokio::test]
    async fn test_budget_manager_clear_limit() {
        let manager = BudgetManager::new();
        manager.set_limit(BudgetLimit::with_tokens(100)).await;
        manager.update_usage(200, 200, 0.0).await;
        assert!(manager.is_exceeded().await);

        manager.clear_limit().await;
        assert!(!manager.is_exceeded().await);
    }

    #[tokio::test]
    async fn test_budget_manager_reset_usage() {
        let manager = BudgetManager::new();
        manager.set_limit(BudgetLimit::with_tokens(1000)).await;
        manager.update_usage(500, 500, 0.10).await;

        let usage = manager.get_usage().await;
        assert_eq!(usage.total_tokens(), 1000);

        manager.reset_usage().await;

        let usage = manager.get_usage().await;
        assert_eq!(usage.total_tokens(), 0);
        assert_eq!(usage.total_cost_usd, 0.0);
        assert_eq!(usage.session_count, 0);
        assert!(!manager.is_exceeded().await);
    }

    #[tokio::test]
    async fn test_budget_manager_warning_callback_fires_only_once() {
        let manager = BudgetManager::new();
        let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_clone = call_count.clone();

        manager
            .set_limit(BudgetLimit::with_cost(1.0).with_warning_threshold(0.8))
            .await;
        manager
            .set_warning_callback(Arc::new(move |_msg| {
                count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }))
            .await;

        // First update pushes past warning threshold
        manager.update_usage(100, 100, 0.85).await;
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Second update still in warning range but callback should NOT fire again
        manager.update_usage(10, 10, 0.05).await;
        assert_eq!(call_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_budget_manager_exceeded_fires_callback() {
        let manager = BudgetManager::new();
        let message = Arc::new(std::sync::Mutex::new(String::new()));
        let msg_clone = message.clone();

        manager
            .set_limit(BudgetLimit::with_cost(1.0))
            .await;
        manager
            .set_warning_callback(Arc::new(move |msg| {
                *msg_clone.lock().unwrap() = msg.to_string();
            }))
            .await;

        // Exceed budget directly
        manager.update_usage(100, 100, 1.50).await;
        assert!(manager.is_exceeded().await);
        assert_eq!(*message.lock().unwrap(), "Budget limit exceeded");
    }

    #[tokio::test]
    async fn test_budget_manager_is_exceeded_no_limit() {
        let manager = BudgetManager::new();
        // No limit set, even with high usage should return false
        manager.update_usage(999999, 999999, 999.0).await;
        assert!(!manager.is_exceeded().await);
    }
}
