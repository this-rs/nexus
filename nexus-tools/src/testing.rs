//! Two tools to exercise the server end to end before the real tools exist. Behind the
//! `test-tools` feature: never part of a release build of the harness.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tool::{Annotations, CallContext, Tool, ToolResult};

/// `echo {text}`: read-only, answers `echo: <text>`.
#[derive(Debug, Default)]
pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Answers with the text it is given."
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
    }

    fn annotations(&self) -> Annotations {
        Annotations::read_only()
    }

    async fn call(&self, _context: &CallContext, arguments: Value) -> ToolResult {
        match arguments.get("text").and_then(Value::as_str) {
            Some(text) => ToolResult::ok(format!("echo: {text}")),
            None => ToolResult::error("echo needs a `text` string"),
        }
    }
}

/// `write {text}`: not read-only, answers `write ok: <text>`.
#[derive(Debug, Default)]
pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Pretends to write; changes nothing."
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }

    fn annotations(&self) -> Annotations {
        Annotations {
            destructive: true,
            ..Annotations::default()
        }
    }

    async fn call(&self, _context: &CallContext, arguments: Value) -> ToolResult {
        let text = arguments
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        ToolResult::ok(format!("write ok: {text}"))
    }
}
