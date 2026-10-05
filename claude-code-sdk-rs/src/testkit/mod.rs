//! Test kit of the agent contract (feature `testkit`).
//!
//! Five pieces, usable from another crate with `features = ["testkit"]`:
//!
//! - [`claude_replay`]: [`claude_code_replay`], the **real** Claude Code provider
//!   on an in-memory replay transport that plays a list of messages and captures
//!   every line written to the CLI's stdin. No executable needed.
//! - [`scripted`]: [`ScriptedProvider`], an in-memory [`AgentProvider`](crate::agent::AgentProvider)
//!   that plays a serialisable [`Script`] and applies the concurrency rules and the
//!   capability fallbacks of `docs/agent-contract.md` (§5, §9, §10). It is the fake
//!   provider of a host's tests: no process, no network.
//! - [`transcript`]: recording of real turns into a canonical JSON [`Transcript`],
//!   normalisation of what changes from run to run, and replay through
//!   [`Script::from_transcript`].
//! - [`conformance`]: the conformance suite. The **same** scenarios run against every
//!   provider through a [`ConformanceTarget`]; an absent capability does not skip a
//!   scenario, the suite then verifies the fallback written in §5.
//! - [`security`]: the mandatory security scenarios (A32, A33, A35). Unlike the
//!   conformance suite they are conditioned by no capability, and each one is
//!   proven red against a deliberately faulty provider before it counts.

pub mod claude_replay;
pub mod conformance;
pub mod scripted;
pub mod security;
pub mod transcript;

pub use claude_replay::{
    ClaudeCodeReplay, ReplayStep, claude_code_replay, claude_code_replay_steps,
};
pub use conformance::{
    ConformanceReport, ConformanceTarget, Prepared, Scenario, ScenarioOutcome, ScriptedTarget,
    StreamKind, capability_present, check_stream_invariants, run_all, run_scenario,
    scripted_target,
};
pub use scripted::{
    RecordedCall, Script, ScriptBuilder, ScriptedProvider, Step, done_event, steps,
};
pub use security::{
    HOST_VARIABLE, LaunchObservation, SECRET_SENTINEL, SecurityReport, SecurityScenario,
    SecurityStaging, SecurityTarget, SecurityVerdict, ToolProfileProbe, Violation,
};
pub use transcript::{Recorder, Transcript, normalize, normalize_with_cwd, record_turn};

use crate::agent::{
    Capabilities, ContextWindow, ContextWindowSource, CostBasis, HookSupport, PermissionScope,
    SandboxLevel, SubagentSupport,
};

/// Every capability present: the starting point of a script that should not be
/// limited by a fallback.
///
/// The sandbox is `workspace` (so the `trust` mode opens), sub-agents are
/// `nested`, hooks are `in_protocol`, the cost is `reported` and the context
/// window is a `configured` 200 000 tokens.
pub fn full_capabilities() -> Capabilities {
    let mut capabilities = Capabilities::none();
    capabilities.interactive_permissions = true;
    capabilities.permission_scopes = vec![
        PermissionScope::Once,
        PermissionScope::Session,
        PermissionScope::Always,
    ];
    capabilities.sandbox = SandboxLevel::Workspace;
    capabilities.secret_isolation = true;
    capabilities.per_session_mcp = true;
    capabilities.hooks = HookSupport::InProtocol;
    capabilities.subagents = SubagentSupport::Nested;
    capabilities.compaction_signal = true;
    capabilities.thinking = true;
    capabilities.images = true;
    capabilities.tools = true;
    capabilities.context_window = Some(ContextWindow {
        value: 200_000,
        source: ContextWindowSource::Configured,
    });
    capabilities.set_model_live = true;
    capabilities.native_question = true;
    capabilities.tool_cancel = true;
    capabilities.background_tasks = true;
    capabilities.resume = true;
    capabilities.cost = CostBasis::Reported;
    capabilities
}
