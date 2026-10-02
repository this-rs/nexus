//! Writing a file and then executing it is a race, and it has been failing CI.
//!
//! `cargo test` runs tests as **threads of one process**. When any thread
//! spawns a child, the whole process is forked and the child inherits every
//! descriptor open at that instant — including a descriptor another thread has
//! open for *writing* on the fake CLI it is about to run. `O_CLOEXEC` closes
//! that descriptor at `exec`, not at `fork`, so for the width of the
//! fork→exec window the file still has a writer, and Linux answers the owner's
//! own `execve` with `ETXTBSY`: `Text file busy (os error 26)`.
//!
//! Measured on this repository: six failures in one hour across six distinct
//! tests of `claude_manager` and `interactive_session`, on ubuntu stable, beta
//! *and* nightly, always exactly one test out of ~480, and twice on `main`
//! itself. macOS is far more permissive, which is why it only ever shows in CI.
//!
//! WHAT DOES NOT FIX IT, so nobody retries these:
//!   * closing the handle earlier — it is already closed before the spawn;
//!   * `rename` into place, or a hard link — `ETXTBSY` is a property of the
//!     **inode**, not of the name;
//!   * a unique path per test — every test already has its own `tempdir`;
//!   * retrying the spawn, sleeping, `#[ignore]`, `--test-threads=1` — these
//!     make the window narrower or invisible and leave the defect in place.
//!
//! WHAT DOES: never let *this* process hold a writable descriptor on the file
//! it will execute. The body is first written to a plain **data** file, which is
//! only ever read, and a **child process** copies that data into the executable.
//! The copying descriptor lives and dies inside that child, so no concurrent
//! `fork` of *our* process can inherit it. `chmod(2)` needs no descriptor at
//! all, so the permission bits are safe to set from here.

use std::path::Path;

/// Create an executable file at `path` whose contents are `body`, without this
/// process ever holding a writable descriptor on it. See the module docs.
pub(crate) fn write_executable(path: &Path, body: &str) {
    // A data file, never executed, so `ETXTBSY` cannot apply to it.
    let data = path.with_extension("body");
    std::fs::write(&data, body).expect("write the fake CLI body");

    #[cfg(unix)]
    {
        // The child owns the only writable descriptor on `path`.
        let status = std::process::Command::new("/bin/cp")
            .arg(&data)
            .arg(path)
            .status()
            .expect("spawn /bin/cp to install the fake CLI");
        assert!(status.success(), "/bin/cp failed to install {path:?}");

        // `chmod(2)` takes a path, not a descriptor: nothing to inherit.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the fake CLI");
    }

    #[cfg(windows)]
    {
        // `ETXTBSY` is a Unix answer; Windows reports sharing violations under
        // different conditions and the plain copy has never failed there.
        std::fs::copy(&data, path).expect("install the fake CLI");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_written_file_runs_and_reports_what_it_was_given() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake.sh");
        write_executable(&script, "#!/bin/sh\necho ran-with \"$1\"\n");

        let out = std::process::Command::new(&script)
            .arg("an-argument")
            .output()
            .expect("the installed file must be executable");
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "ran-with an-argument",
            "the body must survive the copy intact"
        );
    }

    #[test]
    fn the_data_file_sits_next_to_the_executable_and_is_not_executable() {
        // Pins the mechanism, not just the outcome: if someone makes the body
        // file *be* the executable, this fails and the race is back.
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake.sh");
        write_executable(&script, "#!/bin/sh\nexit 0\n");

        let data = script.with_extension("body");
        assert!(data.exists(), "the body is written as a separate data file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&data)
                .expect("stat the body")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o111,
                0,
                "the body file must not be executable: {mode:o}"
            );
        }
    }

    #[test]
    fn many_threads_writing_and_running_their_own_fakes_do_not_collide() {
        // The regression test proper. Under the old code this is the shape that
        // produced `ETXTBSY` in CI; it cannot reproduce it on macOS, so it is
        // here to exercise the path under load on every platform, Linux
        // included, where it is the one that matters.
        let handles: Vec<_> = (0..24)
            .map(|i| {
                std::thread::spawn(move || {
                    let dir = tempfile::tempdir().expect("tempdir");
                    let script = dir.path().join("fake.sh");
                    write_executable(&script, &format!("#!/bin/sh\necho {i}\n"));
                    let out = std::process::Command::new(&script)
                        .output()
                        .unwrap_or_else(|e| panic!("thread {i} could not spawn its fake: {e}"));
                    assert!(out.status.success());
                    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), i.to_string());
                })
            })
            .collect();
        for h in handles {
            h.join().expect("no thread may fail to spawn its own fake");
        }
    }
}
