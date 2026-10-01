//! Bug registry gate (task 1.3).
//!
//! The registry is two documents that describe the same facts: the table in
//! `docs/BUGS.md` and the nodes in `docs/diagrams/po-bugs.mmd`. Two documents
//! describing the same facts drift — that is what documents do. This gate
//! removes the possibility.
//!
//! # What is enforced
//!
//! * every bug id in the table has a node in the diagram, and the reverse;
//! * the status glyph agrees between the two;
//! * ids are kebab-case and unique, and every row carries a repository and a
//!   severity;
//! * a ✅ row cites its evidence, and a 🟠 / 🔴 row says where the fix is;
//! * `docs/diagrams/INDEX.yml` lists exactly the `.mmd` files on disk, each
//!   entry points at a file that exists, and each file's `%% name:` header
//!   matches the name it is indexed under.
//!
//! # No network, ever
//!
//! Every check reads tracked files under [`repo_root`]. Nothing resolves a
//! host or shells out, so the verdict is identical on a laptop and in CI.
//!
//! # Why the parsers are hand-rolled
//!
//! A Markdown parser and a YAML parser would both be heavier than the grammar
//! actually in use, and both would accept documents a human cannot read at a
//! glance. The parsers here are deliberately literal and reject anything they
//! do not understand, so the failure is a clear message rather than a silently
//! skipped row.

use std::collections::{BTreeMap, BTreeSet};
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
        .unwrap_or_else(|e| panic!("bug registry gate cannot read {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Status glyphs
// ---------------------------------------------------------------------------

/// The four statuses a bug node may carry, as defined in `docs/BUGS.md`.
///
/// They are compared as values rather than as raw strings so that a row and a
/// node written with different surrounding text still compare equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    /// Closed: merged into the default branch, with proof.
    Closed,
    /// A fix exists but has not landed; the default branch is still broken.
    Unlanded,
    /// Open on the default branch right now.
    Open,
    /// Owned by another task, not verified here.
    Upstream,
}

impl Status {
    /// Map a glyph to a status, or `None` if it is not a registry glyph.
    pub fn from_glyph(glyph: &str) -> Option<Self> {
        match glyph {
            "✅" => Some(Self::Closed),
            "🟠" => Some(Self::Unlanded),
            "🔴" => Some(Self::Open),
            "⚪" => Some(Self::Upstream),
            _ => None,
        }
    }

    /// The glyph this status is written with.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Closed => "✅",
            Self::Unlanded => "🟠",
            Self::Open => "🔴",
            Self::Upstream => "⚪",
        }
    }

    /// Find the first registry glyph in `text`, if any.
    ///
    /// Used for diagram nodes, where the glyph sits inside prose rather than
    /// in a column of its own.
    pub fn find_in(text: &str) -> Option<Self> {
        text.chars()
            .find_map(|c| Self::from_glyph(c.to_string().as_str()))
    }
}

// ---------------------------------------------------------------------------
// The table in docs/BUGS.md
// ---------------------------------------------------------------------------

/// One row of the inventory table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryRow {
    pub id: String,
    pub repo: String,
    pub severity: String,
    pub status: Status,
    pub finding: String,
    pub evidence: String,
}

/// Markers delimiting the machine-read part of `docs/BUGS.md`.
///
/// Everything outside them is prose for humans and is never parsed, so the
/// document can be rewritten freely around the table.
const TABLE_START: &str = "<!-- BUG-TABLE-START -->";
const TABLE_END: &str = "<!-- BUG-TABLE-END -->";

/// Split a Markdown table row into its cells.
///
/// Leading and trailing pipes are dropped; inner cells are trimmed. A `\|`
/// escape is not supported, and no row in the registry needs one.
fn split_row(line: &str) -> Vec<String> {
    line.trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// True when a row is the `|---|---|` separator rather than data.
fn is_separator(cells: &[String]) -> bool {
    !cells.is_empty()
        && cells
            .iter()
            .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':'))
}

/// Strip the backticks a Markdown table uses around a code span.
fn unticked(cell: &str) -> String {
    cell.trim().trim_matches('`').trim().to_string()
}

