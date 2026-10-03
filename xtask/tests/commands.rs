//! `xtask::run`, end to end, against repositories built for the test — and
//! against this repository, so that `cargo test` itself refuses a stale index
//! or a malformed diagram.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(["-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write(repo: &Path, path: &str, text: &str) {
    let full = repo.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, text).unwrap();
}

fn commit(repo: &Path, message: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", message]);
}

const DEMO: &str =
    "%% name: demo\n%% covers: lib/src/*.rs\n%% verified: 2026-10-01\nflowchart TD\n";

/// A repository with one diagram owning `lib/src/*.rs`, a current index, and a
/// `work` branch checked out on top of `main`.
fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    write(repo, "docs/diagrams/demo.mmd", DEMO);
    write(repo, "docs/diagrams/_TEMPLATE.mmd", "%% name: <name>\n");
    write(
        repo,
        "docs/diagrams/notes/draft.mmd",
        "not a diagram of this directory\n",
    );
    write(repo, "lib/src/a.rs", "fn a() {}\n");
    write(repo, "bin/src/free.rs", "fn main() {}\n");
    assert_eq!(run(repo, &["diagrams", "index", "--write"]).0, 0);
    commit(repo, "base");
    git(repo, &["checkout", "-q", "-b", "work"]);
    dir
}

fn run(cwd: &Path, args: &[&str]) -> (u8, String) {
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let mut out = String::new();
    let code = xtask::run(&args, cwd, &mut out);
    (code, out)
}

fn drift(repo: &Path) -> (u8, String) {
    run(repo, &["diagrams", "drift", "--base", "main"])
}

#[test]
fn covered_file_changed_with_nothing_said_fails() {
    let dir = repository();
    write(dir.path(), "lib/src/a.rs", "fn a() { changed(); }\n");
    commit(dir.path(), "fix: change a");
    let (code, out) = drift(dir.path());
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("ERROR `demo` owns lib/src/a.rs"), "{out}");
    assert!(out.contains("1 problem(s)."), "{out}");
}

#[test]
fn same_change_with_the_diagram_touched_passes() {
    let dir = repository();
    write(dir.path(), "lib/src/a.rs", "fn a() { changed(); }\n");
    write(
        dir.path(),
        "docs/diagrams/demo.mmd",
        &DEMO.replace("10-01", "10-03"),
    );
    commit(dir.path(), "fix: change a, diagram updated");
    let (code, out) = drift(dir.path());
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("no drift"), "{out}");
}

#[test]
fn same_change_with_the_trailer_passes() {
    let dir = repository();
    write(dir.path(), "lib/src/a.rs", "fn a() { changed(); }\n");
    commit(
        dir.path(),
        "fix: change a\n\nDiagram-Unchanged: demo — body only, same flow",
    );
    let (code, out) = drift(dir.path());
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("stated unchanged: demo — body only, same flow"),
        "{out}"
    );
}

#[test]
fn a_deleted_covered_file_fails() {
    let dir = repository();
    write(dir.path(), "lib/src/b.rs", "fn b() {}\n");
    commit(
        dir.path(),
        "feat: b\n\nDiagram-Unchanged: demo — b is internal",
    );
    git(dir.path(), &["checkout", "-q", "-b", "next"]);
    std::fs::remove_file(dir.path().join("lib/src/b.rs")).unwrap();
    commit(dir.path(), "refactor: drop b");
    let (code, out) = run(dir.path(), &["diagrams", "drift", "--base", "work"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("`demo` owns lib/src/b.rs"), "{out}");
}

#[test]
fn a_trailer_without_reason_fails() {
    let dir = repository();
    write(dir.path(), "lib/src/a.rs", "fn a() { changed(); }\n");
    commit(dir.path(), "fix: change a\n\nDiagram-Unchanged: demo");
    let (code, out) = drift(dir.path());
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("reason is mandatory"), "{out}");
}

#[test]
fn an_unowned_change_is_listed_and_passes() {
    let dir = repository();
    write(dir.path(), "bin/src/free.rs", "fn main() { x(); }\n");
    commit(dir.path(), "chore: free");
    let (code, out) = drift(dir.path());
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("owned by no diagram (not an error):\n    bin/src/free.rs"),
        "{out}"
    );
}

