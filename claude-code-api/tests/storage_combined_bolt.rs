//! Behaviour of [`claude_code_api::core::storage::combined`] — the store that
//! writes to Neo4j *and* mirrors into Meilisearch.
//!
//! # Why this file exists at all
//!
//! Both constructors (`CombinedConversationStore::new`,
//! `CombinedSessionStore::new`) demand a concrete `Neo4jClient`, which earlier
//! measurements treated as a dead end: `neo4rs::Graph` speaks the binary **Bolt**
//! protocol over TCP, so `wiremock` (HTTP) cannot stand in for it, and `neo4rs`
//! exposes no injectable connection trait.
//!
//! That conclusion was wrong. `tests/neo4j_permission_bolt.rs` demonstrated the
//! third way: *speak Bolt back*. `neo4rs` 0.8 only supports Bolt 4.0/4.1, whose
//! framing is "`u16` chunk length, payload, `00 00`" and whose payloads are
//! PackStream structs; and `Graph::new` is lazy, so a loopback listener has all
//! the time it needs to come up. The PackStream codec and the server loop below
//! are that file's, copied rather than shared because `tests/support/` is owned
//! by another agent and a `tests/` helper cannot be added to it without editing
//! `tests/support/mod.rs`. The scripting layer is extended here from one global
//! answer to a **route table**, because `combined.rs` fires several different
//! Cypher queries inside a single public call (`add_message` alone issues three)
//! and each needs its own rows.
//!
//! `Neo4jClient::new` itself needs nothing more: its only eager work is
//! `init_schema`, whose three `CREATE CONSTRAINT` statements have their errors
//! swallowed on purpose.
//!
//! # What this proves and what it cannot
//!
//! The fake server records the Cypher it receives **and its parameters**, so the
//! assertions below are about what `combined.rs` *writes*, not only what it
//! returns — that is how the `model: ""`-instead-of-`null` divergence between the
//! two backends was found. What it cannot do is execute Cypher: it does not
//! prove that `MATCH (c) ... DETACH DELETE c, m RETURN count(c) as deleted`
//! really returns what the code assumes, nor that a property written with
//! `datetime($now)` can be read back as a `String`. Those need a real Neo4j and
//! stay out of reach here.
//!
//! Meilisearch needs no such trick: `MeilisearchConfig.url` is a plain HTTP seam
//! (see `support::http_mocks`), and the mocks are inspected with
//! `received_requests()` so the *documents sent* are asserted, not just the
//! return values.

mod support;

use chrono::{DateTime, SecondsFormat, Utc};
use claude_code_api::core::conversation::ConversationMetadata;
use claude_code_api::core::storage::{
    CombinedConversationStore, CombinedSessionStore, ConversationStore, MeilisearchClient,
    Neo4jClient, Neo4jConfig, SessionStore,
};
use claude_code_api::models::openai::{ChatMessage, ContentPart, ImageUrl, MessageContent};
use serde_json::{Value, json};
use std::sync::Arc;
use support::http_mocks;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use bolt::{FakeBolt, pack_int, pack_list, pack_node, pack_null, pack_string};

// ===========================================================================
// The fake Bolt 4.1 server
// ===========================================================================

/// PackStream codec and Bolt 4.1 server loop, copied from
/// `tests/neo4j_permission_bolt.rs` (see the module docs above for why it is a
/// copy) and extended with a per-query route table.
mod bolt {
    // The codec is kept whole so it stays diffable against the file it came
    // from; this target does not call every marker writer.
    #![allow(dead_code)]

    use serde_json::{Value, json};
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    // -- encoding ----------------------------------------------------------

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

