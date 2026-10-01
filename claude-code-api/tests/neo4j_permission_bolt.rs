//! Behaviour tests for [`claude_code_api::core::hooks::Neo4jPermissionProvider`].
//!
//! `Neo4jPermissionProvider` is a *security guard*: it turns rows stored in Neo4j
//! into allow/deny decisions for tool calls. Every path that matters here is a
//! refusal path, or a path where a refusal is *silently converted into an
//! allowance* — a missing rule, a rule with a garbage `decision`, a rule whose
//! `id` property is absent, a graph that answers `FAILURE`.
//!
//! ## Why there is a Bolt server in this file
//!
//! The provider only takes an `Arc<neo4rs::Graph>`; `neo4rs` exposes no
//! injectable connection trait, and `Graph::execute` / `Graph::run` speak the
//! binary **Bolt** protocol over TCP, so `wiremock` (HTTP) cannot stand in for
//! it. What *is* possible is to speak Bolt back: `neo4rs` 0.8 only supports
//! Bolt 4.0/4.1, whose framing is "`u16` chunk length, payload, `00 00`" and
//! whose payloads are PackStream structs. [`FakeBolt`] below is ~200 lines that
//! answer the five request kinds `neo4rs` can send (HELLO, RESET, RUN, DISCARD,
//! PULL) with scripted `SUCCESS` / `FAILURE` / `RECORD` messages.
//!
//! It is a loopback listener on an ephemeral port, in-process, with no service,
//! no fixture file and no network egress — the same shape as the `wiremock`
//! servers the rest of the suite uses, one protocol lower.
//!
//! This is what makes the row-decoding half of the provider (`reload_rules`,
//! `list_rules`, `remove_rule`) reachable at all, including the cases where the
//! stored rule is malformed.
//!
//! ## One line stays uncovered on purpose
//!
//! The argument of `reload_rules`' closing `debug!("Loaded {} permission rules")`
//! is only evaluated when a subscriber is listening at DEBUG. Installing one
//! from a test does not work reliably here: `tracing` caches each callsite's
//! interest — and the global max level — process-wide, and the 39 other tests in
//! this binary re-register those callsites from threads that have no subscriber,
//! so whether the event fires depends on the harness's scheduling. An
//! order-dependent test is worse than an honestly uncovered line.

use claude_code_api::core::hooks::{Neo4jPermissionProvider, PermissionRule, PermissionScope};
use neo4rs::Graph;
use nexus_claude::{CanUseTool, PermissionResult, ToolPermissionContext};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// PackStream encoding (only the markers `neo4rs` 0.8 can parse)
// ---------------------------------------------------------------------------

