//! Neutral tool policy ↔ Claude Code's own vocabulary (contract §6), and the
//! category and stable alias of Claude Code's tools (decision A8).
//!
//! | Native mode | → neutral | neutral → native |
//! |---|---|---|
//! | `default`, `manual` | `ask` | `ask` → `default` |
//! | `acceptEdits` | `auto_edits` | `auto_edits` → `acceptEdits` |
//! | `auto` | `auto_edits` | only through `native_mode` |
//! | `dontAsk` | `ask` | only through `native_mode` |
//! | `plan` | `plan_only` | `plan_only` → `plan` |
//! | `bypassPermissions` | `trust` | `trust` → `bypassPermissions` |

use crate::agent::{PolicyMode, ToolCategory, ToolPolicy};
use crate::types::PermissionMode;

/// Every permission mode the Claude Code CLI knows: its six modes, plus the
/// legacy `default`.
pub const NATIVE_MODES: [&str; 7] = [
    "default",
    "manual",
    "acceptEdits",
    "auto",
    "dontAsk",
    "plan",
    "bypassPermissions",
];

/// Neutral mode of a Claude Code permission mode; `None` for a string the CLI
/// does not know.
pub fn native_to_neutral(native: &str) -> Option<PolicyMode> {
    match native {
        "default" | "manual" | "dontAsk" => Some(PolicyMode::Ask),
        "acceptEdits" | "auto" => Some(PolicyMode::AutoEdits),
        "plan" => Some(PolicyMode::PlanOnly),
        "bypassPermissions" => Some(PolicyMode::Trust),
        _ => None,
    }
}

/// Claude Code mode emitted for a neutral mode when the caller names none.
fn default_native(mode: PolicyMode) -> &'static str {
    match mode {
        PolicyMode::PlanOnly => "plan",
        PolicyMode::Ask => "default",
        PolicyMode::AutoEdits => "acceptEdits",
        PolicyMode::Trust => "bypassPermissions",
    }
}

/// Claude Code mode to apply for a neutral mode.
///
/// `native_override` is the caller's exact mode (`ToolPolicy::native_mode`, or
/// the `native` argument of `set_policy_mode`). It is applied only when it is a
/// Claude Code mode **of the same neutral mode**: `auto` for `auto_edits`,
/// `dontAsk` or `manual` for `ask`. A string of another provider, or one that
/// would change the neutral mode (`bypassPermissions` next to `ask`), is
/// ignored: the neutral mode is what the policy ceiling was checked against.
pub fn neutral_to_native(mode: PolicyMode, native_override: Option<&str>) -> String {
    match native_override {
        Some(native) if native_to_neutral(native) == Some(mode) => native.to_owned(),
        _ => default_native(mode).to_owned(),
    }
}

/// The SDK's four-valued [`PermissionMode`] for a neutral mode.
pub fn permission_mode(mode: PolicyMode) -> PermissionMode {
    match mode {
        PolicyMode::PlanOnly => PermissionMode::Plan,
        PolicyMode::Ask => PermissionMode::Default,
        PolicyMode::AutoEdits => PermissionMode::AcceptEdits,
        PolicyMode::Trust => PermissionMode::BypassPermissions,
    }
}

/// A [`ToolPolicy`] in Claude Code's terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicy {
    /// Mode for `ClaudeCodeOptions::permission_mode` (`--permission-mode`).
    pub permission_mode: PermissionMode,
    /// The exact Claude Code mode asked for, as a string (see [`neutral_to_native`]).
    pub native_mode: String,
    /// `Some` when [`NativePolicy::native_mode`] is one of the modes the SDK's
    /// [`PermissionMode`] cannot express (`auto`, `dontAsk`, `manual`).
    ///
    /// **Not applied at launch yet**: `SubprocessTransport::build_command` only
    /// renders the four values of [`PermissionMode`], so a session asked for
    /// `auto` starts in `acceptEdits` (and `dontAsk` / `manual` in `default`).
    /// Carrying the six modes to the command line is the next slice; the value is
    /// returned here so that slice has nothing to recompute. At run time the six
    /// modes already work: `set_policy_mode` writes the string as is.
    pub native_override: Option<String>,
    /// `ToolPolicy::allow`, in Claude Code's `Tool(glob)` syntax.
    pub allowed_tools: Vec<String>,
    /// `ToolPolicy::deny`, in Claude Code's `Tool(glob)` syntax.
    pub disallowed_tools: Vec<String>,
}

/// Translates a neutral policy.
pub fn translate_policy(policy: &ToolPolicy) -> NativePolicy {
    let native_mode = neutral_to_native(policy.mode, policy.native_mode.as_deref());
    let native_override = (native_mode != default_native(policy.mode)).then(|| native_mode.clone());
    NativePolicy {
        permission_mode: permission_mode(policy.mode),
        native_mode,
        native_override,
        allowed_tools: policy.allow.iter().map(ToString::to_string).collect(),
        disallowed_tools: policy.deny.iter().map(ToString::to_string).collect(),
    }
}

/// Category of a Claude Code tool.
pub fn tool_category(name: &str) -> ToolCategory {
    match name {
        "Bash" | "BashOutput" | "KillShell" => ToolCategory::Command,
        "Read" | "NotebookRead" => ToolCategory::Read,
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => ToolCategory::Edit,
        "Glob" | "Grep" | "LS" => ToolCategory::Search,
        "WebFetch" | "WebSearch" => ToolCategory::Web,
        "Task" | "Agent" => ToolCategory::Agent,
        name if name.starts_with("mcp__") => ToolCategory::Mcp,
        _ => ToolCategory::Other,
    }
}

