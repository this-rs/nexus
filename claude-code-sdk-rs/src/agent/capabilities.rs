//! Capabilities of a provider for a model (contract §5, decision A4).
//!
//! Capabilities are **per model** (`AgentProvider::capabilities(model)`) and frozen
//! on the session when it opens. Every absent capability has a written fallback in
//! `docs/agent-contract.md` §5 and a conformance scenario: an absent capability
//! answers `ProviderError::Unsupported` or applies that fallback, never a silent
//! success.

use serde::{Deserialize, Serialize};

/// How long an approval given by the user lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PermissionScope {
    /// This call only.
    Once,
    /// Until the session ends.
    Session,
    /// Persisted by the provider beyond the session.
    Always,
}

/// Isolation the provider gives to the tools it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SandboxLevel {
    /// No operating-system sandbox. Information for the user (what isolates the tools), not a gate: `trust` opens on every provider.
    #[default]
    None,
    /// Writes confined to the workspace.
    Workspace,
    /// Full isolation (filesystem and network).
    Full,
}

/// How the provider runs the host's hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HookSupport {
    /// Hooks are callbacks inside the protocol: `SessionSpec::hooks` is honoured.
    InProtocol,
    /// The provider can only launch a command; not used in v1 (no relay executable).
    Command,
    /// No hook: `SessionSpec::hooks` is ignored and the host applies its fallback.
    #[default]
    None,
}

/// How the provider reports sub-agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SubagentSupport {
    /// Events of a sub-agent carry `parent` = the spawning tool call.
    Nested,
    /// Events of a sub-agent arrive on another thread; `parent` = that thread's id.
    SeparateThread,
    /// No sub-agent: the host falls back to a child session.
    #[default]
    None,
}

/// Where a cost figure comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CostBasis {
    /// Reported by the provider for a model it knows.
    Reported,
    /// Computed from token usage and the price table.
    Priced,
    /// The endpoint is free (local model).
    Free,
    /// Covered by a subscription: no marginal cost.
    Subscription,
    /// No price known: the amount is `None`, never zero.
    #[default]
    Unknown,
}

/// Where a context-window figure comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContextWindowSource {
    /// Reported by the provider at run time.
    Reported,
    /// Read from the provider's model catalogue.
    Catalog,
    /// Set in the instance configuration.
    Configured,
    /// Measured by the endpoint probe.
    Probed,
    /// A default nobody verified; treat with suspicion.
    Assumed,
}

/// Size of the context window and how it is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContextWindow {
    /// Size in tokens.
    pub value: u64,
    /// Origin of the figure.
    pub source: ContextWindowSource,
}

/// What a provider can do for a given model.
///
/// Build with [`Capabilities::none`] and set the fields; the struct is
/// `#[non_exhaustive]` so a new capability is not a breaking change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Capabilities {
    /// The provider asks before running a tool and waits for the answer.
    pub interactive_permissions: bool,
    /// Scopes an approval may carry.
    pub permission_scopes: Vec<PermissionScope>,
    /// Isolation of the tools the provider runs.
    pub sandbox: SandboxLevel,
    /// The provider process cannot read the host's secrets (clean environment,
    /// MCP credentials off the command line).
    pub secret_isolation: bool,
    /// MCP servers can be attached to one session.
    pub per_session_mcp: bool,
    /// How hooks run.
    pub hooks: HookSupport,
    /// How sub-agents are reported.
    pub subagents: SubagentSupport,
    /// The provider signals context compaction.
    pub compaction_signal: bool,
    /// `AgentSession::compact` works: the history can be compacted on request.
    #[serde(default)]
    pub compaction_manual: bool,
    /// The summary of a compaction can be written by another model of the provider than the
    /// session's, named by `SessionHooks::compaction_directive` (contract v8).
    #[serde(default)]
    pub compaction_model: bool,
    /// The provider emits reasoning.
    pub thinking: bool,
    /// The provider accepts image input.
    pub images: bool,
    /// The model can call tools.
    pub tools: bool,
    /// Context window, when known. `None` is never read as a default size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<ContextWindow>,
    /// The model can be changed on a live session.
    pub set_model_live: bool,
    /// A `before_turn` directive may ask how hard the model thinks on that turn
    /// (`TurnDirective::reasoning_effort`, contract v10) and the provider applies it to
    /// the requests of that turn. Absent: the directive is said with
    /// `provider_notice { reasoning_effort_ignored }` and the turn runs as it would have.
    #[serde(default)]
    pub reasoning_effort: bool,
    /// The provider has its own "ask the user a question" tool.
    pub native_question: bool,
    /// Running tools can be cancelled without ending the turn.
    pub tool_cancel: bool,
    /// The provider reports background tasks.
    pub background_tasks: bool,
    /// A session can be resumed from a `ResumeToken`.
    pub resume: bool,
    /// Where cost figures come from.
    pub cost: CostBasis,
}