fn pack_string(s: &str) -> Vec<u8> {
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

fn pack_int(v: i64) -> Vec<u8> {
    if (-16..=127).contains(&v) {
        vec![v as i8 as u8]
    } else {
        let mut out = vec![0xCB];
        out.extend_from_slice(&v.to_be_bytes());
        out
    }
}

fn pack_bool(v: bool) -> Vec<u8> {
    vec![if v { 0xC3 } else { 0xC2 }]
}

fn pack_null() -> Vec<u8> {
    vec![0xC0]
}

fn pack_list(items: &[Vec<u8>]) -> Vec<u8> {
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

fn pack_map(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
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

fn pack_struct(signature: u8, fields: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![0xB0 | fields.len() as u8, signature];
    for field in fields {
        out.extend_from_slice(field);
    }
    out
}

/// A `(:Node)` value as Bolt 4 encodes it: id, labels, properties.
fn pack_node(id: i64, labels: &[&str], properties: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let labels: Vec<Vec<u8>> = labels.iter().copied().map(pack_string).collect();
    pack_struct(
        0x4E,
        &[pack_int(id), pack_list(&labels), pack_map(properties)],
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

/// One `RUN` the provider sent, as the server saw it.
#[derive(Debug, Clone)]
struct RunLog {
    cypher: String,
    params: Value,
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
struct FakeBolt {
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
    async fn start() -> Self {
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
    async fn graph(&self) -> Arc<Graph> {
        let uri = format!("bolt://{}", self.addr);
        Arc::new(
            Graph::new(uri, "neo4j", "fake-bolt-has-no-auth")
                .await
                .expect("lazy pool"),
        )
    }

    /// The rows the next `PULL` returns, as `(column, value)` pairs.
    fn returning(&self, field: &str, rows: Vec<Vec<u8>>) -> &Self {
        let mut script = self.script.lock().expect("script");
        script.fields = vec![field.to_string()];
        script.records = rows.into_iter().map(|row| pack_list(&[row])).collect();
        drop(script);
        self
    }

    fn returning_nothing(&self) -> &Self {
        self.returning("r", Vec::new())
    }

    /// Answer every `RUN` with a (non-retryable) `FAILURE`.
    fn failing(&self, message: &str) -> &Self {
        self.script.lock().expect("script").failure = Some(Failure {
            needle: None,
            code: "Neo.ClientError.Statement.SyntaxError".to_string(),
            message: message.to_string(),
        });
        self
    }

    /// Answer only the `RUN`s whose Cypher contains `needle` with a `FAILURE`.
    fn failing_only(&self, needle: &str, message: &str) -> &Self {
        self.script.lock().expect("script").failure = Some(Failure {
            needle: Some(needle.to_string()),
            code: "Neo.ClientError.Statement.SyntaxError".to_string(),
            message: message.to_string(),
        });
        self
    }

    fn healed(&self) -> &Self {
        self.script.lock().expect("script").failure = None;
        self
    }

    fn runs(&self) -> Vec<RunLog> {
        self.runs.lock().expect("runs").clone()
    }

    fn forget_runs(&self) -> &Self {
        self.runs.lock().expect("runs").clear();
        self
    }

    /// The single `RUN` whose Cypher contains `needle`, or a panic naming what
    /// the server did see.
    fn run_matching(&self, needle: &str) -> RunLog {
        let runs = self.runs();
        match runs.iter().find(|r| r.cypher.contains(needle)) {
            Some(hit) => hit.clone(),
            None => panic!(
                "no RUN contained {needle:?}; the server saw {:?}",
                runs.iter().map(|r| r.cypher.as_str()).collect::<Vec<_>>()
            ),
        }
    }

    fn count_runs_matching(&self, needle: &str) -> usize {
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

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn rule_node(
    id: &str,
    tool_pattern: &str,
    decision: &str,
    reason: Vec<u8>,
    scope: &str,
    scope_id: Vec<u8>,
    priority: Vec<u8>,
) -> Vec<u8> {
    pack_node(
        1,
        &["NexusPermissionRule"],
        &[
            ("id", pack_string(id)),
            ("tool_pattern", pack_string(tool_pattern)),
            ("decision", pack_string(decision)),
            ("reason", reason),
            ("scope", pack_string(scope)),
            ("scope_id", scope_id),
            ("priority", priority),
        ],
    )
}

/// A global rule with an explicit reason and priority 0.
fn global_rule(id: &str, tool_pattern: &str, decision: &str, reason: &str) -> Vec<u8> {
    rule_node(
        id,
        tool_pattern,
        decision,
        pack_string(reason),
        "global",
        pack_null(),
        pack_int(0),
    )
}

fn context() -> ToolPermissionContext {
    ToolPermissionContext {
        signal: None,
        suggestions: Vec::new(),
    }
}

async fn ask(provider: &Neo4jPermissionProvider, tool: &str) -> PermissionResult {
    provider.can_use_tool(tool, &json!({}), &context()).await
}

/// `"allow"` / `"deny"`, so a failed assertion prints the decision.
fn verdict(result: &PermissionResult) -> &'static str {
    match result {
        PermissionResult::Allow(_) => "allow",
        PermissionResult::Deny(_) => "deny",
    }
}

fn denial(result: &PermissionResult) -> (String, bool) {
    match result {
        PermissionResult::Deny(d) => (d.message.clone(), d.interrupt),
        PermissionResult::Allow(_) => panic!("expected a denial, got an allowance"),
    }
}

// ---------------------------------------------------------------------------
// init_schema
// ---------------------------------------------------------------------------

#[tokio::test]
async fn init_schema_sends_both_uniqueness_constraints_and_the_pattern_index() {
    let bolt = FakeBolt::start().await;
    let provider = Neo4jPermissionProvider::new(bolt.graph().await);

    provider.init_schema().await.expect("schema");

    let cypher: Vec<String> = bolt.runs().into_iter().map(|r| r.cypher).collect();
    assert_eq!(cypher.len(), 3, "two constraints and one index: {cypher:?}");
    assert!(cypher[0].contains("CONSTRAINT nexus_permission_rule_id"));
    assert!(cypher[0].contains("r.id IS UNIQUE"));
    assert!(cypher[1].contains("CONSTRAINT nexus_permission_audit_id"));
    assert!(cypher[1].contains("a.id IS UNIQUE"));
    assert!(cypher[2].contains("INDEX nexus_permission_rule_pattern"));
    assert!(cypher[2].contains("ON (r.tool_pattern)"));
    // Every statement is `IF NOT EXISTS`, so re-running must stay a no-op.
    assert!(
        cypher.iter().all(|c| c.contains("IF NOT EXISTS")),
        "{cypher:?}"
    );
}

/// `init_schema` swallows **every** error with `debug!` and then logs
/// "schema initialized" and returns `Ok(())`. A caller that bootstraps the
/// permission store cannot tell a working Neo4j from one that rejected all
/// three statements.
#[tokio::test]
async fn init_schema_reports_success_even_though_every_statement_failed() {
    let bolt = FakeBolt::start().await;
    bolt.failing("constraints are not supported here");
    let provider = Neo4jPermissionProvider::new(bolt.graph().await);

    let result = provider.init_schema().await;

    assert!(
        result.is_ok(),
        "init_schema hides schema failures; see the report"
    );
    assert_eq!(bolt.runs().len(), 3, "it still tried all three statements");
}

// ---------------------------------------------------------------------------
// reload_rules
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reload_rules_queries_the_three_scopes_with_the_configured_ids() {
    let bolt = FakeBolt::start().await;
    bolt.returning_nothing();
    let provider = Neo4jPermissionProvider::new(bolt.graph().await)
        .with_project("proj-1".to_string())
        .with_workspace("ws-1".to_string());

    provider.reload_rules().await.expect("reload");

    let run = bolt.run_matching("MATCH (r:NexusPermissionRule)");
    assert!(run.cypher.contains("r.scope = 'global'"));
    assert!(
        run.cypher
            .contains("r.scope = 'workspace' AND r.scope_id = $workspace_id")
    );
    assert!(
        run.cypher
            .contains("r.scope = 'project' AND r.scope_id = $project_id")
    );
    assert!(
        run.cypher.contains("ORDER BY r.priority DESC"),
        "ordering is delegated to Cypher: {}",
        run.cypher
    );
    assert_eq!(run.params["workspace_id"], json!("ws-1"));
    assert_eq!(run.params["project_id"], json!("proj-1"));
}

/// With no scope configured, both ids go out as `""` rather than `null`, so a
/// rule stored with `scope_id: ""` is picked up by *every* unscoped provider.
#[tokio::test]
async fn reload_rules_sends_empty_scope_ids_when_no_scope_is_configured() {
    let bolt = FakeBolt::start().await;
    bolt.returning_nothing();
    let provider = Neo4jPermissionProvider::new(bolt.graph().await);

    provider.reload_rules().await.expect("reload");

    let run = bolt.run_matching("MATCH (r:NexusPermissionRule)");
    assert_eq!(run.params["workspace_id"], json!(""));
    assert_eq!(run.params["project_id"], json!(""));
}

#[tokio::test]
async fn reload_rules_decodes_a_deny_rule_and_uses_its_reason_as_the_message() {
    let bolt = FakeBolt::start().await;
    bolt.returning(
        "r",
        vec![global_rule("deny-bash", "Bash", "deny", "no shell in prod")],
    );
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);

    provider.reload_rules().await.expect("reload");

    let (message, interrupt) = denial(&ask(&provider, "Bash").await);
    assert_eq!(message, "no shell in prod");
    assert!(!interrupt, "a denial never interrupts the conversation");
}

/// `reason` is read with `node.get("reason").ok()`, so a `null` property becomes
/// `None` instead of an error — the denial still happens, with a generated
/// message.
#[tokio::test]
async fn reload_rules_falls_back_to_a_generated_message_when_the_reason_is_null() {
    let bolt = FakeBolt::start().await;
    bolt.returning(
        "r",
        vec![rule_node(
            "deny-write",
            "Write",
            "deny",
            pack_null(),
            "global",
            pack_null(),
            pack_int(0),
        )],
    );
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);

    provider.reload_rules().await.expect("reload");

    let (message, _) = denial(&ask(&provider, "Write").await);
    assert_eq!(message, "Tool 'Write' is denied by permission rule");
}

/// A node whose mandatory `id` is missing aborts the whole reload with an error:
/// no rule of that batch is loaded, not even the well-formed ones.
#[tokio::test]
async fn reload_rules_rejects_a_rule_whose_id_property_is_missing() {
    let bolt = FakeBolt::start().await;
    bolt.returning(
        "r",
        vec![pack_node(
            7,
            &["NexusPermissionRule"],
            &[
                ("tool_pattern", pack_string("Bash")),
                ("decision", pack_string("deny")),
            ],
        )],
    );
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);

    let error = provider.reload_rules().await.expect_err("malformed rule");
    // The `?` on `node.get("id")` surfaces the neo4rs message verbatim, which
    // names neither the property nor the rule: an operator reading the gateway
    // log cannot tell which row is broken. See the report.
    assert_eq!(error.to_string(), "The property does not exist");
}

/// The same for a rule with no `decision`: the guard refuses to load it rather
/// than guessing.
#[tokio::test]
async fn reload_rules_rejects_a_rule_whose_decision_property_is_missing() {
    let bolt = FakeBolt::start().await;
    bolt.returning(
        "r",
        vec![pack_node(
            7,
            &["NexusPermissionRule"],
            &[
                ("id", pack_string("broken")),
                ("tool_pattern", pack_string("Bash")),
            ],
        )],
    );
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);

    provider
        .reload_rules()
        .await
        .expect_err("a rule with no decision is not loadable");
}

/// A malformed row is not a way to disarm the guard either: the rules loaded by
/// the previous successful reload stay in effect.
#[tokio::test]
async fn a_reload_that_hits_a_malformed_rule_keeps_the_previous_rules() {
    let bolt = FakeBolt::start().await;
    bolt.returning("r", vec![global_rule("deny-bash", "Bash", "deny", "nope")]);
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);
    provider.reload_rules().await.expect("first reload");

    bolt.returning(
        "r",
        vec![pack_node(
            9,
            &["NexusPermissionRule"],
            &[("id", pack_string("half-written"))],
        )],
    );
    provider.reload_rules().await.expect_err("malformed rule");

    let (message, _) = denial(&ask(&provider, "Bash").await);
    assert_eq!(message, "nope");
}

/// An unknown `scope` string is silently downgraded to `Global`, and a missing
/// `priority` to `0`. Both are visible through `list_rules`, which decodes rows
/// with the same block.
#[tokio::test]
async fn list_rules_downgrades_an_unknown_scope_to_global_and_a_missing_priority_to_zero() {
    let bolt = FakeBolt::start().await;
    bolt.returning(
        "r",
        vec![pack_node(
            1,
            &["NexusPermissionRule"],
            &[
                ("id", pack_string("odd")),
                ("tool_pattern", pack_string("Read")),
                ("decision", pack_string("allow")),
                ("scope", pack_string("organisation")),
            ],
        )],
    );
    let provider = Neo4jPermissionProvider::new(bolt.graph().await);

    let rules = provider.list_rules(None).await.expect("list");

    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].scope, PermissionScope::Global);
    assert_eq!(rules[0].priority, 0);
    assert_eq!(rules[0].reason, None);
}

#[tokio::test]
async fn reload_rules_propagates_the_graph_failure() {
    let bolt = FakeBolt::start().await;
    bolt.failing("graph is down");
    let provider = Neo4jPermissionProvider::new(bolt.graph().await);

    let error = provider.reload_rules().await.expect_err("graph is down");
    assert!(error.to_string().contains("graph is down"), "{error}");
}

/// Regression test for the one-line fix in `reload_rules`: it used to call
/// `rules_cache.clear()` *before* querying Neo4j, so a reload that failed left
/// the guard with no rules at all and `can_use_tool` fell through to its default
/// allowance. Replayed against the previous code this asserts `"allow"` instead
/// of `"deny"`.
#[tokio::test]
async fn a_failed_reload_keeps_the_rules_it_had_already_loaded() {
    let bolt = FakeBolt::start().await;
    bolt.returning(
        "r",
        vec![global_rule("deny-all", "*", "deny", "locked down")],
    );
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);

    provider.reload_rules().await.expect("first reload");
    assert_eq!(verdict(&ask(&provider, "Bash").await), "deny");

    bolt.failing("graph went away");
    provider.reload_rules().await.expect_err("second reload");

    let (message, _) = denial(&ask(&provider, "Bash").await);
    assert_eq!(
        message, "locked down",
        "an unreachable rule store must not grant what it was denying"
    );
}

