//! What a host asks for when it opens a session, and what it sends during one
//! (contract §2, §3, §10 ; decisions A6, A7, A9).

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::capabilities::PermissionScope;
use super::error::ProviderError;
use super::model_provider::ModelBinding;
use super::policy::{ToolCategory, ToolPolicy};

/// Everything needed to open (or resume) a session.
///
/// Not serialisable on purpose: it carries callbacks and MCP credentials.
#[derive(Clone)]
#[non_exhaustive]
pub struct SessionSpec {
    /// Working directory of the session.
    pub cwd: PathBuf,
    /// Model to use; `None` = the instance's default.
    pub model: Option<String>,
    /// System prompt.
    pub system_prompt: Option<SystemPromptSpec>,
    /// Tool policy asked for.
    pub policy: ToolPolicy,
    /// Ceiling the policy may not exceed (child sessions, signed tokens).
    pub policy_ceiling: Option<ToolPolicy>,
    /// MCP servers attached to this session, by name.
    pub mcp_servers: BTreeMap<String, McpServerSpec>,
    /// Extra directories the agent may work in.
    pub extra_dirs: Vec<PathBuf>,
    /// Limit of model round-trips per turn.
    pub max_turns: Option<u32>,
    /// Environment of the provider process.
    pub env: EnvSpec,
    /// Host callbacks around tools and compaction. Ignored (with a notice) when
    /// `Capabilities::hooks` is not `InProtocol`.
    pub hooks: Option<Arc<dyn SessionHooks>>,
    /// Budgets and timeouts.
    pub limits: SessionLimits,
    /// Where this session comes from, for a child session.
    pub lineage: Option<Lineage>,
    /// Ask for streaming deltas.
    pub deltas: bool,
    /// Provider-specific settings, keyed by provider kind (`claude_code`, `codex`, `acp`, `native`).
    pub extensions: BTreeMap<String, Value>,
    /// The model provider this session runs over, apart from the harness (N16). Applied
    /// by [`ProviderRegistry::open_session`](super::ProviderRegistry::open_session), which
    /// resolves it and clears it; a provider that receives it set refuses the session
    /// (`Unsupported { model_binding }`) rather than ignore the request.
    pub model_binding: Option<ModelBinding>,
}

impl SessionSpec {
    /// A spec for `cwd` with the default policy (`ask`) and deltas on.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            model: None,
            system_prompt: None,
            policy: ToolPolicy::default(),
            policy_ceiling: None,
            mcp_servers: BTreeMap::new(),
            extra_dirs: Vec::new(),
            max_turns: None,
            env: EnvSpec::default(),
            hooks: None,
            limits: SessionLimits::default(),
            lineage: None,
            deltas: true,
            extensions: BTreeMap::new(),
            model_binding: None,
        }
    }

    /// Checks the rules every provider applies before opening (contract §3):
    /// the policy must stay within its ceiling.
    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.model_binding.is_some() {
            // Opened through a provider directly, a binding would be silently ignored.
            return Err(ProviderError::unsupported("model_binding"));
        }
        if let Some(ceiling) = &self.policy_ceiling
            && !self.policy.is_within(ceiling)
        {
            return Err(ProviderError::unsupported("policy_ceiling"));
        }
        Ok(())
    }

    /// The extension object for a provider kind, when present.
    pub fn extension(&self, kind: &str) -> Option<&Value> {
        self.extensions.get(kind)
    }
}

impl fmt::Debug for SessionSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionSpec")
            .field("cwd", &self.cwd)
            .field("model", &self.model)
            .field(
                "system_prompt",
                &self.system_prompt.as_ref().map(|p| p.mode),
            )
            .field("policy", &self.policy)
            .field("policy_ceiling", &self.policy_ceiling)
            .field("mcp_servers", &self.mcp_servers)
            .field("extra_dirs", &self.extra_dirs)
            .field("max_turns", &self.max_turns)
            .field("env", &self.env)
            .field("hooks", &self.hooks.is_some())
            .field("limits", &self.limits)
            .field("lineage", &self.lineage)
            .field("deltas", &self.deltas)
            .field("extensions", &self.extensions.keys().collect::<Vec<_>>())
            .field("model_binding", &self.model_binding)
            .finish()
    }
}

/// System prompt and how it combines with the provider's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPromptSpec {
    /// Prompt text.
    pub text: String,
    /// Replace the provider's prompt, or append to it.
    pub mode: SystemPromptMode,
}

/// How a system prompt combines with the provider's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SystemPromptMode {
    /// Use this prompt instead of the provider's.
    #[default]
    Replace,
    /// Add this prompt after the provider's.
    Append,
}

