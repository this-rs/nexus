//! Typed provider errors (contract §7, decision A10).
//!
//! A [`ProviderError`] never carries a credential: every free-text `detail` goes
//! through [`redact`] at construction, by way of
//! the constructors below.

use serde::{Deserialize, Serialize};

use super::credentials::redact;

/// Error returned by a provider, a session or a model endpoint.
///
/// Serialised with a `kind` tag in `snake_case`; the backend maps `kind` to an
/// HTTP status and an error `code` (see [`ProviderError::http_status_hint`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProviderError {
    /// The provider executable is not installed or not on the search path.
    #[error("provider executable not found: {program}")]
    CliNotFound {
        /// Name or path of the executable that was looked up.
        program: String,
    },
    /// The provider is installed but nobody is logged in.
    #[error("authentication required")]
    AuthRequired {
        /// Command a human should run to log in; never run by the orchestrator.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        login_hint: Option<String>,
    },
    /// The credential store is locked: no fallback to another provider.
    #[error("credential store is locked")]
    CredentialsLocked,
    /// The endpoint rejected the credential.
    #[error("unauthorized")]
    Unauthorized,
    /// The endpoint could not be reached (DNS, connection, TLS).
    #[error("endpoint unreachable: {detail}")]
    EndpointUnreachable {
        /// Redacted description of the failure.
        detail: String,
    },
    /// The selected model cannot call tools.
    #[error("model {model} does not support tool calls")]
    ModelNoTools {
        /// Model identifier.
        model: String,
    },
    /// The context window cannot hold the request.
    #[error("context window too small")]
    ContextTooSmall {
        /// Tokens the request needs, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        needed: Option<u64>,
        /// Tokens the model offers, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        available: Option<u64>,
    },
    /// The provider asked to slow down.
    #[error("rate limited")]
    RateLimited {
        /// Delay requested by the provider, in milliseconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<u64>,
    },
    /// The provider is temporarily overloaded.
    #[error("provider overloaded")]
    Overloaded,
    /// No answer within the allotted time.
    #[error("timed out after {after_ms} ms")]
    Timeout {
        /// Time waited, in milliseconds.
        after_ms: u64,
    },
    /// The provider process ended.
    #[error("provider process exited")]
    ProcessExited {
        /// Exit code, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
    },
    /// The provider spoke something the adapter does not understand.
    #[error("protocol error: {detail}")]
    Protocol {
        /// Redacted description of the failure.
        detail: String,
    },
    /// The capability is absent for this provider, model or session.
    #[error("unsupported capability: {capability}")]
    Unsupported {
        /// Name of the missing capability (a `Capabilities` field name when one applies).
        capability: String,
    },
    /// `send_turn` was called while a turn is still running.
    #[error("a turn is already in progress")]
    TurnInProgress,
    /// The caller sent something the contract refuses.
    #[error("invalid request: {detail}")]
    InvalidRequest {
        /// Redacted description of what was refused.
        detail: String,
    },
    /// The session is closed.
    #[error("session closed")]
    Closed,
    /// The model provider speaks a protocol the harness cannot consume (N16). Boxed: this
    /// payload is four times the size of any other, and every `AgentEvent` carries a
    /// `ProviderError`. The serialised form stays flat (`kind` plus the four fields).
    #[error(
        "harness {} cannot use model provider {}: it serves {}, the harness accepts [{}]",
        .0.harness, .0.provider, .0.protocol, .0.accepts.join(", ")
    )]
    ModelProtocolMismatch(Box<ProtocolMismatch>),
}

/// The two sides of a pair whose protocols do not meet (see
/// [`ProviderError::ModelProtocolMismatch`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolMismatch {
    /// Id of the harness instance.
    pub harness: String,
    /// Id of the model provider.
    pub provider: String,
    /// The protocol the model provider serves.
    pub protocol: String,
    /// The protocols the harness consumes; empty for an agent that picks its own model.
    pub accepts: Vec<String>,
}

impl ProviderError {
    /// Builds [`ProviderError::Protocol`] with a redacted detail.
    pub fn protocol(detail: impl AsRef<str>) -> Self {
        Self::Protocol {
            detail: redact(detail.as_ref()),
        }
    }

    /// Builds [`ProviderError::EndpointUnreachable`] with a redacted detail.
    pub fn unreachable(detail: impl AsRef<str>) -> Self {
        Self::EndpointUnreachable {
            detail: redact(detail.as_ref()),
        }
    }

    /// Builds [`ProviderError::InvalidRequest`] with a redacted detail.
    pub fn invalid(detail: impl AsRef<str>) -> Self {
        Self::InvalidRequest {
            detail: redact(detail.as_ref()),
        }
    }

    /// Builds [`ProviderError::Unsupported`] for a named capability.
    pub fn unsupported(capability: impl Into<String>) -> Self {
        Self::Unsupported {
            capability: capability.into(),
        }
    }

    /// Whether trying again later, unchanged, can succeed.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::EndpointUnreachable { .. }
                | Self::RateLimited { .. }
                | Self::Overloaded
                | Self::Timeout { .. }
        )
    }

    /// The serialised `kind` tag, usable as a stable error code.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::CliNotFound { .. } => "cli_not_found",
            Self::AuthRequired { .. } => "auth_required",
            Self::CredentialsLocked => "credentials_locked",
            Self::Unauthorized => "unauthorized",
            Self::EndpointUnreachable { .. } => "endpoint_unreachable",
            Self::ModelNoTools { .. } => "model_no_tools",
            Self::ContextTooSmall { .. } => "context_too_small",
            Self::RateLimited { .. } => "rate_limited",
            Self::Overloaded => "overloaded",
            Self::Timeout { .. } => "timeout",
            Self::ProcessExited { .. } => "process_exited",
            Self::Protocol { .. } => "protocol",
            Self::Unsupported { .. } => "unsupported",
            Self::TurnInProgress => "turn_in_progress",
            Self::InvalidRequest { .. } => "invalid_request",
            Self::Closed => "closed",
            Self::ModelProtocolMismatch { .. } => "model_protocol_mismatch",
        }
    }

    /// HTTP status the backend answers with for this error (contract §7).
    pub fn http_status_hint(&self) -> u16 {
        match self {
            Self::CliNotFound { .. } | Self::Overloaded => 503,
            Self::AuthRequired { .. } | Self::Unauthorized => 401,
            Self::CredentialsLocked => 423,
            Self::EndpointUnreachable { .. }
            | Self::ProcessExited { .. }
            | Self::Protocol { .. } => 502,
            Self::ModelNoTools { .. }
            | Self::ContextTooSmall { .. }
            | Self::ModelProtocolMismatch { .. } => 422,
            Self::RateLimited { .. } => 429,
            Self::Timeout { .. } => 504,
            Self::Unsupported { .. } => 501,
            Self::TurnInProgress => 409,
            Self::InvalidRequest { .. } => 400,
            Self::Closed => 410,
        }
    }
}
