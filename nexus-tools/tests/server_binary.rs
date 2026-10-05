//! The real `nexus-tools` binary, over stdio: what an MCP client actually launches (N18).
//!
//! Built with `--features test-tools` (the production binary has no test tools).

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
