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

use crate::agent::{PolicyMode, ProviderError, ToolPolicy};
use crate::model::ToolSpec;

/// Longest function name OpenAI-compatible servers accept.
const MAX_NAME: usize = 64;

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
        if policy
            .deny
            .iter()
            .any(|pattern| pattern.arg.is_none() && pattern.matches(&entry.name, None))
        {
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
                .any(|pattern| pattern.matches_closed(&entry.name, None))
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
    use crate::agent::ToolPattern;
    use serde_json::json;

    fn entry(server: &str, tool: &str, read_only: bool) -> ToolEntry {
        ToolEntry {
            name: exposed_name(server, tool),
            server: server.into(),
            tool: tool.into(),
            description: String::new(),
            schema: json!({"type": "object"}),
            read_only,
        }
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
}
