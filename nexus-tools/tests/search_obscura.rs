//! The `obscura` engine (N22): Obscura's keyless `serp`, started as an external executable. A
//! fake `/bin/sh` script stands in for it; the real binary is never needed. The golden
//! `tests/data/obscura_serp.json` is an answer of the real `dsh-obscura-nav serp`, recorded once.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nexus_tools::search::{
    Engine, ObscuraBackend, Protection, SearchBackend, SearchQuery, WebSearchTool,
};
use nexus_tools::web::SystemClock;
use nexus_tools::{CallContext, SessionState, Tool, ToolResult};
use serde_json::json;

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/obscura_serp.json");

/// The committed stand-in (`tests/data/fake_obscura.sh`): never written at test time, so no
/// spawn can fail with ETXTBSY on Linux.
const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/fake_obscura.sh");

/// A fake `obscura` in `dir`: a link to [`FAKE`], which records its argv (one per line, in
/// `dir/argv`) and environment (`dir/env`), then sources `body` from `dir/body.sh`.
fn fake(dir: &Path, body: &str) -> PathBuf {
    std::fs::write(dir.join("body.sh"), format!("{body}\n")).unwrap();
    let path = dir.join("fake-obscura");
    std::os::unix::fs::symlink(FAKE, &path).unwrap();
    path
}

fn query(text: &str, limit: usize) -> SearchQuery {
    SearchQuery {
        text: text.into(),
        limit,
    }
}

fn tool(backend: ObscuraBackend) -> WebSearchTool {
    WebSearchTool::new(
        Engine::new(Arc::new(SystemClock::default())).with_backend(backend, Protection::default()),
    )
}

async fn call(tool: &WebSearchTool, args: serde_json::Value) -> ToolResult {
    tool.call(
        &CallContext::new("s", Arc::new(SessionState::default())),
        args,
    )
    .await
}

/// Whether `pid` is still a running process. A zombie (exited, not yet reaped by whoever its
/// parent is now) is not: on Linux it still answers `kill -0`, so its state is read first.
fn alive(pid: i32) -> bool {
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // `pid (comm) S ...`: the state follows the last `)`.
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.trim_start().chars().next());
        return !matches!(state, Some('Z' | 'X') | None);
    }
    if std::path::Path::new("/proc/self/stat").exists() {
        return false; // a /proc without this pid: gone
    }
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Polls until `path` has content, with a deadline (slow CI, coverage builds).
async fn wait_for(path: &Path) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.trim().is_empty()
        {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{} never appeared", path.display());
}

#[tokio::test]
async fn the_golden_answer_maps_to_hits() {
    let dir = tempfile::tempdir().unwrap();
    let program = fake(dir.path(), &format!("cat '{GOLDEN}'"));
    let hits = ObscuraBackend::new(&program)
        .search(&query("rust headless browser", 10))
        .await
        .unwrap();
    assert_eq!(hits.len(), 3, "{hits:?}");
    for hit in &hits {
        assert!(hit.url.starts_with("https://"), "{hit:?}");
        assert!(!hit.title.is_empty() && hit.snippet.is_some(), "{hit:?}");
    }
    assert_eq!(
        ObscuraBackend::new(&program)
            .search(&query("x y", 2))
            .await
            .unwrap()
            .len(),
        2,
        "the limit holds even if the executable prints more"
    );
}

#[tokio::test]
async fn the_query_is_one_argument_after_double_dash_and_no_shell_sees_it() {
    let dir = tempfile::tempdir().unwrap();
    let program = fake(dir.path(), "echo '[]'");
    let marker = dir.path().join("pwned");
    let text = format!(
        "--max-results 999 ; touch '{}' $(touch '{}')",
        marker.display(),
        marker.display()
    );
    let hits = ObscuraBackend::new(&program)
        .search(&query(&text, 7))
        .await
        .unwrap();
    assert!(hits.is_empty(), "no results is an answer");
    let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
    let argv: Vec<&str> = argv.lines().collect();
    assert_eq!(argv, ["serp", "--max-results", "7", "--", text.as_str()]);
    assert!(!marker.exists(), "the query reached a shell");
}