/// A successful reload still replaces the whole rule set, so a rule that was
/// dropped from Neo4j stops applying.
#[tokio::test]
async fn a_successful_reload_replaces_the_previous_rules() {
    let bolt = FakeBolt::start().await;
    bolt.returning(
        "r",
        vec![global_rule("deny-all", "*", "deny", "locked down")],
    );
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);
    provider.reload_rules().await.expect("first reload");
    assert_eq!(verdict(&ask(&provider, "Bash").await), "deny");

    bolt.returning_nothing();
    provider.reload_rules().await.expect("second reload");

    assert_eq!(
        verdict(&ask(&provider, "Bash").await),
        "allow",
        "the deleted rule no longer applies"
    );
}

/// What remains of the fail-open default after that fix: a guard whose rule set
/// is empty — because nothing was ever loaded, or because Neo4j legitimately
/// holds no rule — allows every tool. Ignored because refusing instead would
/// flip the gateway's default from permissive to restrictive, which is a product
/// decision, not a bug fix.
///
/// Faulty function: `<Neo4jPermissionProvider as CanUseTool>::can_use_tool`, its
/// `else` branch, which returns `Allow` and audits `"allow_default"`.
/// Triggering input: any `can_use_tool` call on a provider whose `rules_cache`
/// holds no matching rule.
#[tokio::test]
#[ignore = "fail-open default is a product decision, out of this agent's scope"]
async fn an_empty_rule_set_should_refuse_rather_than_allow() {
    let bolt = FakeBolt::start().await;
    bolt.returning_nothing();
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);
    provider.reload_rules().await.expect("reload");

    assert_eq!(verdict(&ask(&provider, "Bash").await), "deny");
}

