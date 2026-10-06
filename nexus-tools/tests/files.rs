//! `Read`, `Write` and `Edit` (N19): the recorded behaviours of Claude Code 2.1.287, the
//! session's read state, the scope, and atomic writes.
//!
//! Expected texts are the ones in `claude-code-sdk-rs/tests/parity/claude-code-2.1.287/`;
//! the full replay of those recordings is N25.
#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;

use nexus_tools::files::{FileConfig, Scope, register};
use nexus_tools::{CallContext, SessionState, ToolRegistry, ToolResult};
use serde_json::{Value, json};

struct Fixture {
    dir: tempfile::TempDir,
    outside: tempfile::TempDir,
    registry: ToolRegistry,
    context: CallContext,
}

impl Fixture {
    fn new() -> Self {
        Self::with(|config| config)
    }

    fn with(adjust: impl FnOnce(FileConfig) -> FileConfig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let scope = Scope::new(dir.path(), []).unwrap();
        let registry = register(ToolRegistry::new(), &adjust(FileConfig::new(scope)));
        Self {
            dir,
            outside,
            registry,
            context: CallContext::new("s", Arc::new(SessionState::default())),
        }
    }

    /// A second session over the same files: it has read nothing.
    fn another_session(&self) -> CallContext {
        CallContext::new("other", Arc::new(SessionState::default()))
    }

    fn path(&self, name: &str) -> String {
        self.dir.path().join(name).display().to_string()
    }

    fn put(&self, name: &str, content: &str) {
        let path = self.dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn get(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(name)).unwrap()
    }

    async fn call_in(&self, context: &CallContext, tool: &str, arguments: Value) -> ToolResult {
        let profile = nexus_tools::Profile::unrestricted("s");
        self.registry
            .resolve(&profile, tool)
            .unwrap()
            .call(context, arguments)
            .await
    }

    async fn call(&self, tool: &str, arguments: Value) -> ToolResult {
        self.call_in(&self.context, tool, arguments).await
    }

    async fn read(&self, name: &str) -> ToolResult {
        self.call("Read", json!({"file_path": self.path(name)}))
            .await
    }
}

