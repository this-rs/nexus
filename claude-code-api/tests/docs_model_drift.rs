//! Fails when the documentation names a model the code does not know, or when a
//! translation silently loses its staleness notice.
//!
//! ## Why
//!
//! Measured on this branch before the fix, **every** model id written in the nexus
//! documentation was absent from `ClaudeModel::all()` — not some of them, all of them:
//!
//! | Written in the docs | In `ClaudeModel::all()` |
//! |---------------------|-------------------------|
//! | `claude-opus-4-5-20251101` ("Latest Opus 4.5") | no |
//! | `claude-sonnet-4-5-20250929` | no |
//! | `claude-opus-4-20250514` | no |
//! | `claude-3-5-sonnet-20241022` | no |
//! | `claude-3-5-haiku-20241022` | no |
//! | `claude-opus-4-1-20250805` ("currently 4.1") | no |
//!
//! The catalogue had moved to the Claude 5 series and to a runtime
//! `ModelRegistry`, and no document had followed. A reader copying any id out of a README
//! got a 404.
//!
//! The documents now send readers to `GET /v1/models` instead of listing ids. This test
//! keeps it that way: an id written anywhere in the docs must exist in
//! `ClaudeModel::all()`, be an alias, or be listed below as a deliberate
//! counter-example.
//!
//! Nothing here touches the network — `ClaudeModel::all()` is the static fallback
//! catalogue, which is exactly the right reference for a prose claim.
//!
//! ## Why the catalogue is read from the source text
//!
//! `claude-code-api` declares only binaries, no `[lib]`, so an integration test cannot
//! `use` anything from it. The ids are therefore lifted out of
//! `claude-code-api/src/models/claude.rs` by [`catalogue_ids`], which is the same
//! technique the other offline gates in this directory use. `the_catalogue_parser_finds_the_known_series`
//! guards the parse, so a refactor that moves the ids elsewhere fails loudly instead of
//! yielding an empty set that would make everything else pass.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Source file holding the static fallback catalogue (`ClaudeModel::all`).
const CATALOGUE_SOURCE: &str = "claude-code-api/src/models/claude.rs";

/// Model ids declared by `ClaudeModel::all()`, read from the source text.
///
/// Matches the `id: "…".to_string()` field of each `ClaudeModel` literal. Returns them
/// de-duplicated and sorted.
fn catalogue_ids() -> BTreeSet<String> {
    let source = fs::read_to_string(repo_root().join(CATALOGUE_SOURCE))
        .unwrap_or_else(|e| panic!("cannot read {}: {}", CATALOGUE_SOURCE, e));
    let mut out = BTreeSet::new();

    for line in source.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("id: \"") else {
            continue;
        };
        let Some(end) = rest.find('"') else { continue };
        out.insert(rest[..end].to_string());
    }

    out
}

/// Repository root (the workspace directory above this crate).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir has a parent")
        .to_path_buf()
}

/// Markdown files whose model claims must match the code.
///
/// `CHANGELOG.md` is excluded on purpose: it records what past versions supported, so a
/// retired id there is history, not a wrong claim. Translations are excluded because they
/// carry a dated staleness banner instead — `every_translation_carries_a_dated_staleness_notice`
/// is what holds them to account.
fn checked_documents() -> Vec<PathBuf> {
    let root = repo_root();
    let mut out = Vec::new();
    collect(&root, &root, &mut out);
    out.sort();
    out
}

fn collect(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) {
    const SKIP_DIRS: [&str; 5] = ["target", "node_modules", ".git", ".github", "dist"];
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                collect(&path, base, out);
            }
        } else if name.ends_with(".md")
            && name != "CHANGELOG.md"
            && !name.starts_with("RELEASE_")
            && !name.contains("README_CN")
            && !name.contains("README_JA")
        {
            if let Ok(relative) = path.strip_prefix(base) {
                out.push(relative.to_path_buf());
            }
        }
    }
}

/// Model-id-shaped strings a document is allowed to contain even though the catalogue
/// does not list them, because the surrounding prose presents them as rejected.
///
/// Each entry must stay accompanied by that prose; dropping the explanation and keeping
/// the id here would re-create the defect this test exists to catch.
const DOCUMENTED_COUNTER_EXAMPLES: [&str; 1] = [
    // docs/valid-models.md, "Invalid Model Names": a retired id shown as an example of
    // the dated suffix form that no longer resolves.
    "claude-3-opus-20240229",
];

/// Every `claude-<family>-<digits>…` occurrence in a text, de-duplicated.
///
/// Deliberately does not match project names such as `claude-code-api` or
/// `claude-agent-sdk`: a model id has a digit in the segment after the family.
fn model_ids_in(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let bytes = text.as_bytes();
    let needle = b"claude-";
    let mut i = 0usize;

    while i + needle.len() < bytes.len() {
        if &bytes[i..i + needle.len()] != needle {
            i += 1;
            continue;
        }
        let mut end = i + needle.len();
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'-' || bytes[end] == b'.')
        {
            end += 1;
        }
        let candidate = text[i..end].trim_end_matches(['.', '-']).to_string();
        if looks_like_a_model_id(&candidate) {
            out.insert(candidate);
        }
        i = end.max(i + 1);
    }

    out
}

/// Whether a `claude-…` token is shaped like a model id rather than a crate or product name.
fn looks_like_a_model_id(candidate: &str) -> bool {
    let mut segments = candidate.split('-').skip(1); // drop the "claude" prefix
    let Some(family) = segments.next() else {
        return false;
    };
    if !["opus", "sonnet", "haiku", "fable", "3", "4", "5"].contains(&family) {
        return false;
    }
    // A bare family name (`claude-opus`) is not an id; an id carries a version segment.
    segments.next().is_some()
}

