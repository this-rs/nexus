//! Where a session's file tools may go (`readScope`, N19).
//!
//! A path is resolved **component by component against the real filesystem**: a symlink is
//! followed where it is, and a `..` pops the *resolved* path, not the written one. Resolving
//! `..` lexically first would let `inside/link/..` mean `inside` while the kernel means
//! the parent of whatever `link` points to.
//!
//! Three checks then apply to the **resolved** path, in this order:
//!
//! 1. **Local paths only** when the scope is unscoped, or when no root is itself remote: a
//!    Windows UNC path (`\\host\share`, `\\?\UNC\…`), a device namespace path (`\\.\pipe\…`) or
//!    any other verbatim prefix (`\\?\GLOBALROOT\…`) is refused. The written path is checked
//!    **before** the filesystem is touched, since resolving `\\host\share` opens an SMB
//!    connection that sends the user's NTLM hash to `host`.
//! 2. **The deny list**, which holds even when the scope is unscoped: `/proc/<pid>/environ`
//!    always (another process's environment, secrets included), plus every directory the
//!    harness names (`--deny-path`). It is checked on the resolved path, so a symlink, a `..` or
//!    a different spelling that leads into a denied directory is refused too.
//! 3. **The roots** (`--cwd`, `--add-dir`), unless the scope is unscoped.

use std::path::{Component, Path, PathBuf, Prefix};

/// The directories a session may read and write.
#[derive(Debug, Clone)]
pub struct Scope {
    cwd: PathBuf,
    roots: Vec<PathBuf>,
    /// `--unscoped`: paths are still resolved (symlinks, `..`) but the roots are not
    /// enforced. For a session whose permission mode already trusts everything
    /// (`trust`/`bypassPermissions`): it may then follow git worktrees created by
    /// another harness outside its launch directories. The deny list still holds.
    unscoped: bool,
    /// Directories no path may enter, whatever the roots (`--deny-path`), resolved.
    denied: Vec<PathBuf>,
    /// The identity (device, inode) of each denied place that existed at launch. A path
    /// comparison alone misses a second spelling the kernel does not fold into one: a macOS
    /// firmlink (`/System/Volumes/Data/Users/me/.ssh` is `/Users/me/.ssh`, and
    /// `realpath` keeps either spelling), a bind mount, a hard link to a denied file. Unix only:
    /// Windows has no stable file id in `std`, and its junctions are followed by `canonicalize`.
    denied_ids: Vec<FileId>,
}

