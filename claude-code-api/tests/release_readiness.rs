//! Release readiness gate (task 5.1).
//!
//! This gate **does not release anything**. It answers one question, offline
//! and reproducibly: *is this repository in a state where a release would be
//! honest?* The answer lives in `docs/RELEASE_READINESS.md`, and the whole
//! point of this file is to make that document impossible to let rot.
//!
//! # The central invariant
//!
//! [`collect_findings`] recomputes, from the repository files themselves, the
//! set of reasons this repository is not release-ready. The report must list
//! exactly those reasons — no more, no fewer. So:
//!
//! * a new drift appears → the gate fails until the report records it;
//! * a drift is fixed → the gate fails until the report stops claiming it.
//!
//! Either way the document and the repository cannot disagree. A report that
//! merely *looked* good is the failure mode this gate exists to prevent.
//!
//! # No network, ever
//!
//! Every check reads tracked files under [`repo_root`]. Nothing resolves a
//! host, opens a socket, or shells out. The gate therefore produces the same
//! verdict on a developer laptop, in CI, and on a plane.
//!
//! # Why the forbidden-reference check is phrased as an allowlist
//!
//! Private and machine-local services must not be referenced from a public
//! repository. Hard-coding their names *here* would publish them, which is the
//! very thing being prevented. So the rule is inverted: `docs/allowed-hosts.txt`
//! lists every host the repository may reference, and
//! [`foreign_hosts`] reports anything else. A leaked private host fails the
//! gate without ever being named in tracked code.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Repository access
// ---------------------------------------------------------------------------

/// Absolute path to the workspace root (the parent of this crate).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate directory always has a parent")
        .to_path_buf()
}

/// Read a repository-relative file, failing loudly if the layout moved.
fn read_repo_file(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("release readiness gate cannot read {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------

/// One reason the repository is not release-ready.
///
/// `id` is the stable handle used to cross-check `docs/RELEASE_READINESS.md`.
/// It is deliberately short and kebab-case so the report can quote it inline.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    pub id: String,
    pub detail: String,
}

impl Finding {
    fn new(id: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            detail: detail.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Version coherence
// ---------------------------------------------------------------------------

/// Extract `version` from the `[workspace.package]` table.
///
/// This is the single source of truth every other version claim is compared
/// against. Parsing is deliberately literal — a hand-rolled scan over the
/// section rather than a TOML dependency — so the gate keeps working even if
/// the manifest grows tables that a strict parser would reject.
pub fn workspace_version(cargo_toml: &str) -> Option<String> {
    let mut in_section = false;
    for line in cargo_toml.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == "[workspace.package]";
            continue;
        }
        if in_section {
            // Only return on a successful parse. Returning eagerly would let a
            // neighbouring key such as `version-policy = "9.9.9"` swallow the
            // lookup and report "no version" for a manifest that has one.
            if let Some(version) = line.strip_prefix("version").and_then(parse_quoted_value) {
                return Some(version);
            }
        }
    }
    None
}

/// Extract `version` from a member crate's `[package]` table.
///
/// Returns `None` when the member inherits the workspace version
/// (`version.workspace = true`), which is the desired state: one source of
/// truth instead of a value that merely happens to agree today.
pub fn member_package_version(cargo_toml: &str) -> Option<String> {
    let mut in_section = false;
    for line in cargo_toml.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == "[package]";
            continue;
        }
        if in_section {
            if line.starts_with("version.workspace") {
                return None;
            }
            if let Some(version) = line.strip_prefix("version").and_then(parse_quoted_value) {
                return Some(version);
            }
        }
    }
    None
}

/// Pull `"x.y.z"` out of the right-hand side of a `key = "value"` line.
///
/// Guards against matching `version-foo = "1"`: the remainder must start with
/// an `=` once trimmed.
fn parse_quoted_value(rest: &str) -> Option<String> {
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?.trim();
    let inner = rest.strip_prefix('"')?;
    let end = inner.find('"')?;
    Some(inner[..end].to_string())
}

