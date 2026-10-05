//! A search key, by reference (N22).
//!
//! The configuration names *where* a key is (an environment variable, a file the secret store
//! wrote), never the key. The value is read when a search runs, held in a type that cannot be
//! printed, and sent only in a request header.

use std::path::PathBuf;

/// A secret value. Not `Display`; `Debug` prints a marker; no `Serialize`.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// Wraps a value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value, for the one place that must send it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// Where a key lives.
#[derive(Clone)]
pub enum KeySource {
    /// An environment variable of the server process (set by whatever launched it from the
    /// secret store). Only the *name* is configuration.
    Env(String),
    /// A file holding the key (the secret store's `0600` file). Only the *path* is configuration.
    File(PathBuf),
    /// A key already in memory: for tests and embedders that resolved it themselves.
    Static(Secret),
}

impl std::fmt::Debug for KeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Env(name) => write!(f, "KeySource::Env({name})"),
            Self::File(path) => write!(f, "KeySource::File({})", path.display()),
            Self::Static(_) => f.write_str("KeySource::Static([redacted])"),
        }
    }
}

impl KeySource {
    /// Reads the key now. The error says which reference failed, never a value.
    pub fn resolve(&self) -> Result<Secret, String> {
        let value = match self {
            Self::Env(name) => std::env::var(name)
                .map_err(|_| format!("the environment variable {name} is not set"))?,
            Self::File(path) => std::fs::read_to_string(path).map_err(|e| {
                format!("cannot read the key file {}: {}", path.display(), e.kind())
            })?,
            Self::Static(secret) => return Ok(secret.clone()),
        };
        let value = value.trim().to_owned();
        if value.is_empty() {
            return Err("the key reference is empty".to_owned());
        }
        Ok(Secret(value))
    }
}
