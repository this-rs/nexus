//! Several fakes appending to ONE record file at the same time must leave every
//! line readable.
//!
//! The backend's e2e tests open two sessions together on one fake and read one
//! record (`agent_e2e_tests::two_first_sessions_opened_at_once_on_a_refusing_agent_both_open`).
//! With `writeln!` on a `File`, every formatted fragment of the JSON line was its
//! own write(2); two fakes interleaved their fragments, both lines stopped
//! parsing, and the test read an EMPTY record (`asked == []`). Seen twice on the
//! self-hosted runner ffs-n2 (backend runs 39298350837 and 39306976512), where
//! two fakes really do start in the same millisecond. The fix is one write per
//! line; this test is the one that goes red without it.

use std::process::{Command, Stdio};

use serde_json::Value;

const FAKE_ACP: &str = env!("CARGO_BIN_EXE_fake_acp");
const FAKE_CODEX: &str = env!("CARGO_BIN_EXE_fake_codex");

/// Enough fakes to collide on a 16-thread machine, enough rounds to make the
/// collision near certain without the fix; cheap with it (each fake exits at EOF).
const FAKES: usize = 16;
const ROUNDS: usize = 4;

/// Starts `FAKES` copies of `fake` at once, all appending to one record, with an
/// empty transcript and a closed stdin: each one writes its `start` entry (argv,
/// environment names — a long line, many fragments) and its `exit` entry, then
/// leaves. Every line must parse, and every fake must be seen twice.
fn every_line_of_a_shared_record_parses(fake: &str, args: &[&str], env_prefix: &str) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let transcript = dir.path().join("empty.jsonl");
    std::fs::write(&transcript, "").expect("an empty transcript");

    for round in 0..ROUNDS {
        let record = dir.path().join(format!("record-{round}.jsonl"));
        let children: Vec<_> = (0..FAKES)
            .map(|_| {
                Command::new(fake)
                    .args(args)
                    .env(format!("{env_prefix}_TRANSCRIPT"), &transcript)
                    .env(format!("{env_prefix}_RECORD"), &record)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("the fake starts")
            })
            .collect();
        for mut child in children {
            let status = child.wait().expect("the fake is waited for");
            assert!(status.success(), "round {round}: a fake failed: {status}");
        }

        let text = std::fs::read_to_string(&record).expect("the record was written");
        let (mut starts, mut exits) = (0, 0);
        for (number, line) in text.lines().enumerate() {
            let value: Value = serde_json::from_str(line).unwrap_or_else(|error| {
                panic!(
                    "round {round}, line {}: not JSON ({error}) — two fakes interleaved \
                     their fragments: {line:?}",
                    number + 1
                )
            });
            match value["kind"].as_str() {
                Some("start") => starts += 1,
                Some("exit") => exits += 1,
                other => panic!(
                    "round {round}, line {}: unexpected kind {other:?}",
                    number + 1
                ),
            }
        }
        assert_eq!(
            (starts, exits),
            (FAKES, FAKES),
            "round {round}: every fake writes one start and one exit\n{text}"
        );
    }
}

#[test]
fn fake_acp_records_from_concurrent_processes_stay_one_json_line_each() {
    every_line_of_a_shared_record_parses(FAKE_ACP, &[], "FAKE_ACP");
}

#[test]
fn fake_codex_records_from_concurrent_processes_stay_one_json_line_each() {
    // Like the real codex, the fake only serves as `codex app-server`.
    every_line_of_a_shared_record_parses(FAKE_CODEX, &["app-server"], "FAKE_CODEX");
}
