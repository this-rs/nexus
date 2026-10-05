//! The 15-minute page cache, with an injectable clock (N21).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

/// Where "now" comes from. Tests move it by hand instead of sleeping.
pub trait Clock: Send + Sync {
    /// Milliseconds since an arbitrary fixed point.
    fn now_ms(&self) -> u64;
}

/// The real clock.
#[derive(Debug)]
pub struct SystemClock {
    start: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// Fetched pages by URL.
pub struct PageCache {
    ttl_ms: u64,
    capacity: usize,
    entries: Mutex<HashMap<String, (u64, String)>>,
}

impl PageCache {
    /// A cache that keeps a page for `ttl`, at most `capacity` pages.
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            ttl_ms: u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX),
            capacity,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The page cached under `url`, if it is not older than the TTL.
    pub fn get(&self, url: &str, now_ms: u64) -> Option<String> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let (stored, page) = entries.get(url)?;
        (now_ms.saturating_sub(*stored) < self.ttl_ms).then(|| page.clone())
    }

    /// Stores a page; expired entries go first, then the oldest, when it is full.
    pub fn put(&self, url: &str, page: String, now_ms: u64) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let ttl = self.ttl_ms;
        entries.retain(|_, (stored, _)| now_ms.saturating_sub(*stored) < ttl);
        while entries.len() >= self.capacity {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            entries.remove(&oldest);
        }
        entries.insert(url.to_owned(), (now_ms, page));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_is_served_until_its_ttl_and_not_after() {
        let cache = PageCache::new(Duration::from_secs(900), 10);
        cache.put("u", "page".into(), 1_000);
        assert_eq!(cache.get("u", 1_000).as_deref(), Some("page"));
        assert_eq!(cache.get("u", 1_000 + 899_999).as_deref(), Some("page"));
        assert_eq!(
            cache.get("u", 1_000 + 900_000),
            None,
            "exactly at the TTL it is stale"
        );
        assert_eq!(cache.get("other", 1_000), None);
    }

    #[test]
    fn the_oldest_entry_makes_room() {
        let cache = PageCache::new(Duration::from_secs(900), 2);
        cache.put("a", "A".into(), 1);
        cache.put("b", "B".into(), 2);
        cache.put("c", "C".into(), 3);
        assert_eq!(cache.get("a", 4), None);
        assert_eq!(cache.get("b", 4).as_deref(), Some("B"));
        assert_eq!(cache.get("c", 4).as_deref(), Some("C"));
    }
}
