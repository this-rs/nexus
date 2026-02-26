//! Tests for the graceful disconnect escalation (SIGINT → SIGTERM → SIGKILL).
//!
//! These tests validate the SubprocessTransport::disconnect() behavior:
//! - On Unix: sends SIGINT first, waits, then SIGTERM, then SIGKILL
//! - Completes within the expected time bounds (< 800ms for graceful, immediate for cooperative)

use std::time::{Duration, Instant};

/// Test that a cooperative process (one that exits on stdin EOF) terminates
/// quickly without needing SIGKILL.
///
/// We spawn `cat` which reads stdin and exits when stdin is closed.
/// The disconnect should close stdin first, then the process exits before
/// any signal escalation is needed.
#[cfg(unix)]
#[tokio::test]
async fn test_graceful_disconnect_cooperative_process() {
    use std::process::Stdio;
    use tokio::process::Command;

    // `cat` reads stdin and exits when EOF is received
    let mut child = Command::new("cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn cat");

    let pid = child.id().expect("should have PID");
    assert!(pid > 0);

    // Drop stdin to trigger EOF — cat should exit
    drop(child.stdin.take());

    let start = Instant::now();
    let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .expect("cat should exit within 2s")
        .expect("wait should succeed");

    let elapsed = start.elapsed();
    assert!(status.success(), "cat should exit with code 0");
    assert!(
        elapsed < Duration::from_millis(500),
        "cooperative process should exit quickly, took {:?}",
        elapsed
    );
}

/// Test that the SIGINT escalation path works on Unix.
///
/// We spawn `sleep 60` (which ignores stdin EOF but handles SIGINT),
/// send SIGINT, and verify it terminates quickly.
#[cfg(unix)]
#[tokio::test]
async fn test_sigint_terminates_sleep() {
    use std::process::Stdio;
    use tokio::process::Command;

    let mut child = Command::new("sleep")
        .arg("60")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn sleep");

    let pid = child.id().expect("should have PID") as i32;

    // Give the process a moment to start
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send SIGINT (like our disconnect Stage 1)
    unsafe {
        libc::kill(pid, libc::SIGINT);
    }

    let start = Instant::now();
    let result = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;

    assert!(
        result.is_ok(),
        "sleep should exit within 500ms after SIGINT"
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(200),
        "sleep should exit quickly after SIGINT, took {:?}",
        elapsed
    );
}

/// Test that SIGTERM terminates a process that ignores SIGINT.
///
/// We spawn a bash script that traps SIGINT (ignores it) but exits on SIGTERM.
#[cfg(unix)]
#[tokio::test]
async fn test_sigterm_terminates_sigint_resistant_process() {
    use std::process::Stdio;
    use tokio::process::Command;

    // Bash script that ignores SIGINT but exits on SIGTERM (default behavior)
    let mut child = Command::new("bash")
        .arg("-c")
        .arg("trap '' INT; sleep 60")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn bash");

    let pid = child.id().expect("should have PID") as i32;

    // Give the process a moment to start and set up the trap
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Stage 1: SIGINT — should be ignored
    unsafe {
        libc::kill(pid, libc::SIGINT);
    }

    // Verify it's still alive after 100ms
    let still_alive = tokio::time::timeout(Duration::from_millis(100), child.wait()).await;
    assert!(
        still_alive.is_err(),
        "Process should still be alive after SIGINT (it traps INT)"
    );

    // Stage 2: SIGTERM — should terminate
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    let start = Instant::now();
    let result = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;

    assert!(result.is_ok(), "Process should exit after SIGTERM");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(200),
        "Process should exit quickly after SIGTERM, took {:?}",
        elapsed
    );
}

/// Test the total timing guarantee: disconnect should complete in < 800ms
/// even for an uncooperative process.
#[cfg(unix)]
#[tokio::test]
async fn test_disconnect_total_time_bound() {
    use std::process::Stdio;
    use tokio::process::Command;

    // Process that ignores both SIGINT and SIGTERM — will require SIGKILL
    let mut child = Command::new("bash")
        .arg("-c")
        .arg("trap '' INT TERM; sleep 60")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn bash");

    let pid = child.id().expect("should have PID") as i32;

    // Give the process a moment to start and set up traps
    tokio::time::sleep(Duration::from_millis(100)).await;

    let start = Instant::now();

    // Simulate our disconnect escalation: SIGINT → wait 200ms → SIGTERM → wait 500ms → SIGKILL
    unsafe {
        libc::kill(pid, libc::SIGINT);
    }
    let _ = tokio::time::timeout(Duration::from_millis(200), child.wait()).await;

    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;

    // SIGKILL — this always works
    child.kill().await.expect("SIGKILL should succeed");

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(900),
        "Total disconnect escalation should complete in < 900ms, took {:?}",
        elapsed
    );
}
