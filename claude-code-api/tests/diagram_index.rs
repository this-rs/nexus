//! Diagram charter and drift gate for this repository (task 1.1).
//!
//! The charter (`docs/DOCUMENTATION.md` in the main repository) makes the
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
        let diagram_file = entry
            .file
            .clone()
            .unwrap_or_else(|| format!("docs/diagrams/{}.mmd", entry.name));
        if changed.contains(diagram_file.as_str()) || excused.contains(&entry.name) {
            continue;
        }
        let globs: Vec<&str> = entry.covers.iter().filter_map(|g| local_glob(g)).collect();
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
    let globs: Vec<&str> = entries
        .iter()
        .flat_map(|e| e.covers.iter())
        .filter_map(|g| local_glob(g))
        .collect();
    changes
        .changed
        .iter()
        .filter(|path| !globs.iter().any(|glob| glob_matches(glob, path)))
        .cloned()
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
    }
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
