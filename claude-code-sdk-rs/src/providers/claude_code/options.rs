//! Instance configuration of the Claude Code provider, and the translation of a
//! neutral [`SessionSpec`] into [`ClaudeCodeOptions`] (contract §15, last
//! paragraph: the same options the orchestrator's `build_options` produced).

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::Value;

use super::policy_map::translate_policy;
use crate::agent::{
    Capabilities, ContextWindow, CostBasis, HookSupport, McpServerSpec, ModelInfo, PermissionScope,
    ProviderError, ProviderKind, SandboxLevel, SessionSpec, SubagentSupport, SystemPromptMode,
};
use crate::transport::EnvPolicy;
use crate::types::{ClaudeCodeOptions, McpServerConfig, SettingSource};

/// Tool name the CLI is told to route permission prompts to: the control
/// protocol on stdio.
pub const PERMISSION_PROMPT_TOOL: &str = "stdio";

/// Size of the SDK's channels for one CLI process.
pub const CLI_CHANNEL_BUFFER_SIZE: usize = 8192;

/// Keys of the `claude_code` extension this adapter applies. Any other key is
/// reported by [`ignored_extension_keys`], never dropped silently.
pub const APPLIED_EXTENSION_KEYS: [&str; 5] = [
    "cli_path",
    "fallback_model",
    "permission_prompt_tool",
    "extra_args",
    "setting_sources",
];

/// Configuration of one Claude Code provider instance.
///
/// `#[non_exhaustive]`: start from [`ClaudeCodeConfig::default`] and set fields.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ClaudeCodeConfig {
    /// Identifier of the instance (registry key). Default `claude-code`.
    pub id: String,
    /// Path of the CLI; `None` looks it up (`find_claude_cli`). A session's
    /// `claude_code.cli_path` extension wins over it.
    pub cli_path: Option<PathBuf>,
    /// Environment handed to the CLI. Default [`EnvPolicy::claude_code`]: an
    /// allowlist, so the CLI does not inherit the host's secrets.
    pub env_policy: EnvPolicy,
    /// Write the MCP configuration to a private file instead of the command
    /// line. Default `true`.
    pub mcp_config_via_file: bool,
    /// Where `done.cost` comes from. Default `reported`; `subscription` for an
    /// instance logged in through OAuth, `unknown` when the CLI talks to a model
    /// it cannot price (the amount is then withheld).
    pub cost_basis: CostBasis,
    /// Context window declared in the capabilities when a model has none of its
    /// own in [`ClaudeCodeConfig::models`]. `None`: unknown, never a default.
    pub context_window: Option<ContextWindow>,
    /// Model of a session that names none.
    pub default_model: Option<String>,
    /// Catalogue answered by `catalog()`.
    pub models: Vec<ModelInfo>,
}

impl Default for ClaudeCodeConfig {
    fn default() -> Self {
        Self {
            id: "claude-code".to_owned(),
            cli_path: None,
            env_policy: EnvPolicy::claude_code(),
            mcp_config_via_file: true,
            cost_basis: CostBasis::Reported,
            context_window: None,
            default_model: None,
            models: Vec::new(),
        }
    }
}

impl ClaudeCodeConfig {
    /// Capabilities of this instance for a model (`None`: the default model):
    /// the "Claude Code" column of contract §5.
    ///
    /// Three values differ from that column **in this slice**, each with its
    /// fallback verified by the conformance suite: `tool_cancel` and
    /// `background_tasks` are `false` until the adapter tracks the CLI's
    /// descendants, and `images` is `false` in v1 (A12). `secret_isolation` is
    /// declared only when the instance really isolates: an allowlist
    /// environment **and** the MCP configuration off the command line.
    pub fn capabilities(&self, model: Option<&str>) -> Capabilities {
        let model = model.or(self.default_model.as_deref());
        let mut capabilities = Capabilities::none();
        capabilities.interactive_permissions = true;
        capabilities.permission_scopes = vec![
            PermissionScope::Once,
            PermissionScope::Session,
            PermissionScope::Always,
        ];
        capabilities.sandbox = SandboxLevel::None;
        capabilities.secret_isolation = self.env_policy.is_isolated() && self.mcp_config_via_file;
        capabilities.per_session_mcp = true;
        capabilities.hooks = HookSupport::InProtocol;
        capabilities.subagents = SubagentSupport::Nested;
        capabilities.compaction_signal = true;
        capabilities.thinking = true;
        capabilities.images = false;
        capabilities.tools = true;
        capabilities.context_window = model
            .and_then(|model| self.models.iter().find(|info| info.id == model))
            .and_then(|info| info.context_window)
            .or(self.context_window);
        capabilities.set_model_live = true;
        capabilities.native_question = true;
        capabilities.tool_cancel = false;
        capabilities.background_tasks = false;
        capabilities.resume = true;
        capabilities.cost = self.cost_basis;
        capabilities
    }
}

