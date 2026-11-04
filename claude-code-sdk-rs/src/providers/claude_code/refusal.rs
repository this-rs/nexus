//! Reading of a REFUSAL: the CLI's answer when the account or the model has
//! nothing left to give.
//!
//! # Why this module exists, and what was measured
//!
//! On 2026-11-04 a real session pinned to `claude-fable-5-1` (Claude Code 2.1.318)
//! answered, twice in a row, with the refusal AS ITS ANSWER:
//!
//! ```text
//! You've reached your Fable limit. Switch to another model, or manage usage
//! credits at https://claude.ai/settings/usage?from=cc_cli_limit_message, to continue.
//! ```
//!
//! Nothing upstream had flagged it: no `rate_limit_event` was emitted at all, and
//! the end-of-turn `result` carried `is_error: true` **with** `subtype: "success"`,
//! which the projection read as a completed turn. The host then had no error to
//! retry, no quota to mark, and no reason to switch model — the person was left to
//! ask again, and got the same sentence.
//!
//! # Where the vocabulary comes from
//!
//! Not from guessing and not from documentation: from the CLI's own message
//! builder, in the shipped 2.1.318 bundle (function `WVo`), whose branches are
//! quoted one by one below. Every phrase handled here is a phrase that binary can
//! emit; every test in this file is a phrase copied from it or from a capture.
//!
//! # The one distinction that decides the reaction
//!
//! * **Scope `Account`** — the five-hour / seven-day window, the credits, the
//!   organisation's budget: every model of the account is refused, so switching
//!   model is useless and would only burn a call.
//! * **Scope `Model`** — "your Fable limit", "`seven_day_opus`": that model is
//!   out, its neighbours are not. Switching model is exactly the remedy, and the
//!   CLI says so itself ("Switch to another model").
//! * **Scope `Context`** — the credits needed for the 1M context window: neither
//!   of the two above; the model and the account are fine.
//! * **Scope `Unknown`** — a limit we can read but cannot place. A host treats it
//!   as `Account`: a wrong switch would spend a call for nothing.
//!
//! This module reads TEXT, which is the weaker of the two sources a refusal can
//! have. When the CLI also sends a `rate_limit_event`, its `rateLimitType` is the
//! machine-readable source and wins (see `map_events::quota_from_rate_limit_info`).
//! The text path stays because, as measured above, the structured one is silent
//! on exactly the case that hurt.

use crate::agent::ProviderError;

/// What a refusal refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusalScope {
    /// Every model of the account: switching model cannot help.
    Account,
    /// One model, named as the CLI writes it ("Fable", "Opus"…): switching helps.
    Model(String),
    /// The credits the 1M context window needs; neither account nor model is out.
    Context,
    /// A limit that was read but not placed. Treated as [`Self::Account`] by hosts.
    Unknown,
}

impl RefusalScope {
    /// Whether moving to another model is a sensible reaction to this refusal.
    pub fn switch_model_helps(&self) -> bool {
        matches!(self, Self::Model(_))
    }

    /// The model this refusal names, if it names one.
    pub fn model(&self) -> Option<&str> {
        match self {
            Self::Model(name) => Some(name),
            _ => None,
        }
    }
}

/// A refusal read out of the CLI's own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// What is refused.
    pub scope: RefusalScope,
}

impl Refusal {
    fn new(scope: RefusalScope) -> Self {
        Self { scope }
    }

    /// The typed error a host reacts to.
    ///
    /// A refusal that names a model becomes [`ProviderError::ModelLimitReached`]
    /// (switching model is the remedy); a whole-account one stays
    /// [`ProviderError::RateLimited`] (waiting, or another provider, is the remedy);
    /// a context-credit one is the context error it always was.
    pub fn error(&self) -> ProviderError {
        match &self.scope {
            RefusalScope::Model(name) => ProviderError::ModelLimitReached {
                model: Some(name.clone()),
                resets_at_ms: None,
            },
            RefusalScope::Context => ProviderError::ContextTooSmall {
                needed: None,
                available: None,
            },
            RefusalScope::Account | RefusalScope::Unknown => ProviderError::RateLimited {
                retry_after_ms: None,
                resets_at_ms: None,
            },
        }
    }
}

/// The phrases of the CLI's builder that place a refusal, quoted from the 2.1.318
/// bundle. Matched case-insensitively on the whole text.
///
/// Order matters: the context clause is checked first (it contains "credits"), then
/// the account-wide ones, then the model one — whose shape ("reached your … limit")
/// would otherwise swallow "your specified usage limits".
const OUT_OF_CREDITS: &[&str] = &[
    // `You're out of usage credits. Switch to another model…`
    "out of usage credits",
    // `${r} requires usage credits.` — `r` is the model, but a bare "requires usage
    // credits" is the account's credit pool, so it is read as Account unless a model
    // name sits in front of it (see `model_requiring_credits`).
    "usage credits are required",
    "extra usage is required",
];

