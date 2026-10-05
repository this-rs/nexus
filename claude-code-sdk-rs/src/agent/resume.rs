//! Opaque resume token (contract §8, decision A3).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ProviderKind;
use super::error::ProviderError;

/// What a provider needs to resume a session, opaque to everyone else.
///
/// Only the provider that issued a token reads its `data`. The host persists the
/// wire form ([`ResumeToken::to_wire`]) on the session and hands it back
/// unchanged; resuming never re-resolves the provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeToken {
    #[serde(rename = "k")]
    kind: ProviderKind,
    #[serde(rename = "v")]
    version: u32,
    #[serde(rename = "d")]
    data: Value,
}

impl ResumeToken {
    /// Builds a token. For provider adapters only.
    pub fn new(kind: ProviderKind, version: u32, data: Value) -> Self {
        Self {
            kind,
            version,
            data,
        }
    }

    /// A Claude Code token from a CLI session identifier already persisted by a
    /// host that predates the contract.
    pub fn claude_code_session(session_id: impl Into<String>) -> Self {
        Self::new(
            ProviderKind::ClaudeCode,
            1,
            serde_json::json!({ "session_id": session_id.into() }),
        )
    }

    /// Provider family that issued the token.
    pub fn kind(&self) -> ProviderKind {
        self.kind
    }

    /// Version of the token's payload, chosen by the issuing adapter.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The payload, for the adapter of the issuing provider after it has checked
    /// the kind with [`ResumeToken::expect_kind`].
    pub fn data(&self) -> &Value {
        &self.data
    }

    /// Refuses a token issued by another provider family.
    pub fn expect_kind(&self, kind: ProviderKind) -> Result<&Value, ProviderError> {
        if self.kind == kind {
            Ok(&self.data)
        } else {
            Err(ProviderError::invalid(format!(
                "resume token was issued by {:?}, not {:?}",
                self.kind, kind
            )))
        }
    }

    /// Compact JSON string to persist on the session.
    pub fn to_wire(&self) -> String {
        // A struct of a unit enum, an integer and a `Value` always serialises.
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Parses a persisted token.
    pub fn from_wire(text: &str) -> Result<Self, ProviderError> {
        serde_json::from_str(text).map_err(|_| ProviderError::invalid("malformed resume token"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_form_is_stable_and_round_trips() {
        let token = ResumeToken::claude_code_session("abc-123");
        let wire = token.to_wire();
        assert_eq!(
            wire,
            r#"{"k":"claude_code","v":1,"d":{"session_id":"abc-123"}}"#
        );
        assert_eq!(ResumeToken::from_wire(&wire).unwrap(), token);
    }

    #[test]
    fn a_token_from_another_provider_is_refused() {
        let token = ResumeToken::new(
            ProviderKind::Codex,
            1,
            serde_json::json!({"thread_id": "t"}),
        );
        assert!(token.expect_kind(ProviderKind::Codex).is_ok());
        assert!(matches!(
            token.expect_kind(ProviderKind::ClaudeCode),
            Err(ProviderError::InvalidRequest { .. })
        ));
    }

    #[test]
    fn a_malformed_token_is_an_error_that_does_not_echo_it() {
        let error = ResumeToken::from_wire("{not json sk-secretsecretsecretsecret").unwrap_err();
        assert!(!error.to_string().contains("secretsecret"));
        assert!(ResumeToken::from_wire(r#"{"k":"martian","v":1,"d":{}}"#).is_err());
    }
}
