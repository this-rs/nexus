//! `Bash`, `TaskStop`, `Monitor` (N20): the recorded behaviours of Claude Code 2.1.287, process
//! groups, background tasks, environment isolation, and the session's working directory.
#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nexus_tools::files::Scope;
use nexus_tools::shell::{EnvPolicy, ShellConfig, register};
use nexus_tools::{CallContext, Profile, SessionState, ToolRegistry, ToolResult};
use serde_json::{Value, json};

struct Fixture {
    dir: tempfile::TempDir,
    registry: ToolRegistry,
    context: CallContext,
    config: ShellConfig,
}

fn fixture_with(env: EnvPolicy) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let out = dir.path().join("out");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let scope = Arc::new(Scope::new(&work, [out.clone()]).unwrap());
    let config = ShellConfig::new(scope, out).unwrap().with_env(env);
    Fixture {
        registry: register(ToolRegistry::new(), &config),
        context: CallContext::new("s", Arc::new(SessionState::default())),
        config,
        dir,
    }
}

fn fixture() -> Fixture {
    fixture_with(EnvPolicy::default())
}

impl Fixture {
    async fn call(&self, tool: &str, arguments: Value) -> ToolResult {
        self.registry
            .resolve(&Profile::unrestricted("s"), tool)
            .unwrap()
            .call(&self.context, arguments)
            .await
    }

    async fn bash(&self, command: &str) -> ToolResult {
        self.call("Bash", json!({"command": command})).await
    }

    fn work(&self) -> std::path::PathBuf {
        std::fs::canonicalize(self.dir.path().join("work")).unwrap()
    }
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..150 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    panic!("never true: {what}");
}

