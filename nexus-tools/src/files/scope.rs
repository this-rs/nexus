//! Where a session's file tools may go (`readScope`, N19).
//!
//! A path is resolved **component by component against the real filesystem**: a symlink is
//! followed where it is, and a `..` pops the *resolved* path, not the written one. Resolving
//! `..` lexically first would let `inside/link/..` mean `inside` while the kernel means
//! the parent of whatever `link` points to.

use std::path::{Component, Path, PathBuf};

/// The directories a session may read and write.
#[derive(Debug, Clone)]
pub struct Scope {
    cwd: PathBuf,
    roots: Vec<PathBuf>,
}

/// A path that cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeError {
    /// It resolves outside every allowed directory.
    Outside(String),
    /// It is empty or otherwise unusable.
    Invalid(String),
}

impl std::fmt::Display for ScopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Outside(path) => write!(
                f,
                "Path is outside the directories this session may access: {path}"
            ),
            Self::Invalid(why) => f.write_str(why),
        }
    }
}

impl Scope {
    /// A scope rooted at `cwd`, plus `extra` directories. Roots are made real (symlinks
    /// resolved) once, here; a root that does not exist is refused.
    pub fn new(
        cwd: impl AsRef<Path>,
        extra: impl IntoIterator<Item = PathBuf>,
    ) -> std::io::Result<Self> {
        let cwd = std::fs::canonicalize(cwd)?;
        let mut roots = vec![cwd.clone()];
        for dir in extra {
            roots.push(std::fs::canonicalize(dir)?);
        }
        Ok(Self { cwd, roots })
    }

    /// The working directory (relative paths start here).
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The real path `path` designates, if it is inside the scope. The file need not exist.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, ScopeError> {
        if path.trim().is_empty() {
            return Err(ScopeError::Invalid("file_path is empty".to_owned()));
        }
        if path.contains('\0') {
            return Err(ScopeError::Invalid("file_path holds a NUL byte".to_owned()));
        }
        let written = Path::new(path);
        let mut resolved = if written.is_absolute() {
            PathBuf::new()
        } else {
            self.cwd.clone()
        };
        for component in written.components() {
            match component {
                Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
                Component::RootDir => resolved.push(Component::RootDir.as_os_str()),
                Component::CurDir => {},
                Component::ParentDir => {
                    resolved.pop();
                },
                Component::Normal(name) => {
                    resolved.push(name);
                    // Follow a symlink as soon as it is met: what comes after it (`..`
                    // included) is relative to where it leads.
                    if let Ok(real) = std::fs::canonicalize(&resolved) {
                        resolved = real;
                    }
                },
            }
        }
        if self.roots.iter().any(|root| resolved.starts_with(root)) {
            Ok(resolved)
        } else {
            Err(ScopeError::Outside(path.to_owned()))
        }
    }
}
