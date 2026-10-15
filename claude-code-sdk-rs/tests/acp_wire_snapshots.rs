//! Frozen JSON shapes of the ACP wire (Agent Client Protocol, protocol v1).
//!
//! `tests/snapshots/acp_wire_v<N>.json` holds, for every message the adapter
//! *writes*, the exact `wire::canonical_line` bytes `transport::Process::write`
//! puts on the agent's stdio — with one exception: `initialize.clientInfo.version`
//! is normalised to [`NORMALISED_CLIENT_VERSION`] (`0.0.0`), not the live crate
//! version — plus the pure inbound→event projection of `providers::acp::map`.
//! The file name carries `SUPPORTED_PROTOCOL_VERSION`
//! (which is `wire::PROTOCOL_VERSION`): changing a wire shape without bumping
//! the version fails the compare, and bumping the version requires a new file,
//! so a wire change can never slip through as an incidental diff.
//!
//! This is a separate golden from `agent_contract_v<N>.json` on purpose
//! (decision D1): `agent::CONTRACT_VERSION` governs `AgentEvent` shapes and
//! `wire::PROTOCOL_VERSION` governs the ACP wire; a bump of one must not force a
//! re-golden of the other.
//!
//! Regenerate with:
//!
//! ```text
//! UPDATE_ACP_WIRE_SNAPSHOTS=1 cargo test -p nexus-claude --features provider-acp --test acp_wire_snapshots
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use nexus_claude::agent::{AgentEvent, CostBasis, PermissionDecision};
use nexus_claude::model::PriceTable;
use nexus_claude::providers::acp::SUPPORTED_PROTOCOL_VERSION;
use nexus_claude::providers::acp::map::{self, MapConfig, MapState};
use nexus_claude::providers::acp::wire::{
    self, CancelParams, ChunkUpdate, CommandsUpdate, ContentIn, HttpMcpServer, InitializeParams,
    LoadSessionParams, McpServer, ModeUpdate, NameValue, NewSessionParams, PermissionOption,
    PlanUpdate, PromptParams, PromptResult, RpcError, SetModeParams, StdioMcpServer, TextBlock,
    ToolCallUpdateIn, Update, UsageUpdate,
};
use serde_json::{Value, json};

/// The outbound message kinds the golden freezes. Every entry of `outbound`
/// must be here, and every name here must have an entry: adding a new outbound
/// method (e.g. `session/set_model`) without a golden entry fails a test.
const OUTBOUND_KINDS: [&str; 10] = [
    "initialize.params",
    "session/new.params",
    "session/load.params",
    "session/prompt.params",
    "session/cancel.params",
    "session/set_mode.params",
    "session/request_permission.response.selected",
    "session/request_permission.response.cancelled",
    "error_response.method_not_found",
    "error_response.invalid_params",
];

/// The `clientInfo.version` the golden normalises the crate version to: a
/// release must not regenerate the wire golden just because the package version
/// changed (decision D4).
const NORMALISED_CLIENT_VERSION: &str = "0.0.0";

fn wire_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("a wire type serialises")
}

/// `initialize.params` with the crate version normalised out. The real adapter
/// sends `env!("CARGO_PKG_VERSION")` (asserted by its own test); the golden
/// locks every other byte, so a release does not regenerate it.
fn initialize_params() -> Value {
    let mut params = InitializeParams::for_adapter();
    params.client_info.version = NORMALISED_CLIENT_VERSION.to_owned();
    wire_value(&params)
}