#[test]
fn documentation_names_no_model_the_code_does_not_know() {
    let known = catalogue_ids();
    assert!(
        !known.is_empty(),
        "no model id found in {} — `ClaudeModel::all()` moved and the parser needs updating",
        CATALOGUE_SOURCE
    );

    let root = repo_root();
    let mut offenders: Vec<String> = Vec::new();

    for document in checked_documents() {
        let path = root.join(&document);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for id in model_ids_in(&text) {
            if known.contains(&id) || DOCUMENTED_COUNTER_EXAMPLES.contains(&id.as_str()) {
                continue;
            }
            offenders.push(format!("{}: {}", document.display(), id));
        }
    }
    offenders.sort();

    assert!(
        offenders.is_empty(),
        "these documents name models absent from `ClaudeModel::all()`:\n  {}\n\n\
         Known ids: {:?}\n\n\
         Prefer an alias (`opus`, `sonnet`, `haiku`) or point the reader at \
         `GET /v1/models`; pin an id only when the catalogue really contains it.",
        offenders.join("\n  "),
        known
    );
}

#[test]
fn the_model_id_detector_distinguishes_ids_from_project_names() {
    // Guard the heuristic itself: a detector that matched nothing would make the test
    // above pass for ever.
    let found = model_ids_in(
        "see claude-code-api and claude-agent-sdk, which accept claude-opus-5-5 \
         and claude-3-5-haiku-20241022 but not claude-opus alone",
    );
    assert!(found.contains("claude-opus-5-5"), "{:?}", found);
    assert!(found.contains("claude-3-5-haiku-20241022"), "{:?}", found);
    assert!(!found.contains("claude-code-api"), "{:?}", found);
    assert!(!found.contains("claude-agent-sdk"), "{:?}", found);
    assert!(!found.contains("claude-opus"), "{:?}", found);
}

#[test]
fn the_documentation_sweep_actually_reads_files() {
    let documents = checked_documents();
    assert!(
        documents.len() >= 5,
        "expected the markdown sweep to find the documentation, got {:?}",
        documents
    );
    assert!(
        documents.iter().any(|d| d == Path::new("README.md")),
        "README.md must be part of the sweep: {:?}",
        documents
    );
    assert!(
        documents
            .iter()
            .any(|d| d == Path::new("docs/valid-models.md")),
        "docs/valid-models.md must be part of the sweep"
    );
    assert!(
        !documents.iter().any(|d| d == Path::new("CHANGELOG.md")),
        "CHANGELOG.md records history and must stay out of the sweep"
    );
}

/// Translations that are knowingly behind the English original.
///
/// Each must carry a notice with the date it was last compared, so a reader can tell
/// "checked yesterday, minor gaps" from "untouched for a year".
const STALE_TRANSLATIONS: [&str; 4] = [
    "README_CN.md",
    "README_JA.md",
    "claude-code-sdk-rs/README_CN.md",
    "claude-code-sdk-rs/README_JA.md",
];

#[test]
fn every_translation_carries_a_dated_staleness_notice() {
    let root = repo_root();

    for relative in STALE_TRANSLATIONS {
        let path = root.join(relative);
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {}", path.display(), e));

        assert!(
            text.contains("[!WARNING]"),
            "{} has no staleness notice. A translation without one reads as current; \
             either bring it level with README.md and remove it from STALE_TRANSLATIONS, \
             or restore the notice.",
            relative
        );
        assert!(
            text.contains("2026-10-01"),
            "{}'s notice carries no comparison date. State the day the translation was \
             last compared with README.md, so its age is visible.",
            relative
        );
        assert!(
            text.contains("README.md"),
            "{}'s notice must name README.md as the authority",
            relative
        );
        assert!(
            text.contains("model_registry.rs"),
            "{}'s notice must warn that its model list is not the catalogue — that is \
             the claim most likely to mislead a reader",
            relative
        );

        // The notice belongs above the content, not buried at the bottom.
        let notice_at = text.find("[!WARNING]").expect("checked above");
        assert!(
            notice_at < 400,
            "{}'s notice sits {} bytes in; put it directly under the title",
            relative,
            notice_at
        );
    }
}

#[test]
fn the_english_readme_carries_no_staleness_notice() {
    // The authority must not be marked stale: if it ever is, the notices above point at a
    // document that itself disclaims accuracy.
    let text = fs::read_to_string(repo_root().join("README.md")).expect("README.md");
    assert!(
        !text.contains("[!WARNING]"),
        "README.md is the authority the translations defer to; it must not disclaim itself"
    );
}

#[test]
fn the_catalogue_parser_finds_the_known_series() {
    // An empty or truncated parse would make `documentation_names_no_model_the_code_does_not_know`
    // vacuous, so assert the shape of what was found rather than only that it is non-empty.
    let ids = catalogue_ids();
    assert!(
        ids.len() >= 8,
        "expected the fallback catalogue to hold several models, found {:?}",
        ids
    );
    assert!(
        ids.iter().all(|id| id.starts_with("claude-")),
        "every catalogue id should be a claude-* id: {:?}",
        ids
    );
    assert!(
        ids.iter().any(|id| id.contains("opus")),
        "expected at least one Opus entry: {:?}",
        ids
    );
    assert!(
        ids.iter().any(|id| id.contains("sonnet")),
        "expected at least one Sonnet entry: {:?}",
        ids
    );
}
