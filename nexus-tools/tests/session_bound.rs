//! One `nexus-tools` per session, bounded by construction (N27): `--trust-harness` is for one
//! harness over a private pipe and never for HTTP; `--tools` bounds what the process lists AND
//! runs, whatever a client sends; the native harness passes the set its session policy exposes,
//! and the scope of each process is the directories it was launched with.
//!
//! Real binary, real pipes. No POSIX path is written here: every path comes from a temporary
//! directory, and file contents are compared without caring for CRLF.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nexus_claude::agent::{McpServerSpec, PolicyMode, SessionSpec, ToolPolicy};
use nexus_claude::providers::native::DefaultTools;
use nexus_tools::{Claims, SigningKey, issue};
use serde_json::{Value, json};

const NEXUS_TOOLS: &str = env!("CARGO_BIN_EXE_nexus-tools");
const KEY: &str = "0123456789abcdef0123456789abcdef-test-key";

fn binary(args: &[&str]) -> Command {
    let mut command = Command::new(NEXUS_TOOLS);
    command
        .args(args)
        .env_remove("NEXUS_TOOLS_PROFILE")
        .env_remove("NEXUS_TOOLS_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn dir() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    // Resolved where the temporary directory is reached through a link (macOS `/var`); left as is
    // on Windows, where canonical paths take the `\\?\` form.
    let path = if cfg!(windows) {
        temp.path().to_path_buf()
    } else {
        std::fs::canonicalize(temp.path()).unwrap()
    };
    (temp, path)
}

fn text_of(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .replace("\r\n", "\n")
}

fn profile_token(tools: &[&str]) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;
    issue(
        &SigningKey::new(KEY.as_bytes().to_vec()).unwrap(),
        &Claims {
            sid: "bound".into(),
            tools: tools.iter().map(|t| (*t).to_owned()).collect(),
            exp,
        },
    )
}

/// Sends `initialize` then the requests, reads every answer (any order), closes stdin and waits.
fn talk(mut child: Child, requests: &[Value]) -> Vec<Value> {
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-03-26"}})
    )
    .unwrap();
    for request in requests {
        writeln!(stdin, "{request}").unwrap();
    }
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut answers: Vec<Value> = Vec::new();
    while answers.len() < requests.len() + 1 {
        let mut line = String::new();
        if stdout.read_line(&mut line).unwrap() == 0 {
            let mut stderr = String::new();
            std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).ok();
            panic!("stdout closed early: {answers:?}\nstderr: {stderr}");
        }
        answers.push(serde_json::from_str(&line).expect("stdout is JSON lines only"));
    }
    drop(stdin);
    child.wait().unwrap();
    answers.sort_by_key(|a| a["id"].as_u64());
    answers.remove(0); // initialize
    answers
}

fn list() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})
}

fn call(id: u64, name: &str, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}})
}

