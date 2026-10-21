//! Quota of a provider account: what the provider says it has left, and until when.
//!
//! A provider that exposes its limits (Claude Code's `rate_limit_event`, Codex's
//! `account/rateLimits/updated`, the `x-ratelimit-*` headers of an HTTP endpoint)
//! reports them as [`AgentEvent::Quota`](super::AgentEvent::Quota) carrying a
//! [`QuotaSnapshot`]. The snapshot is provider-neutral and says nothing about
//! *which* account it is for: the host knows the instance the session belongs to.
//!
//! Times are epoch milliseconds, utilisation is a fraction (`0.97` = 97 %).

use serde::{Deserialize, Serialize};

/// State of one window, as the provider reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum QuotaStatus {
    /// Under every threshold.
    Allowed,
    /// Past a warning threshold, still served.
    Warning,
    /// Refused: nothing is served until `resets_at`.
    Exhausted,
}

/// Where a snapshot comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum QuotaSource {
    /// Read from the provider.
    #[default]
    Live,
    /// Typed by a human, for a provider that exposes nothing.
    Manual,
}

/// One rate-limit window (`five_hour`, `seven_day`, `requests`, `tokens`, `credits`…).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// Provider's name of the window, normalised (`five_hour`, `seven_day`, `300m`…).
    pub kind: String,
    /// Share of the window already used, `0.0..=1.0` (may exceed 1), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utilization: Option<f64>,
    /// When the window resets (epoch ms), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<u64>,
    /// State of the window.
    pub status: QuotaStatus,
}

/// What a provider says about an account's limits at one instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    /// The windows the provider reported.
    #[serde(default)]
    pub windows: Vec<QuotaWindow>,
    /// Live or manual.
    #[serde(default)]
    pub source: QuotaSource,
    /// When the snapshot was observed (epoch ms). `None` from a pure mapper: the
    /// host stamps it on receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_ms: Option<u64>,
}

impl QuotaSnapshot {
    /// A live snapshot of `windows`, not yet stamped.
    pub fn live(windows: Vec<QuotaWindow>) -> Self {
        Self {
            windows,
            source: QuotaSource::Live,
            observed_at_ms: None,
        }
    }

    /// Whether some window refuses the account.
    pub fn is_exhausted(&self) -> bool {
        self.windows
            .iter()
            .any(|w| w.status == QuotaStatus::Exhausted)
    }

    /// The instant until which the account is refused: the latest reset among the
    /// exhausted windows (all of them must reset for the account to be served).
    /// `None` when nothing is exhausted or no exhausted window says when it resets.
    pub fn exhausted_until_ms(&self) -> Option<u64> {
        self.windows
            .iter()
            .filter(|w| w.status == QuotaStatus::Exhausted)
            .filter_map(|w| w.resets_at_ms)
            .max()
    }
}

/// Name of a window of `minutes` minutes: `five_hour` and `seven_day` like Claude
/// Code, `<n>m` otherwise.
pub fn window_kind_from_minutes(minutes: u64) -> String {
    match minutes {
        300 => "five_hour".into(),
        10_080 => "seven_day".into(),
        other => format!("{other}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(kind: &str, resets: Option<u64>, status: QuotaStatus) -> QuotaWindow {
        QuotaWindow {
            kind: kind.into(),
            utilization: None,
            resets_at_ms: resets,
            status,
        }
    }

    #[test]
    fn the_account_is_refused_until_every_exhausted_window_has_reset() {
        let snapshot = QuotaSnapshot::live(vec![
            window("five_hour", Some(10), QuotaStatus::Exhausted),
            window("seven_day", Some(99), QuotaStatus::Exhausted),
            window("opus", Some(500), QuotaStatus::Allowed),
        ]);
        assert!(snapshot.is_exhausted());
        assert_eq!(snapshot.exhausted_until_ms(), Some(99));
    }

    #[test]
    fn a_warning_is_not_an_exhaustion() {
        let snapshot =
            QuotaSnapshot::live(vec![window("seven_day", Some(99), QuotaStatus::Warning)]);
        assert!(!snapshot.is_exhausted());
        assert_eq!(snapshot.exhausted_until_ms(), None);
    }

    #[test]
    fn window_names_follow_claude_code() {
        assert_eq!(window_kind_from_minutes(300), "five_hour");
        assert_eq!(window_kind_from_minutes(10_080), "seven_day");
        assert_eq!(window_kind_from_minutes(60), "60m");
    }
}
