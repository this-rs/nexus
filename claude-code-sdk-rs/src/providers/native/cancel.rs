//! A minimal cancellation token (`tokio-util` is not a dependency of the crate).
//!
//! One token per turn (interruption, timeout, close) and one per running tool
//! call (`cancel_tools`). Cancelling is sticky and idempotent; waiting for it is
//! cancel-safe.

use std::sync::Arc;

use tokio::sync::watch;

/// A sticky, clonable cancellation flag that can be awaited.
#[derive(Debug, Clone)]
pub struct CancelToken {
    tx: Arc<watch::Sender<bool>>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    /// A token that is not cancelled.
    pub fn new() -> Self {
        Self {
            tx: Arc::new(watch::Sender::new(false)),
        }
    }

    /// Cancels the token. Idempotent.
    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }

    /// Whether [`CancelToken::cancel`] was called on this token or a clone.
    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    /// Completes once the token is cancelled (immediately if it already is).
    pub async fn cancelled(&self) {
        let mut receiver = self.tx.subscribe();
        // The sender lives as long as `self`: the wait cannot fail.
        let _ = receiver.wait_for(|cancelled| *cancelled).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_cancelled_token_wakes_its_waiters_and_stays_cancelled() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        let waiter = {
            let token = token.clone();
            tokio::spawn(async move { token.cancelled().await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        token.cancel();
        token.cancel();
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("the waiter wakes")
            .unwrap();
        // Waiting after the fact returns at once.
        tokio::time::timeout(Duration::from_millis(200), token.cancelled())
            .await
            .expect("already cancelled");
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn an_uncancelled_token_never_completes() {
        let token = CancelToken::new();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), token.cancelled())
                .await
                .is_err()
        );
    }
}
