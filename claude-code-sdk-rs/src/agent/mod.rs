//! Provider-neutral agent contract.
//!
//! The host (the orchestrator's chat manager, its runner) talks to an
//! [`AgentProvider`] and to the [`AgentSession`]s it opens; it never sees the
//! provider's wire protocol. Claude Code, the native harness over an
//! OpenAI-compatible endpoint, Codex and ACP agents are adapters behind these two
//! traits.
//!
//! The frozen specification is `docs/agent-contract.md`; this module is its
//! executable form. When the two disagree it is a bug, fixed in the same change,
//! and [`CONTRACT_VERSION`] goes up whenever a serialised shape or a trait
//! signature changes.
//!
//! One rule runs through the whole contract: **an absent capability answers
//! [`ProviderError::Unsupported`] or applies its written fallback, never a silent
//! success.**

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

pub mod capabilities;
pub mod credentials;
pub mod error;
pub mod event;
pub mod model_provider;
pub mod policy;
pub mod registry;
pub mod resume;
pub mod spec;

pub use capabilities::{
    Capabilities, ContextWindow, ContextWindowSource, CostBasis, HookSupport, PermissionScope,
    SandboxLevel, SubagentSupport,
};
pub use credentials::{
    CredentialRef, CredentialResolver, EnvCredentialResolver, Secret, redact, redact_with,
};
pub use error::{ProtocolMismatch, ProviderError};
pub use event::{
    AgentEvent, BackgroundTask, BackgroundTaskKind, BackgroundTaskStatus, CompactionPhase,
    CompactionTrigger, Cost, DeltaKind, McpServerStatus, ModelUsage, QuestionOption, QuestionReply,
    QuestionSpec, StopReason, TaskPhase, ToolOutput, Usage,
};
pub use model_provider::{ModelBinding, ModelProtocol, ModelProviderConfig, ProtocolSupport};
pub use policy::{PolicyDecision, PolicyMode, ToolCategory, ToolPattern, ToolPolicy};
pub use registry::{
    BUILTIN_ANTHROPIC_PROVIDER_ID, BUILTIN_CLAUDE_CODE_ID, BuiltProvider, CapabilityRefresher,
    KindFactory, PriceBook, ProviderInstanceConfig, ProviderRegistry, SECURITY_GATE_CAPABILITY,
    SecurityGate,
};
pub use resume::ResumeToken;
pub use spec::{
    CancelOutcome, CancelScope, CompactionInfo, EnvSpec, HookVerdict, InputBlock, InterruptOutcome,
    InterruptScope, Lineage, McpServerSpec, PermissionDecision, ProcessDiagnostic, QuestionAnswer,
    QuestionAnswerItem, SessionHooks, SessionLimits, SessionSpec, SystemPromptMode,
    SystemPromptSpec, ToolCallInfo, ToolResultInfo, TurnContext, TurnDirective, TurnInput,
};

/// Version of the contract. Goes up by one whenever a serialised shape
/// ([`AgentEvent`], [`Capabilities`], [`ProviderError`], [`ToolPolicy`],
/// [`ResumeToken`]) or a trait signature changes. The JSON snapshots in
/// `tests/agent_contract_snapshots.rs` carry it: changing a shape without
/// changing the version fails that test.
pub const CONTRACT_VERSION: u32 = 4;

/// Events of a turn, or of the out-of-band channel.
pub type EventStream = Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;

/// Family of adapter behind a provider instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProviderKind {
    /// The Claude Code CLI.
    ClaudeCode,
    /// The native harness over a raw model endpoint.
    Native,
    /// The Codex CLI (`app-server`).
    Codex,
    /// Any agent speaking the Agent Client Protocol on stdio.
    Acp,
    /// The scripted in-memory provider of the test kit.
    Scripted,
}

impl ProviderKind {
    /// The serialised name, also the key of `SessionSpec::extensions`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::Native => "native",
            Self::Codex => "codex",
            Self::Acp => "acp",
            Self::Scripted => "scripted",
        }
    }
}