// ---------------------------------------------------------------------------
// can_use_tool — the decision table
// ---------------------------------------------------------------------------

/// Nothing has ever been loaded: `find_matching_rule` finds no `"all"` entry at
/// all and the tool is allowed. A provider that is constructed but never
/// `reload_rules()`-ed is a no-op guard.
#[tokio::test]
async fn can_use_tool_allows_every_tool_before_the_first_reload() {
    let bolt = FakeBolt::start().await;
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);

    assert_eq!(verdict(&ask(&provider, "Bash").await), "allow");
    assert_eq!(verdict(&ask(&provider, "").await), "allow");
    assert!(bolt.runs().is_empty(), "no rule was ever fetched");
}

#[tokio::test]
async fn can_use_tool_allows_a_tool_matched_by_an_allow_rule() {
    let bolt = FakeBolt::start().await;
    bolt.returning("r", vec![global_rule("ok-read", "Read", "allow", "safe")]);
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);
    provider.reload_rules().await.expect("reload");

    match ask(&provider, "Read").await {
        PermissionResult::Allow(allow) => {
            assert!(
                allow.updated_input.is_none(),
                "the input is never rewritten"
            );
            assert!(allow.updated_permissions.is_none());
        },
        PermissionResult::Deny(_) => panic!("an allow rule denied the tool"),
    }
}