fn rows(count: usize, prefix: &str) -> String {
    (1..=count).map(|n| format!("{prefix} {n}\n")).collect()
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_numbers_lines_like_cat_n_with_a_last_empty_line() {
    let f = Fixture::new();
    f.put("a.txt", &rows(12, "line"));
    let r = f.read("a.txt").await;
    assert!(!r.is_error);
    let expected: String = (1..=12)
        .map(|n| format!("{n}\tline {n}\n"))
        .collect::<String>()
        + "13\t";
    assert_eq!(r.text, expected);
}

#[tokio::test]
async fn read_offset_is_the_first_line_number_and_limit_cuts_without_a_blank_line() {
    let f = Fixture::new();
    f.put("a.txt", &rows(12, "line"));
    let at = |offset: Option<u64>, limit: Option<u64>| {
        let mut args = json!({"file_path": f.path("a.txt")});
        if let Some(o) = offset {
            args["offset"] = json!(o);
        }
        if let Some(l) = limit {
            args["limit"] = json!(l);
        }
        args
    };
    assert_eq!(
        f.call("Read", at(Some(3), Some(2))).await.text,
        "3\tline 3\n4\tline 4"
    );
    assert_eq!(
        f.call("Read", at(None, Some(3))).await.text,
        "1\tline 1\n2\tline 2\n3\tline 3"
    );
    // `offset: 0` is taken literally and shifts the numbering (recorded behaviour).
    assert_eq!(
        f.call("Read", at(Some(0), Some(3))).await.text,
        "0\tline 1\n1\tline 2\n2\tline 3"
    );
    assert_eq!(
        f.call("Read", at(Some(11), Some(100))).await.text,
        "11\tline 11\n12\tline 12\n13\t"
    );
}

#[tokio::test]
async fn read_past_the_end_is_a_warning_not_an_error() {
    let f = Fixture::new();
    f.put("a.txt", &rows(3000, "row"));
    let r = f
        .call(
            "Read",
            json!({"file_path": f.path("a.txt"), "offset": 5000}),
        )
        .await;
    assert!(!r.is_error);
    assert_eq!(
        r.text,
        "<system-reminder>Warning: the file exists but is shorter than the provided offset (5000). The file has 3001 lines.</system-reminder>"
    );
}

#[tokio::test]
async fn read_has_no_2000_line_cap_and_does_not_cut_long_lines() {
    let f = Fixture::new();
    f.put("a3000.txt", &rows(3000, "row"));
    let r = f.read("a3000.txt").await;
    assert!(
        r.text.ends_with("3000\trow 3000\n3001\t"),
        "{}",
        &r.text[r.text.len() - 40..]
    );
    assert_eq!(r.text.len(), 39_791, "the recorded length");

    f.put("line60k.txt", &format!("{}\n", "y".repeat(60_000)));
    let r = f.read("line60k.txt").await;
    // The line is whole. The recording also returned the empty line 2 after it; ours is
    // past the (character) cap by then, so it is dropped and the cut is announced.
    assert!(
        r.text.starts_with(&format!("1\t{}", "y".repeat(60_000))),
        "line cut"
    );
    assert!(
        r.text
            .contains("[output truncated: stopped at line 1 of 2; call Read again with offset 2"),
        "{}",
        &r.text[60_000..]
    );
}

#[tokio::test]
async fn read_stops_at_a_line_boundary_near_the_cap_and_says_so() {
    let f = Fixture::new();
    let big: String = (0..3300)
        .map(|n| format!("word{n:05} word{n:05} word{n:05} word{n:05} \n"))
        .collect();
    f.put("big.txt", &big);
    let r = f.read("big.txt").await;
    assert!(!r.is_error);
    let body = r.text.split("\n[output truncated").next().unwrap();
    assert!(
        r.text.contains("[output truncated: stopped at line "),
        "{}",
        &r.text[r.text.len() - 200..]
    );
    // The recording stopped at line 1250 (56 392 characters): ours is within a few lines.
    let last: usize = body
        .lines()
        .last()
        .unwrap()
        .split('\t')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1245..=1255).contains(&last), "stopped at {last}");
    assert!(body.len() <= 56_400);
    // The marker names the offset that continues exactly where it stopped.
    assert!(
        r.text.contains(&format!("offset {}", last + 1)),
        "{}",
        &r.text[r.text.len() - 150..]
    );
    let next = f
        .call(
            "Read",
            json!({"file_path": f.path("big.txt"), "offset": last + 1, "limit": 1}),
        )
        .await;
    assert!(
        next.text.starts_with(&format!("{}\tword", last + 1)),
        "{}",
        next.text
    );
}

#[tokio::test]
async fn read_errors_are_the_recorded_ones() {
    let f = Fixture::new();
    f.put("empty.txt", "");
    std::fs::create_dir_all(f.dir.path().join("sub")).unwrap();
    let missing = f.read("missing.txt").await;
    assert!(missing.is_error);
    let cwd = std::fs::canonicalize(f.dir.path()).unwrap();
    assert_eq!(
        missing.text,
        format!(
            "File does not exist. Note: your current working directory is {}.",
            cwd.display()
        )
    );
    let dir = f.read("sub").await;
    assert!(dir.is_error);
    assert_eq!(
        dir.text,
        format!(
            "EISDIR: illegal operation on a directory, read '{}'",
            f.path("sub")
        )
    );
    let empty = f.read("empty.txt").await;
    assert!(!empty.is_error);
    assert_eq!(
        empty.text,
        "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>"
    );
}

#[tokio::test]
async fn read_accepts_a_relative_path_from_the_working_directory() {
    let f = Fixture::new();
    f.put("a.txt", "hello\n");
    let r = f.call("Read", json!({"file_path": "a.txt"})).await;
    assert_eq!(r.text, "1\thello\n2\t");
}

#[tokio::test]
async fn read_refuses_a_file_that_is_not_text() {
    let f = Fixture::new();
    std::fs::write(f.dir.path().join("bin"), [0xff, 0xfe, 0x00, 0x80]).unwrap();
    let r = f.read("bin").await;
    assert!(r.is_error, "{r:?}");
    assert!(r.text.contains("not valid UTF-8"));
}

