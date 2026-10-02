//! The lifetime of the background reader task `ClaudeSDKClientWorking::connect`
//! spawns, proved through the only thing it makes observable: its own farewell
//! log line.
//!
//! This lives in a file of its own because the assertion needs a **global**
//! `tracing` subscriber. `tracing` caches each callsite's interest per process, so
//! a thread-local subscriber loses the race against any other thread that reaches
//! the same `debug!` first; one test binary per file means one process per file,
//! and no contamination. The two halves below therefore run as a single test, in
//! order, against one capture buffer.
//!
//! What it establishes: the task does **not** end when the CLI dies — nothing in
//! the loop looks at the child, only at `state` — and the client goes on reporting
//! a live session. `disconnect()` is the only thing that stops it.

mod support;

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use nexus_claude::ClaudeSDKClientWorking;
use support::*;

/// The line `client_working.rs` writes when the reader loop breaks.
const ENDED: &str = "Message reader task ended";

/// A `MakeWriter` that appends every formatted event to a shared buffer.
#[derive(Clone, Default)]
struct Captured(Arc<StdMutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        let buffer = self.0.lock().expect("log buffer is not poisoned");
        String::from_utf8_lossy(&buffer).into_owned()
    }
}

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer is not poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn the_reader_task_survives_the_cli_and_ends_only_on_disconnect() {
    let logs = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("first statement of the only test in this binary");

    // A CLI that prints its handshake and leaves at once. Its stdout reaches EOF,
    // every broadcast sender is dropped, and the message stream the reader task
    // subscribes to is finished for good.
    let fake = Transcript::new().init("sess-gone").exit_with(0).build();
    let mut client = ClaudeSDKClientWorking::new(fake.options());
    client.connect(None).await.expect("connect");

    // Several full cycles of the loop (poll + 100 ms sleep + state check).
    tokio::time::sleep(Duration::from_millis(700)).await;

    assert!(
        !logs.text().contains(ENDED),
        "the reader task must still be spinning: nothing in its loop notices that \
         the CLI is gone, only `state` can stop it. Logs:\n{}",
        logs.text()
    );
    assert!(
        client.is_connected().await,
        "and the client still claims a live session over a dead child process"
    );

    client.disconnect().await.expect("disconnect");

    assert!(
        poll_until(Duration::from_secs(5), || logs.text().contains(ENDED)).await,
        "taking the transport away is the only thing that ends the task. Logs:\n{}",
        logs.text()
    );
}
