//! The signed profile token: who the session is and which tools it may use.
//!
//! `v1.<base64url(claims)>.<base64url(HMAC-SHA256(key, "v1.<base64url(claims)>"))>`.
//!
//! The harness's backend signs one per session and hands it to the server in the
//! environment (stdio) or as a bearer token (HTTP) — **never on the command line**. The
//! token carries an expiry, so it is revoked by running out; ending the session (HTTP
//! `DELETE /mcp`) forgets its state. Every failure to verify is the same answer: the
//! reason would only help someone forging tokens.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::profile::Profile;

type HmacSha256 = Hmac<Sha256>;

/// The shortest key accepted: the length of the HMAC-SHA256 output.
pub const MIN_KEY_BYTES: usize = 32;

/// The key tokens are signed with. Never printed.
#[derive(Clone)]
pub struct SigningKey(Vec<u8>);

/// Why a key was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the signing key must be at least {MIN_KEY_BYTES} bytes")]
pub struct KeyTooShort;

impl SigningKey {
    /// A key of at least [`MIN_KEY_BYTES`] bytes.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, KeyTooShort> {
        let bytes = bytes.into();
        if bytes.len() < MIN_KEY_BYTES {
            return Err(KeyTooShort);
        }
        Ok(Self(bytes))
    }
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SigningKey(<{} bytes>)", self.0.len())
    }
}

/// What a token says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// Session identifier.
    pub sid: String,
    /// The tools the session may use, by canonical name. Never `*`.
    pub tools: Vec<String>,
    /// Expiry, in seconds since the Unix epoch.
    pub exp: u64,
}

/// A token that does not verify. Deliberately one kind: see the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid token")]
pub struct InvalidToken;

fn mac(key: &SigningKey, signed: &str) -> HmacSha256 {
    // `Hmac` accepts a key of any length; the length floor is ours (`SigningKey::new`).
    let mut mac = HmacSha256::new_from_slice(&key.0).unwrap_or_else(|_| unreachable!());
    mac.update(signed.as_bytes());
    mac
}

/// Signs `claims`.
pub fn issue(key: &SigningKey, claims: &Claims) -> String {
    let body = serde_json::to_vec(claims).unwrap_or_default();
    let signed = format!("v1.{}", URL_SAFE_NO_PAD.encode(body));
    let tag = mac(key, &signed).finalize().into_bytes();
    format!("{signed}.{}", URL_SAFE_NO_PAD.encode(tag))
}

/// Verifies `token` and turns it into a [`Profile`]. `now` is in seconds since the epoch.
///
/// Refused: a bad shape, a bad signature (checked in constant time **before** the claims are
/// read), an expired token, an empty tool list, and a `*` in the list.
pub fn verify(key: &SigningKey, token: &str, now: u64) -> Result<Profile, InvalidToken> {
    let mut parts = token.split('.');
    let (Some("v1"), Some(body), Some(tag), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(InvalidToken);
    };
    let tag = URL_SAFE_NO_PAD.decode(tag).map_err(|_| InvalidToken)?;
    mac(key, &format!("v1.{body}"))
        .verify_slice(&tag)
        .map_err(|_| InvalidToken)?;
    let claims: Claims =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).map_err(|_| InvalidToken)?)
            .map_err(|_| InvalidToken)?;
    if claims.exp <= now
        || claims.sid.is_empty()
        || claims.tools.is_empty()
        || claims
            .tools
            .iter()
            .any(|tool| tool == "*" || tool.is_empty())
    {
        return Err(InvalidToken);
    }
    Ok(Profile::only(claims.sid, claims.tools))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SigningKey {
        SigningKey::new(vec![7u8; 32]).unwrap()
    }

    fn claims(exp: u64) -> Claims {
        Claims {
            sid: "s1".into(),
            tools: vec!["Read".into(), "Edit".into()],
            exp,
        }
    }

    #[test]
    fn a_short_key_is_refused_and_never_printed() {
        assert_eq!(SigningKey::new(vec![1u8; 31]).unwrap_err(), KeyTooShort);
        let shown = format!("{:?}", key());
        assert_eq!(shown, "SigningKey(<32 bytes>)");
    }

    #[test]
    fn a_token_round_trips_into_its_profile() {
        let token = issue(&key(), &claims(2_000));
        let profile = verify(&key(), &token, 1_000).unwrap();
        assert_eq!(profile.session_id, "s1");
        assert!(profile.allows("Read") && profile.allows("Edit"));
        assert!(!profile.allows("Bash"));
    }

    #[test]
    fn every_way_to_fail_is_the_same_error() {
        let good = issue(&key(), &claims(2_000));
        let other = SigningKey::new(vec![9u8; 40]).unwrap();
        let forged_claims = {
            let mut parts: Vec<&str> = good.split('.').collect();
            let swapped = URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&Claims {
                    tools: vec!["Bash".into()],
                    ..claims(2_000)
                })
                .unwrap(),
            );
            parts[1] = &swapped;
            parts.join(".")
        };
        for (label, token, key, now) in [
            ("wrong key", good.clone(), &other, 1_000),
            ("expired", good.clone(), &key(), 2_000),
            (
                "claims swapped, old signature",
                forged_claims,
                &key(),
                1_000,
            ),
            ("not a token", "hello".to_owned(), &key(), 1_000),
            ("wrong version", good.replacen("v1", "v2", 1), &key(), 1_000),
            ("extra part", format!("{good}.x"), &key(), 1_000),
            ("empty", String::new(), &key(), 1_000),
        ] {
            assert_eq!(verify(key, &token, now), Err(InvalidToken), "{label}");
        }
    }

    #[test]
    fn a_token_must_name_its_tools_and_never_a_wildcard() {
        for tools in [
            vec![],
            vec!["*".to_owned()],
            vec!["Read".to_owned(), String::new()],
        ] {
            let token = issue(
                &key(),
                &Claims {
                    tools,
                    ..claims(2_000)
                },
            );
            assert_eq!(verify(&key(), &token, 1_000), Err(InvalidToken));
        }
        let no_session = Claims {
            sid: String::new(),
            ..claims(2_000)
        };
        assert_eq!(
            verify(&key(), &issue(&key(), &no_session), 1_000),
            Err(InvalidToken)
        );
    }
}
