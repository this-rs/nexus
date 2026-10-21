//! Neutral tool policy (contract §6, decision A8).
//!
//! [`ToolPolicy`] is what the host asks for; each adapter translates it to its
//! provider's own vocabulary. The local decision ([`ToolPolicy::decide`]) is used
//! by the native harness and by fallbacks, and refuses by default.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::error::ProviderError;

/// How much the agent may do without asking, from least to most permissive.
///
/// The declaration order is the permissiveness order: `PlanOnly < Ask < AutoEdits < Trust`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PolicyMode {
    /// Read and search only; nothing is changed.
    PlanOnly,
    /// Ask before anything that is not a read or a search.
    #[default]
    Ask,
    /// Edits are applied without asking; the rest still asks.
    AutoEdits,
    /// Nothing asks. Refused for a third-party provider without a sandbox.
    Trust,
}

/// Category of a tool, supplied by the adapter (A8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolCategory {
    /// Runs a command (shell).
    Command,
    /// Reads a file or a resource.
    Read,
    /// Writes or edits a file.
    Edit,
    /// Searches (glob, grep, index).
    Search,
    /// Reaches the web.
    Web,
    /// A tool served over MCP.
    Mcp,
    /// Spawns or drives another agent.
    Agent,
    /// Anything else.
    #[default]
    Other,
}

/// Outcome of the local policy decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    /// Run without asking.
    Allow,
    /// Ask the user.
    Ask,
    /// Refuse without asking.
    Deny,
}

/// A tool pattern: a tool name and an optional argument glob.
///
/// Text form (also the JSON form): `Tool` or `Tool(glob)`, the syntax Claude Code
/// uses. `*` matches any run of characters, in the name and in the argument. The
/// legacy `Bash(git:*)` is accepted and normalised to `Bash(git *)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolPattern {
    /// Tool name glob (`Read`, `mcp__po__*`).
    pub tool: String,
    /// Argument glob (`git *`), when the pattern restricts the argument.
    pub arg: Option<String>,
}

impl ToolPattern {
    /// Pattern on a tool name only.
    pub fn tool(name: impl Into<String>) -> Self {
        Self {
            tool: name.into(),
            arg: None,
        }
    }

    /// Whether this pattern matches a call, for an **allow** list: an argument
    /// pattern needs a known argument to match.
    pub fn matches(&self, tool: &str, arg: Option<&str>) -> bool {
        if !glob_match(&self.tool, tool) {
            return false;
        }
        match (&self.arg, arg) {
            (None, _) => true,
            (Some(pattern), Some(arg)) => glob_match(pattern, arg),
            (Some(_), None) => false,
        }
    }

    /// Whether this pattern matches a call, for a **deny** list: when the argument
    /// is unknown the pattern is assumed to match (fail closed).
    pub fn matches_closed(&self, tool: &str, arg: Option<&str>) -> bool {
        if !glob_match(&self.tool, tool) {
            return false;
        }
        match (&self.arg, arg) {
            (Some(pattern), Some(arg)) => glob_match(pattern, arg),
            _ => true,
        }
    }

    /// Whether every call matched by `self` is also matched by `wider`.
    ///
    /// Conservative: decided on the pattern texts, so it may answer `false` for two
    /// equivalent globs, never `true` for a pattern that escapes `wider`.
    pub fn is_covered_by(&self, wider: &ToolPattern) -> bool {
        if !glob_covers(&wider.tool, &self.tool) {
            return false;
        }
        match (&wider.arg, &self.arg) {
            (None, _) => true,
            (Some(wide), Some(narrow)) => glob_covers(wide, narrow),
            (Some(_), None) => false,
        }
    }
}

impl fmt::Display for ToolPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.arg {
            Some(arg) => write!(f, "{}({})", self.tool, arg),
            None => f.write_str(&self.tool),
        }
    }
}

impl FromStr for ToolPattern {
    type Err = ProviderError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let malformed = || ProviderError::invalid(format!("malformed tool pattern: {text}"));
        let text = text.trim();
        let (tool, arg) = match text.find('(') {
            None => (text, None),
            Some(open) => {
                if !text.ends_with(')') {
                    return Err(malformed());
                }
                let arg = &text[open + 1..text.len() - 1];
                if arg.is_empty() {
                    return Err(malformed());
                }
                (&text[..open], Some(arg))
            },
        };
        let name_ok = !tool.is_empty()
            && tool
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '*' | '.'));
        if !name_ok {
            return Err(malformed());
        }
        let arg = arg.map(|arg| match arg.strip_suffix(":*") {
            // Legacy Claude form `Bash(git:*)`.
            Some(prefix) if !prefix.is_empty() => format!("{prefix} *"),
            _ => arg.to_string(),
        });
        Ok(Self {
            tool: tool.to_string(),
            arg,
        })
    }
}

