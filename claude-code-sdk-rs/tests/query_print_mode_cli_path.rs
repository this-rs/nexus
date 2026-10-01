//! `query()` must honour an explicit `options.cli_path`.
//!
//! One test, in its own file, because it empties `PATH` and redirects `HOME` for
//! the whole process: that is the only way to prove the point rather than assert
//! it, since a machine with a real `claude` on its `PATH` would let the old
//! behaviour pass unnoticed — by launching the real CLI.
//!
//! Before the fix, `query_print_mode` called `find_claude_cli()` unconditionally
//! and threw `options.cli_path` away, so this test failed with `CliNotFound` and no
//! caller could point a one-shot query at a non-standard install (or at a test
//! double, which is why this file exists at all).

mod support;

use std::time::Duration;

use futures::StreamExt;
use nexus_claude::{ClaudeCodeOptions, Message, SdkError, find_claude_cli, query};
use support::*;

#[tokio::test]
async fn an_explicit_cli_path_is_used_even_when_no_cli_can_be_discovered() {
    let home = tempfile::tempdir().expect("temp home");
    // SAFETY: this file contains exactly one test, so nothing else in the process
    // reads or writes the environment while these are in effect.
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("USERPROFILE", home.path());
        std::env::set_var("PATH", "");
    }

    let error = find_claude_cli()
        .expect_err("precondition: an empty PATH and a fresh HOME hide every CLI location");
    assert!(
        matches!(error, SdkError::CliNotFound { .. }),
        "expected CliNotFound, got {error:?}"
    );

    // With nothing discoverable, a query that supplies no explicit path must fail
    // with `CliNotFound` — the discovery branch, and the only way to reach it in a
    // test without launching a real CLI.
    let error = query("salut", None)
        .await
        .err()
        .expect("no CLI anywhere means the query cannot start");
    assert!(
        matches!(error, SdkError::CliNotFound { .. }),
        "expected CliNotFound, got {error:?}"
    );

    let fake = Transcript::new()
        .init("sess-explicit")
        .assistant_text("trouve")
        .result_ok("trouve")
        .build();
    let options: ClaudeCodeOptions = fake.options();
    assert!(options.cli_path.is_some(), "the fake sets an explicit path");

    let stream = query("salut", Some(options))
        .await
        .expect("an explicit cli_path must be spawned without any discovery");
    let mut stream = Box::pin(stream);

    let mut messages = Vec::new();
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(item) = stream.next().await {
            messages.push(item.expect("no error was scripted"));
        }
    })
    .await;
    assert!(ended.is_ok(), "the stream must end when the fake exits");

    assert_eq!(messages.len(), 3, "the fake's scripted session, in full");
    assert!(matches!(messages[0], Message::System { .. }));
    assert_eq!(assistant_texts(&messages), vec!["trouve".to_string()]);

    // The recording proves which executable ran, not just that *something* did.
    let program = fake.invocation().raw()["program"]
        .as_str()
        .expect("the fake records its own argv[0]")
        .to_string();
    assert!(
        program.contains("fake_claude"),
        "the fake CLI should have been the program, got {program}"
    );
}