/// Coarse state of a provider instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HealthStatus {
    /// Ready to open sessions.
    Ok,
    /// Usable, with a caveat described in `detail`.
    Degraded,
    /// Cannot open sessions; see `error`.
    Unavailable,
}

/// State of a provider instance. `health()` never fails: failure is a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderHealth {
    /// Coarse state.
    pub status: HealthStatus,
    /// Version of the provider (CLI version, server build).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Human-readable detail, already redacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Why the instance is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ProviderError>,
    /// Command a human should run to log in. Never run by the orchestrator (A27).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_hint: Option<String>,
    /// When the check ran, milliseconds since the Unix epoch.
    #[serde(default)]
    pub checked_at_ms: u64,
}

impl ProviderHealth {
    /// A healthy instance, checked now.
    pub fn ok(version: Option<String>) -> Self {
        Self {
            status: HealthStatus::Ok,
            version,
            detail: None,
            error: None,
            login_hint: None,
            checked_at_ms: now_ms(),
        }
    }

    /// An unavailable instance, checked now. An `AuthRequired` error's login hint
    /// is copied to `login_hint`.
    pub fn unavailable(error: ProviderError) -> Self {
        let login_hint = match &error {
            ProviderError::AuthRequired { login_hint } => login_hint.clone(),
            _ => None,
        };
        Self {
            status: HealthStatus::Unavailable,
            version: None,
            detail: None,
            error: Some(error),
            login_hint,
            checked_at_ms: now_ms(),
        }
    }
}

/// Milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Price of a model, in USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// Input tokens.
    pub input_per_mtok: f64,
    /// Output tokens.
    pub output_per_mtok: f64,
    /// Input tokens read from the prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_per_mtok: Option<f64>,
    /// Input tokens written to the prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_mtok: Option<f64>,
}

/// A model an instance can serve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Identifier to pass as `SessionSpec::model`.
    pub id: String,
    /// Name to show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Context window, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<ContextWindow>,
    /// Whether the model calls tools, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_tools: Option<bool>,
    /// Whether the model accepts images, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_images: Option<bool>,
    /// Whether the model emits reasoning, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_thinking: Option<bool>,
    /// Whether this is the instance's default model.
    #[serde(default)]
    pub is_default: bool,
    /// Price, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ModelPrice>,
}

impl ModelInfo {
    /// A model known only by its identifier.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            context_window: None,
            supports_tools: None,
            supports_images: None,
            supports_thinking: None,
            is_default: false,
            pricing: None,
        }
    }
}

/// A configured provider instance: opens and resumes sessions.
#[async_trait]
pub trait AgentProvider: Send + Sync {
    /// Identifier of the **instance** (registry key), e.g. `claude-code`, `deepseek-prod`.
    fn id(&self) -> &str;

    /// Family of adapter.
    fn kind(&self) -> ProviderKind;

    /// State of the instance. Never an error: failure is a value.
    async fn health(&self) -> ProviderHealth;

    /// Models the instance knows. May be empty when the catalogue is unknown.
    async fn catalog(&self) -> Result<Vec<ModelInfo>, ProviderError>;

    /// Capabilities **per model** (A4). `None` = the instance's default model.
    fn capabilities(&self, model: Option<&str>) -> Capabilities;

    /// Opens a new session.
    async fn open(&self, spec: SessionSpec) -> Result<Arc<dyn AgentSession>, ProviderError>;

    /// Resumes a session from a token this provider issued.
    async fn resume(
        &self,
        spec: SessionSpec,
        token: ResumeToken,
    ) -> Result<Arc<dyn AgentSession>, ProviderError>;
}

