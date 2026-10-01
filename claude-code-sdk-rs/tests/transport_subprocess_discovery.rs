//! How `SubprocessTransport` finds the `claude` binary when nothing points it
//! at one — `find_claude_cli`, and the two constructors that call it.
//!
//! Every test here rewrites `PATH` and `HOME` for the duration of the lookup,
//! which is process-global state: they are therefore **all** `#[serial]`, and
//! this file holds nothing else, so no test in this binary can observe a
//! half-rewritten environment. Nothing is ever spawned — only existence and the
//! executable bit are probed — so the files planted in the temp directories are
//! empty.
//!
//! Unix only: `dirs::home_dir()` is driven by `$HOME` there, whereas on Windows
//! it comes from a shell API that a test cannot redirect, and the location list
//! `find_claude_cli` scans is `#[cfg]`-selected to match.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use nexus_claude::transport::{SubprocessTransport, Transport};
use nexus_claude::{ClaudeCodeOptions, SdkError, find_claude_cli};
use serial_test::serial;
use tempfile::TempDir;

/// Whether two paths name the same file on disk.
///
/// `which` hands back a resolved path and the platform temp directory is a
/// symlink on macOS (`/var` -> `/private/var`), so comparing the strings would
/// fail for reasons that have nothing to do with the search.
fn same_file(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Restores `PATH` and `HOME` however the test ends, panic included.
struct EnvSandbox {
    previous: Vec<(&'static str, Option<String>)>,
    /// Kept alive so the directories below outlive the lookup.
    home: TempDir,
    bin: TempDir,
}

impl EnvSandbox {
    /// An empty `PATH` and a `HOME` with nothing in it: the state in which
    /// `find_claude_cli` can find nothing at all.
    fn barren() -> Self {
        let home = TempDir::new().expect("temp home");
        let bin = TempDir::new().expect("temp bin");
        let previous = ["PATH", "HOME"]
            .into_iter()
            .map(|name| (name, std::env::var(name).ok()))
            .collect();
        let sandbox = Self {
            previous,
            home,
            bin,
        };
        sandbox.put("PATH", &sandbox.bin.path().display().to_string());
        sandbox.put("HOME", &sandbox.home.path().display().to_string());
        sandbox
    }

    fn put(&self, name: &str, value: &str) {
        // SAFETY: every test in this file is `#[serial]` and this file contains
        // only such tests, so no other thread is reading the environment.
        unsafe { std::env::set_var(name, value) };
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    /// Plant an executable file, creating the parent directories.
    fn plant(&self, path: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, b"").expect("plant file");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("make executable");
        path.to_path_buf()
    }

    /// Plant an executable of that name on the sandboxed `PATH`.
    fn plant_on_path(&self, name: &str) -> PathBuf {
        self.plant(&self.bin.path().join(name))
    }
}

impl Drop for EnvSandbox {
    fn drop(&mut self) {
        for (name, value) in std::mem::take(&mut self.previous) {
            match value {
                // SAFETY: as in `put`.
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }
}

/// `PATH` comes first, and `claude-code` is accepted as well as `claude` — the
/// npm package has shipped under both names.
#[test]
#[serial]
fn the_path_is_searched_first_under_both_names() {
    let sandbox = EnvSandbox::barren();
    let planted = sandbox.plant_on_path("claude-code");
    assert!(same_file(
        &find_claude_cli().expect("claude-code on PATH is enough"),
        &planted
    ));

    // With both present, `claude` wins: it is first in the list.
    let preferred = sandbox.plant_on_path("claude");
    assert!(same_file(
        &find_claude_cli().expect("claude on PATH"),
        &preferred
    ));
}

/// Nothing on `PATH`, but the auto-download cache holds a binary: that is what
/// the search falls back to, before any of the hard-coded locations.
#[test]
#[serial]
fn the_sdk_download_cache_is_the_second_place_looked_at() {
    let sandbox = EnvSandbox::barren();
    let cached = nexus_claude::get_cached_cli_path()
        .expect("a cache path can be derived from the sandboxed HOME");
    assert!(
        cached.starts_with(sandbox.home()),
        "the cache must sit inside the sandboxed HOME, got {}",
        cached.display()
    );

    assert!(
        find_claude_cli().is_err(),
        "the cache path exists only as a name until something is planted there"
    );
    sandbox.plant(&cached);
    assert!(same_file(
        &find_claude_cli().expect("the cached CLI counts"),
        &cached
    ));
}

/// Last resort: the hard-coded list of install locations under `$HOME`.
#[test]
#[serial]
fn the_common_install_locations_are_scanned_last() {
    let sandbox = EnvSandbox::barren();
    let planted = sandbox.plant(&sandbox.home().join(".claude/local/claude"));
    assert!(same_file(
        &find_claude_cli().expect("a native install under ~/.claude counts"),
        &planted
    ));
}

/// A directory with the right name is not a CLI: the search requires a *file*,
/// otherwise it would hand back something that cannot be executed.
#[test]
#[serial]
fn a_directory_in_an_install_location_is_not_mistaken_for_the_cli() {
    let sandbox = EnvSandbox::barren();
    std::fs::create_dir_all(sandbox.home().join(".local/bin/claude")).expect("create dir");
    let err = find_claude_cli().expect_err("a directory is not an executable");
    assert!(matches!(err, SdkError::CliNotFound { .. }), "got {err:?}");
}

/// With no Node.js either, the diagnosis is about Node.js: installing the CLI
/// through npm cannot work, so saying "CLI not found" would send the reader down
/// the wrong path.
#[test]
#[serial]
fn without_node_the_failure_blames_node() {
    let _sandbox = EnvSandbox::barren();
    match find_claude_cli().expect_err("nothing to find") {
        SdkError::CliNotFound { searched_paths } => {
            assert!(
                searched_paths.contains("Node.js is not installed"),
                "got {searched_paths}"
            );
            assert!(
                searched_paths.contains("auto_download_cli"),
                "the message must offer the way out that needs no npm: {searched_paths}"
            );
        },
        other => panic!("expected CliNotFound, got {other:?}"),
    }
}

/// With Node.js present the diagnosis changes: npm is a usable route, so the
/// message names it and lists every path that was tried.
#[test]
#[serial]
fn with_node_the_failure_lists_the_searched_paths() {
    let sandbox = EnvSandbox::barren();
    sandbox.plant_on_path("node");

    match find_claude_cli().expect_err("node is not claude") {
        SdkError::CliNotFound { searched_paths } => {
            assert!(
                !searched_paths.contains("Node.js is not installed"),
                "node was found, so it must not be blamed: {searched_paths}"
            );
            assert!(
                searched_paths.contains("npm install -g @anthropic-ai/claude-code"),
                "got {searched_paths}"
            );
            assert!(
                searched_paths.contains(&sandbox.home().display().to_string()),
                "the list must show where it actually looked: {searched_paths}"
            );
        },
        other => panic!("expected CliNotFound, got {other:?}"),
    }
}

/// `new` and `new_async` both fall back to the same search, and both surface its
/// failure unchanged rather than deferring it to `connect`.
#[tokio::test]
#[serial]
async fn both_constructors_report_a_failed_search_immediately() {
    let _sandbox = EnvSandbox::barren();
    let options = ClaudeCodeOptions::default();

    assert!(matches!(
        SubprocessTransport::new(options.clone()),
        Err(SdkError::CliNotFound { .. })
    ));
    assert!(
        matches!(
            SubprocessTransport::new_async(options).await,
            Err(SdkError::CliNotFound { .. })
        ),
        "auto_download_cli is off, so new_async fails exactly like new"
    );
}

/// And when the search succeeds, that is the path the transport will spawn.
#[tokio::test]
#[serial]
async fn new_async_adopts_what_the_search_found() {
    let sandbox = EnvSandbox::barren();
    let planted = sandbox.plant_on_path("claude");
    let transport = SubprocessTransport::new_async(ClaudeCodeOptions::default())
        .await
        .expect("the planted CLI is found");
    assert!(!transport.is_connected(), "finding it does not start it");
    // The path is private; `for_print_mode` goes through the same search, and
    // `connect` would spawn — so assert on the search itself, which is what the
    // constructor delegates to.
    assert!(same_file(
        &find_claude_cli().expect("same search"),
        &planted
    ));
}

/// `for_print_mode` has the same fallback as `new`: no explicit path means the
/// search, and the search's failure is reported there and then.
#[test]
#[serial]
fn for_print_mode_falls_back_to_the_search() {
    let sandbox = EnvSandbox::barren();
    assert!(
        matches!(
            SubprocessTransport::for_print_mode(ClaudeCodeOptions::default(), "hi".into()),
            Err(SdkError::CliNotFound { .. })
        ),
        "with nothing to find, print mode fails immediately"
    );

    let planted = sandbox.plant_on_path("claude");
    SubprocessTransport::for_print_mode(ClaudeCodeOptions::default(), "hi".into())
        .expect("the planted CLI is found");
    assert!(same_file(
        &find_claude_cli().expect("same search"),
        &planted
    ));
}
