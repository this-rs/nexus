//! Behaviour of `Query` — the control-protocol loop of `src/internal_query.rs`.
//!
//! Everything here runs in process against `transport::mock::MockTransport` (or a
//! purpose-built transport declared below): no `claude` binary, no subprocess, no
//! network, no clock wall-time beyond the explicit short waits.
//!
//! The contract this file pins down is **every inbound control request gets
//! exactly one reply**. The CLI blocks the turn while it waits for the response
//! that carries its `request_id`, so a branch that only logs is a hang, not a
//! warning. `fix/sl-32-control-always-reply` turned seven such branches into
//! `subtype: "error"` replies; the tests named `..._is_answered_...` are the
//! regression tests for them, which that change shipped without.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::Stream;
use nexus_claude::transport::mock::{MockTransport, MockTransportHandle};
use nexus_claude::transport::{InputMessage, Transport};
use nexus_claude::{
    CanUseTool, ControlRequest, ControlResponse, HookCallback, HookContext, HookInput,
    HookJSONOutput, HookMatcher, Message, PermissionResult, PermissionResultAllow,
    PermissionResultDeny, PermissionUpdate, Query, Result, SdkError, SdkMcpServer,
    SyncHookJSONOutput, ToolPermissionContext,
};
use serde_json::{Value as JsonValue, json};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::time::timeout;

/// Long enough for a task hand-off on a loaded runner, short enough that a
/// genuinely stuck control loop fails the test instead of hanging the job.
const WAIT: Duration = Duration::from_secs(5);

/// How long we are willing to wait before concluding that nothing is coming.
const QUIET: Duration = Duration::from_millis(300);

// ===========================================================================
// harness
// ===========================================================================

fn query_with(
    can_use_tool: Option<Arc<dyn CanUseTool>>,
    hooks: Option<HashMap<String, Vec<HookMatcher>>>,
    servers: HashMap<String, Arc<dyn std::any::Any + Send + Sync>>,
) -> (Query, MockTransportHandle) {
    let (transport, handle) = MockTransport::pair();
    let query = Query::new(
        Arc::new(Mutex::new(transport)),
        true,
        can_use_tool,
        hooks,
        servers,
    );
    (query, handle)
}

fn plain_query() -> (Query, MockTransportHandle) {
    query_with(None, None, HashMap::new())
}

fn query_with_permission(callback: Arc<dyn CanUseTool>) -> (Query, MockTransportHandle) {
    query_with(Some(callback), None, HashMap::new())
}

/// The next control response the SDK pushed towards the CLI, unwrapped from the
/// `{"type":"control_response","response":…}` envelope `MockTransport` adds.
async fn reply(responses: &mut mpsc::Receiver<JsonValue>) -> JsonValue {
    let outer = timeout(WAIT, responses.recv())
        .await
        .expect("the control request was never answered — the CLI would block the turn")
        .expect("the outbound control channel closed");
    assert_eq!(
        outer["type"], "control_response",
        "control responses must stay wrapped for the CLI: {outer}"
    );
    outer["response"].clone()
}

/// Assert that nothing more is sent — used to prove a reply is sent *once*.
async fn stays_quiet(responses: &mut mpsc::Receiver<JsonValue>) {
    let extra = timeout(QUIET, responses.recv()).await;
    assert!(
        extra.is_err(),
        "a second, unexpected control response was sent: {:?}",
        extra.ok().flatten()
    );
}

/// Answer the first outbound control request, building the nested response from
/// its `request_id`. Returns the request the SDK sent, so tests can assert on it.
fn answer_once<F>(
    mut requests: mpsc::Receiver<JsonValue>,
    to_sdk: mpsc::Sender<JsonValue>,
    build: F,
) -> tokio::task::JoinHandle<JsonValue>
where
    F: FnOnce(&str) -> JsonValue + Send + 'static,
{
    tokio::spawn(async move {
        let request = requests
            .recv()
            .await
            .expect("no control request reached the transport");
        let request_id = request["request_id"]
            .as_str()
            .expect("every control request must carry a request_id")
            .to_string();
        to_sdk
            .send(json!({ "type": "control_response", "response": build(&request_id) }))
            .await
            .expect("the SDK stopped listening for control responses");
        request
    })
}

/// The usual `subtype: success` wrapper the CLI sends back.
fn success_response(request_id: &str, payload: JsonValue) -> JsonValue {
    json!({ "request_id": request_id, "subtype": "success", "response": payload })
}

fn system_message(subtype: &str) -> Message {
    Message::System {
        subtype: subtype.to_string(),
        data: json!({}),
    }
}

/// A minimal, valid `PreToolUse` hook input.
fn hook_input() -> JsonValue {
    json!({
        "hook_event_name": "PreToolUse",
        "session_id": "s1",
        "transcript_path": "/transcript",
        "cwd": "/cwd",
        "tool_name": "Bash",
        "tool_input": {"command": "ls"},
    })
}

// ===========================================================================
// doubles
// ===========================================================================

struct AlwaysAllow {
    updated_input: Option<JsonValue>,
    updated_permissions: Option<Vec<PermissionUpdate>>,
}

#[async_trait]
impl CanUseTool for AlwaysAllow {
    async fn can_use_tool(
        &self,
        _tool_name: &str,
        _input: &JsonValue,
        _context: &ToolPermissionContext,
    ) -> PermissionResult {
        PermissionResult::Allow(PermissionResultAllow {
            updated_input: self.updated_input.clone(),
            updated_permissions: self.updated_permissions.clone(),
        })
    }
}

struct AlwaysDeny {
    message: String,
    interrupt: bool,
}

#[async_trait]
impl CanUseTool for AlwaysDeny {
    async fn can_use_tool(
        &self,
        _tool_name: &str,
        _input: &JsonValue,
        _context: &ToolPermissionContext,
    ) -> PermissionResult {
        PermissionResult::Deny(PermissionResultDeny {
            message: self.message.clone(),
            interrupt: self.interrupt,
        })
    }
}

/// Records what the permission callback actually received, so tests can assert
/// on the arguments the control loop reconstructed from the wire JSON.
#[derive(Default)]
struct SeenCall {
    tool_name: Mutex<String>,
    input: Mutex<JsonValue>,
    suggestions: AtomicUsize,
}

struct Recorder(Arc<SeenCall>);

#[async_trait]
impl CanUseTool for Recorder {
    async fn can_use_tool(
        &self,
        tool_name: &str,
        input: &JsonValue,
        context: &ToolPermissionContext,
    ) -> PermissionResult {
        *self.0.tool_name.lock().await = tool_name.to_string();
        *self.0.input.lock().await = input.clone();
        self.0
            .suggestions
            .store(context.suggestions.len(), Ordering::SeqCst);
        PermissionResult::Allow(PermissionResultAllow {
            updated_input: None,
            updated_permissions: None,
        })
    }
}