    pub fn pack_int(v: i64) -> Vec<u8> {
        if (-16..=127).contains(&v) {
            vec![v as i8 as u8]
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

    /// A Bolt `DateTime` value — what `datetime($now)` actually stores, as
    /// opposed to the string the reading code expects.
    pub fn pack_datetime(seconds: i64, nanoseconds: i64, tz_offset_seconds: i64) -> Vec<u8> {
        pack_struct(
            0x46,
            &[
                pack_int(seconds),
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

    // -- decoding ----------------------------------------------------------

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

    // -- the server --------------------------------------------------------

    /// One `RUN` the store sent, as the server saw it.
    #[derive(Debug, Clone)]
    pub struct RunLog {
        pub cypher: String,
        pub params: Value,
    }

    impl RunLog {
        /// One Cypher parameter, or a panic naming the map that was sent.
        pub fn param(&self, name: &str) -> &Value {
            self.params
                .get(name)
                .unwrap_or_else(|| panic!("no parameter {name:?} in {:?}", self.params))
        }
    }

    /// How the server answers the `RUN`s whose Cypher contains `needle`
    /// (`needle: None` answers anything that no earlier route claimed).
    #[derive(Clone)]
    struct Route {
        needle: Option<String>,
        /// The `fields` metadata of the `RUN` success, i.e. the column names.
        fields: Vec<String>,
        /// One already-packed `RECORD` data list per row the next `PULL` yields.
        records: Vec<Vec<u8>>,
        failure: Option<String>,
    }

    /// A loopback Bolt 4.1 endpoint whose answers the test scripts, per query.
    pub struct FakeBolt {
        addr: SocketAddr,
        routes: Arc<Mutex<Vec<Route>>>,
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
            let routes = Arc::new(Mutex::new(Vec::new()));
            let runs = Arc::new(Mutex::new(Vec::new()));

            let task = tokio::spawn({
                let routes = Arc::clone(&routes);
                let runs = Arc::clone(&runs);
                async move {
                    while let Ok((socket, _)) = listener.accept().await {
                        let routes = Arc::clone(&routes);
                        let runs = Arc::clone(&runs);
                        tokio::spawn(async move {
                            let _ = serve(socket, routes, runs).await;
                        });
                    }
                }
            });

            Self {
                addr,
                routes,
                runs,
                task,
            }
        }

        pub fn uri(&self) -> String {
            format!("bolt://{}", self.addr)
        }

        /// Answer the `RUN`s containing `needle` with these columns and rows.
        /// Routes are tried in the order they were added, so a narrower needle
        /// must be added before a broader one.
        pub fn answering(&self, needle: &str, fields: &[&str], rows: Vec<Vec<Vec<u8>>>) -> &Self {
            self.routes.lock().expect("routes").push(Route {
                needle: Some(needle.to_string()),
                fields: fields.iter().map(|f| f.to_string()).collect(),
                records: rows.iter().map(|row| pack_list(row)).collect(),
                failure: None,
            });
            self
        }

        /// Answer the `RUN`s containing `needle` with a column list but no rows.
        pub fn answering_nothing(&self, needle: &str, fields: &[&str]) -> &Self {
            self.answering(needle, fields, Vec::new())
        }

        /// Answer the `RUN`s containing `needle` with a `FAILURE`. Inserted
        /// ahead of every other route, so it wins over a data route for the
        /// same query.
        pub fn failing(&self, needle: &str, message: &str) -> &Self {
            self.routes.lock().expect("routes").insert(
                0,
                Route {
                    needle: Some(needle.to_string()),
                    fields: Vec::new(),
                    records: Vec::new(),
                    failure: Some(message.to_string()),
                },
            );
            self
        }

        pub fn runs(&self) -> Vec<RunLog> {
            self.runs.lock().expect("runs").clone()
        }

        /// The first `RUN` whose Cypher contains `needle`, or a panic naming
        /// what the server did see.
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
        routes: Arc<Mutex<Vec<Route>>>,
        runs: Arc<Mutex<Vec<RunLog>>>,
    ) -> std::io::Result<()> {
        // 4 magic bytes + four 4-byte version proposals.
        let mut handshake = [0u8; 20];
        socket.read_exact(&mut handshake).await?;
        assert_eq!(&handshake[..4], &[0x60, 0x60, 0xB0, 0x17], "Bolt magic");
        socket.write_all(&[0, 0, 1, 4]).await?; // speak Bolt 4.1
        socket.flush().await?;

        // The rows the last successful RUN promised to the next PULL. RUN and
        // PULL are sequential on one connection, so this is per-connection
        // state rather than part of the shared script.
        let mut pending: Vec<Vec<u8>> = Vec::new();

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
                    let route = routes
                        .lock()
                        .expect("routes")
                        .iter()
                        .find(|r| match &r.needle {
                            Some(needle) => log.cypher.contains(needle.as_str()),
                            None => true,
                        })
                        .cloned();
                    pending.clear();
                    let reply = match &route {
                        Some(Route {
                            failure: Some(message),
                            ..
                        }) => msg_failure("Neo.ClientError.Statement.SyntaxError", message),
                        Some(route) => {
                            pending = route.records.clone();
                            let fields: Vec<Vec<u8>> = route
                                .fields
                                .iter()
                                .map(|f| pack_string(f.as_str()))
                                .collect();
                            msg_success(&[("fields", pack_list(&fields))])
                        },
                        // No route: an empty result, which is what the three
                        // `CREATE CONSTRAINT` statements of `init_schema` want.
                        None => msg_success(&[("fields", pack_list(&[]))]),
                    };
                    write_message(&mut socket, &reply).await?;
                },
                // DISCARD
                0x2F => {
                    pending.clear();
                    write_message(&mut socket, &msg_success(&[])).await?;
                },
                // PULL — every row of the last RUN, then the end-of-stream
                0x3F => {
                    for data in std::mem::take(&mut pending) {
                        write_message(&mut socket, &msg_record(&data)).await?;
                    }
                    write_message(&mut socket, &msg_success(&[("has_more", pack_bool(false))]))
                        .await?;
                },
                _ => {
                    write_message(&mut socket, &msg_success(&[])).await?;
                },
            }
        }
    }
}

// ===========================================================================
// Cypher needles — the substring that identifies each query of the two stores
// ===========================================================================

const Q_CONV_CREATE: &str = "CREATE (c:NexusConversation";
const Q_CONV_GET: &str = "RETURN c, messages";
const Q_CONV_ADD_MESSAGE: &str = "CREATE (m:NexusMessage";
const Q_CONV_UPDATE_METADATA: &str = "SET c.model = $model";
const Q_CONV_LIST: &str = "RETURN c.id as id, c.updated_at as updated_at";
const Q_CONV_CLEANUP: &str = "duration({minutes: $timeout})";
const Q_CONV_DELETE: &str = "DETACH DELETE c, m";
const Q_SESSION_CREATE: &str = "CREATE (s:NexusSession";
const Q_SESSION_UPDATE: &str = "SET s.updated_at = datetime($now)";
const Q_SESSION_REMOVE: &str = "DETACH DELETE s";
const Q_SESSION_BY_ID: &str = "MATCH (s:NexusSession {id: $id})";
const Q_SESSION_LIST: &str = "MATCH (s:NexusSession)";

// ===========================================================================
// Graph rows
// ===========================================================================

fn rfc3339(moment: DateTime<Utc>) -> String {
    moment.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// A `(:NexusConversation)` node with the properties the reader looks for.
/// `created_at` / `updated_at` are packed as **strings**, which is what
/// `parse_neo4j_datetime` requires — see `datetime_properties_...` below for
/// what the writer actually stores.
fn conversation_node(
    id: &str,
    model: Option<&str>,
    total_tokens: i64,
    turn_count: i64,
    updated_at: DateTime<Utc>,
) -> Vec<u8> {
    pack_node(
        1,
        &["NexusConversation"],
        &[
            ("id", pack_string(id)),
            ("model", model.map(pack_string).unwrap_or_else(pack_null)),
            ("total_tokens", pack_int(total_tokens)),
            ("turn_count", pack_int(turn_count)),
            ("created_at", pack_string(&rfc3339(updated_at))),
            ("updated_at", pack_string(&rfc3339(updated_at))),
        ],
    )
}

fn message_node(role: &str, content: &str) -> Vec<u8> {
    pack_node(
        2,
        &["NexusMessage"],
        &[
            ("role", pack_string(role)),
            ("content", pack_string(content)),
        ],
    )
}

fn session_node(
    id: &str,
    project_path: Option<&str>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
) -> Vec<u8> {
    pack_node(
        3,
        &["NexusSession"],
        &[
            ("id", pack_string(id)),
            (
                "project_path",
                project_path.map(pack_string).unwrap_or_else(pack_null),
            ),
            ("created_at", pack_string(&rfc3339(created_at))),
            ("updated_at", pack_string(&rfc3339(updated_at))),
        ],
    )
}

/// The single-row answer every `RETURN c.id as id` / `RETURN s.id as id` query
/// needs in order not to look like "not found".
fn one_id_row(id: &str) -> Vec<Vec<Vec<u8>>> {
    vec![vec![pack_string(id)]]
}

/// The single-row answer of `RETURN count(c) as deleted`.
fn deleted_row(count: i64) -> Vec<Vec<Vec<u8>>> {
    vec![vec![pack_int(count)]]
}

async fn neo4j_client(bolt: &FakeBolt) -> Neo4jClient {
    Neo4jClient::new(Neo4jConfig {
        uri: bolt.uri(),
        user: "neo4j".to_string(),
        password: "the-fake-bolt-server-checks-no-credentials".to_string(),
        max_connections: 1,
    })
    .await
    .expect("Graph::new is lazy and init_schema swallows its errors")
}

// ===========================================================================
// The Meilisearch mock — and the requests it recorded
// ===========================================================================

/// Which Meilisearch operations answer with an error instead of `202`.
#[derive(Clone, Copy, Default)]
struct Rejecting {
    writes: bool,
    deletes: bool,
    searches: bool,
}

fn task_info(kind: &str) -> Value {
    json!({
        "taskUid": 1,
        "indexUid": "nexus_conversations",
        "status": "enqueued",
        "type": kind,
        "details": null,
        "enqueuedAt": "2026-01-01T00:00:00Z",
    })
}

fn search_results(hits: Vec<Value>) -> Value {
    let total = hits.len();
    json!({
        "hits": hits,
        "offset": 0,
        "limit": 20,
        "estimatedTotalHits": total,
        "processingTimeMs": 1,
        "query": "",
    })
}

fn upstream_error() -> Value {
    json!({
        "message": "injected meilisearch failure",
        "code": "internal",
        "type": "internal",
        "link": "https://example.invalid",
    })
}

fn answer(rejected: bool, ok: ResponseTemplate) -> ResponseTemplate {
    if rejected {
        ResponseTemplate::new(500).set_body_json(upstream_error())
    } else {
        ok
    }
}

/// A Meilisearch whose bootstrap always succeeds — `MeilisearchClient::new`
/// must get through — but whose later operations can be made to fail one family
/// at a time. The matchers are disjoint, so no two mocks race for a request.
async fn meilisearch_mock(
    message_hits: Vec<Value>,
    conversation_hits: Vec<Value>,
    rejecting: Rejecting,
) -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/indexes"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task_info("indexCreation")))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex(r"^/indexes/[^/]+/settings$"))
        .respond_with(ResponseTemplate::new(202).set_body_json(task_info("settingsUpdate")))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/indexes/[^/]+/documents$"))
        .respond_with(answer(
            rejecting.writes,
            ResponseTemplate::new(202).set_body_json(task_info("documentAdditionOrUpdate")),
        ))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path_regex(r"^/indexes/[^/]+/documents/.+$"))
        .respond_with(answer(
            rejecting.deletes,
            ResponseTemplate::new(202).set_body_json(task_info("documentDeletion")),
        ))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/indexes/nexus_messages/search"))
        .respond_with(answer(
            rejecting.searches,
            ResponseTemplate::new(200).set_body_json(search_results(message_hits)),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/indexes/nexus_conversations/search"))
        .respond_with(answer(
            rejecting.searches,
            ResponseTemplate::new(200).set_body_json(search_results(conversation_hits)),
        ))
        .mount(&server)
        .await;

    server
}

async fn meilisearch_ok() -> MockServer {
    meilisearch_mock(Vec::new(), Vec::new(), Rejecting::default()).await
}

/// The documents `POST /indexes/<index>/documents` received, flattened out of
/// the one-element arrays the SDK sends.
async fn indexed_documents(server: &MockServer, index: &str) -> Vec<Value> {
    let wanted = format!("/indexes/{index}/documents");
    server
        .received_requests()
        .await
        .expect("wiremock records requests")
        .into_iter()
        .filter(|r| r.method == http_types_post() && r.url.path() == wanted)
        .flat_map(|r| {
            serde_json::from_slice::<Vec<Value>>(&r.body).expect("a JSON array of documents")
        })
        .collect()
}

/// The document ids `DELETE /indexes/<index>/documents/<id>` received. Every id
/// asserted on here is URL-safe, so the path segment is the id.
async fn deleted_document_ids(server: &MockServer, index: &str) -> Vec<String> {
    let prefix = format!("/indexes/{index}/documents/");
    server
        .received_requests()
        .await
        .expect("wiremock records requests")
        .into_iter()
        .filter(|r| r.method == wiremock::http::Method::DELETE)
        .filter_map(|r| r.url.path().strip_prefix(&prefix).map(str::to_string))
        .collect()
}

/// The search bodies `POST /indexes/<index>/search` received.
async fn search_bodies(server: &MockServer, index: &str) -> Vec<Value> {
    let wanted = format!("/indexes/{index}/search");
    server
        .received_requests()
        .await
        .expect("wiremock records requests")
        .into_iter()
        .filter(|r| r.method == http_types_post() && r.url.path() == wanted)
        .map(|r| serde_json::from_slice::<Value>(&r.body).expect("a JSON search body"))
        .collect()
}

fn http_types_post() -> wiremock::http::Method {
    wiremock::http::Method::POST
}

async fn meilisearch_client(server: &MockServer) -> Arc<MeilisearchClient> {
    Arc::new(
        http_mocks::meilisearch_client(server)
            .await
            .expect("the bootstrap mocks answer 202"),
    )
}

// ===========================================================================
// Message fixtures
// ===========================================================================

fn text_message(role: &str, text: &str) -> ChatMessage {
    ChatMessage {
        role: role.to_string(),
        content: Some(MessageContent::Text(text.to_string())),
        name: None,
        tool_calls: None,
    }
}

fn empty_message(role: &str) -> ChatMessage {
    ChatMessage {
        role: role.to_string(),
        content: None,
        name: None,
        tool_calls: None,
    }
}

/// A multimodal message: two text parts around one image part.
fn multimodal_message() -> ChatMessage {
    ChatMessage {
        role: "user".to_string(),
        content: Some(MessageContent::Array(vec![
            ContentPart::Text {
                text: "avant".to_string(),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "https://example.invalid/chat.png".to_string(),
                    detail: None,
                },
            },
            ContentPart::Text {
                text: "apres".to_string(),
            },
        ])),
        name: None,
        tool_calls: None,
    }
}

// ===========================================================================
// Search — the three methods that answer without touching Neo4j
// ===========================================================================

/// Without Meilisearch the three search methods answer `Ok(vec![])`. That is a
/// silent default where a refusal would be more honest: a caller cannot tell
/// "nothing matched" from "search is not configured on this deployment", so a
/// deployment that forgot Meilisearch looks like one with an empty index.
#[tokio::test]
async fn searching_without_meilisearch_is_an_empty_list_not_a_refusal() {
    let bolt = FakeBolt::start().await;
    let store = CombinedConversationStore::new(neo4j_client(&bolt).await, None);

    assert!(
        store
            .search_messages("bonjour", 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .search_conversation_messages("conv-1", "bonjour", 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .search_conversations("bonjour", 10)
            .await
            .unwrap()
            .is_empty()
    );
}

/// An unscoped `search_messages` sends no filter at all, so it really does span
/// every conversation, and the hits come back decoded.
#[tokio::test]
async fn search_messages_spans_every_conversation_and_decodes_the_hits() {
    let bolt = FakeBolt::start().await;
    let search = meilisearch_mock(
        vec![http_mocks::message_hit(
            "conv-7-3",
            "conv-7",
            "assistant",
            "bonjour Nexus",
        )],
        Vec::new(),
        Rejecting::default(),
    )
    .await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    let hits = store.search_messages("bonjour", 5).await.unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "conv-7-3");
    assert_eq!(hits[0].conversation_id, "conv-7");
    assert_eq!(hits[0].role, "assistant");
    assert_eq!(hits[0].content, "bonjour Nexus");

    let body = search_bodies(&search, "nexus_messages").await.remove(0);
    assert_eq!(body["q"], json!("bonjour"));
    assert_eq!(body["limit"], json!(5));
    assert!(
        body.get("filter").is_none_or(Value::is_null),
        "an unscoped search must not narrow to one conversation: {body}"
    );
}

/// Scoping to one conversation becomes a `conversation_id = "..."` filter, and
/// an id carrying a double quote is escaped rather than allowed to close the
/// literal and widen the filter.
#[tokio::test]
async fn search_conversation_messages_scopes_and_escapes_the_conversation_id() {
    let bolt = FakeBolt::start().await;
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    store
        .search_conversation_messages("conv-7", "bonjour", 5)
        .await
        .unwrap();
    store
        .search_conversation_messages(r#"x" OR role = "user"#, "bonjour", 5)
        .await
        .unwrap();

    let bodies = search_bodies(&search, "nexus_messages").await;
    assert_eq!(bodies[0]["filter"], json!(r#"conversation_id = "conv-7""#));
    assert_eq!(
        bodies[1]["filter"],
        json!(r#"conversation_id = "x\" OR role = \"user""#)
    );
}

/// `search_conversations` reads the conversation index, with the caller's limit.
#[tokio::test]
async fn search_conversations_reads_the_conversation_index() {
    let bolt = FakeBolt::start().await;
    let search = meilisearch_mock(
        Vec::new(),
        vec![http_mocks::conversation_hit(
            "conv-7",
            "claude-3",
            "un apercu",
        )],
        Rejecting::default(),
    )
    .await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    let hits = store.search_conversations("apercu", 3).await.unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "conv-7");
    assert_eq!(hits[0].model, Some("claude-3".to_string()));
    assert_eq!(hits[0].content_preview, "un apercu");

    let body = search_bodies(&search, "nexus_conversations")
        .await
        .remove(0);
    assert_eq!(body["limit"], json!(3));
}

/// A search failure is *not* swallowed — unlike every write path in this file,
/// the three search methods propagate their error.
#[tokio::test]
async fn a_failing_search_is_reported_to_the_caller() {
    let bolt = FakeBolt::start().await;
    let search = meilisearch_mock(
        Vec::new(),
        Vec::new(),
        Rejecting {
            searches: true,
            ..Default::default()
        },
    )
    .await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    for message in [
        store.search_messages("x", 1).await.unwrap_err().to_string(),
        store
            .search_conversation_messages("conv-1", "x", 1)
            .await
            .unwrap_err()
            .to_string(),
        store
            .search_conversations("x", 1)
            .await
            .unwrap_err()
            .to_string(),
    ] {
        assert!(
            message.contains("injected meilisearch failure"),
            "the upstream error must survive: {message}"
        );
    }
}

// ===========================================================================
// create
// ===========================================================================

/// The two backends disagree about a conversation with no model from the very
/// first write: Neo4j is given `model: ""` (`Option::unwrap_or_default` in
/// `Neo4jConversationStore::create`) while the Meilisearch document keeps
/// `model: null`. A filter such as `model = ""` therefore matches in one store
/// and not the other, for the same conversation.
#[tokio::test]
async fn create_without_a_model_writes_an_empty_string_to_neo4j_and_null_to_meilisearch() {
    let bolt = FakeBolt::start().await;
    bolt.answering(Q_CONV_CREATE, &["id"], one_id_row("ignored"));
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    let id = store.create(None).await.unwrap();
    assert!(!id.is_empty());

    let run = bolt.run_matching(Q_CONV_CREATE);
    assert_eq!(run.param("id"), &json!(id));
    assert_eq!(
        run.param("model"),
        &json!(""),
        "Neo4j is given an empty string where the caller said None"
    );

    let docs = indexed_documents(&search, "nexus_conversations").await;
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0]["id"], json!(id));
    assert_eq!(docs[0]["model"], Value::Null);
    assert_eq!(docs[0]["message_count"], json!(0));
    assert_eq!(docs[0]["total_tokens"], json!(0));
    assert_eq!(docs[0]["content_preview"], json!(""));
}

/// With a model, both stores carry the same string.
#[tokio::test]
async fn create_indexes_the_model_it_was_given() {
    let bolt = FakeBolt::start().await;
    bolt.answering(Q_CONV_CREATE, &["id"], one_id_row("ignored"));
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    let id = store.create(Some("claude-3".to_string())).await.unwrap();

    assert_eq!(
        bolt.run_matching(Q_CONV_CREATE).param("model"),
        &json!("claude-3")
    );
    let docs = indexed_documents(&search, "nexus_conversations").await;
    assert_eq!(docs[0]["model"], json!("claude-3"));
    assert_eq!(docs[0]["id"], json!(id));
}

/// A Meilisearch that refuses the document does not fail the creation: the
/// conversation exists in Neo4j and the caller gets its id, with only a `warn!`
/// to say the index is already behind.
#[tokio::test]
async fn create_succeeds_even_when_the_index_refuses_the_document() {
    let bolt = FakeBolt::start().await;
    bolt.answering(Q_CONV_CREATE, &["id"], one_id_row("ignored"));
    let search = meilisearch_mock(
        Vec::new(),
        Vec::new(),
        Rejecting {
            writes: true,
            ..Default::default()
        },
    )
    .await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    let id = store.create(Some("claude-3".to_string())).await.unwrap();

    assert!(!id.is_empty());
    assert_eq!(bolt.count_runs_matching(Q_CONV_CREATE), 1);
    assert_eq!(
        indexed_documents(&search, "nexus_conversations")
            .await
            .len(),
        1,
        "the attempt was made, and rejected"
    );
}

/// Without Meilisearch, `create` is exactly `Neo4jConversationStore::create`.
#[tokio::test]
async fn create_without_meilisearch_only_writes_to_neo4j() {
    let bolt = FakeBolt::start().await;
    bolt.answering(Q_CONV_CREATE, &["id"], one_id_row("ignored"));
    let store = CombinedConversationStore::new(neo4j_client(&bolt).await, None);

    let id = store.create(None).await.unwrap();

    assert_eq!(bolt.run_matching(Q_CONV_CREATE).param("id"), &json!(id));
}

/// When Neo4j refuses, nothing is indexed: the `?` fires before the document is
/// built, so the search index never learns about a conversation that does not
/// exist.
#[tokio::test]
async fn create_that_neo4j_refuses_indexes_nothing() {
    let bolt = FakeBolt::start().await;
    bolt.failing(Q_CONV_CREATE, "constraint violated");
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    let error = store.create(None).await.unwrap_err().to_string();

    assert!(
        error.contains("constraint violated"),
        "the Neo4j error must reach the caller: {error}"
    );
    assert!(
        indexed_documents(&search, "nexus_conversations")
            .await
            .is_empty()
    );
}

// ===========================================================================
// get
// ===========================================================================

/// `get` is a straight delegation; the mapping it delegates to turns the node
/// and its message list into a `Conversation`.
#[tokio::test]
async fn get_maps_the_graph_row_onto_a_conversation() {
    let updated = Utc::now() - chrono::Duration::minutes(4);
    let bolt = FakeBolt::start().await;
    bolt.answering(
        Q_CONV_GET,
        &["c", "messages"],
        vec![vec![
            conversation_node("conv-7", Some("claude-3"), 1234, 2, updated),
            pack_list(&[
                message_node("user", "bonjour"),
                message_node("assistant", "salut"),
            ]),
        ]],
    );
    let store = CombinedConversationStore::new(neo4j_client(&bolt).await, None);

    let conv = store.get("conv-7").await.unwrap().expect("one row");

    assert_eq!(conv.id, "conv-7");
    assert_eq!(conv.metadata.model, Some("claude-3".to_string()));
    assert_eq!(conv.metadata.total_tokens, 1234);
    assert_eq!(conv.metadata.turn_count, 2);
    assert_eq!(conv.updated_at.timestamp(), updated.timestamp());
    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[0].role, "user");
    assert!(matches!(
        &conv.messages[1].content,
        Some(MessageContent::Text(text)) if text == "salut"
    ));
    assert_eq!(bolt.run_matching(Q_CONV_GET).param("id"), &json!("conv-7"));
}

/// No row means `Ok(None)`, not an error.
#[tokio::test]
async fn get_of_an_unknown_conversation_is_ok_none() {
    let bolt = FakeBolt::start().await;
    bolt.answering_nothing(Q_CONV_GET, &["c", "messages"]);
    let store = CombinedConversationStore::new(neo4j_client(&bolt).await, None);

    assert!(store.get("jamais-cree").await.unwrap().is_none());
}

/// `Neo4jConversationStore::create` writes `created_at: datetime($now)`, so a
/// real graph answers with a Bolt `DateTime` struct — not the `String` that
/// `parse_neo4j_datetime` asks for. This is the one assumption about Neo4j's
/// *semantics* that can be checked without a server, by sending the wire shape a
/// real Neo4j would send: `neo4rs` renders a `DateTime` as an RFC 3339 string on
/// the way to `String`, so the read holds. Without this test the suspicion that
/// every `get` fails against a real Neo4j would stay open.
#[tokio::test]
async fn timestamps_stored_as_bolt_datetimes_still_parse() {
    let bolt = FakeBolt::start().await;
    let node = bolt::pack_node(
        1,
        &["NexusConversation"],
        &[
            ("id", pack_string("conv-7")),
            ("model", pack_string("claude-3")),
            ("total_tokens", pack_int(0)),
            ("turn_count", pack_int(0)),
            ("created_at", bolt::pack_datetime(1_767_225_600, 0, 0)),
            ("updated_at", bolt::pack_datetime(1_767_225_600, 0, 0)),
        ],
    );
    bolt.answering(
        Q_CONV_GET,
        &["c", "messages"],
        vec![vec![node, pack_list(&[])]],
    );
    let store = CombinedConversationStore::new(neo4j_client(&bolt).await, None);

    let conv = store.get("conv-7").await.unwrap().expect("one row");

    assert_eq!(
        conv.created_at,
        DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap(),
        "a Bolt DateTime property reaches the caller as the moment it encodes"
    );
    assert_eq!(conv.updated_at, conv.created_at);
}

// ===========================================================================
// add_message
// ===========================================================================

/// `add_message` costs three Neo4j round trips for one message: a read for the
/// turn index, the write, and a second read to rebuild the search preview. The
/// indexed document id is `<conversation>-<turn index read before the write>`.
#[tokio::test]
async fn add_message_indexes_with_the_turn_index_it_read_before_writing() {
    let bolt = FakeBolt::start().await;
    bolt.answering(
        Q_CONV_GET,
        &["c", "messages"],
        vec![vec![
            conversation_node("conv-7", Some("claude-3"), 11, 7, Utc::now()),
            pack_list(&[message_node("user", "bonjour")]),
        ]],
    );
    bolt.answering(Q_CONV_ADD_MESSAGE, &["id"], one_id_row("conv-7"));
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    store
        .add_message("conv-7", text_message("assistant", "salut"))
        .await
        .unwrap();

    assert_eq!(
        bolt.count_runs_matching(Q_CONV_GET),
        2,
        "one read for the turn index, one to rebuild the preview"
    );
    let write = bolt.run_matching(Q_CONV_ADD_MESSAGE);
    assert_eq!(write.param("role"), &json!("assistant"));
    assert_eq!(write.param("content"), &json!("salut"));

    let docs = indexed_documents(&search, "nexus_messages").await;
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0]["id"], json!("conv-7-7"));
    assert_eq!(docs[0]["conversation_id"], json!("conv-7"));
    assert_eq!(docs[0]["turn_index"], json!(7));
    assert_eq!(docs[0]["role"], json!("assistant"));
    assert_eq!(docs[0]["content"], json!("salut"));
}

