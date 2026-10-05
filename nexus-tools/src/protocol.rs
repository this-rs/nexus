//! JSON-RPC 2.0 and the part of MCP this server speaks.

use serde_json::{Value, json};

/// The protocol version this server prefers (the one the harness's client sends).
pub const PROTOCOL_VERSION: &str = "2025-03-26";

/// Versions the server accepts from a client; anything else is answered with
/// [`PROTOCOL_VERSION`] and the client decides whether it can live with that.
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-03-26", "2024-11-05"];

/// JSON-RPC error codes.
pub mod code {
    /// Invalid JSON.
    pub const PARSE_ERROR: i64 = -32700;
    /// The message is not a valid request.
    pub const INVALID_REQUEST: i64 = -32600;
    /// No such method.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid or unknown parameters, including an unknown or forbidden tool.
    pub const INVALID_PARAMS: i64 = -32602;
    /// Server-side failure.
    pub const INTERNAL_ERROR: i64 = -32603;
}

/// `{"jsonrpc":"2.0","id":…,"result":…}`.
pub fn result_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// `{"jsonrpc":"2.0","id":…,"error":{code,message}}`.
pub fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

/// The version to answer an `initialize` with.
pub fn negotiate(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|wanted| SUPPORTED_VERSIONS.iter().find(|v| **v == wanted).copied())
        .unwrap_or(PROTOCOL_VERSION)
}

/// A request id as a stable string key (ids are numbers or strings).
pub fn id_key(id: &Value) -> String {
    match id {
        Value::String(text) => format!("s:{text}"),
        other => format!("n:{other}"),
    }
}