/// Echoes back the `tool_use_id` the control loop handed it, so a test can see
/// whether the id survived the trip through the wire JSON.
struct EchoToolUseId(Arc<Mutex<Option<Option<String>>>>);

#[async_trait]
impl HookCallback for EchoToolUseId {
    async fn execute(
        &self,
        _input: &HookInput,
        tool_use_id: Option<&str>,
        _context: &HookContext,
    ) -> std::result::Result<HookJSONOutput, SdkError> {
        *self.0.lock().await = Some(tool_use_id.map(|s| s.to_string()));
        Ok(HookJSONOutput::Sync(SyncHookJSONOutput {
            reason: Some("echoed".to_string()),
            ..Default::default()
        }))
    }
}

/// A hook that signals when it starts and then waits to be released.
struct GatedHook {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl HookCallback for GatedHook {
    async fn execute(
        &self,
        _input: &HookInput,
        _tool_use_id: Option<&str>,
        _context: &HookContext,
    ) -> std::result::Result<HookJSONOutput, SdkError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(HookJSONOutput::Sync(SyncHookJSONOutput::default()))
    }
}

struct FailingHook;

#[async_trait]
impl HookCallback for FailingHook {
    async fn execute(
        &self,
        _input: &HookInput,
        _tool_use_id: Option<&str>,
        _context: &HookContext,
    ) -> std::result::Result<HookJSONOutput, SdkError> {
        Err(SdkError::ConfigError("the hook refused".to_string()))
    }
}

/// Transport that refuses every write and counts the attempts. It exists to
/// drive the `error!("Failed to send …")` arms: a transport failure while
/// answering must be logged and swallowed, never kill the control loop.
struct RefusingTransport {
    control_rx: Option<mpsc::Receiver<JsonValue>>,
    responses: Arc<AtomicUsize>,
    inputs: Arc<AtomicUsize>,
    end_inputs: Arc<AtomicUsize>,
}

#[async_trait]
impl Transport for RefusingTransport {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    async fn connect(&mut self) -> Result<()> {
        Ok(())
    }
    async fn send_message(&mut self, _message: InputMessage) -> Result<()> {
        self.inputs.fetch_add(1, Ordering::SeqCst);
        Err(SdkError::TransportError("stdin is closed".to_string()))
    }
    fn receive_messages(
        &mut self,
    ) -> Pin<Box<dyn Stream<Item = Result<Message>> + Send + 'static>> {
        Box::pin(futures::stream::empty())
    }
    async fn send_control_request(&mut self, _request: ControlRequest) -> Result<()> {
        Ok(())
    }
    async fn receive_control_response(&mut self) -> Result<Option<ControlResponse>> {
        Ok(None)
    }
    async fn send_sdk_control_request(&mut self, _request: JsonValue) -> Result<()> {
        Err(SdkError::TransportError("stdin is closed".to_string()))
    }
    async fn send_sdk_control_response(&mut self, _response: JsonValue) -> Result<()> {
        self.responses.fetch_add(1, Ordering::SeqCst);
        Err(SdkError::TransportError("stdin is closed".to_string()))
    }
    fn take_sdk_control_receiver(&mut self) -> Option<mpsc::Receiver<JsonValue>> {
        self.control_rx.take()
    }
    fn is_connected(&self) -> bool {
        true
    }
    async fn disconnect(&mut self) -> Result<()> {
        Ok(())
    }
    async fn end_input(&mut self) -> Result<()> {
        self.end_inputs.fetch_add(1, Ordering::SeqCst);
        Err(SdkError::TransportError("stdin is closed".to_string()))
    }
}

struct RefusingHandles {
    to_sdk: mpsc::Sender<JsonValue>,
    responses: Arc<AtomicUsize>,
    inputs: Arc<AtomicUsize>,
    end_inputs: Arc<AtomicUsize>,
}

fn query_on_refusing_transport(
    can_use_tool: Option<Arc<dyn CanUseTool>>,
    servers: HashMap<String, Arc<dyn std::any::Any + Send + Sync>>,
) -> (Query, RefusingHandles) {
    let (to_sdk, control_rx) = mpsc::channel(16);
    let handles = RefusingHandles {
        to_sdk,
        responses: Arc::new(AtomicUsize::new(0)),
        inputs: Arc::new(AtomicUsize::new(0)),
        end_inputs: Arc::new(AtomicUsize::new(0)),
    };
    let transport: Box<dyn Transport + Send> = Box::new(RefusingTransport {
        control_rx: Some(control_rx),
        responses: handles.responses.clone(),
        inputs: handles.inputs.clone(),
        end_inputs: handles.end_inputs.clone(),
    });
    let query = Query::new(
        Arc::new(Mutex::new(transport)),
        true,
        can_use_tool,
        None,
        servers,
    );
    (query, handles)
}

/// Transport whose message stream hands out one message, then a hard error, then
/// a message that must never arrive.
struct BrokenStreamTransport;

#[async_trait]
impl Transport for BrokenStreamTransport {
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    async fn connect(&mut self) -> Result<()> {
        Ok(())
    }
    async fn send_message(&mut self, _message: InputMessage) -> Result<()> {
        Ok(())
    }
    fn receive_messages(
        &mut self,
    ) -> Pin<Box<dyn Stream<Item = Result<Message>> + Send + 'static>> {
        Box::pin(futures::stream::iter(vec![
            Ok(system_message("before the break")),
            Err(SdkError::TransportError("the pipe went away".to_string())),
            Ok(system_message("after the break")),
        ]))
    }
    async fn send_control_request(&mut self, _request: ControlRequest) -> Result<()> {
        Ok(())
    }
    async fn receive_control_response(&mut self) -> Result<Option<ControlResponse>> {
        Ok(None)
    }
    async fn send_sdk_control_request(&mut self, _request: JsonValue) -> Result<()> {
        Ok(())
    }
    async fn send_sdk_control_response(&mut self, _response: JsonValue) -> Result<()> {
        Ok(())
    }
    fn is_connected(&self) -> bool {
        true
    }
    async fn disconnect(&mut self) -> Result<()> {
        Ok(())
    }
}

// ===========================================================================
// 1. The control loop itself
// ===========================================================================

/// The strong count once it stops changing, so a transient per-request task
/// cannot be mistaken for a leaked one.
async fn settled_strong_count<T>(arc: &Arc<T>) -> usize {
    let mut last = Arc::strong_count(arc);
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let now = Arc::strong_count(arc);
        if now == last {
            return now;
        }
        last = now;
    }
    last
}