#[tokio::test]
async fn read_renders_a_notebook_as_cells() {
    let f = Fixture::new();
    f.put(
        "nb.ipynb",
        r##"{"cells":[{"id":"c1","cell_type":"code","metadata":{},"source":"print('a')","outputs":[],"execution_count":null},{"id":"c2","cell_type":"markdown","metadata":{},"source":["# ","title"]}],"metadata":{},"nbformat":4,"nbformat_minor":5}"##,
    );
    let r = f.read("nb.ipynb").await;
    assert_eq!(
        r.text,
        "<cell id=\"c1\">print('a')</cell id=\"c1\">\n<cell id=\"c2\"><cell_type>markdown</cell_type># title</cell id=\"c2\">"
    );
}

// ---------------------------------------------------------------------------
// Edit and Write: the read state
// ---------------------------------------------------------------------------

async fn edit(f: &Fixture, name: &str, old: &str, new: &str, all: bool) -> ToolResult {
    f.call(
        "Edit",
        json!({"file_path": f.path(name), "old_string": old, "new_string": new, "replace_all": all}),
    )
    .await
}

const NOT_READ: &str = "<tool_use_error>File has not been read yet. Read it first before writing to it.</tool_use_error>";

#[tokio::test]
async fn edit_and_write_refuse_a_file_the_session_has_not_read_and_change_nothing() {
    let f = Fixture::new();
    f.put("unread.txt", "untouched\n");
    let e = edit(&f, "unread.txt", "untouched", "changed", false).await;
    assert!(e.is_error);
    assert_eq!(e.text, NOT_READ);
    let w = f
        .call(
            "Write",
            json!({"file_path": f.path("unread.txt"), "content": "overwritten\n"}),
        )
        .await;
    assert!(w.is_error);
    assert_eq!(w.text, NOT_READ);
    assert_eq!(f.get("unread.txt"), "untouched\n");

    // What another session read does not count for this one.
    f.read("unread.txt").await;
    let other = f.another_session();
    let e = f
        .call_in(&other, "Edit", json!({"file_path": f.path("unread.txt"), "old_string": "untouched", "new_string": "x"}))
        .await;
    assert_eq!(e.text, NOT_READ);
    assert_eq!(f.get("unread.txt"), "untouched\n");
}

#[tokio::test]
async fn edit_replaces_one_match_and_says_so() {
    let f = Fixture::new();
    f.put("uniq.txt", "one\ntwo\nthree\n");
    f.read("uniq.txt").await;
    let r = edit(&f, "uniq.txt", "two", "TWO", false).await;
    assert!(!r.is_error, "{}", r.text);
    assert_eq!(
        r.text,
        format!(
            "The file {} has been updated successfully. (file state is current in your context — no need to Read it back)",
            f.path("uniq.txt")
        )
    );
    assert_eq!(f.get("uniq.txt"), "one\nTWO\nthree\n");
    // The edit refreshed the read state: a second edit needs no new Read.
    assert!(!edit(&f, "uniq.txt", "one", "ONE", false).await.is_error);
}

#[tokio::test]
async fn edit_refuses_a_non_unique_string_unless_replace_all_and_changes_nothing() {
    let f = Fixture::new();
    f.put("dup.txt", "alpha\nbeta\nalpha\ngamma\nalpha\n");
    f.read("dup.txt").await;
    let r = edit(&f, "dup.txt", "alpha", "ALPHA", false).await;
    assert!(r.is_error);
    assert_eq!(
        r.text,
        "<tool_use_error>Found 3 matches of the string to replace, but replace_all is false. To replace all occurrences, set replace_all to true. To replace only one occurrence, please provide more context to uniquely identify the instance.\nString: alpha</tool_use_error>"
    );
    assert_eq!(f.get("dup.txt"), "alpha\nbeta\nalpha\ngamma\nalpha\n");
    let all = edit(&f, "dup.txt", "alpha", "ALPHA", true).await;
    assert_eq!(
        all.text,
        format!(
            "The file {} has been updated. All occurrences were successfully replaced. (file state is current in your context — no need to Read it back)",
            f.path("dup.txt")
        )
    );
    assert_eq!(f.get("dup.txt"), "ALPHA\nbeta\nALPHA\ngamma\nALPHA\n");
}

