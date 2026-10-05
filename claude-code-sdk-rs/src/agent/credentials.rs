//! Credentials by reference (contract §11, decision A11).
//!
//! A provider instance never stores a credential: it stores a [`CredentialRef`]
//! and asks a [`CredentialResolver`] (supplied by the host) for the [`Secret`] at
//! the moment it is needed. [`Secret`] deliberately implements neither `Debug` nor
//! `Display` nor `Serialize`, so it cannot reach a log, an error or a config by
//! accident.

use std::fmt;
use std::str::FromStr;

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::error::ProviderError;

/// A credential value held in memory.
///
/// No `Debug`, no `Display`, no `Serialize`: the only way out is [`Secret::expose`].
/// The bytes are overwritten when the value is dropped (best effort).
///
/// ```compile_fail
/// let secret = nexus_claude::agent::Secret::new("hunter2");
/// println!("{:?}", secret); // Secret has no Debug
/// ```
///
/// ```compile_fail
/// let secret = nexus_claude::agent::Secret::new("hunter2");
/// println!("{}", secret); // Secret has no Display
/// ```
pub struct Secret(Vec<u8>);

impl Secret {
    /// Wraps a credential value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into().into_bytes())
    }

    /// The credential value. Call it at the last moment and do not store the result.
    pub fn expose(&self) -> &str {
        // The bytes come from a `String` and are only ever zeroed, so they are valid UTF-8.
        std::str::from_utf8(&self.0).unwrap_or_default()
    }

    /// Whether the credential is the empty string.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Length of the credential in bytes (safe to log).
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl Clone for Secret {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Where a credential lives. Serialised as `vault:<name>`, `env:<VAR>` or `none`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum CredentialRef {
    /// An entry of the host's vault.
    Vault(String),
    /// An environment variable of the host process.
    Env(String),
    /// No credential (local endpoint, CLI with its own login).
    #[default]
    None,
}

impl fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vault(name) => write!(f, "vault:{name}"),
            Self::Env(name) => write!(f, "env:{name}"),
            Self::None => f.write_str("none"),
        }
    }
}

impl FromStr for CredentialRef {
    type Err = ProviderError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let valid_name = |name: &str| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
        };
        match s.split_once(':') {
            None if s == "none" => Ok(Self::None),
            Some(("vault", name)) if valid_name(name) => Ok(Self::Vault(name.to_string())),
            Some(("env", name)) if valid_name(name) => Ok(Self::Env(name.to_string())),
            // Never echo the input: a caller who pasted a key instead of a
            // reference must not find it in an error message.
            _ => Err(ProviderError::InvalidRequest {
                detail: "credential reference must be vault:<name>, env:<VAR> or none".to_string(),
            }),
        }
    }
}

impl Serialize for CredentialRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CredentialRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Resolves a [`CredentialRef`] into a [`Secret`] on behalf of a provider instance.
///
/// The host (the backend, on its vault) implements it. A locked vault answers
/// [`ProviderError::CredentialsLocked`]; the caller must not fall back to another
/// provider.
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    /// Resolves `reference` for the instance `instance`. `Ok(None)` means "no
    /// credential", which is the expected answer for [`CredentialRef::None`].
    async fn resolve(
        &self,
        instance: &str,
        reference: &CredentialRef,
    ) -> Result<Option<Secret>, ProviderError>;
}

/// Resolver that only knows `env:` and `none` references.
///
/// A `vault:` reference answers [`ProviderError::CredentialsLocked`]: this resolver
/// has no vault, and silently answering "no credential" would send an
/// unauthenticated request where an authenticated one was configured.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvCredentialResolver;

#[async_trait]
impl CredentialResolver for EnvCredentialResolver {
    async fn resolve(
        &self,
        _instance: &str,
        reference: &CredentialRef,
    ) -> Result<Option<Secret>, ProviderError> {
        match reference {
            CredentialRef::None => Ok(None),
            CredentialRef::Env(name) => match std::env::var(name) {
                Ok(value) if !value.is_empty() => Ok(Some(Secret::new(value))),
                _ => Err(ProviderError::AuthRequired { login_hint: None }),
            },
            CredentialRef::Vault(_) => Err(ProviderError::CredentialsLocked),
        }
    }
}

