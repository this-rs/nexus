//! A loopback Bolt 4.1 endpoint, shared by the `storage/` behaviour tests.
//!
//! ## Why this exists
//!
//! `storage::neo4j` and the L2 half of `storage::tiered_cache` only ever talk to
//! an `neo4rs::Graph`. `neo4rs` exposes no injectable connection trait and
//! `Graph::execute` / `Graph::run` speak the binary **Bolt** protocol over TCP,
//! so `wiremock` (HTTP) cannot stand in for it. What *is* possible is to speak
//! Bolt back: `neo4rs` 0.8 only supports Bolt 4.0/4.1, whose framing is "`u16`
//! chunk length, payload, `00 00`" and whose payloads are PackStream structs,
//! and `Graph::new` is lazy (`create_pool` connects nothing), so the listener
//! has all the time it needs to come up.
//!
//! The codec and the five request kinds `neo4rs` can send (HELLO, RESET, RUN,
//! DISCARD, PULL) are taken from `tests/neo4j_permission_bolt.rs`, which proved
//! the approach. Two things are added here, because `storage/neo4j.rs` needs
//! them and the permission provider did not:
//!
//! - [`FakeBolt::returning_rows`], for the queries that return more than one
//!   column (`RETURN c, messages`, `RETURN c.id as id, c.updated_at as ...`);
//! - [`pack_datetime`], the Bolt 4 `DateTime` struct (`0xB3 0x46`), which is
//!   what a real server sends back for a property written as `datetime($now)`.
//!   Several tests exist only to show what the production code does with it.
//!
//! It is in-process, on an ephemeral loopback port, with no service, no fixture
//! file and no network egress.
//!
//! ## What it proves, and what it cannot
//!
//! The server **re-reads the Cypher and its parameter map**, so a test can
//! assert on what the code *writes*, not only on what it gets back. It does
//! **not** interpret Cypher: it cannot tell whether
//! `DETACH DELETE c, m RETURN count(c) as deleted` really yields the number the
//! caller assumes. That half needs a real Neo4j and stays out of scope.

#![allow(dead_code)] // each consumer uses a different subset of the fixtures

use neo4rs::Graph;
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

/// `neo4rs` 0.8 mis-decodes the *negative* half of the TINY_INT range: its
/// `BoltInteger::parse` tests `(-16..=127).contains(&(marker as i8))` and then
/// evaluates `marker as i64` on the **unsigned** byte, so `0xFF` comes back as
/// `255` rather than `-1`. Anything below zero therefore goes out as INT_64,
/// which it decodes correctly.
pub fn pack_int(v: i64) -> Vec<u8> {
    if (0..=127).contains(&v) {
        vec![v as u8]
    } else {
        let mut out = vec![0xCB];
        out.extend_from_slice(&v.to_be_bytes());
        out
    }
}

pub fn pack_bool(v: bool) -> Vec<u8> {
    vec![if v { 0xC3 } else { 0xC2 }]
}

