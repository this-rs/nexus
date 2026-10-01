//! The control loop must say out loud when a CLI control request fails to match
//! the typed shape.
//!
//! `start_control_handler` tried `serde_json::from_value::<SDKControlPermissionRequest>`
//! and `::<SDKHookCallbackRequest>` and, on failure, silently dropped to a lenient
//! field-picking path. That path is lossy — it throws away the whole
//! `permission_suggestions` list and turns a non-string `tool_use_id` into `None`
//! — so a request could be degraded with nothing anywhere saying why. The serde
//! reason is now logged at `warn`.
//!
//! This lives in its own integration binary on purpose: `tracing` caches each
//! callsite's interest for the whole process, so a thread-local subscriber
//! installed after another thread has already hit the callsite sees nothing. One
//! global subscriber, one process, one test.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nexus_claude::transport::mock::MockTransport;
use nexus_claude::{Query, transport::Transport};
use serde_json::{Value as JsonValue, json};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);

static LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct LogSink;

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut held) = LOG.lock() {
            held.extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn logged() -> String {
    let held = LOG.lock().expect("log buffer");
    String::from_utf8_lossy(held.as_slice()).into_owned()
}

/// One test per binary, so the whole control loop can be exercised under a single
/// global subscriber without another test poisoning the callsite cache.
#[tokio::test]
async fn a_request_that_misses_the_typed_shape_says_why_before_falling_back() {
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(|| LogSink)
            .finish(),
    )
    .expect("no other subscriber may be installed in this binary");

    let (transport, handle) = MockTransport::pair();
    let transport: Arc<AsyncMutex<Box<dyn Transport + Send>>> =
        Arc::new(AsyncMutex::new(transport));
    let mut query = Query::new(transport, true, None, None, HashMap::new());
    query.start().await.expect("start");
    let mut responses = handle.outbound_control_rx;
    let to_sdk = handle.sdk_control_tx;

    // 1. A well-formed request must NOT produce the warning, otherwise it says
    //    nothing when it does appear.
    to_sdk
        .send(json!({
            "type": "control_request",
            "request_id": "well-formed",
            "request": {
                "subtype": "hook_callback",
                "callback_id": "hook_404",
                "input": {"hook_event_name": "Stop", "session_id": "s", "transcript_path": "/t", "cwd": "/c", "stop_hook_active": false},
            },
        }))
        .await
        .expect("send");
    let answer = next(&mut responses).await;
    assert_eq!(answer["request_id"], "well-formed");
    assert!(
        !logged().contains("does not match the typed shape"),
        "a request the SDK understands must not be reported as a shape mismatch:\n{}",
        logged()
    );

    // 2. A permission suggestion the SDK cannot model. The request is still
    //    served, but every suggestion is dropped — the log is the only trace.
    to_sdk
        .send(json!({
            "type": "control_request",
            "request_id": "future-suggestion",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Write",
                "input": {},
                "permission_suggestions": [{"type": "aPermissionUpdateTypeFromTheFuture"}],
            },
        }))
        .await
        .expect("send");
    let answer = next(&mut responses).await;
    assert_eq!(answer["request_id"], "future-suggestion");
    let text = logged();
    let line = line_containing(
        &text,
        "can_use_tool request does not match the typed shape, falling back to lenient fields:",
    );
    assert!(
        line.contains("aPermissionUpdateTypeFromTheFuture"),
        "the warning must name what serde choked on: {line}"
    );
    assert!(
        line.contains("WARN"),
        "a lossy fallback is a warning, not a debug detail: {line}"
    );

    // 3. A hook_callback missing its (non-optional) `input` field.
    to_sdk
        .send(json!({
            "type": "control_request",
            "request_id": "no-input",
            "request": {"subtype": "hook_callback", "callback_id": "hook_404"},
        }))
        .await
        .expect("send");
    let answer = next(&mut responses).await;
    assert_eq!(answer["request_id"], "no-input");
    let text = logged();
    let line = line_containing(
        &text,
        "hook_callback request does not match the typed shape, falling back to lenient fields:",
    );
    assert!(
        line.contains("input"),
        "the warning must name the missing field: {line}"
    );
}

async fn next(responses: &mut tokio::sync::mpsc::Receiver<JsonValue>) -> JsonValue {
    let outer = timeout(WAIT, responses.recv())
        .await
        .expect("the control request was never answered")
        .expect("the outbound control channel closed");
    outer["response"].clone()
}

fn line_containing<'a>(text: &'a str, needle: &str) -> &'a str {
    text.lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no log line contains {needle:?}; captured log was:\n{text}"))
}