/// `recv()` on a closed `mpsc::Receiver` returns `Ready(None)` for ever, so the
/// loop that only reacted to `Some(..)` never stopped: it span on the closed
/// channel for the rest of the process, burning a core (tokio's cooperative
/// budget keeps it from starving its runtime, which is exactly why nothing ever
/// noticed). `SubprocessTransport` deliberately keeps no `Sender`, so this
/// happens on every CLI exit, not just on a contrived close.
///
/// There is no public handle on the task, so the proof is the transport `Arc`:
/// the handler owns a clone, and a task that ends gives it back.
#[tokio::test]
async fn a_closed_control_channel_ends_the_handler_instead_of_spinning() {
    let (inner, handle) = MockTransport::pair();
    let transport = Arc::new(Mutex::new(inner));
    let mut query = Query::new(transport.clone(), true, None, None, HashMap::new());
    query.start().await.expect("start");
    let mut responses = handle.outbound_control_rx;

    // One round trip, so the handler is provably running before we count.
    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "ping",
            "request": {"subtype": "teleport"},
        }))
        .await
        .expect("send");
    assert_eq!(reply(&mut responses).await["request_id"], "ping");

    let before = settled_strong_count(&transport).await;
    assert!(
        before >= 3,
        "expected the test, the Query and its two tasks to hold the transport, got {before}"
    );

    // Close the CLI end of the control channel, as a CLI exit does.
    drop(handle.sdk_control_tx);

    let deadline = tokio::time::Instant::now() + WAIT;
    while Arc::strong_count(&transport) >= before {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the control handler never released the transport: it is still spinning on the closed channel"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A request whose `subtype` is missing entirely used to fall off the end of the
/// `if let Some(subtype)` with no reply at all — the same hang the explicit
/// `_ =>` arm was fixed for.
#[tokio::test]
async fn a_control_request_without_a_subtype_is_answered_with_an_error() {
    let (mut query, mut handle) = plain_query();
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "no-subtype",
            "request": {"tool_name": "Bash"},
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "error");
    assert_eq!(answer["request_id"], "no-subtype");
    assert_eq!(answer["error"], "Control request has no subtype");
    stays_quiet(&mut handle.outbound_control_rx).await;
}

#[tokio::test]
async fn an_unknown_subtype_is_answered_with_an_error_naming_it() {
    let (mut query, mut handle) = plain_query();
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "weird",
            "request": {"subtype": "teleport"},
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "error");
    assert_eq!(answer["error"], "Unsupported control subtype: teleport");
}

/// A transport that cannot write must not take the loop down with it. Every
/// answering branch — each subtype, each of its typed and lenient paths, each of
/// its success and error shapes — has its own `error!` arm; all of them must
/// swallow the failure and leave the loop able to serve the next request.
#[tokio::test]
async fn a_transport_that_refuses_to_write_still_answers_every_later_request() {
    let servers = servers_with("calc", Arc::new(SdkMcpServer::new("calc", "1.0.0")));
    let (mut query, handles) = query_on_refusing_transport(
        Some(Arc::new(AlwaysAllow {
            updated_input: None,
            updated_permissions: None,
        })),
        servers,
    );
    query.start().await.expect("start");
    query
        .register_hook_callback_for_test("cb".to_string(), Arc::new(NamedHook("hooked")))
        .await;

    let shapes = [
        // unknown subtype
        json!({"subtype": "teleport"}),
        // can_use_tool, typed path
        json!({"subtype": "can_use_tool", "tool_name": "Bash", "input": {}}),
        // can_use_tool, lenient path (the suggestion breaks the typed parse)
        json!({
            "subtype": "can_use_tool",
            "tool_name": "Bash",
            "input": {},
            "permission_suggestions": [{"type": "fromTheFuture"}],
        }),
        // hook_callback, typed path, hook found
        json!({"subtype": "hook_callback", "callbackId": "cb", "input": hook_input()}),
        // hook_callback, typed path, hook missing
        json!({"subtype": "hook_callback", "callbackId": "ghost", "input": hook_input()}),
        // hook_callback, lenient path (no `input` breaks the typed parse)
        json!({"subtype": "hook_callback", "callback_id": "cb"}),
        // mcp_message, served
        json!({"subtype": "mcp_message", "server_name": "calc", "message": {"method": "initialize"}}),
        // mcp_message, rejected by the server
        json!({"subtype": "mcp_message", "server_name": "calc", "message": {}}),
        // mcp_message, unknown server
        json!({"subtype": "mcp_message", "server_name": "ghost", "message": {"method": "initialize"}}),
    ];
    let expected = shapes.len();

    for (index, request) in shapes.into_iter().enumerate() {
        handles
            .to_sdk
            .send(json!({
                "type": "control_request",
                "request_id": format!("r{index}"),
                "request": request,
            }))
            .await
            .expect("send");
    }

    let deadline = tokio::time::Instant::now() + WAIT;
    while handles.responses.load(Ordering::SeqCst) < expected {
        assert!(
            tokio::time::Instant::now() < deadline,
            "only {} of {expected} answers were attempted: a write failure stopped the loop",
            handles.responses.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ===========================================================================
// 2. Routing responses back to `send_control_request`
// ===========================================================================

#[tokio::test]
async fn a_response_payload_is_unwrapped_from_the_response_field() {
    let (mut query, handle) = plain_query();
    query.start().await.expect("start");
    let responder = answer_once(
        handle.outbound_control_request_rx,
        handle.sdk_control_tx.clone(),
        |id| success_response(id, json!({"commands": ["/help"]})),
    );

    query.initialize().await.expect("initialize");
    responder.await.expect("responder");

    let init = query
        .get_initialization_result()
        .expect("initialize must record its result");
    assert_eq!(init["commands"][0], "/help");
}

/// Older CLIs put the payload in `data`; `send_control_request` still accepts it.
#[tokio::test]
async fn a_legacy_data_payload_is_accepted_in_place_of_response() {
    let (mut query, handle) = plain_query();
    query.start().await.expect("start");
    let responder = answer_once(
        handle.outbound_control_request_rx,
        handle.sdk_control_tx.clone(),
        |id| json!({"request_id": id, "subtype": "success", "data": {"legacy": true}}),
    );

    query.initialize().await.expect("initialize");
    responder.await.expect("responder");

    assert_eq!(
        query.get_initialization_result().expect("result")["legacy"],
        true
    );
}

#[tokio::test]
async fn a_response_with_no_payload_at_all_yields_an_empty_object() {
    let (mut query, handle) = plain_query();
    query.start().await.expect("start");
    let responder = answer_once(
        handle.outbound_control_request_rx,
        handle.sdk_control_tx.clone(),
        |id| json!({"requestId": id, "subtype": "success"}),
    );

    query.initialize().await.expect("initialize");
    responder.await.expect("responder");

    assert_eq!(
        query.get_initialization_result().expect("result"),
        &json!({}),
        "a success with no payload must still resolve the caller"
    );
}

#[tokio::test]
async fn an_error_response_becomes_a_control_request_error_carrying_the_reason() {
    let (mut query, handle) = plain_query();
    query.start().await.expect("start");
    let responder = answer_once(
        handle.outbound_control_request_rx,
        handle.sdk_control_tx.clone(),
        |id| json!({"request_id": id, "subtype": "error", "error": "hooks are not supported"}),
    );

    let failure = query
        .initialize()
        .await
        .expect_err("must surface the error");
    responder.await.expect("responder");

    match failure {
        SdkError::ControlRequestError(message) => {
            assert_eq!(message, "hooks are not supported");
        },
        other => panic!("expected ControlRequestError, got {other:?}"),
    }
    assert!(
        query.get_initialization_result().is_none(),
        "a failed initialize must not record a result"
    );
}

#[tokio::test]
async fn an_error_response_without_a_reason_gets_the_placeholder_message() {
    let (mut query, handle) = plain_query();
    query.start().await.expect("start");
    let responder = answer_once(
        handle.outbound_control_request_rx,
        handle.sdk_control_tx.clone(),
        |id| json!({"request_id": id, "subtype": "error"}),
    );

    let failure = query.interrupt().await.expect_err("must surface the error");
    responder.await.expect("responder");

    assert!(
        matches!(&failure, SdkError::ControlRequestError(m) if m == "Unknown control request error"),
        "unexpected error: {failure:?}"
    );
}

/// Three malformed `control_response` shapes. None of them can be routed, and
/// none of them may take the loop down: a well-formed request sent afterwards
/// must still be answered.
#[tokio::test]
async fn unroutable_control_responses_are_dropped_without_stopping_the_loop() {
    let unroutable = [
        json!({"type": "control_response", "response": {"subtype": "success"}}),
        json!({"type": "control_response"}),
        json!({"type": "control_response", "response": {"request_id": "never-asked-for"}}),
    ];

    for message in unroutable {
        let (mut query, mut handle) = query_with_permission(Arc::new(AlwaysAllow {
            updated_input: None,
            updated_permissions: None,
        }));
        query.start().await.expect("start");

        handle
            .sdk_control_tx
            .send(message.clone())
            .await
            .expect("send");
        handle
            .sdk_control_tx
            .send(json!({
                "type": "control_request",
                "request_id": "after",
                "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {}},
            }))
            .await
            .expect("send");

        let answer = reply(&mut handle.outbound_control_rx).await;
        assert_eq!(
            answer["request_id"], "after",
            "the loop stopped on {message} instead of ignoring it"
        );
        assert_eq!(answer["subtype"], "success");
    }
}

#[tokio::test(start_paused = true)]
async fn a_control_request_nobody_answers_times_out_after_sixty_seconds() {
    // No `start()`, so nothing ever routes a response back.
    let (mut query, _handle) = plain_query();

    let failure = query.interrupt().await.expect_err("must time out");

    assert!(
        matches!(failure, SdkError::Timeout { seconds: 60 }),
        "unexpected error: {failure:?}"
    );
}

// ===========================================================================
// 3. `can_use_tool`
// ===========================================================================

#[tokio::test]
async fn an_allow_decision_carries_the_rewritten_input_and_the_permission_updates() {
    let updates: Vec<PermissionUpdate> = serde_json::from_value(json!([
        {"type": "setMode", "mode": "acceptEdits", "destination": "session"}
    ]))
    .expect("suggestions fixture");
    let (mut query, mut handle) = query_with_permission(Arc::new(AlwaysAllow {
        updated_input: Some(json!({"command": "ls -la"})),
        updated_permissions: Some(updates),
    }));
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "allow",
            "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "ls"}},
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "success");
    assert_eq!(answer["request_id"], "allow");
    assert_eq!(answer["response"]["allow"], true);
    assert_eq!(answer["response"]["input"]["command"], "ls -la");
    assert_eq!(
        answer["response"]["updatedPermissions"][0]["type"],
        "setMode"
    );
    assert!(
        answer["response"].get("reason").is_none(),
        "an allow must not carry a denial reason: {answer}"
    );
}

#[tokio::test]
async fn a_deny_decision_carries_the_reason_and_the_interrupt_flag() {
    let (mut query, mut handle) = query_with_permission(Arc::new(AlwaysDeny {
        message: "rm -rf is never fine".to_string(),
        interrupt: true,
    }));
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "deny",
            "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {}},
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "success");
    assert_eq!(answer["response"]["allow"], false);
    assert_eq!(answer["response"]["reason"], "rm -rf is never fine");
    assert_eq!(answer["response"]["interrupt"], true);
}

/// An empty denial message is omitted rather than sent as `""`, and a denial
/// that does not interrupt omits the flag instead of sending `false`.
#[tokio::test]
async fn a_silent_deny_omits_both_the_reason_and_the_interrupt_flag() {
    let (mut query, mut handle) = query_with_permission(Arc::new(AlwaysDeny {
        message: String::new(),
        interrupt: false,
    }));
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "quiet-deny",
            "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {}},
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["response"], json!({"allow": false}));
}