/// Organisation-level ceilings: the account's budget, not one model's.
const ACCOUNT_CEILINGS: &[&str] = &[
    // `You've hit your monthly spend limit.`
    "monthly spend limit",
    // `You've hit your channel's monthly spend limit.`
    "channel's monthly spend limit",
    // `You've hit your team's shared budget.`
    "shared budget",
    // `You have reached your specified usage limits.` — the 429's generic wording,
    // matched by the CLI's own `usage_cap_reached` pattern
    // `reached your specified[\w\s-]*?usage limits`.
    "reached your specified",
    "specified usage limits",
];

/// The credits the 1M context needs.
const CONTEXT_CREDITS: &[&str] = &["1m context", "long context"];

/// A limit the CLI names without placing it.
const PLAIN_LIMITS: &[&str] = &[
    "rate limit",       // the CLI's own `serviceErrorMessage`
    "rate_limit",
    "api error: 429",   // the API's own wording, when the CLI passes it through
    "request rejected (429)",
    "too many requests",
];

/// Reads the refusal a `result` carries, when it carries one.
///
/// `None` for any other text: a turn that merely mentions a limit in passing is
/// not a refusal, and a host must not switch model on prose that only looks like one.
pub fn classify_refusal(text: &str) -> Option<Refusal> {
    let lower = text.to_ascii_lowercase();
    let has = |needle: &str| lower.contains(needle);

    if CONTEXT_CREDITS.iter().any(|n| has(n))
        && (has("credit") || has("usage"))
        // "1m context" alone also appears in ordinary tool output; require the
        // refusal's shape around it.
        && (has("required") || has("requires") || has("turn them on"))
    {
        return Some(Refusal::new(RefusalScope::Context));
    }

    if ACCOUNT_CEILINGS.iter().any(|n| has(n)) {
        return Some(Refusal::new(RefusalScope::Account));
    }

    if OUT_OF_CREDITS.iter().any(|n| has(n)) {
        return Some(Refusal::new(RefusalScope::Account));
    }

    // `You've reached your ${model} limit.` / `You've reached your Fable limit.`
    if let Some(name) = reached_your_limit(&lower, text) {
        return Some(Refusal::new(RefusalScope::Model(name)));
    }

    // `${model} requires usage credits.` — the model is named, the credits are its own.
    if has("requires usage credits")
        && let Some(name) = model_before(&lower, text, "requires usage credits")
    {
        return Some(Refusal::new(RefusalScope::Model(name)));
    }

    if is_plain_limit(&lower) {
        return Some(Refusal::new(RefusalScope::Unknown));
    }

    None
}

/// Whether the text is the CLI's error channel saying "limit", as opposed to prose
/// that merely talks about limits ("The rate limit of the API is documented in the
/// README." must not switch a model). Three shapes qualify: the CLI's `API Error:`
/// prefix, the API's own `429`/`rate_limit_error`, and the short "Rate limit
/// reached, try again later" sentence the CLI has long produced — whose refusal is
/// carried by its verb, not by the noun alone.
fn is_plain_limit(lower: &str) -> bool {
    if !PLAIN_LIMITS.iter().any(|n| lower.contains(n)) {
        return false;
    }
    const REFUSAL_VERBS: &[&str] = &["reached", "exceeded", "try again", "retry"];
    lower.contains("api error")
        || lower.contains("rate_limit_error")
        || lower.contains("rate limit error")
        || lower.contains("(429)")
        || lower.contains("429 ")
        || REFUSAL_VERBS.iter().any(|v| lower.contains(v))
}

/// The model named by `you've reached your <model> limit`, as the CLI wrote it.
///
/// `lower` is `text` lowercased ASCII-only, so its byte offsets are `text`'s: the
/// name is cut out of the ORIGINAL, which is what a person and the UI read ("Fable",
/// not "fable"). The words between are kept verbatim up to a short bound: the CLI
/// puts a display name there, never a sentence.
fn reached_your_limit(lower: &str, text: &str) -> Option<String> {
    const HEAD: &str = "reached your";
    const TAIL: &str = " limit";
    let start = lower.find(HEAD)? + HEAD.len();
    let end = start + lower[start..].find(TAIL)?;
    named(&text[start..end])
}

