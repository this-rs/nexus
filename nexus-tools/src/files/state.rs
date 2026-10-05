//! What a session has read, and what its files looked like when it did (N19).
//!
//! Claude Code refuses to edit a file the session has not read, and a file that changed
//! since. Both rules live here: they are what keeps a model from overwriting work it never
//! saw.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

/// A file as it was when the session last read or wrote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seen {
    modified: Option<SystemTime>,
    len: u64,
}

impl Seen {
    /// The file's current state on disk.
    pub fn of(path: &Path) -> std::io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        Ok(Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        })
    }
}

/// The files of one session.
#[derive(Debug, Default)]
pub struct FileState {
    seen: Mutex<HashMap<PathBuf, Seen>>,
    /// Held across a read-modify-write: two edits of one session never interleave.
    pub(crate) writing: tokio::sync::Mutex<()>,
}

/// Why a file may not be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsafe {
    /// The session never read it.
    NotRead,
    /// It changed after the session read it.
    Modified,
}

impl FileState {
    /// Records that `path` was read (or written) in its current state.
    pub fn record(&self, path: &Path) {
        if let Ok(seen) = Seen::of(path) {
            self.seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(path.to_owned(), seen);
        }
    }

    /// Whether the session may overwrite the existing file at `path`.
    pub fn check(&self, path: &Path) -> Result<(), Unsafe> {
        let known = self
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
            .copied();
        match known {
            None => Err(Unsafe::NotRead),
            Some(then) if Seen::of(path).ok() != Some(then) => Err(Unsafe::Modified),
            Some(_) => Ok(()),
        }
    }
}