/// The Meilisearch document id is built from the turn count read **before** the
/// write (`combined.rs`: `format!("{}-{}", conversation_id, turn_index)` in
/// `index_message`, fed by the `self.get(id)` at the top of `add_message`). Two
/// writes that read the same turn count therefore produce the same document id,
/// and the second silently overwrites the first — one message vanishes from the
/// search index while both are in the graph.
///
/// The fake graph here answers both reads with `turn_count: 7`, which is exactly
/// what two concurrent `add_message` calls see: Neo4j assigns distinct
/// `turn_index` values inside its own statement, the index does not.
#[tokio::test]
#[ignore = "defect: add_message/index_message derive the document id from a pre-write read; \
            fixing it means taking the turn index from the post-write row, which \
            Neo4jConversationStore::add_message does not return (neo4j.rs, out of scope)"]
async fn two_messages_that_read_the_same_turn_count_get_distinct_document_ids() {
    let bolt = FakeBolt::start().await;
    bolt.answering(
        Q_CONV_GET,
        &["c", "messages"],
        vec![vec![
            conversation_node("conv-7", None, 0, 7, Utc::now()),
            pack_list(&[]),
        ]],
    );
    bolt.answering(Q_CONV_ADD_MESSAGE, &["id"], one_id_row("conv-7"));
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    store
        .add_message("conv-7", text_message("user", "premier"))
        .await
        .unwrap();
    store
        .add_message("conv-7", text_message("user", "second"))
        .await
        .unwrap();

    let docs = indexed_documents(&search, "nexus_messages").await;
    assert_eq!(docs.len(), 2, "both messages were sent to the index");
    assert_ne!(
        docs[0]["id"], docs[1]["id"],
        "two messages must not share a document id, or one replaces the other"
    );
}