#[tokio::test]
async fn edit_refuses_an_identical_replacement_and_an_absent_string() {
    let f = Fixture::new();
    f.put("uniq.txt", "one\ntwo\nthree\n");
    f.read("uniq.txt").await;
    let same = edit(&f, "uniq.txt", "one", "one", false).await;
    assert!(same.is_error);
    assert_eq!(
        same.text,
        "<tool_use_error>No changes to make: old_string and new_string are exactly the same.</tool_use_error>"
    );
    let absent = edit(&f, "uniq.txt", "zzz", "yyy", false).await;
    assert!(absent.is_error);
    assert_eq!(
        absent.text,
        "<tool_use_error>String to replace not found in file.\nString: zzz</tool_use_error>"
    );
    assert_eq!(f.get("uniq.txt"), "one\ntwo\nthree\n");
}

#[tokio::test]
async fn edit_does_not_strip_the_line_number_prefix_that_read_adds() {
    let f = Fixture::new();
    f.put("a.txt", &rows(5, "line"));
    f.read("a.txt").await;
    let r = edit(&f, "a.txt", "3\tline 3", "THREE", false).await;
    assert!(r.is_error);
    assert!(r.text.contains("String to replace not found in file."));
    assert_eq!(f.get("a.txt"), rows(5, "line"));
}

#[tokio::test]
async fn a_file_changed_since_it_was_read_is_refused() {
    let f = Fixture::new();
    f.put("a.txt", "v1\n");
    f.read("a.txt").await;
    // Someone else (the user, a formatter) changes it.
    std::thread::sleep(std::time::Duration::from_millis(20));
    f.put("a.txt", "v2 by someone else, longer\n");
    let r = edit(&f, "a.txt", "v2", "v3", false).await;
    assert!(r.is_error);
    assert!(r.text.contains("modified since read"), "{}", r.text);
    assert_eq!(f.get("a.txt"), "v2 by someone else, longer\n");
    // Reading again is what lets the edit through.
    f.read("a.txt").await;
    assert!(!edit(&f, "a.txt", "v2", "v3", false).await.is_error);
}

#[tokio::test]
async fn write_creates_files_and_parents_without_a_read_then_overwrites_after_one() {
    let f = Fixture::new();
    let created = f
        .call(
            "Write",
            json!({"file_path": f.path("new/dir/z.txt"), "content": "z\n"}),
        )
        .await;
    assert!(!created.is_error, "{}", created.text);
    assert_eq!(
        created.text,
        format!(
            "File created successfully at: {} (file state is current in your context — no need to Read it back)",
            f.path("new/dir/z.txt")
        )
    );
    assert_eq!(f.get("new/dir/z.txt"), "z\n");
    // It is now known to the session: overwriting it needs no Read.
    let again = f
        .call(
            "Write",
            json!({"file_path": f.path("new/dir/z.txt"), "content": "zz\n"}),
        )
        .await;
    assert_eq!(
        again.text,
        format!(
            "The file {} has been updated successfully. (file state is current in your context — no need to Read it back)",
            f.path("new/dir/z.txt")
        )
    );
    assert_eq!(f.get("new/dir/z.txt"), "zz\n");
}

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn paths_outside_the_scope_are_refused_by_every_tool() {
    let f = Fixture::new();
    let secret = f.outside.path().join("secret.txt");
    std::fs::write(&secret, "outside\n").unwrap();
    let abs = secret.display().to_string();
    let dotdot = format!(
        "{}/../{}/secret.txt",
        f.dir.path().display(),
        f.outside.path().file_name().unwrap().to_string_lossy()
    );

    for path in [abs.as_str(), dotdot.as_str(), "../secret.txt", "/etc/hosts"] {
        let r = f.call("Read", json!({"file_path": path})).await;
        assert!(
            r.is_error && r.text.contains("outside the directories"),
            "{path}: {}",
            r.text
        );
        let w = f
            .call("Write", json!({"file_path": path, "content": "x"}))
            .await;
        assert!(
            w.is_error && w.text.contains("outside the directories"),
            "{path}: {}",
            w.text
        );
        let e = f
            .call(
                "Edit",
                json!({"file_path": path, "old_string": "a", "new_string": "b"}),
            )
            .await;
        assert!(
            e.is_error && e.text.contains("outside the directories"),
            "{path}: {}",
            e.text
        );
    }
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "outside\n");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_inside_the_scope_does_not_lead_out_of_it() {
    let f = Fixture::new();
    let secret = f.outside.path().join("secret.txt");
    std::fs::write(&secret, "outside\n").unwrap();
    std::os::unix::fs::symlink(&secret, f.dir.path().join("link.txt")).unwrap();
    std::os::unix::fs::symlink(f.outside.path(), f.dir.path().join("linkdir")).unwrap();

    for (tool, args) in [
        ("Read", json!({"file_path": f.path("link.txt")})),
        ("Read", json!({"file_path": f.path("linkdir/secret.txt")})),
        (
            "Write",
            json!({"file_path": f.path("link.txt"), "content": "pwned"}),
        ),
        (
            "Write",
            json!({"file_path": f.path("linkdir/new.txt"), "content": "pwned"}),
        ),
        // `..` after a symlink is relative to where the symlink leads, not to where it sits.
        (
            "Read",
            json!({"file_path": f.path("linkdir/../secret.txt")}),
        ),
    ] {
        let r = f.call(tool, args.clone()).await;
        assert!(
            r.is_error && r.text.contains("outside the directories"),
            "{tool} {args}: {}",
            r.text
        );
    }
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "outside\n");
    assert!(!f.outside.path().join("new.txt").exists());
}