/// A path that cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeError {
    /// It resolves outside every allowed directory.
    Outside(String),
    /// It resolves into a denied place (`/proc/<pid>/environ`, a `--deny-path`).
    Denied(String),
    /// It names a network share or a device, not a local file.
    NotLocal(String),
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
            Self::Denied(path) => write!(
                f,
                "Path is in a place this session may never access: {path}"
            ),
            Self::NotLocal(path) => write!(
                f,
                "Path names a network share or a device, not a local file: {path}"
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
        Ok(Self {
            cwd,
            roots,
            unscoped: false,
            denied: Vec::new(),
            denied_ids: Vec::new(),
        })
    }

    /// The same scope with the roots no longer enforced: `resolve` still normalises the
    /// path against the real filesystem, but accepts it wherever it lands — except a network
    /// or device path, and the deny list. Reserved for a session already in trust mode.
    #[must_use]
    pub fn without_bounds(mut self) -> Self {
        self.unscoped = true;
        self
    }

    /// Adds directories (or files) no path may enter, unscoped or not (`--deny-path`). Each
    /// must be absolute. It need not exist: it is resolved as far as it does (symlinks
    /// followed), so a denied place created later is still denied, and both its written and
    /// its real spelling are kept.
    pub fn with_denied(
        mut self,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> std::io::Result<Self> {
        for path in paths {
            if !path.is_absolute() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("a denied path must be absolute: {}", path.display()),
                ));
            }
            if !is_local(&path) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("a denied path must be local: {}", path.display()),
                ));
            }
            let real = follow(PathBuf::new(), &path);
            if let Some(id) = std::fs::metadata(&real).ok().as_ref().and_then(file_id) {
                self.denied_ids.push(id);
            }
            // A place created after launch has no identity yet: on macOS, its firmlink twin is
            // denied by path as well.
            if cfg!(target_os = "macos")
                && let Ok(rest) = real.strip_prefix("/")
            {
                self.denied.push(Path::new(MACOS_DATA_VOLUME).join(rest));
            }
            if real != path {
                self.denied.push(path);
            }
            self.denied.push(real);
        }
        Ok(self)
    }

    /// Whether the roots are not enforced (`--unscoped`).
    pub fn is_unscoped(&self) -> bool {
        self.unscoped
    }

    /// The working directory (relative paths start here).
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Whether `resolved` (a path already made real) is in a denied place. Used by
    /// [`Scope::resolve`] and by the directory walks of `Glob` and `Grep`.
    pub fn is_denied(&self, resolved: &Path) -> bool {
        self.is_denied_by_path(resolved)
            || (!self.denied_ids.is_empty()
                && resolved.ancestors().any(|ancestor| {
                    std::fs::metadata(ancestor)
                        .ok()
                        .as_ref()
                        .and_then(file_id)
                        .is_some_and(|id| self.denied_ids.contains(&id))
                }))
    }

    /// The deny check of one entry of a directory walk (`Glob`, `Grep`): its path, and its own
    /// identity when `metadata` is given. Its ancestors need no check: the walk started from a
    /// resolved, allowed root and never enters a denied directory.
    pub(crate) fn is_denied_entry(
        &self,
        path: &Path,
        metadata: Option<&std::fs::Metadata>,
    ) -> bool {
        self.is_denied_by_path(path)
            || metadata
                .and_then(file_id)
                .is_some_and(|id| self.denied_ids.contains(&id))
    }

    /// Whether a walk must read each entry's identity (some denied place existed at launch).
    pub(crate) fn denies_by_identity(&self) -> bool {
        !self.denied_ids.is_empty()
    }

    fn is_denied_by_path(&self, path: &Path) -> bool {
        is_process_environment(path) || self.denied.iter().any(|dir| under(path, dir))
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
        // Before anything touches the filesystem: resolving a share is already the leak.
        if !is_local(written) && self.local_only() {
            return Err(ScopeError::NotLocal(path.to_owned()));
        }
        let base = if written.is_absolute() {
            PathBuf::new()
        } else {
            self.cwd.clone()
        };
        let resolved = follow(base, written);
        // A symlink or a junction may itself lead to a share.
        if !is_local(&resolved) && self.local_only() {
            return Err(ScopeError::NotLocal(path.to_owned()));
        }
        if self.is_denied(&resolved) {
            return Err(ScopeError::Denied(path.to_owned()));
        }
        if self.unscoped || self.roots.iter().any(|root| resolved.starts_with(root)) {
            Ok(resolved)
        } else {
            Err(ScopeError::Outside(path.to_owned()))
        }
    }

    /// Network and device paths are refused: always when unscoped; when bounded, unless a root
    /// is itself remote (a session launched on a share), where the roots decide as before.
    fn local_only(&self) -> bool {
        self.unscoped || self.roots.iter().all(|root| is_local(root))
    }
}

/// Where macOS mounts the data volume: `/System/Volumes/Data/Users` is `/Users` (a firmlink).
const MACOS_DATA_VOLUME: &str = "/System/Volumes/Data";

/// A file's identity on its device: `(st_dev, st_ino)`.
type FileId = (u64, u64);

/// The identity of the file `metadata` describes; `None` where `std` gives no stable one.
#[cfg(unix)]
fn file_id(metadata: &std::fs::Metadata) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

/// See the Unix version: Windows has no stable file id in `std`.
#[cfg(not(unix))]
fn file_id(_metadata: &std::fs::Metadata) -> Option<FileId> {
    None
}

/// `written` resolved against the real filesystem from `base`, component by component.
fn follow(base: PathBuf, written: &Path) -> PathBuf {
    let mut resolved = base;
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
    resolved
}