impl Capabilities {
    /// Nothing supported. The starting point of every adapter.
    pub fn none() -> Self {
        Self {
            interactive_permissions: false,
            permission_scopes: Vec::new(),
            sandbox: SandboxLevel::None,
            secret_isolation: false,
            per_session_mcp: false,
            hooks: HookSupport::None,
            subagents: SubagentSupport::None,
            compaction_signal: false,
            compaction_manual: false,
            compaction_model: false,
            thinking: false,
            images: false,
            tools: false,
            context_window: None,
            set_model_live: false,
            reasoning_effort: false,
            native_question: false,
            tool_cancel: false,
            background_tasks: false,
            resume: false,
            cost: CostBasis::Unknown,
        }
    }

    /// Names of the 21 fields, in declaration order. The conformance suite walks
    /// this list so that a capability added without a scenario fails a test.
    pub const FIELDS: [&'static str; 21] = [
        "interactive_permissions",
        "permission_scopes",
        "sandbox",
        "secret_isolation",
        "per_session_mcp",
        "hooks",
        "subagents",
        "compaction_signal",
        "compaction_manual",
        "compaction_model",
        "thinking",
        "images",
        "tools",
        "context_window",
        "set_model_live",
        "reasoning_effort",
        "native_question",
        "tool_cancel",
        "background_tasks",
        "resume",
        "cost",
    ];
}

impl Default for Capabilities {
    fn default() -> Self {
        Self::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_constant_matches_the_serialised_shape() {
        let mut caps = Capabilities::none();
        caps.context_window = Some(ContextWindow {
            value: 1,
            source: ContextWindowSource::Assumed,
        });
        let value = serde_json::to_value(&caps).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut expected = Capabilities::FIELDS.to_vec();
        expected.sort_unstable();
        let mut actual = keys.clone();
        actual.sort_unstable();
        assert_eq!(actual, expected);
    }

    #[test]
    fn an_unknown_context_window_is_absent_not_a_default() {
        let json = serde_json::to_string(&Capabilities::none()).unwrap();
        assert!(!json.contains("context_window"));
        assert!(!json.contains("200000"));
        let back: Capabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(back.context_window, None);
        assert_eq!(back.cost, CostBasis::Unknown);
    }

    #[test]
    fn reasoning_effort_is_absent_by_default_and_read_from_a_v9_peer() {
        assert!(!Capabilities::none().reasoning_effort);
        assert!(Capabilities::FIELDS.contains(&"reasoning_effort"));
        // A v9 peer never writes the field: it parses as absent, never as an error.
        let mut json = serde_json::to_value(Capabilities::none()).unwrap();
        json.as_object_mut().unwrap().remove("reasoning_effort");
        let back: Capabilities = serde_json::from_value(json).unwrap();
        assert!(!back.reasoning_effort);
        let mut declared = Capabilities::none();
        declared.reasoning_effort = true;
        let json = serde_json::to_value(&declared).unwrap();
        assert_eq!(json["reasoning_effort"], true);
        assert_eq!(
            serde_json::from_value::<Capabilities>(json).unwrap(),
            declared
        );
    }
}