/// A message added to a conversation that does not exist fails on the Neo4j
/// write, and nothing is indexed — the turn index silently defaults to 0 first,
/// which would collide with the real turn 0 if the write ever succeeded.
#[tokio::test]
async fn add_message_to_an_unknown_conversation_fails_and_indexes_nothing() {
    let bolt = FakeBolt::start().await;
    bolt.answering_nothing(Q_CONV_GET, &["c", "messages"]);
    bolt.answering_nothing(Q_CONV_ADD_MESSAGE, &["id"]);
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    let error = store
        .add_message("jamais-cree", text_message("user", "bonjour"))
        .await
        .unwrap_err()
        .to_string();

    assert_eq!(error, "Conversation not found: jamais-cree");
    assert!(
        indexed_documents(&search, "nexus_messages")
            .await
            .is_empty()
    );
}

/// The image part of a multimodal message is dropped from the indexed content —
/// correct for a text index, but silent: the remaining text parts are joined
/// with a newline and nothing records that a part was discarded.
#[tokio::test]
async fn add_message_indexes_only_the_text_parts_of_a_multimodal_message() {
    let bolt = FakeBolt::start().await;
    bolt.answering(
        Q_CONV_GET,
        &["c", "messages"],
        vec![vec![
            conversation_node("conv-7", None, 0, 0, Utc::now()),
            pack_list(&[]),
        ]],
    );
    bolt.answering(Q_CONV_ADD_MESSAGE, &["id"], one_id_row("conv-7"));
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    store
        .add_message("conv-7", multimodal_message())
        .await
        .unwrap();

    let docs = indexed_documents(&search, "nexus_messages").await;
    assert_eq!(
        docs[0]["content"],
        json!("avant\napres"),
        "the image url must not reach the index, and the text must not be lost"
    );
}

