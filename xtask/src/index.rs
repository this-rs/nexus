//! `docs/diagrams/INDEX.yml`, derived from the diagram headers.
//!
//! WHY DERIVE RATHER THAN WRITE IT. The index and the diagrams describe the
//! same facts: which source files each diagram owns. Two documents describing
//! the same facts always drift. So each diagram declares its own scope in its
//! first three lines, and the index is generated. A hand-written index is a
//! second truth, and the first thing it does is disagree.
//!
//! What fails: a malformed header (see [`crate::header`]), a `covers` glob
//! matching NO file — the usual cause is a file that moved, and the diagram
//! silently stops owning anything — and one file claimed by TWO diagrams:
//! ownership must be exclusive, or "who documents this file" has no answer.
//!
//! The source files no diagram claims are listed, not failed: a binary entry
//! point, a `mod.rs` that only declares modules, a `lib.rs` that only
//! re-exports are not diagram material. A real gap is a module with logic that
//! nothing documents, and it belongs in that list so it can be seen.

use crate::{DIAGRAM_DIR, glob, header};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// One diagram of the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    /// File name inside [`DIAGRAM_DIR`].
    pub file: String,
    pub verified: String,
    pub covers: Vec<String>,
}

/// The ownership map and everything wrong with it.
#[derive(Debug, Default)]
pub struct Index {
    pub entries: Vec<Entry>,
    /// File → the diagram that owns it.
    pub owner: BTreeMap<String, String>,
    /// Every Rust source the repository holds, test and example trees aside.
    pub sources: Vec<String>,
    /// The sources no diagram claims.
    pub orphans: Vec<String>,
    pub problems: Vec<String>,
}

/// Is this repository path a Rust source that a diagram could own?
fn is_source(path: &str) -> bool {
    path.ends_with(".rs")
        && path.contains("/src/")
        && !path.contains("/tests/")
        && !path.contains("/examples/")
}

/// Builds the index from the diagrams (`(repository path, text)`, sorted) and
/// the repository's files.
pub fn derive(diagrams: &[(String, String)], files: &[String]) -> Index {
    let mut index = Index::default();
    for (path, text) in diagrams {
        let (parsed, errors) = header::parse(path, text);
        index.problems.extend(errors);
        let Some(head) = parsed else { continue };
        for pattern in &head.covers {
            let hits: Vec<&String> = files.iter().filter(|f| glob::matches(pattern, f)).collect();
            if hits.is_empty() {
                index.problems.push(format!(
                    "{path}: `covers` glob `{pattern}` matches no file — has the code moved?"
                ));
            }
            for hit in hits {
                match index.owner.get(hit) {
                    Some(other) if *other != head.name => index.problems.push(format!(
                        "{hit} is claimed by both `{other}` and `{}`",
                        head.name
                    )),
                    _ => {
                        index.owner.insert(hit.clone(), head.name.clone());
                    },
                }
            }
        }
        index.entries.push(Entry {
            name: head.name,
            file: path.rsplit('/').next().unwrap_or(path).to_owned(),
            verified: head.verified,
            covers: head.covers,
        });
    }
    index.sources = files.iter().filter(|f| is_source(f)).cloned().collect();
    index.sources.sort();
    index.orphans = index
        .sources
        .iter()
        .filter(|s| !index.owner.contains_key(*s))
        .cloned()
        .collect();
    index
}

impl Index {
    /// What a person reads: the counts, the problems, the unclaimed sources.
    pub fn report(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "{} diagrams | {} source files | {} claimed | {} unclaimed",
            self.entries.len(),
            self.sources.len(),
            self.owner.len(),
            self.orphans.len()
        );
        for problem in &self.problems {
            let _ = writeln!(out, "  ERROR {problem}");
        }
        if !self.orphans.is_empty() {
            out.push_str("  unclaimed (not an error — see xtask/src/index.rs):\n");
            for orphan in &self.orphans {
                let _ = writeln!(out, "    {orphan}");
            }
        }
        out
    }

    /// The text of `INDEX.yml`.
    pub fn render(&self) -> String {
        let mut out = format!(
            "# Diagram -> the code it owns. See README.md for the charter.\n\
             #\n\
             # GENERATED from the .mmd headers — do not edit by hand:\n\
             #   cargo xtask diagrams index --write\n\
             #\n\
             # A diagram owns a source file exclusively. A file no diagram\n\
             # claims is listed under `orphans` so the gap is visible rather\n\
             # than implicit; binaries, `mod.rs` and re-exporting `lib.rs`\n\
             # files legitimately live there.\n\
             #\n\
             # {} diagrams, {}/{} source files claimed, {} unclaimed.\n\
             \n\
             diagrams:\n",
            self.entries.len(),
            self.owner.len(),
            self.sources.len(),
            self.orphans.len()
        );
        for entry in &self.entries {
            let _ = writeln!(out, "  - name: {}", entry.name);
            let _ = writeln!(out, "    file: {}", entry.file);
            let _ = writeln!(out, "    verified: {}", entry.verified);
            out.push_str("    covers:\n");
            for pattern in &entry.covers {
                let _ = writeln!(out, "      - {pattern}");
            }
        }
        out.push_str("\norphans:\n");
        if self.orphans.is_empty() {
            out.push_str("  []\n");
        }
        for orphan in &self.orphans {
            let _ = writeln!(out, "  - {orphan}");
        }
        out
    }
}

