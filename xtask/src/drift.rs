//! Fail a pull request that changes code a diagram owns without saying so.
//!
//! WHY. The index proves the ownership map is well formed. It says nothing
//! about whether a diagram still DESCRIBES its files. A diagram goes wrong the
//! day someone edits the code under it and nobody looks at the picture. This
//! gate makes that moment visible.
//!
//! THE RULE. For every file a pull request changes, find the diagrams whose
//! `covers` match it. For each such diagram, the same pull request must either
//! change `docs/diagrams/<name>.mmd`, or carry, in one of its commit messages,
//!
//! ```text
//! Diagram-Unchanged: <name> — <reason>
//! ```
//!
//! which is the author stating they looked and the diagram is still exact. The
//! reason is mandatory: a trailer without one is a way to switch the gate off,
//! not an answer to it.
//!
//! WHAT IT DOES NOT PROVE. That the diagram is right. A touched `.mmd` or a
//! trailer is a claim by the author, checked by the reviewer. The gate only
//! guarantees the question was asked.

use crate::{DIAGRAM_DIR, glob};
use std::collections::{BTreeMap, BTreeSet};

/// The commit-message trailer that answers the gate.
pub const TRAILER_KEY: &str = "Diagram-Unchanged:";

/// A diagram and what it owns.
#[derive(Debug, Clone)]
pub struct Owner {
    pub name: String,
    /// Repository path of the `.mmd`.
    pub file: String,
    pub covers: Vec<String>,
}

/// What [`find`] found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Drift {
    pub problems: Vec<String>,
    /// Changed Rust sources no diagram owns: listed, never an error.
    pub unowned: Vec<String>,
}

/// The `Diagram-Unchanged:` lines of the commit messages, as `name → reason`,
/// and the lines that have the key but not the shape.
pub fn parse_trailers(messages: &[String]) -> (BTreeMap<String, String>, Vec<String>) {
    let mut stated = BTreeMap::new();
    let mut problems = Vec::new();
    for line in messages.iter().flat_map(|m| m.lines()).map(str::trim) {
        let Some(rest) = line.strip_prefix(TRAILER_KEY) else {
            continue;
        };
        match split_trailer(rest.trim()) {
            Some((name, reason)) => {
                stated.insert(name.to_owned(), reason.to_owned());
            },
            None => problems.push(format!(
                "trailer `{line}` is not `{TRAILER_KEY} <name> — <reason>`: the reason is mandatory"
            )),
        }
    }
    (stated, problems)
}

/// `<name> <dash> <reason>` → `(name, reason)`; the dash is `—`, `–`, `-` or `--`.
fn split_trailer(rest: &str) -> Option<(&str, &str)> {
    let (name, after) = rest.split_once(char::is_whitespace)?;
    let (dash, reason) = after.trim_start().split_once(char::is_whitespace)?;
    let reason = reason.trim();
    (matches!(dash, "—" | "–" | "-" | "--") && !reason.is_empty()).then_some((name, reason))
}

