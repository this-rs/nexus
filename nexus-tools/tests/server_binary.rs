//! The real `nexus-tools` binary, over stdio: what an MCP client actually launches (N18).
//!
//! Built with `--features test-tools` (the production binary has no test tools).
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use nexus_tools::{Claims, SigningKey, issue};
use serde_json::{Value, json};

const KEY: &str = "0123456789abcdef0123456789abcdef-test-key";

fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nexus-tools"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
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
            sid: "bin".into(),
            tools: tools.iter().map(|t| (*t).to_owned()).collect(),
            exp,
        },
    )
}

/// Sends the requests, reads `expected` answers (they may come in any order: calls run
/// concurrently), then closes stdin. Closing early would abort calls still in flight, by design.
fn talk(mut child: Child, requests: &[Value], expected: usize) -> (Vec<Value>, String) {
    let mut stdin = child.stdin.take().unwrap();
    for request in requests {
        writeln!(stdin, "{request}").unwrap();
    }
    let mut answers: Vec<Value> = Vec::new();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    while answers.len() < expected {
        let mut line = String::new();
        assert!(
            stdout.read_line(&mut line).unwrap() > 0,
            "stdout closed early: {answers:?}"
        );
        answers.push(serde_json::from_str(&line).expect("stdout is JSON lines only"));
    }
    drop(stdin); // EOF ends the session
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut stdout, &mut rest).unwrap();
    assert!(rest.is_empty(), "unexpected extra output: {rest}");
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    child.wait().unwrap();
    answers.sort_by_key(|a| a["id"].as_u64());
    (answers, stderr)
}

#[test]
fn serves_the_token_profile_over_stdio_and_logs_no_argument() {
    let child = binary()
        .env("NEXUS_TOOLS_KEY", KEY)
        .env("NEXUS_TOOLS_PROFILE", profile_token(&["echo"]))
        .env("NEXUS_TOOLS_LOG", "info")
        .spawn()
        .unwrap();
    let sentinel = "sk-sentinel-binary-7c1d";
    let (answers, stderr) = talk(
        child,
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"text":sentinel}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"write","arguments":{"text":"x"}}}),
        ],
        4,
    );
    assert_eq!(answers.len(), 4, "{answers:?}");
    let names: Vec<&str> = answers[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["echo"]);
    assert_eq!(
        answers[2]["result"]["content"][0]["text"],
        format!("echo: {sentinel}")
    );
    assert_eq!(answers[3]["error"]["code"], -32602, "outside the profile");
    assert!(
        !stderr.contains(sentinel),
        "stderr holds the argument: {stderr}"
    );
    assert!(!stderr.contains(KEY), "stderr holds the key: {stderr}");
}

#[test]
fn stdio_without_a_profile_or_the_explicit_flag_refuses_to_start() {
    let output = binary().output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("NEXUS_TOOLS_PROFILE") && stderr.contains("--unrestricted"),
        "{stderr}"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn unrestricted_must_be_asked_for_and_then_lists_everything() {
    let child = binary().arg("--unrestricted").spawn().unwrap();
    let (answers, _) = talk(
        child,
        &[json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})],
        1,
    );
    let names: Vec<&str> = answers[0]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"echo") && names.contains(&"write"),
        "{names:?}"
    );
}

#[test]
fn a_bad_or_expired_profile_token_refuses_to_start() {
    let output = binary()
        .env("NEXUS_TOOLS_KEY", KEY)
        .env("NEXUS_TOOLS_PROFILE", "v1.garbage.garbage")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("garbage.garbage"),
        "the token is not echoed: {stderr}"
    );
}

#[test]
fn listening_without_a_long_enough_key_refuses_to_start() {
    for env in [None, Some("short")] {
        let mut command = binary();
        command.args(["--listen", "127.0.0.1:0"]);
        if let Some(value) = env {
            command.env("NEXUS_TOOLS_KEY", value);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success(), "{env:?} started");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("NEXUS_TOOLS_KEY"), "{stderr}");
        assert!(!stderr.contains("short"), "the key is not echoed: {stderr}");
    }
}

// ---------------------------------------------------------------------------
// WebSearch is configured, never assumed (N22)
// ---------------------------------------------------------------------------

fn names(answer: &Value) -> Vec<String> {
    answer["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

/// Starts the `fake_search` binary and returns it with its address.
fn fake_search(key: &str) -> (Child, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fake_search"))
        .args(["127.0.0.1:0", key])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    let addr = line
        .trim()
        .strip_prefix("listening on ")
        .expect("the fake says where it listens")
        .to_owned();
    (child, addr)
}

#[test]
fn web_search_is_absent_until_an_engine_is_configured() {
    let child = binary().arg("--unrestricted").spawn().unwrap();
    let (answers, _) = talk(
        child,
        &[json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})],
        1,
    );
    let tools = names(&answers[0]);
    assert!(!tools.contains(&"WebSearch".to_owned()), "{tools:?}");
    for expected in [
        "Read",
        "Write",
        "Edit",
        "Glob",
        "Grep",
        "NotebookEdit",
        "Bash",
        "TaskStop",
        "Monitor",
        "WebFetch",
    ] {
        assert!(
            tools.contains(&expected.to_owned()),
            "{expected} missing from {tools:?}"
        );
    }
}

#[test]
fn a_keyed_engine_works_end_to_end_and_its_key_stays_in_the_environment() {
    let key = "sk-binary-search-key-91e7";
    let (mut fake, addr) = fake_search(key);
    let child = binary()
        .args([
            "--unrestricted",
            "--search-engine",
            "brave:MY_SEARCH_KEY",
            "--search-allow-private",
        ])
        .args(["--brave-endpoint", &format!("http://{addr}/brave")])
        .env("MY_SEARCH_KEY", key)
        .spawn()
        .unwrap();
    let (answers, stderr) = talk(
        child,
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"WebSearch","arguments":{"query":"end to end search","blocked_domains":["blocked.test"]}}}),
        ],
        2,
    );
    fake.kill().ok();
    assert!(names(&answers[0]).contains(&"WebSearch".to_owned()));
    let text = answers[1]["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(answers[1]["result"]["isError"], false, "{text}");
    assert!(
        text.contains("(via brave)") && text.contains("Result 1 for end to end search"),
        "{text}"
    );
    assert!(!text.contains("blocked.test"), "{text}");
    assert!(
        !text.contains(key) && !stderr.contains(key),
        "the key leaked:\n{text}\n{stderr}"
    );
}

#[test]
fn a_key_pasted_where_a_variable_name_belongs_is_refused_without_being_echoed() {
    let pasted = "sk-live-abcdef-0123456789";
    let output = binary()
        .args([
            "--unrestricted",
            "--search-engine",
            &format!("brave:{pasted}"),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("NAME of the environment variable"),
        "{stderr}"
    );
    assert!(
        !stderr.contains(pasted),
        "the pasted key was echoed: {stderr}"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn the_html_engine_needs_its_name_and_says_what_it_is() {
    let child = binary()
        .args(["--unrestricted", "--search-engine", "html"])
        .spawn()
        .unwrap();
    let (answers, stderr) = talk(
        child,
        &[json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})],
        1,
    );
    assert!(names(&answers[0]).contains(&"WebSearch".to_owned()));
    assert!(
        stderr.contains("fragile"),
        "the operator is warned: {stderr}"
    );
}