/// The golden `outbound` section: one canonical JSON-RPC line per message the
/// adapter writes, with fixed ids and fixed scalars so the bytes are
/// deterministic. MCP values are non-secret only (the fake masks credentials).
fn outbound() -> BTreeMap<&'static str, Value> {
    let line = |message: Value| json!(wire::canonical_line(&message));
    let mut out = BTreeMap::new();

    out.insert(
        "initialize.params",
        line(wire::request_line(1, "initialize", initialize_params())),
    );

    let session_new = NewSessionParams {
        cwd: "/work".to_owned(),
        mcp_servers: vec![
            McpServer::Stdio(StdioMcpServer {
                name: "po".to_owned(),
                command: "/bin/po-mcp".to_owned(),
                args: vec!["--stdio".to_owned()],
                env: vec![NameValue {
                    name: "LEVEL".to_owned(),
                    value: "1".to_owned(),
                }],
            }),
            McpServer::Http(HttpMcpServer {
                r#type: "http".to_owned(),
                name: "remote".to_owned(),
                url: "https://mcp.example/api".to_owned(),
                headers: vec![NameValue {
                    name: "X-Tenant".to_owned(),
                    value: "acme".to_owned(),
                }],
            }),
        ],
    };
    out.insert(
        "session/new.params",
        line(wire::request_line(
            2,
            "session/new",
            wire_value(&session_new),
        )),
    );

    let session_load = LoadSessionParams {
        session_id: "sess_prev".to_owned(),
        cwd: "/work".to_owned(),
        mcp_servers: Vec::new(),
    };
    out.insert(
        "session/load.params",
        line(wire::request_line(
            3,
            "session/load",
            wire_value(&session_load),
        )),
    );

    let prompt = PromptParams {
        session_id: "sess_fake1".to_owned(),
        prompt: vec![TextBlock {
            r#type: "text".to_owned(),
            text: "go".to_owned(),
        }],
    };
    out.insert(
        "session/prompt.params",
        line(wire::request_line(4, "session/prompt", wire_value(&prompt))),
    );

    // `session/cancel` is a notification: no `id`.
    let cancel = CancelParams {
        session_id: "sess_fake1".to_owned(),
    };
    out.insert(
        "session/cancel.params",
        line(wire::notification_line(
            "session/cancel",
            wire_value(&cancel),
        )),
    );

    let set_mode = SetModeParams {
        session_id: "sess_fake1".to_owned(),
        mode_id: "acceptEdits".to_owned(),
    };
    out.insert(
        "session/set_mode.params",
        line(wire::request_line(
            5,
            "session/set_mode",
            wire_value(&set_mode),
        )),
    );

    out.insert(
        "session/request_permission.response.selected",
        line(wire::response_line(
            &json!("perm_1"),
            wire::permission_selected("allow-once"),
        )),
    );
    out.insert(
        "session/request_permission.response.cancelled",
        line(wire::response_line(
            &json!("perm_1"),
            wire::permission_cancelled(),
        )),
    );

    out.insert(
        "error_response.method_not_found",
        line(wire::error_response_line(
            &json!("s1"),
            -32601,
            "method not found: this client serves no fs or terminal",
        )),
    );
    out.insert(
        "error_response.invalid_params",
        line(wire::error_response_line(
            &json!("s1"),
            -32602,
            "invalid params",
        )),
    );

    out
}

/// The fixed `MapConfig` of the golden projections. `thinking: true` except for
/// the not-declared case, which uses a second config.
fn map_config(thinking: bool) -> MapConfig {
    MapConfig {
        deltas: false,
        cost_basis: CostBasis::Unknown,
        prices: PriceTable::new(),
        login_hint: None,
        mcp_servers: vec!["po".to_owned()],
        thinking,
    }
}

fn chunk(text: &str) -> ChunkUpdate {
    ChunkUpdate {
        content: ContentIn {
            r#type: Some("text".to_owned()),
            text: Some(text.to_owned()),
        },
        message_id: None,
    }
}

/// Runs `map_update` on a fresh state and serialises the events it emits.
fn project(thinking: bool, updates: &[Update]) -> Value {
    let mut state = MapState::new(map_config(thinking));
    state.begin_turn();
    let mut events = Vec::new();
    for update in updates {
        events.extend(state.map_update(update.clone()));
    }
    serde_json::to_value(&events).expect("AgentEvent serialises")
}

/// Like [`project`], but flushes the buffered chunks with a clean `finish` and
/// drops the terminal `Done`: its `duration_ms` is wall-clock (`Instant::now`),
/// not a golden shape (decision D3). Used for the chunk cases, whose coalesced
/// `text`/`thinking` is only emitted by a flush.
fn project_flushed(thinking: bool, updates: &[Update]) -> Value {
    let mut state = MapState::new(map_config(thinking));
    state.begin_turn();
    let mut events = Vec::new();
    for update in updates {
        events.extend(state.map_update(update.clone()));
    }
    events.extend(state.finish(Ok(PromptResult {
        stop_reason: "end_turn".to_owned(),
        usage: None,
    })));
    events.retain(|event| !matches!(event, AgentEvent::Done { .. }));
    serde_json::to_value(&events).expect("AgentEvent serialises")
}

fn stop_reason_value(reason: &str) -> Value {
    let (stop_reason, is_error, error) = map::stop_of(reason);
    json!({ "stop_reason": stop_reason, "is_error": is_error, "error": error })
}

fn rpc_error_value(code: i64, message: &str) -> Value {
    serde_json::to_value(map::classify_rpc_error(&RpcError {
        code,
        message: message.to_owned(),
        detail: None,
    }))
    .expect("ProviderError serialises")
}

fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption {
            option_id: "allow-once".to_owned(),
            name: "Allow once".to_owned(),
            kind: "allow_once".to_owned(),
        },
        PermissionOption {
            option_id: "reject-once".to_owned(),
            name: "Reject once".to_owned(),
            kind: "reject_once".to_owned(),
        },
    ]
}

fn permission_answer(options: &[PermissionOption], decision: &PermissionDecision) -> Value {
    map::answer_for(options, decision)
        .expect("a supported decision")
        .0
}