/// A rule the loaded set does not match falls through to the default
/// allowance — the guard is an allow-list only for the tools it names.
#[tokio::test]
async fn can_use_tool_allows_a_tool_no_loaded_rule_matches() {
    let bolt = FakeBolt::start().await;
    bolt.returning("r", vec![global_rule("deny-bash", "Bash", "deny", "nope")]);
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);
    provider.reload_rules().await.expect("reload");

    assert_eq!(verdict(&ask(&provider, "Bash").await), "deny");
    assert_eq!(
        verdict(&ask(&provider, "WebFetch").await),
        "allow",
        "an unlisted tool is allowed, not refused"
    );
}

/// `decision: "ask"` becomes an **unconditional allowance**. The comment in the
/// code says "let SDK handle asking", but `PermissionResult` has exactly two
/// variants, `Allow` and `Deny`: there is nothing downstream that can ask.
#[tokio::test]
async fn can_use_tool_turns_an_ask_rule_into_a_silent_allowance() {
    let bolt = FakeBolt::start().await;
    bolt.returning(
        "r",
        vec![global_rule("ask-bash", "Bash", "ask", "confirm first")],
    );
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);
    provider.reload_rules().await.expect("reload");

    assert_eq!(
        verdict(&ask(&provider, "Bash").await),
        "allow",
        "'ask' is not forwarded anywhere; it is an allowance"
    );
}

/// A `decision` the code does not recognise — a typo, the wrong case, a value
/// from a future schema — is an allowance too. A rule meant to forbid `Bash`,
/// stored as `"Deny"`, grants it.
#[tokio::test]
async fn can_use_tool_allows_a_tool_whose_rule_decision_is_malformed() {
    for decision in ["Deny", "DENY", "blocked", "refuse", ""] {
        let bolt = FakeBolt::start().await;
        bolt.returning(
            "r",
            vec![global_rule("typo", "Bash", decision, "meant to deny")],
        );
        let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);
        provider.reload_rules().await.expect("reload");

        assert_eq!(
            verdict(&ask(&provider, "Bash").await),
            "allow",
            "decision {decision:?} silently granted the tool"
        );
    }
}

/// Expresses what a guard should do with a rule it cannot interpret: refuse.
/// Ignored because the fix changes `can_use_tool`'s contract for `"ask"` as
/// well, which the SDK cannot represent yet.
///
/// Faulty function: `<Neo4jPermissionProvider as CanUseTool>::can_use_tool`,
/// `_ =>` arm of the `match decision`.
/// Triggering input: a rule whose `decision` is anything but `"allow"`/`"deny"`,
/// e.g. `"Deny"`.
#[tokio::test]
#[ignore = "unknown decisions allow today; refusing them is a contract change"]
async fn can_use_tool_should_refuse_a_rule_decision_it_cannot_interpret() {
    let bolt = FakeBolt::start().await;
    bolt.returning(
        "r",
        vec![global_rule("typo", "Bash", "Deny", "meant to deny")],
    );
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);
    provider.reload_rules().await.expect("reload");

    assert_eq!(verdict(&ask(&provider, "Bash").await), "deny");
}

/// `find_matching_rule` takes the first rule that matches, in the order Neo4j
/// returned them. `PermissionScope::priority()` — which documents Project as the
/// highest — is never consulted, so a broad global rule returned first wins over
/// a narrow project rule.
#[tokio::test]
async fn can_use_tool_takes_the_first_returned_rule_and_ignores_the_scope_priority() {
    let global_allow_all = rule_node(
        "global-allow",
        "*",
        "allow",
        pack_string("open bar"),
        "global",
        pack_null(),
        pack_int(0),
    );
    let project_deny_bash = rule_node(
        "project-deny",
        "Bash",
        "deny",
        pack_string("not in this project"),
        "project",
        pack_string("proj-1"),
        pack_int(100),
    );

    // Global first: it shadows the project rule even though the project scope is
    // documented as the highest priority.
    let bolt = FakeBolt::start().await;
    bolt.returning(
        "r",
        vec![global_allow_all.clone(), project_deny_bash.clone()],
    );
    let provider = Neo4jPermissionProvider::new(bolt.graph().await)
        .with_project("proj-1".to_string())
        .with_audit(false);
    provider.reload_rules().await.expect("reload");
    assert_eq!(
        verdict(&ask(&provider, "Bash").await),
        "allow",
        "row order, not scope, decided"
    );

    // Project first: the same two rules now deny.
    let bolt = FakeBolt::start().await;
    bolt.returning("r", vec![project_deny_bash, global_allow_all]);
    let provider = Neo4jPermissionProvider::new(bolt.graph().await)
        .with_project("proj-1".to_string())
        .with_audit(false);
    provider.reload_rules().await.expect("reload");
    let (message, _) = denial(&ask(&provider, "Bash").await);
    assert_eq!(message, "not in this project");
}

/// A `Bash(git:*)` rule is matched by *base name only*: it also covers
/// `Bash(curl …)`. An operator who allows a narrow command allows the whole
/// tool.
#[tokio::test]
async fn can_use_tool_widens_a_parenthesised_pattern_to_the_whole_tool() {
    let bolt = FakeBolt::start().await;
    bolt.returning(
        "r",
        vec![global_rule(
            "git-only",
            "Bash(git:*)",
            "allow",
            "git is fine",
        )],
    );
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);
    provider.reload_rules().await.expect("reload");

    assert_eq!(
        verdict(&ask(&provider, "Bash(curl http://example.invalid)").await),
        "allow",
        "the narrow pattern granted an unrelated Bash invocation"
    );
}