/// Every version this document claims for the published crate.
///
/// Covers the three shapes the READMEs use — a shields.io badge, the
/// `nexus-claude vX.Y.Z` heading, and the `nexus-claude = "X.Y.Z"` dependency
/// snippet a reader would copy — in all translations, since the badge label is
/// localised but the version is not.
pub fn documented_versions(markdown: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for line in markdown.lines() {
        if let Some(version) = badge_version(line) {
            found.insert(version);
        }
        if let Some(version) = heading_version(line) {
            found.insert(version);
        }
        if let Some(version) = dependency_version(line) {
            found.insert(version);
        }
    }
    found
}

/// `[![Version](https://img.shields.io/badge/version-0.5.0-blue.svg)]` and its
/// localised equivalents, where only the `-<version>-` segment is stable.
fn badge_version(line: &str) -> Option<String> {
    let badge = line.find("img.shields.io/badge/")?;
    let segment = &line[badge..];
    // The label is localised (`version`, `版本`, `バージョン`), so anchor on the
    // dashes around the semver triple rather than on the label text.
    segment
        .split('-')
        .find_map(|part| looks_like_semver(part).then(|| part.to_string()))
}

/// `## nexus-claude v0.5.0 - ...` in any translation.
fn heading_version(line: &str) -> Option<String> {
    if !line.starts_with('#') {
        return None;
    }
    // `filter_map` then `find`, not `find_map`: the first word starting with a
    // `v` is not necessarily the version ("## very fast v0.5.0"), and stopping
    // at it would silently miss the real claim.
    line.split_whitespace()
        .filter_map(|word| word.strip_prefix('v'))
        .find(|candidate| looks_like_semver(candidate))
        .map(str::to_string)
}

/// `nexus-claude = "0.5.0"` — the line a reader copies into their manifest.
///
/// Also handles the table form `nexus-claude = { version = "0.5.0", .. }`,
/// since a reader copying either one must not be handed a stale version.
fn dependency_version(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("nexus-claude")?;
    if let Some(version) = parse_quoted_value(rest).filter(|v| looks_like_semver(v)) {
        return Some(version);
    }
    let table = rest.trim_start().strip_prefix('=')?.trim_start();
    if !table.starts_with('{') {
        return None;
    }
    let key = table.find("version")?;
    parse_quoted_value(&table[key + "version".len()..]).filter(|v| looks_like_semver(v))
}

/// Accept `MAJOR.MINOR.PATCH` with purely numeric components.
///
/// Rejects the many near-misses in these files: `0.0.0` bind addresses are
/// accepted by shape but never appear in the scanned positions, while
/// `1.88` (an MSRV) and `v2` (an action pin) are rejected outright.
fn looks_like_semver(candidate: &str) -> bool {
    let mut parts = candidate.split('.');
    let valid = (0..3).all(|_| {
        parts
            .next()
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    });
    valid && parts.next().is_none()
}

/// The version of the newest `## [x.y.z]` entry in a keep-a-changelog file.
pub fn changelog_top_version(changelog: &str) -> Option<String> {
    changelog.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("## [")?;
        let end = rest.find(']')?;
        let candidate = &rest[..end];
        looks_like_semver(candidate).then(|| candidate.to_string())
    })
}

// ---------------------------------------------------------------------------
// Forbidden references
// ---------------------------------------------------------------------------

/// Parse `docs/allowed-hosts.txt` into a set of hosts.
pub fn parse_allowed_hosts(listing: &str) -> BTreeSet<String> {
    listing
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.to_ascii_lowercase())
        .collect()
}

/// Hosts referenced over http(s) by `content` that the allowlist does not cover.
///
/// Returns them sorted and deduplicated so a failure message is stable and a
/// single offending host is reported once however often it appears.
pub fn foreign_hosts(content: &str, allowed: &BTreeSet<String>) -> BTreeSet<String> {
    extract_hosts(content)
        .into_iter()
        .filter(|host| !is_reserved_for_testing(host) && !allowed.contains(host))
        .collect()
}

/// Whether a host sits under a top-level domain reserved so it can never
/// resolve to a real service (RFC 2606 and RFC 6761).
///
/// Such a host cannot leak anything, because nothing can be reachable there.
/// Exempting them is what lets this gate's own negative controls — which must
/// name hosts that are *not* allowlisted — live in a file the gate scans. The
/// alternative, excluding the gate's source from the scan, would carve a blind
/// spot into precisely the file where a leak would be easiest to hide.
fn is_reserved_for_testing(host: &str) -> bool {
    const RESERVED_TLDS: &[&str] = &[".test", ".invalid", ".example", ".localhost"];
    host == "localhost" || RESERVED_TLDS.iter().any(|tld| host.ends_with(tld))
}