/// An MCP server attached to a session.
///
/// `Debug` never prints the values of `env` or `headers`: they routinely carry a
/// session token or a database password.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpServerSpec {
    /// A server launched as a child process speaking MCP on stdio.
    Stdio {
        /// Executable.
        command: String,
        /// Arguments.
        args: Vec<String>,
        /// Environment of the server process.
        env: BTreeMap<String, String>,
    },
    /// A server reached over streamable HTTP.
    Http {
        /// Endpoint URL.
        url: String,
        /// Request headers.
        headers: BTreeMap<String, String>,
    },
    /// A server reached over SSE.
    Sse {
        /// Endpoint URL.
        url: String,
        /// Request headers.
        headers: BTreeMap<String, String>,
    },
}

impl McpServerSpec {
    /// A stdio server with no argument and no environment.
    pub fn stdio(command: impl Into<String>) -> Self {
        Self::Stdio {
            command: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }
}

pub(crate) fn redacted_map(map: &BTreeMap<String, String>) -> BTreeMap<&str, String> {
    map.iter()
        .map(|(name, value)| (name.as_str(), format!("<redacted:{}>", value.len())))
        .collect()
}

impl fmt::Debug for McpServerSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio { command, args, env } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", &format_args!("<{} args>", args.len()))
                .field("env", &redacted_map(env))
                .finish(),
            Self::Http { url, headers } => f
                .debug_struct("Http")
                .field("url", &super::credentials::redact(url))
                .field("headers", &redacted_map(headers))
                .finish(),
            Self::Sse { url, headers } => f
                .debug_struct("Sse")
                .field("url", &super::credentials::redact(url))
                .field("headers", &redacted_map(headers))
                .finish(),
        }
    }
}

/// Environment of the provider process: a clean environment, a base allowlist
/// (see `transport::spawn`), plus what is listed here.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct EnvSpec {
    /// Names inherited from the host process in addition to the base allowlist.
    pub inherit: Vec<String>,
    /// Variables set explicitly.
    pub set: BTreeMap<String, String>,
}

impl fmt::Debug for EnvSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnvSpec")
            .field("inherit", &self.inherit)
            .field("set", &redacted_map(&self.set))
            .finish()
    }
}

/// Budgets and timeouts of a session. `None` = no limit of that kind.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SessionLimits {
    /// Spending ceiling in USD.
    pub max_cost_usd: Option<f64>,
    /// Token ceiling (input + output).
    pub max_tokens: Option<u64>,
    /// Longest a turn may run, in milliseconds.
    pub turn_timeout_ms: Option<u64>,
    /// Longest chain of tool round-trips inside one turn (native harness).
    pub max_tool_iterations: Option<u32>,
}

/// Where a child session comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lineage {
    /// Session that spawned this one.
    pub parent_session_id: String,
    /// First ancestor.
    pub root_session_id: String,
    /// Distance to the root (a direct child is 1).
    pub depth: u32,
    /// Why it was spawned (`delegation`, `runner`…).
    pub spawned_by: String,
}

/// Host callbacks around tools and compaction (A7).
///
/// Every method has a do-nothing default, so a host implements only what it needs.
#[async_trait]
pub trait SessionHooks: Send + Sync {
    /// Called before a tool runs.
    async fn before_tool(&self, _call: &ToolCallInfo) -> HookVerdict {
        HookVerdict::Continue
    }

    /// Called after a tool ran. The returned text is added to the model's context.
    async fn after_tool(&self, _result: &ToolResultInfo) -> Option<String> {
        None
    }

    /// Called before a compaction. The returned text is added to the compaction instructions.
    async fn before_compaction(&self, _info: &CompactionInfo) -> Option<String> {
        None
    }

    /// Called before a turn starts, before the harness reads which model to send
    /// it to. The returned directive may name another model of the **same**
    /// provider: the turn then runs on that model and a [`AgentEvent::ModelChanged`]
    /// is emitted first. A provider without `set_model_live` ignores the model and
    /// says so with `provider_notice { kind: "model_directive_ignored" }`.
    ///
    /// [`AgentEvent::ModelChanged`]: crate::agent::AgentEvent::ModelChanged
    async fn before_turn(&self, _ctx: &TurnContext) -> TurnDirective {
        TurnDirective::default()
    }
}

