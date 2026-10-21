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

/// What a resumed session must still share with the one that issued its token
/// (PYSR-2). Only STABLE inputs: the model in force, the NAMES of the attached MCP
/// servers (never their env or headers, which carry credentials), and a digest the
/// host computes over the stable parts of its prompt. Comparing the whole prompt
/// would refuse every resume, since it embeds the date and the live context.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResumeFingerprint {
    /// Model in force when the token was issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Names of the MCP servers attached, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<String>,
    /// Opaque digest of the stable part of the prompt, computed by the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_digest: Option<String>,
}

/// A resume refused because the session no longer matches its token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeIncompatible {
    /// The field that differs: `model`, `mcp_servers` or `prompt`.
    pub field: &'static str,
}

impl std::fmt::Display for ResumeIncompatible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot resume: the {} changed since the session was opened",
            self.field
        )
    }
}

impl std::error::Error for ResumeIncompatible {}

impl ResumeFingerprint {
    /// Builds a fingerprint; `mcp_servers` is sorted and deduplicated.
    pub fn new(
        model: Option<String>,
        mcp_servers: impl IntoIterator<Item = String>,
        prompt_digest: Option<String>,
    ) -> Self {
        let mut mcp_servers: Vec<String> = mcp_servers.into_iter().collect();
        mcp_servers.sort();
        mcp_servers.dedup();
        Self {
            model,
            mcp_servers,
            prompt_digest,
        }
    }

    /// Checks that `now` still matches `self`. A side left unset (`None`) on the stored
    /// fingerprint is not compared: a token issued before PYSR-2 carries none.
    pub fn check(&self, now: &ResumeFingerprint) -> Result<(), ResumeIncompatible> {
        if self.model.is_some() && self.model != now.model {
            return Err(ResumeIncompatible { field: "model" });
        }
        if !self.mcp_servers.is_empty() && self.mcp_servers != now.mcp_servers {
            return Err(ResumeIncompatible {
                field: "mcp_servers",
            });
        }
        if self.prompt_digest.is_some() && self.prompt_digest != now.prompt_digest {
            return Err(ResumeIncompatible { field: "prompt" });
        }
        Ok(())
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
    fn fp(model: &str, mcp: &[&str], digest: &str) -> ResumeFingerprint {
        ResumeFingerprint::new(
            Some(model.into()),
            mcp.iter().map(|s| s.to_string()),
            Some(digest.into()),
        )
    }

    #[test]
    fn a_resume_with_the_same_fingerprint_is_accepted() {
        let a = fp("m1", &["po", "nexus"], "d");
        assert!(a.check(&fp("m1", &["nexus", "po"], "d")).is_ok());
    }

    #[test]
    fn a_changed_model_names_the_model() {
        let err = fp("m1", &["po"], "d")
            .check(&fp("m2", &["po"], "d"))
            .unwrap_err();
        assert_eq!(err.field, "model");
    }

    #[test]
    fn changed_mcp_servers_name_mcp_servers() {
        let err = fp("m1", &["po"], "d")
            .check(&fp("m1", &["po", "x"], "d"))
            .unwrap_err();
        assert_eq!(err.field, "mcp_servers");
    }

    #[test]
    fn a_changed_prompt_digest_names_the_prompt() {
        let err = fp("m1", &["po"], "d")
            .check(&fp("m1", &["po"], "e"))
            .unwrap_err();
        assert_eq!(err.field, "prompt");
        assert!(err.to_string().contains("prompt"));
    }

    #[test]
    fn a_token_without_fingerprint_is_never_refused() {
        assert!(
            ResumeFingerprint::default()
                .check(&fp("m9", &["z"], "q"))
                .is_ok()
        );
    }

    #[test]
    fn the_fingerprint_never_carries_a_server_value() {
        let wire = serde_json::to_string(&fp("m1", &["po"], "d")).unwrap();
        assert_eq!(
            wire,
            r#"{"model":"m1","mcp_servers":["po"],"prompt_digest":"d"}"#
        );
    }
}