/// A path on a local disk: no prefix (Unix, or a rooted Windows path), a drive letter, or the
/// verbatim form of a drive letter (`\\?\C:\`, what `canonicalize` returns on Windows). A UNC
/// share (`\\host\share`, `\\?\UNC\host\share`), a device (`\\.\pipe\x`) and any other
/// verbatim prefix (`\\?\GLOBALROOT`) are not. Neither is the NT object-namespace spelling
/// `\??\UNC\host\share` on Windows: `std` parses it as a root then a component named `??`,
/// but the layers below may hand it to the object manager as a share.
pub(crate) fn is_local(path: &Path) -> bool {
    if cfg!(windows) {
        let mut components = path.components();
        if components.next() == Some(Component::RootDir)
            && components.next() == Some(Component::Normal("??".as_ref()))
        {
            return false;
        }
    }
    match path.components().next() {
        Some(Component::Prefix(prefix)) => {
            matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
        },
        _ => true,
    }
}

/// `/proc/<anything>/…/environ`: the environment of a process (the server's parent's
/// included), which would hand over secrets the harness keeps out of the shell's environment.
fn is_process_environment(path: &Path) -> bool {
    let mut components = path.components();
    components.next() == Some(Component::RootDir)
        && components.next() == Some(Component::Normal("proc".as_ref()))
        && path.file_name() == Some("environ".as_ref())
}

/// Whether `path` is `dir` or below it. On the file systems that ignore case by default
/// (macOS, Windows) the comparison ignores case too: `~/.SSH` is `~/.ssh` there, and a part of
/// the path that does not exist yet keeps the case it was written in.
fn under(path: &Path, dir: &Path) -> bool {
    if path.starts_with(dir) {
        return true;
    }
    if !cfg!(any(target_os = "macos", windows)) {
        return false;
    }
    let mut components = path.components();
    dir.components().all(|expected| {
        components.next().is_some_and(|actual| {
            actual.as_os_str().to_string_lossy().to_lowercase()
                == expected.as_os_str().to_string_lossy().to_lowercase()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_environment_is_recognised_at_any_depth_of_proc() {
        for denied in [
            "/proc/1/environ",
            "/proc/self/environ",
            "/proc/42/task/43/environ",
        ] {
            assert!(is_process_environment(Path::new(denied)), "{denied}");
        }
        for allowed in [
            "/proc/1/status",
            "/home/u/environ",
            "/environ",
            "proc/1/environ",
        ] {
            assert!(!is_process_environment(Path::new(allowed)), "{allowed}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn only_drive_paths_are_local_on_windows() {
        for local in [r"C:\work\x", r"\\?\C:\work\x", r"\work\x", "relative"] {
            assert!(is_local(Path::new(local)), "{local}");
        }
        for remote in [
            r"\\host\share\x",
            "//host/share/x",
            r"\\?\UNC\host\share\x",
            r"\\.\pipe\x",
            r"\\.\COM1",
            r"\\?\GLOBALROOT\Device\x",
            r"\??\UNC\host\share\x",
            "/??/UNC/host/share/x",
        ] {
            assert!(!is_local(Path::new(remote)), "{remote}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn an_unscoped_scope_refuses_a_share_or_a_device_before_touching_it() {
        let dir = std::env::temp_dir();
        let scope = Scope::new(&dir, []).unwrap().without_bounds();
        for remote in [
            r"\\host.invalid\share\x",
            "//host.invalid/share/x",
            r"\\?\UNC\host.invalid\share\x",
            r"\\.\pipe\x",
            r"\\?\GLOBALROOT\Device\x",
            r"\??\UNC\host.invalid\share\x",
        ] {
            assert_eq!(
                scope.resolve(remote),
                Err(ScopeError::NotLocal(remote.to_owned())),
                "{remote}"
            );
        }
    }

    #[test]
    fn a_relative_or_remote_deny_path_is_refused() {
        let dir = std::env::temp_dir();
        assert!(
            Scope::new(&dir, [])
                .unwrap()
                .with_denied([PathBuf::from("relative/dir")])
                .is_err()
        );
        #[cfg(windows)]
        assert!(
            Scope::new(&dir, [])
                .unwrap()
                .with_denied([PathBuf::from(r"\\host\share")])
                .is_err()
        );
    }
}