/// Before `fix/sl-32-control-always-reply` a client built without a permission
/// callback simply dropped the request and the turn never ended.
#[tokio::test]
async fn a_permission_request_with_no_callback_configured_is_answered_with_an_error() {
    let (mut query, mut handle) = plain_query();
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "nobody-home",
            "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {}},
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "error");
    assert_eq!(answer["request_id"], "nobody-home");
    assert_eq!(answer["error"], "No can_use_tool callback registered");
    stays_quiet(&mut handle.outbound_control_rx).await;
}

/// Here the typed shape *is* parseable but there is no `tool_name` at all, so
/// neither path can reconstruct a call.
#[tokio::test]
async fn a_permission_request_without_a_tool_name_is_answered_with_an_error() {
    let (mut query, mut handle) = query_with_permission(Arc::new(AlwaysAllow {
        updated_input: None,
        updated_permissions: None,
    }));
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "nameless",
            "request": {"subtype": "can_use_tool", "input": {}},
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "error");
    assert_eq!(
        answer["error"],
        "Invalid can_use_tool request or no callback registered"
    );
}

#[tokio::test]
async fn camel_case_permission_fields_reach_the_callback_with_their_suggestions() {
    let seen = Arc::new(SeenCall::default());
    let (mut query, mut handle) = query_with_permission(Arc::new(Recorder(seen.clone())));
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "requestId": "camel",
            "request": {
                "subtype": "can_use_tool",
                "toolName": "Write",
                "input": {"path": "notes.md"},
                "permissionSuggestions": [
                    {"type": "setMode", "mode": "acceptEdits", "destination": "session"}
                ],
            },
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(
        answer["request_id"], "camel",
        "a camelCase requestId must be echoed back"
    );
    assert_eq!(*seen.tool_name.lock().await, "Write");
    assert_eq!(seen.input.lock().await["path"], "notes.md");
    assert_eq!(seen.suggestions.load(Ordering::SeqCst), 1);
}

