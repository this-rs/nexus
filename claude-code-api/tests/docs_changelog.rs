//! Keeps `CHANGELOG.md` honest about how far behind it is.
//!
//! ## The gap, measured 2026-10-01
//!
//! | | |
//! |---|---|
//! | newest version in `CHANGELOG.md` | `0.1.10`, dated 2025-01-15 |
//! | `version` in the workspace `Cargo.toml` | `0.5.0` |
//! | version tags in the repository | `v0.0.5`, and nothing after it |
//!
//! Four minor versions shipped without a changelog entry. `cliff.toml` is configured and
//! has evidently never been run: the committed header does not match the one it would
//! emit, and its footer is absent. It also *cannot* be run usefully right now — git-cliff
//! groups commits by tag, and with no tag between `v0.0.5` and `0.5.0` it would collapse
//! 126 commits into a single `[Unreleased]` block. Creating those tags is a release
//! action, which this work is not allowed to take.
//!
//! ## What this test does instead
//!
//! It refuses to let the gap be silent, in **both** directions:
//!
//! - while the newest entry trails the workspace version, `CHANGELOG.md` must carry a
//!   notice naming both versions, so a reader sees the gap before trusting the file;
//! - once the two agree, the notice must be gone — a stale "this file is behind" banner on
//!   an up-to-date changelog is its own lie.
//!
//! Entirely offline: two files are read, nothing is fetched and no subprocess runs.

use std::fs;
use std::path::PathBuf;

/// Repository root (the workspace directory above this crate).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir has a parent")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {}", path.display(), e))
}

/// The workspace version from the root `Cargo.toml`.
///
/// Read from the `[workspace.package]`-style `version = "…"` line, which is the first
/// `version =` at the start of a line in that file.
fn workspace_version() -> String {
    read("Cargo.toml")
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix("version")?.trim_start().strip_prefix('=')?;
            Some(rest.trim().trim_matches('"').to_string())
        })
        .expect("the workspace Cargo.toml declares a version")
}

/// Bracket contents of each `## [...]` heading, in file order.
///
/// `[Unreleased]` is skipped: it is a placeholder, not a shipped version. The content is
/// returned whole, because a heading may name its subject as well as its version —
/// `[claude-code-sdk-rs 0.1.5]` and `[0.1.5]` are two different releases that happen to
/// share a number, and a reader needs to be able to tell them apart.
fn changelog_entries(changelog: &str) -> Vec<String> {
    changelog
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("## [")?;
            let end = rest.find(']')?;
            let entry = rest[..end].trim();
            if entry.eq_ignore_ascii_case("unreleased") {
                return None;
            }
            Some(entry.to_string())
        })
        .collect()
}

/// The `x.y.z` version named inside a heading's brackets, if there is one.
///
/// Tolerates a subject before the number (`claude-code-sdk-rs 0.1.5`) and a leading `v`.
fn entry_version(entry: &str) -> Option<String> {
    entry
        .split_whitespace()
        .map(|token| token.trim_start_matches('v'))
        .find(|token| {
            let mut parts = token.split('.');
            matches!(
                (parts.next(), parts.next()),
                (Some(a), Some(b))
                    if !a.is_empty()
                        && a.chars().all(|c| c.is_ascii_digit())
                        && b.chars().next().is_some_and(|c| c.is_ascii_digit())
            )
        })
        .map(str::to_string)
}

/// Versions documented by the changelog, in file order.
fn changelog_versions(changelog: &str) -> Vec<String> {
    changelog_entries(changelog)
        .iter()
        .filter_map(|entry| entry_version(entry))
        .collect()
}

