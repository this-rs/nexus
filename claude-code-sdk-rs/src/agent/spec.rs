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
    /// Token ceiling of the session: the tokens billed at full price, that is input not
    /// served from a prompt cache, cache writes and output. Cache reads (a prefix sent
    /// again and served from the cache, billed at a fraction) do not count: a long
    /// conversation re-reads its history on every request, and counting it at full weight
    /// spends a budget on tokens nobody pays for in full. The USD budget prices them.
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

    /// Called before a compaction, right after [`Self::before_compaction`] (contract v8). The
    /// returned directive may name the model that writes the summary — a model of the
    /// **same** provider, for this summary only: the session's model does not change. A
    /// provider with [`Capabilities::compaction_model`] summarises with it and says so with
    /// `provider_notice { kind: "compaction_model" }`; one without ignores it and says so
    /// with `provider_notice { kind: "compaction_model_ignored" }`, the compaction going on
    /// with the session's model. Never an error. Naming the session's model, or a blank
    /// name, changes nothing.
    ///
    /// [`Capabilities::compaction_model`]: crate::agent::Capabilities::compaction_model
    async fn compaction_directive(&self, _info: &CompactionInfo) -> CompactionDirective {
        CompactionDirective::default()
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

    /// Whether the host runs sub-agents for this session: a harness that has none of its own
    /// (`Capabilities::subagents` is `None`) then offers the model an `Agent` tool whose calls
    /// go to [`Self::run_subagent`] (decision A18: the host's fallback is a child session).
    /// Asked once, when the session opens. `false` by default: no `Agent` tool.
    ///
    /// The harness refuses the tool to a session whose `lineage.depth` is 1 or more; nothing
    /// else stops recursion. A host that opens the sub-agent as a session of the same harness
    /// gives it a `lineage` with `depth >= 1`, or hooks that answer `false` here.
    fn runs_subagents(&self) -> bool {
        false
    }

    /// Runs one sub-agent to its end and returns what it answered (`Ok`), or why it could
    /// not run or failed (`Err`); either becomes the `tool_result` of the `Agent` call.
    ///
    /// Unlike the other hooks it is not bounded by the hook timeout: a sub-agent takes as
    /// long as its work. The harness drops the future when the turn is interrupted or the
    /// call cancelled; the host stops the sub-agent then.
    ///
    /// The host bounds how many sub-agents run at once and what they spend: the harness passes
    /// every call on (concurrently when the model makes several in one step) and does not count
    /// a sub-agent's cost in the parent session's budget.
    async fn run_subagent(&self, _request: &SubagentRequest) -> Result<String, String> {
        Err("this host runs no sub-agent".to_owned())
    }

    /// Called once per session, before its first turn (Claude Code's `SessionStart`, contract
    /// v7). [`HookReply::additional_context`] reaches the model with the first turn;
    /// [`HookReply::system_message`] is shown to the user
    /// (`provider_notice { kind: "hook_system_message" }`). A session start cannot be blocked:
    /// [`HookReply::block`] is ignored.
    async fn session_start(&self, _info: &SessionStartInfo) -> HookReply {
        HookReply::none()
    }

    /// Called when the user's input of a turn arrives, before anything of the turn runs
    /// (Claude Code's `UserPromptSubmit`, contract v7). [`HookReply::block`] stops the turn
    /// before any model request: the input is not kept, `provider_notice { kind:
    /// "prompt_blocked" }` then `done { completed, num_turns: 0 }` whose `result_text` names the
    /// reason. [`HookReply::additional_context`] reaches the model with the input.
    async fn user_prompt_submit(&self, _prompt: &PromptInfo) -> HookReply {
        HookReply::none()
    }

    /// Called right before the turn concludes, when the model answered without calling a tool
    /// (Claude Code's `Stop`, contract v7). [`HookReply::block`] keeps the turn going: the
    /// reason reaches the model as a user message (`Stop hook feedback:\n<reason>`) and the
    /// hook is asked again at the next answer with [`StopInfo::stop_hook_active`]. After
    /// [`STOP_HOOK_BLOCK_CAP`] blocks in a row the turn ends anyway
    /// (`provider_notice { kind: "stop_hook_block_cap" }`), as on the CLI.
    async fn before_stop(&self, _info: &StopInfo) -> HookReply {
        HookReply::none()
    }

    /// The same, for a session that is a sub-agent (Claude Code's `SubagentStop`, contract v7):
    /// a sub-agent the harness runs itself, or a session opened with a `lineage` of depth 1 or
    /// more. A block keeps the sub-agent working, with the reason as its next message.
    async fn before_subagent_stop(&self, _info: &SubagentStopInfo) -> HookReply {
        HookReply::none()
    }

    /// Called when the harness would notify the user (Claude Code's `Notification`, contract
    /// v7), e.g. `permission_prompt` when a tool call waits for a human. Observation only, as on
    /// the CLI: nothing comes back, and the turn does not wait for it.
    async fn notification(&self, _info: &NotificationInfo) {}
}

