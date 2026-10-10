//! The tools the model is offered: the union of the tools of the session's MCP
//! servers, filtered by the session's policy (decisions A8 and A35).
//!
//! # Names
//!
//! A tool is offered to the model as `mcp__<server>__<tool>` (the `canonical`
//! name of the contract, §4, and the name the policy patterns match). Characters
//! outside `[A-Za-z0-9_-]` become `_` (OpenAI function names allow no others) and
//! a name longer than 64 characters is cut and given a hash suffix. Two tools that
//! end up with the same name refuse the session (`invalid_request`).
//!
//! # Canonical names (N24)
//!
//! The tools of the `nexus` server (`nexus-tools`: `Read`, `Write`, `Edit`, `NotebookEdit`,
//! `Glob`, `Grep`, `Bash`, `Monitor`, `TaskStop`, `WebFetch`, `WebSearch`) also have a
//! **canonical name**: their own, the one Claude Code uses. A policy written as
//! `Read(.env*)` or `Bash(git *)` applies to `mcp__nexus__Read` / `mcp__nexus__Bash`, with the
//! pattern's argument matched against the call's *primary argument* (the command line, the
//! path, the URL's domain) and not against the JSON of the whole input; a pattern written with
//! the full name (`mcp__nexus__Bash`) applies too. Their category follows what they do (read,
//! edit, search, command, web), so `auto_edits` lets edits through and `ask` still asks for a
//! command or a web fetch. Tools of other servers keep their old treatment.
//!
//! # Exposure rule
//!
//! A tool is **offered** unless one of these holds:
//!
//! 1. a `deny` pattern without argument matches its name;
//! 2. the mode is `plan_only` and the tool is not read-only (MCP annotation
//!    `readOnlyHint: true`) — an `allow` entry cannot lift plan mode;
//! 3. strict exposure (the default) and the policy has a non-empty `allow` list
//!    that matches none of its names: **an allow-list is an exposure list**, a
//!    tool it does not name is not offered at all.
//!
//! With an empty `allow` every other tool is offered and the mode decides, call
//! by call, between running it, asking, and refusing (`ToolPolicy::decide`). A
//! call to a tool that was not offered is answered by an error `tool_result`
//! (`unknown tool`), without any permission request.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::agent::{PolicyMode, ProviderError, ToolCategory, ToolPolicy};
use crate::model::ToolSpec;

/// Longest function name OpenAI-compatible servers accept.
const MAX_NAME: usize = 64;

/// The name under which the harness attaches `nexus-tools`.
pub const NEXUS_TOOLS_SERVER: &str = "nexus";

/// Every tool `nexus-tools` can serve, by canonical name, with whether it is read-only (its
/// `readOnlyHint`). The harness bounds the server it launches with it before the server runs
/// (`--tools`, N27); `nexus-tools/tests/session_bound.rs` checks it against the real server.
pub const NEXUS_TOOLS_CATALOG: &[(&str, bool)] = &[
    ("Read", true),
    ("Write", false),
    ("Edit", false),
    ("NotebookEdit", false),
    ("Glob", true),
    ("Grep", true),
    ("Bash", false),
    ("Monitor", false),
    ("TaskStop", false),
    ("WebFetch", true),
    ("WebSearch", true),
];

/// The `nexus-tools` tools a session may ever be offered, by canonical name: what the harness
/// passes as `--tools` when it launches the session's server (N27), so a tool the policy does not
/// expose is not even in the process.
///
/// The exposure rule is applied to each tool of [`NEXUS_TOOLS_CATALOG`] with one widening: the
/// mode can be raised during the session (`set_policy_mode`) up to the ceiling, so the plan-mode
/// rule only bounds the process when the session can never leave `plan_only`. The `allow` and
/// `deny` lists cannot change once the session is open. One addition: `TaskStop` is in the
/// process whenever `Bash` or `Monitor` is, since the harness stops a background task with it
/// (`cancel_tools(task)`); the model is offered it only if the policy exposes it.
pub fn nexus_tools_bound(
    policy: &ToolPolicy,
    ceiling: Option<&ToolPolicy>,
    strict: bool,
) -> Vec<&'static str> {
    let can_leave_plan = ceiling.is_none_or(|ceiling| ceiling.mode > PolicyMode::PlanOnly);
    let reach = if policy.mode == PolicyMode::PlanOnly && can_leave_plan {
        let mut wider = policy.clone();
        wider.mode = PolicyMode::Ask;
        std::borrow::Cow::Owned(wider)
    } else {
        std::borrow::Cow::Borrowed(policy)
    };
    let exposed = |tool: &str, read_only: bool| {
        let entry = ToolEntry::mcp(
            NEXUS_TOOLS_SERVER,
            tool,
            String::new(),
            Value::Null,
            read_only,
        );
        ToolRegistry::is_exposed(&entry, &reach, strict)
    };
    // A process that can start background tasks also holds `TaskStop`: it is how the harness
    // stops one (`cancel_tools(task)`). The model is still offered it only if the policy exposes
    // it (the exposure rule is applied again, call by call).
    let starts_tasks = exposed("Bash", false) || exposed("Monitor", false);
    NEXUS_TOOLS_CATALOG
        .iter()
        .filter(|(tool, read_only)| {
            exposed(tool, *read_only) || (*tool == "TaskStop" && starts_tasks)
        })
        .map(|(tool, _)| *tool)
        .collect()
}

