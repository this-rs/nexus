//! The rule of a consent about a SUBJECT (N23b), in one place for every harness that asks one.
//!
//! A hook may hand a call back to the host with [`HookVerdict::RequireConsent`]: a person must
//! consent to a **subject** — the origin a page would reach, the host — before the call runs.
//! Two harnesses answer that question through different channels (the native engine asks in
//! band, Claude Code through the CLI's own permission flow) and must still keep the same
//! memory: what a `session` answer covers, and where it never spills.
//!
//! [`HookVerdict::RequireConsent`]: crate::agent::HookVerdict::RequireConsent

/// Separator of a consent key. It cannot appear in a tool name (`mcp__server__tool`) nor in a
/// subject (an origin, a host), so a key never collides with a bare tool name.
const CONSENT_SUBJECT_SEPARATOR: char = '\u{0}';

/// What a `session` answer is remembered under: the tool alone for an ordinary tool approval,
/// the tool **and** the subject when a hook asked about one. Answering "session" for
/// `browser_navigate` on one origin must not open every other origin.
pub(crate) fn consent_key(tool: &str, subject: &str) -> String {
    format!("{tool}{CONSENT_SUBJECT_SEPARATOR}{subject}")
}

/// Where a [`HookVerdict::RequireConsent`] verdict sends the call.
///
/// [`HookVerdict::RequireConsent`]: crate::agent::HookVerdict::RequireConsent
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsentRoute {
    /// This tool+subject was approved for the session: the call runs.
    Approved,
    /// A human must answer: the harness asks.
    Ask,
    /// Nobody can be asked (no `interactive_permissions`: an agent session, a delegation, a
    /// runner): the call is refused. Fail closed, never open.
    Refused,
}

/// The rule of `RequireConsent`, in one place: an approval already given wins, then a session
/// that can ask asks, and anything else is refused.
pub(crate) fn consent_route(approved: bool, interactive: bool) -> ConsentRoute {
    if approved {
        ConsentRoute::Approved
    } else if interactive {
        ConsentRoute::Ask
    } else {
        ConsentRoute::Refused
    }
}

#[cfg(test)]
mod tests {
    use super::{ConsentRoute, consent_key, consent_route};

    /// A tool approval and a subject approval never share a key: the separator of a consent key
    /// is a character no tool name and no origin can hold.
    #[test]
    fn a_consent_key_is_the_tool_and_its_subject_and_never_a_bare_tool_name() {
        let key = consent_key("browser_navigate", "https://example.com");
        assert_eq!(key, "browser_navigate\u{0}https://example.com");
        assert_ne!(key, "browser_navigate");
        assert_ne!(
            key,
            consent_key("browser_navigate", "https://evil.example"),
            "two origins are two consents"
        );
        assert_ne!(
            consent_key("write", "https://example.com"),
            consent_key("read", "https://example.com"),
            "two tools are two consents"
        );
    }

    /// The rule of `RequireConsent`, before any I/O: an approval wins, then a session that can
    /// ask asks, and a session where nobody can be asked is REFUSED (fail closed).
    #[test]
    fn a_consent_is_approved_asked_or_refused_and_the_last_case_never_opens() {
        assert_eq!(consent_route(true, true), ConsentRoute::Approved);
        assert_eq!(
            consent_route(true, false),
            ConsentRoute::Approved,
            "an approval a human already gave still holds where nobody answers"
        );
        assert_eq!(consent_route(false, true), ConsentRoute::Ask);
        assert_eq!(
            consent_route(false, false),
            ConsentRoute::Refused,
            "nobody can be asked: the call does not run"
        );
    }
}