/// How many `Stop` / `SubagentStop` blocks in a row a turn takes before it ends anyway: the
/// Claude Code CLI's default (2.1.296, measured 18/10/2026: "A hook blocked the turn from ending
/// 9 consecutive times — overriding and ending turn").
pub const STOP_HOOK_BLOCK_CAP: u32 = 9;

/// What a `session_start`, `user_prompt_submit`, `before_stop` or `before_subagent_stop` hook
/// answers: the fields the Claude Code CLI reads from a hook's output (`decision: "block"` +
/// `reason`, `hookSpecificOutput.additionalContext`, `systemMessage`). Each event reads the
/// fields that mean something to it (see the hook); the others are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct HookReply {
    /// `Some(reason)`: block (`decision: "block"`), with the reason the model reads (a stop) or
    /// the user reads (a prompt).
    pub block: Option<String>,
    /// Text added to the model's context.
    pub additional_context: Option<String>,
    /// Text shown to the user, never to the model.
    pub system_message: Option<String>,
}

impl HookReply {
    /// A reply that changes nothing.
    pub fn none() -> Self {
        Self::default()
    }

    /// A reply that blocks, with `reason`.
    pub fn block(reason: impl Into<String>) -> Self {
        Self {
            block: Some(reason.into()),
            ..Self::default()
        }
    }

    /// A reply that adds `text` to the model's context.
    pub fn context(text: impl Into<String>) -> Self {
        Self {
            additional_context: Some(text.into()),
            ..Self::default()
        }
    }

    /// The same reply, with `message` shown to the user.
    pub fn with_system_message(mut self, message: impl Into<String>) -> Self {
        self.system_message = Some(message.into());
        self
    }

    /// The block reason, when there is one that says something.
    pub fn block_reason(&self) -> Option<&str> {
        non_blank(self.block.as_deref())
    }

    /// The added context, when it says something.
    pub fn context_text(&self) -> Option<&str> {
        non_blank(self.additional_context.as_deref())
    }

    /// The user's message, when it says something.
    pub fn system_text(&self) -> Option<&str> {
        non_blank(self.system_message.as_deref())
    }
}

fn non_blank(text: Option<&str>) -> Option<&str> {
    text.filter(|text| !text.trim().is_empty())
}

/// A session about to run its first turn, as a `session_start` hook sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionStartInfo {
    /// `startup` (a new session) or `resume` (a resumed one), as Claude Code names them.
    pub source: String,
}

impl SessionStartInfo {
    /// A start from `source`.
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
        }
    }
}

/// The user's input of a turn, as a `user_prompt_submit` hook sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PromptInfo {
    /// The text of the input (its text blocks joined).
    pub prompt: String,
}

impl PromptInfo {
    /// The input `prompt`.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
        }
    }
}

/// A turn about to conclude, as a `before_stop` hook sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StopInfo {
    /// Whether a stop hook already blocked this turn (Claude Code's `stop_hook_active`): a hook
    /// that blocks should let the turn end when this is true.
    pub stop_hook_active: bool,
    /// The model's last answer, when it wrote one.
    pub last_assistant_message: Option<String>,
}

impl StopInfo {
    /// A stop after `last_assistant_message`.
    pub fn new(stop_hook_active: bool, last_assistant_message: Option<String>) -> Self {
        Self {
            stop_hook_active,
            last_assistant_message,
        }
    }
}

/// A sub-agent about to conclude, as a `before_subagent_stop` hook sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SubagentStopInfo {
    /// Whether a stop hook already blocked this sub-agent's turn.
    pub stop_hook_active: bool,
    /// The sub-agent's last answer, when it wrote one.
    pub last_assistant_message: Option<String>,
    /// The sub-agent's identifier, when the provider gives one.
    pub agent_id: Option<String>,
    /// The kind of sub-agent (`general-purpose`…), when known.
    pub agent_type: Option<String>,
}

impl SubagentStopInfo {
    /// A sub-agent stop after `last_assistant_message`.
    pub fn new(stop_hook_active: bool, last_assistant_message: Option<String>) -> Self {
        Self {
            stop_hook_active,
            last_assistant_message,
            agent_id: None,
            agent_type: None,
        }
    }
}

