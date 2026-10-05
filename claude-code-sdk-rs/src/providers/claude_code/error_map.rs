//! Classification of Claude Code failures into [`ProviderError`] (contract §7).
//!
//! Two sources: the SDK's own [`SdkError`], and the text of an end-of-turn
//! `result` flagged as an error, which is where the CLI reports what the
//! Anthropic API answered.

use crate::agent::ProviderError;
use crate::errors::SdkError;

/// Name reported for a missing CLI.
const CLAUDE_PROGRAM: &str = "claude";

impl From<SdkError> for ProviderError {
    /// `CliNotFound → cli_not_found`, `Timeout → timeout`,
    /// `ProcessExited → process_exited`, `NotSupported → unsupported`, anything
    /// else → `protocol` (its message goes through the redaction of
    /// [`ProviderError::protocol`]).
    fn from(error: SdkError) -> Self {
        match error {
            SdkError::CliNotFound { .. } => ProviderError::CliNotFound {
                program: CLAUDE_PROGRAM.to_owned(),
            },
            SdkError::Timeout { seconds } => ProviderError::Timeout {
                after_ms: seconds.saturating_mul(1000),
            },
            SdkError::ProcessExited { code } => ProviderError::ProcessExited { code },
            SdkError::NotSupported { feature } => ProviderError::unsupported(feature),
            other => ProviderError::protocol(other.to_string()),
        }
    }
}

/// Classifies the text of a failed turn (`result` with `is_error`): the errors
/// of the Anthropic API that a host handles differently from "the turn failed".
///
/// - overloaded (`overloaded_error`, HTTP 529) → [`ProviderError::Overloaded`] ;
/// - rate limit (`rate_limit_error`, HTTP 429) → [`ProviderError::RateLimited`] ;
/// - "prompt is too long" → [`ProviderError::ContextTooSmall`] ;
/// - HTTP 401 / `authentication_error` / invalid API key → [`ProviderError::Unauthorized`].
///
/// `None` for any other text: the turn then ends with its `done`, error flag set.
pub fn classify_result_error(text: &str) -> Option<ProviderError> {
    let lower = text.to_ascii_lowercase();
    let has = |needle: &str| lower.contains(needle);
    if has("prompt is too long") {
        return Some(ProviderError::ContextTooSmall {
            needed: None,
            available: None,
        });
    }
    if has("overloaded") || has("api error: 529") {
        return Some(ProviderError::Overloaded);
    }
    if has("rate limit") || has("rate_limit") || has("api error: 429") {
        return Some(ProviderError::RateLimited {
            retry_after_ms: None,
        });
    }
    if has("authentication_error")
        || has("authentication failed")
        || has("invalid api key")
        || has("invalid x-api-key")
        || has("api error: 401")
        || has("401 unauthorized")
    {
        return Some(ProviderError::Unauthorized);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_errors_map_as_the_contract_says() {
        assert_eq!(
            ProviderError::from(SdkError::CliNotFound {
                searched_paths: "/usr/bin\n/opt".into()
            }),
            ProviderError::CliNotFound {
                program: "claude".into()
            }
        );
        assert_eq!(
            ProviderError::from(SdkError::Timeout { seconds: 30 }),
            ProviderError::Timeout { after_ms: 30_000 }
        );
        assert_eq!(
            ProviderError::from(SdkError::ProcessExited { code: Some(3) }),
            ProviderError::ProcessExited { code: Some(3) }
        );
        assert_eq!(
            ProviderError::from(SdkError::NotSupported {
                feature: "images".into()
            }),
            ProviderError::unsupported("images")
        );
        for other in [
            SdkError::TransportError("pipe closed".into()),
            SdkError::ChannelClosed,
            SdkError::InvalidState {
                message: "Not connected".into(),
            },
        ] {
            let text = other.to_string();
            assert_eq!(ProviderError::from(other), ProviderError::protocol(text));
        }
    }

    #[test]
    fn a_protocol_error_never_carries_a_credential() {
        let error = ProviderError::from(SdkError::TransportError(
            "POST failed, Authorization: Bearer sk-ant-api03-abcdefghijklmnopqrstuvwxyz".into(),
        ));
        assert!(
            !error.to_string().contains("abcdefghijklmnopqrstuvwxyz"),
            "{error}"
        );
    }

    #[test]
    fn result_texts_are_classified() {
        assert_eq!(
            classify_result_error(
                r#"API Error: 529 {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
            ),
            Some(ProviderError::Overloaded)
        );
        assert_eq!(
            classify_result_error(
                r#"API Error: 429 {"type":"error","error":{"type":"rate_limit_error"}}"#
            ),
            Some(ProviderError::RateLimited {
                retry_after_ms: None
            })
        );
        assert_eq!(
            classify_result_error("Rate limit reached, try again later"),
            Some(ProviderError::RateLimited {
                retry_after_ms: None
            })
        );
        assert_eq!(
            classify_result_error("Prompt is too long: 250000 tokens > 200000 maximum"),
            Some(ProviderError::ContextTooSmall {
                needed: None,
                available: None
            })
        );
        assert_eq!(
            classify_result_error(
                r#"API Error: 401 {"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#
            ),
            Some(ProviderError::Unauthorized)
        );
        assert_eq!(
            classify_result_error("Invalid API key · Please run /login"),
            Some(ProviderError::Unauthorized)
        );
        assert_eq!(classify_result_error("the build failed"), None);
        assert_eq!(classify_result_error(""), None);
    }

    #[test]
    fn retryability_follows_the_classification() {
        assert!(
            classify_result_error("overloaded_error")
                .unwrap()
                .retryable()
        );
        assert!(
            classify_result_error("rate_limit_error")
                .unwrap()
                .retryable()
        );
        assert!(
            !classify_result_error("prompt is too long")
                .unwrap()
                .retryable()
        );
        assert!(
            !classify_result_error("authentication_error")
                .unwrap()
                .retryable()
        );
    }
}