/// A suggestion the SDK cannot model makes the *typed* parse fail, so the loop
/// drops to its lenient path — which keeps the call but silently throws every
/// suggestion away. The callback is told the CLI suggested nothing.
#[tokio::test]
async fn an_unmodellable_permission_suggestion_costs_every_suggestion() {
    let seen = Arc::new(SeenCall::default());
    let (mut query, mut handle) = query_with_permission(Arc::new(Recorder(seen.clone())));
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "lenient",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Write",
                "input": {"path": "notes.md"},
                "permission_suggestions": [
                    {"type": "setMode", "mode": "acceptEdits", "destination": "session"},
                    {"type": "aPermissionUpdateTypeFromTheFuture"}
                ],
            },
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "success");
    assert_eq!(*seen.tool_name.lock().await, "Write");
    assert_eq!(
        seen.suggestions.load(Ordering::SeqCst),
        0,
        "the lenient path drops the whole suggestion list, including the good entry"
    );
}

/// A request only the lenient path can read must still carry the full decision:
/// the rewritten input, the permission updates, the denial reason and the
/// interrupt flag are built a second time there, and used to differ.
#[tokio::test]
async fn the_lenient_permission_path_carries_the_whole_decision_too() {
    let updates: Vec<PermissionUpdate> = serde_json::from_value(json!([
        {"type": "addRules", "behavior": "allow", "destination": "session"}
    ]))
    .expect("suggestions fixture");
    // Only the lenient path can read this request.
    let unmodellable = json!({
        "subtype": "can_use_tool",
        "tool_name": "Bash",
        "input": {"command": "ls"},
        "permission_suggestions": [{"type": "fromTheFuture"}],
    });

    let (mut query, mut handle) = query_with_permission(Arc::new(AlwaysAllow {
        updated_input: Some(json!({"command": "ls -la"})),
        updated_permissions: Some(updates),
    }));
    query.start().await.expect("start");
    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "lenient-allow",
            "request": unmodellable.clone(),
        }))
        .await
        .expect("send");
    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "success");
    assert_eq!(answer["response"]["allow"], true);
    assert_eq!(answer["response"]["input"]["command"], "ls -la");
    assert_eq!(
        answer["response"]["updatedPermissions"][0]["type"],
        "addRules"
    );

    let (mut query, mut handle) = query_with_permission(Arc::new(AlwaysDeny {
        message: "no listing today".to_string(),
        interrupt: true,
    }));
    query.start().await.expect("start");
    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "lenient-deny",
            "request": unmodellable,
        }))
        .await
        .expect("send");
    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "success");
    assert_eq!(answer["response"]["allow"], false);
    assert_eq!(answer["response"]["reason"], "no listing today");
    assert_eq!(answer["response"]["interrupt"], true);
}

/// Two shapes with no nested `request`: a bare request object, and a wrapper
/// whose fields are inline. Both are handled, and the bare one has no request id
/// to echo — the reply carries `null`, which is what the CLI gets today.
#[tokio::test]
async fn requests_without_a_nested_request_field_are_still_handled() {
    let (mut query, mut handle) = query_with_permission(Arc::new(AlwaysAllow {
        updated_input: None,
        updated_permissions: None,
    }));
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({"subtype": "can_use_tool", "tool_name": "Bash", "input": {}}))
        .await
        .expect("send");
    let bare = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(bare["subtype"], "success");
    assert!(
        bare["request_id"].is_null(),
        "an unidentified request can only be answered with a null id: {bare}"
    );

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "inline",
            "subtype": "can_use_tool",
            "tool_name": "Bash",
            "input": {},
        }))
        .await
        .expect("send");
    let inline = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(inline["subtype"], "success");
    assert_eq!(inline["request_id"], "inline");
}

// ===========================================================================
// 4. `hook_callback`
// ===========================================================================

#[tokio::test]
async fn an_unknown_hook_callback_id_is_answered_with_an_error_naming_it() {
    let (mut query, mut handle) = plain_query();
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "ghost",
            "request": {
                "subtype": "hook_callback",
                "callback_id": "hook_404",
                "input": hook_input(),
            },
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "error");
    assert_eq!(answer["request_id"], "ghost");
    assert_eq!(answer["error"], "No hook callback found for ID: hook_404");
}

/// Same, through the lenient path: dropping `input` makes the typed parse fail
/// (the field is not optional), so the unknown id is discovered in the fallback,
/// whose branch used to only log.
#[tokio::test]
async fn an_unknown_hook_callback_id_is_answered_from_the_lenient_path_too() {
    let (mut query, mut handle) = plain_query();
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "ghost-lenient",
            "request": {"subtype": "hook_callback", "callback_id": "hook_404"},
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "error");
    assert_eq!(answer["request_id"], "ghost-lenient");
    assert_eq!(answer["error"], "No hook callback found for ID: hook_404");
}

#[tokio::test]
async fn a_hook_callback_without_an_id_is_answered_with_an_error() {
    let (mut query, mut handle) = plain_query();
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "anonymous",
            "request": {"subtype": "hook_callback", "input": hook_input()},
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "error");
    assert_eq!(
        answer["error"],
        "Invalid hook_callback: missing callback_id"
    );
}

/// An input that does not name a known `hook_event_name` cannot become a
/// `HookInput`; the hook is not called and the CLI is told why.
#[tokio::test]
async fn an_uninterpretable_hook_input_is_answered_with_a_parse_error() {
    let (mut query, mut handle) = plain_query();
    query.start().await.expect("start");
    let seen = Arc::new(Mutex::new(None));
    query
        .register_hook_callback_for_test("cb".to_string(), Arc::new(EchoToolUseId(seen.clone())))
        .await;

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "bad-input",
            "request": {
                "subtype": "hook_callback",
                "callbackId": "cb",
                "input": {"hook_event_name": "AnEventFromTheFuture"},
            },
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "error");
    assert_eq!(answer["request_id"], "bad-input");
    assert!(
        answer["error"]
            .as_str()
            .is_some_and(|e| e.contains("Invalid hook input")),
        "the reply must say the input could not be parsed: {answer}"
    );
    assert!(
        seen.lock().await.is_none(),
        "the hook must not run on an input that failed to parse"
    );
}

#[tokio::test]
async fn a_hook_that_returns_an_error_is_reported_as_subtype_error() {
    let (mut query, mut handle) = plain_query();
    query.start().await.expect("start");
    query
        .register_hook_callback_for_test("cb".to_string(), Arc::new(FailingHook))
        .await;

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "hook-failed",
            "request": {"subtype": "hook_callback", "callbackId": "cb", "input": hook_input()},
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "error");
    assert!(
        answer["error"]
            .as_str()
            .is_some_and(|e| e.contains("the hook refused")),
        "the hook's own error text must reach the CLI: {answer}"
    );
}