/// The desired behaviour of the same input: a `Bash(git:*)` rule should not
/// match `Bash(curl …)`. Ignored because a real glob matcher is a new
/// dependency and a behaviour change for every stored rule.
///
/// Faulty function: `PermissionRule::matches`, the
/// `tool_pattern.contains('(') && tool_pattern.contains(')')` branch that
/// returns `true` on the base name alone (the code comments it as
/// "Simplified").
/// Triggering input: pattern `"Bash(git:*)"`, tool name
/// `"Bash(curl http://example.invalid)"`.
#[test]
#[ignore = "glob matching is 'Simplified' in the code; a real matcher is out of scope"]
fn parenthesised_patterns_should_match_their_argument_too() {
    let rule = PermissionRule {
        id: "git-only".to_string(),
        tool_pattern: "Bash(git:*)".to_string(),
        decision: "allow".to_string(),
        reason: None,
        scope: PermissionScope::Global,
        priority: 0,
    };
    assert!(rule.matches("Bash(git:status)"));
    assert!(!rule.matches("Bash(curl http://example.invalid)"));
}

// ---------------------------------------------------------------------------
// log_audit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn log_audit_records_the_rule_that_decided_and_the_session() {
    let bolt = FakeBolt::start().await;
    bolt.returning("r", vec![global_rule("deny-bash", "Bash", "deny", "nope")]);
    let provider =
        Neo4jPermissionProvider::new(bolt.graph().await).with_session("sess-42".to_string());
    provider.reload_rules().await.expect("reload");
    bolt.forget_runs();

    assert_eq!(verdict(&ask(&provider, "Bash").await), "deny");

    let audit = bolt.run_matching("CREATE (a:NexusPermissionAudit");
    assert_eq!(audit.params["tool_name"], json!("Bash"));
    assert_eq!(audit.params["decision"], json!("deny"));
    assert_eq!(audit.params["rule_id"], json!("deny-bash"));
    assert_eq!(audit.params["session_id"], json!("sess-42"));
    assert!(
        audit.params["id"].as_str().is_some_and(|id| id.len() == 36),
        "the audit entry carries a fresh uuid: {:?}",
        audit.params["id"]
    );
    assert!(
        audit.params["now"]
            .as_str()
            .is_some_and(|t| t.contains('T'))
    );
}

/// A tool nobody wrote a rule for is audited as `"allow_default"` with an empty
/// `rule_id`, and with an empty `session_id` when no session was configured.
#[tokio::test]
async fn log_audit_marks_an_unmatched_tool_as_allow_default() {
    let bolt = FakeBolt::start().await;
    bolt.returning_nothing();
    let provider = Neo4jPermissionProvider::new(bolt.graph().await);
    provider.reload_rules().await.expect("reload");
    bolt.forget_runs();

    assert_eq!(verdict(&ask(&provider, "Bash").await), "allow");

    let audit = bolt.run_matching("CREATE (a:NexusPermissionAudit");
    assert_eq!(audit.params["decision"], json!("allow_default"));
    assert_eq!(audit.params["rule_id"], json!(""));
    assert_eq!(audit.params["session_id"], json!(""));
}

#[tokio::test]
async fn log_audit_writes_nothing_when_audit_is_disabled() {
    let bolt = FakeBolt::start().await;
    bolt.returning("r", vec![global_rule("deny-bash", "Bash", "deny", "nope")]);
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);
    provider.reload_rules().await.expect("reload");
    bolt.forget_runs();

    assert_eq!(verdict(&ask(&provider, "Bash").await), "deny");
    assert_eq!(verdict(&ask(&provider, "Unlisted").await), "allow");

    assert_eq!(
        bolt.count_runs_matching("NexusPermissionAudit"),
        0,
        "with_audit(false) must not write audit nodes"
    );
}

/// A refused tool stays refused when the audit write fails: the audit error is
/// only `warn!`-logged. The decision is right, but the audit trail is lost
/// silently — see the report.
#[tokio::test]
async fn a_failing_audit_write_does_not_change_the_decision() {
    let bolt = FakeBolt::start().await;
    bolt.returning("r", vec![global_rule("deny-bash", "Bash", "deny", "nope")]);
    let provider =
        Neo4jPermissionProvider::new(bolt.graph().await).with_session("sess-7".to_string());
    provider.reload_rules().await.expect("reload");
    bolt.failing_only("NexusPermissionAudit", "audit store is read-only");
    bolt.forget_runs();

    let (message, _) = denial(&ask(&provider, "Bash").await);

    assert_eq!(message, "nope");
    assert_eq!(
        bolt.count_runs_matching("NexusPermissionAudit"),
        1,
        "the audit write was attempted and refused"
    );
}

// ---------------------------------------------------------------------------
// add_rule
// ---------------------------------------------------------------------------