/// The repository path of a diagram, from its name.
pub fn diagram_path(name: &str) -> String {
    format!("{DIAGRAM_DIR}/{name}.mmd")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagram(name: &str, covers: &str) -> (String, String) {
        (
            diagram_path(name),
            format!(
                "%% name: {name}\n%% covers: {covers}\n%% verified: 2026-10-02\nflowchart TD\n"
            ),
        )
    }

    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn a_clean_tree_yields_owners_orphans_and_a_stable_file() {
        let tree = files(&[
            "api/src/a.rs",
            "api/src/bin/main.rs",
            "api/tests/t.rs",
            "api/examples/src/e.rs",
            "api/build.rs",
            "README.md",
        ]);
        let index = derive(&[diagram("api", "api/src/*.rs, api/build.rs")], &tree);
        assert_eq!(index.problems, Vec::<String>::new());
        assert_eq!(
            index.sources,
            files(&["api/src/a.rs", "api/src/bin/main.rs"])
        );
        assert_eq!(index.orphans, files(&["api/src/bin/main.rs"]));
        assert_eq!(index.owner.len(), 2);
        assert_eq!(
            index.render(),
            "# Diagram -> the code it owns. See README.md for the charter.\n#\n\
             # GENERATED from the .mmd headers — do not edit by hand:\n\
             #   cargo xtask diagrams index --write\n#\n\
             # A diagram owns a source file exclusively. A file no diagram\n\
             # claims is listed under `orphans` so the gap is visible rather\n\
             # than implicit; binaries, `mod.rs` and re-exporting `lib.rs`\n\
             # files legitimately live there.\n#\n\
             # 1 diagrams, 2/2 source files claimed, 1 unclaimed.\n\n\
             diagrams:\n  - name: api\n    file: api.mmd\n    verified: 2026-10-02\n    covers:\n      - api/src/*.rs\n      - api/build.rs\n\n\
             orphans:\n  - api/src/bin/main.rs\n"
        );
        let report = index.report();
        assert!(
            report.starts_with("1 diagrams | 2 source files | 2 claimed | 1 unclaimed\n"),
            "{report}"
        );
        assert!(report.contains("    api/src/bin/main.rs\n"));
    }

    #[test]
    fn no_orphan_is_written_as_an_empty_list() {
        let index = derive(&[diagram("api", "api/src/*.rs")], &files(&["api/src/a.rs"]));
        assert!(index.render().ends_with("\norphans:\n  []\n"));
        assert!(!index.report().contains("unclaimed (not"));
    }

    #[test]
    fn a_glob_matching_nothing_is_a_problem() {
        let index = derive(
            &[diagram("api", "api/src/moved/*.rs")],
            &files(&["api/src/a.rs"]),
        );
        assert_eq!(index.problems.len(), 1);
        assert!(index.problems[0].contains("matches no file"));
        assert!(index.report().contains("  ERROR docs/diagrams/api.mmd"));
    }

    #[test]
    fn a_file_claimed_twice_is_a_problem_and_once_twice_by_the_same_is_not() {
        let tree = files(&["api/src/a.rs"]);
        let twice = derive(
            &[
                diagram("one", "api/src/*.rs"),
                diagram("two", "api/src/a.rs"),
            ],
            &tree,
        );
        assert_eq!(
            twice.problems,
            vec!["api/src/a.rs is claimed by both `one` and `two`".to_owned()]
        );
        let same = derive(&[diagram("one", "api/src/*.rs, api/src/a.rs")], &tree);
        assert_eq!(same.problems, Vec::<String>::new());
    }

    #[test]
    fn a_malformed_diagram_is_reported_and_left_out() {
        let bad = (diagram_path("bad"), "flowchart TD\n".to_owned());
        let index = derive(
            &[bad, diagram("api", "api/src/*.rs")],
            &files(&["api/src/a.rs"]),
        );
        assert_eq!(index.problems.len(), 3);
        assert_eq!(index.entries.len(), 1);
    }
}