/// A message with no content at all is indexed as an empty string rather than
/// skipped, so the search index gains a document that can never match.
#[tokio::test]
async fn add_message_without_content_indexes_an_empty_document() {
    let bolt = FakeBolt::start().await;
    bolt.answering(
        Q_CONV_GET,
        &["c", "messages"],
        vec![vec![
            conversation_node("conv-7", None, 0, 0, Utc::now()),
            pack_list(&[]),
        ]],
    );
    bolt.answering(Q_CONV_ADD_MESSAGE, &["id"], one_id_row("conv-7"));
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    store
        .add_message("conv-7", empty_message("tool"))
        .await
        .unwrap();

    let docs = indexed_documents(&search, "nexus_messages").await;
    assert_eq!(docs[0]["content"], json!(""));
    assert_eq!(
        docs[0]["role"],
        json!("tool"),
        "no role is filtered out on the way to the index"
    );
}

/// Both indexing calls of `add_message` are fire-and-forget: the message is in
/// Neo4j, the caller gets `Ok`, and only a `warn!` says the index is stale.
#[tokio::test]
async fn add_message_returns_ok_when_both_index_writes_fail() {
    let bolt = FakeBolt::start().await;
    bolt.answering(
        Q_CONV_GET,
        &["c", "messages"],
        vec![vec![
            conversation_node("conv-7", None, 0, 0, Utc::now()),
            pack_list(&[]),
        ]],
    );
    bolt.answering(Q_CONV_ADD_MESSAGE, &["id"], one_id_row("conv-7"));
    let search = meilisearch_mock(
        Vec::new(),
        Vec::new(),
        Rejecting {
            writes: true,
            ..Default::default()
        },
    )
    .await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    store
        .add_message("conv-7", text_message("user", "bonjour"))
        .await
        .unwrap();

    assert_eq!(
        indexed_documents(&search, "nexus_messages").await.len(),
        1,
        "the message write was attempted"
    );
    assert_eq!(
        indexed_documents(&search, "nexus_conversations")
            .await
            .len(),
        1,
        "and so was the conversation preview"
    );
}

/// Without Meilisearch, `add_message` still pays for the second read even
/// though nothing consumes it.
#[tokio::test]
async fn add_message_without_meilisearch_still_reads_the_conversation_twice() {
    let bolt = FakeBolt::start().await;
    bolt.answering(
        Q_CONV_GET,
        &["c", "messages"],
        vec![vec![
            conversation_node("conv-7", None, 0, 3, Utc::now()),
            pack_list(&[]),
        ]],
    );
    bolt.answering(Q_CONV_ADD_MESSAGE, &["id"], one_id_row("conv-7"));
    let store = CombinedConversationStore::new(neo4j_client(&bolt).await, None);

    store
        .add_message("conv-7", text_message("user", "bonjour"))
        .await
        .unwrap();

    assert_eq!(bolt.count_runs_matching(Q_CONV_GET), 2);
}

// ===========================================================================
// The search preview built by update_conversation_index
// ===========================================================================

/// The preview takes the five *most recent* messages and joins them in reverse
/// order, so the indexed text reads newest-first.
#[tokio::test]
async fn the_preview_keeps_the_five_most_recent_messages_newest_first() {
    let bolt = FakeBolt::start().await;
    let messages: Vec<Vec<u8>> = (1..=7)
        .map(|n| message_node("user", &format!("m{n}")))
        .collect();
    bolt.answering(
        Q_CONV_GET,
        &["c", "messages"],
        vec![vec![
            conversation_node("conv-7", Some("claude-3"), 55, 7, Utc::now()),
            pack_list(&messages),
        ]],
    );
    bolt.answering(Q_CONV_ADD_MESSAGE, &["id"], one_id_row("conv-7"));
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    store
        .add_message("conv-7", text_message("user", "m8"))
        .await
        .unwrap();

    let docs = indexed_documents(&search, "nexus_conversations").await;
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0]["content_preview"], json!("m7 m6 m5 m4 m3"));
    assert_eq!(
        docs[0]["message_count"],
        json!(7),
        "message_count is the whole list, not the five previewed"
    );
    assert_eq!(docs[0]["total_tokens"], json!(55));
}

