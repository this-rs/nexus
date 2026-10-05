//! `Glob` and `Grep` against a real ripgrep (N19).
//!
//! Each case runs the same search with `Grep` and with `rg`, on a tree built here, and the
//! two outputs must be equal. Where `rg` is not installed the output recorded from it
//! (`tests/golden_grep/`) is compared instead; when it is installed the goldens are also
//! checked against it, so they cannot drift. Regenerate with
//! `NEXUS_UPDATE_GOLDENS=1 cargo test -p nexus-tools --test search`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use nexus_tools::files::{FileConfig, Scope, register};
use nexus_tools::{CallContext, Profile, SessionState, ToolRegistry, ToolResult};
use serde_json::{Value, json};

struct Tree {
    dir: tempfile::TempDir,
    registry: ToolRegistry,
    context: CallContext,
}

fn put(root: &Path, name: &str, content: &[u8]) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn tree() -> Tree {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    std::fs::create_dir_all(r.join(".git")).unwrap();
    put(r, ".gitignore", b"build/\n*.log\n");
    put(
        r,
        "src/main.rs",
        b"fn main() {\n    println!(\"hello\");\n}\n// TODO: refactor main\n",
    );
    put(r, "src/lib.rs", b"pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n// TODO: document add\n// Todo: lowercase variant\n");
    put(
        r,
        "src/util/helpers.rs",
        b"pub fn helper() {}\npub fn other() {}\n",
    );
    put(r, "README.md", b"# Project\n\nA TODO list lives here.\n");
    put(
        r,
        "docs/guide.md",
        b"## Guide\nfirst\nsecond\nthird\nfourth\nfifth\nsixth\nseventh TODO\neighth\nninth\n",
    );
    put(r, "script.py", b"def run():\n    return 1  # TODO later\n");
    put(r, ".hidden/secret.txt", b"a hidden TODO\n");
    put(r, "build/out.txt", b"TODO in an ignored directory\n");
    put(r, "debug.log", b"TODO in an ignored file\n");
    put(r, "data.bin", b"TODO\0binary\n");
    put(r, "notes.txt", b"alpha\nBeta\nGAMMA\nbeta again\n");
    put(
        r,
        "multi.txt",
        b"start\nmiddle one\nmiddle two\nend\nstart\nnot the end\n",
    );
    let scope = Scope::new(r, []).unwrap();
    let registry = register(ToolRegistry::new(), &FileConfig::new(scope));
    Tree {
        dir,
        registry,
        context: CallContext::new("s", Arc::new(SessionState::default())),
    }
}

impl Tree {
    async fn run(&self, tool: &str, arguments: Value) -> ToolResult {
        self.registry
            .resolve(&Profile::unrestricted("s"), tool)
            .unwrap()
            .call(&self.context, arguments)
            .await
    }
}