/// The turn about to start, as a `before_turn` hook sees it.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct TurnContext {
    /// Index of the turn in the session, starting at 0.
    pub turn_index: u32,
    /// Model the turn would run on if the hook says nothing.
    pub current_model: String,
    /// Length of the user input, in characters.
    pub input_chars: usize,
    /// Tokens in context after the previous turn, when the provider reported them.
    pub context_tokens: Option<u64>,
    /// Tokens spent by the session so far.
    pub tokens_spent: u64,
    /// USD spent by the session so far, when a price is known.
    pub usd_spent: Option<f64>,
}

impl TurnContext {
    /// A context for `turn_index` on `current_model`.
    pub fn new(turn_index: u32, current_model: impl Into<String>) -> Self {
        Self {
            turn_index,
            current_model: current_model.into(),
            input_chars: 0,
            context_tokens: None,
            tokens_spent: 0,
            usd_spent: None,
        }
    }
}

/// What a `before_turn` hook asks of the turn about to start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TurnDirective {
    /// Model to run the turn on (same provider), when the hook wants another one.
    pub model: Option<String>,
}

impl TurnDirective {
    /// A directive that changes nothing.
    pub fn none() -> Self {
        Self::default()
    }

    /// A directive that runs the turn on `model`.
    pub fn model(model: impl Into<String>) -> Self {
        Self {
            model: Some(model.into()),
        }
    }
}

/// A tool call, as seen by a hook or by the policy.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallInfo {
    /// Identifier of the call, when known at that point.
    pub id: Option<String>,
    /// Tool name as the provider spells it.
    pub name: String,
    /// Stable alias of the tool.
    pub canonical: Option<String>,
    /// Category of the tool.
    pub category: ToolCategory,
    /// Tool input.
    pub input: Value,
}

/// A tool result, as seen by a hook.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResultInfo {
    /// The call that produced it.
    pub call: ToolCallInfo,
    /// Output of the tool.
    pub output: Value,
    /// Whether the tool failed.
    pub is_error: bool,
}

/// A compaction about to start, as seen by a hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionInfo {
    /// `manual` or `auto`.
    pub trigger: String,
    /// Instructions the user gave for the compaction, if any.
    pub custom_instructions: Option<String>,
}

/// What a `before_tool` hook decides.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum HookVerdict {
    /// Let the call go through the normal policy.
    Continue,
    /// Refuse the call.
    Deny {
        /// Reason shown to the model.
        reason: String,
    },
    /// Run the call with another input.
    ReplaceInput(Value),
    /// Let the call go and add text to the model's context.
    AddContext(String),
}

/// What the user sends to start a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnInput {
    /// Content blocks, in order.
    pub blocks: Vec<InputBlock>,
}

impl TurnInput {
    /// A turn made of one text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            blocks: vec![InputBlock::Text { text: text.into() }],
        }
    }

    /// Whether any block is an image.
    pub fn has_images(&self) -> bool {
        self.blocks
            .iter()
            .any(|block| matches!(block, InputBlock::Image { .. }))
    }

    /// The text blocks joined by a blank line.
    pub fn joined_text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                InputBlock::Text { text } => Some(text.as_str()),
                InputBlock::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// One block of user input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum InputBlock {
    /// Text.
    Text {
        /// The text.
        text: String,
    },
    /// An image. Refused with `Unsupported { capability: "images" }` when the
    /// session's capabilities say `images: false`.
    Image {
        /// MIME type (`image/png`…).
        media_type: String,
        /// Base64 payload, without a `data:` prefix.
        data_base64: String,
    },
}

/// Answer to a [`AgentEvent::PermissionAsk`](super::AgentEvent::PermissionAsk).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PermissionDecision {
    /// Let the tool run.
    Allow {
        /// How long the approval lasts.
        scope: PermissionScope,
        /// Input to run instead of the original one. `None` = the adapter replays
        /// the original input.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_input: Option<Value>,
    },
    /// Refuse the tool.
    Deny {
        /// Message given to the model.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        /// Also end the turn.
        #[serde(default)]
        interrupt: bool,
    },
}

impl PermissionDecision {
    /// Allow this call only, with its original input.
    pub fn allow_once() -> Self {
        Self::Allow {
            scope: PermissionScope::Once,
            updated_input: None,
        }
    }

    /// Refuse, with the default message.
    pub fn deny() -> Self {
        Self::Deny {
            message: None,
            interrupt: false,
        }
    }
}

/// Answer to a [`AgentEvent::Question`](super::AgentEvent::Question) whose reply mode is `call`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[non_exhaustive]
pub enum QuestionAnswer {
    /// The user answered.
    Answered {
        /// One entry per question.
        answers: Vec<QuestionAnswerItem>,
    },
    /// The user dismissed the question.
    Cancelled,
}