/// The preview is cut at 500 **characters**, counted with `chars()`, so a
/// multi-byte conversation is not cut mid-character.
#[tokio::test]
async fn the_preview_is_cut_at_five_hundred_characters_not_bytes() {
    let bolt = FakeBolt::start().await;
    let long = "é".repeat(600);
    bolt.answering(
        Q_CONV_GET,
        &["c", "messages"],
        vec![vec![
            conversation_node("conv-7", None, 0, 1, Utc::now()),
            pack_list(&[message_node("user", &long)]),
        ]],
    );
    bolt.answering(Q_CONV_ADD_MESSAGE, &["id"], one_id_row("conv-7"));
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    store
        .add_message("conv-7", text_message("user", "x"))
        .await
        .unwrap();

    let docs = indexed_documents(&search, "nexus_conversations").await;
    let preview = docs[0]["content_preview"].as_str().expect("a string");
    assert_eq!(preview.chars().count(), 500);
    assert_eq!(preview.len(), 1000, "500 two-byte characters");
}

/// Re-indexing after the first message replaces the `model: null` written at
/// creation with `model: ""`, because that is what Neo4j now holds. The same
/// conversation's indexed model therefore changes shape on its own.
#[tokio::test]
async fn the_first_message_rewrites_a_null_model_as_an_empty_string() {
    let bolt = FakeBolt::start().await;
    bolt.answering(Q_CONV_CREATE, &["id"], one_id_row("ignored"));
    // What `Neo4jConversationStore::create(None)` stored: the empty string.
    bolt.answering(
        Q_CONV_GET,
        &["c", "messages"],
        vec![vec![
            conversation_node("conv-7", Some(""), 0, 0, Utc::now()),
            pack_list(&[]),
        ]],
    );
    bolt.answering(Q_CONV_ADD_MESSAGE, &["id"], one_id_row("conv-7"));
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    let id = store.create(None).await.unwrap();
    store
        .add_message(&id, text_message("user", "bonjour"))
        .await
        .unwrap();

    let docs = indexed_documents(&search, "nexus_conversations").await;
    assert_eq!(docs[0]["model"], Value::Null, "at creation");
    assert_eq!(docs[1]["model"], json!(""), "after the first message");
}

// ===========================================================================
// update_metadata
// ===========================================================================

/// `update_metadata` writes the metadata to Neo4j and then re-indexes from what
/// Neo4j reports, not from the value it was given.
#[tokio::test]
async fn update_metadata_writes_to_neo4j_then_reindexes_from_the_graph() {
    let bolt = FakeBolt::start().await;
    bolt.answering(Q_CONV_UPDATE_METADATA, &["id"], one_id_row("conv-7"));
    bolt.answering(
        Q_CONV_GET,
        &["c", "messages"],
        vec![vec![
            conversation_node("conv-7", Some("claude-3"), 4242, 9, Utc::now()),
            pack_list(&[message_node("user", "bonjour")]),
        ]],
    );
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    store
        .update_metadata(
            "conv-7",
            ConversationMetadata {
                model: None,
                total_tokens: 100,
                turn_count: 5,
                project_path: Some("/projet".to_string()),
            },
        )
        .await
        .unwrap();

    let write = bolt.run_matching(Q_CONV_UPDATE_METADATA);
    assert_eq!(write.param("total_tokens"), &json!(100));
    assert_eq!(write.param("turn_count"), &json!(5));
    assert_eq!(
        write.param("model"),
        &json!(""),
        "a None model is written as an empty string"
    );
    assert!(
        write.params.get("project_path").is_none(),
        "project_path is accepted by the API and then dropped: {:?}",
        write.params
    );

    let docs = indexed_documents(&search, "nexus_conversations").await;
    assert_eq!(
        docs[0]["total_tokens"],
        json!(4242),
        "the graph's value wins"
    );
    assert_eq!(docs[0]["model"], json!("claude-3"));
}

/// A metadata update on a conversation that is gone is an error, and no
/// document is re-indexed.
#[tokio::test]
async fn update_metadata_on_an_unknown_conversation_fails_and_reindexes_nothing() {
    let bolt = FakeBolt::start().await;
    bolt.answering_nothing(Q_CONV_UPDATE_METADATA, &["id"]);
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    let error = store
        .update_metadata("jamais-cree", ConversationMetadata::default())
        .await
        .unwrap_err()
        .to_string();

    assert_eq!(error, "Conversation not found: jamais-cree");
    assert!(
        indexed_documents(&search, "nexus_conversations")
            .await
            .is_empty()
    );
    assert_eq!(
        bolt.count_runs_matching(Q_CONV_GET),
        0,
        "the re-index read is skipped too"
    );
}

// ===========================================================================
// list_active
// ===========================================================================

/// `list_active` is a straight delegation, and the delegate drops any row whose
/// `updated_at` is not RFC 3339 — silently, so a corrupt timestamp makes a
/// conversation disappear from every caller that iterates this list.
#[tokio::test]
async fn list_active_silently_drops_rows_with_an_unparseable_timestamp() {
    let moment = Utc::now() - chrono::Duration::minutes(3);
    let bolt = FakeBolt::start().await;
    bolt.answering(
        Q_CONV_LIST,
        &["id", "updated_at"],
        vec![
            vec![pack_string("lisible"), pack_string(&rfc3339(moment))],
            vec![pack_string("corrompu"), pack_string("2026-10-01 12:00:00")],
        ],
    );
    let store = CombinedConversationStore::new(neo4j_client(&bolt).await, None);

    let active = store.list_active().await.unwrap();

    assert_eq!(active.len(), 1, "the unparseable row vanished");
    assert_eq!(active[0].0, "lisible");
    assert_eq!(active[0].1.timestamp(), moment.timestamp());
}

// ===========================================================================
// cleanup_expired
// ===========================================================================

/// Expiry is decided **twice**: here, in the client, from `list_active`'s
/// timestamps, to choose which search documents to drop; and again inside Neo4j,
/// in Cypher, to choose which nodes to delete. This test pins that only the
/// client-side selection reaches Meilisearch, and that the count returned is
/// Neo4j's.
#[tokio::test]
async fn cleanup_expired_drops_the_search_documents_it_judged_expired_itself() {
    let bolt = FakeBolt::start().await;
    bolt.answering(
        Q_CONV_LIST,
        &["id", "updated_at"],
        vec![
            vec![
                pack_string("vieille"),
                pack_string(&rfc3339(Utc::now() - chrono::Duration::hours(3))),
            ],
            vec![pack_string("recente"), pack_string(&rfc3339(Utc::now()))],
        ],
    );
    bolt.answering(Q_CONV_CLEANUP, &["deleted"], deleted_row(1));
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    assert_eq!(store.cleanup_expired(60).await.unwrap(), 1);

    assert_eq!(
        deleted_document_ids(&search, "nexus_conversations").await,
        vec!["vieille".to_string()]
    );
    assert_eq!(
        bolt.run_matching(Q_CONV_CLEANUP).param("timeout"),
        &json!(60)
    );
}

/// A negative timeout makes the client-side predicate true for everything,
/// including a conversation updated a moment ago, so every search document is
/// dropped. The value is passed straight to Neo4j as well.
#[tokio::test]
async fn cleanup_expired_with_a_negative_timeout_drops_every_search_document() {
    let bolt = FakeBolt::start().await;
    bolt.answering(
        Q_CONV_LIST,
        &["id", "updated_at"],
        vec![
            vec![pack_string("une"), pack_string(&rfc3339(Utc::now()))],
            vec![pack_string("deux"), pack_string(&rfc3339(Utc::now()))],
        ],
    );
    bolt.answering(Q_CONV_CLEANUP, &["deleted"], deleted_row(2));
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    assert_eq!(store.cleanup_expired(-1).await.unwrap(), 2);

    let mut dropped = deleted_document_ids(&search, "nexus_conversations").await;
    dropped.sort();
    assert_eq!(dropped, vec!["deux".to_string(), "une".to_string()]);
    assert_eq!(
        bolt.run_matching(Q_CONV_CLEANUP).param("timeout"),
        &json!(-1)
    );
}

