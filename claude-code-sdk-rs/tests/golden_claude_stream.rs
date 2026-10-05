//! Goldens of the existing Claude Code message stream (N1).
//!
//! Each scenario scripts `fake_claude`, drives a real `SubprocessTransport`, and
//! records the `Message` sequence the SDK hands back as canonical JSON in
//! `tests/golden/<scenario>.json`. The façade work that follows (N5…) must not
//! change what an existing consumer sees: any drift in `message_parser` or in the
//! `Message` serialisation shows up as a diff against these files.
//!
//! Canonical form: object keys sorted, ids (`msg_*`, `toolu_*`, session ids)
//! replaced by placeholders numbered in order of first appearance, absolute paths
//! by `<PATH>`, ISO-8601 timestamps by `<TS>`, durations by `<MS>`. Everything a
//! real recording would vary from run to run is therefore neutral, and the files
//! stay readable in review.
//!
//! Regenerate deliberately with `UPDATE_GOLDEN=1 cargo test --test
//! golden_claude_stream`, then read the diff: an unexpected change is a bug in the
//! parser, not in the golden.

mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use nexus_claude::{ClaudeCodeOptions, Message};
use serde_json::{Map, Value, json};
use support::*;

const WAIT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// canonicalisation
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Normalizer {
    ids: BTreeMap<String, String>,
}

impl Normalizer {
    fn placeholder(&mut self, kind: &str, raw: &str) -> String {
        let next = self.ids.values().filter(|v| v.starts_with(kind)).count() + 1;
        self.ids
            .entry(raw.to_string())
            .or_insert_with(|| format!("{kind}{next}>"))
            .clone()
    }

    fn string(&mut self, key: Option<&str>, s: &str) -> String {
        const ID_PREFIXES: [(&str, &str); 3] =
            [("msg_", "<MSG"), ("toolu_", "<TOOL"), ("req_", "<REQ")];
        for (prefix, kind) in ID_PREFIXES {
            if s.starts_with(prefix) {
                return self.placeholder(kind, s);
            }
        }
        if matches!(key, Some("session_id" | "sessionId")) {
            return self.placeholder("<SESSION", s);
        }
        if is_timestamp(s) {
            return "<TS>".to_string();
        }
        if s.starts_with('/') && s.len() > 1 && !s.contains(' ') && s[1..].contains('/') {
            return "<PATH>".to_string();
        }
        s.to_string()
    }

    fn value(&mut self, key: Option<&str>, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.string(key, s)),
            Value::Array(items) => Value::Array(items.iter().map(|i| self.value(key, i)).collect()),
            Value::Object(map) => {
                let mut sorted: Vec<(&String, &Value)> = map.iter().collect();
                sorted.sort_by(|a, b| a.0.cmp(b.0));
                let mut out = Map::new();
                for (k, val) in sorted {
                    let norm = if matches!(k.as_str(), "duration_ms" | "duration_api_ms") {
                        json!("<MS>")
                    } else {
                        self.value(Some(k), val)
                    };
                    out.insert(k.clone(), norm);
                }
                Value::Object(out)
            },
            other => other.clone(),
        }
    }
}

/// `2026-10-05T12:34:56Z`, with optional fraction and offset.
fn is_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 20
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[..4].iter().all(u8::is_ascii_digit)
}

fn canonical(messages: &[Message]) -> Value {
    let mut n = Normalizer::default();
    let items: Vec<Value> = messages
        .iter()
        .map(|m| {
            let raw = serde_json::to_value(m).expect("Message serialises");
            n.value(None, &raw)
        })
        .collect();
    json!({ "messages": items })
}

// ---------------------------------------------------------------------------
// golden files
// ---------------------------------------------------------------------------

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(format!("{name}.json"))
}

fn check_golden(name: &str, actual: &Value) {
    let rendered = format!("{}\n", serde_json::to_string_pretty(actual).unwrap());
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    // A Windows checkout may turn the golden files into CRLF (core.autocrlf): compare the content.
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| {
            panic!(
                "missing golden {}; record it with UPDATE_GOLDEN=1",
                path.display()
            )
        })
        .replace("\r\n", "\n");
    assert_eq!(
        expected, rendered,
        "golden `{name}` drifted (UPDATE_GOLDEN=1 to re-record after reviewing the diff)"
    );
}

async fn record(name: &str, transcript: Transcript, options: Option<ClaudeCodeOptions>) {
    let fake = transcript.build();
    let mut transport = match options {
        Some(o) => fake.transport_with(o),
        None => fake.transport(),
    };
    transport.connect().await.expect("connect");
    let stream = start_turn(&mut transport, "go").await;
    let messages = collect_until_result(stream, WAIT).await;
    transport.disconnect().await.ok();
    assert!(
        matches!(messages.last(), Some(Message::Result { .. })),
        "scenario `{name}` must end on a result, got {} messages",
        messages.len()
    );
    check_golden(name, &canonical(&messages));
}