impl Serialize for ToolPattern {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ToolPattern {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// What the agent may do, in provider-neutral terms.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolPolicy {
    /// Base mode.
    pub mode: PolicyMode,
    /// Exact provider mode when the caller knows it (legacy strings such as
    /// `acceptEdits`, `dontAsk`). An adapter that recognises it applies it; the
    /// others ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_mode: Option<String>,
    /// Calls approved in advance.
    #[serde(default)]
    pub allow: Vec<ToolPattern>,
    /// Calls refused whatever the mode. Deny always wins.
    #[serde(default)]
    pub deny: Vec<ToolPattern>,
}

impl ToolPolicy {
    /// A policy with a mode and empty lists.
    pub fn new(mode: PolicyMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// Builds a policy from Claude-style pattern strings. A malformed pattern is an
    /// error, never skipped: a dropped `deny` entry would silently widen the policy.
    pub fn from_patterns<S: AsRef<str>>(
        mode: PolicyMode,
        allow: &[S],
        deny: &[S],
    ) -> Result<Self, ProviderError> {
        let parse = |list: &[S]| {
            list.iter()
                .map(|text| text.as_ref().parse::<ToolPattern>())
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(Self {
            mode,
            native_mode: None,
            allow: parse(allow)?,
            deny: parse(deny)?,
        })
    }

    /// Local decision for one call. `arg` is the command line for a command tool,
    /// the path for a file tool, `None` otherwise.
    ///
    /// Order: `deny` wins, then `allow`, then the mode. Reads and searches never
    /// ask; `plan_only` refuses everything else.
    pub fn decide(&self, tool: &str, arg: Option<&str>, category: ToolCategory) -> PolicyDecision {
        if self
            .deny
            .iter()
            .any(|pattern| pattern.matches_closed(tool, arg))
        {
            return PolicyDecision::Deny;
        }
        let read_only = matches!(category, ToolCategory::Read | ToolCategory::Search);
        if self.mode == PolicyMode::PlanOnly {
            // An allow entry cannot lift plan mode: planning changes nothing.
            return if read_only {
                PolicyDecision::Allow
            } else {
                PolicyDecision::Deny
            };
        }
        if self.allow.iter().any(|pattern| pattern.matches(tool, arg)) {
            return PolicyDecision::Allow;
        }
        match self.mode {
            PolicyMode::Trust => PolicyDecision::Allow,
            PolicyMode::AutoEdits if read_only || category == ToolCategory::Edit => {
                PolicyDecision::Allow
            },
            PolicyMode::Ask | PolicyMode::AutoEdits if read_only => PolicyDecision::Allow,
            PolicyMode::Ask | PolicyMode::AutoEdits => PolicyDecision::Ask,
            PolicyMode::PlanOnly => PolicyDecision::Deny,
        }
    }

    /// The policy a child session gets under `parent`: never more permissive (A17).
    ///
    /// Mode: the less permissive of the two. Deny: the union. Allow: the child's
    /// entries that a parent entry covers (all of them when the parent trusts).
    pub fn restrict(&self, parent: &ToolPolicy) -> ToolPolicy {
        let mode = self.mode.min(parent.mode);
        let mut deny = parent.deny.clone();
        for pattern in &self.deny {
            if !deny.contains(pattern) {
                deny.push(pattern.clone());
            }
        }
        let allow = self
            .allow
            .iter()
            .filter(|pattern| {
                parent.mode == PolicyMode::Trust
                    || parent.allow.iter().any(|wide| pattern.is_covered_by(wide))
            })
            .cloned()
            .collect();
        ToolPolicy {
            mode,
            // A native mode is only kept when the mode itself survived the restriction.
            native_mode: if mode == self.mode {
                self.native_mode.clone()
            } else {
                None
            },
            allow,
            deny,
        }
    }

    /// Whether this policy stays inside `ceiling`.
    pub fn is_within(&self, ceiling: &ToolPolicy) -> bool {
        self.mode <= ceiling.mode
            && ceiling
                .deny
                .iter()
                .all(|pattern| self.deny.contains(pattern))
            && (ceiling.mode == PolicyMode::Trust
                || self
                    .allow
                    .iter()
                    .all(|pattern| ceiling.allow.iter().any(|wide| pattern.is_covered_by(wide))))
    }
}

/// `*` wildcard match over the whole text.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while t < text.len() {
        if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some(star_at) = star {
            p = star_at + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// Whether glob `wide` matches everything glob `narrow` matches. Sound, not complete:
/// `narrow` is treated as a literal in which `*` only matches a `*` of `wide`.
fn glob_covers(wide: &str, narrow: &str) -> bool {
    if wide == narrow {
        return true;
    }
    // A `*` of `narrow` stands for any text, so it must face a `*` of `wide`.
    // Replace it with a character no pattern contains and require a match.
    const ANY: char = '\u{0}';
    let probe: String = narrow
        .chars()
        .map(|c| if c == '*' { ANY } else { c })
        .collect();
    if narrow.contains('*') {
        // Each run that contains the marker must be swallowed by a `*` of `wide`:
        // matching the probe is necessary; also require that `wide` does not match
        // the marker with a literal (it cannot: no pattern contains NUL).
        return glob_match(wide, &probe);
    }
    glob_match(wide, narrow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(text: &str) -> ToolPattern {
        text.parse().unwrap()
    }

    #[test]
    fn patterns_round_trip_and_normalise_the_legacy_colon_form() {
        assert_eq!(pattern("Read").to_string(), "Read");
        assert_eq!(pattern("Bash(git *)").to_string(), "Bash(git *)");
        assert_eq!(pattern("Bash(git:*)").to_string(), "Bash(git *)");
        assert_eq!(pattern(" mcp__po__* ").to_string(), "mcp__po__*");
        let json = serde_json::to_string(&pattern("Bash(npm run *)")).unwrap();
        assert_eq!(json, "\"Bash(npm run *)\"");
        assert_eq!(
            serde_json::from_str::<ToolPattern>(&json).unwrap(),
            pattern("Bash(npm run *)")
        );
    }

    #[test]
    fn malformed_patterns_are_errors_never_skipped() {
        for bad in [
            "", "Bash(", "Bash()", "Bash(git", "Ba sh", "(x)", "Bash(a)b",
        ] {
            assert!(
                bad.parse::<ToolPattern>().is_err(),
                "{bad:?} must be refused"
            );
        }
        assert!(ToolPolicy::from_patterns(PolicyMode::Ask, &["Read"], &["Bash("]).is_err());
        assert!(serde_json::from_str::<ToolPolicy>(r#"{"mode":"ask","deny":["Bash("]}"#).is_err());
        assert!(serde_json::from_str::<ToolPolicy>(r#"{"mode":"yolo"}"#).is_err());
    }

    #[test]
    fn policy_serialises_to_the_documented_shape() {
        let policy = ToolPolicy::from_patterns(
            PolicyMode::Ask,
            &["Read", "Bash(git *)"],
            &["mcp__po__admin"],
        )
        .unwrap();
        assert_eq!(
            serde_json::to_string(&policy).unwrap(),
            r#"{"mode":"ask","allow":["Read","Bash(git *)"],"deny":["mcp__po__admin"]}"#
        );
    }

    #[test]
    fn deny_wins_over_allow_and_over_trust() {
        let mut policy =
            ToolPolicy::from_patterns(PolicyMode::Trust, &["Bash(*)"], &["Bash(rm *)"]).unwrap();
        assert_eq!(
            policy.decide("Bash", Some("rm -rf /"), ToolCategory::Command),
            PolicyDecision::Deny
        );
        assert_eq!(
            policy.decide("Bash", Some("ls"), ToolCategory::Command),
            PolicyDecision::Allow
        );
        // Unknown argument against an argument-scoped deny: fail closed.
        assert_eq!(
            policy.decide("Bash", None, ToolCategory::Command),
            PolicyDecision::Deny
        );
        policy.deny = vec![pattern("mcp__po__*")];
        assert_eq!(
            policy.decide("mcp__po__plan", None, ToolCategory::Mcp),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn each_mode_decides_as_documented() {
        use PolicyDecision::{Allow, Ask, Deny};
        use ToolCategory::{Command, Edit, Mcp, Other, Read, Search};
        let table = [
            (PolicyMode::PlanOnly, [Allow, Allow, Deny, Deny, Deny, Deny]),
            (PolicyMode::Ask, [Allow, Allow, Ask, Ask, Ask, Ask]),
            (PolicyMode::AutoEdits, [Allow, Allow, Allow, Ask, Ask, Ask]),
            (
                PolicyMode::Trust,
                [Allow, Allow, Allow, Allow, Allow, Allow],
            ),
        ];
        for (mode, expected) in table {
            let policy = ToolPolicy::new(mode);
            for (category, want) in [Read, Search, Edit, Command, Mcp, Other]
                .into_iter()
                .zip(expected)
            {
                assert_eq!(
                    policy.decide("T", None, category),
                    want,
                    "{mode:?} {category:?}"
                );
            }
        }
    }

    #[test]
    fn an_allow_entry_skips_the_question_but_cannot_lift_plan_mode() {
        let ask = ToolPolicy::from_patterns(PolicyMode::Ask, &["Bash(git *)"], &[]).unwrap();
        assert_eq!(
            ask.decide("Bash", Some("git status"), ToolCategory::Command),
            PolicyDecision::Allow
        );
        assert_eq!(
            ask.decide("Bash", Some("cargo test"), ToolCategory::Command),
            PolicyDecision::Ask
        );
        // An allow entry with an argument never matches a call whose argument is unknown.
        assert_eq!(
            ask.decide("Bash", None, ToolCategory::Command),
            PolicyDecision::Ask
        );
        let plan = ToolPolicy::from_patterns(PolicyMode::PlanOnly, &["Bash(*)"], &[]).unwrap();
        assert_eq!(
            plan.decide("Bash", Some("git status"), ToolCategory::Command),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn restrict_is_monotone_and_idempotent() {
        let parent =
            ToolPolicy::from_patterns(PolicyMode::Ask, &["Bash(git *)", "Read"], &["Bash(rm *)"])
                .unwrap();
        let child = ToolPolicy {
            mode: PolicyMode::Trust,
            native_mode: Some("bypassPermissions".into()),
            allow: vec![
                pattern("Bash(git status)"),
                pattern("Bash(*)"),
                pattern("Read"),
                pattern("Write"),
            ],
            deny: vec![pattern("WebFetch")],
        };
        let restricted = child.restrict(&parent);
        assert_eq!(restricted.mode, PolicyMode::Ask);
        assert_eq!(
            restricted.native_mode, None,
            "a lowered mode drops the native mode"
        );
        assert_eq!(
            restricted.allow,
            vec![pattern("Bash(git status)"), pattern("Read")],
            "only entries the parent covers survive"
        );
        assert_eq!(
            restricted.deny,
            vec![pattern("Bash(rm *)"), pattern("WebFetch")]
        );
        assert!(restricted.is_within(&parent));
        assert!(!child.is_within(&parent));
        assert_eq!(restricted.restrict(&parent), restricted);

        // Whatever the child asks, the restricted policy never allows what the
        // parent would not allow.
        for (tool, arg, category) in [
            ("Bash", Some("rm -rf x"), ToolCategory::Command),
            ("Bash", Some("cargo test"), ToolCategory::Command),
            ("Write", Some("/etc/passwd"), ToolCategory::Edit),
            ("Bash", Some("git status"), ToolCategory::Command),
        ] {
            if restricted.decide(tool, arg, category) == PolicyDecision::Allow {
                assert_eq!(parent.decide(tool, arg, category), PolicyDecision::Allow);
            }
        }
    }

    #[test]
    fn a_trusting_parent_keeps_the_childs_allow_list_but_adds_its_deny() {
        let parent =
            ToolPolicy::from_patterns(PolicyMode::Trust, &[] as &[&str], &["Bash(sudo *)"])
                .unwrap();
        let child =
            ToolPolicy::from_patterns(PolicyMode::AutoEdits, &["Bash(make *)"], &[]).unwrap();
        let restricted = child.restrict(&parent);
        assert_eq!(restricted.mode, PolicyMode::AutoEdits);
        assert_eq!(restricted.allow, vec![pattern("Bash(make *)")]);
        assert_eq!(restricted.deny, vec![pattern("Bash(sudo *)")]);
        assert!(restricted.is_within(&parent));
    }

    #[test]
    fn glob_matching_and_coverage() {
        assert!(glob_match("git *", "git status"));
        assert!(glob_match("*", ""));
        assert!(glob_match("mcp__*__plan", "mcp__po__plan"));
        assert!(!glob_match("git *", "gitk"));
        assert!(!glob_match("git", "git status"));
        assert!(pattern("Bash(git status)").is_covered_by(&pattern("Bash(git *)")));
        assert!(pattern("Bash(git log *)").is_covered_by(&pattern("Bash(git *)")));
        assert!(!pattern("Bash(*)").is_covered_by(&pattern("Bash(git *)")));
        assert!(!pattern("Bash(g*)").is_covered_by(&pattern("Bash(git *)")));
        assert!(!pattern("Bash").is_covered_by(&pattern("Bash(git *)")));
        assert!(pattern("Bash(git *)").is_covered_by(&pattern("Bash")));
        assert!(pattern("mcp__po__plan").is_covered_by(&pattern("mcp__po__*")));
    }

    #[test]
    fn mode_order_is_the_permissiveness_order() {
        assert!(PolicyMode::PlanOnly < PolicyMode::Ask);
        assert!(PolicyMode::Ask < PolicyMode::AutoEdits);
        assert!(PolicyMode::AutoEdits < PolicyMode::Trust);
    }
}
