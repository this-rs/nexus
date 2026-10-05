//! Starting a command in its own process group, with a clean environment, and ending the
//! whole group (N20).
//!
//! The group is what makes "stop" mean stop: a shell that starts `sleep` or a compiler has
//! children that outlive it if only the shell is killed.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

/// What the child may see of the host environment.
#[derive(Debug, Clone)]
pub struct EnvPolicy {
    /// Host variables passed through by name. Everything else is dropped.
    pub inherit: Vec<String>,
    /// Variables set explicitly.
    pub set: Vec<(String, String)>,
}

impl Default for EnvPolicy {
    fn default() -> Self {
        Self {
            inherit: ["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TZ"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            set: Vec::new(),
        }
    }
}

impl EnvPolicy {
    /// The environment the child gets: the allow-listed host variables that are set, then
    /// the explicit ones, then `HOME` (always a directory of its own, never the user's).
    pub fn build(
        &self,
        host: impl Iterator<Item = (String, String)>,
        home: &Path,
    ) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = host
            .filter(|(name, _)| self.inherit.iter().any(|n| n == name))
            .collect();
        for (name, value) in &self.set {
            out.retain(|(n, _)| n != name);
            out.push((name.clone(), value.clone()));
        }
        out.retain(|(n, _)| n != "HOME");
        out.push(("HOME".into(), home.display().to_string()));
        out
    }
}

/// The shell that runs commands: bash when there is one, else the POSIX shell.
pub fn default_shell() -> PathBuf {
    ["/bin/bash", "/usr/bin/bash", "/bin/sh"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// A running command: its child and the id of its process group.
pub struct Running {
    /// The child (the group leader).
    pub child: Child,
    /// Process-group id (equal to the child's pid).
    pub pgid: u32,
}

/// Starts `script` in a new process group. Output (both streams) goes to `output`.
///
/// The command text is handed over in an environment variable that the wrapper unsets
/// before running it, so it is never part of the command line (`ps`) and never quoted into
/// shell source.
pub fn spawn(
    shell: &Path,
    command: &str,
    cwd: &Path,
    env: &[(String, String)],
    output: &std::fs::File,
    cwd_file: Option<&Path>,
) -> std::io::Result<Running> {
    let wrapper = if cwd_file.is_some() {
        "__nx_c=$NEXUS_TOOLS_CMD; __nx_f=$NEXUS_TOOLS_CWDFILE; unset NEXUS_TOOLS_CMD NEXUS_TOOLS_CWDFILE; \
         eval \"$__nx_c\"; __nx_e=$?; pwd -P >\"$__nx_f\"; exit $__nx_e"
    } else {
        "__nx_c=$NEXUS_TOOLS_CMD; unset NEXUS_TOOLS_CMD; eval \"$__nx_c\""
    };
    let mut cmd = Command::new(shell);
    cmd.arg("-c")
        .arg(wrapper)
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .env("NEXUS_TOOLS_CMD", command)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output.try_clone()?))
        .process_group(0)
        .kill_on_drop(false);
    if let Some(file) = cwd_file {
        cmd.env("NEXUS_TOOLS_CWDFILE", file);
    }
    let child = cmd.spawn()?;
    let pgid = child
        .id()
        .ok_or_else(|| std::io::Error::other("the command exited at once"))?;
    Ok(Running { child, pgid })
}

fn signal_group(pgid: u32, signal: nix::sys::signal::Signal) {
    let Ok(raw) = i32::try_from(pgid) else { return };
    // The group may already be gone: that is the goal.
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(raw), signal);
}

/// Asks the group to stop (SIGTERM), then, after `grace`, makes it (SIGKILL). Descendants
/// that ignore SIGTERM are killed too: the second signal goes to the whole group, even when
/// its leader has already exited.
pub async fn terminate_group(pgid: u32, grace: Duration) {
    signal_group(pgid, nix::sys::signal::Signal::SIGTERM);
    tokio::time::sleep(grace).await;
    signal_group(pgid, nix::sys::signal::Signal::SIGKILL);
}

/// Kills the group at once, without waiting. Safe to call from `Drop`.
pub fn kill_group_now(pgid: u32) {
    signal_group(pgid, nix::sys::signal::Signal::SIGKILL);
}

/// Kills its group when dropped, unless disarmed: a call that is abandoned (the request was
/// cancelled, the session ended) must not leave its command running.
pub struct GroupGuard {
    pgid: Option<u32>,
}

impl GroupGuard {
    /// Guards `pgid`.
    pub fn new(pgid: u32) -> Self {
        Self { pgid: Some(pgid) }
    }

    /// The command finished by itself: nothing to kill.
    pub fn disarm(&mut self) {
        self.pgid = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid {
            kill_group_now(pgid);
        }
    }
}