/// `tool_use_id` is read with `as_str()` in the lenient path, so a CLI that sent
/// a number instead of a string still gets its hook run — but the hook is told
/// there is no tool use id. The hook ran, so the turn proceeds; the id is the
/// silent casualty.
#[tokio::test]
async fn a_non_string_tool_use_id_runs_the_hook_but_loses_the_id() {
    let (mut query, mut handle) = plain_query();
    query.start().await.expect("start");
    let seen = Arc::new(Mutex::new(None));
    query
        .register_hook_callback_for_test("cb".to_string(), Arc::new(EchoToolUseId(seen.clone())))
        .await;

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "numeric-id",
            "request": {
                "subtype": "hook_callback",
                "callback_id": "cb",
                "input": hook_input(),
                "tool_use_id": 42,
            },
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "success");
    assert_eq!(answer["response"]["reason"], "echoed");
    assert_eq!(
        *seen.lock().await,
        Some(None),
        "the hook ran, but the numeric tool_use_id was dropped on the floor"
    );
}

#[tokio::test]
async fn a_string_tool_use_id_reaches_the_hook() {
    let (mut query, mut handle) = plain_query();
    query.start().await.expect("start");
    let seen = Arc::new(Mutex::new(None));
    query
        .register_hook_callback_for_test("cb".to_string(), Arc::new(EchoToolUseId(seen.clone())))
        .await;

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "tu",
            "request": {
                "subtype": "hook_callback",
                "callback_id": "cb",
                "input": hook_input(),
                "toolUseId": "toolu_01",
            },
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "success");
    assert_eq!(*seen.lock().await, Some(Some("toolu_01".to_string())));
}

/// The lenient hook path parses its input a second time, and must report a
/// failure there the same way the typed path does.
#[tokio::test]
async fn an_uninterpretable_hook_input_on_the_lenient_path_is_answered_too() {
    let (mut query, mut handle) = plain_query();
    query.start().await.expect("start");
    let seen = Arc::new(Mutex::new(None));
    query
        .register_hook_callback_for_test("cb".to_string(), Arc::new(EchoToolUseId(seen.clone())))
        .await;

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "lenient-bad-input",
            "request": {
                "subtype": "hook_callback",
                "callback_id": "cb",
                "input": {"hook_event_name": "AnEventFromTheFuture"},
                // A non-string tool_use_id is what pushes this to the lenient path.
                "tool_use_id": 42,
            },
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "error");
    assert_eq!(answer["request_id"], "lenient-bad-input");
    assert!(
        answer["error"]
            .as_str()
            .is_some_and(|e| e.contains("Invalid hook input")),
        "the lenient path must explain itself too: {answer}"
    );
    assert!(
        seen.lock().await.is_none(),
        "the hook must not run on an input that failed to parse"
    );
}

/// The hook registry is a `tokio::sync::RwLock`, which is write-preferring: a
/// read guard held across a slow hook queues a writer, and that queued writer
/// then blocks every later reader. Holding the guard across `execute().await`
/// therefore froze both `initialize()` and all subsequent hook callbacks for as
/// long as one hook was running. The guard is now released before the await.
#[tokio::test]
async fn a_slow_hook_does_not_freeze_the_hook_registry() {
    let (mut query, mut handle) = plain_query();
    query.start().await.expect("start");

    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    query
        .register_hook_callback_for_test(
            "slow".to_string(),
            Arc::new(GatedHook {
                entered: entered.clone(),
                release: release.clone(),
            }),
        )
        .await;
    let seen = Arc::new(Mutex::new(None));
    query
        .register_hook_callback_for_test("fast".to_string(), Arc::new(EchoToolUseId(seen.clone())))
        .await;

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "slow-req",
            "request": {"subtype": "hook_callback", "callbackId": "slow", "input": hook_input()},
        }))
        .await
        .expect("send");
    timeout(WAIT, entered.notified())
        .await
        .expect("the slow hook never started");

    // A writer queues behind whatever the slow handler is holding; a second
    // hook callback then has to get through regardless.
    let registering =
        query.register_hook_callback_for_test("third".to_string(), Arc::new(FailingHook));
    let fast = async {
        handle
            .sdk_control_tx
            .send(json!({
                "type": "control_request",
                "request_id": "fast-req",
                "request": {
                    "subtype": "hook_callback",
                    "callbackId": "fast",
                    "input": hook_input(),
                },
            }))
            .await
            .expect("send");
        reply(&mut handle.outbound_control_rx).await
    };
    let (_, answer) = tokio::join!(registering, fast);

    assert_eq!(
        answer["request_id"], "fast-req",
        "the fast hook had to answer while the slow one was still running"
    );
    assert_eq!(answer["subtype"], "success");

    release.notify_one();
    let slow = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(slow["request_id"], "slow-req");
    assert_eq!(slow["subtype"], "success");
}

// ===========================================================================
// 5. `initialize` and the hook callback ids it mints
// ===========================================================================

struct NamedHook(&'static str);

#[async_trait]
impl HookCallback for NamedHook {
    async fn execute(
        &self,
        _input: &HookInput,
        _tool_use_id: Option<&str>,
        _context: &HookContext,
    ) -> std::result::Result<HookJSONOutput, SdkError> {
        Ok(HookJSONOutput::Sync(SyncHookJSONOutput {
            reason: Some(self.0.to_string()),
            ..Default::default()
        }))
    }
}

#[tokio::test]
async fn initialize_mints_one_callback_id_per_hook_and_the_cli_can_call_them_back() {
    let hooks = HashMap::from([(
        "PreToolUse".to_string(),
        vec![
            HookMatcher {
                matcher: Some(json!("Bash")),
                hooks: vec![Arc::new(NamedHook("first")), Arc::new(NamedHook("second"))],
            },
            HookMatcher {
                matcher: None,
                hooks: vec![Arc::new(NamedHook("third"))],
            },
        ],
    )]);
    let (mut query, handle) = query_with(None, Some(hooks), HashMap::new());
    query.start().await.expect("start");
    let MockTransportHandle {
        outbound_control_rx: mut responses,
        outbound_control_request_rx: requests,
        sdk_control_tx,
        ..
    } = handle;
    let responder = answer_once(requests, sdk_control_tx.clone(), |id| {
        success_response(id, json!({}))
    });

    query.initialize().await.expect("initialize");
    let sent = responder.await.expect("responder");

    // The wire shape: one entry per event, one object per matcher.
    let matchers = sent["request"]["hooks"]["PreToolUse"]
        .as_array()
        .expect("hooks must be sent as an array of matchers");
    assert_eq!(matchers.len(), 2);
    assert_eq!(matchers[0]["matcher"], "Bash");
    assert!(
        matchers[1]["matcher"].is_null(),
        "a matcher-less entry keeps a null matcher: {}",
        matchers[1]
    );

    let ids: Vec<&str> = matchers
        .iter()
        .flat_map(|m| {
            m["hookCallbackIds"]
                .as_array()
                .expect("hookCallbackIds")
                .iter()
                .map(|v| v.as_str().expect("callback id is a string"))
        })
        .collect();
    assert_eq!(ids.len(), 3, "one id per registered hook: {ids:?}");
    assert!(
        ids[0].starts_with("hook_1_")
            && ids[1].starts_with("hook_2_")
            && ids[2].starts_with("hook_3_"),
        "ids are numbered from the shared counter: {ids:?}"
    );
    let unique: std::collections::HashSet<&&str> = ids.iter().collect();
    assert_eq!(unique.len(), 3, "ids must be unique: {ids:?}");

    // And the ids actually resolve: the CLI calling the second one runs the
    // second hook, not the first.
    sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "callback",
            "request": {
                "subtype": "hook_callback",
                "callbackId": ids[1],
                "input": hook_input(),
            },
        }))
        .await
        .expect("send");
    let answer = reply(&mut responses).await;
    assert_eq!(answer["subtype"], "success");
    assert_eq!(answer["response"]["reason"], "second");
}