#[tokio::test]
async fn an_additional_directory_is_part_of_the_scope() {
    let extra = tempfile::tempdir().unwrap();
    std::fs::write(extra.path().join("x.txt"), "extra\n").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let scope = Scope::new(dir.path(), [extra.path().to_path_buf()]).unwrap();
    let registry = register(ToolRegistry::new(), &FileConfig::new(scope));
    let profile = nexus_tools::Profile::unrestricted("s");
    let context = CallContext::new("s", Arc::new(SessionState::default()));
    let r = registry
        .resolve(&profile, "Read")
        .unwrap()
        .call(
            &context,
            json!({"file_path": extra.path().join("x.txt").display().to_string()}),
        )
        .await;
    assert_eq!(r.text, "1\textra\n2\t");
}

// ---------------------------------------------------------------------------
// Atomic writes
// ---------------------------------------------------------------------------

fn no_temporary_file_left(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .unwrap()
        .all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp"))
}

/// A reader that never stops sees the old content or the new content, whole, every time:
/// never a prefix, never an empty file.
#[tokio::test]
async fn a_reader_never_sees_a_half_written_file() {
    let f = Fixture::new();
    let a = "A".repeat(2_000_000);
    let b = "B".repeat(2_000_000);
    f.put("shared.txt", &a);
    f.read("shared.txt").await;

    let path = f.dir.path().join("shared.txt");
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let (path, stop, a, b) = (path.clone(), Arc::clone(&stop), a.clone(), b.clone());
        std::thread::spawn(move || {
            let mut reads = 0u32;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let seen = std::fs::read_to_string(&path).unwrap();
                assert!(
                    seen == a || seen == b,
                    "a torn file of {} bytes",
                    seen.len()
                );
                reads += 1;
            }
            reads
        })
    };
    for round in 0..40 {
        let (old, new) = if round % 2 == 0 { (&a, &b) } else { (&b, &a) };
        let r = f
            .call(
                "Edit",
                json!({"file_path": f.path("shared.txt"), "old_string": old, "new_string": new}),
            )
            .await;
        assert!(!r.is_error, "{}", &r.text[..r.text.len().min(200)]);
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(reader.join().unwrap() > 0);
    assert!(no_temporary_file_left(f.dir.path()));
}

#[cfg(unix)]
#[tokio::test]
async fn a_write_that_cannot_happen_leaves_the_file_and_the_directory_as_they_were() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    f.put("locked/a.txt", "original\n");
    f.read("locked/a.txt").await;
    let locked = f.dir.path().join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(locked.join("probe"), "x").is_ok() {
        // Running as root: permissions do not bind. The case cannot be staged.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let r = edit(&f, "locked/a.txt", "original", "changed", false).await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(r.is_error);
    assert_eq!(f.get("locked/a.txt"), "original\n");
    assert!(no_temporary_file_left(&locked));
}