fn tool_description_value(name: &str, kind: Option<&str>, mcp_servers: &[String]) -> Value {
    let (category, canonical) = map::describe_tool(name, kind, mcp_servers);
    json!([category, canonical])
}

/// The golden `response_mapping` section: the pure inbound→event projection,
/// no I/O, no wall clock (decision D3).
fn response_mapping() -> BTreeMap<&'static str, Value> {
    let mut mapping = BTreeMap::new();

    let mut update = BTreeMap::new();
    // Two chunks coalesce into one `text` (only a flush emits it; the terminal
    // `Done` is dropped, see `project_flushed`).
    update.insert(
        "agent_message_chunk",
        project_flushed(
            true,
            &[
                Update::AgentMessageChunk(chunk("Hel")),
                Update::AgentMessageChunk(chunk("lo")),
            ],
        ),
    );
    update.insert(
        "agent_thought_chunk",
        project_flushed(true, &[Update::AgentThoughtChunk(chunk("hmm"))]),
    );
    // Thinking not declared: the chunk is dropped, one notice instead.
    update.insert(
        "agent_thought_chunk_not_declared",
        project(false, &[Update::AgentThoughtChunk(chunk("hmm"))]),
    );
    update.insert(
        "user_message_chunk",
        project(true, &[Update::UserMessageChunk(chunk("hi"))]),
    );
    update.insert(
        "tool_call",
        project(
            true,
            &[Update::ToolCall(ToolCallUpdateIn {
                tool_call_id: "c1".to_owned(),
                title: Some("ls".to_owned()),
                kind: Some("execute".to_owned()),
                status: Some("completed".to_owned()),
                raw_input: Some(json!({"command": "ls"})),
                raw_output: None,
                content: Some(vec![json!({
                    "type": "content",
                    "content": {"type": "text", "text": "a.txt"},
                })]),
            })],
        ),
    );
    update.insert(
        "tool_call_update",
        project(
            true,
            &[Update::ToolCallUpdate(ToolCallUpdateIn {
                tool_call_id: "c2".to_owned(),
                title: Some("po_write".to_owned()),
                kind: Some("other".to_owned()),
                status: Some("failed".to_owned()),
                raw_input: None,
                raw_output: None,
                content: Some(vec![json!({
                    "type": "content",
                    "content": {"type": "text", "text": "boom"},
                })]),
            })],
        ),
    );
    update.insert(
        "plan",
        project(
            true,
            &[Update::Plan(PlanUpdate {
                entries: vec![json!({"content": "do it", "priority": 1, "status": "pending"})],
            })],
        ),
    );
    update.insert(
        "available_commands_update",
        project(
            true,
            &[Update::AvailableCommands(CommandsUpdate {
                available_commands: vec![json!({"name": "build", "description": "build it"})],
            })],
        ),
    );
    update.insert(
        "current_mode_update",
        project(
            true,
            &[Update::CurrentMode(ModeUpdate {
                current_mode_id: "plan".to_owned(),
            })],
        ),
    );
    // `usage_update` emits nothing: it is recorded into `done.usage.context_tokens`.
    update.insert(
        "usage_update",
        project(
            true,
            &[Update::Usage(UsageUpdate {
                used: Some(42),
                size: Some(200_000),
            })],
        ),
    );
    mapping.insert("update", json!(update));

    let mut stop_reason = BTreeMap::new();
    for reason in [
        "end_turn",
        "max_tokens",
        "max_turn_requests",
        "refusal",
        "cancelled",
        "unknown",
    ] {
        stop_reason.insert(reason, stop_reason_value(reason));
    }
    mapping.insert("stop_reason", json!(stop_reason));

    let mut rpc_error = BTreeMap::new();
    rpc_error.insert(
        "auth_required",
        rpc_error_value(-32000, "Authentication required"),
    );
    rpc_error.insert("invalid_request", rpc_error_value(-32602, "bad params"));
    rpc_error.insert(
        "rate_limited",
        rpc_error_value(-32603, "Rate limit exceeded"),
    );
    rpc_error.insert("overloaded", rpc_error_value(-32603, "overloaded"));
    rpc_error.insert(
        "context_too_small",
        rpc_error_value(-32603, "context length"),
    );
    rpc_error.insert("unauthorized", rpc_error_value(-32603, "Unauthorized"));
    rpc_error.insert("endpoint_unreachable", rpc_error_value(-32603, "network"));
    rpc_error.insert("protocol", rpc_error_value(-32603, "whatever"));
    mapping.insert("rpc_error", json!(rpc_error));

    let options = permission_options();
    let allow_once_only = vec![options[0].clone()];
    let mut permission = BTreeMap::new();
    permission.insert(
        "allow_once",
        permission_answer(&options, &PermissionDecision::allow_once()),
    );
    permission.insert(
        "deny",
        permission_answer(&options, &PermissionDecision::deny()),
    );
    // Deny with no reject option answers `cancelled`, not a selected option.
    permission.insert(
        "cancelled",
        permission_answer(&allow_once_only, &PermissionDecision::deny()),
    );
    mapping.insert("permission", json!(permission));

    let no_servers: Vec<String> = Vec::new();
    let po_servers = vec!["po".to_owned()];
    let mut tool_description = BTreeMap::new();
    for (key, kind) in [
        ("read", "read"),
        ("edit", "edit"),
        ("delete", "delete"),
        ("move", "move"),
        ("execute", "execute"),
        ("search", "search"),
        ("fetch", "fetch"),
        ("other", "other"),
    ] {
        tool_description.insert(
            key,
            tool_description_value("tool title", Some(kind), &no_servers),
        );
    }
    // opencode's `<server>_<tool>` title spelling maps to `mcp__<server>__<tool>`.
    tool_description.insert(
        "opencode",
        tool_description_value("po_write", None, &po_servers),
    );
    mapping.insert("tool_description", json!(tool_description));

    mapping
}