/// The search documents go **before** Neo4j is asked to delete anything. When
/// the Neo4j cleanup then fails, the conversation is still in the graph but no
/// longer in the index: it exists and is unsearchable, and nothing will put it
/// back because `cleanup_expired` never re-indexes.
#[tokio::test]
async fn a_failed_neo4j_cleanup_leaves_the_search_documents_already_deleted() {
    let bolt = FakeBolt::start().await;
    bolt.answering(
        Q_CONV_LIST,
        &["id", "updated_at"],
        vec![vec![
            pack_string("vieille"),
            pack_string(&rfc3339(Utc::now() - chrono::Duration::hours(3))),
        ]],
    );
    bolt.failing(Q_CONV_CLEANUP, "deadlock detected");
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    let error = store.cleanup_expired(60).await.unwrap_err().to_string();

    assert!(error.contains("deadlock detected"), "got {error}");
    assert_eq!(
        deleted_document_ids(&search, "nexus_conversations").await,
        vec!["vieille".to_string()],
        "the index was already emptied of a conversation Neo4j still holds"
    );
}

/// A row `list_active` could not parse never reaches the expired set, so its
/// search documents survive a cleanup that Neo4j reports as successful: an
/// orphan document pointing at a conversation that no longer exists.
#[tokio::test]
async fn cleanup_expired_orphans_the_documents_of_rows_it_could_not_parse() {
    let bolt = FakeBolt::start().await;
    bolt.answering(
        Q_CONV_LIST,
        &["id", "updated_at"],
        vec![vec![
            pack_string("corrompu"),
            pack_string("2026-10-01 12:00:00"),
        ]],
    );
    bolt.answering(Q_CONV_CLEANUP, &["deleted"], deleted_row(1));
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    assert_eq!(
        store.cleanup_expired(60).await.unwrap(),
        1,
        "Neo4j deleted the conversation"
    );
    assert!(
        deleted_document_ids(&search, "nexus_conversations")
            .await
            .is_empty(),
        "but its search documents were left behind"
    );
}

/// A failing Meilisearch cannot stop the cleanup: the delete error is discarded
/// with `let _ =`, without even a `warn!`.
#[tokio::test]
async fn cleanup_expired_ignores_a_failing_index_without_a_word() {
    let bolt = FakeBolt::start().await;
    bolt.answering(
        Q_CONV_LIST,
        &["id", "updated_at"],
        vec![vec![
            pack_string("vieille"),
            pack_string(&rfc3339(Utc::now() - chrono::Duration::hours(3))),
        ]],
    );
    bolt.answering(Q_CONV_CLEANUP, &["deleted"], deleted_row(1));
    let search = meilisearch_mock(
        Vec::new(),
        Vec::new(),
        Rejecting {
            deletes: true,
            ..Default::default()
        },
    )
    .await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    assert_eq!(store.cleanup_expired(60).await.unwrap(), 1);
    assert_eq!(
        deleted_document_ids(&search, "nexus_conversations").await,
        vec!["vieille".to_string()],
        "the attempt was made, the 500 was dropped on the floor"
    );
}

/// Without Meilisearch, `cleanup_expired` still reads `list_active` and filters
/// it before throwing the result away.
#[tokio::test]
async fn cleanup_expired_without_meilisearch_still_lists_the_conversations() {
    let bolt = FakeBolt::start().await;
    bolt.answering(Q_CONV_LIST, &["id", "updated_at"], Vec::new());
    bolt.answering(Q_CONV_CLEANUP, &["deleted"], deleted_row(0));
    let store = CombinedConversationStore::new(neo4j_client(&bolt).await, None);

    assert_eq!(store.cleanup_expired(60).await.unwrap(), 0);
    assert_eq!(bolt.count_runs_matching(Q_CONV_LIST), 1);
}

/// When `list_active` fails there is nothing to filter, and the Neo4j cleanup is
/// never attempted either.
#[tokio::test]
async fn cleanup_expired_stops_if_it_cannot_list_the_conversations() {
    let bolt = FakeBolt::start().await;
    bolt.failing(Q_CONV_LIST, "index unavailable");
    bolt.answering(Q_CONV_CLEANUP, &["deleted"], deleted_row(9));
    let store = CombinedConversationStore::new(neo4j_client(&bolt).await, None);

    let error = store.cleanup_expired(60).await.unwrap_err().to_string();

    assert!(error.contains("index unavailable"), "got {error}");
    assert_eq!(bolt.count_runs_matching(Q_CONV_CLEANUP), 0);
}

// ===========================================================================
// delete
// ===========================================================================

/// `delete` removes the search documents first and then reports whatever Neo4j
/// says it deleted.
#[tokio::test]
async fn delete_drops_the_search_documents_then_reports_neo4js_answer() {
    let bolt = FakeBolt::start().await;
    bolt.answering(Q_CONV_DELETE, &["deleted"], deleted_row(1));
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    assert!(store.delete("conv-7").await.unwrap());

    assert_eq!(
        deleted_document_ids(&search, "nexus_conversations").await,
        vec!["conv-7".to_string()]
    );
    assert_eq!(
        bolt.run_matching(Q_CONV_DELETE).param("id"),
        &json!("conv-7")
    );
}

/// Deleting an id that does not exist returns `false` — but the search documents
/// for that id are destroyed first, unconditionally. A caller that treats
/// `Ok(false)` as "nothing happened" is wrong.
#[tokio::test]
async fn delete_of_an_unknown_id_returns_false_after_wiping_the_index_anyway() {
    let bolt = FakeBolt::start().await;
    bolt.answering(Q_CONV_DELETE, &["deleted"], deleted_row(0));
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    assert!(!store.delete("jamais-cree").await.unwrap());

    assert_eq!(
        deleted_document_ids(&search, "nexus_conversations").await,
        vec!["jamais-cree".to_string()],
        "the index write happened before anyone knew the conversation existed"
    );
}

/// No row at all also means `false`.
#[tokio::test]
async fn delete_with_no_row_at_all_is_false() {
    let bolt = FakeBolt::start().await;
    bolt.answering_nothing(Q_CONV_DELETE, &["deleted"]);
    let store = CombinedConversationStore::new(neo4j_client(&bolt).await, None);

    assert!(!store.delete("conv-7").await.unwrap());
}

/// A failing index delete is discarded with `let _ =` and no log: the orphan
/// document it leaves behind is undiagnosable from the outside.
#[tokio::test]
async fn delete_ignores_a_failing_index_delete_entirely() {
    let bolt = FakeBolt::start().await;
    bolt.answering(Q_CONV_DELETE, &["deleted"], deleted_row(1));
    let search = meilisearch_mock(
        Vec::new(),
        Vec::new(),
        Rejecting {
            deletes: true,
            ..Default::default()
        },
    )
    .await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    assert!(
        store.delete("conv-7").await.unwrap(),
        "the Neo4j deletion is all the caller is told about"
    );
}

/// When Neo4j refuses the delete, the search documents are already gone.
#[tokio::test]
async fn a_failed_neo4j_delete_leaves_the_search_documents_deleted() {
    let bolt = FakeBolt::start().await;
    bolt.failing(Q_CONV_DELETE, "transaction terminated");
    let search = meilisearch_ok().await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    let error = store.delete("conv-7").await.unwrap_err().to_string();

    assert!(error.contains("transaction terminated"), "got {error}");
    assert_eq!(
        deleted_document_ids(&search, "nexus_conversations").await,
        vec!["conv-7".to_string()]
    );
}

/// The message documents of the deleted conversation go too — found by a scoped
/// search, which is why the filter escaping matters here as well.
#[tokio::test]
async fn delete_also_removes_the_message_documents_it_can_find() {
    let bolt = FakeBolt::start().await;
    bolt.answering(Q_CONV_DELETE, &["deleted"], deleted_row(1));
    let search = meilisearch_mock(
        vec![
            http_mocks::message_hit("conv-7-0", "conv-7", "user", "bonjour"),
            http_mocks::message_hit("conv-7-1", "conv-7", "assistant", "salut"),
        ],
        Vec::new(),
        Rejecting::default(),
    )
    .await;
    let store = CombinedConversationStore::new(
        neo4j_client(&bolt).await,
        Some(meilisearch_client(&search).await),
    );

    assert!(store.delete("conv-7").await.unwrap());

    let mut dropped = deleted_document_ids(&search, "nexus_messages").await;
    dropped.sort();
    assert_eq!(
        dropped,
        vec!["conv-7-0".to_string(), "conv-7-1".to_string()]
    );
}