#[cfg(unix)]
#[tokio::test]
async fn overwriting_keeps_the_permissions_of_the_file() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    f.put("run.sh", "echo one\n");
    std::fs::set_permissions(
        f.dir.path().join("run.sh"),
        std::fs::Permissions::from_mode(0o750),
    )
    .unwrap();
    f.read("run.sh").await;
    assert!(!edit(&f, "run.sh", "one", "two", false).await.is_error);
    let mode = std::fs::metadata(f.dir.path().join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o750);
}

#[tokio::test]
async fn the_previous_content_is_kept_in_the_backup_directory_when_one_is_set() {
    let backups = tempfile::tempdir().unwrap();
    let f = Fixture::with(|c| c.with_backup_dir(backups.path()));
    f.put("a.txt", "before\n");
    f.read("a.txt").await;
    assert!(!edit(&f, "a.txt", "before", "after", false).await.is_error);
    let saved: Vec<_> = std::fs::read_dir(backups.path()).unwrap().collect();
    assert_eq!(saved.len(), 1);
    assert_eq!(
        std::fs::read_to_string(saved[0].as_ref().unwrap().path()).unwrap(),
        "before\n"
    );
    assert_eq!(f.get("a.txt"), "after\n");
}

// ---------------------------------------------------------------------------
// Annotations: what the harness's policy reads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_annotations_tell_the_policy_which_tools_change_things() {
    let f = Fixture::new();
    let profile = nexus_tools::Profile::unrestricted("s");
    let note = |name: &str| f.registry.resolve(&profile, name).unwrap().annotations();
    assert!(note("Read").read_only);
    assert!(!note("Write").read_only && note("Write").destructive);
    assert!(!note("Edit").read_only && note("Edit").destructive);
}

// ---------------------------------------------------------------------------
// NotebookEdit
// ---------------------------------------------------------------------------

const NOTEBOOK: &str = r##"{"cells":[{"id":"c1","cell_type":"code","metadata":{},"source":"print('a')","outputs":[{"output_type":"stream","name":"stdout","text":"a\n"}],"execution_count":3},{"id":"c2","cell_type":"markdown","metadata":{},"source":"# title"}],"metadata":{},"nbformat":4,"nbformat_minor":5}"##;

async fn nb(f: &Fixture, args: Value) -> ToolResult {
    let mut args = args;
    args["notebook_path"] = json!(f.path("nb.ipynb"));
    f.call("NotebookEdit", args).await
}