pub fn pack_null() -> Vec<u8> {
    vec![0xC0]
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

/// A `(:Node)` value as Bolt 4 encodes it: id, labels, properties.
pub fn pack_node(id: i64, labels: &[&str], properties: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let labels: Vec<Vec<u8>> = labels.iter().copied().map(pack_string).collect();
    pack_struct(
        0x4E,
        &[pack_int(id), pack_list(&labels), pack_map(properties)],
    )
}

/// A Bolt 4 `DateTime` (`0xB3 0x46`: seconds, nanoseconds, tz offset) — what a
/// real server returns for a property that was written as `datetime($now)`.
pub fn pack_datetime(epoch_seconds: i64, nanoseconds: i64, tz_offset_seconds: i64) -> Vec<u8> {
    pack_struct(
        0x46,
        &[
            pack_int(epoch_seconds),
            pack_int(nanoseconds),
            pack_int(tz_offset_seconds),
        ],
    )
}

fn msg_success(metadata: &[(&str, Vec<u8>)]) -> Vec<u8> {
    pack_struct(0x70, &[pack_map(metadata)])
}

fn msg_failure(code: &str, message: &str) -> Vec<u8> {
    pack_struct(
        0x7F,
        &[pack_map(&[
            ("code", pack_string(code)),
            ("message", pack_string(message)),
        ])],
    )
}

/// A `RECORD` whose single field is the already-packed row list.
fn msg_record(row: &[u8]) -> Vec<u8> {
    let mut out = vec![0xB1, 0x71];
    out.extend_from_slice(row);
    out
}

// ---------------------------------------------------------------------------
// PackStream decoding, so the test can assert on the Cypher *and* its parameters
// ---------------------------------------------------------------------------

fn take(input: &mut &[u8], n: usize) -> Vec<u8> {
    let (head, tail) = input.split_at(n);
    *input = tail;
    head.to_vec()
}

fn take_usize(input: &mut &[u8], width: usize) -> usize {
    take(input, width)
        .into_iter()
        .fold(0usize, |acc, b| (acc << 8) | b as usize)
}

fn take_i64(input: &mut &[u8], width: usize) -> i64 {
    let bytes = take(input, width);
    let mut value = bytes[0] as i8 as i64;
    for b in &bytes[1..] {
        value = (value << 8) | *b as i64;
    }
    value
}

fn unpack_string(input: &mut &[u8], n: usize) -> String {
    String::from_utf8(take(input, n)).expect("packstream string is utf-8")
}

/// Decode one PackStream value into JSON, so assertions read like the Cypher
/// parameter map they describe.
fn unpack(input: &mut &[u8]) -> Value {
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

fn unpack_list(input: &mut &[u8], len: usize) -> Value {
    Value::Array((0..len).map(|_| unpack(input)).collect())
}

fn unpack_map(input: &mut &[u8], len: usize) -> Value {
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

/// One `RUN` the code under test sent, as the server saw it.
#[derive(Debug, Clone)]
pub struct RunLog {
    pub cypher: String,
    pub params: Value,
}

impl RunLog {
    /// The named parameter, or a panic naming the map that was actually sent.
    pub fn param(&self, name: &str) -> &Value {
        self.params
            .get(name)
            .unwrap_or_else(|| panic!("no parameter {name:?} in {}", self.params))
    }
}

#[derive(Debug, Clone)]
struct Failure {
    /// Only fail `RUN`s whose Cypher contains this needle (`None` = every one).
    needle: Option<String>,
    code: String,
    message: String,
}

#[derive(Default, Clone)]
struct Script {
    /// The `fields` metadata of the `RUN` success, i.e. the row column names.
    fields: Vec<String>,
    /// One already-packed `RECORD` data list per row the next `PULL` returns.
    records: Vec<Vec<u8>>,
    failure: Option<Failure>,
}

/// A loopback Bolt 4.1 endpoint whose answers the test scripts.
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

    /// The `bolt://` URI that reaches this server.
    pub fn uri(&self) -> String {
        format!("bolt://{}", self.addr)
    }

    /// A `Graph` whose connection pool dials this server. Nothing connects until
    /// the first query: `neo4rs` builds the pool lazily.
    pub async fn graph(&self) -> Arc<Graph> {
        Arc::new(
            Graph::new(self.uri(), "neo4j", "fake-bolt-has-no-auth")
                .await
                .expect("lazy pool"),
        )
    }

    /// The rows the next `PULL` returns, as single-column `(column, value)` pairs.
    pub fn returning(&self, field: &str, rows: Vec<Vec<u8>>) -> &Self {
        self.returning_rows(&[field], rows.into_iter().map(|v| vec![v]).collect())
    }

    /// The rows the next `PULL` returns for a multi-column `RETURN`. Each row
    /// must carry one already-packed value per field, in order.
    pub fn returning_rows(&self, fields: &[&str], rows: Vec<Vec<Vec<u8>>>) -> &Self {
        let mut script = self.script.lock().expect("script");
        script.fields = fields.iter().map(|f| (*f).to_string()).collect();
        script.records = rows
            .into_iter()
            .map(|row| {
                assert_eq!(
                    row.len(),
                    fields.len(),
                    "a RECORD must carry one value per RETURN column"
                );
                pack_list(&row)
            })
            .collect();
        drop(script);
        self
    }

    pub fn returning_nothing(&self) -> &Self {
        self.returning("r", Vec::new())
    }

    /// Answer every `RUN` with a (non-retryable) `FAILURE`.
    pub fn failing(&self, message: &str) -> &Self {
        self.script.lock().expect("script").failure = Some(Failure {
            needle: None,
            code: "Neo.ClientError.Statement.SyntaxError".to_string(),
            message: message.to_string(),
        });
        self
    }

    /// Answer only the `RUN`s whose Cypher contains `needle` with a `FAILURE`.
    pub fn failing_only(&self, needle: &str, message: &str) -> &Self {
        self.script.lock().expect("script").failure = Some(Failure {
            needle: Some(needle.to_string()),
            code: "Neo.ClientError.Statement.SyntaxError".to_string(),
            message: message.to_string(),
        });
        self
    }

    pub fn healed(&self) -> &Self {
        self.script.lock().expect("script").failure = None;
        self
    }

    pub fn runs(&self) -> Vec<RunLog> {
        self.runs.lock().expect("runs").clone()
    }

    pub fn cypher(&self) -> Vec<String> {
        self.runs().into_iter().map(|r| r.cypher).collect()
    }

    pub fn forget_runs(&self) -> &Self {
        self.runs.lock().expect("runs").clear();
        self
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

    pub fn count_runs_matching(&self, needle: &str) -> usize {
        self.runs()
            .iter()
            .filter(|r| r.cypher.contains(needle))
            .count()
    }
}

/// Read one chunked Bolt message, or `None` on a clean end of stream.
async fn read_message(socket: &mut TcpStream) -> std::io::Result<Option<Vec<u8>>> {
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

async fn write_message(socket: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    socket
        .write_all(&(payload.len() as u16).to_be_bytes())
        .await?;
    socket.write_all(payload).await?;
    socket.write_all(&[0, 0]).await?;
    socket.flush().await
}

/// Decode a `RUN` message: `struct(0x10){ query, params, extra }`.
fn decode_run(message: &[u8]) -> RunLog {
    let mut cursor = &message[2..];
    let cypher = unpack(&mut cursor)
        .as_str()
        .expect("RUN carries the Cypher as a string")
        .to_string();
    let params = unpack(&mut cursor);
    RunLog { cypher, params }
}

async fn serve(
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
                runs.lock().expect("runs").push(log.clone());
                let script = script.lock().expect("script").clone();
                let failure = script.failure.as_ref().filter(|f| match &f.needle {
                    Some(needle) => log.cypher.contains(needle.as_str()),
                    None => true,
                });
                let reply = match failure {
                    Some(f) => msg_failure(&f.code, &f.message),
                    None => {
                        let fields: Vec<Vec<u8>> = script
                            .fields
                            .iter()
                            .map(|f| pack_string(f.as_str()))
                            .collect();
                        msg_success(&[("fields", pack_list(&fields))])
                    },
                };
                write_message(&mut socket, &reply).await?;
            },
            // DISCARD
            0x2F => {
                write_message(&mut socket, &msg_success(&[])).await?;
            },
            // PULL — every row, then the end-of-stream SUCCESS
            0x3F => {
                let records = script.lock().expect("script").records.clone();
                for data in records {
                    write_message(&mut socket, &msg_record(&data)).await?;
                }
                write_message(&mut socket, &msg_success(&[("has_more", pack_bool(false))])).await?;
            },
            _ => {
                write_message(&mut socket, &msg_success(&[])).await?;
            },
        }
    }
}
