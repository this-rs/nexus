//! E2E tests for the permission request/response flow using InteractiveClient.
//!
//! These tests validate the pattern used by the PO Backend ChatManager:
//! - InteractiveClient + take_sdk_control_receiver() to listen for permission requests
//! - send_control_response() to allow/deny tool usage
//!
//! Unlike the existing e2e_control.rs tests which use Query + CanUseTool callback,
//! these tests exercise the raw control channel pattern (no callback).

use nexus_claude::InteractiveClient;
use nexus_claude::transport::mock::MockTransport;
use serde_json::json;
use std::time::Duration;
use tokio::time::timeout;

/// Test that a control_request injected via the mock transport is received
/// by the InteractiveClient's take_sdk_control_receiver() channel.
///
/// This is the exact pattern the PO Backend uses: it takes the receiver
/// and listens for permission requests alongside the message stream.
#[tokio::test]
async fn test_permission_request_forwarded_to_sdk_control_rx() {
    let (transport, handle) = MockTransport::pair();
    let mut client = InteractiveClient::from_transport(transport);
    client.connect().await.unwrap();

    // Take the SDK control receiver (PO Backend pattern)
    let mut sdk_control_rx = client
        .take_sdk_control_receiver()
        .await
        .expect("should get control receiver");

    // Simulate CLI sending a can_use_tool permission request
    let control_request = json!({
        "type": "control_request",
        "request_id": "perm_001",
        "request": {
            "subtype": "can_use_tool",
            "tool_name": "Bash",
            "input": {"command": "echo Hello World"}
        }
    });
    handle
        .sdk_control_tx
        .send(control_request.clone())
        .await
        .unwrap();

    // Verify the control request is received on the channel
    let received = timeout(Duration::from_millis(100), sdk_control_rx.recv())
        .await
        .expect("should receive within timeout")
        .expect("channel should not be closed");

    // Verify the message structure
    assert_eq!(received["type"], "control_request");
    assert_eq!(received["request_id"], "perm_001");
    assert_eq!(received["request"]["subtype"], "can_use_tool");
    assert_eq!(received["request"]["tool_name"], "Bash");
    assert_eq!(received["request"]["input"]["command"], "echo Hello World");

    client.disconnect().await.unwrap();
}

/// Test that take_sdk_control_receiver() can only be called once.
/// Subsequent calls return None (the receiver has been moved).
#[tokio::test]
async fn test_take_sdk_control_receiver_only_once() {
    let (transport, _handle) = MockTransport::pair();
    let client = InteractiveClient::from_transport(transport);

    // First call: should succeed
    let rx1 = client.take_sdk_control_receiver().await;
    assert!(rx1.is_some(), "First call should return Some");

    // Second call: should return None
    let rx2 = client.take_sdk_control_receiver().await;
    assert!(rx2.is_none(), "Second call should return None");
}

/// Test that send_control_response({allow: true}) sends the correct
/// JSON structure to the transport (which forwards to CLI stdin).
///
/// The transport wraps the response in:
/// {"type": "control_response", "response": <payload>}
#[tokio::test]
async fn test_permission_response_allow_sent_to_cli() {
    let (transport, mut handle) = MockTransport::pair();
    let mut client = InteractiveClient::from_transport(transport);
    client.connect().await.unwrap();

    // Send allow response
    let response = json!({"allow": true});
    client.send_control_response(response).await.unwrap();

    // Verify the transport received the wrapped response
    let sent = timeout(
        Duration::from_millis(100),
        handle.outbound_control_rx.recv(),
    )
    .await
    .expect("should receive within timeout")
    .expect("channel should not be closed");

    assert_eq!(sent["type"], "control_response");
    assert_eq!(sent["response"]["allow"], true);

    client.disconnect().await.unwrap();
}

/// Test that send_control_response({allow: false, reason: "..."}) sends
/// a denial with the reason field preserved.
#[tokio::test]
async fn test_permission_response_deny_sent_to_cli() {
    let (transport, mut handle) = MockTransport::pair();
    let mut client = InteractiveClient::from_transport(transport);
    client.connect().await.unwrap();

    // Send deny response with reason
    let response = json!({
        "allow": false,
        "reason": "User denied: dangerous command"
    });
    client.send_control_response(response).await.unwrap();

    // Verify the transport received the wrapped response
    let sent = timeout(
        Duration::from_millis(100),
        handle.outbound_control_rx.recv(),
    )
    .await
    .expect("should receive within timeout")
    .expect("channel should not be closed");

    assert_eq!(sent["type"], "control_response");
    assert_eq!(sent["response"]["allow"], false);
    assert_eq!(sent["response"]["reason"], "User denied: dangerous command");

    client.disconnect().await.unwrap();
}