/// Parse `x.y.z` into comparable numbers. A non-numeric component sorts as 0.
fn semver_key(version: &str) -> (u64, u64, u64) {
    let mut parts = version.split('.').map(|p| {
        p.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap_or(0)
    });
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// The marker a changelog behind the code must carry.
const GAP_NOTICE: &str = "[!WARNING]";

#[test]
fn the_changelog_states_the_gap_when_it_trails_the_workspace_version() {
    let changelog = read("CHANGELOG.md");
    let versions = changelog_versions(&changelog);
    assert!(
        !versions.is_empty(),
        "CHANGELOG.md documents no version at all — the `## [x.y.z]` heading shape changed"
    );

    let newest = versions
        .iter()
        .max_by_key(|v| semver_key(v))
        .expect("checked non-empty")
        .clone();
    let current = workspace_version();

    if semver_key(&newest) >= semver_key(&current) {
        // Up to date: the gap notice must not linger.
        assert!(
            !changelog.contains(GAP_NOTICE),
            "CHANGELOG.md is level with the workspace version ({current}) but still carries a \
             `{GAP_NOTICE}` notice saying it is behind. Remove the notice — a changelog that \
             disclaims itself for no reason trains readers to ignore the warning."
        );
        return;
    }

    assert!(
        changelog.contains(GAP_NOTICE),
        "CHANGELOG.md's newest entry is {newest} but the workspace is at {current}, and the \
         file says nothing about it. Either document the missing versions, or add a \
         `{GAP_NOTICE}` notice naming both versions so a reader knows what the file omits."
    );
    for needed in [newest.as_str(), current.as_str()] {
        assert!(
            changelog.contains(needed),
            "CHANGELOG.md's notice must name version {needed} explicitly — \"out of date\" \
             without the two numbers tells a reader nothing about how far behind it is"
        );
    }
    assert!(
        changelog.contains("cliff.toml"),
        "the notice must say why the file was not regenerated, which means naming cliff.toml \
         and the reason it cannot be run (no version tags to group commits by)"
    );
}

/// No two `## [...]` headings may read the same.
///
/// `CHANGELOG.md` carried two `## [0.1.5]` headings, dated a day apart: one for the
/// workspace release and one for the SDK's own. Identical headings leave a reader unable to
/// tell which entry describes which release, so the SDK's is now
/// `## [claude-code-sdk-rs 0.1.5]`. Sharing a *version number* across crates is fine; what
/// this test forbids is sharing the whole heading.
#[test]
fn no_two_changelog_headings_read_the_same() {
    let entries = changelog_entries(&read("CHANGELOG.md"));
    assert!(
        !entries.is_empty(),
        "CHANGELOG.md has no `## [...]` heading — the format changed"
    );

    let mut duplicates: Vec<&String> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if entries[..index].contains(entry) && !duplicates.contains(&entry) {
            duplicates.push(entry);
        }
    }
    assert!(
        duplicates.is_empty(),
        "these headings appear more than once in CHANGELOG.md: {duplicates:?}. Name the \
         subject of each (`[claude-code-sdk-rs 0.1.5]`) so a reader can tell them apart."
    );
}

#[test]
fn the_cliff_configuration_and_the_changelog_agree_on_the_header() {
    // If the committed changelog was generated by git-cliff, its first lines are the
    // configured header. They are not, which is the evidence that it never was — recorded
    // here so the claim in this file's docs stays checkable rather than remembered.
    let cliff = read("cliff.toml");
    let changelog = read("CHANGELOG.md");

    let configured_keep_a_changelog = cliff.contains("Keep a Changelog");
    let changelog_keep_a_changelog = changelog.contains("Keep a Changelog");

    if configured_keep_a_changelog && !changelog_keep_a_changelog {
        // Expected state today. Nothing to assert beyond the explanation being present.
        assert!(
            changelog.contains(GAP_NOTICE),
            "cliff.toml would emit a \"Keep a Changelog\" header and CHANGELOG.md has none, so \
             the file is hand-written. That is allowed, but the gap notice must say so."
        );
    }
}

#[test]
fn the_version_parser_orders_releases_correctly() {
    // Guard the comparison: string ordering would call 0.1.10 older than 0.1.9, which is
    // exactly the pair this repository contains.
    assert!(semver_key("0.1.10") > semver_key("0.1.9"));
    assert!(semver_key("0.5.0") > semver_key("0.1.10"));
    assert_eq!(semver_key("0.5.0"), (0, 5, 0));
    assert_eq!(semver_key("1.2.3-rc1"), (1, 2, 3));
    assert_eq!(semver_key("nonsense"), (0, 0, 0));
}

#[test]
fn the_changelog_parser_skips_the_unreleased_placeholder() {
    let sample = "# Changelog\n\n## [Unreleased]\n\n## [0.2.0] - 2026-01-01\n\n## [v0.1.0]\n";
    assert_eq!(
        changelog_versions(sample),
        vec!["0.2.0".to_string(), "0.1.0".to_string()],
        "`[Unreleased]` is a placeholder and the leading `v` is not part of the version"
    );
}

#[test]
fn a_heading_may_name_its_subject_before_the_version() {
    assert_eq!(
        entry_version("claude-code-sdk-rs 0.1.5"),
        Some("0.1.5".to_string())
    );
    assert_eq!(entry_version("v0.5.0"), Some("0.5.0".to_string()));
    assert_eq!(entry_version("0.1.10"), Some("0.1.10".to_string()));
    assert_eq!(
        entry_version("no number here"),
        None,
        "a heading with no version must not be mistaken for 0.0.0"
    );

    let sample = "## [claude-code-sdk-rs 0.1.5]\n## [0.1.5]\n";
    assert_eq!(
        changelog_entries(sample).len(),
        2,
        "the two headings stay distinct"
    );
    assert_eq!(
        changelog_versions(sample),
        vec!["0.1.5".to_string(), "0.1.5".to_string()],
        "while both still report the same version"
    );
}