/// Collect the host component of every `http://` / `https://` URL.
fn extract_hosts(content: &str) -> BTreeSet<String> {
    let mut hosts = BTreeSet::new();
    let bytes = content.as_bytes();
    let mut cursor = 0usize;
    while let Some(offset) = content[cursor..].find("://") {
        let scheme_end = cursor + offset;
        // Walk backwards over the scheme; only http and https carry a host we
        // care about (a `file://` path or an XML namespace has none).
        let scheme_start = content[..scheme_end]
            .rfind(|c: char| !c.is_ascii_alphabetic())
            .map_or(0, |i| i + 1);
        let scheme = &content[scheme_start..scheme_end];
        cursor = scheme_end + 3;
        if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
            continue;
        }
        let host_end = bytes[cursor..]
            .iter()
            .position(|b| !is_host_byte(*b))
            .map_or(content.len(), |len| cursor + len);
        let host = content[cursor..host_end].trim_end_matches('.');
        if !host.is_empty() {
            hosts.insert(host.to_ascii_lowercase());
        }
        cursor = host_end;
    }
    hosts
}

/// Bytes allowed inside a hostname. A `:` ends the host so a port is dropped.
fn is_host_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'~')
}

// ---------------------------------------------------------------------------
// Release pipeline guards
// ---------------------------------------------------------------------------

/// Guards a publishing workflow must carry before a release can be trusted.
///
/// Each entry is `(finding id, human description, marker that proves it)`.
/// The marker is matched case-insensitively against the workflow text, so a
/// step named `Verify checksums` and one named `Generate SHA256SUMS` both
/// satisfy the checksum guard.
const RELEASE_GUARDS: &[(&str, &str, &[&str])] = &[
    (
        "release-no-checksums",
        "the publishing workflow uploads artifacts without checksums, so a \
         download cannot be verified",
        &["sha256", "shasum", "checksum"],
    ),
    (
        "release-no-smoke-test",
        "no job installs a built artifact and runs it, so a binary that cannot \
         start would still be published",
        &["--version", "smoke"],
    ),
    (
        "release-tag-version-unchecked",
        "the release version is taken from the git tag and never compared with \
         the workspace manifest, so a mistyped tag would publish a crate whose \
         version nobody chose",
        &["version-consistency", "verify_version", "verify version"],
    ),
];

/// Which of [`RELEASE_GUARDS`] the workflow text fails to satisfy.
pub fn missing_release_guards(workflow: &str) -> Vec<Finding> {
    let haystack = workflow.to_ascii_lowercase();
    RELEASE_GUARDS
        .iter()
        .filter(|(_, _, markers)| {
            !markers
                .iter()
                .any(|marker| haystack.contains(&marker.to_ascii_lowercase()))
        })
        .map(|(id, detail, _)| Finding::new(*id, *detail))
        .collect()
}

// ---------------------------------------------------------------------------
// The aggregate
// ---------------------------------------------------------------------------

/// Documents whose version claims must agree with the workspace manifest.
const VERSIONED_DOCS: &[&str] = &["README.md", "README_CN.md", "README_JA.md"];

/// Member manifests that should inherit the workspace version.
const MEMBER_MANIFESTS: &[&str] = &[
    "claude-code-api/Cargo.toml",
    "claude-code-sdk-rs/Cargo.toml",
];