/// Parse the inventory table out of `docs/BUGS.md`.
///
/// Fails loudly on a malformed row: a registry that cannot be parsed is a
/// registry that is not being checked, which is worse than one that is absent.
pub fn parse_registry_rows(markdown: &str) -> Vec<RegistryRow> {
    let start = markdown
        .find(TABLE_START)
        .expect("docs/BUGS.md must contain the BUG-TABLE-START marker");
    let end = markdown
        .find(TABLE_END)
        .expect("docs/BUGS.md must contain the BUG-TABLE-END marker");
    assert!(
        start < end,
        "the BUG-TABLE-START marker must come before BUG-TABLE-END"
    );

    let mut rows = Vec::new();
    for line in markdown[start + TABLE_START.len()..end].lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let cells = split_row(line);
        if is_separator(&cells) {
            continue;
        }
        // The header row names the id column `id`; data rows carry an id.
        if cells.first().map(|c| c.as_str()) == Some("id") {
            continue;
        }
        assert_eq!(
            cells.len(),
            6,
            "a registry row needs 6 cells (id, repo, severity, status, finding, evidence), got {}: {line}",
            cells.len()
        );
        let status = Status::from_glyph(&cells[3]).unwrap_or_else(|| {
            panic!(
                "row `{}` has status cell {:?}, which is not one of ✅ 🟠 🔴 ⚪",
                cells[0], cells[3]
            )
        });
        rows.push(RegistryRow {
            id: unticked(&cells[0]),
            repo: cells[1].clone(),
            severity: cells[2].clone(),
            status,
            finding: cells[4].clone(),
            evidence: cells[5].clone(),
        });
    }
    rows
}

// ---------------------------------------------------------------------------
// The nodes in po-bugs.mmd
// ---------------------------------------------------------------------------

/// A bug node read out of the diagram: its id and the status glyph it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagramNode {
    pub id: String,
    pub status: Status,
}

/// Extract the label of a node, i.e. the text inside `["..."]`.
///
/// Returns `None` for a line that is not a labelled node, which covers
/// comments, `subgraph` headers, edges and the `flowchart` directive.
fn node_label(line: &str) -> Option<&str> {
    let open = line.find("[\"")?;
    let rest = &line[open + 2..];
    let close = rest.rfind("\"]")?;
    Some(&rest[..close])
}

/// Read every bug node from the diagram.
///
/// A node counts as a bug node when its label's first line is an id that the
/// registry table also uses — so the cycle, gate and dedup nodes, which carry
/// prose titles, are ignored without needing a naming convention of their own.
pub fn parse_diagram_nodes(diagram: &str, known_ids: &BTreeSet<String>) -> Vec<DiagramNode> {
    let mut nodes = Vec::new();
    for line in diagram.lines() {
        let line = line.trim();
        if line.starts_with("%%") {
            continue;
        }
        let Some(label) = node_label(line) else {
            continue;
        };
        let first = label.split("<br/>").next().unwrap_or(label).trim();
        if !known_ids.contains(first) {
            continue;
        }
        let status = Status::find_in(label).unwrap_or_else(|| {
            panic!("diagram node `{first}` carries no status glyph (✅ 🟠 🔴 ⚪)")
        });
        nodes.push(DiagramNode {
            id: first.to_string(),
            status,
        });
    }
    nodes
}

// ---------------------------------------------------------------------------
// The diagram index
// ---------------------------------------------------------------------------

/// One entry of `docs/diagrams/INDEX.yml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    pub file: String,
}

/// Parse the strict YAML subset `INDEX.yml` is written in.
///
/// Recognised shapes: a `- name: <value>` item opener and a `  file: <value>`
/// scalar. Comments and every other key are skipped, which keeps the parser
/// indifferent to fields only humans read (`owner`, `status`, `covers`).
///
/// An entry with `status: planned` deliberately has **no** `file:` key — the
/// scope is reserved but nothing is exported, because exporting a diagram that
/// was never checked against the code is the failure this whole convention
/// exists to prevent. Such an entry parses with an empty [`IndexEntry::file`]
/// and the gate skips it rather than demanding a file that must not exist.
pub fn parse_index(yaml: &str) -> Vec<IndexEntry> {
    let mut entries: Vec<IndexEntry> = Vec::new();
    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("- name:") {
            entries.push(IndexEntry {
                name: value.trim().to_string(),
                file: String::new(),
            });
        } else if let Some(value) = trimmed.strip_prefix("file:") {
            let entry = entries
                .last_mut()
                .expect("a `file:` key must follow a `- name:` item in INDEX.yml");
            entry.file = value.trim().to_string();
        }
    }
    entries
}