// ===========================================================================
// CombinedSessionStore — a pass-through, with Neo4j's quirks showing through
// ===========================================================================

/// `create` generates the id itself and writes the project path; a `None` path
/// becomes `""` in the graph, exactly as for the conversation model.
#[tokio::test]
async fn session_create_writes_an_empty_string_for_an_absent_project_path() {
    let bolt = FakeBolt::start().await;
    let store = CombinedSessionStore::new(neo4j_client(&bolt).await);

    let with_path = store.create(Some("/projet".to_string())).await.unwrap();
    let without_path = store.create(None).await.unwrap();

    assert_ne!(with_path, without_path);
    let runs: Vec<Value> = bolt
        .runs()
        .into_iter()
        .filter(|r| r.cypher.contains(Q_SESSION_CREATE))
        .map(|r| r.params)
        .collect();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["project_path"], json!("/projet"));
    assert_eq!(runs[0]["id"], json!(with_path));
    assert_eq!(
        runs[1]["project_path"],
        json!(""),
        "None becomes an empty string, not null"
    );
}

/// A Neo4j refusal on create reaches the caller.
#[tokio::test]
async fn session_create_propagates_a_neo4j_failure() {
    let bolt = FakeBolt::start().await;
    bolt.failing(Q_SESSION_CREATE, "database is read only");
    let store = CombinedSessionStore::new(neo4j_client(&bolt).await);

    let error = store.create(None).await.unwrap_err().to_string();
    assert!(error.contains("database is read only"), "got {error}");
}

/// `get` maps the node, and an empty `project_path` comes back as `Some("")` —
/// so a session created without a path is indistinguishable from one created
/// with the empty path.
#[tokio::test]
async fn session_get_returns_some_empty_string_for_a_path_that_was_never_given() {
    let created = Utc::now() - chrono::Duration::hours(2);
    let updated = Utc::now() - chrono::Duration::minutes(1);
    let bolt = FakeBolt::start().await;
    bolt.answering(
        Q_SESSION_BY_ID,
        &["s"],
        vec![vec![session_node("sess-1", Some(""), created, updated)]],
    );
    let store = CombinedSessionStore::new(neo4j_client(&bolt).await);

    let session = store.get("sess-1").await.unwrap().expect("one row");

    assert_eq!(session.id, "sess-1");
    assert_eq!(
        session.project_path,
        Some(String::new()),
        "the round trip turned None into Some(\"\")"
    );
    assert_eq!(session.created_at.timestamp(), created.timestamp());
    assert_eq!(session.updated_at.timestamp(), updated.timestamp());
}

/// No row means `Ok(None)`.
#[tokio::test]
async fn session_get_of_an_unknown_id_is_ok_none() {
    let bolt = FakeBolt::start().await;
    bolt.answering_nothing(Q_SESSION_BY_ID, &["s"]);
    let store = CombinedSessionStore::new(neo4j_client(&bolt).await);

    assert!(store.get("jamais-cree").await.unwrap().is_none());
}

/// `update` only touches `updated_at`, and says so in the parameters it sends.
#[tokio::test]
async fn session_update_sends_only_a_new_timestamp() {
    let bolt = FakeBolt::start().await;
    bolt.answering(Q_SESSION_UPDATE, &["id"], one_id_row("sess-1"));
    let store = CombinedSessionStore::new(neo4j_client(&bolt).await);

    store.update("sess-1").await.unwrap();

    let run = bolt.run_matching(Q_SESSION_UPDATE);
    assert_eq!(run.param("id"), &json!("sess-1"));
    let now = run.param("now").as_str().expect("an RFC 3339 string");
    assert!(
        DateTime::parse_from_rfc3339(now).is_ok(),
        "the timestamp must be the format the reader expects: {now}"
    );
}

/// Touching a session that is gone is an error, not a silent insert.
#[tokio::test]
async fn session_update_of_an_unknown_id_is_an_error() {
    let bolt = FakeBolt::start().await;
    bolt.answering_nothing(Q_SESSION_UPDATE, &["id"]);
    let store = CombinedSessionStore::new(neo4j_client(&bolt).await);

    let error = store.update("jamais-cree").await.unwrap_err().to_string();
    assert_eq!(error, "Session not found: jamais-cree");
}

/// `remove` reads the session first so it can return it, then deletes it.
#[tokio::test]
async fn session_remove_returns_the_session_it_deleted() {
    let moment = Utc::now();
    let bolt = FakeBolt::start().await;
    bolt.answering(Q_SESSION_REMOVE, &[], Vec::new());
    bolt.answering(
        Q_SESSION_BY_ID,
        &["s"],
        vec![vec![session_node(
            "sess-1",
            Some("/projet"),
            moment,
            moment,
        )]],
    );
    let store = CombinedSessionStore::new(neo4j_client(&bolt).await);

    let removed = store.remove("sess-1").await.unwrap().expect("the session");

    assert_eq!(removed.id, "sess-1");
    assert_eq!(removed.project_path, Some("/projet".to_string()));
    assert_eq!(
        bolt.run_matching(Q_SESSION_REMOVE).param("id"),
        &json!("sess-1")
    );
}

/// Removing an unknown session issues no delete at all.
#[tokio::test]
async fn session_remove_of_an_unknown_id_issues_no_delete() {
    let bolt = FakeBolt::start().await;
    bolt.answering(Q_SESSION_REMOVE, &[], Vec::new());
    bolt.answering_nothing(Q_SESSION_BY_ID, &["s"]);
    let store = CombinedSessionStore::new(neo4j_client(&bolt).await);

    assert!(store.remove("jamais-cree").await.unwrap().is_none());
    assert_eq!(bolt.count_runs_matching(Q_SESSION_REMOVE), 0);
}

/// `list` maps every row, and — unlike `get` — falls back to "now" for a
/// timestamp it cannot parse instead of failing, so a corrupt session is
/// returned looking freshly updated.
#[tokio::test]
async fn session_list_invents_a_timestamp_for_a_row_it_cannot_parse() {
    let moment = Utc::now() - chrono::Duration::days(1);
    let bolt = FakeBolt::start().await;
    let corrupt = bolt::pack_node(
        3,
        &["NexusSession"],
        &[
            ("id", pack_string("corrompue")),
            ("project_path", pack_string("/projet")),
            ("created_at", pack_string("pas-une-date")),
            ("updated_at", pack_string("pas-une-date")),
        ],
    );
    bolt.answering(
        Q_SESSION_LIST,
        &["s"],
        vec![
            vec![session_node("saine", None, moment, moment)],
            vec![corrupt],
        ],
    );
    let store = CombinedSessionStore::new(neo4j_client(&bolt).await);

    let sessions = store.list().await.unwrap();

    assert_eq!(sessions.len(), 2, "the corrupt row is kept, not dropped");
    assert_eq!(sessions[0].id, "saine");
    assert_eq!(
        sessions[0].project_path, None,
        "a null property reads back as None"
    );
    assert_eq!(sessions[1].id, "corrompue");
    assert!(
        sessions[1].updated_at > moment,
        "the unparseable timestamp was replaced by now, hiding the corruption"
    );
}

/// A row missing the `id` property aborts the whole listing rather than being
/// skipped: `node.get("id")?` is the only `?` in the loop.
#[tokio::test]
async fn session_list_fails_entirely_on_a_row_without_an_id() {
    let bolt = FakeBolt::start().await;
    let nameless = bolt::pack_node(
        3,
        &["NexusSession"],
        &[("project_path", pack_string("/projet"))],
    );
    bolt.answering(Q_SESSION_LIST, &["s"], vec![vec![nameless]]);
    let store = CombinedSessionStore::new(neo4j_client(&bolt).await);

    assert!(store.list().await.is_err());
}

/// An empty graph is an empty list, not an error.
#[tokio::test]
async fn session_list_of_an_empty_graph_is_empty() {
    let bolt = FakeBolt::start().await;
    bolt.answering_nothing(Q_SESSION_LIST, &["s"]);
    let store = CombinedSessionStore::new(neo4j_client(&bolt).await);

    assert!(store.list().await.unwrap().is_empty());
}