#[tokio::test]
async fn notebook_edit_follows_the_recorded_sequence_and_file_format() {
    let f = Fixture::new();
    f.put("nb.ipynb", NOTEBOOK);
    // Not read yet: refused, nothing changed.
    let refused = nb(&f, json!({"cell_id": "c1", "new_source": "print('b')"})).await;
    assert!(refused.is_error);
    assert_eq!(refused.text, NOT_READ);
    assert_eq!(f.get("nb.ipynb"), NOTEBOOK);

    f.read("nb.ipynb").await;
    let replaced = nb(&f, json!({"cell_id": "c1", "new_source": "print('b')"})).await;
    assert_eq!(replaced.text, "Updated cell c1 with print('b')");

    let inserted = nb(
        &f,
        json!({"cell_id": "c1", "new_source": "x = 1", "cell_type": "code", "edit_mode": "insert"}),
    )
    .await;
    let id = inserted
        .text
        .strip_prefix("Inserted cell ")
        .unwrap()
        .strip_suffix(" with x = 1")
        .unwrap()
        .to_owned();
    assert_eq!(id.len(), 8);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "{id}");

    let deleted = nb(
        &f,
        json!({"cell_id": "c2", "new_source": "", "edit_mode": "delete"}),
    )
    .await;
    assert_eq!(deleted.text, "Deleted cell c2");

    // The file as Claude Code wrote it: one-space indent, source as a string, the inserted
    // cell's keys in the recorded order, the existing cell's keys where they were.
    let expected = r#"{
 "cells": [
  {
   "id": "c1",
   "cell_type": "code",
   "metadata": {},
   "source": "print('b')",
   "outputs": [],
   "execution_count": null
  },
  {
   "cell_type": "code",
   "id": "<ID>",
   "source": "x = 1",
   "metadata": {},
   "execution_count": null,
   "outputs": []
  }
 ],
 "metadata": {},
 "nbformat": 4,
 "nbformat_minor": 5
}"#
    .replace("<ID>", &id);
    assert_eq!(f.get("nb.ipynb"), expected);
    // Still a valid notebook after every operation.
    let parsed: Value = serde_json::from_str(&f.get("nb.ipynb")).unwrap();
    assert_eq!(parsed["cells"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn notebook_edit_errors_are_the_recorded_ones_and_change_nothing() {
    let f = Fixture::new();
    f.put("nb.ipynb", NOTEBOOK);
    f.read("nb.ipynb").await;
    let unknown = nb(&f, json!({"cell_id": "nope", "new_source": "z"})).await;
    assert!(unknown.is_error);
    assert_eq!(
        unknown.text,
        "<tool_use_error>Cell with ID \"nope\" not found in notebook.</tool_use_error>"
    );
    let no_type = nb(
        &f,
        json!({"cell_id": "c1", "new_source": "z", "edit_mode": "insert"}),
    )
    .await;
    assert!(
        no_type.is_error && no_type.text.contains("cell_type is required"),
        "{}",
        no_type.text
    );
    let not_a_notebook = f
        .call(
            "NotebookEdit",
            json!({"notebook_path": f.path("a.txt"), "new_source": "z"}),
        )
        .await;
    assert!(not_a_notebook.is_error);
    assert_eq!(f.get("nb.ipynb"), NOTEBOOK);
}

#[tokio::test]
async fn notebook_edit_inserts_first_without_a_cell_id_and_is_in_scope() {
    let f = Fixture::new();
    f.put("nb.ipynb", NOTEBOOK);
    f.read("nb.ipynb").await;
    let r = nb(
        &f,
        json!({"new_source": "# top", "cell_type": "markdown", "edit_mode": "insert"}),
    )
    .await;
    assert!(!r.is_error, "{}", r.text);
    let parsed: Value = serde_json::from_str(&f.get("nb.ipynb")).unwrap();
    assert_eq!(parsed["cells"][0]["source"], "# top");
    assert_eq!(parsed["cells"][1]["id"], "c1");
    // A markdown cell has no outputs.
    assert!(parsed["cells"][0].get("outputs").is_none());

    let outside = f.outside.path().join("o.ipynb");
    std::fs::write(&outside, NOTEBOOK).unwrap();
    let r = f
        .call("NotebookEdit", json!({"notebook_path": outside.display().to_string(), "cell_id": "c1", "new_source": "x"}))
        .await;
    assert!(
        r.is_error && r.text.contains("outside the directories"),
        "{}",
        r.text
    );
    assert_eq!(std::fs::read_to_string(outside).unwrap(), NOTEBOOK);
}

// ---------------------------------------------------------------------------
// Pictures (N19b)
// ---------------------------------------------------------------------------

const PNG_1X1: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, b'I', b'H', b'D', b'R', 0, 0, 0,
    1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89,
];

#[tokio::test]
async fn read_returns_a_picture_as_an_image_block_with_a_text_stand_in() {
    use base64::Engine as _;
    let f = Fixture::new();
    std::fs::write(f.dir.path().join("p.png"), PNG_1X1).unwrap();
    let r = f.read("p.png").await;
    assert!(!r.is_error, "{}", r.text);
    assert_eq!(
        r.text,
        format!(
            "[image: {}, image/png, {} bytes]",
            f.path("p.png"),
            PNG_1X1.len()
        )
    );
    assert_eq!(r.images.len(), 1);
    assert_eq!(r.images[0].mime_type, "image/png");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&r.images[0].data)
            .unwrap(),
        PNG_1X1
    );
    // Reading it made it known to the session, like any read.
    assert!(
        !f.call(
            "Write",
            json!({"file_path": f.path("p.png"), "content": "x"})
        )
        .await
        .is_error
    );
}

#[tokio::test]
async fn a_file_that_claims_to_be_a_picture_but_is_not_is_read_as_what_it_is() {
    let f = Fixture::new();
    f.put("fake.png", "just text\n");
    let r = f.read("fake.png").await;
    assert!(r.images.is_empty());
    assert_eq!(r.text, "1\tjust text\n2\t");
}