fn names(answer: &Value) -> Vec<String> {
    answer["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("not a tools/list result: {answer}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

/// A tools/call that the SERVER refused (a JSON-RPC error, the tool never ran).
fn refused(answer: &Value) -> bool {
    answer["error"]["code"] == -32602
        && answer["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("unknown tool"))
}

/// The text of a tools/call result, and whether it is an error result.
fn result_text(answer: &Value) -> (String, bool) {
    (
        answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        answer["result"]["isError"] == true,
    )
}

/// Waits at most `limit` for the child to exit; kills it otherwise.
fn exit_within(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().ok();
    child.wait().ok();
    None
}

/// A command line to run in Bash that leaves `marker` behind if it runs.
fn touch(marker: &Path) -> Value {
    json!({"command": format!("echo ran > \"{}\"", marker.display())})
}

// ---------------------------------------------------------------------------
// --trust-harness is stdio only
// ---------------------------------------------------------------------------

#[test]
fn trust_harness_cannot_be_combined_with_listen() {
    // With a valid key: without the rule, the server would start listening and never exit.
    let mut child = binary(&["--trust-harness", "--listen", "127.0.0.1:0"])
        .env("NEXUS_TOOLS_KEY", KEY)
        .spawn()
        .unwrap();
    let status = exit_within(&mut child, Duration::from_secs(10))
        .expect("--trust-harness --listen must refuse to start, it served instead");
    assert_eq!(status.code(), Some(2));
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(
        stderr.contains(
            "--trust-harness is for one harness over a private pipe; it cannot be combined with --listen"
        ),
        "{stderr}"
    );
    let mut stdout = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut stdout).unwrap();
    assert!(!stdout.contains("LISTENING"), "{stdout}");
}

// ---------------------------------------------------------------------------
// --tools
// ---------------------------------------------------------------------------

#[test]
fn tools_bounds_the_list_and_refuses_a_direct_call_to_any_other_tool() {
    let (_temp, cwd) = dir();
    let written = cwd.join("written.txt");
    let marker = cwd.join("marker");
    let child = binary(&[
        "--trust-harness",
        "--cwd",
        cwd.to_str().unwrap(),
        "--tools",
        "Read,Glob",
    ])
    .spawn()
    .unwrap();
    let answers = talk(
        child,
        &[
            list(),
            // Asked for directly, as a compromised or buggy client would.
            call(
                2,
                "Write",
                json!({"file_path": written.display().to_string(), "content": "x"}),
            ),
            call(3, "Bash", touch(&marker)),
            call(
                4,
                "Read",
                json!({"file_path": cwd.join("absent").display().to_string()}),
            ),
        ],
    );
    assert_eq!(names(&answers[0]), ["Glob", "Read"]);
    assert!(refused(&answers[1]), "Write ran: {}", answers[1]);
    assert!(refused(&answers[2]), "Bash ran: {}", answers[2]);
    assert!(!written.exists(), "the refused Write wrote");
    assert!(!marker.exists(), "the refused Bash ran");
    // An allowed tool still answers as a tool (an error result, not a refusal).
    assert!(answers[3]["result"].is_object(), "{}", answers[3]);
}

#[test]
fn without_tools_the_trusted_harness_still_gets_every_tool() {
    let (_temp, cwd) = dir();
    let child = binary(&["--trust-harness", "--cwd", cwd.to_str().unwrap()])
        .spawn()
        .unwrap();
    let tools = names(&talk(child, &[list()])[0]);
    for expected in [
        "Read",
        "Write",
        "Edit",
        "Glob",
        "Grep",
        "NotebookEdit",
        "WebFetch",
    ] {
        assert!(
            tools.contains(&expected.to_owned()),
            "{expected}: {tools:?}"
        );
    }
    #[cfg(unix)]
    for expected in ["Bash", "Monitor", "TaskStop"] {
        assert!(
            tools.contains(&expected.to_owned()),
            "{expected}: {tools:?}"
        );
    }
}

#[test]
fn an_unknown_name_in_tools_refuses_to_start_and_lists_the_valid_names() {
    // `read` is not `Read`: names are canonical, case included.
    for bad in ["Read,Rm", "read"] {
        let output = binary(&["--trust-harness", "--tools", bad])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{bad}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let unknown = if bad == "read" { "read" } else { "Rm" };
        assert!(stderr.contains(unknown), "{stderr}");
        for valid in ["Read", "Write", "Bash", "WebFetch", "WebSearch", "TaskStop"] {
            assert!(stderr.contains(valid), "{valid} not listed: {stderr}");
        }
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn a_canonical_tool_this_platform_lacks_is_accepted_and_simply_absent() {
    // The harness computes `--tools` from a policy, not from the platform: `Bash` must not stop
    // the server from starting where there is no shell tool (Windows).
    let (_temp, cwd) = dir();
    let child = binary(&[
        "--trust-harness",
        "--cwd",
        cwd.to_str().unwrap(),
        "--tools",
        "Read,Bash",
    ])
    .spawn()
    .unwrap();
    let tools = names(&talk(child, &[list()])[0]);
    if cfg!(unix) {
        assert_eq!(tools, ["Bash", "Read"]);
    } else {
        assert_eq!(tools, ["Read"]);
    }
}

#[test]
fn repeated_tools_options_narrow_each_other() {
    let (_temp, cwd) = dir();
    let child = binary(&[
        "--trust-harness",
        "--cwd",
        cwd.to_str().unwrap(),
        "--tools",
        "Read,Glob,Grep",
        "--tools",
        "Grep,Read,Write",
    ])
    .spawn()
    .unwrap();
    assert_eq!(names(&talk(child, &[list()])[0]), ["Grep", "Read"]);
}

#[test]
fn an_empty_tools_list_serves_no_tool() {
    let (_temp, cwd) = dir();
    let child = binary(&[
        "--trust-harness",
        "--cwd",
        cwd.to_str().unwrap(),
        "--tools",
        "",
    ])
    .spawn()
    .unwrap();
    let answers = talk(child, &[list(), call(2, "Read", json!({"file_path": "x"}))]);
    assert!(names(&answers[0]).is_empty(), "{}", answers[0]);
    assert!(refused(&answers[1]), "{}", answers[1]);
}

#[test]
fn tools_is_an_intersection_with_a_signed_profile_never_wider() {
    let (_temp, cwd) = dir();
    let written = cwd.join("written.txt");
    let child = binary(&["--cwd", cwd.to_str().unwrap(), "--tools", "Read,Glob"])
        .env("NEXUS_TOOLS_KEY", KEY)
        .env("NEXUS_TOOLS_PROFILE", profile_token(&["Read", "Write"]))
        .spawn()
        .unwrap();
    let answers = talk(
        child,
        &[
            list(),
            // In the profile, not in --tools.
            call(
                2,
                "Write",
                json!({"file_path": written.display().to_string(), "content": "x"}),
            ),
            // In --tools, not in the profile.
            call(3, "Glob", json!({"pattern": "*"})),
        ],
    );
    assert_eq!(names(&answers[0]), ["Read"]);
    assert!(refused(&answers[1]), "{}", answers[1]);
    assert!(refused(&answers[2]), "{}", answers[2]);
    assert!(!written.exists());
}

#[test]
fn tools_also_bounds_unrestricted() {
    let (_temp, cwd) = dir();
    let child = binary(&[
        "--unrestricted",
        "--cwd",
        cwd.to_str().unwrap(),
        "--tools",
        "Read",
    ])
    .spawn()
    .unwrap();
    assert_eq!(names(&talk(child, &[list()])[0]), ["Read"]);
}

// ---------------------------------------------------------------------------
// The native harness passes the bound its session policy exposes
// ---------------------------------------------------------------------------

fn spec(cwd: &Path, policy: ToolPolicy) -> SessionSpec {
    let mut spec = SessionSpec::new(cwd);
    spec.policy = policy;
    spec
}

/// Launches the server exactly as the harness would for `spec`.
fn launch_as_the_harness(spec: &SessionSpec) -> Child {
    let McpServerSpec::Stdio { command, args, env } =
        DefaultTools::new(NEXUS_TOOLS).server_for(spec, true)
    else {
        panic!("the default tools are a stdio server");
    };
    let mut process = Command::new(command);
    process
        .args(&args)
        .envs(&env)
        .env_remove("NEXUS_TOOLS_PROFILE")
        .env_remove("NEXUS_TOOLS_KEY")
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    process.spawn().unwrap()
}

#[test]
fn a_session_whose_policy_does_not_expose_bash_cannot_run_bash_even_by_a_direct_call() {
    let (_temp, cwd) = dir();
    let marker = cwd.join("marker");
    let written = cwd.join("written.txt");
    let policy = ToolPolicy::from_patterns(PolicyMode::Ask, &["Read", "Grep"], &[]).unwrap();
    let answers = talk(
        launch_as_the_harness(&spec(&cwd, policy)),
        &[
            list(),
            call(2, "Bash", touch(&marker)),
            call(
                3,
                "Write",
                json!({"file_path": written.display().to_string(), "content": "x"}),
            ),
        ],
    );
    assert_eq!(names(&answers[0]), ["Grep", "Read"]);
    assert!(refused(&answers[1]), "Bash ran: {}", answers[1]);
    assert!(refused(&answers[2]), "Write ran: {}", answers[2]);
    assert!(!marker.exists(), "the Bash command ran");
    assert!(!written.exists(), "the Write ran");
}

#[test]
fn a_deny_without_argument_removes_the_tool_from_the_process_too() {
    let (_temp, cwd) = dir();
    let marker = cwd.join("marker");
    let policy = ToolPolicy::from_patterns::<&str>(PolicyMode::Ask, &[], &["Bash"]).unwrap();
    let answers = talk(
        launch_as_the_harness(&spec(&cwd, policy)),
        &[list(), call(2, "Bash", touch(&marker))],
    );
    let tools = names(&answers[0]);
    assert!(!tools.contains(&"Bash".to_owned()), "{tools:?}");
    assert!(tools.contains(&"Write".to_owned()), "{tools:?}");
    assert!(refused(&answers[1]), "{}", answers[1]);
    assert!(!marker.exists());
}

#[test]
fn two_native_sessions_in_two_directories_cannot_read_each_others_files() {
    let (_ta, a) = dir();
    let (_tb, b) = dir();
    std::fs::write(a.join("a-secret.txt"), "alpha-secret\n").unwrap();
    std::fs::write(b.join("b-secret.txt"), "bravo-secret\n").unwrap();
    let open = || ToolPolicy::new(PolicyMode::Ask);
    let read =
        |id: u64, path: PathBuf| call(id, "Read", json!({"file_path": path.display().to_string()}));
    let write = |id: u64, path: PathBuf| {
        call(
            id,
            "Write",
            json!({"file_path": path.display().to_string(), "content": "intruder"}),
        )
    };
    let from_a = talk(
        launch_as_the_harness(&spec(&a, open())),
        &[
            read(1, b.join("b-secret.txt")),
            read(2, a.join("a-secret.txt")),
            write(3, b.join("planted.txt")),
        ],
    );
    let from_b = talk(
        launch_as_the_harness(&spec(&b, open())),
        &[
            read(1, a.join("a-secret.txt")),
            read(2, b.join("b-secret.txt")),
            write(3, a.join("planted.txt")),
        ],
    );
    for (who, answers, other_secret, own_secret) in [
        ("A", &from_a, "bravo-secret", "alpha-secret"),
        ("B", &from_b, "alpha-secret", "bravo-secret"),
    ] {
        let (text, is_error) = result_text(&answers[0]);
        assert!(is_error, "{who} read the other session's file: {text}");
        assert!(!text.contains(other_secret), "{who}: {text}");
        let (text, is_error) = result_text(&answers[1]);
        assert!(
            !is_error && text.contains(own_secret),
            "{who} own file: {text}"
        );
        let (text, is_error) = result_text(&answers[2]);
        assert!(is_error, "{who} wrote into the other session: {text}");
    }
    assert!(!a.join("planted.txt").exists() && !b.join("planted.txt").exists());
    assert_eq!(text_of(&a.join("a-secret.txt")), "alpha-secret\n");
}

#[test]
fn the_harness_catalog_is_the_servers_own() {
    use nexus_claude::providers::native::NEXUS_TOOLS_CATALOG;
    let catalog: Vec<&str> = NEXUS_TOOLS_CATALOG.iter().map(|(name, _)| *name).collect();
    let mut sorted = catalog.clone();
    sorted.sort_unstable();
    let mut canonical = nexus_tools::registry::CANONICAL_TOOLS.to_vec();
    canonical.sort_unstable();
    assert_eq!(
        sorted, canonical,
        "the harness and the server disagree on the tool names"
    );
    // Every tool this platform serves is in the catalog with the same read-only annotation (the
    // harness uses it to hide edits in plan mode before the server is even started).
    let (_temp, cwd) = dir();
    let child = binary(&[
        "--trust-harness",
        "--cwd",
        cwd.to_str().unwrap(),
        "--search-engine",
        "searxng:http://127.0.0.1:9/searxng",
    ])
    .spawn()
    .unwrap();
    let answers = talk(child, &[list()]);
    let served = answers[0]["result"]["tools"].as_array().unwrap();
    for tool in served {
        let name = tool["name"].as_str().unwrap();
        if !nexus_tools::registry::CANONICAL_TOOLS.contains(&name) {
            continue; // a test tool of the test-tools feature
        }
        let read_only = tool["annotations"]["readOnlyHint"] == true;
        let entry = NEXUS_TOOLS_CATALOG
            .iter()
            .find(|(n, _)| *n == name)
            .unwrap_or_else(|| panic!("{name} is served but not in the harness catalog"));
        assert_eq!(entry.1, read_only, "{name}: read-only annotation differs");
    }
}

// ---------------------------------------------------------------------------
// What one process per session costs (measured, not asserted)
// ---------------------------------------------------------------------------

/// Start-up to the first `initialize` answer (mean of 20 launches) and resident memory once
/// initialised, of a session server launched as the harness launches it. Run in release:
/// `cargo test -p nexus-tools --release --test session_bound -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement, run on demand"]
fn measure_the_cost_of_one_process_per_session() {
    let (_temp, cwd) = dir();
    let spec = spec(&cwd, ToolPolicy::new(PolicyMode::Ask));
    let runs = 20;
    // The first exec of the binary from a fresh process pays an operating-system cost (on macOS
    // about 300 ms, even for `--version`): reported apart, so it is not taken for start-up.
    let cold = Instant::now();
    Command::new(NEXUS_TOOLS).arg("--version").output().unwrap();
    println!(
        "first exec of the binary (--version, no runtime): {:?}",
        cold.elapsed()
    );
    let mut startup = Vec::new();
    let mut rss_kib = Vec::new();
    for _ in 0..runs {
        let started = Instant::now();
        let mut child = launch_as_the_harness(&spec);
        let mut stdin = child.stdin.take().unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-03-26"}})
        )
        .unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        startup.push(started.elapsed());
        assert!(line.contains("\"protocolVersion\""), "{line}");
        // Idle after initialize: let the runtime settle, then read the resident set.
        std::thread::sleep(Duration::from_millis(200));
        if cfg!(unix) {
            let out = Command::new("ps")
                .args(["-o", "rss=", "-p", &child.id().to_string()])
                .output()
                .unwrap();
            if let Ok(kib) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>() {
                rss_kib.push(kib);
            }
        }
        drop(stdin);
        child.wait().unwrap();
    }
    let mean = startup.iter().sum::<Duration>() / runs;
    let (min, max) = (startup.iter().min().unwrap(), startup.iter().max().unwrap());
    let mut sorted = startup.clone();
    sorted.sort();
    println!(
        "startup to initialize answer over {runs} launches: mean {mean:?}, median {:?}, min {min:?}, max {max:?} (first launch {:?})",
        sorted[sorted.len() / 2],
        startup[0]
    );
    if !rss_kib.is_empty() {
        let mean_rss = rss_kib.iter().sum::<u64>() / rss_kib.len() as u64;
        println!(
            "idle RSS after initialize: mean {} KiB, min {} KiB, max {} KiB",
            mean_rss,
            rss_kib.iter().min().unwrap(),
            rss_kib.iter().max().unwrap()
        );
    }
}