#[tokio::test]
async fn add_rule_writes_every_property_and_defaults_a_missing_reason_to_empty() {
    let bolt = FakeBolt::start().await;
    let provider = Neo4jPermissionProvider::new(bolt.graph().await);

    provider
        .add_rule(PermissionRule {
            id: "rule-1".to_string(),
            tool_pattern: "Bash*".to_string(),
            decision: "deny".to_string(),
            reason: None,
            scope: PermissionScope::Global,
            priority: 7,
        })
        .await
        .expect("add");

    let run = bolt.run_matching("CREATE (r:NexusPermissionRule");
    assert_eq!(run.params["id"], json!("rule-1"));
    assert_eq!(run.params["tool_pattern"], json!("Bash*"));
    assert_eq!(run.params["decision"], json!("deny"));
    assert_eq!(
        run.params["reason"],
        json!(""),
        "None becomes \"\", not null"
    );
    assert_eq!(run.params["scope"], json!("global"));
    assert_eq!(
        run.params["scope_id"],
        json!(""),
        "a global rule stores an empty scope_id, not null"
    );
    assert_eq!(run.params["priority"], json!(7));
}

#[tokio::test]
async fn add_rule_stores_the_workspace_and_project_scope_ids() {
    for (scope, expected_scope, expected_id) in [
        (
            PermissionScope::Workspace("ws-9".to_string()),
            "workspace",
            "ws-9",
        ),
        (
            PermissionScope::Project("proj-9".to_string()),
            "project",
            "proj-9",
        ),
    ] {
        let bolt = FakeBolt::start().await;
        let provider = Neo4jPermissionProvider::new(bolt.graph().await);

        provider
            .add_rule(PermissionRule {
                id: format!("rule-{expected_scope}"),
                tool_pattern: "Read".to_string(),
                decision: "allow".to_string(),
                reason: Some("scoped".to_string()),
                scope,
                priority: 0,
            })
            .await
            .expect("add");

        let run = bolt.run_matching("CREATE (r:NexusPermissionRule");
        assert_eq!(run.params["scope"], json!(expected_scope));
        assert_eq!(run.params["scope_id"], json!(expected_id));
        assert_eq!(run.params["reason"], json!("scoped"));
    }
}

/// `add_rule` empties `rules_cache` on success but does not reload it, so every
/// rule — including the deny rules — stops applying until someone calls
/// `reload_rules` again. Adding a rule therefore *opens* the guard.
#[tokio::test]
async fn add_rule_empties_the_cache_and_leaves_the_guard_permissive() {
    let bolt = FakeBolt::start().await;
    bolt.returning("r", vec![global_rule("deny-bash", "Bash", "deny", "nope")]);
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);
    provider.reload_rules().await.expect("reload");
    assert_eq!(verdict(&ask(&provider, "Bash").await), "deny");

    provider
        .add_rule(PermissionRule {
            id: "unrelated".to_string(),
            tool_pattern: "Read".to_string(),
            decision: "allow".to_string(),
            reason: None,
            scope: PermissionScope::Global,
            priority: 0,
        })
        .await
        .expect("add");

    assert_eq!(
        verdict(&ask(&provider, "Bash").await),
        "allow",
        "adding an unrelated rule dropped the deny rule"
    );
}

/// The mirror image: when the `CREATE` fails the cache is left alone, because
/// the invalidation sits after the `?`.
#[tokio::test]
async fn add_rule_propagates_the_failure_and_keeps_the_loaded_rules() {
    let bolt = FakeBolt::start().await;
    bolt.returning("r", vec![global_rule("deny-bash", "Bash", "deny", "nope")]);
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);
    provider.reload_rules().await.expect("reload");

    bolt.failing_only("CREATE (r:NexusPermissionRule", "constraint violated");
    let error = provider
        .add_rule(PermissionRule {
            id: "deny-bash".to_string(),
            tool_pattern: "Bash".to_string(),
            decision: "deny".to_string(),
            reason: None,
            scope: PermissionScope::Global,
            priority: 0,
        })
        .await
        .expect_err("duplicate id");
    assert!(error.to_string().contains("constraint violated"), "{error}");

    bolt.healed();
    assert_eq!(verdict(&ask(&provider, "Bash").await), "deny");
}

// ---------------------------------------------------------------------------
// remove_rule
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remove_rule_reports_the_deletion_and_invalidates_the_cache() {
    let bolt = FakeBolt::start().await;
    bolt.returning("r", vec![global_rule("deny-bash", "Bash", "deny", "nope")]);
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);
    provider.reload_rules().await.expect("reload");
    assert_eq!(verdict(&ask(&provider, "Bash").await), "deny");

    bolt.returning("deleted", vec![pack_int(1)]);
    assert!(provider.remove_rule("deny-bash").await.expect("remove"));

    let run = bolt.run_matching("DELETE r");
    assert!(
        run.cypher
            .contains("MATCH (r:NexusPermissionRule {id: $id})")
    );
    assert_eq!(run.params["id"], json!("deny-bash"));
    assert_eq!(
        verdict(&ask(&provider, "Bash").await),
        "allow",
        "the removed rule stops applying"
    );
}

