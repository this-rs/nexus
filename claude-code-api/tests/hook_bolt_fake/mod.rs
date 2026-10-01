//! A fake Bolt 4.1 server, and the hook fixtures that go with it, shared by the
//! two `Neo4jHookCallback` test binaries (`neo4j_hook_callback_bolt.rs` and
//! `neo4j_hook_callback_logs.rs`).
//!
//! ## Why there is a Bolt server here
//!
//! `Neo4jHookCallback::new` only takes an `Arc<neo4rs::Graph>`, and `neo4rs`
//! exposes no injectable connection trait: `Graph::run` speaks the binary
//! **Bolt** protocol over TCP, so `wiremock` (HTTP) cannot stand in for it. What
//! works is speaking Bolt back, the way `tests/neo4j_permission_bolt.rs` does for
//! the permission provider — `neo4rs` 0.8 only supports Bolt 4.0/4.1, whose
//! framing is "`u16` chunk length, payload, `00 00`" and whose payloads are
//! PackStream structs, and `Graph::new` is lazy, so the listener has all the time
//! it needs to come up.
//!
//! [`FakeBolt`] is that listener: loopback, ephemeral port, in-process, no
//! service and no network egress. It is deliberately a *recording* fake — it
//! decodes the Cypher and the parameter map — because the callback returns
//! `Ok(())` whatever happens, so the only way to assert on it is to read what it
//! wrote.
//!
//! The encoder and decoder are the ones from `tests/neo4j_permission_bolt.rs`,
//! reduced to the markers these queries use. They live here rather than in
//! `tests/support/` because that module belongs to the harness; consolidating the
//! two copies into `support::bolt` is a job for its owner.
//!
//! ## The honest limit
//!
//! The fake checks the *text* of a statement and its parameters, not Cypher
//! semantics. It does not prove that `sum(COALESCE(t.duration_ms, 0))` adds up
//! what `handle_stop` believes, only that the statement asking for it is the one
//! that goes out. That half needs a real Neo4j.

// Each of the two binaries uses a different part of this module.
#![allow(dead_code, unused_imports)]

// The gateway harness, so a test here can build a `MeilisearchClient` over a
// `wiremock` server. `#[path]` because this module is itself a directory module.
#[path = "../support/mod.rs"]
pub mod support;

use claude_code_api::core::hooks::{Neo4jHookCallback, Neo4jHookCallbackConfig};
use neo4rs::Graph;
use nexus_claude::{
    HookCallback, HookContext, HookInput, HookJSONOutput, PostToolUseHookInput,
    PreCompactHookInput, PreToolUseHookInput, StopHookInput, SubagentStopHookInput,
    SyncHookJSONOutput, UserPromptSubmitHookInput,
};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// PackStream encoding (only the markers `neo4rs` 0.8 can parse)
// ---------------------------------------------------------------------------

pub fn pack_string(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() + 3);
    let n = b.len();
    if n < 16 {
        out.push(0x80 | n as u8);
    } else if n < 256 {
        out.push(0xD0);
        out.push(n as u8);
    } else {
        out.push(0xD1);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    }
    out.extend_from_slice(b);
    out
}

pub fn pack_bool(v: bool) -> Vec<u8> {
    vec![if v { 0xC3 } else { 0xC2 }]
}