#[tokio::test]
async fn the_environment_is_an_allow_list_and_private_networks_stay_off() {
    let dir = tempfile::tempdir().unwrap();
    let program = fake(dir.path(), "echo '[]'");
    // SAFETY of the test: a variable no allow-list names; this test only reads it back.
    unsafe {
        std::env::set_var("NEXUS_OBSCURA_TEST_SECRET", "sk-should-not-pass");
        std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    }
    ObscuraBackend::new(&program)
        .search(&query("env check", 3))
        .await
        .unwrap();
    let env = std::fs::read_to_string(dir.path().join("env")).unwrap();
    assert!(!env.contains("sk-should-not-pass"), "{env}");
    assert!(
        env.lines().any(|l| l == "OBSCURA_ALLOW_PRIVATE_NETWORK=0"),
        "{env}"
    );
    assert!(!env.lines().any(|l| l.starts_with("HOME=")), "{env}");
}

#[tokio::test]
async fn domain_filters_apply_to_obscura_like_every_engine() {
    let dir = tempfile::tempdir().unwrap();
    let program = fake(
        dir.path(),
        r#"echo '[{"url":"https://docs.rs/a","title":"Docs","snippet":"d"},{"url":"https://github.com/b","title":"Hub","snippet":"h"},{"url":"https://evil.test/c","title":"Evil","snippet":"e"}]'"#,
    );
    let backend = ObscuraBackend::new(&program);

    let blocked = call(
        &tool(backend.clone()),
        json!({"query": "filters", "blocked_domains": ["evil.test"]}),
    )
    .await;
    assert!(!blocked.is_error, "{}", blocked.text);
    assert!(blocked.text.contains("(via obscura)"), "{}", blocked.text);
    assert!(
        blocked.text.contains("https://docs.rs/a") && blocked.text.contains("https://github.com/b")
    );
    assert!(!blocked.text.contains("evil.test"), "{}", blocked.text);

    let allowed = call(
        &tool(backend),
        json!({"query": "filters", "allowed_domains": ["docs.rs"]}),
    )
    .await;
    assert!(
        allowed.text.contains("https://docs.rs/a"),
        "{}",
        allowed.text
    );
    assert!(
        !allowed.text.contains("github.com") && !allowed.text.contains("evil.test"),
        "{}",
        allowed.text
    );
}

#[tokio::test]
async fn a_missing_executable_is_a_clear_error_that_names_it() {
    let missing = "/nonexistent/dir/dsh-obscura-nav";
    let error = ObscuraBackend::new(missing)
        .search(&query("anything", 3))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), "not_configured");
    assert!(error.to_string().contains(missing), "{error}");

    let answer = call(
        &tool(ObscuraBackend::new(missing)),
        json!({"query": "anything"}),
    )
    .await;
    assert!(answer.is_error);
    assert!(
        answer.text.contains("not installed") && answer.text.contains(missing),
        "{}",
        answer.text
    );
}

#[tokio::test]
async fn a_failure_reports_its_last_stderr_line_and_garbage_is_a_bad_response() {
    let dir = tempfile::tempdir().unwrap();
    let failing = fake(
        dir.path(),
        "echo noise >&2; echo 'Error: SERP blocked' >&2; exit 3",
    );
    let error = ObscuraBackend::new(&failing)
        .search(&query("fail", 3))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), "unavailable");
    let message = error.to_string();
    assert!(
        message.contains("code 3") && message.contains("Error: SERP blocked"),
        "{message}"
    );
    assert!(!message.contains("noise"), "{message}");

    let dir = tempfile::tempdir().unwrap();
    let garbage = fake(dir.path(), "echo '<html>captcha</html>'");
    let error = ObscuraBackend::new(&garbage)
        .search(&query("garbage", 3))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), "bad_response");
}

