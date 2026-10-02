//! A session table superseded by [`crate::core::storage::InMemorySessionStore`].
//!
//! Only the [`Session`] row type is live here: it is the unit of the
//! [`crate::core::storage::SessionStore`] trait and of its three
//! implementations, and the four storage modules import it from this file.
//! [`SessionManager`] itself has never had a caller — the gateway's real
//! session table is
//! [`crate::core::interactive_session::InteractiveSessionManager`], which also
//! owns the TTL sweep (`cleanup_expired_sessions`) that this type has never
//! had. Nothing expires in this map: a row lives until someone removes it.
//!
//! The tests below therefore pin what a future caller would be entitled to
//! assume, and record the one point where this type and
//! `InMemorySessionStore` — otherwise field-for-field twins — disagree:
//! touching an unknown id is silent here and an `Err` there.

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct Session {
    pub id: String,
    pub project_path: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<String, Session>>>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn create_session(&self, project_path: Option<String>) -> String {
        let session_id = Uuid::new_v4().to_string();
        let now = Utc::now();

        let session = Session {
            id: session_id.clone(),
            project_path,
            created_at: now,
            updated_at: now,
        };

        self.sessions.write().insert(session_id.clone(), session);
        session_id
    }

    pub fn get_session(&self, session_id: &str) -> Option<Session> {
        self.sessions.read().get(session_id).cloned()
    }

    pub fn update_session(&self, session_id: &str) {
        if let Some(session) = self.sessions.write().get_mut(session_id) {
            session.updated_at = Utc::now();
        }
    }

    pub fn remove_session(&self, session_id: &str) -> Option<Session> {
        self.sessions.write().remove(session_id)
    }

    pub fn list_sessions(&self) -> Vec<Session> {
        self.sessions.read().values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Barrier;
    use std::thread;
    use std::time::Duration;

    /// Long enough that two `Utc::now()` readings taken either side of it are
    /// ordered on every platform, short enough not to slow the suite down.
    const TICK: Duration = Duration::from_millis(5);

    #[test]
    fn a_new_manager_holds_no_session_and_default_agrees_with_new() {
        assert!(SessionManager::new().list_sessions().is_empty());
        assert!(SessionManager::default().list_sessions().is_empty());
        assert!(
            SessionManager::default()
                .get_session("n-importe-quoi")
                .is_none()
        );
    }

    /// Every `create_session` mints a distinct v4 UUID. A collision would make
    /// the `insert` in `create_session` overwrite a live session silently —
    /// the return value of `HashMap::insert` is discarded — so the absence of
    /// collisions is what keeps that path unreachable.
    #[test]
    fn create_session_mints_a_distinct_v4_uuid_every_time() {
        let manager = SessionManager::new();
        let mut ids = HashSet::new();

        for _ in 0..512 {
            let id = manager.create_session(None);
            let uuid = Uuid::parse_str(&id).expect("create_session must return a UUID");
            assert_eq!(uuid.get_version_num(), 4, "expected a v4 UUID, got {id}");
            assert!(ids.insert(id), "create_session handed out a duplicate id");
        }

        assert_eq!(
            manager.list_sessions().len(),
            512,
            "a session was overwritten"
        );
    }

    /// A freshly created session carries the caller's `project_path` verbatim
    /// and is stamped once: `created_at == updated_at` until something touches
    /// it.
    #[test]
    fn create_session_stores_the_project_path_and_stamps_both_dates_once() {
        let manager = SessionManager::new();

        let with_path = manager.create_session(Some("/projet/alpha".to_string()));
        let without_path = manager.create_session(None);

        let alpha = manager.get_session(&with_path).unwrap();
        assert_eq!(alpha.id, with_path, "Session.id must be the map key");
        assert_eq!(alpha.project_path, Some("/projet/alpha".to_string()));
        assert_eq!(
            alpha.created_at, alpha.updated_at,
            "an untouched session must have a single timestamp"
        );

        let bare = manager.get_session(&without_path).unwrap();
        assert_eq!(bare.project_path, None);
    }

    #[test]
    fn get_session_of_an_unknown_id_is_none_and_does_not_create_it() {
        let manager = SessionManager::new();
        let live = manager.create_session(Some("/projet".to_string()));

        assert!(manager.get_session("jamais-cree").is_none());
        assert!(manager.get_session("").is_none());
        // A near miss must not match: lookup is exact, not a prefix.
        assert!(manager.get_session(&live[..live.len() - 1]).is_none());
        assert_eq!(
            manager.list_sessions().len(),
            1,
            "a failed lookup inserted a session"
        );
    }

    /// `get_session` hands back a clone. Mutating it must not reach the map,
    /// otherwise one caller's edits would leak into every later reader.
    #[test]
    fn get_session_returns_an_independent_snapshot() {
        let manager = SessionManager::new();
        let id = manager.create_session(Some("/projet/origine".to_string()));

        let mut snapshot = manager.get_session(&id).unwrap();
        snapshot.project_path = Some("/projet/pirate".to_string());
        snapshot.id = "id-pirate".to_string();

        let stored = manager.get_session(&id).unwrap();
        assert_eq!(stored.project_path, Some("/projet/origine".to_string()));
        assert_eq!(stored.id, id);
        assert!(manager.get_session("id-pirate").is_none());
    }

    /// Two sessions keep their own `project_path`; a lookup never serves the
    /// other one's row.
    #[test]
    fn two_sessions_never_serve_each_others_payload() {
        let manager = SessionManager::new();
        let un = manager.create_session(Some("/projet/un".to_string()));
        let deux = manager.create_session(Some("/projet/deux".to_string()));

        assert_ne!(un, deux);
        assert_eq!(
            manager.get_session(&un).unwrap().project_path,
            Some("/projet/un".to_string())
        );
        assert_eq!(
            manager.get_session(&deux).unwrap().project_path,
            Some("/projet/deux".to_string())
        );
    }

    /// `update_session` moves `updated_at` forward and leaves everything else
    /// — `created_at`, `id`, `project_path` — alone.
    #[test]
    fn update_session_advances_updated_at_only() {
        let manager = SessionManager::new();
        let id = manager.create_session(Some("/projet".to_string()));
        let before = manager.get_session(&id).unwrap();

        thread::sleep(TICK);
        manager.update_session(&id);

        let after = manager.get_session(&id).unwrap();
        assert!(
            after.updated_at > before.updated_at,
            "updated_at must advance: {} -> {}",
            before.updated_at,
            after.updated_at
        );
        assert_eq!(
            after.created_at, before.created_at,
            "created_at must not move"
        );
        assert_eq!(after.id, before.id);
        assert_eq!(after.project_path, before.project_path);
    }

    /// Touching one session must not touch its neighbours.
    #[test]
    fn update_session_touches_only_the_targeted_session() {
        let manager = SessionManager::new();
        let cible = manager.create_session(Some("/cible".to_string()));
        let temoin_a = manager.create_session(Some("/temoin-a".to_string()));
        let temoin_b = manager.create_session(None);

        let avant_a = manager.get_session(&temoin_a).unwrap();
        let avant_b = manager.get_session(&temoin_b).unwrap();

        thread::sleep(TICK);
        manager.update_session(&cible);

        let apres_a = manager.get_session(&temoin_a).unwrap();
        let apres_b = manager.get_session(&temoin_b).unwrap();
        assert_eq!(
            apres_a.updated_at, avant_a.updated_at,
            "/temoin-a was touched too"
        );
        assert_eq!(
            apres_b.updated_at, avant_b.updated_at,
            "/temoin-b was touched too"
        );
        assert!(manager.get_session(&cible).unwrap().updated_at > avant_a.updated_at);
    }

    /// Touching an unknown id is a SILENT no-op: no insert, and the caller is
    /// told nothing because the method returns `()`.
    ///
    /// This is a deliberate divergence from the sibling implementation
    /// [`crate::core::storage::InMemorySessionStore`], whose `update` answers
    /// `Err("Session not found: {id}")` on the same input. Anything wiring
    /// this manager in must not assume the two behave alike.
    #[test]
    fn update_session_on_an_unknown_id_is_a_silent_no_op() {
        let manager = SessionManager::new();

        manager.update_session("jamais-cree");
        assert!(
            manager.list_sessions().is_empty(),
            "the touch inserted a session"
        );

        let id = manager.create_session(None);
        let before = manager.get_session(&id).unwrap();
        thread::sleep(TICK);
        manager.update_session("toujours-pas");
        let after = manager.get_session(&id).unwrap();
        assert_eq!(
            after.updated_at, before.updated_at,
            "a touch on an unknown id moved another session's clock"
        );
        assert_eq!(manager.list_sessions().len(), 1);
    }

    /// `remove_session` hands back the row it deleted and deletes nothing else.
    #[test]
    fn remove_session_returns_the_row_and_removes_only_that_one() {
        let manager = SessionManager::new();
        let cible = manager.create_session(Some("/cible".to_string()));
        let temoin = manager.create_session(Some("/temoin".to_string()));

        let removed = manager
            .remove_session(&cible)
            .expect("remove must return the row");
        assert_eq!(removed.id, cible);
        assert_eq!(removed.project_path, Some("/cible".to_string()));

        assert!(manager.get_session(&cible).is_none());
        assert_eq!(
            manager.get_session(&temoin).unwrap().project_path,
            Some("/temoin".to_string()),
            "removing one session took the other one with it"
        );
        assert_eq!(manager.list_sessions().len(), 1);
    }

    /// Removing twice, or removing something that was never there, is
    /// `None` — not a panic and not an error.
    #[test]
    fn remove_session_is_idempotent_and_unknown_ids_are_none() {
        let manager = SessionManager::new();
        let id = manager.create_session(None);

        assert!(manager.remove_session(&id).is_some());
        assert!(
            manager.remove_session(&id).is_none(),
            "the second remove found a row"
        );
        assert!(manager.remove_session("jamais-cree").is_none());
        assert!(manager.list_sessions().is_empty());
    }

    /// `list_sessions` returns every live row exactly once. The order is
    /// `HashMap` iteration order, so it is deliberately not asserted.
    #[test]
    fn list_sessions_returns_every_live_row_exactly_once() {
        let manager = SessionManager::new();
        let ids: HashSet<String> = (0..16)
            .map(|i| manager.create_session(Some(format!("/p{i}"))))
            .collect();

        let listed = manager.list_sessions();
        assert_eq!(listed.len(), 16);
        let listed_ids: HashSet<String> = listed.iter().map(|s| s.id.clone()).collect();
        assert_eq!(listed_ids, ids, "list_sessions lost or duplicated a row");

        let gone = ids.iter().next().unwrap().clone();
        manager.remove_session(&gone);
        let after: HashSet<String> = manager
            .list_sessions()
            .iter()
            .map(|s| s.id.clone())
            .collect();
        assert!(!after.contains(&gone), "a removed session is still listed");
        assert_eq!(after.len(), 15);
    }

    /// Creation from many threads at once: every id is distinct, every id is
    /// readable afterwards, and nothing is lost. The barrier makes the threads
    /// contend on the same write lock instead of running one after another.
    #[test]
    fn concurrent_creation_loses_no_session_and_collides_on_no_id() {
        const THREADS: usize = 4;
        const PER_THREAD: usize = 32;

        let manager = Arc::new(SessionManager::new());
        let barrier = Arc::new(Barrier::new(THREADS));

        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let manager = Arc::clone(&manager);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    (0..PER_THREAD)
                        .map(|i| manager.create_session(Some(format!("/fil-{t}/{i}"))))
                        .collect::<Vec<_>>()
                })
            })
            .collect();

        let mut all = HashSet::new();
        for handle in handles {
            for id in handle.join().expect("a creating thread panicked") {
                assert!(
                    all.insert(id),
                    "two threads were handed the same session id"
                );
            }
        }

        assert_eq!(all.len(), THREADS * PER_THREAD);
        assert_eq!(manager.list_sessions().len(), THREADS * PER_THREAD);
        for id in &all {
            assert!(
                manager.get_session(id).is_some(),
                "session {id} vanished after creation"
            );
        }
    }

    /// Readers running against concurrent writers either see a session or do
    /// not — they never see a half-built row. Each id a writer publishes is
    /// readable, with its own payload, the instant the writer has it.
    #[test]
    fn a_reader_racing_creation_never_sees_a_half_built_session() {
        const WRITERS: usize = 3;
        const PER_WRITER: usize = 32;

        let manager = Arc::new(SessionManager::new());
        let barrier = Arc::new(Barrier::new(WRITERS + 1));

        let writers: Vec<_> = (0..WRITERS)
            .map(|w| {
                let manager = Arc::clone(&manager);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    for i in 0..PER_WRITER {
                        let path = format!("/fil-{w}/{i}");
                        let id = manager.create_session(Some(path.clone()));
                        let read = manager
                            .get_session(&id)
                            .expect("a just-created session must be readable");
                        assert_eq!(read.id, id);
                        assert_eq!(read.project_path, Some(path));
                        assert_eq!(read.created_at, read.updated_at);
                    }
                })
            })
            .collect();

        let reader = {
            let manager = Arc::clone(&manager);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let mut seen = 0;
                for _ in 0..50 {
                    for session in manager.list_sessions() {
                        // Whatever a reader observes is internally consistent.
                        assert!(session.project_path.is_some());
                        assert!(session.updated_at >= session.created_at);
                        seen += 1;
                    }
                }
                seen
            })
        };

        for writer in writers {
            writer.join().expect("a writing thread panicked");
        }
        reader.join().expect("the reading thread panicked");
        assert_eq!(manager.list_sessions().len(), WRITERS * PER_WRITER);
    }

    /// Creation and removal interleaved: a thread that removes its own
    /// sessions must not take anyone else's with it.
    #[test]
    fn concurrent_creation_and_removal_only_removes_what_each_thread_owns() {
        const THREADS: usize = 2;
        const PER_THREAD: usize = 32;

        let manager = Arc::new(SessionManager::new());
        let barrier = Arc::new(Barrier::new(THREADS * 2));

        let keepers: Vec<_> = (0..THREADS)
            .map(|t| {
                let manager = Arc::clone(&manager);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    (0..PER_THREAD)
                        .map(|i| manager.create_session(Some(format!("/garde-{t}/{i}"))))
                        .collect::<Vec<_>>()
                })
            })
            .collect();

        let churners: Vec<_> = (0..THREADS)
            .map(|t| {
                let manager = Arc::clone(&manager);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    for i in 0..PER_THREAD {
                        let id = manager.create_session(Some(format!("/jetable-{t}/{i}")));
                        manager.update_session(&id);
                        let removed = manager
                            .remove_session(&id)
                            .expect("a thread could not remove its own session");
                        assert_eq!(removed.id, id);
                    }
                })
            })
            .collect();

        let mut kept = HashSet::new();
        for handle in keepers {
            kept.extend(handle.join().expect("a keeping thread panicked"));
        }
        for handle in churners {
            handle.join().expect("a churning thread panicked");
        }

        let survivors: HashSet<String> = manager
            .list_sessions()
            .iter()
            .map(|s| s.id.clone())
            .collect();
        assert_eq!(
            survivors, kept,
            "the churn removed sessions it did not own, or left its own behind"
        );
    }
}