/// The rule, as a pure function over the changed paths.
pub fn find(changed: &[String], owners: &[Owner], trailers: &BTreeMap<String, String>) -> Drift {
    let mut drift = Drift::default();
    let changed_set: BTreeSet<&str> = changed.iter().map(String::as_str).collect();

    for name in trailers.keys() {
        if !owners.iter().any(|o| o.name == *name) {
            drift.problems.push(format!(
                "trailer `{TRAILER_KEY} {name}` names no diagram in {DIAGRAM_DIR}/"
            ));
        }
    }

    let mut owned = BTreeSet::new();
    for owner in owners {
        let touched: BTreeSet<&str> = changed_set
            .iter()
            .copied()
            .filter(|f| owner.covers.iter().any(|p| glob::matches(p, f)))
            .collect();
        owned.extend(touched.iter().copied());
        let answered =
            changed_set.contains(owner.file.as_str()) || trailers.contains_key(&owner.name);
        if touched.is_empty() || answered {
            continue;
        }
        let touched: Vec<&str> = touched.into_iter().collect();
        drift.problems.push(format!(
            "`{}` owns {} — changed without touching {}. Update the diagram, or state \
             `{TRAILER_KEY} {} — <reason>` in a commit message.",
            owner.name,
            touched.join(", "),
            owner.file,
            owner.name
        ));
    }

    let diagram_dir = format!("{DIAGRAM_DIR}/");
    drift.unowned = changed_set
        .iter()
        .filter(|f| !owned.contains(*f) && f.ends_with(".rs") && !f.starts_with(&diagram_dir))
        .map(|f| (*f).to_owned())
        .collect();
    drift
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owners() -> Vec<Owner> {
        vec![
            Owner {
                name: "transport".into(),
                file: "docs/diagrams/transport.mmd".into(),
                covers: vec!["sdk/src/transport/*.rs".into()],
            },
            Owner {
                name: "api".into(),
                file: "docs/diagrams/api.mmd".into(),
                covers: vec!["api/src/**/*.rs".into(), "api/build.rs".into()],
            },
        ]
    }

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|p| (*p).to_owned()).collect()
    }

    fn trailers(list: &[(&str, &str)]) -> BTreeMap<String, String> {
        list.iter()
            .map(|(n, r)| ((*n).to_owned(), (*r).to_owned()))
            .collect()
    }

    #[test]
    fn trailers_are_read_with_any_dash() {
        let (stated, problems) = parse_trailers(&paths(&[
            "fix: x\n\nDiagram-Unchanged: transport — comment only\n",
            "Diagram-Unchanged: api - rename of a private helper",
            "  Diagram-Unchanged: other -- typo\r\nDiagram-Unchanged: last – en dash",
        ]));
        assert_eq!(problems, Vec::<String>::new());
        assert_eq!(
            stated,
            trailers(&[
                ("transport", "comment only"),
                ("api", "rename of a private helper"),
                ("other", "typo"),
                ("last", "en dash"),
            ])
        );
    }

    #[test]
    fn a_trailer_without_a_reason_is_a_problem_not_a_pass() {
        for line in [
            "Diagram-Unchanged: transport",
            "Diagram-Unchanged: transport —",
            "Diagram-Unchanged: transport —   ",
            "Diagram-Unchanged:",
            "Diagram-Unchanged: transport because I said so",
        ] {
            let (stated, problems) = parse_trailers(&paths(&[line]));
            assert!(stated.is_empty(), "{line}");
            assert_eq!(problems.len(), 1, "{line}");
            assert!(problems[0].contains("reason is mandatory"), "{line}");
        }
    }

    #[test]
    fn other_lines_are_ignored() {
        let (stated, problems) = parse_trailers(&paths(&[
            "Co-Authored-By: x\n\nprose about Diagram-Unchanged",
        ]));
        assert!(stated.is_empty() && problems.is_empty());
    }

    #[test]
    fn a_covered_file_changed_alone_is_drift() {
        let drift = find(
            &paths(&["sdk/src/transport/mod.rs"]),
            &owners(),
            &trailers(&[]),
        );
        assert_eq!(drift.problems.len(), 1);
        assert!(drift.problems[0].starts_with("`transport` owns sdk/src/transport/mod.rs — changed without touching docs/diagrams/transport.mmd."));
        assert!(drift.unowned.is_empty());
    }

    #[test]
    fn touching_the_diagram_answers_it() {
        let changed = paths(&["sdk/src/transport/mod.rs", "docs/diagrams/transport.mmd"]);
        assert_eq!(find(&changed, &owners(), &trailers(&[])), Drift::default());
    }

    #[test]
    fn the_trailer_answers_it() {
        let stated = trailers(&[("transport", "comment")]);
        let drift = find(&paths(&["sdk/src/transport/mod.rs"]), &owners(), &stated);
        assert_eq!(drift, Drift::default());
    }

    #[test]
    fn one_diagram_answered_does_not_excuse_another() {
        let changed = paths(&[
            "sdk/src/transport/mod.rs",
            "api/build.rs",
            "docs/diagrams/transport.mmd",
        ]);
        let drift = find(&changed, &owners(), &trailers(&[]));
        assert_eq!(drift.problems.len(), 1);
        assert!(drift.problems[0].starts_with("`api` owns api/build.rs"));
    }

    #[test]
    fn a_deleted_covered_file_still_counts() {
        // The path is matched, not the disk: nothing here exists.
        let drift = find(&paths(&["api/src/gone/old.rs"]), &owners(), &trailers(&[]));
        assert_eq!(drift.problems.len(), 1);
    }

    #[test]
    fn a_trailer_naming_no_diagram_is_a_problem() {
        let drift = find(&[], &owners(), &trailers(&[("trnasport", "typo")]));
        assert_eq!(
            drift.problems,
            vec![
                "trailer `Diagram-Unchanged: trnasport` names no diagram in docs/diagrams/"
                    .to_owned()
            ]
        );
    }

    #[test]
    fn unowned_sources_are_listed_not_failed() {
        let changed = paths(&[
            "cli/src/main.rs",
            "README.md",
            "docs/diagrams/api.mmd",
            "docs/diagrams/x.rs",
        ]);
        let drift = find(&changed, &owners(), &trailers(&[]));
        assert!(drift.problems.is_empty());
        assert_eq!(drift.unowned, paths(&["cli/src/main.rs"]));
    }
}