/// A live session with a provider.
///
/// Every method takes `&self`: answering a permission, interrupting and
/// cancelling must be possible while another task consumes the turn's stream.
/// Concurrency rules are in `docs/agent-contract.md` §9: one turn at a time, and
/// every event goes to exactly one of the turn stream or [`AgentSession::out_of_band`].
#[async_trait]
pub trait AgentSession: Send + Sync {
    /// Snapshot frozen when the session opened (A4). Does not change, even after `set_model`.
    fn capabilities(&self) -> &Capabilities;

    /// Current resume token; `None` until the provider has issued one.
    fn resume_token(&self) -> Option<ResumeToken>;

    /// Starts a turn. The stream ends with exactly one terminal event
    /// ([`AgentEvent::Done`] or [`AgentEvent::Error`]). While a turn is running,
    /// answers [`ProviderError::TurnInProgress`].
    async fn send_turn(&self, input: TurnInput) -> Result<EventStream, ProviderError>;

    /// Answers a [`AgentEvent::PermissionAsk`].
    async fn answer_permission(
        &self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), ProviderError>;

    /// Answers a [`AgentEvent::Question`] whose reply mode is `call`.
    async fn answer_question(
        &self,
        question_id: &str,
        answer: QuestionAnswer,
    ) -> Result<(), ProviderError>;

    /// Ends the running turn. Outside a turn: `Ok` with `turn_interrupted: false`.
    async fn interrupt(&self, scope: InterruptScope) -> Result<InterruptOutcome, ProviderError>;

    /// Stops tools **without** ending the turn (A6).
    async fn cancel_tools(&self, scope: CancelScope) -> Result<CancelOutcome, ProviderError>;

    /// Changes the model of the live session.
    async fn set_model(&self, model: &str) -> Result<(), ProviderError>;

    /// Changes the policy mode of the live session. Only the mode can change after
    /// opening; `native` is the provider's own mode name when the caller knows it.
    async fn set_policy_mode(
        &self,
        mode: PolicyMode,
        native: Option<&str>,
    ) -> Result<(), ProviderError>;

    /// Events that arrive outside a turn. Single consumer: the first call returns
    /// the stream, later calls return `None`.
    fn out_of_band(&self) -> Option<EventStream>;

    /// Closes the session. Idempotent; afterwards every method answers [`ProviderError::Closed`].
    async fn close(&self) -> Result<(), ProviderError>;
}

/// Aggregate of one turn, for one-shot callers.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnSummary {
    /// Final text: `done.result_text`, or the concatenated top-level `text` events.
    pub text: String,
    /// Why the turn ended.
    pub stop_reason: StopReason,
    /// Token usage.
    pub usage: Usage,
    /// Cost.
    pub cost: Cost,
    /// Whether the turn ended in error.
    pub is_error: bool,
}

/// Runs one turn to its end and aggregates it. A permission request met on the
/// way is **denied** (a one-shot caller has nobody to ask) rather than left
/// hanging; questions are ignored.
pub async fn run_turn(
    session: &dyn AgentSession,
    input: TurnInput,
) -> Result<TurnSummary, ProviderError> {
    let mut stream = session.send_turn(input).await?;
    let mut texts: Vec<String> = Vec::new();
    while let Some(event) = stream.next().await {
        match event {
            AgentEvent::Text {
                text, parent: None, ..
            } => texts.push(text),
            AgentEvent::PermissionAsk { request_id, .. } => {
                // Best effort: a provider that cannot take the answer ends the
                // turn by itself, and that end is what we report.
                let _ = session
                    .answer_permission(&request_id, PermissionDecision::deny())
                    .await;
            },
            AgentEvent::Done {
                stop_reason,
                is_error,
                result_text,
                usage,
                cost,
                ..
            } => {
                return Ok(TurnSummary {
                    text: result_text.unwrap_or_else(|| texts.join("\n")),
                    stop_reason,
                    usage,
                    cost,
                    is_error,
                });
            },
            AgentEvent::Error { error } => return Err(error),
            _ => {},
        }
    }
    Err(ProviderError::protocol(
        "turn stream ended without a terminal event",
    ))
}
