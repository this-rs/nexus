//! The single process launcher (contract §11, decision A33).
//!
//! A provider process must not inherit the host's environment: the orchestrator
//! runs with database passwords and signing keys in its own, and a child that
//! inherits them hands them to whatever the model decides to run. Every provider
//! adapter therefore builds its `Command` here — [`isolated_command`] starts from
//! an **empty** environment and adds back an explicit allowlist — and a guard
//! test forbids `Command::new` under `src/providers/`.
//!
//! Secrets that a CLI wants as a configuration blob (MCP servers and their
//! credentials) go through [`SecretFile`]: a `0600` file in a `0700` directory,
//! passed by path and deleted when the session closes, instead of a command-line
//! argument any local user can read with `ps`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use tokio::process::Command;

/// Variables every provider process may inherit: what a program needs to find
/// executables, a home, a locale, a temp dir, certificates and a proxy. Nothing
/// credential-shaped.
pub const BASE_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TMPDIR",
    "TZ",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    // Windows: a process without these cannot load system DLLs or find its profile.
    "SYSTEMROOT",
    "SystemRoot",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "USERPROFILE",
    "USERNAME",
    "HOMEDRIVE",
    "HOMEPATH",
    "TEMP",
    "TMP",
    "Path",
];

/// Name prefixes the Claude Code CLI needs on top of the base list: its own
/// settings and the Anthropic credentials it authenticates with.
pub const CLAUDE_CODE_ENV_PREFIXES: &[&str] = &["ANTHROPIC_", "CLAUDE_"];

/// What a child process inherits from the host's environment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum EnvPolicy {
    /// Inherit everything. The historical behaviour of `ClaudeCodeOptions`, kept
    /// as its default so existing callers are not changed behind their back. Not
    /// available to the provider adapters.
    #[default]
    InheritAll,
    /// Start from an empty environment and inherit only [`BASE_ENV_ALLOWLIST`]
    /// plus what is listed here.
    Allowlist {
        /// Extra variable names inherited from the host.
        inherit: Vec<String>,
        /// Variables inherited when their name starts with one of these prefixes.
        inherit_prefixes: Vec<String>,
        /// Replaces `HOME` (and `USERPROFILE`) with a dedicated directory, so the
        /// provider cannot read the host user's dotfiles and credentials.
        home: Option<PathBuf>,
    },
}

impl EnvPolicy {
    /// Clean environment with the base allowlist only.
    pub fn allowlist() -> Self {
        Self::Allowlist {
            inherit: Vec::new(),
            inherit_prefixes: Vec::new(),
            home: None,
        }
    }

    /// Clean environment for the Claude Code CLI: the base allowlist plus the
    /// `ANTHROPIC_*` and `CLAUDE_*` variables. `HOME` stays the real one — the
    /// CLI's login lives there.
    pub fn claude_code() -> Self {
        Self::Allowlist {
            inherit: Vec::new(),
            inherit_prefixes: CLAUDE_CODE_ENV_PREFIXES
                .iter()
                .map(|prefix| (*prefix).to_string())
                .collect(),
            home: None,
        }
    }

    /// Adds variable names to inherit. No effect on [`EnvPolicy::InheritAll`].
    pub fn with_inherited<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if let Self::Allowlist { inherit, .. } = &mut self {
            inherit.extend(names.into_iter().map(Into::into));
        }
        self
    }

    /// Gives the child a dedicated home directory. No effect on [`EnvPolicy::InheritAll`].
    pub fn with_home(mut self, dir: impl Into<PathBuf>) -> Self {
        if let Self::Allowlist { home, .. } = &mut self {
            *home = Some(dir.into());
        }
        self
    }

    /// Whether the child starts from an empty environment.
    pub fn is_isolated(&self) -> bool {
        matches!(self, Self::Allowlist { .. })
    }

    /// Whether this policy lets the host variable `name` through.
    pub fn allows(&self, name: &str) -> bool {
        match self {
            Self::InheritAll => true,
            Self::Allowlist {
                inherit,
                inherit_prefixes,
                ..
            } => {
                BASE_ENV_ALLOWLIST.contains(&name)
                    || inherit.iter().any(|allowed| allowed == name)
                    || inherit_prefixes
                        .iter()
                        .any(|prefix| !prefix.is_empty() && name.starts_with(prefix.as_str()))
            },
        }
    }
}

/// Applies `policy` to a command **before** any explicit `cmd.env(..)`:
/// `env_clear` also drops variables already set on the command.
pub fn apply_env_policy(cmd: &mut Command, policy: &EnvPolicy) {
    apply_env_policy_from(cmd, policy, std::env::vars_os());
}

/// [`apply_env_policy`] over an explicit host environment (for tests).
pub(crate) fn apply_env_policy_from<I>(cmd: &mut Command, policy: &EnvPolicy, host_env: I)
where
    I: IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
{
    let EnvPolicy::Allowlist { home, .. } = policy else {
        return;
    };
    cmd.env_clear();
    for (name, value) in host_env {
        // A name that is not UTF-8 cannot be on the allowlist: drop it.
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if policy.allows(name_str) {
            cmd.env(&name, &value);
        }
    }
    if let Some(home) = home {
        cmd.env("HOME", home);
        cmd.env("USERPROFILE", home);
    }
}

/// Builds the command every provider adapter starts from: `program` with an
/// environment reduced by `policy`. Add arguments and explicit variables afterwards.
pub fn isolated_command(program: impl AsRef<OsStr>, policy: &EnvPolicy) -> Command {
    let mut cmd = Command::new(program);
    apply_env_policy(&mut cmd, policy);
    cmd
}