/// Longest `detail` kept in an error, in bytes.
pub const MAX_DETAIL_BYTES: usize = 512;

/// Removes credential-shaped fragments from a free text and bounds its length.
///
/// Masks the value after `Bearer`/`Basic`, the value of `key=`/`token=`/`secret=`/
/// `password=`-like assignments (also in JSON form), the user-info part of a URL,
/// and well-known key prefixes (`sk-…`). Fails closed: when in doubt, mask.
pub fn redact(text: &str) -> String {
    redact_with(text, &[])
}

/// Same as [`redact`], and also masks every occurrence of the given secrets.
pub fn redact_with(text: &str, secrets: &[&Secret]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        if secret.len() >= 4 {
            out = out.replace(secret.expose(), "<redacted>");
        }
    }
    let out = mask_tokens(&out);
    truncate_on_char_boundary(out, MAX_DETAIL_BYTES)
}

const SENSITIVE_MARKERS: [&str; 9] = [
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "authorization",
    "cookie",
    "bearer",
];

pub(crate) fn is_sensitive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SENSITIVE_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Word-by-word masking. A "word" is delimited by whitespace; inside a word the
/// separators `=`, `:` and quotes are honoured so both `api_key=abc` and
/// `"api_key":"abc"` lose their value.
fn mask_tokens(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut mask_next = false;
    for (index, word) in text.split(' ').enumerate() {
        if index > 0 {
            result.push(' ');
        }
        if word.is_empty() {
            continue;
        }
        let bare = word.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ':' | '{' | '}'));
        let lower = bare.to_ascii_lowercase();
        if lower == "bearer" || lower == "basic" {
            // The scheme name is not the credential: the word after it is.
            result.push_str(word);
            mask_next = true;
            continue;
        }
        if mask_next {
            mask_next = false;
            result.push_str("<redacted>");
            continue;
        }
        result.push_str(&mask_word(word, &mut mask_next));
    }
    result
}

fn mask_word(word: &str, mask_next: &mut bool) -> String {
    // URL user-info: scheme://user:pass@host
    if let Some(scheme_end) = word.find("://") {
        let rest = &word[scheme_end + 3..];
        let authority_end = rest.find('/').unwrap_or(rest.len());
        if let Some(at) = rest[..authority_end].rfind('@') {
            return format!(
                "{}://<redacted>@{}",
                &word[..scheme_end],
                mask_query(&rest[at + 1..])
            );
        }
        return mask_query(word);
    }
    // name=value or "name":"value"
    for separator in ['=', ':'] {
        if let Some(position) = word.find(separator) {
            let (name, value) = (&word[..position], &word[position + 1..]);
            if is_sensitive_name(name) {
                let value_bare = value.trim_matches(|c: char| matches!(c, '"' | '\'' | ','));
                if value_bare.is_empty() {
                    // `Authorization: abc` — the value is the next word.
                    *mask_next = true;
                    return word.to_string();
                }
                if value_bare.eq_ignore_ascii_case("bearer")
                    || value_bare.eq_ignore_ascii_case("basic")
                {
                    *mask_next = true;
                    return word.to_string();
                }
                return format!("{name}{separator}<redacted>");
            }
        }
    }
    let bare = word.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';' | ')' | '('));
    if looks_like_key(bare) {
        return word.replace(bare, "<redacted>");
    }
    word.to_string()
}

fn mask_query(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let masked: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, _)) if is_sensitive_name(name) => format!("{name}=<redacted>"),
            _ => pair.to_string(),
        })
        .collect();
    format!("{base}?{}", masked.join("&"))
}