// ---------------------------------------------------------------------------
// the eight scenarios
// ---------------------------------------------------------------------------

#[tokio::test]
async fn golden_01_simple_text() {
    record(
        "01_simple_text",
        Transcript::new()
            .await_stdin()
            .init("sess-aaa")
            .assistant_text("Hello.")
            .result_ok("Hello."),
        None,
    )
    .await;
}

#[tokio::test]
async fn golden_02_thinking_then_text() {
    record(
        "02_thinking_then_text",
        Transcript::new()
            .await_stdin()
            .init("sess-aaa")
            .assistant_thinking("Let me think.", "sig-abc")
            .assistant_text("Done thinking.")
            .result_ok("Done thinking."),
        None,
    )
    .await;
}

#[tokio::test]
async fn golden_03_tool_use_and_result() {
    record(
        "03_tool_use_and_result",
        Transcript::new()
            .await_stdin()
            .init("sess-aaa")
            .tool_use("toolu_01A", "Bash", json!({"command": "ls /tmp/work"}))
            .tool_result("toolu_01A", "a.txt\nb.txt", false)
            .assistant_text("Two files.")
            .result_ok("Two files."),
        None,
    )
    .await;
}

#[tokio::test]
async fn golden_04_tool_error() {
    record(
        "04_tool_error",
        Transcript::new()
            .await_stdin()
            .init("sess-aaa")
            .tool_use(
                "toolu_02B",
                "Read",
                json!({"file_path": "/etc/missing/file"}),
            )
            .tool_result("toolu_02B", "No such file", true)
            .assistant_text("It does not exist.")
            .result_ok("It does not exist."),
        None,
    )
    .await;
}

#[tokio::test]
async fn golden_05_partial_message_stream() {
    let options = ClaudeCodeOptions::builder()
        .include_partial_messages(true)
        .build();
    record(
        "05_partial_message_stream",
        Transcript::new()
            .await_stdin()
            .init("sess-aaa")
            .text_delta("Hel")
            .text_delta("lo")
            .assistant_text("Hello")
            .result_ok("Hello"),
        Some(options),
    )
    .await;
}

#[tokio::test]
async fn golden_06_system_notices() {
    record(
        "06_system_notices",
        Transcript::new()
            .await_stdin()
            .init("sess-aaa")
            .system(
                "compact_boundary",
                json!({"trigger": "auto", "pre_tokens": 1234}),
            )
            .system("status", json!({"status": "compacting"}))
            .assistant_text("Compacted.")
            .result_ok("Compacted."),
        None,
    )
    .await;
}

#[tokio::test]
async fn golden_07_subagent_sidechain() {
    let sub = |text: &str| {
        let mut v = msg::assistant_text(text);
        v["parent_tool_use_id"] = json!("toolu_03C");
        v
    };
    record(
        "07_subagent_sidechain",
        Transcript::new()
            .await_stdin()
            .init("sess-aaa")
            .tool_use(
                "toolu_03C",
                "Task",
                json!({"description": "dig", "prompt": "dig"}),
            )
            .json(sub("sub-agent working"))
            .tool_result("toolu_03C", "found it", false)
            .assistant_text("The agent found it.")
            .result_ok("The agent found it."),
        None,
    )
    .await;
}

#[tokio::test]
async fn golden_08_noise_and_failed_result() {
    record(
        "08_noise_and_failed_result",
        Transcript::new()
            .await_stdin()
            .init("sess-aaa")
            .garbage()
            .malformed_json()
            .assistant_text("Still here.")
            .result_error("boom"),
        None,
    )
    .await;
}

// ---------------------------------------------------------------------------
// control flow: permissions, interruption, process death
// ---------------------------------------------------------------------------
//
// These do not travel on the `Message` stream: a `can_use_tool` request arrives on
// the inbound control channel and the answer goes back on stdin. The golden for
// them therefore also records what the SDK wrote to the child (everything after
// the first prompt line), because that is the other half of the contract.

/// Messages plus the control-channel traffic, canonicalised with one shared
/// numbering so an id seen in a message and in a control line gets one placeholder.
fn canonical_with_control(messages: &[Message], control: &[Value], stdin: &[Value]) -> Value {
    let mut n = Normalizer::default();
    let msgs: Vec<Value> = messages
        .iter()
        .map(|m| n.value(None, &serde_json::to_value(m).expect("Message serialises")))
        .collect();
    let ctl: Vec<Value> = control.iter().map(|v| n.value(None, v)).collect();
    let sent: Vec<Value> = stdin.iter().map(|v| n.value(None, v)).collect();
    json!({
        "messages": msgs,
        "control_requests_from_cli": ctl,
        "lines_sent_to_cli": sent,
    })
}