pub fn pack_list(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    if items.len() < 16 {
        out.push(0x90 | items.len() as u8);
    } else {
        out.push(0xD4);
        out.push(items.len() as u8);
    }
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

pub fn pack_map(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    if entries.len() < 16 {
        out.push(0xA0 | entries.len() as u8);
    } else {
        out.push(0xD8);
        out.push(entries.len() as u8);
    }
    for (key, value) in entries {
        out.extend_from_slice(&pack_string(key));
        out.extend_from_slice(value);
    }
    out
}

pub fn pack_struct(signature: u8, fields: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![0xB0 | fields.len() as u8, signature];
    for field in fields {
        out.extend_from_slice(field);
    }
    out
}

pub fn msg_success(metadata: &[(&str, Vec<u8>)]) -> Vec<u8> {
    pack_struct(0x70, &[pack_map(metadata)])
}

pub fn msg_failure(code: &str, message: &str) -> Vec<u8> {
    pack_struct(
        0x7F,
        &[pack_map(&[
            ("code", pack_string(code)),
            ("message", pack_string(message)),
        ])],
    )
}

// ---------------------------------------------------------------------------
// PackStream decoding, so a test can assert on the Cypher *and* its parameters
// ---------------------------------------------------------------------------

pub fn take(input: &mut &[u8], n: usize) -> Vec<u8> {
    let (head, tail) = input.split_at(n);
    *input = tail;
    head.to_vec()
}

pub fn take_usize(input: &mut &[u8], width: usize) -> usize {
    take(input, width)
        .into_iter()
        .fold(0usize, |acc, b| (acc << 8) | b as usize)
}

pub fn take_i64(input: &mut &[u8], width: usize) -> i64 {
    let bytes = take(input, width);
    let mut value = bytes[0] as i8 as i64;
    for b in &bytes[1..] {
        value = (value << 8) | *b as i64;
    }
    value
}

pub fn unpack_string(input: &mut &[u8], n: usize) -> String {
    String::from_utf8(take(input, n)).expect("packstream string is utf-8")
}

/// Decode one PackStream value into JSON, so assertions read like the Cypher
/// parameter map they describe.
pub fn unpack(input: &mut &[u8]) -> Value {
    let marker = take(input, 1)[0];
    match marker {
        0x00..=0x7F => Value::from(marker as i64),
        0xF0..=0xFF => Value::from(marker as i8 as i64),
        0x80..=0x8F => Value::from(unpack_string(input, (marker & 0x0F) as usize)),
        0xD0 => {
            let n = take_usize(input, 1);
            Value::from(unpack_string(input, n))
        },
        0xD1 => {
            let n = take_usize(input, 2);
            Value::from(unpack_string(input, n))
        },
        0xD2 => {
            let n = take_usize(input, 4);
            Value::from(unpack_string(input, n))
        },
        0x90..=0x9F => unpack_list(input, (marker & 0x0F) as usize),
        0xD4 => {
            let n = take_usize(input, 1);
            unpack_list(input, n)
        },
        0xD5 => {
            let n = take_usize(input, 2);
            unpack_list(input, n)
        },
        0xA0..=0xAF => unpack_map(input, (marker & 0x0F) as usize),
        0xD8 => {
            let n = take_usize(input, 1);
            unpack_map(input, n)
        },
        0xD9 => {
            let n = take_usize(input, 2);
            unpack_map(input, n)
        },
        0xB0..=0xBF => {
            let signature = take(input, 1)[0];
            let fields = unpack_list(input, (marker & 0x0F) as usize - 1);
            json!({ "signature": signature, "fields": fields })
        },
        0xC0 => Value::Null,
        0xC2 => Value::Bool(false),
        0xC3 => Value::Bool(true),
        0xC8 => Value::from(take_i64(input, 1)),
        0xC9 => Value::from(take_i64(input, 2)),
        0xCA => Value::from(take_i64(input, 4)),
        0xCB => Value::from(take_i64(input, 8)),
        other => panic!("fake Bolt server cannot decode marker {other:#04X}"),
    }
}

pub fn unpack_list(input: &mut &[u8], len: usize) -> Value {
    Value::Array((0..len).map(|_| unpack(input)).collect())
}

pub fn unpack_map(input: &mut &[u8], len: usize) -> Value {
    let mut map = serde_json::Map::new();
    for _ in 0..len {
        let key = unpack(input);
        let value = unpack(input);
        map.insert(
            key.as_str().expect("map key is a string").to_string(),
            value,
        );
    }
    Value::Object(map)
}

// ---------------------------------------------------------------------------
// The fake Bolt 4.1 server
// ---------------------------------------------------------------------------

/// One `RUN` the callback sent, as the server saw it.
#[derive(Debug, Clone)]
pub struct RunLog {
    pub cypher: String,
    pub params: Value,
}

#[derive(Default, Clone)]
pub struct Script {
    /// `Some((code, message))` answers every `RUN` with that `FAILURE`.
    failure: Option<(String, String)>,
}

/// A loopback Bolt 4.1 endpoint the test scripts.
pub struct FakeBolt {
    addr: SocketAddr,
    script: Arc<Mutex<Script>>,
    runs: Arc<Mutex<Vec<RunLog>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeBolt {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeBolt {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let script = Arc::new(Mutex::new(Script::default()));
        let runs = Arc::new(Mutex::new(Vec::new()));

        let task = tokio::spawn({
            let script = Arc::clone(&script);
            let runs = Arc::clone(&runs);
            async move {
                while let Ok((socket, _)) = listener.accept().await {
                    let script = Arc::clone(&script);
                    let runs = Arc::clone(&runs);
                    tokio::spawn(async move {
                        let _ = serve(socket, script, runs).await;
                    });
                }
            }
        });

        Self {
            addr,
            script,
            runs,
            task,
        }
    }

    /// A `Graph` whose connection pool dials this server. Nothing connects until
    /// the first query: `neo4rs` builds the pool lazily.
    pub async fn graph(&self) -> Arc<Graph> {
        let uri = format!("bolt://{}", self.addr);
        Arc::new(
            Graph::new(uri, "neo4j", "fake-bolt-has-no-auth")
                .await
                .expect("lazy pool"),
        )
    }

    /// Answer every `RUN` with a non-retryable `FAILURE`, so `Graph::run` gives
    /// up after one attempt instead of backing off.
    pub fn failing(&self, message: &str) -> &Self {
        self.script.lock().expect("script").failure = Some((
            "Neo.ClientError.Statement.SyntaxError".to_string(),
            message.to_string(),
        ));
        self
    }

    pub fn runs(&self) -> Vec<RunLog> {
        self.runs.lock().expect("runs").clone()
    }

    pub fn cypher(&self) -> Vec<String> {
        self.runs().into_iter().map(|r| r.cypher).collect()
    }

    /// The single `RUN` whose Cypher contains `needle`, or a panic naming what
    /// the server did see.
    pub fn run_matching(&self, needle: &str) -> RunLog {
        let runs = self.runs();
        match runs.iter().find(|r| r.cypher.contains(needle)) {
            Some(hit) => hit.clone(),
            None => panic!(
                "no RUN contained {needle:?}; the server saw {:?}",
                runs.iter().map(|r| r.cypher.as_str()).collect::<Vec<_>>()
            ),
        }
    }

    /// The parameter map of the last `RUN`, for the single-statement handlers.
    pub fn last_params(&self) -> Value {
        self.runs()
            .last()
            .expect("the callback sent at least one RUN")
            .params
            .clone()
    }
}

/// Read one chunked Bolt message, or `None` on a clean end of stream.
pub async fn read_message(socket: &mut TcpStream) -> std::io::Result<Option<Vec<u8>>> {
    let mut payload = Vec::new();
    loop {
        let mut header = [0u8; 2];
        match socket.read_exact(&mut header).await {
            Ok(_) => {},
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let len = u16::from_be_bytes(header) as usize;
        if len == 0 {
            if payload.is_empty() {
                continue; // a NOOP chunk, not the end of a message
            }
            return Ok(Some(payload));
        }
        let at = payload.len();
        payload.resize(at + len, 0);
        socket.read_exact(&mut payload[at..]).await?;
    }
}

pub async fn write_message(socket: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    socket
        .write_all(&(payload.len() as u16).to_be_bytes())
        .await?;
    socket.write_all(payload).await?;
    socket.write_all(&[0, 0]).await?;
    socket.flush().await
}

/// Decode a `RUN` message: `struct(0x10){ query, params, extra }`.
pub fn decode_run(message: &[u8]) -> RunLog {
    let mut cursor = &message[2..];
    let cypher = unpack(&mut cursor)
        .as_str()
        .expect("RUN carries the Cypher as a string")
        .to_string();
    let params = unpack(&mut cursor);
    RunLog { cypher, params }
}

pub async fn serve(
    mut socket: TcpStream,
    script: Arc<Mutex<Script>>,
    runs: Arc<Mutex<Vec<RunLog>>>,
) -> std::io::Result<()> {
    // 4 magic bytes + four 4-byte version proposals.
    let mut handshake = [0u8; 20];
    socket.read_exact(&mut handshake).await?;
    assert_eq!(&handshake[..4], &[0x60, 0x60, 0xB0, 0x17], "Bolt magic");
    socket.write_all(&[0, 0, 1, 4]).await?; // speak Bolt 4.1
    socket.flush().await?;

    loop {
        let Some(message) = read_message(&mut socket).await? else {
            return Ok(());
        };
        if message.len() < 2 {
            return Ok(());
        }
        match message[1] {
            // HELLO / RESET
            0x01 | 0x0F => {
                let reply = msg_success(&[
                    ("server", pack_string("Neo4j/4.1.0")),
                    ("connection_id", pack_string("fake-bolt-1")),
                ]);
                write_message(&mut socket, &reply).await?;
            },
            // GOODBYE
            0x02 => return Ok(()),
            // RUN
            0x10 => {
                let log = decode_run(&message);
                runs.lock().expect("runs").push(log);
                let failure = script.lock().expect("script").failure.clone();
                let reply = match failure {
                    Some((code, message)) => msg_failure(&code, &message),
                    // `Graph::run` discards the stream, so an empty column list
                    // is all the metadata it needs.
                    None => msg_success(&[("fields", pack_list(&[]))]),
                };
                write_message(&mut socket, &reply).await?;
            },
            // DISCARD / PULL
            0x2F => {
                write_message(&mut socket, &msg_success(&[])).await?;
            },
            0x3F => {
                write_message(&mut socket, &msg_success(&[("has_more", pack_bool(false))])).await?;
            },
            _ => {
                write_message(&mut socket, &msg_success(&[])).await?;
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

pub const SESSION: &str = "sess-42";

pub fn context() -> HookContext {
    HookContext { signal: None }
}

pub fn pre_tool_use(tool_name: &str, tool_input: Value) -> HookInput {
    HookInput::PreToolUse(PreToolUseHookInput {
        session_id: SESSION.to_string(),
        transcript_path: "transcript.jsonl".to_string(),
        cwd: "workdir".to_string(),
        permission_mode: None,
        tool_name: tool_name.to_string(),
        tool_input,
        agent_id: None,
        agent_type: None,
    })
}

pub fn post_tool_use(tool_name: &str, tool_input: Value, tool_response: Value) -> HookInput {
    HookInput::PostToolUse(PostToolUseHookInput {
        session_id: SESSION.to_string(),
        transcript_path: "transcript.jsonl".to_string(),
        cwd: "workdir".to_string(),
        permission_mode: None,
        tool_name: tool_name.to_string(),
        tool_input,
        tool_response,
        agent_id: None,
        agent_type: None,
    })
}

pub fn user_prompt(prompt: &str) -> HookInput {
    HookInput::UserPromptSubmit(UserPromptSubmitHookInput {
        session_id: SESSION.to_string(),
        transcript_path: "transcript.jsonl".to_string(),
        cwd: "workdir".to_string(),
        permission_mode: None,
        prompt: prompt.to_string(),
    })
}

pub fn stop(stop_hook_active: bool) -> HookInput {
    HookInput::Stop(StopHookInput {
        session_id: SESSION.to_string(),
        transcript_path: "transcript.jsonl".to_string(),
        cwd: "workdir".to_string(),
        permission_mode: None,
        stop_hook_active,
    })
}

pub fn callback(graph: Arc<Graph>, config: Neo4jHookCallbackConfig) -> Neo4jHookCallback {
    Neo4jHookCallback::new(graph, None, config)
}

/// Run one hook event through the production `HookCallback` entry point and
/// unwrap the synchronous output, which is the only variant this callback emits.
pub async fn fire(
    cb: &Neo4jHookCallback,
    input: &HookInput,
    tool_use_id: Option<&str>,
) -> SyncHookJSONOutput {
    let output = cb
        .execute(input, tool_use_id, &context())
        .await
        .expect("the callback never fails the hook");
    match output {
        HookJSONOutput::Sync(sync) => sync,
        HookJSONOutput::Async(async_) => {
            panic!("the callback is synchronous, got an async output: {async_:?}")
        },
    }
}

/// Every handler answers with `SyncHookJSONOutput::default()`: no `continue`
/// override, no decision, no system message. A hook that blocked a tool call
/// would set one of these, so asserting they are all absent is asserting that
/// the audit trail never interferes with the conversation.
pub fn assert_transparent(output: &SyncHookJSONOutput) {
    assert_eq!(output.continue_, None, "never overrides `continue`");
    assert_eq!(output.decision, None, "never blocks or approves");
    assert_eq!(output.stop_reason, None);
    assert_eq!(output.reason, None);
    assert_eq!(output.system_message, None);
    assert_eq!(output.suppress_output, None);
    assert!(output.hook_specific_output.is_none());
}