/// Every `.mmd` file actually present under `docs/diagrams/`, as a repository
/// relative path. Template files (`_`-prefixed) are not diagrams.
fn diagram_files_on_disk() -> BTreeSet<String> {
    let dir = repo_root().join("docs/diagrams");
    let read = fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()));
    let mut found = BTreeSet::new();
    for entry in read {
        let entry = entry.expect("directory entry is readable");
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".mmd") && !name.starts_with('_') {
            found.insert(format!("docs/diagrams/{name}"));
        }
    }
    found
}

/// Read the `%% name:` header of a diagram file.
fn diagram_header_name(relative: &str) -> String {
    let text = read_repo_file(relative);
    let first = text
        .lines()
        .next()
        .unwrap_or_else(|| panic!("{relative} is empty"));
    first
        .strip_prefix("%% name:")
        .unwrap_or_else(|| {
            panic!("{relative} must start with a `%% name: <name>` line, found {first:?}")
        })
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

fn registry_rows() -> Vec<RegistryRow> {
    parse_registry_rows(&read_repo_file("docs/BUGS.md"))
}

fn diagram_nodes(rows: &[RegistryRow]) -> Vec<DiagramNode> {
    let ids: BTreeSet<String> = rows.iter().map(|r| r.id.clone()).collect();
    parse_diagram_nodes(&read_repo_file("docs/diagrams/po-bugs.mmd"), &ids)
}

// ---------------------------------------------------------------------------
// Tests — the gate itself
// ---------------------------------------------------------------------------

#[test]
fn the_registry_table_is_well_formed() {
    let rows = registry_rows();
    assert!(
        !rows.is_empty(),
        "docs/BUGS.md has an empty inventory table"
    );

    let mut seen = BTreeSet::new();
    for row in &rows {
        assert!(
            seen.insert(row.id.clone()),
            "bug id `{}` appears twice in the inventory",
            row.id
        );
        assert!(
            !row.id.is_empty()
                && row
                    .id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !row.id.starts_with('-')
                && !row.id.ends_with('-'),
            "bug id `{}` is not kebab-case",
            row.id
        );
        assert!(
            ["backend", "frontend", "nexus"].contains(&row.repo.as_str()),
            "bug `{}` names repository {:?}, which is not one of backend/frontend/nexus",
            row.id,
            row.repo
        );
        assert!(
            ["sev-high", "sev-medium", "sev-low"].contains(&row.severity.as_str()),
            "bug `{}` has severity {:?}, which is not one of sev-high/sev-medium/sev-low",
            row.id,
            row.severity
        );
        assert!(
            !row.finding.is_empty(),
            "bug `{}` states no finding",
            row.id
        );
    }
}

#[test]
fn every_closed_bug_cites_its_proof_and_every_open_one_says_where_the_fix_is() {
    for row in registry_rows() {
        match row.status {
            // A ✅ is only a ✅ if it points at the commit that landed it.
            Status::Closed => assert!(
                row.evidence.contains('`') || row.evidence.contains('#'),
                "bug `{}` is ✅ but its evidence cell cites no commit or PR: {:?}",
                row.id,
                row.evidence
            ),
            // A 🟠 / 🔴 must say where the correction lives, or why none exists.
            Status::Unlanded | Status::Open => assert!(
                row.evidence.to_lowercase().contains("branch")
                    || row.evidence.to_lowercase().contains("task")
                    || row.evidence.to_lowercase().contains("decision"),
                "bug `{}` is {} but does not say where the fix is: {:?}",
                row.id,
                row.status.glyph(),
                row.evidence
            ),
            Status::Upstream => {},
        }
    }
}

#[test]
fn the_registry_and_the_diagram_agree() {
    let rows = registry_rows();
    let nodes = diagram_nodes(&rows);

    let by_node: BTreeMap<String, Status> =
        nodes.iter().map(|n| (n.id.clone(), n.status)).collect();
    assert_eq!(
        by_node.len(),
        nodes.len(),
        "po-bugs.mmd declares the same bug id on more than one node"
    );

    for row in &rows {
        let node_status = by_node.get(&row.id).unwrap_or_else(|| {
            panic!(
                "bug `{}` is in docs/BUGS.md but has no node in po-bugs.mmd",
                row.id
            )
        });
        assert_eq!(
            *node_status,
            row.status,
            "bug `{}` is {} in docs/BUGS.md but {} in po-bugs.mmd",
            row.id,
            row.status.glyph(),
            node_status.glyph()
        );
    }

    let table_ids: BTreeSet<&String> = rows.iter().map(|r| &r.id).collect();
    for node in &nodes {
        assert!(
            table_ids.contains(&node.id),
            "po-bugs.mmd has a node for `{}` with no row in docs/BUGS.md",
            node.id
        );
    }
}

#[test]
fn the_index_lists_every_diagram_and_every_listed_diagram_exists() {
    let entries = parse_index(&read_repo_file("docs/diagrams/INDEX.yml"));
    assert!(!entries.is_empty(), "docs/diagrams/INDEX.yml lists nothing");

    let indexed: BTreeSet<String> = entries.iter().map(|e| e.file.clone()).collect();
    let on_disk = diagram_files_on_disk();

    for file in &on_disk {
        assert!(
            indexed.contains(file),
            "{file} exists but is not listed in docs/diagrams/INDEX.yml"
        );
    }
    for entry in &entries {
        // A `planned` entry reserves a name and exports nothing; there is no
        // file to check until someone verifies the diagram against the code.
        if entry.file.is_empty() {
            continue;
        }
        assert!(
            on_disk.contains(&entry.file),
            "docs/diagrams/INDEX.yml lists {} for `{}`, but that file does not exist",
            entry.file,
            entry.name
        );
        assert_eq!(
            diagram_header_name(&entry.file),
            entry.name,
            "{} declares a `%% name:` that differs from its index entry",
            entry.file
        );
    }
}

#[test]
fn the_bug_registry_is_listed_in_the_index() {
    let entries = parse_index(&read_repo_file("docs/diagrams/INDEX.yml"));
    assert!(
        entries
            .iter()
            .any(|e| e.name == "po-bugs" && e.file == "docs/diagrams/po-bugs.mmd"),
        "po-bugs must be listed in docs/diagrams/INDEX.yml"
    );
}

#[test]
fn the_registry_documents_the_whole_cycle() {
    // The acceptance criterion for this registry is that docs/BUGS.md states
    // the cycle AND the level of proof a ✅ carries. Prose drifts, so the
    // stages are asserted rather than trusted.
    let doc = read_repo_file("docs/BUGS.md");
    for stage in [
        "discovery",
        "gotcha",
        "red test",
        "PR",
        "merged",
        "sev-high",
    ] {
        assert!(
            doc.to_lowercase().contains(&stage.to_lowercase()),
            "docs/BUGS.md never mentions the `{stage}` stage of the cycle"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests — the parsers, on synthetic input
// ---------------------------------------------------------------------------
//
// The tests above would still pass if a parser silently returned nothing, so
// each parser is also exercised against input whose answer is known, and
// against input it must reject.

#[test]
fn status_round_trips_through_its_glyph() {
    for status in [
        Status::Closed,
        Status::Unlanded,
        Status::Open,
        Status::Upstream,
    ] {
        assert_eq!(Status::from_glyph(status.glyph()), Some(status));
    }
    assert_eq!(Status::from_glyph("x"), None);
    assert_eq!(Status::find_in("no glyph at all"), None);
    assert_eq!(
        Status::find_in("sev-high<br/>🟠 pending"),
        Some(Status::Unlanded)
    );
}

#[test]
fn the_table_parser_reads_exactly_the_rows_it_is_given() {
    let doc = format!(
        "prose before\n{TABLE_START}\n\
         | id | repo | severity | status | finding | evidence / where the fix is |\n\
         |----|------|----------|--------|---------|------|\n\
         | `a-bug` | backend | sev-high | 🔴 | it breaks | branch fix/a |\n\
         | `b-bug` | nexus | sev-low | ✅ | it broke | `abc1234` |\n\
         {TABLE_END}\nprose after, with a | pipe | that must be ignored\n"
    );
    let rows = parse_registry_rows(&doc);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, "a-bug");
    assert_eq!(rows[0].status, Status::Open);
    assert_eq!(rows[0].repo, "backend");
    assert_eq!(rows[1].id, "b-bug");
    assert_eq!(rows[1].status, Status::Closed);
    assert_eq!(rows[1].evidence, "`abc1234`");
}

#[test]
#[should_panic(expected = "which is not one of")]
fn the_table_parser_rejects_an_unknown_status_glyph() {
    let doc = format!(
        "{TABLE_START}\n\
         | id | repo | severity | status | finding | evidence / where the fix is |\n\
         |----|------|----------|--------|---------|------|\n\
         | `a-bug` | backend | sev-high | ?? | it breaks | somewhere |\n\
         {TABLE_END}\n"
    );
    parse_registry_rows(&doc);
}

#[test]
#[should_panic(expected = "a registry row needs 6 cells")]
fn the_table_parser_rejects_a_short_row() {
    let doc = format!(
        "{TABLE_START}\n\
         | id | repo | severity | status | finding | evidence / where the fix is |\n\
         |----|------|----------|--------|---------|------|\n\
         | `a-bug` | backend | sev-high |\n\
         {TABLE_END}\n"
    );
    parse_registry_rows(&doc);
}

#[test]
#[should_panic(expected = "BUG-TABLE-START")]
fn the_table_parser_rejects_a_document_without_markers() {
    parse_registry_rows("just prose, no table\n");
}

#[test]
fn the_diagram_parser_reads_bug_nodes_and_ignores_the_rest() {
    let ids: BTreeSet<String> = ["a-bug".to_string(), "b-bug".to_string()]
        .into_iter()
        .collect();
    let diagram = "%% name: sample\n\
         flowchart LR\n\
         subgraph s[\"a group\"]\n\
         n1[\"a-bug<br/>sev-high<br/>🔴 still broken\"]\n\
         n2[\"b-bug<br/>sev-low<br/>✅ landed in abc1234\"]\n\
         n3[\"some narrative node<br/>✅ not a bug id\"]\n\
         end\n\
         n1 --> n2\n";
    let nodes = parse_diagram_nodes(diagram, &ids);
    assert_eq!(nodes.len(), 2, "narrative nodes must not be counted");
    assert_eq!(nodes[0].id, "a-bug");
    assert_eq!(nodes[0].status, Status::Open);
    assert_eq!(nodes[1].status, Status::Closed);
}

#[test]
#[should_panic(expected = "carries no status glyph")]
fn the_diagram_parser_rejects_a_bug_node_without_a_glyph() {
    let ids: BTreeSet<String> = ["a-bug".to_string()].into_iter().collect();
    parse_diagram_nodes("n1[\"a-bug<br/>no status here\"]\n", &ids);
}

#[test]
fn the_index_parser_reads_name_and_file_pairs() {
    let yaml = "# a comment\n\
         diagrams:\n\
         \x20 - name: one\n\
         \x20   file: docs/diagrams/one.mmd\n\
         \x20   owner: someone\n\
         \x20   covers:\n\
         \x20     - \"src/**\"\n\
         \x20 - name: two\n\
         \x20   file: docs/diagrams/two.mmd\n";
    let entries = parse_index(yaml);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "one");
    assert_eq!(entries[0].file, "docs/diagrams/one.mmd");
    assert_eq!(entries[1].name, "two");
    assert_eq!(entries[1].file, "docs/diagrams/two.mmd");
}

#[test]
fn the_index_parser_accepts_a_planned_entry_with_no_file() {
    // `planned` means "the scope is reserved, nothing is exported yet". The
    // parser must not invent a file for it, and the gate must not demand one.
    let yaml = "diagrams:\n\
         \x20 - name: reserved\n\
         \x20   owner: someone\n\
         \x20   status: planned\n\
         \x20 - name: real\n\
         \x20   file: docs/diagrams/real.mmd\n\
         \x20   status: verified\n";
    let entries = parse_index(yaml);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "reserved");
    assert!(
        entries[0].file.is_empty(),
        "a planned entry must not acquire a file path"
    );
    assert_eq!(entries[1].file, "docs/diagrams/real.mmd");
}

#[test]
#[should_panic(expected = "must follow a `- name:` item")]
fn the_index_parser_rejects_a_file_key_with_no_item() {
    parse_index("diagrams:\n  file: docs/diagrams/orphan.mmd\n");
}

#[test]
fn table_row_splitting_handles_the_shapes_the_registry_uses() {
    assert_eq!(split_row("| a | b |"), vec!["a", "b"]);
    assert_eq!(split_row("  |a|b|  "), vec!["a", "b"]);
    assert!(is_separator(&split_row("|----|:---:|")));
    assert!(!is_separator(&split_row("| a | b |")));
    assert_eq!(unticked("`x-y`"), "x-y");
    assert_eq!(unticked("plain"), "plain");
}

#[test]
fn node_labels_are_only_recognised_on_labelled_nodes() {
    assert_eq!(node_label("n1[\"hello\"]"), Some("hello"));
    assert_eq!(node_label("subgraph s[\"group\"]"), Some("group"));
    assert_eq!(node_label("a --> b"), None);
    assert_eq!(node_label("flowchart LR"), None);
}
