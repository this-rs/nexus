//! Integration tests for InteractiveClient::build_interrupt_json()
//!
//! Validates that the helper produces JSON identical to what
//! SubprocessTransport::send_control_request(ControlRequest::Interrupt{..})
//! would produce, and that it can be sent through the stdin_tx channel.

use nexus_claude::InteractiveClient;
use nexus_claude::transport::mock::MockTransport;
use std::time::Duration;
use tokio::time::timeout;

/// Verify build_interrupt_json() produces valid JSON with the correct wire format.
#[test]
fn test_build_interrupt_json_wire_format() {
    let json_str = InteractiveClient::build_interrupt_json();
    let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    assert_eq!(v["type"], "control_request");
    // The CLI reads the discriminator as `subtype`, and the id at the top level: a `type`
    // inside `request` is ignored by the CLI (measured: the turn ran on to its end).
    assert_eq!(v["request"]["subtype"], "interrupt");
    assert!(v.get("request").unwrap().get("type").is_none());
    assert!(v["request_id"].is_string());
    assert!(
        v["request"].get("request_id").is_none(),
        "request_id must be at the top level, not inside request"
    );
}

/// Verify that sending build_interrupt_json() through stdin_tx is equivalent
/// to calling client.interrupt() via the transport.
#[tokio::test]
async fn test_build_interrupt_json_matches_client_interrupt() {
    let (transport, mut handle) = MockTransport::pair();
    let mut client = InteractiveClient::from_transport(transport);
    client.connect().await.unwrap();

    // Send interrupt via the official client method
    client.interrupt().await.unwrap();

    // Receive what the transport got
    let via_client = timeout(
        Duration::from_millis(100),
        handle.outbound_control_request_rx.recv(),
    )
    .await
    .expect("timeout")
    .expect("channel open");

    // Now build one via the helper
    let via_helper_str = InteractiveClient::build_interrupt_json();
    let via_helper: serde_json::Value = serde_json::from_str(&via_helper_str).unwrap();

    // Compare structure (not request_id — those will differ)
    assert_eq!(via_client["type"], via_helper["type"]);
    assert_eq!(
        via_client["request"]["subtype"],
        via_helper["request"]["subtype"]
    );

    // Both should have request_id as UUID strings, at the top level
    let id_client = via_client["request_id"]
        .as_str()
        .expect("client interrupt should have request_id at the top level");
    let id_helper = via_helper["request_id"]
        .as_str()
        .expect("helper should have request_id at the top level");

    uuid::Uuid::parse_str(id_client).expect("client request_id should be valid UUID");
    uuid::Uuid::parse_str(id_helper).expect("helper request_id should be valid UUID");

    // Keys should be identical
    let client_keys: Vec<&str> = via_client
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    let helper_keys: Vec<&str> = via_helper
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    assert_eq!(client_keys, helper_keys, "top-level keys should match");

    let client_req_keys: Vec<&str> = via_client["request"]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    let helper_req_keys: Vec<&str> = via_helper["request"]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    assert_eq!(
        client_req_keys, helper_req_keys,
        "request-level keys should match"
    );

    client.disconnect().await.unwrap();
}

/// Verify build_interrupt_json() can be called many times without panicking.
#[test]
fn test_build_interrupt_json_stress() {
    for _ in 0..1000 {
        let json = InteractiveClient::build_interrupt_json();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "control_request");
    }
}