/// A fake that starts a grandchild, writes both pids, and hangs.
fn hanging(dir: &Path) -> (PathBuf, PathBuf) {
    let pids = dir.join("pids");
    let program = fake(
        dir,
        &format!("sleep 60 & echo \"$$ $!\" > '{}'; wait", pids.display()),
    );
    (program, pids)
}

fn parse_pids(text: &str) -> Vec<i32> {
    text.split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect()
}

async fn all_gone(pids: &[i32]) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if pids.iter().all(|&p| !alive(p)) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn a_slow_search_times_out_and_its_whole_group_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let (program, pids) = hanging(dir.path());
    let error = ObscuraBackend::new(&program)
        .with_timeout(Duration::from_secs(5))
        .search(&query("slow", 3))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), "timeout");
    let pids = parse_pids(&wait_for(&pids).await);
    assert_eq!(pids.len(), 2);
    assert!(
        all_gone(&pids).await,
        "a process of the search survived: {pids:?}"
    );
}

#[tokio::test]
async fn a_cancelled_search_kills_its_whole_group() {
    let dir = tempfile::tempdir().unwrap();
    let (program, pids_file) = hanging(dir.path());
    let backend = ObscuraBackend::new(&program);
    let search = tokio::spawn(async move { backend.search(&query("cancelled", 3)).await });
    let pids = tokio::select! {
        text = wait_for(&pids_file) => parse_pids(&text),
        () = async {
            while !search.is_finished() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        } => {
            panic!("the search ended before it was cancelled: {:?}", search.await)
        },
    };
    assert!(pids.iter().all(|&p| alive(p)), "started: {pids:?}");
    search.abort();
    let _ = search.await;
    assert!(
        all_gone(&pids).await,
        "a process of the search survived: {pids:?}"
    );
}

#[test]
fn the_executable_is_found_on_a_path_under_its_distribution_name_first() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    for (dir, name) in [(a.path(), "obscura"), (b.path(), "dsh-obscura-nav")] {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = std::env::join_paths([a.path(), b.path()]).unwrap();
    let found = ObscuraBackend::locate_in(&path).unwrap();
    assert_eq!(found.program(), b.path().join("dsh-obscura-nav"));
    let only_a = std::env::join_paths([a.path()]).unwrap();
    assert_eq!(
        ObscuraBackend::locate_in(&only_a).unwrap().program(),
        a.path().join("obscura")
    );
    let empty = tempfile::tempdir().unwrap();
    assert!(ObscuraBackend::locate_in(&std::env::join_paths([empty.path()]).unwrap()).is_none());
}

#[tokio::test]
async fn a_runaway_output_is_a_bad_response_at_once_not_a_timeout() {
    let dir = tempfile::tempdir().unwrap();
    // Writes for ever: only the ceiling can end it.
    let program = fake(dir.path(), "exec yes 0123456789abcdef");
    let started = std::time::Instant::now();
    let error = ObscuraBackend::new(&program)
        .with_timeout(Duration::from_secs(30))
        .search(&query("runaway", 3))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), "bad_response", "{error}");
    assert!(error.to_string().contains("more than"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "ended by the ceiling, not the time limit: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_chatty_stderr_never_blocks_the_answer() {
    let dir = tempfile::tempdir().unwrap();
    // 1 MiB on stderr (far past the pipe buffer and the kept 16 KiB), then a valid answer.
    let program = fake(
        dir.path(),
        r#"head -c 1048576 /dev/zero >&2; echo '[{"url":"https://docs.rs/x","title":"X","snippet":"s"}]'"#,
    );
    let hits = ObscuraBackend::new(&program)
        .with_timeout(Duration::from_secs(10))
        .search(&query("chatty", 3))
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
}