/// Recompute every reason this repository is not release-ready.
///
/// The caller supplies the file contents so the aggregation itself is testable
/// against synthetic repositories — including repositories with no findings at
/// all, which no amount of reading the real tree could produce.
pub fn collect_findings(files: &RepoSnapshot) -> Vec<Finding> {
    let mut findings = Vec::new();

    let truth = workspace_version(&files.workspace_manifest);
    let truth = match truth {
        Some(v) => v,
        None => {
            findings.push(Finding::new(
                "workspace-version-unreadable",
                "[workspace.package] declares no version, so nothing can be \
                 compared against it",
            ));
            return findings;
        },
    };

    for (path, manifest) in &files.member_manifests {
        if let Some(hardcoded) = member_package_version(manifest) {
            findings.push(Finding::new(
                "member-version-not-inherited",
                format!(
                    "{path} hard-codes version {hardcoded} instead of \
                     `version.workspace = true`, so it is a second source of \
                     truth that can silently drift from {truth}"
                ),
            ));
        }
    }

    for (path, markdown) in &files.versioned_docs {
        for claimed in documented_versions(markdown) {
            if claimed != truth {
                findings.push(Finding::new(
                    "documented-version-mismatch",
                    format!("{path} advertises version {claimed} but the workspace is at {truth}"),
                ));
            }
        }
    }

    match changelog_top_version(&files.changelog) {
        Some(top) if top != truth => findings.push(Finding::new(
            "changelog-behind-workspace",
            format!(
                "the newest changelog entry is {top} while the workspace is at \
                 {truth}: the released history is undocumented"
            ),
        )),
        None => findings.push(Finding::new(
            "changelog-unreadable",
            "no `## [x.y.z]` entry found in the changelog",
        )),
        Some(_) => {},
    }

    let allowed = parse_allowed_hosts(&files.allowed_hosts);
    for (path, content) in &files.scanned_sources {
        for host in foreign_hosts(content, &allowed) {
            findings.push(Finding::new(
                "foreign-host",
                format!(
                    "{path} references {host}, which is not in \
                     docs/allowed-hosts.txt"
                ),
            ));
        }
    }

    findings.extend(missing_release_guards(&files.release_workflow));

    findings.sort();
    findings.dedup();
    findings
}

/// The repository contents the gate reasons about.
///
/// Holding this as data rather than reading files inside [`collect_findings`]
/// is what makes the negative controls below possible.
#[derive(Debug, Default)]
pub struct RepoSnapshot {
    pub workspace_manifest: String,
    pub member_manifests: Vec<(String, String)>,
    pub versioned_docs: Vec<(String, String)>,
    pub changelog: String,
    pub allowed_hosts: String,
    pub scanned_sources: Vec<(String, String)>,
    pub release_workflow: String,
}

impl RepoSnapshot {
    /// Load the real repository.
    fn from_repo() -> Self {
        Self {
            workspace_manifest: read_repo_file("Cargo.toml"),
            member_manifests: MEMBER_MANIFESTS
                .iter()
                .map(|p| ((*p).to_string(), read_repo_file(p)))
                .collect(),
            versioned_docs: VERSIONED_DOCS
                .iter()
                .map(|p| ((*p).to_string(), read_repo_file(p)))
                .collect(),
            changelog: read_repo_file("CHANGELOG.md"),
            allowed_hosts: read_repo_file("docs/allowed-hosts.txt"),
            scanned_sources: tracked_text_files(),
            release_workflow: read_repo_file(".github/workflows/release.yml"),
        }
    }
}