/// Answer to one question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionAnswerItem {
    /// The question answered.
    pub question: String,
    /// Labels of the options chosen.
    #[serde(default)]
    pub selected: Vec<String>,
    /// Free text typed by the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_text: Option<String>,
}

/// What an interruption stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum InterruptScope {
    /// End the turn and stop the tools it is running.
    TurnAndTools,
    /// End the turn; leave background work alone.
    TurnOnly,
}

/// Which tools a cancellation stops. The turn is preserved.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CancelScope {
    /// Every tool currently running.
    All,
    /// One background task.
    Task {
        /// Identifier of the task (`BackgroundTask::id`).
        id: String,
    },
}

/// Process-level detail of an interruption or a cancellation. Diagnostic only.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProcessDiagnostic {
    /// Process identifier of the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Processes that were signalled.
    #[serde(default)]
    pub killed_pids: Vec<u32>,
}

/// Result of `AgentSession::interrupt`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InterruptOutcome {
    /// Whether a running turn was asked to stop (`false` when no turn was running).
    pub turn_interrupted: bool,
    /// Number of tools stopped.
    pub tools_cancelled: u32,
    /// Process-level detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<ProcessDiagnostic>,
}

/// Result of `AgentSession::cancel_tools`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CancelOutcome {
    /// Number of tools stopped.
    pub tools_cancelled: u32,
    /// Process-level detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<ProcessDiagnostic>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::policy::{PolicyMode, ToolPattern};

    #[test]
    fn debug_of_a_spec_never_prints_mcp_or_env_values() {
        let mut spec = SessionSpec::new("/work");
        spec.system_prompt = Some(SystemPromptSpec {
            text: "PROMPT-BODY-SHOULD-NOT-APPEAR".into(),
            mode: SystemPromptMode::Replace,
        });
        spec.env
            .set
            .insert("PO_VAULT_TOKEN".into(), "vault-token-value".into());
        spec.mcp_servers.insert(
            "po".into(),
            McpServerSpec::Stdio {
                command: "/bin/mcp".into(),
                args: vec!["--token=argv-token-value".into()],
                env: BTreeMap::from([(
                    "NEO4J_PASSWORD".to_string(),
                    "db-password-value".to_string(),
                )]),
            },
        );
        spec.mcp_servers.insert(
            "remote".into(),
            McpServerSpec::Http {
                url: "https://user:url-password-value@host/mcp".into(),
                headers: BTreeMap::from([(
                    "Authorization".to_string(),
                    "Bearer header-token-value".to_string(),
                )]),
            },
        );
        spec.extensions
            .insert("claude_code".into(), serde_json::json!({"x": "ext-value"}));
        let debug = format!("{spec:?}");
        for leak in [
            "vault-token-value",
            "argv-token-value",
            "db-password-value",
            "url-password-value",
            "header-token-value",
            "PROMPT-BODY-SHOULD-NOT-APPEAR",
            "ext-value",
        ] {
            assert!(!debug.contains(leak), "{leak} leaked into {debug}");
        }
        // What debugging needs is still there.
        for kept in [
            "NEO4J_PASSWORD",
            "PO_VAULT_TOKEN",
            "/bin/mcp",
            "Authorization",
            "claude_code",
        ] {
            assert!(debug.contains(kept), "{kept} missing from {debug}");
        }
    }

    #[test]
    fn validate_refuses_a_policy_above_its_ceiling() {
        let mut spec = SessionSpec::new("/work");
        spec.policy = ToolPolicy::new(PolicyMode::Trust);
        assert_eq!(spec.validate(), Ok(()));
        spec.policy_ceiling = Some(ToolPolicy::new(PolicyMode::Ask));
        assert_eq!(
            spec.validate(),
            Err(ProviderError::unsupported("policy_ceiling"))
        );
        spec.policy = ToolPolicy::new(PolicyMode::Ask);
        assert_eq!(spec.validate(), Ok(()));
        spec.policy.allow.push(ToolPattern::tool("Bash"));
        assert_eq!(
            spec.validate(),
            Err(ProviderError::unsupported("policy_ceiling")),
            "an allow entry the ceiling does not cover is above the ceiling"
        );
    }

    #[test]
    fn turn_input_helpers() {
        let mut input = TurnInput::text("hello");
        assert!(!input.has_images());
        input.blocks.push(InputBlock::Image {
            media_type: "image/png".into(),
            data_base64: "AAAA".into(),
        });
        input.blocks.push(InputBlock::Text {
            text: "world".into(),
        });
        assert!(input.has_images());
        assert_eq!(input.joined_text(), "hello\n\nworld");
    }
}