fn claude_extension(spec: &SessionSpec) -> Option<&serde_json::Map<String, Value>> {
    spec.extension(ProviderKind::ClaudeCode.as_str())
        .and_then(Value::as_object)
}

/// Keys of the session's `claude_code` extension this adapter does **not**
/// apply (`betas`, `append_system_prompt_preset`, or an unknown key). The
/// session reports them with a `provider_notice { kind: "extension_ignored" }`.
pub fn ignored_extension_keys(spec: &SessionSpec) -> Vec<String> {
    claude_extension(spec)
        .map(|extension| {
            extension
                .keys()
                .filter(|key| !APPLIED_EXTENSION_KEYS.contains(&key.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn mcp_config(server: &McpServerSpec) -> Result<McpServerConfig, ProviderError> {
    let optional = |map: &std::collections::BTreeMap<String, String>| {
        (!map.is_empty()).then(|| map.clone().into_iter().collect::<HashMap<_, _>>())
    };
    Ok(match server {
        McpServerSpec::Stdio { command, args, env } => McpServerConfig::Stdio {
            command: command.clone(),
            args: (!args.is_empty()).then(|| args.clone()),
            env: optional(env),
        },
        McpServerSpec::Http { url, headers } => McpServerConfig::Http {
            url: url.clone(),
            headers: optional(headers),
        },
        McpServerSpec::Sse { url, headers } => McpServerConfig::Sse {
            url: url.clone(),
            headers: optional(headers),
        },
        // `McpServerSpec` is `#[non_exhaustive]`: a transport added to the
        // contract is refused here until this adapter learns it.
        #[allow(unreachable_patterns)]
        _ => return Err(ProviderError::unsupported("mcp_transport")),
    })
}

/// Translates a session spec into the options of the CLI.
///
/// Identical to what the orchestrator built by hand: `permission_prompt_tool_name`
/// `"stdio"`, a channel buffer of 8192, partial messages when `spec.deltas`,
/// allow and deny lists left out when empty. `resume` is the CLI session
/// identifier of a resume token.
///
/// Not carried: `spec.hooks` (attached by the session, which owns the callbacks),
/// `spec.lineage`, and the limits other than `max_cost_usd` (`--max-budget-usd`),
/// which this adapter does not enforce. A native mode outside the SDK's
/// `PermissionMode` (`auto`, `dontAsk`, `manual`) starts in the closest of the
/// four modes; see [`NativePolicy::native_override`](super::policy_map::NativePolicy::native_override).
pub fn build_options(
    config: &ClaudeCodeConfig,
    spec: &SessionSpec,
    resume: Option<&str>,
) -> Result<ClaudeCodeOptions, ProviderError> {
    let policy = translate_policy(&spec.policy);
    let mut builder = ClaudeCodeOptions::builder()
        .cwd(spec.cwd.clone())
        .permission_mode(policy.permission_mode)
        .include_partial_messages(spec.deltas)
        .permission_prompt_tool_name(PERMISSION_PROMPT_TOOL)
        .cli_channel_buffer_size(CLI_CHANNEL_BUFFER_SIZE);
    // The same builder calls as the orchestrator's `build_options`.
    if let Some(prompt) = &spec.system_prompt {
        builder = match prompt.mode {
            SystemPromptMode::Replace => builder.system_prompt(prompt.text.clone()),
            SystemPromptMode::Append => builder.append_system_prompt(prompt.text.clone()),
        };
    }
    let mut options = builder.build();

    options.model = spec.model.clone().or_else(|| config.default_model.clone());
    if let Some(turns) = spec.max_turns {
        options.max_turns = Some(i32::try_from(turns).unwrap_or(i32::MAX));
    }
    for (name, server) in &spec.mcp_servers {
        options
            .mcp_servers
            .insert(name.clone(), mcp_config(server)?);
    }
    // Empty lists are left as the builder made them: nothing on the command line.
    if !policy.allowed_tools.is_empty() {
        options.allowed_tools = policy.allowed_tools;
    }
    if !policy.disallowed_tools.is_empty() {
        options.disallowed_tools = policy.disallowed_tools;
    }
    options.resume = resume.map(str::to_owned);
    options.add_dirs = spec.extra_dirs.clone();
    options.env = spec.env.set.clone().into_iter().collect();
    options.env_policy = config
        .env_policy
        .clone()
        .with_inherited(spec.env.inherit.iter().cloned());
    options.mcp_config_via_file = config.mcp_config_via_file;
    options.cli_path = config.cli_path.clone();
    options.max_budget_usd = spec.limits.max_cost_usd;

    if let Some(extension) = claude_extension(spec) {
        let text = |key: &str| extension.get(key).and_then(Value::as_str);
        if let Some(path) = text("cli_path") {
            options.cli_path = Some(PathBuf::from(path));
        }
        if let Some(model) = text("fallback_model") {
            options.fallback_model = Some(model.to_owned());
        }
        if let Some(tool) = text("permission_prompt_tool") {
            options.permission_prompt_tool_name = Some(tool.to_owned());
        }
        if let Some(args) = extension.get("extra_args") {
            let args = args.as_object().ok_or_else(|| {
                ProviderError::invalid("claude_code.extra_args must be an object")
            })?;
            for (flag, value) in args {
                let value = match value {
                    Value::Null => None,
                    Value::String(value) => Some(value.clone()),
                    _ => {
                        return Err(ProviderError::invalid(format!(
                            "claude_code.extra_args.{flag} must be a string or null"
                        )));
                    },
                };
                options.extra_args.insert(flag.clone(), value);
            }
        }
        if let Some(sources) = extension.get("setting_sources") {
            let sources: Vec<SettingSource> =
                serde_json::from_value(sources.clone()).map_err(|_| {
                    ProviderError::invalid(
                        "claude_code.setting_sources must be a list of user, project, local",
                    )
                })?;
            options.setting_sources = Some(sources);
        }
    }
    Ok(options)
}

#[cfg(test)]
// The tests read `system_prompt` / `append_system_prompt`, the deprecated pair
// the builder still fills and `build_command` still renders.
#[allow(deprecated, clippy::field_reassign_with_default)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::agent::{
        ContextWindowSource, PolicyMode, SessionLimits, SystemPromptSpec, ToolPolicy,
    };
    use crate::types::PermissionMode;

    #[test]
    fn the_default_instance_isolates_and_reports_its_cost() {
        let config = ClaudeCodeConfig::default();
        assert_eq!(config.id, "claude-code");
        assert_eq!(config.env_policy, EnvPolicy::claude_code());
        assert!(config.mcp_config_via_file);
        assert_eq!(config.cost_basis, CostBasis::Reported);
        assert_eq!(config.context_window, None);
    }

    #[test]
    fn capabilities_are_the_claude_code_column_of_this_slice() {
        let capabilities = ClaudeCodeConfig::default().capabilities(None);
        assert_eq!(
            serde_json::to_value(&capabilities).unwrap(),
            json!({
                "interactive_permissions": true,
                "permission_scopes": ["once", "session", "always"],
                "sandbox": "none",
                "secret_isolation": true,
                "per_session_mcp": true,
                "hooks": "in_protocol",
                "subagents": "nested",
                "compaction_signal": true,
                "thinking": true,
                "images": false,
                "tools": true,
                "set_model_live": true,
                "native_question": true,
                "tool_cancel": false,
                "background_tasks": false,
                "resume": true,
                "cost": "reported",
            })
        );
    }

    #[test]
    fn secret_isolation_is_declared_only_when_it_is_real() {
        let mut config = ClaudeCodeConfig::default();
        config.env_policy = EnvPolicy::InheritAll;
        assert!(!config.capabilities(None).secret_isolation);
        let mut config = ClaudeCodeConfig::default();
        config.mcp_config_via_file = false;
        assert!(!config.capabilities(None).secret_isolation);
    }

    #[test]
    fn the_context_window_is_the_models_then_the_instances() {
        let window = |value| ContextWindow {
            value,
            source: ContextWindowSource::Configured,
        };
        let mut config = ClaudeCodeConfig::default();
        config.context_window = Some(window(200_000));
        let mut big = ModelInfo::new("big");
        big.context_window = Some(window(1_000_000));
        config.models = vec![big, ModelInfo::new("plain")];
        config.default_model = Some("big".to_owned());
        assert_eq!(
            config.capabilities(None).context_window,
            Some(window(1_000_000))
        );
        assert_eq!(
            config.capabilities(Some("plain")).context_window,
            Some(window(200_000))
        );
        assert_eq!(
            config.capabilities(Some("unlisted")).context_window,
            Some(window(200_000))
        );
        config.cost_basis = CostBasis::Subscription;
        assert_eq!(config.capabilities(None).cost, CostBasis::Subscription);
    }

    #[test]
    fn a_bare_spec_gives_the_constants_of_build_options_and_nothing_else() {
        let options = build_options(
            &ClaudeCodeConfig::default(),
            &SessionSpec::new("/work"),
            None,
        )
        .unwrap();
        assert_eq!(options.cwd, Some(PathBuf::from("/work")));
        assert_eq!(options.permission_mode, PermissionMode::Default);
        assert_eq!(
            options.permission_prompt_tool_name.as_deref(),
            Some("stdio")
        );
        assert_eq!(options.cli_channel_buffer_size, Some(8192));
        assert!(options.include_partial_messages, "deltas default to true");
        assert_eq!(options.env_policy, EnvPolicy::claude_code());
        assert!(options.mcp_config_via_file);
        let untouched = ClaudeCodeOptions::default();
        assert_eq!(options.allowed_tools, untouched.allowed_tools);
        assert_eq!(options.disallowed_tools, untouched.disallowed_tools);
        assert_eq!(options.model, None);
        assert_eq!(options.max_turns, None);
        assert_eq!(options.resume, None);
        assert_eq!(options.system_prompt, None);
        assert_eq!(options.append_system_prompt, None);
        assert!(options.mcp_servers.is_empty());
        assert!(options.add_dirs.is_empty());
        assert!(options.env.is_empty());
        assert_eq!(options.cli_path, None);
        assert_eq!(options.max_budget_usd, None);
        assert!(options.hooks.is_none());
    }

    #[test]
    fn every_field_of_the_spec_reaches_the_options() {
        let mut config = ClaudeCodeConfig::default();
        config.cli_path = Some(PathBuf::from("/opt/claude"));
        config.default_model = Some("instance-default".to_owned());

        let mut spec = SessionSpec::new("/work");
        spec.model = Some("claude-opus-4".to_owned());
        spec.system_prompt = Some(SystemPromptSpec {
            text: "be terse".to_owned(),
            mode: SystemPromptMode::Replace,
        });
        spec.policy = ToolPolicy::from_patterns(
            PolicyMode::AutoEdits,
            &["Read", "Bash(git:*)"],
            &["mcp__po__admin"],
        )
        .unwrap();
        spec.mcp_servers.insert(
            "po".to_owned(),
            McpServerSpec::Stdio {
                command: "/bin/mcp".to_owned(),
                args: vec!["serve".to_owned()],
                env: BTreeMap::from([("K".to_owned(), "v".to_owned())]),
            },
        );
        spec.mcp_servers.insert(
            "remote".to_owned(),
            McpServerSpec::Http {
                url: "https://mcp.example".to_owned(),
                headers: BTreeMap::new(),
            },
        );
        spec.extra_dirs = vec![PathBuf::from("/other")];
        spec.max_turns = Some(50);
        spec.env.inherit = vec!["AWS_REGION".to_owned()];
        spec.env.set.insert("PATH".to_owned(), "/bin".to_owned());
        spec.limits = SessionLimits {
            max_cost_usd: Some(2.5),
            ..SessionLimits::default()
        };
        spec.deltas = false;

        let options = build_options(&config, &spec, Some("cli-session-9")).unwrap();
        assert_eq!(options.model.as_deref(), Some("claude-opus-4"));
        assert_eq!(options.system_prompt.as_deref(), Some("be terse"));
        assert_eq!(options.append_system_prompt, None);
        assert_eq!(options.permission_mode, PermissionMode::AcceptEdits);
        assert_eq!(options.allowed_tools, ["Read", "Bash(git *)"]);
        assert_eq!(options.disallowed_tools, ["mcp__po__admin"]);
        assert_eq!(options.max_turns, Some(50));
        assert!(!options.include_partial_messages);
        assert_eq!(options.resume.as_deref(), Some("cli-session-9"));
        assert_eq!(options.add_dirs, [PathBuf::from("/other")]);
        assert_eq!(options.env.get("PATH").map(String::as_str), Some("/bin"));
        assert!(options.env_policy.allows("AWS_REGION"));
        assert!(options.env_policy.allows("ANTHROPIC_API_KEY"));
        assert!(!options.env_policy.allows("NEO4J_PASSWORD"));
        assert_eq!(options.cli_path, Some(PathBuf::from("/opt/claude")));
        assert_eq!(options.max_budget_usd, Some(2.5));
        match options.mcp_servers.get("po") {
            Some(McpServerConfig::Stdio { command, args, env }) => {
                assert_eq!(command, "/bin/mcp");
                assert_eq!(args.as_deref(), Some(&["serve".to_owned()][..]));
                assert_eq!(
                    env.as_ref()
                        .and_then(|env| env.get("K"))
                        .map(String::as_str),
                    Some("v")
                );
            },
            _ => panic!("the stdio server is carried over"),
        }
        assert!(matches!(
            options.mcp_servers.get("remote"),
            Some(McpServerConfig::Http { headers: None, .. })
        ));
    }

    #[test]
    fn an_appended_prompt_and_the_instance_default_model() {
        let mut config = ClaudeCodeConfig::default();
        config.default_model = Some("instance-default".to_owned());
        let mut spec = SessionSpec::new("/work");
        spec.system_prompt = Some(SystemPromptSpec {
            text: "and polite".to_owned(),
            mode: SystemPromptMode::Append,
        });
        let options = build_options(&config, &spec, None).unwrap();
        assert_eq!(options.system_prompt, None);
        assert_eq!(options.append_system_prompt.as_deref(), Some("and polite"));
        assert_eq!(options.model.as_deref(), Some("instance-default"));
    }

    #[test]
    fn the_extension_overrides_the_instance_and_reports_what_it_ignores() {
        let mut config = ClaudeCodeConfig::default();
        config.cli_path = Some(PathBuf::from("/opt/claude"));
        let mut spec = SessionSpec::new("/work");
        spec.extensions.insert(
            "claude_code".to_owned(),
            json!({
                "cli_path": "/tmp/fake_claude",
                "fallback_model": "haiku",
                "permission_prompt_tool": "mcp__po__approve",
                "extra_args": {"debug": null, "max-thinking": "9"},
                "setting_sources": ["user", "project"],
                "betas": ["context-1m"],
                "brand_new": true,
            }),
        );
        spec.extensions
            .insert("codex".to_owned(), json!({"sandbox": "x"}));
        let options = build_options(&config, &spec, None).unwrap();
        assert_eq!(options.cli_path, Some(PathBuf::from("/tmp/fake_claude")));
        assert_eq!(options.fallback_model.as_deref(), Some("haiku"));
        assert_eq!(
            options.permission_prompt_tool_name.as_deref(),
            Some("mcp__po__approve")
        );
        assert_eq!(options.extra_args.get("debug"), Some(&None));
        assert_eq!(
            options.extra_args.get("max-thinking"),
            Some(&Some("9".to_owned()))
        );
        assert_eq!(
            options.setting_sources,
            Some(vec![SettingSource::User, SettingSource::Project])
        );
        assert_eq!(ignored_extension_keys(&spec), ["betas", "brand_new"]);
        assert!(ignored_extension_keys(&SessionSpec::new("/work")).is_empty());
    }

    #[test]
    fn a_malformed_extension_is_refused_not_ignored() {
        for extension in [
            json!({"extra_args": ["--debug"]}),
            json!({"extra_args": {"n": 3}}),
            json!({"setting_sources": ["martian"]}),
        ] {
            let mut spec = SessionSpec::new("/work");
            spec.extensions.insert("claude_code".to_owned(), extension);
            assert!(matches!(
                build_options(&ClaudeCodeConfig::default(), &spec, None),
                Err(ProviderError::InvalidRequest { .. })
            ));
        }
    }
}
