//! Tests for InteractiveClient::set_permission_mode()
//!
//! Validates that the method sends the correct JSON control request
//! format and rejects invalid mode strings.

use nexus_claude::transport::mock::MockTransport;
use nexus_claude::InteractiveClient;

/// Helper: create an InteractiveClient backed by MockTransport
fn make_client() -> (InteractiveClient, nexus_claude::transport::mock::MockTransportHandle) {
    let (transport, handle) = MockTransport::pair();
    let client = InteractiveClient::from_transport(transport);
    (client, handle)
}

#[tokio::test]
async fn set_permission_mode_sends_correct_json_format() {
    let (mut client, mut handle) = make_client();
    client.connect().await.unwrap();

    // Send the mode change
    client.set_permission_mode("acceptEdits").await.unwrap();

    // Verify what was sent via the outbound_control_request_rx
    let req = handle.outbound_control_request_rx.recv().await.unwrap();

    // Top-level must have type = "control_request"
    assert_eq!(
        req.get("type").and_then(|v| v.as_str()),
        Some("control_request"),
        "Missing type: control_request"
    );

    // Must have a request_id (UUID)
    assert!(
        req.get("request_id")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()),
        "Missing or empty request_id"
    );

    // Inner request must have subtype and mode
    let inner = req.get("request").expect("Missing request field");
    assert_eq!(
        inner.get("subtype").and_then(|v| v.as_str()),
        Some("set_permission_mode"),
        "Wrong subtype"
    );
    assert_eq!(
        inner.get("mode").and_then(|v| v.as_str()),
        Some("acceptEdits"),
        "Wrong mode"
    );
}

#[tokio::test]
async fn set_permission_mode_all_valid_modes() {
    let valid_modes = ["default", "acceptEdits", "bypassPermissions", "plan"];

    for mode in &valid_modes {
        let (mut client, mut handle) = make_client();
        client.connect().await.unwrap();

        let result = client.set_permission_mode(mode).await;
        assert!(result.is_ok(), "Mode '{}' should be valid", mode);

        let req = handle.outbound_control_request_rx.recv().await.unwrap();
        let inner = req.get("request").unwrap();
        assert_eq!(
            inner.get("mode").and_then(|v| v.as_str()),
            Some(*mode),
            "Mode mismatch for '{}'",
            mode
        );
    }
}

#[tokio::test]
async fn set_permission_mode_rejects_invalid_mode() {
    let (mut client, _handle) = make_client();
    client.connect().await.unwrap();

    let result = client.set_permission_mode("invalid_mode").await;
    assert!(result.is_err(), "Invalid mode should be rejected");

    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("Invalid permission mode"),
        "Error should mention invalid mode, got: {}",
        err_msg
    );
}

#[tokio::test]
async fn set_permission_mode_fails_when_not_connected() {
    let (mut client, _handle) = make_client();
    // Do NOT call connect()

    let result = client.set_permission_mode("default").await;
    assert!(result.is_err(), "Should fail when not connected");

    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("Not connected"),
        "Error should mention not connected, got: {}",
        err_msg
    );
}

#[tokio::test]
async fn set_permission_mode_unique_request_ids() {
    let (mut client, mut handle) = make_client();
    client.connect().await.unwrap();

    // Send two mode changes
    client.set_permission_mode("default").await.unwrap();
    client
        .set_permission_mode("bypassPermissions")
        .await
        .unwrap();

    let req1 = handle.outbound_control_request_rx.recv().await.unwrap();
    let req2 = handle.outbound_control_request_rx.recv().await.unwrap();

    let id1 = req1
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();
    let id2 = req2
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    assert_ne!(id1, id2, "Each request must have a unique request_id");
}