/// Stable alias of a Claude Code tool. Claude Code already spells an MCP tool
/// `mcp__<server>__<tool>` and its own tools by their public name, so the alias
/// is the name itself; `None` for an empty name (a tool call whose block has
/// only just started).
pub fn canonical_name(name: &str) -> Option<String> {
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_native_mode_has_the_neutral_mode_of_the_table() {
        let table = [
            ("default", PolicyMode::Ask),
            ("manual", PolicyMode::Ask),
            ("acceptEdits", PolicyMode::AutoEdits),
            ("auto", PolicyMode::AutoEdits),
            ("dontAsk", PolicyMode::Ask),
            ("plan", PolicyMode::PlanOnly),
            ("bypassPermissions", PolicyMode::Trust),
        ];
        assert_eq!(table.len(), NATIVE_MODES.len());
        for (native, neutral) in table {
            assert!(NATIVE_MODES.contains(&native));
            assert_eq!(native_to_neutral(native), Some(neutral), "{native}");
        }
        assert_eq!(native_to_neutral("on-request"), None);
        assert_eq!(native_to_neutral(""), None);
    }

    #[test]
    fn each_neutral_mode_emits_the_native_mode_of_the_table() {
        assert_eq!(neutral_to_native(PolicyMode::Ask, None), "default");
        assert_eq!(
            neutral_to_native(PolicyMode::AutoEdits, None),
            "acceptEdits"
        );
        assert_eq!(neutral_to_native(PolicyMode::PlanOnly, None), "plan");
        assert_eq!(
            neutral_to_native(PolicyMode::Trust, None),
            "bypassPermissions"
        );
    }

    #[test]
    fn a_native_override_applies_only_within_its_neutral_mode() {
        assert_eq!(
            neutral_to_native(PolicyMode::AutoEdits, Some("auto")),
            "auto"
        );
        assert_eq!(
            neutral_to_native(PolicyMode::Ask, Some("dontAsk")),
            "dontAsk"
        );
        assert_eq!(neutral_to_native(PolicyMode::Ask, Some("manual")), "manual");
        // Never an escalation through the native string.
        assert_eq!(
            neutral_to_native(PolicyMode::Ask, Some("bypassPermissions")),
            "default"
        );
        assert_eq!(
            neutral_to_native(PolicyMode::PlanOnly, Some("auto")),
            "plan"
        );
        // Another provider's vocabulary is ignored.
        assert_eq!(
            neutral_to_native(PolicyMode::AutoEdits, Some("on-request")),
            "acceptEdits"
        );
    }

    #[test]
    fn a_policy_translates_to_mode_and_tool_lists() {
        let policy = ToolPolicy::from_patterns(
            PolicyMode::Ask,
            &["Read", "Bash(git:*)"],
            &["mcp__po__admin"],
        )
        .unwrap();
        assert_eq!(
            translate_policy(&policy),
            NativePolicy {
                permission_mode: PermissionMode::Default,
                native_mode: "default".into(),
                native_override: None,
                allowed_tools: vec!["Read".into(), "Bash(git *)".into()],
                disallowed_tools: vec!["mcp__po__admin".into()],
            }
        );
    }

    #[test]
    fn the_modes_outside_the_sdk_enum_come_back_as_an_override() {
        for (mode, native, launch) in [
            (PolicyMode::AutoEdits, "auto", PermissionMode::AcceptEdits),
            (PolicyMode::Ask, "dontAsk", PermissionMode::Default),
            (PolicyMode::Ask, "manual", PermissionMode::Default),
        ] {
            let mut policy = ToolPolicy::new(mode);
            policy.native_mode = Some(native.to_owned());
            let translated = translate_policy(&policy);
            assert_eq!(translated.permission_mode, launch);
            assert_eq!(translated.native_mode, native);
            assert_eq!(translated.native_override.as_deref(), Some(native));
        }
        let mut policy = ToolPolicy::new(PolicyMode::Trust);
        policy.native_mode = Some("bypassPermissions".to_owned());
        assert_eq!(translate_policy(&policy).native_override, None);
        assert_eq!(
            translate_policy(&policy).permission_mode,
            PermissionMode::BypassPermissions
        );
        assert_eq!(
            translate_policy(&ToolPolicy::new(PolicyMode::PlanOnly)).permission_mode,
            PermissionMode::Plan
        );
    }

    #[test]
    fn tools_have_the_category_of_the_table() {
        use ToolCategory::{Agent, Command, Edit, Mcp, Other, Read, Search, Web};
        for (name, category) in [
            ("Bash", Command),
            ("BashOutput", Command),
            ("KillShell", Command),
            ("Read", Read),
            ("NotebookRead", Read),
            ("Edit", Edit),
            ("Write", Edit),
            ("MultiEdit", Edit),
            ("NotebookEdit", Edit),
            ("Glob", Search),
            ("Grep", Search),
            ("LS", Search),
            ("WebFetch", Web),
            ("WebSearch", Web),
            ("mcp__po__plan", Mcp),
            ("Task", Agent),
            ("Agent", Agent),
            ("TodoWrite", Other),
            ("AskUserQuestion", Other),
            ("", Other),
        ] {
            assert_eq!(tool_category(name), category, "{name}");
        }
    }

    #[test]
    fn the_canonical_name_is_the_name() {
        assert_eq!(canonical_name("Bash").as_deref(), Some("Bash"));
        assert_eq!(
            canonical_name("mcp__po__plan").as_deref(),
            Some("mcp__po__plan")
        );
        assert_eq!(canonical_name(""), None);
    }
}