// ---------------------------------------------------------------------------
// PDF (N19b)
// ---------------------------------------------------------------------------

/// A valid minimal PDF with one line of text per page.
fn pdf(pages: &[&str]) -> Vec<u8> {
    let mut objects: Vec<String> = Vec::new();
    let n = pages.len();
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".into());
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
    objects.push(format!(
        "<< /Type /Pages /Kids [{}] /Count {n} >>",
        kids.join(" ")
    ));
    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into());
    for (i, text) in pages.iter().enumerate() {
        objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>", 5 + 2 * i));
        let stream = format!("BT /F1 12 Tf 20 150 Td ({text}) Tj ET");
        objects.push(format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ));
    }
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

#[tokio::test]
async fn read_extracts_the_text_of_a_short_pdf_page_by_page() {
    let f = Fixture::new();
    std::fs::write(
        f.dir.path().join("d.pdf"),
        pdf(&["alpha page", "beta page"]),
    )
    .unwrap();
    let r = f.read("d.pdf").await;
    assert!(!r.is_error, "{}", r.text);
    assert!(
        r.text.contains("--- page 1 of 2 ---\nalpha page"),
        "{}",
        r.text
    );
    assert!(
        r.text.contains("--- page 2 of 2 ---\nbeta page"),
        "{}",
        r.text
    );
}

#[tokio::test]
async fn a_long_pdf_must_be_read_in_ranges_of_at_most_twenty_pages() {
    let f = Fixture::new();
    let texts: Vec<String> = (1..=30).map(|n| format!("text of page {n}")).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    std::fs::write(f.dir.path().join("long.pdf"), pdf(&refs)).unwrap();
    let whole = f.read("long.pdf").await;
    assert!(
        whole.is_error && whole.text.contains("30 pages") && whole.text.contains("`pages`"),
        "{}",
        whole.text
    );
    let ask = |pages: &str| json!({"file_path": f.path("long.pdf"), "pages": pages});
    let some = f.call("Read", ask("2-3,28")).await;
    assert!(!some.is_error, "{}", some.text);
    assert!(
        some.text.contains("text of page 2")
            && some.text.contains("text of page 28")
            && !some.text.contains("page 4"),
        "{}",
        some.text
    );
    let too_many = f.call("Read", ask("1-21")).await;
    assert!(
        too_many.is_error && too_many.text.contains("at most 20"),
        "{}",
        too_many.text
    );
    assert!(f.call("Read", ask("99")).await.is_error);
    assert!(f.call("Read", ask("x")).await.is_error);
}

#[tokio::test]
async fn a_damaged_pdf_is_an_error_not_a_crash() {
    let f = Fixture::new();
    f.put("bad.pdf", "%PDF-1.4\nnot really\n");
    let r = f.read("bad.pdf").await;
    assert!(
        r.is_error && r.text.contains("not a PDF that can be read"),
        "{}",
        r.text
    );
}

// ---------------------------------------------------------------------------
// Size ceiling (N26): Read and Edit load the file whole, so a huge one is refused
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_and_edit_refuse_a_file_over_the_ceiling_and_say_how_to_go_on() {
    let f = Fixture::with(|config| config.with_max_text_bytes(1000));
    f.put("big.txt", &"x".repeat(1001));
    f.put("ok.txt", &"y".repeat(1000));

    let r = f
        .call("Read", json!({"file_path": f.path("big.txt")}))
        .await;
    assert!(
        r.is_error
            && r.text
                .contains("1001 bytes, over the 1000 byte limit of Read"),
        "{}",
        r.text
    );
    assert!(r.text.contains("Grep"), "{}", r.text);

    let r = f
        .call(
            "Edit",
            json!({"file_path": f.path("big.txt"), "old_string": "x", "new_string": "z", "replace_all": true}),
        )
        .await;
    assert!(r.is_error && r.text.contains("limit of Edit"), "{}", r.text);
    assert_eq!(
        f.get("big.txt"),
        "x".repeat(1001),
        "a refused edit must not touch the file"
    );

    // At the ceiling exactly, it still reads.
    let r = f.call("Read", json!({"file_path": f.path("ok.txt")})).await;
    assert!(!r.is_error, "{}", r.text);
}