/// A notification, as a `notification` hook sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NotificationInfo {
    /// Claude Code's `notification_type`: `permission_prompt`, `idle_prompt`…
    pub notification_type: String,
    /// What the notification says.
    pub message: String,
    /// Its title, when it has one.
    pub title: Option<String>,
}

impl NotificationInfo {
    /// A notification of `notification_type` saying `message`.
    pub fn new(notification_type: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            notification_type: notification_type.into(),
            message: message.into(),
            title: None,
        }
    }

    /// The notification Claude Code sends when a tool call waits for the user's permission.
    pub fn permission_prompt(tool: &str) -> Self {
        Self::new(
            "permission_prompt",
            format!("Claude needs your permission to use {tool}"),
        )
    }
}

/// The user message a stop hook's block reason becomes (what the Claude Code CLI writes,
/// measured on 2.1.296).
pub fn stop_hook_feedback(reason: &str) -> String {
    format!("Stop hook feedback:\n{reason}")
}

/// The `result_text` of a turn a `user_prompt_submit` hook blocked (what the Claude Code CLI
/// writes, measured on 2.1.296).
pub fn prompt_blocked_text(reason: &str, prompt: &str) -> String {
    format!("UserPromptSubmit operation blocked by hook:\n{reason}\n\nOriginal prompt: {prompt}")
}

/// What the model asked of a sub-agent through the `Agent` tool (see
/// [`SessionHooks::run_subagent`]).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SubagentRequest {
    /// The `tool_call` that asked for it.
    pub tool_call_id: String,
    /// A short description of the task (3–5 words), when the model gave one.
    pub description: Option<String>,
    /// The task, in full: the sub-agent's first message.
    pub prompt: String,
    /// The kind of agent asked for, when the model named one.
    pub subagent_type: Option<String>,
    /// The call's input as the model wrote it.
    pub input: Value,
}

impl SubagentRequest {
    /// A request for `prompt`, asked by the call `tool_call_id`.
    pub fn new(tool_call_id: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            description: None,
            prompt: prompt.into(),
            subagent_type: None,
            input: Value::Null,
        }
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
    /// Tokens spent by the session so far, as `SessionLimits::max_tokens` counts them
    /// (cache reads excluded).
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

/// What a `compaction_directive` hook asks of the compaction about to start (contract v8).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CompactionDirective {
    /// Model that writes the summary (same provider), when the hook wants another one than
    /// the session's.
    pub model: Option<String>,
}

impl CompactionDirective {
    /// A directive that changes nothing.
    pub fn none() -> Self {
        Self::default()
    }

    /// A directive that writes the summary with `model`.
    pub fn model(model: impl Into<String>) -> Self {
        Self {
            model: Some(model.into()),
        }
    }

    /// The model named, when it says something and differs from `current`.
    pub fn model_other_than(&self, current: &str) -> Option<&str> {
        non_blank(self.model.as_deref()).filter(|model| *model != current)
    }
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
    /// The hook does not decide: a human must consent to `subject` before the call runs (N23b).
    ///
    /// A hook that judges *what* a call is about — an origin a browser would reach, a host a
    /// command would contact — cannot decide in the model's place, and `Deny` would be a final
    /// refusal the human never sees. This verdict names the **subject** and the harness asks the
    /// user in band, through the permission channel it already has (`permission_ask` /
    /// `answer_permission`), so the answer is remembered against that subject and never against
    /// the tool as a whole.
    ///
    /// `subject` is asserted by the **harness** (the origin a tool call really targets), never
    /// read from the model's text: it is what the user's answer is scoped to.
    ///
    /// A provider that cannot ask (no `interactive_permissions`, nobody to answer) fails closed:
    /// the call does not run and the model reads `reason`.
    RequireConsent {
        /// What the consent is about, e.g. `https://example.com` — asserted by the harness.
        subject: String,
        /// Why the subject is not allowed yet: shown to the human, and to the model when
        /// nobody can be asked.
        reason: String,
    },
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

/// Result of `AgentSession::compact`.
///
/// `compacted: false` is a successful no-op (the history is too short to be worth a
/// summary): nothing was sent to the model and the history is as it was.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CompactOutcome {
    /// A summary replaced the older part of the history.
    pub compacted: bool,
    /// Estimated size of the history before, in tokens (system prompt included).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_before: Option<u64>,
    /// Estimated size of the history after, in tokens. `None` when nothing was done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_after: Option<u64>,
    /// Messages the summary stands for (`0` when nothing was done).
    pub messages_summarised: u32,
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
