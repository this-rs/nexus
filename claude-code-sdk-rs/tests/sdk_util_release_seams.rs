//! The `CC_SDK_TEST_*` environment overrides used by `cli_download`'s unit
//! tests exist only in the library's own test build (`#[cfg(test)]`).
//!
//! An integration test links the library exactly like a dependent crate does —
//! without `cfg(test)` — so this file is where that claim can actually be
//! checked: the overrides must be *inert* here, and the cache directory must
//! stay the user's real one.

use serial_test::serial;

/// SAFETY: both tests in this file are `#[serial]`, they run in their own test
/// binary, and each restores nothing because the value is only ever set (never
/// read by anything else in this process).
fn set_override(value: &str) {
    unsafe { std::env::set_var("CC_SDK_TEST_CACHE_DIR", value) };
}

fn clear_override() {
    unsafe { std::env::remove_var("CC_SDK_TEST_CACHE_DIR") };
}

#[test]
#[serial]
fn cache_dir_ignores_the_unit_test_override() {
    let scratch = tempfile::tempdir().expect("tempdir");
    set_override(scratch.path().to_str().expect("utf-8 tempdir"));

    let resolved = nexus_claude::cli_download::get_cache_dir()
        .expect("a cache directory must always be resolvable");

    clear_override();

    assert_ne!(
        resolved,
        scratch.path(),
        "CC_SDK_TEST_CACHE_DIR must not be honoured outside the library's own test build"
    );
    assert!(
        resolved.ends_with("cc-sdk/cli") || resolved.ends_with("cc-sdk\\cli"),
        "the real cache directory is expected, got {}",
        resolved.display()
    );
}

#[test]
#[serial]
fn cached_cli_path_ignores_the_unit_test_override() {
    let scratch = tempfile::tempdir().expect("tempdir");
    set_override(scratch.path().to_str().expect("utf-8 tempdir"));

    let resolved = nexus_claude::cli_download::get_cached_cli_path()
        .expect("a cached CLI path must always be resolvable");

    clear_override();

    assert!(
        !resolved.starts_with(scratch.path()),
        "the override leaked into a non-test build: {}",
        resolved.display()
    );
    let expected_name = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    assert_eq!(
        resolved.file_name().and_then(|n| n.to_str()),
        Some(expected_name)
    );
}
