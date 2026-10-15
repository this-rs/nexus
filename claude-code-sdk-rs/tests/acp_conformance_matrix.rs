//! The ACP conformance matrix: the generated doc, and the negative contract that
//! keeps the doc and the registry identical.
//!
//! `docs/acp-conformance.md` is generated from `support::acp_matrix::ACP_AGENTS`.
//! `the_conformance_matrix_matches_the_doc` is the doc↔code contract: the table
//! between the sentinels must equal `matrix_markdown()` byte-for-byte, so a
//! registry edit without a doc regeneration fails the gate (and vice-versa).
//!
//! Regenerate the doc with
//! `UPDATE_ACP_MATRIX=1 cargo test -p nexus-claude --features provider-acp --test acp_conformance_matrix`.

mod support;

use std::path::PathBuf;

use support::acp_matrix::*;

fn doc_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/acp-conformance.md")
}

/// The fixed prose around the generated table. Everything after the final line
/// here is the sentinel + the generated table, assembled by [`doc_body`].
const PREAMBLE: &str = r#"# ACP conformance matrix

The gate's answer to "which ACP agents does this SDK guarantee, and how is each
claim established?". The table is generated from `tests/support/acp_matrix.rs`
(the single source of truth, a data table) and is kept byte-for-byte identical
to it by `the_conformance_matrix_matches_the_doc`. A future "which agents do we
guarantee" query is answered by reading `ACP_AGENTS` there, never by scraping
this file.

**No live agent was ever run.** Every `Verified` claim is established offline:
`fake_acp` replays JSONL transcripts written from the public Agent Client
Protocol specification (version 1). `opencode`, a Gemini CLI and openclaw were
never executed; their rows below record what was read, and what was not.

## How to add or re-verify an agent

1. Add an `AcpAgentEntry` to `ACP_AGENTS` in `tests/support/acp_matrix.rs`, with
   `status: Verification::NotVerified` until a run actually proves it offline.
2. If that run establishes a new behaviour, add its name to `ACP_BEHAVIOURS` and
   wire it into the proof — `every_matrix_behaviour_is_a_known_behaviour` and
   `the_matrix_says_fake_acp_is_verified_for_exactly_what_the_suite_proves` fail
   otherwise.
3. Regenerate this file:

   ```text
   UPDATE_ACP_MATRIX=1 cargo test -p nexus-claude --features provider-acp --test acp_conformance_matrix
   ```

The items the gate does **not** prove stay in
`tests/transcripts/acp/1/schema/PROVENANCE.md` (the "NOT VERIFIED" table): the
`usage` shape of a `session/prompt` result, `usage_update` fields, `messageId`,
`rawOutput`, `availableCommands`, `session/load` result modes, the `-32000`
auth-refusal answer, the agent-published mode ids, opencode's `<server>_<tool>`
spelling, and the launch arguments of real agents."#;

/// The whole `docs/acp-conformance.md` body: preamble, start sentinel, the
/// generated table, end sentinel.
fn doc_body() -> String {
    format!(
        "{PREAMBLE}\n<!-- ACP-MATRIX:START -->\n{}<!-- ACP-MATRIX:END -->\n",
        matrix_markdown()
    )
}

/// The slice strictly between the two sentinels — exactly what [`matrix_markdown`]
/// must reproduce.
fn matrix_slice(doc: &str) -> String {
    const START: &str = "<!-- ACP-MATRIX:START -->\n";
    const END: &str = "<!-- ACP-MATRIX:END -->";
    let start = doc
        .find(START)
        .expect("docs/acp-conformance.md has the ACP-MATRIX:START sentinel")
        + START.len();
    let end = doc
        .find(END)
        .expect("docs/acp-conformance.md has the ACP-MATRIX:END sentinel");
    doc[start..end].to_owned()
}

#[test]
fn the_conformance_matrix_matches_the_doc() {
    let path = doc_path();
    if std::env::var_os("UPDATE_ACP_MATRIX").is_some() {
        std::fs::write(&path, doc_body()).expect("write docs/acp-conformance.md");
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "{} is missing: regenerate it with UPDATE_ACP_MATRIX=1 cargo test \
             -p nexus-claude --features provider-acp --test acp_conformance_matrix",
            path.display()
        )
    });
    assert_eq!(
        matrix_slice(&committed),
        matrix_markdown(),
        "the matrix in docs/acp-conformance.md diverged from ACP_AGENTS in \
         tests/support/acp_matrix.rs; regenerate with UPDATE_ACP_MATRIX=1"
    );
}

#[test]
fn every_matrix_behaviour_is_a_known_behaviour() {
    for entry in ACP_AGENTS {
        let mut seen: Vec<&str> = Vec::new();
        for behaviour in entry.verified_behaviours {
            assert!(
                ACP_BEHAVIOURS.contains(behaviour),
                "`{}` (agent `{}`) is not in ACP_BEHAVIOURS",
                behaviour,
                entry.id
            );
            assert!(
                !seen.contains(behaviour),
                "`{}` (agent `{}`) is listed twice",
                behaviour,
                entry.id
            );
            seen.push(behaviour);
        }
    }
}

#[test]
fn fake_acp_is_the_only_fully_verified_agent_and_it_is_verified_offline() {
    let verified: Vec<&AcpAgentEntry> = ACP_AGENTS
        .iter()
        .filter(|entry| entry.status == Verification::Verified)
        .collect();
    assert_eq!(verified.len(), 1, "exactly one agent may be `Verified`");
    let fake = verified[0];
    assert_eq!(fake.id, "fake_acp");
    assert!(
        fake.evidence.contains("offline") && fake.evidence.contains("fake_acp"),
        "the only `Verified` claim must be offline on fake_acp, got: {}",
        fake.evidence
    );
    for id in ["opencode", "gemini_cli"] {
        let entry = ACP_AGENTS.iter().find(|e| e.id == id).expect(id);
        assert_eq!(
            entry.status,
            Verification::NotVerified,
            "a claim that `{id}` (a real agent) was run cannot slip in silently"
        );
    }
}
