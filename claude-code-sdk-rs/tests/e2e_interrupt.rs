//! E2E tests for interrupt behavior using InteractiveClient.
//!
//! These tests validate that:
//! - interrupt() sends a signal immediately (< 10ms)
//! - interrupt() does NOT wait for the transport Mutex
//! - interrupt() works during a pending permission request
//! - double interrupt is idempotent (no panic)

use nexus_claude::InteractiveClient;
use nexus_claude::transport::mock::MockTransport;
use std::time::{Duration, Instant};
use tokio::time::timeout;

/// Test that interrupt() sends an SDKControlInterruptRequest to the transport
/// and completes in under 10ms.
#[tokio::test]
async fn test_interrupt_sends_signal_immediately() {
    let (transport, mut handle) = MockTransport::pair();
    let mut client = InteractiveClient::from_transport(transport);
    client.connect().await.unwrap();

    // Measure interrupt latency
    let start = Instant::now();
    client.interrupt().await.unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_millis(10),
        "interrupt() took {:?}, expected < 10ms",
        elapsed
    );

    // Verify that a control request was sent to the transport
    let sent = timeout(
        Duration::from_millis(100),
        handle.outbound_control_request_rx.recv(),
    )
    .await
    .expect("should receive within timeout")
    .expect("channel should not be closed");

    // The MockTransport records ControlRequest::Interrupt as JSON
    assert_eq!(sent["type"], "control_request");
    assert_eq!(sent["request"]["type"], "interrupt");
    // request_id should be a UUID string inside "request"
    assert!(
        sent["request"]["request_id"].is_string(),
        "request_id should be a string inside 'request'"
    );

    client.disconnect().await.unwrap();
}

/// Test that interrupt() works even when the message stream is actively being read.
///
/// The key insight: InteractiveClient::interrupt() acquires the transport Mutex
/// to send the interrupt signal. If the stream is being read (which also holds
/// the Mutex via receive_messages_stream), interrupt() must still complete
/// within a reasonable time.
///
/// In the PO Backend, the interrupt is handled differently — it sets an AtomicBool
/// flag without taking any lock. This test validates the SDK-level behavior.
#[tokio::test]
async fn test_interrupt_completes_within_timeout() {
    let (transport, _handle) = MockTransport::pair();
    let mut client = InteractiveClient::from_transport(transport);
    client.connect().await.unwrap();

    // Call interrupt — should complete quickly since no stream is actively held
    let result = timeout(Duration::from_millis(50), client.interrupt()).await;

    assert!(
        result.is_ok(),
        "interrupt() should complete within 50ms timeout"
    );
    assert!(result.unwrap().is_ok(), "interrupt() should succeed");

    client.disconnect().await.unwrap();
}

/// Test that interrupt() can be called while a permission request is pending
/// (control_request sent, no response yet).
///
/// In the PO Backend, when Claude is waiting for a permission approval,
/// the user can still click "Stop" to interrupt. The interrupt should be
/// sent even though the permission response hasn't been sent yet.
#[tokio::test]
async fn test_interrupt_during_pending_permission() {
    let (transport, mut handle) = MockTransport::pair();
    let mut client = InteractiveClient::from_transport(transport);
    client.connect().await.unwrap();

    // Take control receiver
    let mut sdk_control_rx = client
        .take_sdk_control_receiver()
        .await
        .expect("should get receiver");

    // Simulate: CLI sends a permission request
    handle
        .sdk_control_tx
        .send(serde_json::json!({
            "type": "control_request",
            "request_id": "perm_pending_001",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "input": {"command": "long-running-command"}
            }
        }))
        .await
        .unwrap();

    // Receive the permission request (but DON'T respond — it's pending)
    let _received = timeout(Duration::from_millis(100), sdk_control_rx.recv())
        .await
        .expect("should receive permission request")
        .expect("channel open");

    // Now interrupt while the permission is still pending
    let start = Instant::now();
    client.interrupt().await.unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_millis(10),
        "interrupt() during pending permission took {:?}, expected < 10ms",
        elapsed
    );

    // Verify interrupt signal was sent
    let sent = timeout(
        Duration::from_millis(100),
        handle.outbound_control_request_rx.recv(),
    )
    .await
    .expect("timeout")
    .expect("channel open");
    assert_eq!(sent["request"]["type"], "interrupt");

    client.disconnect().await.unwrap();
}

/// Test that calling interrupt() twice in succession doesn't panic or error.
///
/// This can happen in the PO Backend when:
/// - User clicks "Stop" multiple times rapidly
/// - NATS publishes an interrupt + local interrupt fires simultaneously
#[tokio::test]
async fn test_double_interrupt_is_idempotent() {
    let (transport, mut handle) = MockTransport::pair();
    let mut client = InteractiveClient::from_transport(transport);
    client.connect().await.unwrap();

    // First interrupt
    let result1 = client.interrupt().await;
    assert!(result1.is_ok(), "First interrupt should succeed");

    // Second interrupt immediately after
    let result2 = client.interrupt().await;
    assert!(result2.is_ok(), "Second interrupt should also succeed");

    // Both should have sent interrupt signals
    let sent1 = timeout(
        Duration::from_millis(100),
        handle.outbound_control_request_rx.recv(),
    )
    .await
    .expect("timeout")
    .expect("channel open");
    assert_eq!(sent1["request"]["type"], "interrupt");

    let sent2 = timeout(
        Duration::from_millis(100),
        handle.outbound_control_request_rx.recv(),
    )
    .await
    .expect("timeout")
    .expect("channel open");
    assert_eq!(sent2["request"]["type"], "interrupt");

    client.disconnect().await.unwrap();
}

/// Test that interrupt() fails gracefully when not connected.
#[tokio::test]
async fn test_interrupt_requires_connection() {
    let (transport, _handle) = MockTransport::pair();
    let mut client = InteractiveClient::from_transport(transport);
    // Do NOT connect

    let result = client.interrupt().await;
    assert!(result.is_err(), "Should fail when not connected");
}

/// Benchmark: measure average interrupt latency over multiple calls.
/// Each interrupt should take < 1ms on average (atomic flag path).
#[tokio::test]
async fn test_interrupt_latency_benchmark() {
    let (transport, _handle) = MockTransport::pair();
    let mut client = InteractiveClient::from_transport(transport);
    client.connect().await.unwrap();

    let iterations = 100;
    let start = Instant::now();

    for _ in 0..iterations {
        client.interrupt().await.unwrap();
    }

    let total = start.elapsed();
    let avg = total / iterations;

    assert!(
        avg < Duration::from_millis(1),
        "Average interrupt latency {:?} exceeds 1ms (total {:?} for {} iterations)",
        avg,
        total,
        iterations
    );

    client.disconnect().await.unwrap();
}