/// The category of a canonical `nexus-tools` tool, and the input fields that make up its
/// primary argument (first present wins), or `None` for a tool that is not one of theirs.
fn canonical_profile(tool: &str) -> Option<(ToolCategory, &'static [&'static str])> {
    Some(match tool {
        "Read" | "Write" | "Edit" => (
            if tool == "Read" {
                ToolCategory::Read
            } else {
                ToolCategory::Edit
            },
            &["file_path"],
        ),
        "NotebookEdit" => (ToolCategory::Edit, &["notebook_path"]),
        "Glob" | "Grep" => (ToolCategory::Search, &["pattern"]),
        "Bash" | "Monitor" => (ToolCategory::Command, &["command"]),
        "TaskStop" => (ToolCategory::Command, &["task_id", "shell_id"]),
        "WebFetch" => (ToolCategory::Web, &["url"]),
        "WebSearch" => (ToolCategory::Web, &["query"]),
        _ => return None,
    })
}

/// One tool of one MCP server, as offered to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolEntry {
    /// `mcp__<server>__<tool>`: what the model calls, what policies match.
    pub name: String,
    /// Name of the MCP server in `SessionSpec::mcp_servers`.
    pub server: String,
    /// The tool's own name, as the server knows it.
    pub tool: String,
    /// Description given to the model.
    pub description: String,
    /// JSON schema of the arguments.
    pub schema: serde_json::Value,
    /// The server declared `readOnlyHint: true`.
    pub read_only: bool,
    /// The canonical name (`Read`, `Bash`…) when this is a `nexus-tools` tool: what policy
    /// patterns written the Claude Code way match.
    pub canonical: Option<String>,
    /// What the tool does, for the policy: read, edit, search, command, web; `Read` for a
    /// read-only tool of another server and `Mcp` for the rest.
    pub category: ToolCategory,
}

impl ToolEntry {
    /// The entry of `tool` served by `server`.
    pub fn mcp(
        server: &str,
        tool: &str,
        description: String,
        schema: Value,
        read_only: bool,
    ) -> Self {
        let profile = (server == NEXUS_TOOLS_SERVER)
            .then(|| canonical_profile(tool))
            .flatten();
        // The optional browser (N23): classified by name, since its server does not annotate.
        let browser = (server == super::browser::BROWSER_SERVER && tool.starts_with("browser_"))
            .then(|| super::browser::profile(tool));
        let read_only = read_only || browser.is_some_and(|(_, reads)| reads);
        Self {
            name: exposed_name(server, tool),
            server: server.to_owned(),
            tool: tool.to_owned(),
            description,
            schema,
            read_only,
            canonical: profile.map(|_| tool.to_owned()),
            category: match (profile, browser) {
                (Some((category, _)), _) => category,
                (None, Some((category, _))) => category,
                (None, None) if read_only => ToolCategory::Read,
                (None, None) => ToolCategory::Mcp,
            },
        }
    }

