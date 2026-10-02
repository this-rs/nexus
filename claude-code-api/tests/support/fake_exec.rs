//! Planting the executable a test points `claude.command` at.
//!
//! # Why not just write the file
//!
//! The obvious shape — `File::create`, `write_all`, `drop`, `chmod 0755`, then
//! `Command::new(path).spawn()` — is a race under Linux, and it is the race that
//! made `cargo test` fail with `Text file busy (os error 26)` on unrelated pull
//! requests. Closing the handle first does not fix it, and neither does writing
//! to a temporary name and renaming it into place.
//!
//! `cargo test` runs the tests of one binary as **threads of a single process**.
//! `Command::spawn` forks that whole process, so the child inherits every
//! descriptor the parent had open at the instant of the fork — including a write
//! handle another thread opened a microsecond earlier on *its own*, completely
//! unrelated, fake CLI. `O_CLOEXEC` (which Rust sets) closes that inherited
//! descriptor at `execve`, **not** at `fork`: in between, the child holds it. A
//! file's write count is tracked on the *inode*, by the open file description,
//! and `fork` only bumps its reference count. So while that child is on its way
//! to `execve`, the owning thread's own `execve` of its script is refused with
//! `ETXTBSY` — even though the owning thread closed its handle, chmodded the
//! file and never shared the path with anyone.
//!
//! Renaming cannot help: the race is on the inode, and a rename keeps it. A
//! per-test path cannot help either; every site already had one.
//!
//! # What is done instead
//!
//! Never execute an inode this process has opened for writing. The executed file
//! is `tests/support/exec_shim.sh`, checked into the repository and
//! therefore written by `git`, long before any test thread exists. A test plants
//! a **symlink** to it (creating a symlink opens nothing) and writes the script
//! it actually wants as a plain data file beside it. The shim `exec`s that data
//! file through `/bin/sh`, which only *reads* it — `ETXTBSY` is raised when
//! opening a file for execution, so a script that is read as data is immune.
//!
//! Windows needs none of this: it has no `fork`, so no sibling thread can
//! capture a handle, and it has no `ETXTBSY`. There the batch file is written
//! straight to its final name, exactly as before.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Name of the planted executable. Only the extension matters (`cmd.exe` needs
/// one); the tests never look at it.
const FAKE_CLI: &str = if cfg!(windows) {
    "fake-cli.cmd"
} else {
    "fake-cli"
};

/// Plant `body` as the fake `claude` executable inside `dir` and return the path
/// to feed to `settings.claude.command` / `ClaudeManager::new`.
///
/// `body` is a `/bin/sh` script on unix and a `cmd.exe` batch file on Windows;
/// callers build it with `cfg!(windows)` as they always have.
pub fn plant_fake_cli(dir: &Path, body: &str) -> PathBuf {
    plant_executable(&dir.join(FAKE_CLI), body)
}

/// Same, but at an exact path — for a test that needs the executable to carry a
/// particular name (a fake `npm`, a planted `claude` on a sandboxed `PATH`).
pub fn plant_executable(target: &Path, body: &str) -> PathBuf {
    #[cfg(unix)]
    {
        // The data file first: if the symlink exists and the spec does not, a
        // concurrently started spawn would run a missing script.
        std::fs::write(spec_path(target), body).expect("write the fake CLI spec");
        std::os::unix::fs::symlink(shim(), target).expect("symlink the fake CLI to the shim");
    }
    #[cfg(not(unix))]
    {
        std::fs::write(target, body).expect("write the fake CLI batch file");
    }
    target.to_path_buf()
}

/// Where [`plant_executable`] writes the script that `target` runs.
#[cfg(unix)]
pub fn spec_path(target: &Path) -> PathBuf {
    let mut path = target.as_os_str().to_owned();
    path.push(".spec.sh");
    PathBuf::from(path)
}

/// The checked-in shim. `CARGO_MANIFEST_DIR` is set for the lib's unit-test
/// target and for every integration test, so this resolves in both.
#[cfg(unix)]
fn shim() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/exec_shim.sh"
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The guarantee this module exists for: the file that gets executed is not
    /// one this process wrote. If this regresses, `ETXTBSY` comes back.
    #[test]
    fn the_executed_path_is_a_symlink_to_the_checked_in_shim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = plant_fake_cli(dir.path(), "#!/bin/sh\nexit 0\n");

        let link = std::fs::symlink_metadata(&cli).expect("the planted path exists");
        assert!(
            link.file_type().is_symlink(),
            "the executed path must be a symlink, not a file this process wrote"
        );
        assert_eq!(
            std::fs::read_link(&cli).expect("read_link"),
            shim(),
            "the symlink must point at the checked-in shim"
        );
        assert!(
            shim().exists(),
            "the shim must be checked in at {}",
            shim().display()
        );
    }

    /// The shim is transparent: argv, stdout and the exit status are the spec's.
    #[test]
    fn the_shim_forwards_argv_stdout_and_the_exit_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = plant_fake_cli(dir.path(), "#!/bin/sh\necho \"args:$*\"\nexit 7\n");

        let out = std::process::Command::new(&cli)
            .arg("--print")
            .arg("--output-format")
            .output()
            .expect("the planted CLI must spawn");

        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "args:--print --output-format\n"
        );
        assert_eq!(out.status.code(), Some(7));
    }
}
