//! Diagram charter and drift gate for this repository (task 1.1).
//!
//! The charter (`docs/DOCUMENTATION.md`, in this repository; the main
//! repository keeps its own for the cross-repo cartography) makes the
//! in-repo diagrams authoritative: a `docs/diagrams/<name>.mmd` per diagram,
//! reviewed like code, changed in the same pull request as the code it
//! describes. Two things have to be enforced for that to mean anything, and
//! neither can be left to a reviewer's attention:
//!
//! 1. **The charter's own rules.** A header with `name` / `covers` /
//!    `verified`; `name` equal to the file name; `covers` identical to the
//!    index entry; every node carrying a status mark; no line numbers. A
//!    diagram that breaks these is not documentation, it is decoration.
//!
//! 2. **Drift.** When a covered file changes, the owning diagram changes in
//!    the same pull request — or a commit says, in writing, why not.
//!
//! # Why Rust and not a script
//!
//! This is a pure Rust workspace. Adding a Node toolchain for a checker would
//! mean a `package.json`, a lockfile and `actions/setup-node` in a repository
//! that has none of them. As an integration test the gate instead runs inside
//! the existing CI `test` job, on all three operating systems, with no new
//! dependency — and `cargo test` is what a contributor already runs.
//!
//! Everything is offline: files are read from the working tree, and the pull
//! request context arrives through the environment rather than by shelling out
//! to git.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Repository access
// ---------------------------------------------------------------------------

/// This repository's prefix in the charter's repo-prefixed globs.
const REPO_PREFIX: &str = "nexus:";

const INDEX_PATH: &str = "docs/diagrams/INDEX.yml";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate directory always has a parent")
        .to_path_buf()
}

fn read_repo_file(relative: &str) -> String {
    let path = repo_root().join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("diagram gate cannot read {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// The index
// ---------------------------------------------------------------------------

/// One diagram as the index declares it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    /// Present only for `status: verified`, per the charter.
    pub file: Option<String>,
    pub owner: Option<String>,
    pub status: String,
    pub verified: Option<String>,
    pub covers: Vec<String>,
}

/// Parse the charter's strict YAML subset.
///
/// A real YAML parser is deliberately avoided: it would be a new dependency to
/// read a file the charter defines as a fixed shape, and it would silently
/// accept constructs the charter forbids. This parser rejects anything it does
/// not recognise, which is the point — a typo becomes a failure rather than a
/// quietly dropped field.
pub fn parse_index(yaml: &str) -> Result<Vec<IndexEntry>, String> {
    let mut entries: Vec<IndexEntry> = Vec::new();
    let mut in_diagrams = false;
    let mut in_covers = false;

    for (lineno, raw) in yaml.lines().enumerate() {
        let lineno = lineno + 1;
        let line = raw.split(" #").next().unwrap_or(raw).trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }

        if line == "diagrams:" {
            in_diagrams = true;
            continue;
        }
        if !in_diagrams {
            return Err(format!("line {lineno}: content before `diagrams:`"));
        }

        let trimmed = line.trim_start();

        if let Some(rest) = trimmed.strip_prefix("- name:") {
            in_covers = false;
            entries.push(IndexEntry {
                name: unquote(rest.trim()).to_string(),
                status: String::new(),
                ..Default::default()
            });
            continue;
        }

        let entry = entries
            .last_mut()
            .ok_or_else(|| format!("line {lineno}: field outside any entry"))?;

        if let Some(rest) = trimmed.strip_prefix("- ") {
            if !in_covers {
                return Err(format!("line {lineno}: list item outside `covers:`"));
            }
            entry.covers.push(unquote(rest.trim()).to_string());
            continue;
        }

        in_covers = false;
        let (key, value) = trimmed
            .split_once(':')
            .ok_or_else(|| format!("line {lineno}: not a `key: value` pair"))?;
        let value = unquote(value.trim()).to_string();
        match key.trim() {
            "covers" => {
                if !value.is_empty() {
                    return Err(format!(
                        "line {lineno}: `covers:` takes a list, not a scalar"
                    ));
                }
                in_covers = true;
            },
            "file" => entry.file = Some(value),
            "owner" => entry.owner = Some(value),
            "status" => entry.status = value,
            "verified" => entry.verified = Some(value),
            other => return Err(format!("line {lineno}: unknown field `{other}`")),
        }
    }

    if entries.is_empty() {
        return Err("the index declares no diagram".to_string());
    }
    Ok(entries)
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
}

// ---------------------------------------------------------------------------
// The diagram header
// ---------------------------------------------------------------------------

/// The three mandatory header fields of a `.mmd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramHeader {
    pub name: String,
    pub covers: Vec<String>,
    pub verified: String,
}

/// Read the charter header from the first three lines.
///
/// The charter fixes both the order and the position, so this does not search
/// the file: a header pushed below a stray line is a violation, not something
/// to be tolerantly recovered.
pub fn parse_header(mmd: &str) -> Result<DiagramHeader, String> {
    let mut lines = mmd.lines();
    let name = expect_field(lines.next(), "name")?;
    let covers = expect_field(lines.next(), "covers")?;
    let verified = expect_field(lines.next(), "verified")?;

    if name.is_empty() {
        return Err("`%% name:` is empty".to_string());
    }
    if verified.is_empty() {
        return Err("`%% verified:` is empty".to_string());
    }
    // A short git sha, not a date. The whole point of the field is that a reader can run
    // `git show <verified>:<file>` and see exactly the code the author read. A date cannot be
    // checked out, so it makes the diagram's verification unreproducible — which is the defect
    // diagrams exist to correct. The main repository's checker enforces the same shape; without
    // this, a diagram refused there passed here.
    if !is_short_sha(&verified) {
        return Err(format!(
            "`%% verified:` must be a short git sha (7-40 lowercase hex), not {verified:?} \
             — a date or a word cannot be checked out"
        ));
    }
    let covers: Vec<String> = covers.split_whitespace().map(str::to_string).collect();
    if covers.is_empty() {
        return Err("`%% covers:` lists no glob".to_string());
    }
    Ok(DiagramHeader {
        name,
        covers,
        verified,
    })
}