/// Text files to scan for forbidden references.
///
/// Walks the working tree rather than asking git, so the gate needs no
/// subprocess and works from an exported tarball. Build outputs, caches and
/// binary assets are skipped: they are not reviewed content, and scanning them
/// would make the gate slow and noisy.
fn tracked_text_files() -> Vec<(String, String)> {
    const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".fastembed_cache"];
    const TEXT_EXTENSIONS: &[&str] = &[
        "rs", "toml", "md", "yml", "yaml", "json", "sh", "ps1", "txt", "example",
    ];

    let root = repo_root();
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(path);
                }
                continue;
            }
            let is_text = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| TEXT_EXTENSIONS.contains(&ext));
            if !is_text {
                continue;
            }
            if let Ok(content) = fs::read_to_string(&path) {
                let relative = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                out.push((relative, content));
            }
        }
    }
    out.sort();
    out
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod version_parsing {
    use super::*;

    #[test]
    fn reads_the_workspace_version() {
        let manifest = "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"1.2.3\"\n";
        assert_eq!(workspace_version(manifest).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn ignores_a_version_outside_the_workspace_package_table() {
        // `[workspace.dependencies]` is full of `version = ...` entries; picking
        // one of those up would make the gate compare against a dependency.
        let manifest = "[workspace.dependencies]\nserde = { version = \"1.0.0\" }\n";
        assert_eq!(workspace_version(manifest), None);
    }

    #[test]
    fn does_not_confuse_a_similarly_named_key() {
        let manifest = "[workspace.package]\nversion-policy = \"9.9.9\"\nversion = \"1.2.3\"\n";
        assert_eq!(workspace_version(manifest).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn inherited_member_version_is_not_a_second_source_of_truth() {
        let manifest = "[package]\nname = \"x\"\nversion.workspace = true\n";
        assert_eq!(member_package_version(manifest), None);
    }

    #[test]
    fn hardcoded_member_version_is_reported() {
        let manifest = "[package]\nname = \"x\"\nversion = \"0.5.0\"\n";
        assert_eq!(member_package_version(manifest).as_deref(), Some("0.5.0"));
    }
}

#[cfg(test)]
mod documented_version_parsing {
    use super::*;

    #[test]
    fn finds_the_badge_the_heading_and_the_dependency_snippet() {
        let doc = "\
[![Version](https://img.shields.io/badge/version-0.5.0-blue.svg)](https://github.com/x/y)

## nexus-claude v0.5.0 - Rust SDK

```toml
nexus-claude = \"0.5.0\"
```
";
        let found = documented_versions(doc);
        assert_eq!(found.len(), 1, "all three agree: {found:?}");
        assert!(found.contains("0.5.0"));
    }

    #[test]
    fn finds_a_localised_badge() {
        // The Chinese and Japanese READMEs localise the badge label, so the
        // parser must not anchor on the word "version".
        let doc =
            "[![版本](https://img.shields.io/badge/版本-0.5.0-blue.svg)](https://github.com/x/y)";
        assert!(documented_versions(doc).contains("0.5.0"));
    }

    #[test]
    fn finds_a_version_inside_a_dependency_table() {
        let doc = "nexus-claude = { version = \"0.5.0\", features = [\"full\"] }";
        assert!(documented_versions(doc).contains("0.5.0"));
    }

    #[test]
    fn reports_every_distinct_claim_when_a_document_disagrees_with_itself() {
        let doc = "\
[![Version](https://img.shields.io/badge/version-0.4.0-blue.svg)](https://github.com/x/y)

## nexus-claude v0.5.0

nexus-claude = \"0.3.1\"
";
        let found = documented_versions(doc);
        assert_eq!(found.len(), 3, "{found:?}");
    }

    #[test]
    fn ignores_a_bind_address_and_an_msrv() {
        // `0.0.0.0` has four components and `1.88` has two; neither is a
        // version claim, and both appear in the real READMEs.
        let doc = "## Setup\n\nCLAUDE_CODE__SERVER__HOST=0.0.0.0\n\nMSRV 1.88\n";
        assert!(documented_versions(doc).is_empty());
    }

    #[test]
    fn reads_the_newest_changelog_entry_only() {
        let changelog = "# Changelog\n\n## [0.5.0] - 2026-01-01\n\n## [0.4.0] - 2025-01-01\n";
        assert_eq!(changelog_top_version(changelog).as_deref(), Some("0.5.0"));
    }

    #[test]
    fn skips_an_unreleased_heading() {
        let changelog = "# Changelog\n\n## [Unreleased]\n\n## [0.5.0] - 2026-01-01\n";
        assert_eq!(changelog_top_version(changelog).as_deref(), Some("0.5.0"));
    }
}

#[cfg(test)]
mod host_scanning {
    use super::*;

    fn allowlist() -> BTreeSet<String> {
        parse_allowed_hosts("# comment\n\ngithub.com\ndocs.rs\n")
    }

    /// A host that is neither allowlisted nor under a reserved top-level
    /// domain — the shape of an actual leak.
    ///
    /// Assembled from fragments and interpolated, never written as a complete
    /// URL, so this file does not itself contain a reference the gate would
    /// have to report when it scans the working tree.
    fn unlisted_host() -> String {
        format!("{}.{}", "internal-design", "exemplar-host.net")
    }

    #[test]
    fn an_allowlisted_host_is_not_reported() {
        let content = "See https://github.com/this-rs/nexus and https://docs.rs/nexus-claude.";
        assert!(foreign_hosts(content, &allowlist()).is_empty());
    }

    /// Negative control for the leak this gate exists to catch: a host nobody
    /// added to the allowlist must be reported.
    #[test]
    fn an_unlisted_host_is_reported() {
        let host = unlisted_host();
        let content = format!("curl https://{host}/state");
        let hosts = foreign_hosts(&content, &allowlist());
        assert_eq!(hosts.len(), 1, "{hosts:?}");
        assert!(hosts.contains(&host));
    }

    /// A reserved top-level domain can never resolve, so it is never a leak.
    /// Without this exemption the gate could not keep its own fixtures.
    #[test]
    fn a_reserved_tld_is_never_reported() {
        let content = "https://anything.test/x https://a.invalid/y https://b.example/z";
        assert!(foreign_hosts(content, &allowlist()).is_empty());
    }

    #[test]
    fn a_port_and_a_path_are_not_part_of_the_host() {
        let content = "http://github.com:8080/owner/repo?x=1#frag";
        assert!(foreign_hosts(content, &allowlist()).is_empty());
    }

    #[test]
    fn a_non_http_scheme_carries_no_host() {
        // `file://` and XML namespaces must not be mistaken for network
        // references, or every schema URI would need allowlisting.
        let content = format!("file:///etc/passwd and ftp://ftp.{}/pub", unlisted_host());
        assert!(foreign_hosts(&content, &allowlist()).is_empty());
    }

    #[test]
    fn the_same_unlisted_host_is_reported_once() {
        let host = unlisted_host();
        let content = format!("https://{host}/x https://{host}/y");
        assert_eq!(foreign_hosts(&content, &allowlist()).len(), 1);
    }

    #[test]
    fn host_matching_ignores_case() {
        let content = "https://GitHub.COM/this-rs/nexus";
        assert!(foreign_hosts(content, &allowlist()).is_empty());
    }

    #[test]
    fn a_trailing_sentence_period_is_not_part_of_the_host() {
        let content = "Read https://docs.rs.";
        assert!(foreign_hosts(content, &allowlist()).is_empty());
    }
}

#[cfg(test)]
mod release_guards {
    use super::*;

    #[test]
    fn a_workflow_with_every_guard_reports_nothing() {
        let workflow = "\
jobs:
  verify:
    steps:
      - name: version-consistency
        run: ./script/check-version-consistency.sh
      - name: Checksums
        run: shasum -a 256 dist/* > SHA256SUMS
      - name: Smoke test
        run: ./dist/claude-code-api --version
";
        assert!(missing_release_guards(workflow).is_empty());
    }

    /// Negative control: strip the guards and all three must come back.
    #[test]
    fn a_bare_publishing_workflow_reports_every_guard() {
        let workflow = "jobs:\n  build:\n    steps:\n      - run: cargo build --release\n";
        let ids: Vec<_> = missing_release_guards(workflow)
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(
            ids,
            vec![
                "release-no-checksums",
                "release-no-smoke-test",
                "release-tag-version-unchecked"
            ]
        );
    }

    #[test]
    fn a_guard_is_recognised_whatever_its_step_name() {
        let workflow = "- run: sha256sum dist/*\n";
        let ids: Vec<_> = missing_release_guards(workflow)
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert!(
            !ids.contains(&"release-no-checksums".to_string()),
            "{ids:?}"
        );
    }
}

#[cfg(test)]
mod aggregation {
    use super::*;

    /// A repository with nothing wrong must yield an empty finding set.
    ///
    /// Without this case the aggregate could be unconditionally non-empty and
    /// every other assertion would still pass.
    fn clean_snapshot() -> RepoSnapshot {
        RepoSnapshot {
            workspace_manifest: "[workspace.package]\nversion = \"1.0.0\"\n".into(),
            member_manifests: vec![(
                "a/Cargo.toml".into(),
                "[package]\nversion.workspace = true\n".into(),
            )],
            versioned_docs: vec![("README.md".into(), "## nexus-claude v1.0.0\n".into())],
            changelog: "# Changelog\n\n## [1.0.0] - 2026-01-01\n".into(),
            allowed_hosts: "github.com\n".into(),
            scanned_sources: vec![("README.md".into(), "https://github.com/x/y".into())],
            release_workflow: "steps:\n  - run: shasum -a 256 x\n  - run: x --version\n  \
                               - name: version-consistency\n"
                .into(),
        }
    }

    #[test]
    fn a_clean_repository_has_no_findings() {
        let findings = collect_findings(&clean_snapshot());
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn a_hardcoded_member_version_is_a_finding_even_when_it_agrees() {
        let mut snapshot = clean_snapshot();
        // Agrees with the workspace *today* — that is exactly the trap.
        snapshot.member_manifests = vec![(
            "a/Cargo.toml".into(),
            "[package]\nversion = \"1.0.0\"\n".into(),
        )];
        let ids: Vec<_> = collect_findings(&snapshot)
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(ids, vec!["member-version-not-inherited"]);
    }

    #[test]
    fn a_stale_readme_is_a_finding() {
        let mut snapshot = clean_snapshot();
        snapshot.versioned_docs = vec![("README.md".into(), "## nexus-claude v0.9.0\n".into())];
        let ids: Vec<_> = collect_findings(&snapshot)
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(ids, vec!["documented-version-mismatch"]);
    }

    #[test]
    fn a_changelog_behind_the_workspace_is_a_finding() {
        let mut snapshot = clean_snapshot();
        snapshot.changelog = "# Changelog\n\n## [0.1.10] - 2025-01-15\n".into();
        let ids: Vec<_> = collect_findings(&snapshot)
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(ids, vec!["changelog-behind-workspace"]);
    }

    #[test]
    fn a_leaked_host_is_a_finding() {
        let mut snapshot = clean_snapshot();
        let host = format!("{}.{}", "private-design", "exemplar-host.net");
        snapshot.scanned_sources = vec![(
            "script/publish.sh".into(),
            format!("curl https://{host}/api"),
        )];
        let ids: Vec<_> = collect_findings(&snapshot)
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(ids, vec!["foreign-host"]);
    }

    #[test]
    fn an_unreadable_workspace_version_short_circuits() {
        // Nothing can be compared without a source of truth, so the gate says
        // that and stops rather than emitting a cascade of mismatches.
        let mut snapshot = clean_snapshot();
        snapshot.workspace_manifest = "[workspace]\nmembers = []\n".into();
        let ids: Vec<_> = collect_findings(&snapshot)
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(ids, vec!["workspace-version-unreadable"]);
    }
}

// ---------------------------------------------------------------------------
// The gate proper: the report must match the repository
// ---------------------------------------------------------------------------

/// Path of the document this gate keeps honest.
const REPORT: &str = "docs/RELEASE_READINESS.md";

/// Finding ids the report is allowed to discuss without the gate computing
/// them, because they are about the *other* repositories (backend, frontend)
/// or about facts no offline check can establish. Keeping this list explicit —
/// rather than loosening the comparison — is what stops the report from
/// quietly accumulating unverifiable claims.
const REPORT_ONLY_IDS: &[&str] = &[
    "frontend-ci-never-runs-on-main",
    "backend-main-references-local-tooling",
    "bug-registry-incomplete",
    "distribution-matrix-incomplete",
];

/// Every finding id the gate knows how to emit.
fn known_ids() -> BTreeSet<&'static str> {
    let mut ids: BTreeSet<&'static str> = [
        "workspace-version-unreadable",
        "member-version-not-inherited",
        "documented-version-mismatch",
        "changelog-behind-workspace",
        "changelog-unreadable",
        "foreign-host",
    ]
    .into_iter()
    .collect();
    ids.extend(RELEASE_GUARDS.iter().map(|(id, _, _)| *id));
    ids.extend(REPORT_ONLY_IDS.iter().copied());
    ids
}

/// Finding ids the report claims, read back from its own table.
///
/// The report marks each one with a <!-- finding: id --> comment so the gate
/// reads intent rather than guessing from prose.
/// Markers sit wherever the prose needs them — most often inside a table cell
/// — so the scan is positional, not line-anchored, and a line may carry more
/// than one.
fn ids_claimed_by_report(report: &str) -> BTreeSet<String> {
    const OPEN: &str = "<!-- finding:";
    let mut ids = BTreeSet::new();
    let mut rest = report;
    while let Some(start) = rest.find(OPEN) {
        rest = &rest[start + OPEN.len()..];
        let Some(end) = rest.find("-->") else { break };
        let id = rest[..end].trim();
        if !id.is_empty() {
            ids.insert(id.to_string());
        }
        rest = &rest[end + 3..];
    }
    ids
}

#[cfg(test)]
mod report_markers {
    use super::*;

    #[test]
    fn reads_a_marker_from_inside_a_table_cell() {
        // The shape the report actually uses. An earlier line-anchored parser
        // silently read *nothing* here, which made the gate pass by vacuity —
        // the worst possible failure for a gate whose whole job is to refuse.
        let report = "| `a-b` <!-- finding: a-b --> | high | something |\n";
        let ids = ids_claimed_by_report(report);
        assert_eq!(ids.len(), 1, "{ids:?}");
        assert!(ids.contains("a-b"));
    }

    #[test]
    fn reads_several_markers_from_one_line() {
        let report = "<!-- finding: one --> and <!-- finding: two -->";
        assert_eq!(ids_claimed_by_report(report).len(), 2);
    }

    #[test]
    fn a_report_with_no_marker_claims_nothing() {
        let report = "# Report\n\nEverything is fine, honestly.\n";
        assert!(ids_claimed_by_report(report).is_empty());
    }

    #[test]
    fn an_unterminated_marker_does_not_hang_or_swallow_the_rest() {
        let report = "<!-- finding: good --> then <!-- finding: oops";
        let ids = ids_claimed_by_report(report);
        assert_eq!(ids.len(), 1, "{ids:?}");
        assert!(ids.contains("good"));
    }
}

#[test]
fn the_report_lists_exactly_the_findings_the_repository_has() {
    let snapshot = RepoSnapshot::from_repo();
    let computed: BTreeSet<String> = collect_findings(&snapshot)
        .into_iter()
        .map(|f| f.id)
        .collect();
    let report = read_repo_file(REPORT);
    let claimed = ids_claimed_by_report(&report);

    let report_only: BTreeSet<String> = REPORT_ONLY_IDS.iter().map(|s| s.to_string()).collect();
    let expected: BTreeSet<String> = computed.union(&report_only).cloned().collect();

    let undocumented: Vec<_> = expected.difference(&claimed).collect();
    let stale: Vec<_> = claimed.difference(&expected).collect();

    assert!(
        undocumented.is_empty(),
        "{REPORT} does not record these findings, which the gate just \
         recomputed from the repository: {undocumented:?}. Add a row for each \
         with a `<!-- finding: id -->` marker."
    );
    assert!(
        stale.is_empty(),
        "{REPORT} still claims these findings, but the gate can no longer \
         reproduce them: {stale:?}. If they were fixed, delete the rows — a \
         report that overstates the problems is as useless as one that hides \
         them."
    );
}

#[test]
fn the_report_claims_no_finding_the_gate_cannot_name() {
    // Guards against a typo in a marker silently turning into a finding id
    // that neither side will ever reconcile.
    let report = read_repo_file(REPORT);
    let known = known_ids();
    let unknown: Vec<_> = ids_claimed_by_report(&report)
        .into_iter()
        .filter(|id| !known.contains(id.as_str()))
        .collect();
    assert!(
        unknown.is_empty(),
        "{REPORT} uses finding ids the gate does not define: {unknown:?}"
    );
}

#[test]
fn the_report_states_that_nothing_was_released() {
    // The task that produced this gate forbids publishing. The report is the
    // artifact a reader will trust, so the statement must be in it.
    let report = read_repo_file(REPORT).to_ascii_lowercase();
    for phrase in ["no release", "no tag", "no version bump"] {
        assert!(
            report.contains(phrase),
            "{REPORT} must state plainly that there was {phrase}"
        );
    }
}

#[test]
fn the_allowlist_has_no_duplicate_or_scheme_bearing_entries() {
    let listing = read_repo_file("docs/allowed-hosts.txt");
    let entries: Vec<&str> = listing
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let unique = parse_allowed_hosts(&listing);
    assert_eq!(
        entries.len(),
        unique.len(),
        "docs/allowed-hosts.txt has duplicate entries"
    );
    for entry in entries {
        assert!(
            !entry.contains("://") && !entry.contains('/'),
            "docs/allowed-hosts.txt entry {entry:?} must be a bare host, with \
             no scheme and no path"
        );
    }
}
