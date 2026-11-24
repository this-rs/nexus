//! The persistent memory of an "always" approval (PERM-ALWAYS-NATIVE, contract §5).
//!
//! A session-scoped approval (`PermissionScope::Session`) lives in the session's
//! `State.approved` set and dies with the session. An "always" approval
//! (`PermissionScope::Always`) is the SAME key, persisted beyond the session. As
//! with [`TranscriptStore`], the library defines the trait and an in-memory
//! implementation; the HOST injects a durable one (a file, a database) — the
//! library never touches a real store on its own, and every test runs against the
//! memory one, never a real machine.
//!
//! [`TranscriptStore`]: super::transcript::TranscriptStore

use std::collections::HashSet;
use std::sync::{Mutex, PoisonError};

use crate::agent::ProviderError;

/// Where "always" approvals are kept. Implementations are synchronous: an
/// approval is a short string, written once per answer.
///
/// The store is read once, at session open: a [`remove`](Self::remove) (revocation)
/// affects sessions opened afterward. Revoking a rule for an already-open session
/// is the host's responsibility (it owns the session lifecycle) — the same
/// load-at-open contract as [`TranscriptStore`].
///
/// [`TranscriptStore`]: super::transcript::TranscriptStore
pub trait PermissionRuleStore: Send + Sync {
    /// Every persisted approval key (a bare tool name, or `tool\u{0}subject`).
    fn load(&self) -> Result<Vec<String>, ProviderError>;
    /// Persists an approval key.
    fn insert(&self, key: &str) -> Result<(), ProviderError>;
    /// Removes an approval key (revocation).
    fn remove(&self, key: &str) -> Result<(), ProviderError>;
}

/// Approval keys kept in memory (the default): nothing survives a restart.
#[derive(Debug, Default)]
pub struct MemoryRuleStore {
    keys: Mutex<HashSet<String>>,
}

impl MemoryRuleStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl PermissionRuleStore for MemoryRuleStore {
    fn load(&self) -> Result<Vec<String>, ProviderError> {
        Ok(self
            .keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect())
    }

    fn insert(&self, key: &str) -> Result<(), ProviderError> {
        self.keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_owned());
        Ok(())
    }

    fn remove(&self, key: &str) -> Result<(), ProviderError> {
        self.keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryRuleStore, PermissionRuleStore};

    #[test]
    fn a_rule_inserted_is_loaded_and_removed() {
        let store = MemoryRuleStore::new();
        assert!(store.load().unwrap().is_empty());
        store.insert("mcp__nexus__Bash").unwrap();
        store
            .insert("browser_navigate\u{0}https://example.com")
            .unwrap();
        let keys = store.load().unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"mcp__nexus__Bash".to_owned()));
        assert!(keys.contains(&"browser_navigate\u{0}https://example.com".to_owned()));
        store.remove("mcp__nexus__Bash").unwrap();
        assert_eq!(
            store.load().unwrap(),
            vec!["browser_navigate\u{0}https://example.com".to_owned()]
        );
    }
}