fn read_pid(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

// ---------------------------------------------------------------------------
// Recorded behaviours
// ---------------------------------------------------------------------------

#[tokio::test]
async fn output_exit_codes_and_merging_are_the_recorded_ones() {
    let f = fixture();
    assert_eq!(f.bash("echo hi").await.text, "hi");
    let r = f.bash("exit 3").await;
    assert!(r.is_error);
    assert_eq!(r.text, "Exit code 3");
    assert_eq!(f.bash("echo out; echo err 1>&2").await.text, "out\nerr");
    assert_eq!(f.bash("true").await.text, "(Bash completed with no output)");
    assert_eq!(f.bash("printf 'no newline'").await.text, "no newline");
    assert_eq!(f.bash("printf 'tab\\there\\n'").await.text, "tab\there");
    // The code of the LAST command: `false` in the middle is not an error.
    let r = f.bash("echo a; false; echo b").await;
    assert!(!r.is_error);
    assert_eq!(r.text, "a\nb");
    let r = f.bash("ls nonexistent-dir").await;
    assert!(r.is_error);
    assert!(r.text.starts_with("Exit code "), "{}", r.text);
    assert!(r.text.contains("nonexistent-dir"), "{}", r.text);
}

#[tokio::test]
async fn a_command_that_runs_too_long_is_ended_with_the_recorded_message() {
    let f = fixture();
    let started = std::time::Instant::now();
    let r = f
        .call("Bash", json!({"command": "sleep 30", "timeout": 1000}))
        .await;
    assert!(r.is_error);
    assert_eq!(r.text, "Exit code 143\nCommand timed out after 1s");
    assert!(started.elapsed() < Duration::from_secs(10));
    // A long timeout is accepted, and a command that finishes first is not an error.
    let r = f
        .call("Bash", json!({"command": "sleep 1", "timeout": 99_999_999}))
        .await;
    assert_eq!(r.text, "(Bash completed with no output)");
    let r = f
        .call(
            "Bash",
            json!({"command": "echo start; sleep 1; echo end", "timeout": 5000}),
        )
        .await;
    assert_eq!(r.text, "start\nend");
}

#[tokio::test]
async fn output_over_30000_bytes_is_saved_to_a_file_and_previewed() {
    let f = fixture();
    let inline = f.bash("head -c 29000 /dev/zero | tr '\\0' 'a'").await;
    assert_eq!(inline.text.len(), 29_000);

    let r = f.bash("head -c 31000 /dev/zero | tr '\\0' 'a'").await;
    assert!(!r.is_error);
    assert!(
        r.text
            .starts_with("<persisted-output>\nOutput too large (30.3KB). Full output saved to: "),
        "{}",
        &r.text[..100]
    );
    assert!(r.text.ends_with("\n...\n</persisted-output>"));
    let path = r
        .text
        .split("saved to: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(path).unwrap().len(),
        31_000,
        "the whole output is on disk"
    );
    let preview = r
        .text
        .split("Preview (first 2KB):\n")
        .nth(1)
        .unwrap()
        .strip_suffix("\n...\n</persisted-output>")
        .unwrap();
    assert_eq!(preview.len(), 2000);

    let r = f.bash("seq 1 60000").await;
    assert!(
        r.text.contains("Output too large (340.7KB)"),
        "{}",
        &r.text[..80]
    );
    // The preview stops at a whole line.
    let preview = r
        .text
        .split("Preview (first 2KB):\n")
        .nth(1)
        .unwrap()
        .strip_suffix("\n...\n</persisted-output>")
        .unwrap();
    assert!(preview.lines().last().unwrap().parse::<u32>().is_ok());
    assert!(preview.len() <= 2000);
}

// ---------------------------------------------------------------------------
// The working directory
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_working_directory_persists_and_a_cd_out_of_scope_is_undone() {
    let f = fixture();
    std::fs::create_dir_all(f.work().join("sub")).unwrap();
    assert_eq!(f.bash("pwd").await.text, f.work().display().to_string());
    assert_eq!(
        f.bash("cd sub").await.text,
        "(Bash completed with no output)"
    );
    assert_eq!(
        f.bash("pwd").await.text,
        f.work().join("sub").display().to_string()
    );
    // Variables do not persist (a new shell each time); the directory does.
    f.bash("X=1").await;
    assert_eq!(f.bash("echo \"[$X]\"").await.text, "[]");

    let r = f.bash("cd /").await;
    assert!(r.text.contains("Shell cwd was reset to"), "{}", r.text);
    assert!(
        r.text.contains(&f.work().join("sub").display().to_string()),
        "{}",
        r.text
    );
    assert_eq!(
        f.bash("pwd").await.text,
        f.work().join("sub").display().to_string()
    );

    // Another session starts at the scope's directory again.
    let other = CallContext::new("o", Arc::new(SessionState::default()));
    let tool = f
        .registry
        .resolve(&Profile::unrestricted("s"), "Bash")
        .unwrap();
    assert_eq!(
        tool.call(&other, json!({"command": "pwd"})).await.text,
        f.work().display().to_string()
    );
}

// ---------------------------------------------------------------------------
// Process groups
// ---------------------------------------------------------------------------

/// `(sleep 300 & echo $! > pid; wait)`: a shell whose child is a grandchild of the call.
fn with_grandchild(f: &Fixture, name: &str) -> (String, std::path::PathBuf) {
    let pid_file = f.dir.path().join(name);
    (
        format!("(sleep 300 & echo $! > {}; wait)", pid_file.display()),
        pid_file,
    )
}

#[tokio::test]
async fn a_timeout_kills_the_descendants_too() {
    let f = fixture();
    let (command, pid_file) = with_grandchild(&f, "pid1");
    let r = f
        .call("Bash", json!({"command": command, "timeout": 1000}))
        .await;
    assert!(r.text.contains("timed out"), "{}", r.text);
    let pid = read_pid(&pid_file);
    eventually("the grandchild is gone after the timeout", || !alive(pid)).await;
}

#[tokio::test]
async fn an_abandoned_call_kills_its_command_and_descendants() {
    let f = fixture();
    let (command, pid_file) = with_grandchild(&f, "pid2");
    // The caller gives up (the request was cancelled): the future is dropped mid-call.
    let abandoned = tokio::time::timeout(
        Duration::from_millis(700),
        f.call("Bash", json!({"command": command})),
    )
    .await;
    assert!(
        abandoned.is_err(),
        "the call should still have been running"
    );
    let pid = read_pid(&pid_file);
    eventually("the grandchild is gone after the call was dropped", || {
        !alive(pid)
    })
    .await;
}

#[tokio::test]
async fn ending_the_session_kills_what_it_left_running() {
    let f = fixture();
    let (command, pid_file) = with_grandchild(&f, "pid3");
    let r = f
        .call(
            "Bash",
            json!({"command": command, "run_in_background": true}),
        )
        .await;
    assert!(
        r.text
            .starts_with("Command running in background with ID: "),
        "{}",
        r.text
    );
    eventually("the grandchild started", || {
        pid_file.exists()
            && !std::fs::read_to_string(&pid_file)
                .unwrap()
                .trim()
                .is_empty()
    })
    .await;
    let pid = read_pid(&pid_file);
    assert!(alive(pid));
    // The session's state is dropped: the server forgot the session.
    let Fixture { context, .. } = f;
    drop(context);
    eventually("the grandchild is gone once the session is over", || {
        !alive(pid)
    })
    .await;
}

// ---------------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------------

fn task_id(text: &str) -> String {
    text.split("ID: ")
        .nth(1)
        .unwrap()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap()
        .to_owned()
}

fn output_path(text: &str) -> String {
    text.split("written to: ")
        .nth(1)
        .unwrap()
        .split(". ")
        .next()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn a_background_task_has_an_id_and_its_output_is_readable_during_and_after() {
    let f = fixture();
    let r = f
        .call(
            "Bash",
            json!({"command": "echo one; sleep 3; echo two", "run_in_background": true}),
        )
        .await;
    assert!(!r.is_error, "{}", r.text);
    let id = task_id(&r.text);
    assert_eq!(id.len(), 9);
    let path = output_path(&r.text);
    // During: the first line is there, the second is not yet.
    eventually("first line", || {
        std::fs::read_to_string(&path).is_ok_and(|t| t == "one\n")
    })
    .await;
    // After: both.
    eventually("both lines", || {
        std::fs::read_to_string(&path).is_ok_and(|t| t == "one\ntwo\n")
    })
    .await;
    // A finished task is still there, and stopping it is not an error.
    let stop = f.call("TaskStop", json!({"task_id": id})).await;
    assert!(!stop.is_error, "{}", stop.text);
    assert!(stop.text.contains("not running"), "{}", stop.text);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
}

#[tokio::test]
async fn task_stop_ends_the_task_and_its_descendants_and_is_idempotent() {
    let f = fixture();
    let (command, pid_file) = with_grandchild(&f, "pid4");
    let r = f
        .call(
            "Bash",
            json!({"command": command, "run_in_background": true}),
        )
        .await;
    let id = task_id(&r.text);
    eventually("started", || {
        pid_file.exists()
            && !std::fs::read_to_string(&pid_file)
                .unwrap()
                .trim()
                .is_empty()
    })
    .await;
    let pid = read_pid(&pid_file);

    let first = f.call("TaskStop", json!({"task_id": id})).await;
    assert!(!first.is_error, "{}", first.text);
    assert!(
        first
            .text
            .starts_with(&format!("Successfully stopped task: {id}")),
        "{}",
        first.text
    );
    eventually("the grandchild is gone", || !alive(pid)).await;
    let second = f.call("TaskStop", json!({"task_id": id})).await;
    assert!(!second.is_error, "idempotent: {}", second.text);
    let unknown = f.call("TaskStop", json!({"task_id": "nosuchid1"})).await;
    assert!(unknown.is_error);
    assert_eq!(unknown.text, "No task found with ID: nosuchid1");
}

#[tokio::test]
async fn a_task_of_another_session_cannot_be_stopped() {
    let f = fixture();
    let r = f
        .call(
            "Bash",
            json!({"command": "sleep 60", "run_in_background": true}),
        )
        .await;
    let id = task_id(&r.text);
    let other = CallContext::new("o", Arc::new(SessionState::default()));
    let stop = f
        .registry
        .resolve(&Profile::unrestricted("s"), "TaskStop")
        .unwrap();
    let r = stop.call(&other, json!({"task_id": id})).await;
    assert!(
        r.is_error && r.text.starts_with("No task found"),
        "{}",
        r.text
    );
    f.call("TaskStop", json!({"task_id": id})).await;
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_command_sees_no_host_variable_outside_the_allow_list() {
    let f = fixture();
    // The test process has CARGO_MANIFEST_DIR (cargo sets it); the command must not.
    assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
    let names = f.bash("env | cut -d= -f1 | sort").await.text;
    let allowed = [
        "PATH", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "HOME", "PWD", "SHLVL", "OLDPWD", "_",
    ];
    for name in names.lines() {
        assert!(
            allowed.contains(&name),
            "`{name}` reached the command: {names}"
        );
    }
    // HOME is the server's own, never the user's.
    let home = f.bash("echo $HOME").await.text;
    assert_eq!(
        home,
        f.config.output_dir().join("home").display().to_string()
    );
    assert!(
        !home.contains(&std::env::var("HOME").unwrap_or_default())
            || std::env::var("HOME").unwrap_or_default().is_empty()
    );
    // And the wrapper's own variables are gone by the time the command runs.
    let r = f
        .bash("echo \"[$NEXUS_TOOLS_CMD][$NEXUS_TOOLS_CWDFILE]\"")
        .await;
    assert_eq!(r.text, "[][]");
}

#[tokio::test]
async fn an_inherited_or_explicit_variable_is_visible_only_when_the_policy_says_so() {
    let mut policy = EnvPolicy::default();
    policy.inherit.push("CARGO_MANIFEST_DIR".into());
    policy.set.push(("GREETING".into(), "hello".into()));
    let f = fixture_with(policy);
    assert_eq!(f.bash("echo $GREETING").await.text, "hello");
    assert_eq!(
        f.bash("echo $CARGO_MANIFEST_DIR").await.text,
        std::env::var("CARGO_MANIFEST_DIR").unwrap()
    );
}

#[tokio::test]
async fn the_command_text_is_not_on_the_command_line() {
    let f = fixture();
    let r = f.bash("echo SENTINEL-9f2c; ps -o args= -p $$").await;
    assert_eq!(
        r.text.matches("SENTINEL-9f2c").count(),
        1,
        "the command line holds the command: {}",
        r.text
    );
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bad_arguments_are_refused_before_anything_runs() {
    let f = fixture();
    for args in [
        json!({}),
        json!({"command": ""}),
        json!({"command": "   "}),
        json!({"command": 3}),
    ] {
        let r = f.call("Bash", args.clone()).await;
        assert!(r.is_error, "{args}");
    }
    let r = f
        .call("Bash", json!({"command": "x".repeat(100_001)}))
        .await;
    assert!(
        r.is_error && r.text.contains("longer than"),
        "{}",
        &r.text[..60]
    );
    let r = f.call("TaskStop", json!({})).await;
    assert!(r.is_error);
}

#[tokio::test]
async fn the_shell_tools_say_what_they_do_to_the_policy() {
    let f = fixture();
    let profile = Profile::unrestricted("s");
    for name in ["Bash", "TaskStop", "Monitor"] {
        let a = f.registry.resolve(&profile, name).unwrap().annotations();
        assert!(!a.read_only && a.destructive, "{name}");
    }
    assert!(
        f.registry
            .resolve(&profile, "Bash")
            .unwrap()
            .annotations()
            .open_world
    );
}

// ---------------------------------------------------------------------------
// Output ceiling (N26): a command that floods its output file is ended
// ---------------------------------------------------------------------------

fn with_ceiling(f: &mut Fixture, bytes: u64) {
    f.config = f.config.clone().with_max_output_bytes(bytes);
    f.registry = register(ToolRegistry::new(), &f.config);
}

#[tokio::test]
async fn a_command_that_floods_its_output_is_ended_at_the_ceiling() {
    let mut f = fixture();
    with_ceiling(&mut f, 1024 * 1024);
    let started = std::time::Instant::now();
    let r = f.bash("yes flood").await;
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "yes was not stopped"
    );
    assert!(r.is_error, "{}", r.text);
    assert!(r.text.contains("went past 1 MiB"), "{}", r.text);
    // What is on disk is bounded: the ceiling plus the note, never the flood.
    let path = r
        .text
        .split("saved to: ")
        .nth(1)
        .unwrap()
        .split('\n')
        .next()
        .unwrap()
        .to_owned();
    let size = std::fs::metadata(&path).unwrap().len();
    assert!(size < 1024 * 1024 + 4096, "{size} bytes on disk");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(
        body.trim_end().ends_with("was ended]"),
        "no stop note in the file"
    );
}

#[tokio::test]
async fn a_background_task_that_floods_is_ended_and_its_file_says_so() {
    let mut f = fixture();
    with_ceiling(&mut f, 512 * 1024);
    let r = f
        .call(
            "Bash",
            json!({"command": "yes flood", "run_in_background": true}),
        )
        .await;
    let path = output_path(&r.text);
    eventually("the flood is cut", || {
        std::fs::read_to_string(&path).is_ok_and(|t| t.trim_end().ends_with("was ended]"))
    })
    .await;
    assert!(std::fs::metadata(&path).unwrap().len() < 512 * 1024 + 4096);
}