fn looks_like_key(word: &str) -> bool {
    const PREFIXES: [&str; 6] = ["sk-", "sk_", "nvapi-", "ghp_", "xoxb-", "eyJ"];
    word.len() >= 20 && PREFIXES.iter().any(|prefix| word.starts_with(prefix))
}

fn truncate_on_char_boundary(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push('…');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_ref_round_trips_through_its_text_form() {
        for text in ["vault:deepseek", "env:DEEPSEEK_API_KEY", "none"] {
            let parsed: CredentialRef = text.parse().unwrap();
            assert_eq!(parsed.to_string(), text);
            let json = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, format!("\"{text}\""));
            assert_eq!(
                serde_json::from_str::<CredentialRef>(&json).unwrap(),
                parsed
            );
        }
    }

    #[test]
    fn a_pasted_key_is_refused_and_never_echoed() {
        let pasted = "sk-live-0123456789abcdefghij";
        let error = pasted.parse::<CredentialRef>().unwrap_err();
        assert!(!error.to_string().contains("0123456789"));
        assert!(!format!("{error:?}").contains("0123456789"));
        assert!("vault:".parse::<CredentialRef>().is_err());
        assert!("env:A B".parse::<CredentialRef>().is_err());
        assert!("".parse::<CredentialRef>().is_err());
    }

    #[test]
    fn redact_masks_headers_assignments_urls_and_key_shapes() {
        let cases = [
            ("Authorization: Bearer abc.def.ghi", "abc.def.ghi"),
            ("authorization: tok_12345", "tok_12345"),
            ("api_key=topsecretvalue&x=1", "topsecretvalue"),
            ("{\"api_key\":\"topsecretvalue\"}", "topsecretvalue"),
            ("GET https://user:hunter2pass@host/v1", "hunter2pass"),
            ("GET https://host/v1?token=tok_abcdef&x=1", "tok_abcdef"),
            (
                "failed with sk-abcdefghijklmnopqrstuvwxyz",
                "abcdefghijklmnop",
            ),
            ("PASSWORD=p4ssw0rd", "p4ssw0rd"),
        ];
        for (input, leak) in cases {
            let output = redact(input);
            assert!(!output.contains(leak), "{input:?} leaked into {output:?}");
            assert!(output.contains("<redacted>"), "{input:?} -> {output:?}");
        }
        assert_eq!(
            redact("connection refused (os error 61)"),
            "connection refused (os error 61)"
        );
        assert_eq!(
            redact("GET https://host/v1/models?limit=3"),
            "GET https://host/v1/models?limit=3"
        );
    }

    #[test]
    fn redact_with_masks_a_registered_secret_wherever_it_appears() {
        let secret = Secret::new("plain-looking-value");
        let output = redact_with("upstream said: plain-looking-value is wrong", &[&secret]);
        assert_eq!(output, "upstream said: <redacted> is wrong");
    }

    #[test]
    fn redact_bounds_the_length_on_a_char_boundary() {
        let long = "é".repeat(600);
        let output = redact(&long);
        assert!(output.len() <= MAX_DETAIL_BYTES + '…'.len_utf8());
        assert!(output.ends_with('…'));
    }

    #[tokio::test]
    async fn env_resolver_refuses_vault_references_instead_of_answering_none() {
        let resolver = EnvCredentialResolver;
        assert!(
            resolver
                .resolve("i", &CredentialRef::None)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            resolver
                .resolve("i", &CredentialRef::Vault("k".into()))
                .await
                .err(),
            Some(ProviderError::CredentialsLocked)
        );
        assert_eq!(
            resolver
                .resolve(
                    "i",
                    &CredentialRef::Env("NEXUS_TEST_SURELY_UNSET_VAR".into())
                )
                .await
                .err(),
            Some(ProviderError::AuthRequired { login_hint: None })
        );
        // PATH is always set in a test process.
        let path = resolver
            .resolve("i", &CredentialRef::Env("PATH".into()))
            .await
            .unwrap()
            .unwrap();
        assert!(!path.is_empty());
        assert_eq!(path.len(), path.expose().len());
    }
}