/// The model named immediately before `phrase` (`"fable requires usage credits"`).
fn model_before(lower: &str, text: &str, phrase: &str) -> Option<String> {
    let at = lower.find(phrase)?;
    let head = text[..at].trim_end();
    // The builder writes `${r} requires usage credits.`, `r` being the display
    // name: take the last few words before it, no more.
    let words: Vec<&str> = head.split_whitespace().rev().take(3).collect();
    if words.is_empty() {
        return None;
    }
    let candidate = words.into_iter().rev().collect::<Vec<_>>().join(" ");
    named(&format!(" {candidate}"))
}

/// A display name pulled out of the sentence, or `None` when what sits there is
/// prose ("specified usage limits") rather than a model.
fn named(raw: &str) -> Option<String> {
    let name = raw
        .trim()
        .trim_matches(|c: char| c == '.' || c == ',' || c == ':' || c == '"' || c == '\'');
    if name.is_empty() || name.len() > 32 {
        return None;
    }
    // A model name has no verb and no trailing clause; these words mean the text
    // is a sentence, so the scope is not "one model".
    const PROSE: &[&str] = &[
        "specified", "usage", "your", "the", "a", "an", "rate", "reached", "and", "or",
    ];
    let first = name.split_whitespace().next()?.to_ascii_lowercase();
    if PROSE.contains(&first.as_str()) {
        return None;
    }
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact sentence of the screenshot (2026-11-04, `claude-fable-5-1`).
    const FABLE: &str = "You've reached your Fable limit. Switch to another model, or \
manage usage credits at https://claude.ai/settings/usage?from=cc_cli_limit_message, to \
continue.";

    /// The exact `result` text of a real 429 capture, against a local endpoint
    /// answering `anthropic-ratelimit-unified-status: rejected`.
    const CAPTURED_429: &str =
        "API Error: Request rejected (429) · You have reached your specified usage limits.";

    #[test]
    fn the_fable_refusal_names_its_model_and_says_a_switch_helps() {
        let refusal = classify_refusal(FABLE).expect("a refusal");
        assert_eq!(refusal.scope, RefusalScope::Model("Fable".into()));
        assert!(refusal.scope.switch_model_helps());
    }

    #[test]
    fn the_captured_429_is_a_whole_account_ceiling_so_a_switch_cannot_help() {
        let refusal = classify_refusal(CAPTURED_429).expect("a refusal");
        assert_eq!(refusal.scope, RefusalScope::Account);
        assert!(!refusal.scope.switch_model_helps());
    }

    #[test]
    fn every_wording_of_the_cli_builder_lands_on_its_scope() {
        // `WVo`, Claude Code 2.1.318, branch by branch.
        let table: &[(&str, RefusalScope)] = &[
            (
                "You've hit your channel's monthly spend limit. Switch to another model, or manage usage credits at https://x, to continue.",
                RefusalScope::Account,
            ),
            (
                "You've hit your monthly spend limit. Switch to another model to continue.",
                RefusalScope::Account,
            ),
            (
                "You've hit your team's shared budget. /model to switch models.",
                RefusalScope::Account,
            ),
            (
                "You're out of usage credits. /model to switch models.",
                RefusalScope::Account,
            ),
            (
                "You're out of usage credits. Switch to another model, or manage usage credits at https://x, to continue.",
                RefusalScope::Account,
            ),
            (
                "Opus requires usage credits. Run /usage-credits to continue or switch models with /model.",
                RefusalScope::Model("Opus".into()),
            ),
            (
                "Fable requires usage credits. Run /usage-credits to continue or switch models with /model.",
                RefusalScope::Model("Fable".into()),
            ),
            (
                "You've reached your Sonnet limit. Switch to another model to continue.",
                RefusalScope::Model("Sonnet".into()),
            ),
            (
                "API Error: Usage credits required for 1M context · run /model to switch to standard context",
                RefusalScope::Context,
            ),
            (
                "API Error: 429 {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}",
                RefusalScope::Unknown,
            ),
        ];
        for (text, expected) in table {
            let refusal = classify_refusal(text).unwrap_or_else(|| panic!("no refusal for {text}"));
            assert_eq!(&refusal.scope, expected, "{text}");
        }
    }

    #[test]
    fn ordinary_prose_about_a_limit_is_not_a_refusal() {
        // What the agent may legitimately write as an answer.
        for text in [
            "The rate limit of the API is documented in the README.",
            "J'ai atteint la limite du fichier",
            "the timeout is a limit the CLI enforces",
            "",
        ] {
            assert!(
                classify_refusal(text).is_none(),
                "{text} must not be read as a refusal"
            );
        }
    }

    #[test]
    fn a_named_limit_that_is_really_prose_does_not_name_a_model() {
        // "reached your specified usage limits" has the shape of the model clause;
        // the prose guard must keep it out of `Model`.
        let refusal = classify_refusal("You have reached your specified usage limits.").unwrap();
        assert_eq!(refusal.scope, RefusalScope::Account);
    }
}