/// Test the full round-trip: receive permission request → send allow → verify.
///
/// Simulates the complete flow as it happens in the PO Backend ChatManager:
/// 1. CLI sends can_use_tool control_request
/// 2. Backend receives it via sdk_control_rx
/// 3. Backend (or frontend via WS) decides to allow
/// 4. Backend sends control_response({allow: true})
/// 5. CLI receives the response and proceeds
#[tokio::test]
async fn test_permission_full_roundtrip_allow() {
    let (transport, mut handle) = MockTransport::pair();
    let mut client = InteractiveClient::from_transport(transport);
    client.connect().await.unwrap();

    // Take control receiver
    let mut sdk_control_rx = client
        .take_sdk_control_receiver()
        .await
        .expect("should get receiver");

    // 1. CLI sends permission request
    let request = json!({
        "type": "control_request",
        "request_id": "roundtrip_001",
        "request": {
            "subtype": "can_use_tool",
            "tool_name": "Edit",
            "input": {
                "file_path": "/tmp/test.txt",
                "old_string": "foo",
                "new_string": "bar"
            },
            "permission_suggestions": []
        }
    });
    handle.sdk_control_tx.send(request).await.unwrap();

    // 2. Receive on control channel
    let received = timeout(Duration::from_millis(100), sdk_control_rx.recv())
        .await
        .expect("timeout")
        .expect("channel open");
    assert_eq!(received["request"]["tool_name"], "Edit");

    // 3. Send allow response
    client
        .send_control_response(json!({"allow": true}))
        .await
        .unwrap();

    // 4. Verify the response was sent to CLI
    let sent = timeout(
        Duration::from_millis(100),
        handle.outbound_control_rx.recv(),
    )
    .await
    .expect("timeout")
    .expect("channel open");
    assert_eq!(sent["type"], "control_response");
    assert_eq!(sent["response"]["allow"], true);

    client.disconnect().await.unwrap();
}

/// Test the full round-trip with denial.
#[tokio::test]
async fn test_permission_full_roundtrip_deny() {
    let (transport, mut handle) = MockTransport::pair();
    let mut client = InteractiveClient::from_transport(transport);
    client.connect().await.unwrap();

    let mut sdk_control_rx = client
        .take_sdk_control_receiver()
        .await
        .expect("should get receiver");

    // CLI sends permission request for a dangerous command
    let request = json!({
        "type": "control_request",
        "request_id": "deny_001",
        "request": {
            "subtype": "can_use_tool",
            "tool_name": "Bash",
            "input": {"command": "rm -rf /"}
        }
    });
    handle.sdk_control_tx.send(request).await.unwrap();

    // Receive on control channel
    let received = timeout(Duration::from_millis(100), sdk_control_rx.recv())
        .await
        .expect("timeout")
        .expect("channel open");
    assert_eq!(received["request"]["tool_name"], "Bash");
    assert_eq!(received["request"]["input"]["command"], "rm -rf /");

    // Deny the request
    client
        .send_control_response(json!({
            "allow": false,
            "reason": "Dangerous command blocked by policy"
        }))
        .await
        .unwrap();

    // Verify denial sent
    let sent = timeout(
        Duration::from_millis(100),
        handle.outbound_control_rx.recv(),
    )
    .await
    .expect("timeout")
    .expect("channel open");
    assert_eq!(sent["response"]["allow"], false);
    assert_eq!(
        sent["response"]["reason"],
        "Dangerous command blocked by policy"
    );

    client.disconnect().await.unwrap();
}

/// Test that multiple sequential permission requests can be handled.
///
/// This validates that the control channel doesn't get stuck after
/// one request/response cycle.
#[tokio::test]
async fn test_multiple_sequential_permission_requests() {
    let (transport, mut handle) = MockTransport::pair();
    let mut client = InteractiveClient::from_transport(transport);
    client.connect().await.unwrap();

    let mut sdk_control_rx = client
        .take_sdk_control_receiver()
        .await
        .expect("should get receiver");

    // Send 3 permission requests in sequence
    for i in 0..3 {
        let tool = match i {
            0 => "Read",
            1 => "Bash",
            _ => "Write",
        };

        // CLI sends request
        handle
            .sdk_control_tx
            .send(json!({
                "type": "control_request",
                "request_id": format!("seq_{}", i),
                "request": {
                    "subtype": "can_use_tool",
                    "tool_name": tool,
                    "input": {}
                }
            }))
            .await
            .unwrap();

        // Receive
        let received = timeout(Duration::from_millis(100), sdk_control_rx.recv())
            .await
            .expect("timeout")
            .expect("channel open");
        assert_eq!(received["request"]["tool_name"], tool);

        // Allow
        client
            .send_control_response(json!({"allow": true}))
            .await
            .unwrap();

        // Verify sent
        let sent = timeout(
            Duration::from_millis(100),
            handle.outbound_control_rx.recv(),
        )
        .await
        .expect("timeout")
        .expect("channel open");
        assert_eq!(sent["response"]["allow"], true);
    }

    client.disconnect().await.unwrap();
}

/// Test that the control channel works correctly when the client is not connected.
/// send_control_response should fail with InvalidState.
#[tokio::test]
async fn test_send_control_response_requires_connection() {
    let (transport, _handle) = MockTransport::pair();
    let mut client = InteractiveClient::from_transport(transport);
    // Do NOT connect

    let result = client.send_control_response(json!({"allow": true})).await;
    assert!(result.is_err(), "Should fail when not connected");
}