/// `deleted = 0`: nothing matched, so the answer is `false` and the loaded rules
/// are deliberately left in place.
#[tokio::test]
async fn remove_rule_reports_false_when_no_rule_matched_and_keeps_the_cache() {
    let bolt = FakeBolt::start().await;
    bolt.returning("r", vec![global_rule("deny-bash", "Bash", "deny", "nope")]);
    let provider = Neo4jPermissionProvider::new(bolt.graph().await).with_audit(false);
    provider.reload_rules().await.expect("reload");

    bolt.returning("deleted", vec![pack_int(0)]);
    assert!(!provider.remove_rule("ghost").await.expect("remove"));

    assert_eq!(verdict(&ask(&provider, "Bash").await), "deny");
}

/// And when the query returns no row at all, rather than a zero count.
#[tokio::test]
async fn remove_rule_reports_false_when_the_query_returns_no_row() {
    let bolt = FakeBolt::start().await;
    bolt.returning("deleted", Vec::new());
    let provider = Neo4jPermissionProvider::new(bolt.graph().await);

    assert!(!provider.remove_rule("ghost").await.expect("remove"));
}

#[tokio::test]
async fn remove_rule_propagates_the_graph_failure() {
    let bolt = FakeBolt::start().await;
    bolt.failing("graph is down");
    let provider = Neo4jPermissionProvider::new(bolt.graph().await);

    let error = provider.remove_rule("deny-bash").await.expect_err("down");
    assert!(error.to_string().contains("graph is down"), "{error}");
}

/// The `deleted` column is read as `i64`; a row carrying something else is an
/// error rather than a silent `false`.
#[tokio::test]
async fn remove_rule_rejects_a_non_numeric_deleted_count() {
    let bolt = FakeBolt::start().await;
    bolt.returning("deleted", vec![pack_string("one")]);
    let provider = Neo4jPermissionProvider::new(bolt.graph().await);

    provider
        .remove_rule("deny-bash")
        .await
        .expect_err("the count must be an integer");
}

// ---------------------------------------------------------------------------
// list_rules
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_rules_without_a_scope_lists_everything_unfiltered() {
    let bolt = FakeBolt::start().await;
    bolt.returning(
        "r",
        vec![
            rule_node(
                "p",
                "Bash",
                "deny",
                pack_string("shell off"),
                "project",
                pack_string("proj-1"),
                pack_int(100),
            ),
            rule_node(
                "w",
                "Read",
                "allow",
                pack_null(),
                "workspace",
                pack_string("ws-1"),
                pack_int(50),
            ),
        ],
    );
    let provider = Neo4jPermissionProvider::new(bolt.graph().await);

    let rules = provider.list_rules(None).await.expect("list");

    let run = bolt.run_matching("MATCH (r:NexusPermissionRule)");
    assert!(!run.cypher.contains("WHERE"), "no filter: {}", run.cypher);
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].id, "p");
    assert_eq!(rules[0].tool_pattern, "Bash");
    assert_eq!(rules[0].decision, "deny");
    assert_eq!(rules[0].reason.as_deref(), Some("shell off"));
    assert_eq!(
        rules[0].scope,
        PermissionScope::Project("proj-1".to_string())
    );
    assert_eq!(rules[0].priority, 100);
    assert_eq!(
        rules[1].scope,
        PermissionScope::Workspace("ws-1".to_string())
    );
    assert_eq!(rules[1].reason, None);
}

#[tokio::test]
async fn list_rules_filters_on_the_scope_and_its_id() {
    for (scope, expected_scope, expected_id) in [
        (PermissionScope::Global, "global", ""),
        (
            PermissionScope::Workspace("ws-1".to_string()),
            "workspace",
            "ws-1",
        ),
        (
            PermissionScope::Project("proj-1".to_string()),
            "project",
            "proj-1",
        ),
    ] {
        let bolt = FakeBolt::start().await;
        bolt.returning_nothing();
        let provider = Neo4jPermissionProvider::new(bolt.graph().await);

        let rules = provider.list_rules(Some(scope)).await.expect("list");

        assert!(rules.is_empty());
        let run = bolt.run_matching("MATCH (r:NexusPermissionRule)");
        assert!(
            run.cypher.contains(
                "WHERE r.scope = $scope AND (r.scope_id = $scope_id OR r.scope_id IS NULL)"
            ),
            "{}",
            run.cypher
        );
        assert_eq!(run.params["scope"], json!(expected_scope));
        assert_eq!(run.params["scope_id"], json!(expected_id));
    }
}

#[tokio::test]
async fn list_rules_propagates_the_graph_failure() {
    let bolt = FakeBolt::start().await;
    bolt.failing("graph is down");
    let provider = Neo4jPermissionProvider::new(bolt.graph().await);

    let error = provider.list_rules(None).await.expect_err("down");
    assert!(error.to_string().contains("graph is down"), "{error}");
}

#[tokio::test]
async fn list_rules_rejects_a_rule_whose_tool_pattern_is_missing() {
    let bolt = FakeBolt::start().await;
    bolt.returning(
        "r",
        vec![pack_node(
            1,
            &["NexusPermissionRule"],
            &[
                ("id", pack_string("broken")),
                ("decision", pack_string("deny")),
            ],
        )],
    );
    let provider = Neo4jPermissionProvider::new(bolt.graph().await);

    provider
        .list_rules(None)
        .await
        .expect_err("a rule with no tool_pattern is not loadable");
}