fn rg_available() -> bool {
    Command::new("rg")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// What the real ripgrep prints for `args` in the tree, paths without the `./`.
fn rg(root: &Path, args: &[&str]) -> String {
    let output = Command::new("rg")
        .current_dir(root)
        .args([
            "--no-config",
            "--no-heading",
            "--sort",
            "path",
            "--hidden",
            "-g",
            "!.git/",
        ])
        .args(args)
        .output()
        .expect("rg runs");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| l.strip_prefix("./").unwrap_or(l).to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

fn golden_path(case: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/golden_grep/{case}.txt"))
}

/// The expected output of `case`: from `rg` when installed (and the golden is refreshed or
/// verified), else from the golden.
fn expected(case: &str, root: &Path, rg_args: &[&str]) -> String {
    let golden = golden_path(case);
    if rg_available() {
        let live = rg(root, rg_args);
        if std::env::var_os("NEXUS_UPDATE_GOLDENS").is_some() {
            std::fs::write(&golden, &live).unwrap();
        }
        let recorded = std::fs::read_to_string(&golden)
            .unwrap_or_else(|_| panic!("no golden for {case}: run with NEXUS_UPDATE_GOLDENS=1"));
        assert_eq!(
            recorded, live,
            "the golden of {case} drifted from the real rg"
        );
        live
    } else {
        std::fs::read_to_string(&golden).unwrap_or_else(|_| panic!("no golden for {case}"))
    }
}

/// Our content/count output with the pagination note and the count footer removed.
fn body(result: &ToolResult) -> String {
    assert!(!result.is_error, "{}", result.text);
    result.text.split("\n\n").next().unwrap().to_owned()
}

fn sorted_lines(text: &str) -> String {
    let mut lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    lines.sort_unstable();
    lines.join("\n")
}

async fn same_as_rg(case: &str, ours: Value, rg_args: &[&str]) {
    let t = tree();
    let want = expected(case, t.dir.path(), rg_args);
    let got = body(&t.run("Grep", ours).await);
    assert_eq!(got, want, "case {case}");
}

#[tokio::test]
async fn content_with_line_numbers_matches_rg() {
    same_as_rg(
        "content_n",
        json!({"pattern": "TODO", "output_mode": "content"}),
        &["-n", "TODO", "."],
    )
    .await;
}

#[tokio::test]
async fn content_without_line_numbers_matches_rg() {
    same_as_rg(
        "content_no_n",
        json!({"pattern": "TODO", "output_mode": "content", "-n": false}),
        &["TODO", "."],
    )
    .await;
}

#[tokio::test]
async fn case_insensitive_matches_rg() {
    same_as_rg(
        "case_insensitive",
        json!({"pattern": "beta", "output_mode": "content", "-i": true}),
        &["-n", "-i", "beta", "."],
    )
    .await;
}

#[tokio::test]
async fn context_lines_and_group_separators_match_rg() {
    same_as_rg(
        "context_c1",
        json!({"pattern": "TODO", "output_mode": "content", "-C": 1}),
        &["-n", "-C", "1", "TODO", "."],
    )
    .await;
    same_as_rg(
        "context_a2_b1",
        json!({"pattern": "TODO", "output_mode": "content", "-A": 2, "-B": 1}),
        &["-n", "-A", "2", "-B", "1", "TODO", "."],
    )
    .await;
    same_as_rg(
        "context_alias",
        json!({"pattern": "fifth", "output_mode": "content", "context": 2, "path": "docs"}),
        &["-n", "-C", "2", "fifth", "docs"],
    )
    .await;
}

#[tokio::test]
async fn count_matches_rg() {
    let t = tree();
    let want = expected("count", t.dir.path(), &["-c", "TODO", "."]);
    let r = t
        .run("Grep", json!({"pattern": "TODO", "output_mode": "count"}))
        .await;
    assert_eq!(body(&r), want);
    assert!(
        r.text
            .ends_with("Found 6 total occurrences across 6 files."),
        "{}",
        r.text
    );
}

#[tokio::test]
async fn files_with_matches_lists_the_same_files_as_rg() {
    let t = tree();
    let want = expected("files", t.dir.path(), &["-l", "TODO", "."]);
    let r = t.run("Grep", json!({"pattern": "TODO"})).await;
    assert!(!r.is_error);
    let (header, list) = r.text.split_once('\n').unwrap();
    assert_eq!(header, "Found 6 files");
    assert_eq!(sorted_lines(list), sorted_lines(&want));
}

#[tokio::test]
async fn glob_and_type_filters_match_rg() {
    same_as_rg(
        "glob_rs",
        json!({"pattern": "TODO", "output_mode": "content", "glob": "*.rs"}),
        &["-n", "-g", "*.rs", "TODO", "."],
    )
    .await;
    same_as_rg(
        "glob_negated",
        json!({"pattern": "TODO", "output_mode": "content", "glob": "!*.md"}),
        &["-n", "-g", "!*.md", "TODO", "."],
    )
    .await;
    same_as_rg(
        "type_rust",
        json!({"pattern": "TODO", "output_mode": "content", "type": "rust"}),
        &["-n", "-t", "rust", "TODO", "."],
    )
    .await;
    same_as_rg(
        "type_py",
        json!({"pattern": "TODO", "output_mode": "content", "type": "py"}),
        &["-n", "-t", "py", "TODO", "."],
    )
    .await;
}

#[tokio::test]
async fn anchors_work_per_line_like_rg() {
    same_as_rg(
        "anchors",
        json!({"pattern": "^pub fn", "output_mode": "content"}),
        &["-n", "^pub fn", "."],
    )
    .await;
}

#[tokio::test]
async fn multiline_matches_rg() {
    same_as_rg(
        "multiline",
        json!({"pattern": "start.*?end", "output_mode": "content", "multiline": true}),
        &["-n", "-U", "--multiline-dotall", "start.*?end", "."],
    )
    .await;
}

#[tokio::test]
async fn a_single_file_prints_no_file_name_like_rg() {
    same_as_rg(
        "single_file",
        json!({"pattern": "TODO", "output_mode": "content", "path": "docs/guide.md"}),
        &["-n", "TODO", "docs/guide.md"],
    )
    .await;
}

#[tokio::test]
async fn ignored_hidden_and_binary_files_behave_like_rg() {
    let t = tree();
    let r = t.run("Grep", json!({"pattern": "TODO"})).await;
    // Hidden files are searched; .gitignore'd ones and binary ones are not.
    assert!(r.text.contains(".hidden/secret.txt"), "{}", r.text);
    assert!(!r.text.contains("build/out.txt"), "{}", r.text);
    assert!(!r.text.contains("debug.log"), "{}", r.text);
    assert!(!r.text.contains("data.bin"), "{}", r.text);
}

#[tokio::test]
async fn pagination_slices_the_entries_and_says_how_to_continue() {
    let t = tree();
    let all = body(
        &t.run(
            "Grep",
            json!({"pattern": "TODO", "output_mode": "content", "-n": false}),
        )
        .await,
    );
    let all: Vec<&str> = all.lines().collect();
    assert_eq!(all.len(), 6);
    let page = t
        .run("Grep", json!({"pattern": "TODO", "output_mode": "content", "-n": false, "head_limit": 3, "offset": 2}))
        .await;
    let lines: Vec<&str> = page.text.lines().collect();
    assert_eq!(&lines[..3], &all[2..5]);
    assert!(
        page.text
            .contains("showing 3–5 of 6; call again with offset 5 to continue"),
        "{}",
        page.text
    );
    let unlimited = t
        .run(
            "Grep",
            json!({"pattern": "TODO", "output_mode": "content", "-n": false, "head_limit": 0}),
        )
        .await;
    assert!(!unlimited.text.contains("output limited"));
}

#[tokio::test]
async fn nothing_found_is_a_plain_answer_and_a_bad_request_an_error() {
    let t = tree();
    assert_eq!(
        t.run("Grep", json!({"pattern": "zzz-never"})).await.text,
        "No files found"
    );
    assert_eq!(
        t.run(
            "Grep",
            json!({"pattern": "zzz-never", "output_mode": "content"})
        )
        .await
        .text,
        "No matches found"
    );
    let bad = t.run("Grep", json!({"pattern": "("})).await;
    assert!(
        bad.is_error && bad.text.contains("Invalid regular expression"),
        "{}",
        bad.text
    );
    let kind = t
        .run("Grep", json!({"pattern": "x", "type": "nonesuch"}))
        .await;
    assert!(
        kind.is_error && kind.text.contains("Unknown file type"),
        "{}",
        kind.text
    );
    let missing = t
        .run("Grep", json!({"pattern": "x", "path": "nowhere"}))
        .await;
    assert!(
        missing.is_error && missing.text.contains("does not exist"),
        "{}",
        missing.text
    );
}

#[tokio::test]
async fn grep_and_glob_cannot_leave_the_scope() {
    let t = tree();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("o.txt"), "TODO outside\n").unwrap();
    for tool in ["Grep", "Glob"] {
        let r = t
            .run(tool, json!({"pattern": if tool == "Grep" { "TODO" } else { "*" }, "path": outside.path().display().to_string()}))
            .await;
        assert!(
            r.is_error && r.text.contains("outside the directories"),
            "{tool}: {}",
            r.text
        );
    }
    #[cfg(unix)]
    {
        // A symlink inside the tree to the outside is not followed.
        std::os::unix::fs::symlink(outside.path(), t.dir.path().join("escape")).unwrap();
        let r = t.run("Grep", json!({"pattern": "TODO outside"})).await;
        assert_eq!(r.text, "No files found", "{}", r.text);
        let r = t.run("Glob", json!({"pattern": "escape/*"})).await;
        assert_eq!(r.text, "No files found", "{}", r.text);
    }
}

// ---------------------------------------------------------------------------
// Glob
// ---------------------------------------------------------------------------

#[tokio::test]
async fn glob_finds_files_by_pattern_and_honours_gitignore() {
    let t = tree();
    let r = t.run("Glob", json!({"pattern": "**/*.rs"})).await;
    assert_eq!(
        sorted_lines(&r.text),
        "src/lib.rs\nsrc/main.rs\nsrc/util/helpers.rs"
    );
    // `*` does not cross a directory; `**` does.
    let r = t.run("Glob", json!({"pattern": "*.md"})).await;
    assert_eq!(r.text, "README.md");
    let r = t.run("Glob", json!({"pattern": "**/*.md"})).await;
    assert_eq!(sorted_lines(&r.text), "README.md\ndocs/guide.md");
    let r = t.run("Glob", json!({"pattern": "src/**/*.{rs,py}"})).await;
    assert_eq!(
        sorted_lines(&r.text),
        "src/lib.rs\nsrc/main.rs\nsrc/util/helpers.rs"
    );
    // Ignored files do not appear; hidden ones do.
    let r = t.run("Glob", json!({"pattern": "**/*.txt"})).await;
    assert!(!r.text.contains("build/out.txt"), "{}", r.text);
    assert!(r.text.contains(".hidden/secret.txt"), "{}", r.text);
    let r = t.run("Glob", json!({"pattern": "**/*.nothing"})).await;
    assert_eq!(r.text, "No files found");
    let r = t
        .run("Glob", json!({"pattern": "*.rs", "path": "src"}))
        .await;
    assert_eq!(sorted_lines(&r.text), "src/lib.rs\nsrc/main.rs");
}

#[tokio::test]
async fn glob_sorts_newest_first_and_caps_at_a_hundred() {
    let t = tree();
    let old = t.dir.path().join("docs/guide.md");
    let new = t.dir.path().join("README.md");
    let older = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(older)
        .unwrap();
    let r = t.run("Glob", json!({"pattern": "**/*.md"})).await;
    assert_eq!(r.text, "README.md\ndocs/guide.md");
    let _ = new;

    for n in 0..120 {
        put(t.dir.path(), &format!("many/f{n:03}.dat"), b"x");
    }
    let r = t.run("Glob", json!({"pattern": "many/*.dat"})).await;
    let lines: Vec<&str> = r.text.lines().collect();
    assert_eq!(lines.len(), 101, "100 paths and the notice");
    assert_eq!(
        lines[100],
        "(Results are truncated. Consider using a more specific path or pattern.)"
    );
}

#[tokio::test]
async fn glob_rejects_a_bad_pattern() {
    let t = tree();
    let r = t.run("Glob", json!({"pattern": "[unclosed"})).await;
    assert!(
        r.is_error && r.text.contains("Invalid glob pattern"),
        "{}",
        r.text
    );
}