#[tokio::test]
async fn initialize_without_hooks_sends_no_hooks_field_at_all() {
    let (mut query, handle) = plain_query();
    assert!(
        query.get_initialization_result().is_none(),
        "there is no initialization result before initialize()"
    );
    query.start().await.expect("start");
    let responder = answer_once(
        handle.outbound_control_request_rx,
        handle.sdk_control_tx.clone(),
        |id| success_response(id, json!({})),
    );

    query.initialize().await.expect("initialize");
    let sent = responder.await.expect("responder");

    assert_eq!(sent["type"], "control_request");
    assert_eq!(sent["request"]["type"], "initialize");
    assert!(
        sent["request"].get("hooks").is_none(),
        "no hooks configured must mean no hooks key: {sent}"
    );
}

// ===========================================================================
// 6. `mcp_message`
// ===========================================================================

fn servers_with(
    name: &str,
    server: Arc<dyn std::any::Any + Send + Sync>,
) -> HashMap<String, Arc<dyn std::any::Any + Send + Sync>> {
    HashMap::from([(name.to_string(), server)])
}

#[tokio::test]
async fn an_mcp_message_is_answered_by_the_in_process_server() {
    let servers = servers_with("calc", Arc::new(SdkMcpServer::new("calc", "9.9.9")));
    let (mut query, mut handle) = query_with(None, None, servers);
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "mcp-init",
            "request": {
                "subtype": "mcp_message",
                "server_name": "calc",
                "message": {"jsonrpc": "2.0", "id": 7, "method": "initialize"},
            },
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "success");
    assert_eq!(answer["request_id"], "mcp-init");
    let mcp = &answer["response"]["mcp_response"];
    assert_eq!(mcp["id"], 7, "the JSON-RPC id must be echoed: {mcp}");
    assert_eq!(mcp["result"]["serverInfo"]["name"], "calc");
    assert_eq!(mcp["result"]["serverInfo"]["version"], "9.9.9");
}

#[tokio::test]
async fn an_mcp_message_the_server_rejects_is_answered_with_subtype_error() {
    let servers = servers_with("calc", Arc::new(SdkMcpServer::new("calc", "1.0.0")));
    let (mut query, mut handle) = query_with(None, None, servers);
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "mcp-bad",
            "request": {
                "subtype": "mcp_message",
                "server_name": "calc",
                "message": {"jsonrpc": "2.0", "id": 1},
            },
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "error");
    assert!(
        answer["error"]
            .as_str()
            .is_some_and(|e| e.starts_with("MCP server error:") && e.contains("Missing method")),
        "the server's own diagnosis must reach the CLI: {answer}"
    );
}

/// `sdk_mcp_servers` is typed `Arc<dyn Any>`, so an entry of the wrong concrete
/// type is only discovered at downcast time. That branch used to only log.
#[tokio::test]
async fn an_mcp_entry_of_the_wrong_type_is_answered_with_an_error() {
    let servers = servers_with("calc", Arc::new("not a server at all".to_string()));
    let (mut query, mut handle) = query_with(None, None, servers);
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "wrong-type",
            "request": {
                "subtype": "mcp_message",
                "server_name": "calc",
                "message": {"method": "initialize"},
            },
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "error");
    assert_eq!(answer["error"], "Server 'calc' is not an SDK MCP server");
}

#[tokio::test]
async fn an_mcp_message_for_an_unknown_server_is_answered_with_an_error() {
    let servers = servers_with("calc", Arc::new(SdkMcpServer::new("calc", "1.0.0")));
    let (mut query, mut handle) = query_with(None, None, servers);
    query.start().await.expect("start");

    handle
        .sdk_control_tx
        .send(json!({
            "type": "control_request",
            "request_id": "mcp-missing",
            "request": {
                "subtype": "mcp_message",
                "server_name": "nope",
                "message": {"method": "initialize"},
            },
        }))
        .await
        .expect("send");

    let answer = reply(&mut handle.outbound_control_rx).await;
    assert_eq!(answer["subtype"], "error");
    assert_eq!(answer["error"], "Server 'nope' not found");
}

/// Two incomplete shapes: no `server_name`, and no `message`.
#[tokio::test]
async fn an_incomplete_mcp_message_is_answered_with_an_error() {
    let incomplete = [
        json!({"subtype": "mcp_message", "message": {"method": "initialize"}}),
        json!({"subtype": "mcp_message", "server_name": "calc"}),
    ];

    for request in incomplete {
        let servers = servers_with("calc", Arc::new(SdkMcpServer::new("calc", "1.0.0")));
        let (mut query, mut handle) = query_with(None, None, servers);
        query.start().await.expect("start");

        handle
            .sdk_control_tx
            .send(json!({
                "type": "control_request",
                "request_id": "incomplete",
                "request": request.clone(),
            }))
            .await
            .expect("send");

        let answer = reply(&mut handle.outbound_control_rx).await;
        assert_eq!(answer["subtype"], "error", "for {request}");
        assert_eq!(
            answer["error"], "Invalid mcp_message: missing server_name or message",
            "for {request}"
        );
    }
}

// ===========================================================================
// 7. The SDK message forwarder started by `start()`
// ===========================================================================

#[tokio::test]
async fn messages_from_the_cli_reach_the_query_receiver() {
    let (mut query, handle) = plain_query();
    query.start().await.expect("start");
    let mut messages = query.receive_messages().await;

    // The forwarder subscribes inside a spawned task; wait for it, otherwise the
    // broadcast drops what we publish.
    let deadline = tokio::time::Instant::now() + WAIT;
    while handle.inbound_message_tx.receiver_count() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "forwarder never subscribed"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    handle
        .inbound_message_tx
        .send(system_message("init"))
        .expect("publish");

    let forwarded = timeout(WAIT, messages.recv())
        .await
        .expect("nothing was forwarded")
        .expect("the forwarder closed the channel")
        .expect("the message arrived as an error");
    match forwarded {
        Message::System { subtype, .. } => assert_eq!(subtype, "init"),
        other => panic!("expected a system message, got {other:?}"),
    }
}