fn current_snapshot() -> Value {
    json!({
        "protocol_version": SUPPORTED_PROTOCOL_VERSION,
        "outbound": outbound(),
        "response_mapping": response_mapping(),
    })
}

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(format!("acp_wire_v{SUPPORTED_PROTOCOL_VERSION}.json"))
}

/// The negative contract: the whole file compares byte-for-byte against the
/// committed golden. No `.contains()`, no partial matching.
#[test]
fn shapes_match_the_committed_acp_wire_golden() {
    let current = serde_json::to_string_pretty(&current_snapshot()).unwrap() + "\n";
    let path = snapshot_path();
    if std::env::var_os("UPDATE_ACP_WIRE_SNAPSHOTS").is_some() {
        std::fs::write(&path, &current).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "{} is missing: SUPPORTED_PROTOCOL_VERSION changed (or this is the first run). \
             Review the wire shapes, then regenerate with UPDATE_ACP_WIRE_SNAPSHOTS=1.",
            path.display()
        )
    });
    assert_eq!(
        committed.replace("\r\n", "\n"),
        current,
        "a serialised ACP wire shape changed: regenerate the snapshot \
         (UPDATE_ACP_WIRE_SNAPSHOTS=1)"
    );
}

/// Every outbound message kind has a golden entry, and nothing else: adding a
/// new outbound method without a golden entry fails this test.
#[test]
fn every_outbound_message_kind_has_a_golden_entry() {
    let golden = current_snapshot();
    let outbound = golden["outbound"]
        .as_object()
        .expect("outbound is an object");
    let actual: BTreeSet<&str> = outbound.keys().map(String::as_str).collect();
    let expected: BTreeSet<&str> = OUTBOUND_KINDS.iter().copied().collect();
    assert_eq!(
        actual, expected,
        "the golden outbound keys must equal the closed list in OUTBOUND_KINDS"
    );
}

/// The golden is keyed by `wire::PROTOCOL_VERSION` (through
/// `SUPPORTED_PROTOCOL_VERSION`), not by `agent::CONTRACT_VERSION`: a protocol
/// bump requires a new `acp_wire_vN.json`, and the read of the old name panics.
#[test]
fn the_golden_is_keyed_by_the_supported_protocol_version() {
    let golden = current_snapshot();
    assert_eq!(
        golden["protocol_version"],
        json!(SUPPORTED_PROTOCOL_VERSION),
        "the golden carries the SUPPORTED_PROTOCOL_VERSION it was generated for"
    );
    let name = snapshot_path()
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a file name")
        .to_owned();
    assert_eq!(
        name,
        format!("acp_wire_v{SUPPORTED_PROTOCOL_VERSION}.json"),
        "the golden file name embeds SUPPORTED_PROTOCOL_VERSION"
    );
}

/// `initialize.clientInfo.version` is the crate version, never the golden's
/// `0.0.0`: the golden normalises it out (decision D4), but the adapter sends
/// the real one.
#[test]
fn the_initialize_version_is_the_crate_version_not_the_golden() {
    assert_eq!(
        InitializeParams::for_adapter().client_info.version,
        env!("CARGO_PKG_VERSION"),
        "the adapter announces the crate version in initialize.clientInfo.version"
    );
    let golden = current_snapshot();
    let line = golden["outbound"]["initialize.params"]
        .as_str()
        .expect("a canonical line");
    let parsed: Value = serde_json::from_str(line).expect("the line is JSON");
    assert_eq!(
        parsed["params"]["clientInfo"]["version"].as_str(),
        Some(NORMALISED_CLIENT_VERSION),
        "the golden normalises the crate version out so a release does not regenerate it"
    );
}