/// A file holding a secret-bearing configuration, readable by its owner only and
/// deleted on drop.
///
/// The file lives alone in a fresh `0700` directory, so the directory can be
/// removed with it and no other user can list or replace the file.
#[derive(Debug)]
pub struct SecretFile {
    dir: PathBuf,
    path: PathBuf,
}

impl SecretFile {
    /// Writes `contents` to a new owner-only file named `file_name` under the
    /// system temp dir (or `TMPDIR`).
    pub fn create(file_name: &str, contents: &[u8]) -> std::io::Result<Self> {
        Self::create_in(&std::env::temp_dir(), file_name, contents)
    }

    /// Same as [`SecretFile::create`], under `parent`.
    pub fn create_in(parent: &Path, file_name: &str, contents: &[u8]) -> std::io::Result<Self> {
        let dir = parent.join(format!("nexus-{}", uuid::Uuid::new_v4().simple()));
        create_private_dir(&dir)?;
        let path = dir.join(file_name);
        let written = write_private_file(&path, contents);
        if let Err(error) = written {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(error);
        }
        Ok(Self { dir, path })
    }

    /// Path to hand to the child process.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SecretFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir(dir)
}

#[cfg(unix)]
fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // `create_new`: never follow or reuse something already at that path.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;

    fn host() -> Vec<(OsString, OsString)> {
        [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/host"),
            ("LANG", "C.UTF-8"),
            ("NEO4J_PASSWORD", "db-secret"),
            ("PO_JWT_SECRET", "signing-secret"),
            ("ANTHROPIC_API_KEY", "anthropic-secret"),
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("AWS_SECRET_ACCESS_KEY", "aws-secret"),
            ("OPENAI_API_KEY", "openai-secret"),
            ("MY_TOOLCHAIN", "stable"),
        ]
        .into_iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect()
    }

    fn envs_after(policy: &EnvPolicy) -> HashMap<String, String> {
        let mut cmd = Command::new("/nonexistent/provider");
        // Set before the policy on purpose: it must not survive `env_clear`.
        cmd.env("SET_TOO_EARLY", "x");
        apply_env_policy_from(&mut cmd, policy, host());
        cmd.as_std()
            .get_envs()
            .filter_map(|(name, value)| {
                Some((
                    name.to_string_lossy().into_owned(),
                    value?.to_string_lossy().into_owned(),
                ))
            })
            .collect()
    }

    #[test]
    fn base_allowlist_keeps_what_a_program_needs_and_nothing_secret() {
        let envs = envs_after(&EnvPolicy::allowlist());
        let mut names: Vec<&str> = envs.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["HOME", "LANG", "PATH"]);
    }

    #[test]
    fn claude_code_policy_adds_only_its_two_prefixes() {
        let envs = envs_after(&EnvPolicy::claude_code());
        let mut names: Vec<&str> = envs.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "ANTHROPIC_API_KEY",
                "CLAUDE_CODE_USE_BEDROCK",
                "HOME",
                "LANG",
                "PATH"
            ]
        );
        assert_eq!(envs["HOME"], "/home/host", "Claude keeps the real home");
    }

    #[test]
    fn named_inheritance_and_dedicated_home() {
        let policy = EnvPolicy::allowlist()
            .with_inherited(["MY_TOOLCHAIN"])
            .with_home("/var/lib/po/providers/codex-1");
        let envs = envs_after(&policy);
        assert_eq!(envs.get("MY_TOOLCHAIN").map(String::as_str), Some("stable"));
        assert_eq!(envs["HOME"], "/var/lib/po/providers/codex-1");
        assert_eq!(envs["USERPROFILE"], "/var/lib/po/providers/codex-1");
        assert!(!envs.contains_key("OPENAI_API_KEY"));
        assert!(!envs.contains_key("NEO4J_PASSWORD"));
    }

    #[test]
    fn inherit_all_touches_nothing() {
        let envs = envs_after(&EnvPolicy::InheritAll);
        assert_eq!(envs.len(), 1, "only what the caller set explicitly");
        assert!(envs.contains_key("SET_TOO_EARLY"));
        assert!(EnvPolicy::InheritAll.allows("NEO4J_PASSWORD"));
        assert!(!EnvPolicy::InheritAll.is_isolated());
        // The builders are no-ops on InheritAll rather than a silent upgrade.
        assert_eq!(
            EnvPolicy::InheritAll.with_inherited(["X"]).with_home("/h"),
            EnvPolicy::InheritAll
        );
    }

    #[test]
    fn an_empty_prefix_does_not_open_everything() {
        let policy = EnvPolicy::Allowlist {
            inherit: Vec::new(),
            inherit_prefixes: vec![String::new()],
            home: None,
        };
        assert!(!policy.allows("NEO4J_PASSWORD"));
        assert!(policy.allows("PATH"));
    }

    #[test]
    fn secret_file_is_private_and_removed_on_drop() {
        let parent = tempfile::tempdir().unwrap();
        let file = SecretFile::create_in(parent.path(), "mcp.json", b"{\"token\":\"t\"}").unwrap();
        let path = file.path().to_path_buf();
        let dir = path.parent().unwrap().to_path_buf();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"token\":\"t\"}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600);
            assert_eq!(dir_mode, 0o700);
        }
        drop(file);
        assert!(!path.exists());
        assert!(!dir.exists());
    }
}