/// The forwarder reports the first stream error and then stops: anything the
/// stream would have produced afterwards is not forwarded.
#[tokio::test]
async fn a_stream_error_is_forwarded_once_and_ends_the_forwarder() {
    let transport: Box<dyn Transport + Send> = Box::new(BrokenStreamTransport);
    let mut query = Query::new(
        Arc::new(Mutex::new(transport)),
        true,
        None,
        None,
        HashMap::new(),
    );
    query.start().await.expect("start");
    let mut messages = query.receive_messages().await;

    let first = timeout(WAIT, messages.recv())
        .await
        .expect("nothing forwarded")
        .expect("channel closed")
        .expect("the first item is a message");
    assert!(matches!(first, Message::System { .. }));

    let failure = timeout(WAIT, messages.recv())
        .await
        .expect("the error was not forwarded")
        .expect("channel closed")
        .expect_err("the second item is the transport error");
    assert!(
        matches!(&failure, SdkError::TransportError(m) if m == "the pipe went away"),
        "unexpected error: {failure:?}"
    );

    assert!(
        timeout(QUIET, messages.recv()).await.is_err(),
        "the forwarder kept reading the stream after an error"
    );
}

/// When the consumer drops its receiver the forwarder must stop too, instead of
/// holding its broadcast subscription for the life of the process.
#[tokio::test]
async fn dropping_the_receiver_ends_the_forwarder() {
    let (mut query, handle) = plain_query();
    query.start().await.expect("start");
    let messages = query.receive_messages().await;

    let deadline = tokio::time::Instant::now() + WAIT;
    while handle.inbound_message_tx.receiver_count() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "forwarder never subscribed"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    drop(messages);
    handle
        .inbound_message_tx
        .send(system_message("nobody is listening"))
        .expect("publish");

    let deadline = tokio::time::Instant::now() + WAIT;
    while handle.inbound_message_tx.receiver_count() > 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the forwarder kept its broadcast subscription after its consumer went away"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
#[should_panic(expected = "Receiver already taken")]
async fn taking_the_message_receiver_twice_panics() {
    let (mut query, _handle) = plain_query();
    let _first = query.receive_messages().await;
    let _second = query.receive_messages().await;
}

// ===========================================================================
// 8. `stream_input`
// ===========================================================================

#[tokio::test]
async fn unconvertible_streaming_input_is_skipped_and_end_input_still_runs() {
    let (mut query, mut handle) = plain_query();
    query.start().await.expect("start");

    query
        .stream_input(futures::stream::iter(vec![
            json!(42),
            json!(["not", "a", "message"]),
            json!({"content": "Ping", "session_id": "s7"}),
        ]))
        .await
        .expect("stream_input");

    let sent = timeout(WAIT, handle.sent_input_rx.recv())
        .await
        .expect("nothing was sent")
        .expect("input channel closed");
    assert_eq!(sent.session_id, "s7");
    assert_eq!(sent.message["content"], "Ping");

    assert!(
        timeout(QUIET, handle.sent_input_rx.recv()).await.is_err(),
        "the unconvertible values must not produce input messages"
    );
    assert!(
        timeout(WAIT, handle.end_input_rx.recv())
            .await
            .expect("end_input was never signalled")
            .expect("end_input channel closed"),
        "end_input must run even when some inputs were skipped"
    );
}

/// A transport that refuses stdin writes must not abort the stream: every value
/// is still attempted and `end_input` is still signalled.
#[tokio::test]
async fn a_refused_stdin_write_does_not_abort_the_input_stream() {
    let (mut query, handles) = query_on_refusing_transport(None, HashMap::new());

    query
        .stream_input(futures::stream::iter(vec![json!("one"), json!("two")]))
        .await
        .expect("stream_input");

    let deadline = tokio::time::Instant::now() + WAIT;
    while handles.end_inputs.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "end_input was never attempted: {} input(s) sent",
            handles.inputs.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        handles.inputs.load(Ordering::SeqCst),
        2,
        "the second value must be attempted even though the first write failed"
    );
}

// ===========================================================================
// 9. The remaining control requests, and shutdown
// ===========================================================================

#[tokio::test]
async fn interrupt_sends_an_interrupt_control_request() {
    let (mut query, handle) = plain_query();
    query.start().await.expect("start");
    let responder = answer_once(
        handle.outbound_control_request_rx,
        handle.sdk_control_tx.clone(),
        |id| success_response(id, json!({})),
    );

    query.interrupt().await.expect("interrupt");
    let sent = responder.await.expect("responder");

    assert_eq!(sent["type"], "control_request");
    assert_eq!(sent["request"]["type"], "interrupt");
    assert!(
        sent["request_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("req_1_")),
        "request ids are numbered from one per Query: {sent}"
    );
}

#[tokio::test]
async fn rewind_files_sends_the_user_message_id() {
    let (mut query, handle) = plain_query();
    query.start().await.expect("start");
    let responder = answer_once(
        handle.outbound_control_request_rx,
        handle.sdk_control_tx.clone(),
        |id| success_response(id, json!({})),
    );

    query.rewind_files("msg-uuid-42").await.expect("rewind");
    let sent = responder.await.expect("responder");

    assert_eq!(sent["request"]["type"], "rewind_files");
    assert_eq!(
        sent["request"]["userMessageId"]
            .as_str()
            .or_else(|| sent["request"]["user_message_id"].as_str()),
        Some("msg-uuid-42"),
        "the message id must reach the CLI: {sent}"
    );
}

/// Successive requests from the same Query get successive counters, so a reply
/// can never be routed to the wrong caller.
#[tokio::test]
async fn request_ids_increase_across_calls() {
    let (mut query, handle) = plain_query();
    query.start().await.expect("start");
    let mut requests = handle.outbound_control_request_rx;
    let to_sdk = handle.sdk_control_tx.clone();

    let mut ids = Vec::new();
    for _ in 0..2 {
        let interrupt = query.interrupt();
        let pump = async {
            let request = requests.recv().await.expect("request");
            let id = request["request_id"].as_str().expect("id").to_string();
            to_sdk
                .send(json!({
                    "type": "control_response",
                    "response": success_response(&id, json!({})),
                }))
                .await
                .expect("reply");
            id
        };
        let (result, id) = tokio::join!(interrupt, pump);
        result.expect("interrupt");
        ids.push(id);
    }

    assert!(ids[0].starts_with("req_1_"), "first id: {:?}", ids[0]);
    assert!(ids[1].starts_with("req_2_"), "second id: {:?}", ids[1]);
    assert_ne!(ids[0], ids[1]);
}

#[tokio::test]
async fn close_disconnects_the_transport() {
    let (transport, _handle) = MockTransport::pair();
    let transport = Arc::new(Mutex::new(transport));
    let mut query = Query::new(transport.clone(), true, None, None, HashMap::new());
    transport.lock().await.connect().await.expect("connect");
    assert!(transport.lock().await.is_connected());

    query.close().await.expect("close");

    assert!(
        !transport.lock().await.is_connected(),
        "close() must disconnect the transport"
    );
}
