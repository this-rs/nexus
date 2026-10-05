//! Guard of decision A33 (contract §11): a provider adapter never builds a
//! process by itself. The single launcher is
//! `transport::spawn::isolated_command`, which starts from an empty environment;
//! a `Command::new` under `src/providers/` would inherit the host's secrets.
//!
//! The check is textual on purpose: it also catches a spawn hidden behind a
//! `use` alias of the type's path, and it needs no build.

use std::path::{Path, PathBuf};

/// What must not appear under `src/providers/`.
const FORBIDDEN: &str = "Command::new";

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()));
    for entry in entries {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            rust_files(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

fn offences(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    rust_files(root, &mut files);
    files.sort();
    let mut offences = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", file.display()));
        for (index, line) in text.lines().enumerate() {
            if line.contains(FORBIDDEN) {
                offences.push(format!("{}:{}: {}", file.display(), index + 1, line.trim()));
            }
        }
    }
    offences
}

#[test]
fn no_provider_spawns_a_process_by_itself() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/providers");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    assert!(
        files.len() >= 2,
        "the guard found almost nothing to check under {}: {files:?}",
        root.display()
    );
    let offences = offences(&root);
    assert!(
        offences.is_empty(),
        "`{FORBIDDEN}` is forbidden under src/providers/ (use transport::spawn::isolated_command):\n{}",
        offences.join("\n")
    );
}

/// The guard itself is checked: it does find the pattern where there is one.
#[test]
fn the_guard_finds_a_spawn_when_there_is_one() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let nested = dir.path().join("some_provider");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("clean.rs"), "fn main() {}\n").unwrap();
    let spawn = [
        "let child = tokio::process::Command",
        "::new(\"claude\");\n",
    ]
    .concat();
    std::fs::write(nested.join("spawns.rs"), spawn).unwrap();
    let offences = offences(dir.path());
    assert_eq!(offences.len(), 1, "{offences:?}");
    assert!(offences[0].contains("spawns.rs:1"));
}