    /// Every name a policy pattern may match this tool by: the offered name, then the canonical.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.name.as_str()).chain(self.canonical.as_deref())
    }

    /// What a pattern's argument is matched against: the primary argument of a canonical
    /// tool (the command, the path, the domain of the URL), the whole input for the others.
    pub fn primary_argument(&self, input: &Value) -> Option<String> {
        let Some(canonical) = self.canonical.as_deref() else {
            return Some(input.to_string());
        };
        let (_, fields) = canonical_profile(canonical)?;
        let text = fields
            .iter()
            .find_map(|field| input.get(*field).and_then(Value::as_str))?;
        if canonical == "WebFetch" {
            // `WebFetch(domain:example.com)`, as in Claude Code.
            let host = url::Url::parse(text.trim())
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))?;
            // `evil.com.` is `evil.com` (the root dot), and DNS ignores case.
            let host = host.trim_end_matches('.').to_lowercase();
            return Some(format!("domain:{host}"));
        }
        Some(text.to_owned())
    }
}

fn sanitize(segment: &str) -> String {
    segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn stable_hash(text: &str) -> u32 {
    // FNV-1a: stable across runs, unlike the std hasher.
    text.bytes().fold(0x811c_9dc5u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

/// `mcp__<server>__<tool>`, sanitised and bounded.
pub fn exposed_name(server: &str, tool: &str) -> String {
    let name = format!("mcp__{}__{}", sanitize(server), sanitize(tool));
    if name.len() <= MAX_NAME {
        return name;
    }
    let suffix = format!("_{:08x}", stable_hash(&format!("{server}\u{0}{tool}")));
    format!("{}{suffix}", &name[..MAX_NAME - suffix.len()])
}

/// The tools of a session.
#[derive(Debug, Clone, Default)]
pub struct ToolRegistry {
    entries: BTreeMap<String, ToolEntry>,
}

impl ToolRegistry {
    /// An empty registry (a session without MCP server).
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a tool; two tools with the same offered name are refused.
    pub fn insert(&mut self, entry: ToolEntry) -> Result<(), ProviderError> {
        if self.entries.contains_key(&entry.name) {
            return Err(ProviderError::invalid(format!(
                "two MCP tools are offered under the same name `{}`",
                entry.name
            )));
        }
        self.entries.insert(entry.name.clone(), entry);
        Ok(())
    }

    /// The tool offered under `name`.
    pub fn get(&self, name: &str) -> Option<&ToolEntry> {
        self.entries.get(name)
    }

    /// Number of tools, offered or not.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the registry holds no tool.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `entry` is offered under `policy` (module documentation).
    pub fn is_exposed(entry: &ToolEntry, policy: &ToolPolicy, strict: bool) -> bool {
        if policy.deny.iter().any(|pattern| {
            pattern.arg.is_none() && entry.names().any(|name| pattern.matches(name, None))
        }) {
            return false;
        }
        if policy.mode == PolicyMode::PlanOnly && !entry.read_only {
            return false;
        }
        if strict
            && !policy.allow.is_empty()
            && !policy
                .allow
                .iter()
                .any(|pattern| entry.names().any(|name| pattern.matches_closed(name, None)))
        {
            return false;
        }
        true
    }

    /// The tools offered under `policy`, in name order.
    pub fn exposed(&self, policy: &ToolPolicy, strict: bool) -> Vec<&ToolEntry> {
        self.entries
            .values()
            .filter(|entry| Self::is_exposed(entry, policy, strict))
            .collect()
    }

    /// The offered tools as the endpoint wants them.
    pub fn specs(&self, policy: &ToolPolicy, strict: bool) -> Vec<ToolSpec> {
        self.exposed(policy, strict)
            .into_iter()
            .map(|entry| ToolSpec {
                name: entry.name.clone(),
                description: entry.description.clone(),
                parameters: entry.schema.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ToolCategory, ToolPattern};
    use serde_json::json;

    fn entry(server: &str, tool: &str, read_only: bool) -> ToolEntry {
        ToolEntry::mcp(
            server,
            tool,
            String::new(),
            json!({"type": "object"}),
            read_only,
        )
    }

    fn registry() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.insert(entry("fake", "echo", true)).unwrap();
        registry.insert(entry("fake", "write", false)).unwrap();
        registry
    }

    fn names(registry: &ToolRegistry, policy: &ToolPolicy, strict: bool) -> Vec<String> {
        registry
            .exposed(policy, strict)
            .into_iter()
            .map(|e| e.name.clone())
            .collect()
    }

    fn policy(mode: PolicyMode, allow: &[&str], deny: &[&str]) -> ToolPolicy {
        ToolPolicy::from_patterns(mode, allow, deny).unwrap()
    }

    #[test]
    fn names_follow_the_canonical_form_and_stay_valid_function_names() {
        assert_eq!(exposed_name("po", "task"), "mcp__po__task");
        assert_eq!(exposed_name("my.srv", "do it"), "mcp__my_srv__do_it");
        let long = exposed_name("s", &"x".repeat(100));
        assert_eq!(long.len(), MAX_NAME);
        assert_eq!(long, exposed_name("s", &"x".repeat(100)));
        assert_ne!(long, exposed_name("s", &format!("{}y", "x".repeat(99))));
    }

    #[test]
    fn two_tools_with_one_name_are_refused() {
        let mut registry = ToolRegistry::new();
        registry.insert(entry("a.b", "t", false)).unwrap();
        let clash = registry.insert(entry("a_b", "t", false)).unwrap_err();
        assert_eq!(clash.kind(), "invalid_request");
    }

    #[test]
    fn an_empty_allow_list_offers_everything_not_denied() {
        let registry = registry();
        assert_eq!(
            names(&registry, &policy(PolicyMode::Ask, &[], &[]), true).len(),
            2
        );
        assert_eq!(
            names(
                &registry,
                &policy(PolicyMode::Ask, &[], &["mcp__fake__write"]),
                true
            ),
            vec!["mcp__fake__echo"]
        );
    }

    #[test]
    fn an_allow_list_is_an_exposure_list_in_strict_mode_only() {
        let registry = registry();
        let allow_echo = policy(PolicyMode::Ask, &["mcp__fake__echo"], &[]);
        assert_eq!(names(&registry, &allow_echo, true), vec!["mcp__fake__echo"]);
        assert_eq!(names(&registry, &allow_echo, false).len(), 2);
    }

    #[test]
    fn plan_only_offers_read_only_tools_whatever_allow_says() {
        let registry = registry();
        let plan = policy(PolicyMode::PlanOnly, &["mcp__fake__*"], &[]);
        assert_eq!(names(&registry, &plan, true), vec!["mcp__fake__echo"]);
    }

    #[test]
    fn a_deny_with_an_argument_does_not_hide_the_tool() {
        let registry = registry();
        let mut policy = ToolPolicy::new(PolicyMode::Ask);
        policy.deny.push(ToolPattern {
            tool: "mcp__fake__write".into(),
            arg: Some("*rm*".into()),
        });
        assert_eq!(names(&registry, &policy, true).len(), 2);
    }

    #[test]
    fn a_nexus_tool_has_a_canonical_name_a_category_and_a_primary_argument() {
        let bash = entry("nexus", "Bash", false);
        assert_eq!(bash.canonical.as_deref(), Some("Bash"));
        assert_eq!(bash.category, ToolCategory::Command);
        assert_eq!(bash.name, "mcp__nexus__Bash");
        assert_eq!(
            bash.names().collect::<Vec<_>>(),
            ["mcp__nexus__Bash", "Bash"]
        );
        assert_eq!(
            bash.primary_argument(&json!({"command": "git status"}))
                .as_deref(),
            Some("git status")
        );
        for (tool, category) in [
            ("Read", ToolCategory::Read),
            ("Write", ToolCategory::Edit),
            ("Edit", ToolCategory::Edit),
            ("NotebookEdit", ToolCategory::Edit),
            ("Glob", ToolCategory::Search),
            ("Grep", ToolCategory::Search),
            ("Monitor", ToolCategory::Command),
            ("TaskStop", ToolCategory::Command),
            ("WebFetch", ToolCategory::Web),
            ("WebSearch", ToolCategory::Web),
        ] {
            assert_eq!(entry("nexus", tool, false).category, category, "{tool}");
        }
        let fetch = entry("nexus", "WebFetch", true);
        assert_eq!(
            fetch
                .primary_argument(&json!({"url": "https://docs.rs:443/x?q=1"}))
                .as_deref(),
            Some("domain:docs.rs")
        );
        assert_eq!(fetch.primary_argument(&json!({"url": "nope"})), None);
    }

    #[test]
    fn other_servers_and_unknown_tools_get_no_canonical_name() {
        assert_eq!(entry("other", "Bash", false).canonical, None);
        assert_eq!(entry("nexus", "Mystery", false).canonical, None);
        // As before: the whole input is the argument; read-only means read, else generic MCP.
        let other = entry("other", "Bash", true);
        assert_eq!(other.category, ToolCategory::Read);
        assert_eq!(entry("other", "Bash", false).category, ToolCategory::Mcp);
        assert_eq!(
            other.primary_argument(&json!({"a": 1})).as_deref(),
            Some("{\"a\":1}")
        );
    }

    #[test]
    fn the_nexus_tools_bound_is_what_the_policy_can_expose() {
        let all: Vec<&str> = NEXUS_TOOLS_CATALOG.iter().map(|(n, _)| *n).collect();
        let bound = |p: &ToolPolicy, ceiling: Option<&ToolPolicy>, strict| {
            nexus_tools_bound(p, ceiling, strict)
        };
        assert_eq!(bound(&policy(PolicyMode::Ask, &[], &[]), None, true), all);
        // An allow list is an exposure list, by canonical or full name, with or without argument.
        assert_eq!(
            bound(
                &policy(
                    PolicyMode::Ask,
                    &["Read", "mcp__nexus__Grep", "Bash(git *)"],
                    &[]
                ),
                None,
                true
            ),
            // `TaskStop` comes with `Bash`: the harness's way to stop what it starts.
            ["Read", "Grep", "Bash", "TaskStop"]
        );
        assert_eq!(
            bound(&policy(PolicyMode::Ask, &["Read"], &[]), None, true),
            ["Read"]
        );
        // ...but not when exposure is not strict.
        assert_eq!(
            bound(&policy(PolicyMode::Ask, &["Read"], &[]), None, false),
            all
        );
        // A deny without argument removes the tool; one with an argument does not.
        let no_bash = bound(&policy(PolicyMode::Ask, &[], &["Bash"]), None, true);
        assert!(!no_bash.contains(&"Bash") && no_bash.contains(&"Write"));
        let rm_only = bound(&policy(PolicyMode::Ask, &[], &["Bash(rm *)"]), None, true);
        assert!(rm_only.contains(&"Bash"));
        // Other servers' names do not leak in.
        assert!(
            bound(
                &policy(PolicyMode::Ask, &["mcp__po__task"], &[]),
                None,
                true
            )
            .is_empty()
        );
    }

    #[test]
    fn plan_mode_bounds_the_process_only_when_the_session_cannot_leave_it() {
        let plan = policy(PolicyMode::PlanOnly, &[], &[]);
        let read_only: Vec<&str> = NEXUS_TOOLS_CATALOG
            .iter()
            .filter(|(_, ro)| *ro)
            .map(|(n, _)| *n)
            .collect();
        assert_eq!(read_only, ["Read", "Glob", "Grep", "WebFetch", "WebSearch"]);
        // No ceiling: `set_policy_mode(ask)` may come, the edits must be there then.
        assert_eq!(
            nexus_tools_bound(&plan, None, true).len(),
            NEXUS_TOOLS_CATALOG.len()
        );
        let ask = ToolPolicy::new(PolicyMode::Ask);
        assert_eq!(
            nexus_tools_bound(&plan, Some(&ask), true).len(),
            NEXUS_TOOLS_CATALOG.len()
        );
        // A plan-only ceiling: never anything but reads.
        let ceiling = ToolPolicy::new(PolicyMode::PlanOnly);
        assert_eq!(nexus_tools_bound(&plan, Some(&ceiling), true), read_only);
    }

    #[test]
    fn exposure_patterns_can_use_the_canonical_name() {
        let mut registry = ToolRegistry::new();
        for tool in ["Read", "Grep", "Bash", "WebFetch"] {
            registry
                .insert(entry("nexus", tool, tool == "Read"))
                .unwrap();
        }
        registry.insert(entry("po", "task", false)).unwrap();
        // An allow list is an exposure list: `Read` and `Grep` expose the nexus tools of that name.
        let allow = policy(PolicyMode::Ask, &["Read", "Grep"], &[]);
        assert_eq!(
            names(&registry, &allow, true),
            vec!["mcp__nexus__Grep", "mcp__nexus__Read"]
        );
        // A deny without argument hides the tool, by either name.
        let deny = policy(PolicyMode::Ask, &[], &["Bash", "mcp__nexus__WebFetch"]);
        assert_eq!(
            names(&registry, &deny, true),
            vec!["mcp__nexus__Grep", "mcp__nexus__Read", "mcp__po__task"]
        );
    }
}