#[test]
fn a_malformed_header_fails_the_drift_check_too() {
    let dir = repository();
    write(
        dir.path(),
        "docs/diagrams/bad.mmd",
        "%% name: bad\nflowchart TD\n",
    );
    commit(dir.path(), "docs: bad diagram");
    let (code, out) = drift(dir.path());
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("docs/diagrams/bad.mmd: header `covers` is missing"),
        "{out}"
    );
}

#[test]
fn drift_defaults_to_origin_main_and_reports_a_missing_ref() {
    let dir = repository();
    let (code, out) = run(dir.path(), &["diagrams", "drift"]);
    assert_eq!(code, 2, "{out}");
    assert!(
        out.starts_with("error: git diff --name-only -z origin/main...HEAD failed"),
        "{out}"
    );
}

#[test]
fn index_prints_checks_and_writes() {
    let dir = repository();
    let repo = dir.path();
    let (code, out) = run(repo, &["diagrams", "index"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.starts_with("1 diagrams | 2 source files | 1 claimed | 1 unclaimed\n"),
        "{out}"
    );
    assert!(
        !repo
            .join("docs/diagrams/INDEX.yml")
            .metadata()
            .unwrap()
            .permissions()
            .readonly()
    );

    let (code, out) = run(repo, &["diagrams", "index", "--check"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("docs/diagrams/INDEX.yml is current"), "{out}");

    // A new, not yet committed source changes what the index must say.
    write(repo, "lib/src/new.rs", "fn n() {}\n");
    let (code, out) = run(repo, &["diagrams", "index", "--check"]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("is stale — run `cargo xtask diagrams index --write`"),
        "{out}"
    );

    let (code, out) = run(repo, &["diagrams", "index", "--write"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("docs/diagrams/INDEX.yml written"), "{out}");
    assert_eq!(run(repo, &["diagrams", "index", "--check"]).0, 0);
}

#[test]
fn index_tolerates_crlf_in_the_committed_file() {
    let dir = repository();
    let path = dir.path().join("docs/diagrams/INDEX.yml");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace('\n', "\r\n")).unwrap();
    assert_eq!(run(dir.path(), &["diagrams", "index", "--check"]).0, 0);
}

#[test]
fn index_refuses_to_write_over_a_problem() {
    let dir = repository();
    let repo = dir.path();
    write(
        repo,
        "docs/diagrams/demo.mmd",
        &DEMO.replace("lib/src/*.rs", "moved/*.rs"),
    );
    let before = std::fs::read_to_string(repo.join("docs/diagrams/INDEX.yml")).unwrap();
    let (code, out) = run(repo, &["diagrams", "index", "--write"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("matches no file"), "{out}");
    assert!(
        out.contains("1 problem(s). INDEX.yml not written."),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("docs/diagrams/INDEX.yml")).unwrap(),
        before
    );
}

#[test]
fn a_file_deleted_but_still_in_the_index_of_git_is_not_a_source() {
    let dir = repository();
    std::fs::remove_file(dir.path().join("bin/src/free.rs")).unwrap();
    let (code, out) = run(dir.path(), &["diagrams", "index"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.starts_with("1 diagrams | 1 source files | 1 claimed | 0 unclaimed\n"),
        "{out}"
    );
}

#[test]
fn no_diagram_at_all_is_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    write(dir.path(), "lib/src/a.rs", "fn a() {}\n");
    let (code, out) = run(dir.path(), &["diagrams", "index"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("no diagram found in docs/diagrams/"), "{out}");
}

#[test]
fn outside_a_repository_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let (code, out) = run(dir.path(), &["diagrams", "index"]);
    assert_eq!(code, 2, "{out}");
    assert!(
        out.starts_with("error: git rev-parse --show-toplevel failed"),
        "{out}"
    );
}

#[test]
fn an_unknown_command_prints_the_usage() {
    for args in [
        &[][..],
        &["diagrams"],
        &["diagrams", "index", "--force"],
        &["diagrams", "drift", "--base"],
    ] {
        let (code, out) = run(Path::new("."), args);
        assert_eq!(code, 2, "{args:?}");
        assert!(out.starts_with("usage:"), "{args:?}");
    }
}

/// The repository this crate lives in.
fn this_repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_owned()
}

#[test]
fn the_committed_diagrams_are_well_formed_and_the_index_is_current() {
    let (code, out) = run(&this_repository(), &["diagrams", "index", "--check"]);
    assert_eq!(code, 0, "{out}");
}
