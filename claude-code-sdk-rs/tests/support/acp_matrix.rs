//! The ACP conformance matrix: the single source of truth for "which ACP agents
//! does this SDK guarantee, and how is each claim established?".
//!
//! This is data, not prose. `docs/acp-conformance.md` is generated from it (via
//! [`matrix_markdown`]), and `tests/acp_conformance_matrix.rs` keeps the two
//! byte-for-byte identical, so a future "which agents do we guarantee" query is
//! answered by reading `ACP_AGENTS` here — never by scraping the doc.
//!
//! Every `Verified` claim is established offline. No real ACP agent (opencode,
//! Gemini CLI, openclaw) was ever run; see
//! `tests/transcripts/acp/1/schema/PROVENANCE.md` for what that leaves unproven.

#![allow(dead_code)] // compiled per test file, like support/mod.rs

/// How a row's claim was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    Verified,
    NotVerified,
}

/// One ACP agent this SDK knows about, and what the gate claims about it.
pub struct AcpAgentEntry {
    /// Stable id, quoted verbatim in `docs/acp-conformance.md`.
    pub id: &'static str,
    /// Product/command identifier (display only).
    pub product: &'static str,
    pub status: Verification,
    /// How the claim is established — must contain one of the markers below.
    pub evidence: &'static str,
    /// Subset of [`ACP_BEHAVIOURS`] this agent is verified against (empty when
    /// `NotVerified`).
    pub verified_behaviours: &'static [&'static str],
}

/// The closed set of behaviour names the gate knows. Adding a behaviour here (or
/// a new `wire::MESSAGE_KINDS` entry) without wiring it into the proof MUST fail
/// a test — see `the_matrix_says_fake_acp_is_verified_for_exactly_what_the_suite_proves`.
pub const ACP_BEHAVIOURS: &[&str] = &[
    // The 21 `wire::MESSAGE_KINDS`, verbatim and in that order.
    "initialize.params",
    "initialize.result",
    "session/new.params",
    "session/new.result",
    "session/load.params",
    "session/load.result",
    "session/prompt.params",
    "session/prompt.result",
    "session/cancel.params",
    "session/set_mode.params",
    "session/update:agent_message_chunk",
    "session/update:agent_thought_chunk",
    "session/update:tool_call",
    "session/update:tool_call_update",
    "session/update:plan",
    "session/update:available_commands_update",
    "session/update:current_mode_update",
    "session/update:usage_update",
    "session/update:user_message_chunk",
    "session/request_permission.params",
    "session/request_permission.response",
    // The capability/behaviour names the contract §5 gates.
    "resume",
    "thinking",
    "tools",
    "per_session_mcp",
    "interactive_permissions",
    "permission_scopes",
    "cost",
    "context_window",
    "secret_isolation",
    "images_fallback",
    "hooks_fallback",
    "subagents_fallback",
    "compaction_signal_fallback",
    "background_tasks_fallback",
    "tool_cancel_fallback",
    "native_question_fallback",
    "set_model_live_fallback",
    "sandbox_fallback",
    "mcp_servers_refused_retry",
];

pub const ACP_AGENTS: &[AcpAgentEntry] = &[
    AcpAgentEntry {
        id: "fake_acp",
        product: "fake_acp (protocol v1)",
        status: Verification::Verified,
        evidence: "offline: AcpProvider on fake_acp replaying spec-derived JSONL transcripts",
        verified_behaviours: ACP_BEHAVIOURS, // the full closed set
    },
    AcpAgentEntry {
        id: "opencode",
        product: "opencode acp",
        status: Verification::NotVerified,
        evidence: "never run; <server>_<tool> MCP title spelling read from the provider review",
        verified_behaviours: &[],
    },
    AcpAgentEntry {
        id: "gemini_cli",
        product: "Gemini CLI (ACP)",
        status: Verification::NotVerified,
        evidence: "never run",
        verified_behaviours: &[],
    },
    AcpAgentEntry {
        id: "openclaw",
        product: "openclaw acp (2026.9.x)",
        status: Verification::NotVerified,
        evidence: "published code read, not run",
        verified_behaviours: &["mcp_servers_refused_retry"], // the ONE read-from-code behaviour
    },
];

/// The markdown table (no sentinels) that `docs/acp-conformance.md` embeds
/// verbatim between `<!-- ACP-MATRIX:START -->` and `<!-- ACP-MATRIX:END -->`.
pub fn matrix_markdown() -> String {
    let mut out = String::from("| agent | status | evidence | verified behaviours |\n");
    out.push_str("| --- | --- | --- | --- |\n");
    for entry in ACP_AGENTS {
        let status = match entry.status {
            Verification::Verified => "Verified",
            Verification::NotVerified => "Not verified",
        };
        let behaviours = if entry.verified_behaviours.is_empty() {
            "—".to_owned()
        } else {
            entry.verified_behaviours.join(", ")
        };
        out.push_str(&format!(
            "| `{}` {} | {} | {} | {} |\n",
            entry.id, entry.product, status, entry.evidence, behaviours
        ));
    }
    out
}