/// Whether a string is a short git sha: 7 to 40 lowercase hex digits.
///
/// Lowercase only, because that is what `git rev-parse --short` emits and accepting both would
/// let two spellings of the same sha into the index.
fn is_short_sha(value: &str) -> bool {
    (7..=40).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn expect_field(line: Option<&str>, field: &str) -> Result<String, String> {
    let line = line.ok_or_else(|| format!("file ends before `%% {field}:`"))?;
    let prefix = format!("%% {field}:");
    line.strip_prefix(&prefix)
        .map(|rest| rest.trim().to_string())
        .ok_or_else(|| format!("expected `{prefix}` but found {line:?}"))
}

/// Status marks the charter defines, one of which every node must carry.
const STATUS_MARKS: &[char] = &['✅', '🟠', '🔴', '⚪'];

/// Node labels that carry no status mark.
///
/// The charter calls an unmarked node "a review error": it asserts something
/// about the code while saying nothing about whether anyone checked it. That is
/// precisely the failure this plan exists to correct, so it is enforced rather
/// than trusted.
pub fn nodes_without_status(mmd: &str) -> Vec<String> {
    node_labels(mmd)
        .into_iter()
        .filter(|label| !label.chars().any(|c| STATUS_MARKS.contains(&c)))
        .collect()
}

/// Every quoted node label in the diagram body, comments excluded.
///
/// Labels are delimited by `["` and `"]` in the flowchart syntax this charter
/// uses, which is what lets a label contain brackets and punctuation freely.
///
/// A `subgraph` title is excluded: it groups nodes rather than asserting
/// anything about the code, so there is nothing for a status mark to be true
/// *of*. Requiring one would push a ✅ onto a container and cheapen the mark
/// everywhere else.
fn node_labels(mmd: &str) -> Vec<String> {
    let mut labels = Vec::new();
    for line in mmd.lines() {
        let line = line.trim();
        if line.starts_with("%%") || line.starts_with("subgraph ") {
            continue;
        }
        let mut rest = line;
        while let Some(start) = rest.find("[\"") {
            rest = &rest[start + 2..];
            let Some(end) = rest.find("\"]") else { break };
            labels.push(rest[..end].to_string());
            rest = &rest[end + 2..];
        }
    }
    labels
}

/// Mermaid constructs that render wrong or not at all, with the line number.
///
/// This is not a parser. A real Mermaid parser is JavaScript, and adding a
/// Node toolchain to a Rust workspace for it was rejected — so this checks the
/// specific traps that have actually bitten this repository, which is the
/// honest scope. It caught nothing when written; it was written because both
/// diagrams here were committed carrying the first one.
///
/// - A bare `%%` line. Mermaid treats `%%` followed by nothing as the start of
///   a directive rather than a comment, and the block can swallow the lines
///   after it. The charter already requires `%% ` with a trailing space; this
///   makes the requirement enforceable instead of remembered.
/// - An unbalanced `["` / `"]` on one line, which silently truncates a label.
pub fn mermaid_traps(mmd: &str) -> Vec<String> {
    let mut traps = Vec::new();
    for (index, line) in mmd.lines().enumerate() {
        let lineno = index + 1;
        if line.trim_end() == "%%" && !line.ends_with(' ') {
            traps.push(format!(
                "line {lineno}: bare \"%%\" — write \"%% \" with a trailing space, \
                 or Mermaid reads it as a directive"
            ));
        }
        let opens = line.matches("[\"").count();
        let closes = line.matches("\"]").count();
        if opens != closes {
            traps.push(format!(
                "line {lineno}: {opens} `[\"` against {closes} `\"]` — a label \
                 left open truncates silently"
            ));
        }
    }
    traps
}

/// Node labels that cite a source location as `file:line` instead of a name.
///
/// The charter's reason is empirical: a line number is wrong as soon as a
/// neighbouring commit lands, while a function name is wrong only when the
/// function really changes.
pub fn nodes_citing_line_numbers(mmd: &str) -> Vec<String> {
    node_labels(mmd)
        .into_iter()
        .filter(|label| contains_file_line_citation(label))
        .collect()
}

/// Detect `something.rs:123`, the shape the charter forbids.
fn contains_file_line_citation(label: &str) -> bool {
    const SOURCE_EXTENSIONS: &[&str] = &[".rs:", ".toml:", ".yml:", ".yaml:", ".sh:", ".md:"];
    SOURCE_EXTENSIONS.iter().any(|ext| {
        label.match_indices(ext).any(|(at, _)| {
            label[at + ext.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        })
    })
}

// ---------------------------------------------------------------------------
// Covered-path globs
// ---------------------------------------------------------------------------

/// Expand a charter glob, with its repo prefix removed, against a path list.
///
/// Supports exactly the three constructs the index uses: `**` across
/// directories, `*` within a segment, and `{a,b}` alternation. Anything richer
/// would invite globs nobody can check by eye.
pub fn glob_matches(glob: &str, path: &str) -> bool {
    for alternative in expand_braces(glob) {
        if wildcard_matches(&alternative, path) {
            return true;
        }
    }
    false
}

/// Turn `a/{b,c}.rs` into `["a/b.rs", "a/c.rs"]`.
fn expand_braces(glob: &str) -> Vec<String> {
    let Some(open) = glob.find('{') else {
        return vec![glob.to_string()];
    };
    let Some(close) = glob[open..].find('}').map(|i| open + i) else {
        return vec![glob.to_string()];
    };
    let (prefix, suffix) = (&glob[..open], &glob[close + 1..]);
    glob[open + 1..close]
        .split(',')
        .flat_map(|choice| expand_braces(&format!("{prefix}{choice}{suffix}")))
        .collect()
}

/// Match a brace-free glob against a path.
fn wildcard_matches(glob: &str, path: &str) -> bool {
    // Recursive descent over the pattern: `**` may consume any number of
    // characters including `/`, a single `*` stops at a separator.
    fn go(pattern: &[u8], text: &[u8]) -> bool {
        if pattern.is_empty() {
            return text.is_empty();
        }
        if pattern[0] == b'*' {
            if pattern.len() > 1 && pattern[1] == b'*' {
                let rest = &pattern[2..];
                // `**/` also matches zero directories, so `a/**/b.rs` matches
                // `a/b.rs`; without this a glob would need two spellings.
                let rest = rest.strip_prefix(b"/").unwrap_or(rest);
                if go(rest, text) {
                    return true;
                }
                for i in 0..text.len() {
                    if go(rest, &text[i + 1..]) {
                        return true;
                    }
                }
                return false;
            }
            let rest = &pattern[1..];
            if go(rest, text) {
                return true;
            }
            for i in 0..text.len() {
                if text[i] == b'/' {
                    break;
                }
                if go(rest, &text[i + 1..]) {
                    return true;
                }
            }
            return false;
        }
        if text.is_empty() || pattern[0] != text[0] {
            return false;
        }
        go(&pattern[1..], &text[1..])
    }
    go(glob.as_bytes(), path.as_bytes())
}

/// Strip this repository's prefix, returning `None` for another repository's glob.
pub fn local_glob(glob: &str) -> Option<&str> {
    glob.strip_prefix(REPO_PREFIX)
}

// ---------------------------------------------------------------------------
// Drift
// ---------------------------------------------------------------------------

/// A diagram that should have changed in this pull request but did not.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Drift {
    pub diagram: String,
    pub triggered_by: String,
}

/// The pull request context the drift check needs.
#[derive(Debug, Default)]
pub struct ChangeSet {
    /// Repo-relative paths changed by the pull request.
    pub changed: Vec<String>,
    /// `Diagram-Unchanged: <name> — <reason>` trailers from its commits.
    pub trailers: Vec<String>,
}

/// Diagram names excused by a `Diagram-Unchanged` trailer.
///
/// The reason is required: a bare name would make the escape hatch free, and a
/// free escape hatch is the one everybody takes. Anything after the name counts
/// as the reason, whatever dash or punctuation separates it.
pub fn excused_diagrams(trailers: &[String]) -> BTreeSet<String> {
    let mut excused = BTreeSet::new();
    for trailer in trailers {
        let Some(rest) = trailer.trim().strip_prefix("Diagram-Unchanged:") else {
            continue;
        };
        let rest = rest.trim();
        let (name, reason) = match rest.find([' ', '\t']) {
            Some(at) => (&rest[..at], rest[at..].trim()),
            None => (rest, ""),
        };
        let reason = reason.trim_start_matches(['-', '—', '–', ':']).trim();
        if !name.is_empty() && !reason.is_empty() {
            excused.insert(name.to_string());
        }
    }
    excused
}

/// Which owning diagrams this change set failed to update.
///
/// A diagram's own `.mmd` appearing in `changed` satisfies it, as does a
/// trailer naming it with a reason.
pub fn detect_drift(entries: &[IndexEntry], changes: &ChangeSet) -> Vec<Drift> {
    let excused = excused_diagrams(&changes.trailers);
    let changed: BTreeSet<&str> = changes.changed.iter().map(String::as_str).collect();

    let mut drifts = Vec::new();
    for entry in entries {
        // Only a verified entry has a file to update. Demanding a change to a
        // `planned` entry's `.mmd` would demand a change to a file that does
        // not exist: unsatisfiable except by the escape hatch, which is how an
        // escape hatch becomes routine. Its paths fall through to
        // `unowned_paths`, which reports instead of failing.
        let Some(diagram_file) = entry.file.clone() else {
            continue;
        };
        if changed.contains(diagram_file.as_str()) || excused.contains(&entry.name) {
            continue;
        }
        let globs: Vec<&str> = owning_globs(std::slice::from_ref(entry));
        if let Some(trigger) = changes
            .changed
            .iter()
            .filter(|path| path.as_str() != diagram_file.as_str())
            .find(|path| globs.iter().any(|glob| glob_matches(glob, path)))
        {
            drifts.push(Drift {
                diagram: entry.name.clone(),
                triggered_by: trigger.clone(),
            });
        }
    }
    drifts.sort();
    drifts
}

/// Changed files no diagram claims.
///
/// Reported, never failed: until every source file has an owning diagram
/// (task 0.2), failing here would block every pull request and the gate would
/// simply be switched off. A gate nobody can satisfy teaches nothing.
pub fn unowned_paths(entries: &[IndexEntry], changes: &ChangeSet) -> Vec<String> {
    let globs = owning_globs(entries);
    changes
        .changed
        .iter()
        .filter(|path| !globs.iter().any(|glob| glob_matches(glob, path)))
        .cloned()
        .collect()
}

/// Local globs belonging to diagrams that actually exist.
///
/// A `planned` entry owns nothing: there is no file, nobody has checked its
/// content against the code, and it names a diagram somebody intends to write.
/// Treating its globs as ownership would let the orphan ceiling fall, and the
/// drift check go quiet, without a single diagram being written — the index
/// would be buying credit for intentions. Only `verified` counts.
fn owning_globs(entries: &[IndexEntry]) -> Vec<&str> {
    entries
        .iter()
        .filter(|e| e.status == "verified")
        .flat_map(|e| e.covers.iter())
        .filter_map(|g| local_glob(g))
        .collect()
}

// ---------------------------------------------------------------------------
// Pull request context, from the environment
// ---------------------------------------------------------------------------

/// Read the change set from the environment, if CI supplied one.
///
/// `None` means "not running against a pull request", which is the normal case
/// for a local `cargo test`. The distinction is explicit rather than silent:
/// see `drift_is_reported_when_ci_supplies_a_change_set`.
fn change_set_from_env() -> Option<ChangeSet> {
    let changed = std::env::var("DIAGRAM_DRIFT_CHANGED_FILES").ok()?;
    let trailers = std::env::var("DIAGRAM_DRIFT_TRAILERS").unwrap_or_default();
    Some(ChangeSet {
        changed: changed
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        trailers: trailers
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod index_parsing {
    use super::*;

    const SAMPLE: &str = "\
# a comment
diagrams:
  - name: po-api
    owner: theotime
    status: planned
    covers:
      - \"backend:src/api/**\"
  - name: release-readiness
    file: docs/diagrams/release-readiness.mmd
    owner: theotime
    status: verified
    verified: dc510d5
    covers:
      - \"nexus:Cargo.toml\"
      - \"nexus:README.md\"
";

    #[test]
    fn reads_every_entry_and_field() {
        let entries = parse_index(SAMPLE).expect("sample is valid");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "po-api");
        assert_eq!(entries[0].status, "planned");
        assert_eq!(entries[0].file, None);
        assert_eq!(
            entries[1].file.as_deref(),
            Some("docs/diagrams/release-readiness.mmd")
        );
        assert_eq!(entries[1].verified.as_deref(), Some("dc510d5"));
        assert_eq!(entries[1].covers.len(), 2);
    }

    #[test]
    fn rejects_an_unknown_field() {
        // A typo must fail rather than be dropped: a silently ignored
        // `coverz:` would leave a diagram owning nothing while looking fine.
        let yaml = "diagrams:\n  - name: x\n    status: planned\n    coverz:\n";
        let err = parse_index(yaml).expect_err("should reject");
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn rejects_covers_written_as_a_scalar() {
        let yaml = "diagrams:\n  - name: x\n    covers: \"nexus:a.rs\"\n";
        let err = parse_index(yaml).expect_err("should reject");
        assert!(err.contains("takes a list"), "{err}");
    }

    #[test]
    fn rejects_a_list_item_outside_covers() {
        let yaml = "diagrams:\n  - name: x\n    status: planned\n      - \"nexus:a.rs\"\n";
        assert!(parse_index(yaml).is_err());
    }

    #[test]
    fn rejects_an_empty_index() {
        assert!(parse_index("diagrams:\n").is_err());
    }
}

#[cfg(test)]
mod header_parsing {
    use super::*;

    #[test]
    fn reads_the_three_mandatory_fields() {
        let mmd =
            "%% name: x\n%% covers: nexus:a.rs nexus:b.rs\n%% verified: abc1234\nflowchart TB\n";
        let header = parse_header(mmd).expect("valid header");
        assert_eq!(header.name, "x");
        assert_eq!(header.covers.len(), 2);
        assert_eq!(header.verified, "abc1234");
    }

    #[test]
    fn accepts_a_short_sha_of_any_usual_length() {
        for sha in ["abc1234", "9540068", "dc510d5", &"a".repeat(40)] {
            let mmd = format!("%% name: x\n%% covers: nexus:a.rs\n%% verified: {sha}\n");
            assert!(parse_header(&mmd).is_ok(), "{sha} should be accepted");
        }
    }

    #[test]
    fn rejects_a_verified_that_is_not_a_sha() {
        // The field used to be checked only for emptiness, so every one of these passed the
        // gate while being impossible to check out.
        for bad in [
            "yesterday",
            "2026-10-01",
            "TODO",
            "HEAD",
            "abc123",        // too short
            "ABC1234",       // uppercase is not what git emits
            "zzzzzzz",       // not hex
            &"a".repeat(41), // too long
        ] {
            let mmd = format!("%% name: x\n%% covers: nexus:a.rs\n%% verified: {bad}\n");
            let err = parse_header(&mmd).expect_err(&format!("{bad} should be refused"));
            assert!(
                err.contains("short git sha"),
                "{bad}: unexpected error {err}"
            );
        }
    }

    #[test]
    fn rejects_a_header_in_the_wrong_order() {
        let mmd = "%% covers: nexus:a.rs\n%% name: x\n%% verified: abc\n";
        assert!(parse_header(mmd).is_err());
    }

    #[test]
    fn rejects_a_header_pushed_below_a_stray_line() {
        let mmd = "\n%% name: x\n%% covers: nexus:a.rs\n%% verified: abc\n";
        assert!(parse_header(mmd).is_err());
    }

    #[test]
    fn rejects_an_empty_covers_list() {
        let mmd = "%% name: x\n%% covers:   \n%% verified: abc\n";
        assert!(parse_header(mmd).is_err());
    }

    #[test]
    fn rejects_a_truncated_file() {
        assert!(parse_header("%% name: x\n").is_err());
    }
}

#[cfg(test)]
mod node_rules {
    use super::*;

    #[test]
    fn an_unmarked_node_is_reported() {
        let mmd = "flowchart TB\n  a[\"marked ✅ how\"]\n  b[\"not marked at all\"]\n";
        let unmarked = nodes_without_status(mmd);
        assert_eq!(unmarked, vec!["not marked at all"]);
    }

    #[test]
    fn every_charter_mark_counts() {
        let mmd = "flowchart TB\n  a[\"✅ x\"]\n  b[\"🟠 y\"]\n  c[\"🔴 z\"]\n  d[\"⚪ w\"]\n";
        assert!(nodes_without_status(mmd).is_empty());
    }

    #[test]
    fn a_comment_is_not_a_node() {
        // The header legend names the marks and must not be mistaken for nodes.
        let mmd = "%% legend [\"not a node\"]\nflowchart TB\n  a[\"✅ x\"]\n";
        assert!(nodes_without_status(mmd).is_empty());
    }

    #[test]
    fn a_subgraph_title_needs_no_mark() {
        // A container asserts nothing about the code, so there is nothing a
        // status mark could be true of. Its member nodes still need one.
        let mmd = "flowchart TB\n  subgraph g[\"a grouping\"]\n    a[\"✅ x\"]\n  end\n";
        assert!(nodes_without_status(mmd).is_empty());
    }

    #[test]
    fn an_unmarked_node_inside_a_subgraph_is_still_reported() {
        let mmd = "flowchart TB\n  subgraph g[\"a grouping\"]\n    a[\"no mark\"]\n  end\n";
        assert_eq!(nodes_without_status(mmd), vec!["no mark"]);
    }

    #[test]
    fn a_bare_comment_line_is_reported() {
        // Both diagrams in this repository were committed with these. The
        // offline Mermaid linter caught them; nothing in the repository did.
        let traps = mermaid_traps("flowchart TB\n%%\n  a[\"✅ x\"]\n");
        assert_eq!(traps.len(), 1, "{traps:?}");
        assert!(traps[0].contains("line 2"), "{traps:?}");
    }

    #[test]
    fn a_comment_line_with_the_trailing_space_is_accepted() {
        assert!(mermaid_traps("flowchart TB\n%% \n  a[\"✅ x\"]\n").is_empty());
    }

    #[test]
    fn an_unbalanced_label_is_reported() {
        let traps = mermaid_traps("flowchart TB\n  a[\"unterminated\n");
        assert_eq!(traps.len(), 1, "{traps:?}");
    }

    #[test]
    fn a_balanced_line_with_two_labels_is_accepted() {
        assert!(mermaid_traps("  a[\"✅ one\"] --> b[\"✅ two\"]\n").is_empty());
    }

    #[test]
    fn a_line_number_citation_is_reported() {
        let mmd = "flowchart TB\n  a[\"✅ see routes.rs:58\"]\n";
        assert_eq!(nodes_citing_line_numbers(mmd).len(), 1);
    }

    #[test]
    fn a_function_name_is_not_a_line_citation() {
        let mmd = "flowchart TB\n  a[\"✅ proven by create_router in routes.rs\"]\n";
        assert!(nodes_citing_line_numbers(mmd).is_empty());
    }
}

#[cfg(test)]
mod globbing {
    use super::*;

    #[test]
    fn matches_an_exact_path() {
        assert!(glob_matches("Cargo.toml", "Cargo.toml"));
        assert!(!glob_matches("Cargo.toml", "claude-code-api/Cargo.toml"));
    }

    #[test]
    fn a_single_star_stops_at_a_separator() {
        assert!(glob_matches("src/*.rs", "src/lib.rs"));
        assert!(!glob_matches("src/*.rs", "src/api/lib.rs"));
    }

    #[test]
    fn a_double_star_crosses_directories() {
        assert!(glob_matches("src/**", "src/api/ws/handler.rs"));
        assert!(glob_matches("src/**/*.rs", "src/api/handler.rs"));
    }

    #[test]
    fn a_double_star_also_matches_zero_directories() {
        // `a/**/b.rs` must match `a/b.rs`, or every glob would need two forms.
        assert!(glob_matches("src/**/lib.rs", "src/lib.rs"));
    }

    #[test]
    fn brace_alternation_expands() {
        assert!(glob_matches("src/{lib,main}.rs", "src/main.rs"));
        assert!(!glob_matches("src/{lib,main}.rs", "src/other.rs"));
    }

    #[test]
    fn nested_alternation_expands() {
        assert!(glob_matches("{a,b}/{c,d}.rs", "b/d.rs"));
    }

    #[test]
    fn a_foreign_repo_glob_is_not_local() {
        assert_eq!(local_glob("backend:src/api/**"), None);
        assert_eq!(local_glob("nexus:Cargo.toml"), Some("Cargo.toml"));
    }
}

#[cfg(test)]
mod drift_rules {
    use super::*;

    fn index() -> Vec<IndexEntry> {
        parse_index(
            "diagrams:\n  - name: d1\n    file: docs/diagrams/d1.mmd\n    status: verified\n    \
             covers:\n      - \"nexus:src/**\"\n      - \"backend:src/api/**\"\n",
        )
        .expect("fixture index is valid")
    }

    fn changes(changed: &[&str], trailers: &[&str]) -> ChangeSet {
        ChangeSet {
            changed: changed.iter().map(|s| s.to_string()).collect(),
            trailers: trailers.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Fixture 1 of the acceptance criteria: a covered file changed, with
    /// neither a diagram update nor a trailer, must fail.
    #[test]
    fn a_covered_change_without_diagram_or_trailer_drifts() {
        let drifts = detect_drift(&index(), &changes(&["src/manager.rs"], &[]));
        assert_eq!(drifts.len(), 1, "{drifts:?}");
        assert_eq!(drifts[0].diagram, "d1");
        assert_eq!(drifts[0].triggered_by, "src/manager.rs");
    }

    /// Fixture 2a: updating the diagram in the same change set satisfies it.
    #[test]
    fn updating_the_diagram_clears_the_drift() {
        let drifts = detect_drift(
            &index(),
            &changes(&["src/manager.rs", "docs/diagrams/d1.mmd"], &[]),
        );
        assert!(drifts.is_empty(), "{drifts:?}");
    }

    /// Fixture 2b: a trailer with a reason also satisfies it.
    #[test]
    fn a_trailer_with_a_reason_clears_the_drift() {
        let drifts = detect_drift(
            &index(),
            &changes(
                &["src/manager.rs"],
                &["Diagram-Unchanged: d1 — rename only, no edge changed"],
            ),
        );
        assert!(drifts.is_empty(), "{drifts:?}");
    }

    /// The escape hatch must cost something, or it becomes the default.
    #[test]
    fn a_trailer_without_a_reason_does_not_clear_the_drift() {
        let drifts = detect_drift(
            &index(),
            &changes(&["src/manager.rs"], &["Diagram-Unchanged: d1"]),
        );
        assert_eq!(drifts.len(), 1, "{drifts:?}");
    }

    #[test]
    fn a_trailer_naming_another_diagram_does_not_clear_this_one() {
        let drifts = detect_drift(
            &index(),
            &changes(
                &["src/manager.rs"],
                &["Diagram-Unchanged: other — unrelated"],
            ),
        );
        assert_eq!(drifts.len(), 1, "{drifts:?}");
    }

    #[test]
    fn another_repositorys_glob_never_triggers_local_drift() {
        // `backend:src/api/**` must not fire on this repository's paths.
        let drifts = detect_drift(&index(), &changes(&["src/api/thing.rs"], &[]));
        assert_eq!(drifts.len(), 1, "fires once, via nexus:src/**: {drifts:?}");
    }

    /// A `planned` entry has no `.mmd`, so drift against it could only be
    /// cleared by the escape hatch — which would make the hatch routine for
    /// every domain still awaiting its diagram. Its paths are reported as
    /// unowned instead, which is the honest state: no diagram owns them yet.
    #[test]
    fn a_planned_entry_never_demands_a_diagram_that_does_not_exist() {
        let index = parse_index(
            "diagrams:\n  - name: planned_one\n    status: planned\n    covers:\n      \
             - \"nexus:src/**\"\n",
        )
        .expect("fixture index is valid");
        let set = changes(&["src/thing.rs"], &[]);
        assert!(
            detect_drift(&index, &set).is_empty(),
            "must not demand a missing file"
        );
        assert_eq!(
            unowned_paths(&index, &set),
            vec!["src/thing.rs"],
            "reported as unowned instead"
        );
    }

    #[test]
    fn an_uncovered_change_does_not_drift_but_is_listed_as_unowned() {
        let set = changes(&["README.md"], &[]);
        assert!(detect_drift(&index(), &set).is_empty());
        assert_eq!(unowned_paths(&index(), &set), vec!["README.md"]);
    }

    #[test]
    fn a_covered_change_is_not_listed_as_unowned() {
        let set = changes(&["src/manager.rs"], &[]);
        assert!(unowned_paths(&index(), &set).is_empty());
    }

    #[test]
    fn an_empty_change_set_drifts_nothing() {
        assert!(detect_drift(&index(), &changes(&[], &[])).is_empty());
    }
}

#[cfg(test)]
mod ownership_rules {
    use super::*;

    fn index() -> Vec<IndexEntry> {
        parse_index(
            "diagrams:\n  - name: d1\n    file: docs/diagrams/d1.mmd\n    status: verified\n    \
             covers:\n      - \"nexus:claude-code-api/src/api/**\"\n  - name: d2\n    \
             status: planned\n    covers:\n      - \"nexus:src/other.rs\"\n",
        )
        .expect("fixture index is valid")
    }

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_covered_source_file_is_not_an_orphan() {
        let found = orphans_among(&index(), paths(&["claude-code-api/src/api/routes.rs"]));
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn an_uncovered_source_file_is_an_orphan() {
        let found = orphans_among(&index(), paths(&["claude-code-api/src/core/manager.rs"]));
        assert_eq!(found, vec!["claude-code-api/src/core/manager.rs"]);
    }

    #[test]
    fn only_a_planned_entrys_globs_do_not_protect_a_file() {
        // `d2` is planned, so it has no diagram yet. Its globs still appear in
        // the index, and they must NOT excuse a file: counting a planned entry
        // as an owner would let the ceiling fall without a single diagram
        // being written.
        let found = orphans_among(&index(), paths(&["src/other.rs"]));
        assert_eq!(
            found,
            vec!["src/other.rs"],
            "a planned entry owns nothing yet"
        );
    }

    #[test]
    fn examples_and_tests_are_not_counted() {
        let found = orphans_among(
            &index(),
            paths(&[
                "claude-code-sdk-rs/examples/basic.rs",
                "claude-code-sdk-rs/tests/e2e_hooks.rs",
                "claude-code-api/tests/diagram_index.rs",
            ]),
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_non_rust_file_is_not_counted() {
        let found = orphans_among(&index(), paths(&["claude-code-api/src/config.toml"]));
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn an_indexed_diagram_is_not_reported_as_unindexed() {
        let found = unindexed_diagrams(&index(), &paths(&["d1"]));
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_diagram_with_no_entry_is_reported() {
        let found = unindexed_diagrams(&index(), &paths(&["d1", "stray"]));
        assert_eq!(found, vec!["stray"]);
    }

    #[test]
    fn a_diagram_whose_entry_is_only_planned_is_reported() {
        // A file exists while the index still calls it planned: the two
        // disagree, and the index is the one claiming nothing was verified.
        let found = unindexed_diagrams(&index(), &paths(&["d2"]));
        assert_eq!(found, vec!["d2"]);
    }
}

// ---------------------------------------------------------------------------
// The gate proper: this repository must obey its own charter
// ---------------------------------------------------------------------------

#[test]
fn the_index_parses() {
    parse_index(&read_repo_file(INDEX_PATH))
        .expect("INDEX.yml must obey the charter's YAML subset");
}

#[test]
fn every_entry_has_a_name_and_a_known_status() {
    let entries = parse_index(&read_repo_file(INDEX_PATH)).expect("index parses");
    let mut seen = BTreeSet::new();
    for entry in &entries {
        assert!(!entry.name.is_empty(), "an entry has no name");
        assert!(
            seen.insert(entry.name.clone()),
            "{} is declared twice",
            entry.name
        );
        assert!(
            matches!(entry.status.as_str(), "planned" | "verified"),
            "{} has status {:?}, expected planned or verified",
            entry.name,
            entry.status
        );
        assert!(
            !entry.covers.is_empty(),
            "{} covers nothing, so it owns nothing",
            entry.name
        );
    }
}

#[test]
fn a_planned_entry_has_no_file_and_a_verified_one_does() {
    let entries = parse_index(&read_repo_file(INDEX_PATH)).expect("index parses");
    for entry in &entries {
        match entry.status.as_str() {
            "planned" => assert!(
                entry.file.is_none(),
                "{} is planned but names a file: a file means its content was \
                 reviewed against the code, which is what verified records",
                entry.name
            ),
            "verified" => {
                let file = entry
                    .file
                    .as_deref()
                    .unwrap_or_else(|| panic!("{} is verified but names no file", entry.name));
                assert_eq!(
                    file,
                    format!("docs/diagrams/{}.mmd", entry.name),
                    "{}: the charter fixes the path from the name",
                    entry.name
                );
                assert!(
                    entry.verified.is_some(),
                    "{} is verified but records no sha",
                    entry.name
                );
                assert!(
                    repo_root().join(file).is_file(),
                    "{} is verified but {file} does not exist",
                    entry.name
                );
            },
            _ => unreachable!("status already validated"),
        }
    }
}

#[test]
fn every_verified_diagram_obeys_the_header_rules() {
    let entries = parse_index(&read_repo_file(INDEX_PATH)).expect("index parses");
    for entry in entries.iter().filter(|e| e.status == "verified") {
        let file = entry.file.as_deref().expect("verified entry has a file");
        let mmd = read_repo_file(file);
        let header = parse_header(&mmd)
            .unwrap_or_else(|e| panic!("{file}: header does not obey the charter: {e}"));

        assert_eq!(
            header.name, entry.name,
            "{file}: `%% name:` must equal the index entry and the file name"
        );
        assert_eq!(
            header.covers, entry.covers,
            "{file}: `%% covers:` and the index `covers:` must be identical, \
             or the two disagree about what the diagram owns"
        );
        assert_eq!(
            Some(header.verified.as_str()),
            entry.verified.as_deref(),
            "{file}: the header sha and the index sha must agree"
        );

        let unmarked = nodes_without_status(&mmd);
        assert!(
            unmarked.is_empty(),
            "{file}: these nodes carry no status mark, which the charter calls \
             a review error — each asserts something about the code while \
             saying nothing about whether anyone checked it: {unmarked:?}"
        );

        let line_citations = nodes_citing_line_numbers(&mmd);
        assert!(
            line_citations.is_empty(),
            "{file}: these nodes cite a line number instead of a name, and a \
             line number is wrong as soon as a neighbouring commit lands: \
             {line_citations:?}"
        );

        let traps = mermaid_traps(&mmd);
        assert!(
            traps.is_empty(),
            "{file}: constructs that render wrong or not at all: {traps:?}"
        );
    }
}

/// Where the orphan ceiling is recorded.
const ORPHANS_PATH: &str = "docs/diagrams/ORPHANS.md";

/// Source files (`.rs` under a `src/`) that no diagram's `covers` claims.
///
/// Examples and tests are excluded on purpose: an example exists to be read
/// and a test to be executed, so neither is logic a diagram would describe.
/// Counting them would inflate the gap with files that will never have an
/// owner, and a metric nobody can ever drive to zero gets ignored.
fn orphan_source_files(entries: &[IndexEntry]) -> Vec<String> {
    orphans_among(entries, repo_relative_paths())
}

/// The pure core of [`orphan_source_files`], taking the path list as an
/// argument.
///
/// Split out so the selection rule can be tested against fixtures. Left inside
/// the repository walk it was only ever proven by running the gate against the
/// real tree and reading the number, which demonstrates nothing about the rule
/// and would survive a refactor that broke it.
pub fn orphans_among(entries: &[IndexEntry], paths: Vec<String>) -> Vec<String> {
    let globs = owning_globs(entries);
    let mut orphans: Vec<String> = paths
        .into_iter()
        .filter(|p| is_countable_source(p))
        .filter(|p| !globs.iter().any(|glob| glob_matches(glob, p)))
        .collect();
    orphans.sort();
    orphans
}

/// Whether a path counts towards the orphan ceiling.
///
/// Rust sources under a `src/`, and nothing else. An `examples/` or `tests/`
/// file is excluded by not being under `src/` in the first place.
fn is_countable_source(path: &str) -> bool {
    path.ends_with(".rs") && (path.starts_with("src/") || path.contains("/src/"))
}

/// Diagram files present on disk with no `status: verified` index entry.
///
/// Pure, for the same reason as [`orphans_among`]: the on-disk variant was
/// proven only by dropping a file in `docs/diagrams/` by hand.
pub fn unindexed_diagrams(entries: &[IndexEntry], stems: &[String]) -> Vec<String> {
    let verified: BTreeSet<&str> = entries
        .iter()
        .filter(|e| e.status == "verified")
        .map(|e| e.name.as_str())
        .collect();
    let mut unindexed: Vec<String> = stems
        .iter()
        .filter(|stem| !verified.contains(stem.as_str()))
        .cloned()
        .collect();
    unindexed.sort();
    unindexed
}

/// Names (without extension) of the `.mmd` files in `docs/diagrams/`.
fn diagram_stems_on_disk() -> Vec<String> {
    let dir = repo_root().join("docs/diagrams");
    fs::read_dir(&dir)
        .expect("docs/diagrams must exist")
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("mmd") {
                return None;
            }
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .collect()
}

/// Read `<!-- orphan-ceiling: N -->` from the published list.
fn orphan_ceiling(doc: &str) -> Option<usize> {
    let at = doc.find("<!-- orphan-ceiling:")?;
    let rest = &doc[at + "<!-- orphan-ceiling:".len()..];
    let end = rest.find("-->")?;
    rest[..end].trim().parse().ok()
}

#[test]
fn the_orphan_count_never_grows() {
    // The charter wants every source file owned by a diagram. Most are not yet
    // (task 0.2), so a hard "zero orphans" assertion would fail on day one and
    // get deleted. A ceiling that can only descend turns the same goal into
    // something a pull request can be held to today: claim a file, or at least
    // do not add another unowned one.
    //
    // Regenerate the list behind the ceiling with:
    //   cargo test -p claude-code-api --test diagram_index -- --nocapture \
    //     the_orphan_count_never_grows
    let entries = parse_index(&read_repo_file(INDEX_PATH)).expect("index parses");
    let orphans = orphan_source_files(&entries);
    let doc = read_repo_file(ORPHANS_PATH);
    let ceiling = orphan_ceiling(&doc)
        .unwrap_or_else(|| panic!("{ORPHANS_PATH} carries no `orphan-ceiling` marker"));

    println!("orphan source files ({}):", orphans.len());
    for path in &orphans {
        println!("  {path}");
    }

    assert!(
        orphans.len() <= ceiling,
        "{} source files have no owning diagram, above the ceiling of {ceiling} \
         recorded in {ORPHANS_PATH}. Give the new file a diagram by adding it \
         to a `covers` glob — raising the ceiling is not the way out.",
        orphans.len()
    );
}

#[test]
fn the_orphan_ceiling_marker_is_readable() {
    // Negative controls for the parser, so a typo in the marker cannot turn
    // the ratchet off by making the ceiling unreadable.
    assert_eq!(orphan_ceiling("<!-- orphan-ceiling: 42 -->"), Some(42));
    assert_eq!(
        orphan_ceiling("text <!-- orphan-ceiling:7--> more"),
        Some(7)
    );
    assert_eq!(orphan_ceiling("<!-- orphan-ceiling: -->"), None);
    assert_eq!(orphan_ceiling("<!-- orphan-ceiling: many -->"), None);
    assert_eq!(orphan_ceiling("no marker at all"), None);
}

#[test]
fn every_diagram_file_on_disk_is_indexed_as_verified() {
    // The other direction of the index check. Without this a `.mmd` can sit in
    // docs/diagrams/ owning nothing: no `covers`, so the drift check never
    // fires for it, and no owner, so nobody maintains it. The charter's
    // lifecycle says creating a diagram means adding its entry in the same
    // pull request; this is what makes that more than advice.
    let entries = parse_index(&read_repo_file(INDEX_PATH)).expect("index parses");
    let unindexed = unindexed_diagrams(&entries, &diagram_stems_on_disk());
    assert!(
        unindexed.is_empty(),
        "these diagrams exist in docs/diagrams/ but have no `status: verified` \
         entry in {INDEX_PATH}: {unindexed:?}. Add an entry with its `covers` \
         globs in this same pull request — an unindexed diagram owns nothing, \
         so the drift check can never fire for it and nobody is its owner."
    );
}

#[test]
fn every_local_glob_matches_something_that_exists() {
    // A glob written from prose rather than from the tree silently owns
    // nothing — the diagram then looks authoritative over code it never
    // covered. This is the check that catches it.
    let entries = parse_index(&read_repo_file(INDEX_PATH)).expect("index parses");
    let paths = repo_relative_paths();
    let mut orphans: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in &entries {
        for glob in &entry.covers {
            let Some(local) = local_glob(glob) else {
                continue;
            };
            if !paths.iter().any(|path| glob_matches(local, path)) {
                orphans
                    .entry(entry.name.clone())
                    .or_default()
                    .push(glob.clone());
            }
        }
    }
    assert!(
        orphans.is_empty(),
        "these `covers` globs match no file in this repository: {orphans:?}"
    );
}

#[test]
fn drift_is_reported_when_ci_supplies_a_change_set() {
    let entries = parse_index(&read_repo_file(INDEX_PATH)).expect("index parses");
    let Some(changes) = change_set_from_env() else {
        // Local run: there is no pull request to judge. Say so out loud rather
        // than passing silently, so a green test is never mistaken for "no
        // drift" when the check simply did not run.
        eprintln!(
            "diagram drift: no DIAGRAM_DRIFT_CHANGED_FILES in the environment, \
             so only the charter invariants were checked. CI supplies the \
             change set; see .github/workflows/ci.yml."
        );
        return;
    };

    let unowned = unowned_paths(&entries, &changes);
    if !unowned.is_empty() {
        // Reported, never failed: until every file has an owning diagram
        // (task 0.2) failing here would block every pull request, and a gate
        // nobody can satisfy gets switched off instead of obeyed.
        eprintln!("diagram drift: these changed files have no owning diagram yet: {unowned:?}");
    }

    let drifts = detect_drift(&entries, &changes);
    assert!(
        drifts.is_empty(),
        "these diagrams own a file this change touched but were not updated: \
         {drifts:?}. Either change the .mmd in this pull request, or add a \
         commit trailer `Diagram-Unchanged: <name> — <reason>` saying why the \
         diagram still holds."
    );
}

/// Every repo-relative file path, for glob checking.
fn repo_relative_paths() -> Vec<String> {
    const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".fastembed_cache"];
    let root = repo_root();
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(path);
                }
                continue;
            }
            if let Ok(relative) = path.strip_prefix(&root) {
                // Normalise to forward slashes so the same globs work on Windows.
                out.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out
}