async fn permission_scenario(name: &str, decision: Value, final_text: &str) {
    let fake = Transcript::new()
        .await_stdin()
        .init("sess-aaa")
        .permission_request("req_perm1", "Bash", json!({"command": "rm -rf build"}))
        .await_stdin_containing("control_response")
        .assistant_text(final_text)
        .result_ok(final_text)
        .wait_eof()
        .build();
    let mut transport = fake.transport();
    transport.connect().await.expect("connect");
    let mut control_rx = transport.take_sdk_control_receiver().expect("control rx");
    let stream = start_turn(&mut transport, "clean").await;
    let request = tokio::time::timeout(WAIT, control_rx.recv())
        .await
        .expect("permission request arrives")
        .expect("control channel open");
    transport
        .send_sdk_control_response(json!({
            "subtype": "success",
            "request_id": "req_perm1",
            "response": decision,
        }))
        .await
        .expect("answer");
    let messages = collect_until_result(stream, WAIT).await;
    let sent = fake.wait_for_stdin_lines(2, WAIT).await;
    transport.disconnect().await.ok();
    let sent: Vec<Value> = sent
        .iter()
        .skip(1) // the prompt itself; its shape is covered by the transport tests
        .map(|l| serde_json::from_str(l).expect("stdin line is JSON"))
        .collect();
    check_golden(name, &canonical_with_control(&messages, &[request], &sent));
}

#[tokio::test]
async fn golden_09_permission_granted() {
    permission_scenario(
        "09_permission_granted",
        json!({"behavior": "allow", "updatedInput": {"command": "rm -rf build"}}),
        "Cleaned.",
    )
    .await;
}

#[tokio::test]
async fn golden_10_permission_denied() {
    permission_scenario(
        "10_permission_denied",
        json!({"behavior": "deny", "message": "not allowed"}),
        "I was refused.",
    )
    .await;
}

#[tokio::test]
async fn golden_11_interrupt() {
    use nexus_claude::ControlRequest;
    let fake = Transcript::new()
        .await_stdin()
        .init("sess-aaa")
        .assistant_text("Working on it")
        .reply_control_success("interrupt")
        .result_error("interrupted")
        .wait_eof()
        .build();
    let mut transport = fake.transport();
    transport.connect().await.expect("connect");
    let stream = start_turn(&mut transport, "long job").await;
    transport
        .send_control_request(ControlRequest::Interrupt {
            request_id: "req_int1".into(),
        })
        .await
        .expect("send interrupt");
    let ack = tokio::time::timeout(WAIT, transport.receive_control_response())
        .await
        .expect("ack in time")
        .expect("an ack");
    let messages = collect_until_result(stream, WAIT).await;
    let sent = fake.wait_for_stdin_lines(2, WAIT).await;
    transport.disconnect().await.ok();
    let sent: Vec<Value> = sent
        .iter()
        .skip(1)
        .map(|l| serde_json::from_str(l).expect("stdin line is JSON"))
        .collect();
    let ack = serde_json::to_value(&ack).expect("ack serialises");
    check_golden(
        "11_interrupt",
        &canonical_with_control(&messages, &[ack], &sent),
    );
}

#[tokio::test]
async fn golden_12_process_death_mid_turn() {
    let fake = Transcript::new()
        .await_stdin()
        .init("sess-aaa")
        .assistant_text("half an answer")
        .exit_with(9)
        .build();
    let mut transport = fake.transport();
    transport.connect().await.expect("connect");
    let mut stream = start_turn(&mut transport, "die").await;
    let mut messages = Vec::new();
    let _ = tokio::time::timeout(WAIT, async {
        while let Some(item) = futures::StreamExt::next(&mut stream).await {
            if let Ok(m) = item {
                messages.push(m);
            }
        }
    })
    .await;
    transport.disconnect().await.ok();
    assert!(
        !messages.iter().any(|m| matches!(m, Message::Result { .. })),
        "a dead CLI never produces a result: {messages:?}"
    );
    check_golden("12_process_death_mid_turn", &canonical(&messages));
}

// ---------------------------------------------------------------------------
// the normaliser itself
// ---------------------------------------------------------------------------

#[test]
fn normaliser_numbers_ids_in_order_and_neutralises_volatile_fields() {
    let mut n = Normalizer::default();
    let v = n.value(
        None,
        &json!({
            "b": "msg_x", "a": "msg_y", "again": "msg_x",
            "session_id": "s1", "at": "2026-10-05T12:00:00.123Z",
            "path": "/Users/me/p/file.rs", "duration_ms": 41, "plain": "hello /world",
        }),
    );
    // Keys are visited in sorted order (a, again, b), so numbering follows that.
    assert_eq!(v["a"], "<MSG1>");
    assert_eq!(v["again"], "<MSG2>");
    assert_eq!(v["b"], "<MSG2>");
    assert_eq!(v["session_id"], "<SESSION1>");
    assert_eq!(v["at"], "<TS>");
    assert_eq!(v["path"], "<PATH>");
    assert_eq!(v["duration_ms"], "<MS>");
    assert_eq!(v["plain"], "hello /world");
}
