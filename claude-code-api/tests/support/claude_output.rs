//! Builders for the `stream-json` messages the Claude CLI emits.
//!
//! These are the inputs of everything downstream of the subprocess:
//! `handle_non_streaming_response`, `handle_enhanced_streaming_response` and the
//! interactive-session collectors all consume
//! `mpsc::Receiver<ClaudeCodeOutput>`. Feeding that channel directly (see
//! [`channel`]) is the cheapest way to cover those code paths — no process, no
//! timing, fully deterministic.

use claude_code_api::models::claude::ClaudeCodeOutput;
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// An `assistant` message carrying a single `text` content block.
pub fn assistant_text(text: &str) -> ClaudeCodeOutput {
    assistant_blocks(vec![json!({"type": "text", "text": text})])
}

/// An `assistant` message carrying a single `tool_use` content block.
pub fn assistant_tool_use(id: &str, name: &str, input: Value) -> ClaudeCodeOutput {
    assistant_blocks(vec![json!({
        "type": "tool_use",
        "id": id,
        "name": name,
        "input": input,
    })])
}

/// An `assistant` message carrying a `tool_result` block (informational: the
/// handlers log it and emit nothing).
pub fn assistant_tool_result(tool_use_id: &str, content: &str) -> ClaudeCodeOutput {
    assistant_blocks(vec![json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id,
        "content": content,
    })])
}

/// An `assistant` message with an arbitrary list of content blocks.
pub fn assistant_blocks(blocks: Vec<Value>) -> ClaudeCodeOutput {
    ClaudeCodeOutput {
        r#type: "assistant".to_string(),
        subtype: None,
        data: json!({
            "message": {
                "role": "assistant",
                "content": blocks,
            }
        }),
    }
}

/// An `assistant` message tagged with `parent_tool_use_id`, i.e. coming from a
/// subagent sidechain. Every handler must drop these.
pub fn sidechain_text(parent_tool_use_id: &str, text: &str) -> ClaudeCodeOutput {
    ClaudeCodeOutput {
        r#type: "assistant".to_string(),
        subtype: None,
        data: json!({
            "parent_tool_use_id": parent_tool_use_id,
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": text}],
            }
        }),
    }
}

/// The terminal `result` message that closes a turn.
pub fn result_success() -> ClaudeCodeOutput {
    ClaudeCodeOutput {
        r#type: "result".to_string(),
        subtype: Some("success".to_string()),
        data: json!({
            "is_error": false,
            "num_turns": 1,
            "duration_ms": 1,
        }),
    }
}

/// A `result` message flagged as an error.
pub fn result_error(message: &str) -> ClaudeCodeOutput {
    ClaudeCodeOutput {
        r#type: "result".to_string(),
        subtype: Some("error".to_string()),
        data: json!({"is_error": true, "error": message}),
    }
}

/// A message type no handler knows about — exercises the `_ => {}` arms.
pub fn unknown(kind: &str) -> ClaudeCodeOutput {
    ClaudeCodeOutput {
        r#type: kind.to_string(),
        subtype: None,
        data: json!({"note": "unhandled output type"}),
    }
}

/// Serialise a transcript to newline-delimited JSON, the wire format the CLI
/// writes on stdout and [`crate::support::fake_cli::FakeClaudeCli`] replays.
pub fn ndjson(outputs: &[ClaudeCodeOutput]) -> String {
    let mut out = String::new();
    for output in outputs {
        out.push_str(&serde_json::to_string(output).expect("serialize ClaudeCodeOutput"));
        out.push('\n');
    }
    out
}

/// A closed `mpsc::Receiver` pre-loaded with `outputs`.
///
/// This is the substitute for a CLI subprocess when testing the response
/// assemblers directly:
///
/// ```no_run
/// let rx = claude_output::channel(&[
///     claude_output::assistant_text("hi"),
///     claude_output::result_success(),
/// ]);
/// let chunks = sse::collect(handle_enhanced_streaming_response(
///     "m".into(), rx, None, None,
/// ).await).await;
/// ```
pub fn channel(outputs: &[ClaudeCodeOutput]) -> mpsc::Receiver<ClaudeCodeOutput> {
    let (tx, rx) = mpsc::channel(outputs.len().max(1));
    for output in outputs {
        tx.try_send(output.clone()).expect("preload channel");
    }
    drop(tx);
    rx
}
