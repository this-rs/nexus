//! Type definitions for the Claude Code SDK
//!
//! This module contains all the core types used throughout the SDK,
//! including messages, configuration options, and content blocks.

#![allow(missing_docs)]
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Callback type for stderr output handling.
/// Called with each line of stderr output from the CLI.
pub type StderrCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Permission mode for tool execution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Default mode - CLI prompts for dangerous tools
    #[default]
    Default,
    /// Auto-accept file edits
    AcceptEdits,
    /// Plan mode - for planning tasks
    Plan,
    /// Allow all tools without prompting (use with caution)
    BypassPermissions,
}

// ============================================================================
// SDK Beta Features (matching Python SDK v0.1.12+)
// ============================================================================

/// SDK Beta features - see <https://docs.anthropic.com/en/api/beta-headers>
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SdkBeta {
    /// Extended context window (1M tokens)
    #[serde(rename = "context-1m-2025-08-07")]
    Context1M,
}

impl std::fmt::Display for SdkBeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SdkBeta::Context1M => write!(f, "context-1m-2025-08-07"),
        }
    }
}

// ============================================================================
// Tools Configuration (matching Python SDK v0.1.12+)
// ============================================================================

/// Tools configuration for controlling available tools
///
/// This controls the base set of tools available to Claude, distinct from
/// `allowed_tools` which only controls auto-approval permissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolsConfig {
    /// List of specific tool names to enable
    /// Example: `["Read", "Edit", "Bash"]`
    List(Vec<String>),
    /// Preset-based tools configuration
    Preset(ToolsPreset),
}

/// Tools preset configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsPreset {
    /// Type identifier (always "preset")
    #[serde(rename = "type")]
    pub preset_type: String,
    /// Preset name (e.g., "claude_code")
    pub preset: String,
}

impl ToolsConfig {
    /// Create a new tools list
    pub fn list(tools: Vec<String>) -> Self {
        ToolsConfig::List(tools)
    }

    /// Create an empty tools list (disables all built-in tools)
    pub fn none() -> Self {
        ToolsConfig::List(vec![])
    }

    /// Create the claude_code preset
    pub fn claude_code_preset() -> Self {
        ToolsConfig::Preset(ToolsPreset {
            preset_type: "preset".to_string(),
            preset: "claude_code".to_string(),
        })
    }
}

// ============================================================================
// Sandbox Configuration (matching Python SDK)
// ============================================================================

/// Network configuration for sandbox
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxNetworkConfig {
    /// Unix socket paths accessible in sandbox (e.g., SSH agents)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_unix_sockets: Option<Vec<String>>,
    /// Allow all Unix sockets (less secure)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_all_unix_sockets: Option<bool>,
    /// Allow binding to localhost ports (macOS only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_local_binding: Option<bool>,
    /// HTTP proxy port if bringing your own proxy
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_proxy_port: Option<u16>,
    /// SOCKS5 proxy port if bringing your own proxy
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socks_proxy_port: Option<u16>,
}

/// Violations to ignore in sandbox
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxIgnoreViolations {
    /// File paths for which violations should be ignored
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<Vec<String>>,
    /// Network hosts for which violations should be ignored
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<Vec<String>>,
}

/// Sandbox settings configuration
///
/// Controls how Claude Code sandboxes bash commands for filesystem
/// and network isolation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSettings {
    /// Enable bash sandboxing (macOS/Linux only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Auto-approve bash commands when sandboxed (default: true)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_allow_bash_if_sandboxed: Option<bool>,
    /// Commands that should run outside the sandbox (e.g., ["git", "docker"])
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_commands: Option<Vec<String>>,
    /// Allow commands to bypass sandbox via dangerouslyDisableSandbox
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_unsandboxed_commands: Option<bool>,
    /// Network configuration for sandbox
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<SandboxNetworkConfig>,
    /// Violations to ignore
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore_violations: Option<SandboxIgnoreViolations>,
    /// Enable weaker sandbox for unprivileged Docker environments (Linux only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_weaker_nested_sandbox: Option<bool>,
}

// ============================================================================
// Plugin Configuration (matching Python SDK v0.1.5+)
// ============================================================================

/// SDK plugin configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SdkPluginConfig {
    /// Local plugin loaded from filesystem path
    Local {
        /// Path to the plugin directory
        path: String,
    },
}

/// Control protocol format for sending messages
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ControlProtocolFormat {
    /// Legacy format: {"type":"sdk_control_request","request":{...}}
    #[default]
    Legacy,
    /// New format: {"type":"control","control":{...}}
    Control,
    /// Auto-detect based on CLI capabilities (default to Legacy for compatibility)
    Auto,
}

/// MCP (Model Context Protocol) server configuration
#[derive(Clone)]
pub enum McpServerConfig {
    /// Standard I/O based MCP server
    Stdio {
        /// Command to execute
        command: String,
        /// Command arguments
        args: Option<Vec<String>>,
        /// Environment variables
        env: Option<HashMap<String, String>>,
    },
    /// Server-Sent Events based MCP server
    Sse {
        /// Server URL
        url: String,
        /// HTTP headers
        headers: Option<HashMap<String, String>>,
    },
    /// HTTP-based MCP server
    Http {
        /// Server URL
        url: String,
        /// HTTP headers
        headers: Option<HashMap<String, String>>,
    },
    /// SDK MCP server (in-process)
    Sdk {
        /// Server name
        name: String,
        /// Server instance
        instance: Arc<dyn std::any::Any + Send + Sync>,
    },
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdio { command, args, env } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .field("env", env)
                .finish(),
            Self::Sse { url, headers } => f
                .debug_struct("Sse")
                .field("url", url)
                .field("headers", headers)
                .finish(),
            Self::Http { url, headers } => f
                .debug_struct("Http")
                .field("url", url)
                .field("headers", headers)
                .finish(),
            Self::Sdk { name, .. } => f
                .debug_struct("Sdk")
                .field("name", name)
                .field("instance", &"<Arc<dyn Any>>")
                .finish(),
        }
    }
}

impl Serialize for McpServerConfig {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;

        match self {
            Self::Stdio { command, args, env } => {
                map.serialize_entry("type", "stdio")?;
                map.serialize_entry("command", command)?;
                if let Some(args) = args {
                    map.serialize_entry("args", args)?;
                }
                if let Some(env) = env {
                    map.serialize_entry("env", env)?;
                }
            },
            Self::Sse { url, headers } => {
                map.serialize_entry("type", "sse")?;
                map.serialize_entry("url", url)?;
                if let Some(headers) = headers {
                    map.serialize_entry("headers", headers)?;
                }
            },
            Self::Http { url, headers } => {
                map.serialize_entry("type", "http")?;
                map.serialize_entry("url", url)?;
                if let Some(headers) = headers {
                    map.serialize_entry("headers", headers)?;
                }
            },
            Self::Sdk { name, .. } => {
                map.serialize_entry("type", "sdk")?;
                map.serialize_entry("name", name)?;
            },
        }

        map.end()
    }
}

impl<'de> Deserialize<'de> for McpServerConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "lowercase")]
        enum McpServerConfigHelper {
            Stdio {
                command: String,
                #[serde(skip_serializing_if = "Option::is_none")]
                args: Option<Vec<String>>,
                #[serde(skip_serializing_if = "Option::is_none")]
                env: Option<HashMap<String, String>>,
            },
            Sse {
                url: String,
                #[serde(skip_serializing_if = "Option::is_none")]
                headers: Option<HashMap<String, String>>,
            },
            Http {
                url: String,
                #[serde(skip_serializing_if = "Option::is_none")]
                headers: Option<HashMap<String, String>>,
            },
        }

        let helper = McpServerConfigHelper::deserialize(deserializer)?;
        Ok(match helper {
            McpServerConfigHelper::Stdio { command, args, env } => {
                McpServerConfig::Stdio { command, args, env }
            },
            McpServerConfigHelper::Sse { url, headers } => McpServerConfig::Sse { url, headers },
            McpServerConfigHelper::Http { url, headers } => McpServerConfig::Http { url, headers },
        })
    }
}

/// Permission update destination
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionUpdateDestination {
    /// User settings
    UserSettings,
    /// Project settings
    ProjectSettings,
    /// Local settings
    LocalSettings,
    /// Session
    Session,
}

/// Permission behavior
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionBehavior {
    /// Allow the action
    Allow,
    /// Deny the action
    Deny,
    /// Ask the user
    Ask,
}

/// Permission rule value
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRuleValue {
    /// Tool name
    pub tool_name: String,
    /// Rule content
    pub rule_content: Option<String>,
}

/// Permission update type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionUpdateType {
    /// Add rules
    AddRules,
    /// Replace rules
    ReplaceRules,
    /// Remove rules
    RemoveRules,
    /// Set mode
    SetMode,
    /// Add directories
    AddDirectories,
    /// Remove directories
    RemoveDirectories,
}

/// Permission update
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionUpdate {
    /// Update type
    #[serde(rename = "type")]
    pub update_type: PermissionUpdateType,
    /// Rules to update
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<PermissionRuleValue>>,
    /// Behavior to set
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behavior: Option<PermissionBehavior>,
    /// Mode to set
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<PermissionMode>,
    /// Directories to add/remove
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directories: Option<Vec<String>>,
    /// Destination for the update
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<PermissionUpdateDestination>,
}

/// Tool permission context
#[derive(Debug, Clone)]
pub struct ToolPermissionContext {
    /// Abort signal (future support)
    pub signal: Option<Arc<dyn std::any::Any + Send + Sync>>,
    /// Permission suggestions from CLI
    pub suggestions: Vec<PermissionUpdate>,
}

/// Permission result - Allow
#[derive(Debug, Clone)]
pub struct PermissionResultAllow {
    /// Updated input parameters
    pub updated_input: Option<serde_json::Value>,
    /// Updated permissions
    pub updated_permissions: Option<Vec<PermissionUpdate>>,
}

/// Permission result - Deny
#[derive(Debug, Clone)]
pub struct PermissionResultDeny {
    /// Denial message
    pub message: String,
    /// Whether to interrupt the conversation
    pub interrupt: bool,
}

/// Permission result
#[derive(Debug, Clone)]
pub enum PermissionResult {
    /// Allow the tool use
    Allow(PermissionResultAllow),
    /// Deny the tool use
    Deny(PermissionResultDeny),
}

/// Tool permission callback trait
#[async_trait]
pub trait CanUseTool: Send + Sync {
    /// Check if a tool can be used
    async fn can_use_tool(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        context: &ToolPermissionContext,
    ) -> PermissionResult;
}

/// Hook context
#[derive(Debug, Clone)]
pub struct HookContext {
    /// Abort signal (future support)
    pub signal: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

// ============================================================================
// Hook Input Types (Strongly-typed hook inputs for type safety)
// ============================================================================

/// Base hook input fields present across many hook events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseHookInput {
    /// Session ID for this conversation
    pub session_id: String,
    /// Path to the transcript file
    pub transcript_path: String,
    /// Current working directory
    pub cwd: String,
    /// Permission mode (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
}

/// Input data for PreToolUse hook events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreToolUseHookInput {
    /// Session ID for this conversation
    pub session_id: String,
    /// Path to the transcript file
    pub transcript_path: String,
    /// Current working directory
    pub cwd: String,
    /// Permission mode (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Name of the tool being used
    pub tool_name: String,
    /// Input parameters for the tool
    pub tool_input: serde_json::Value,
}

/// Input data for PostToolUse hook events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostToolUseHookInput {
    /// Session ID for this conversation
    pub session_id: String,
    /// Path to the transcript file
    pub transcript_path: String,
    /// Current working directory
    pub cwd: String,
    /// Permission mode (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Name of the tool that was used
    pub tool_name: String,
    /// Input parameters that were passed to the tool
    pub tool_input: serde_json::Value,
    /// Response from the tool execution
    pub tool_response: serde_json::Value,
}

/// Input data for UserPromptSubmit hook events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPromptSubmitHookInput {
    /// Session ID for this conversation
    pub session_id: String,
    /// Path to the transcript file
    pub transcript_path: String,
    /// Current working directory
    pub cwd: String,
    /// Permission mode (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// The prompt submitted by the user
    pub prompt: String,
}

/// Input data for Stop hook events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopHookInput {
    /// Session ID for this conversation
    pub session_id: String,
    /// Path to the transcript file
    pub transcript_path: String,
    /// Current working directory
    pub cwd: String,
    /// Permission mode (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Whether stop hook is active
    pub stop_hook_active: bool,
}

/// Input data for SubagentStop hook events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentStopHookInput {
    /// Session ID for this conversation
    pub session_id: String,
    /// Path to the transcript file
    pub transcript_path: String,
    /// Current working directory
    pub cwd: String,
    /// Permission mode (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Whether stop hook is active
    pub stop_hook_active: bool,
}

/// Input data for PreCompact hook events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreCompactHookInput {
    /// Session ID for this conversation
    pub session_id: String,
    /// Path to the transcript file
    pub transcript_path: String,
    /// Current working directory
    pub cwd: String,
    /// Permission mode (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Trigger type: "manual" or "auto"
    pub trigger: String,
    /// Custom instructions for compaction (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
}

/// Union type for all hook inputs (discriminated by hook_event_name)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "hook_event_name")]
pub enum HookInput {
    /// PreToolUse hook input
    #[serde(rename = "PreToolUse")]
    PreToolUse(PreToolUseHookInput),
    /// PostToolUse hook input
    #[serde(rename = "PostToolUse")]
    PostToolUse(PostToolUseHookInput),
    /// UserPromptSubmit hook input
    #[serde(rename = "UserPromptSubmit")]
    UserPromptSubmit(UserPromptSubmitHookInput),
    /// Stop hook input
    #[serde(rename = "Stop")]
    Stop(StopHookInput),
    /// SubagentStop hook input
    #[serde(rename = "SubagentStop")]
    SubagentStop(SubagentStopHookInput),
    /// PreCompact hook input
    #[serde(rename = "PreCompact")]
    PreCompact(PreCompactHookInput),
}

// ============================================================================
// Hook Output Types (Strongly-typed hook outputs for type safety)
// ============================================================================

/// Async hook output for deferred execution
///
/// When a hook returns this output, the hook execution is deferred and
/// Claude continues without waiting for the hook to complete.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsyncHookJSONOutput {
    /// Must be true to indicate async execution
    #[serde(rename = "async")]
    pub async_: bool,
    /// Optional timeout in milliseconds for async operation
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "asyncTimeout")]
    pub async_timeout: Option<u32>,
}

/// Synchronous hook output with control and decision fields
///
/// This defines the structure for hook callbacks to control execution and provide
/// feedback to Claude.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncHookJSONOutput {
    // Common control fields
    /// Whether Claude should proceed after hook execution (default: true)
    #[serde(rename = "continue", skip_serializing_if = "Option::is_none")]
    pub continue_: Option<bool>,
    /// Hide stdout from transcript mode (default: false)
    #[serde(rename = "suppressOutput", skip_serializing_if = "Option::is_none")]
    pub suppress_output: Option<bool>,
    /// Message shown when continue is false
    #[serde(rename = "stopReason", skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,

    // Decision fields
    /// Set to "block" to indicate blocking behavior
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>, // "block" or "approve" (deprecated)
    /// Warning message displayed to the user
    #[serde(rename = "systemMessage", skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,
    /// Feedback message for Claude about the decision
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    // Hook-specific outputs
    /// Event-specific controls (e.g., permissionDecision for PreToolUse)
    #[serde(rename = "hookSpecificOutput", skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

/// Union type for hook outputs
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HookJSONOutput {
    /// Async hook output (deferred execution)
    Async(AsyncHookJSONOutput),
    /// Sync hook output (immediate execution)
    Sync(SyncHookJSONOutput),
}

/// Hook-specific output for PreToolUse events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreToolUseHookSpecificOutput {
    /// Permission decision: "allow", "deny", or "ask"
    #[serde(rename = "permissionDecision", skip_serializing_if = "Option::is_none")]
    pub permission_decision: Option<String>,
    /// Reason for the permission decision
    #[serde(
        rename = "permissionDecisionReason",
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_decision_reason: Option<String>,
    /// Updated input parameters for the tool
    #[serde(rename = "updatedInput", skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<serde_json::Value>,
    /// Additional context to provide to Claude (e.g. skill activation context)
    #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

/// Hook-specific output for PostToolUse events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostToolUseHookSpecificOutput {
    /// Additional context to provide to Claude
    #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

/// Hook-specific output for UserPromptSubmit events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPromptSubmitHookSpecificOutput {
    /// Additional context to provide to Claude
    #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

/// Hook-specific output for SessionStart events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStartHookSpecificOutput {
    /// Additional context to provide to Claude
    #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

/// Union type for hook-specific outputs (discriminated by hookEventName)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "hookEventName")]
pub enum HookSpecificOutput {
    /// PreToolUse-specific output
    #[serde(rename = "PreToolUse")]
    PreToolUse(PreToolUseHookSpecificOutput),
    /// PostToolUse-specific output
    #[serde(rename = "PostToolUse")]
    PostToolUse(PostToolUseHookSpecificOutput),
    /// UserPromptSubmit-specific output
    #[serde(rename = "UserPromptSubmit")]
    UserPromptSubmit(UserPromptSubmitHookSpecificOutput),
    /// SessionStart-specific output
    #[serde(rename = "SessionStart")]
    SessionStart(SessionStartHookSpecificOutput),
}

// ============================================================================
// Hook Callback Trait (Updated for strong typing)
// ============================================================================

/// Hook callback trait with strongly-typed inputs and outputs
///
/// This trait is used to implement custom hook callbacks that can intercept
/// and modify Claude's behavior at various points in the conversation.
#[async_trait]
pub trait HookCallback: Send + Sync {
    /// Execute the hook with strongly-typed input and output
    ///
    /// # Arguments
    ///
    /// * `input` - Strongly-typed hook input (discriminated union)
    /// * `tool_use_id` - Optional tool use identifier
    /// * `context` - Hook context with abort signal support
    ///
    /// # Returns
    ///
    /// A `HookJSONOutput` that controls Claude's behavior
    async fn execute(
        &self,
        input: &HookInput,
        tool_use_id: Option<&str>,
        context: &HookContext,
    ) -> Result<HookJSONOutput, crate::errors::SdkError>;
}

/// Legacy hook callback trait for backward compatibility
///
/// This trait is deprecated and will be removed in v0.4.0.
/// Please migrate to the new `HookCallback` trait with strong typing.
#[deprecated(
    since = "0.3.0",
    note = "Use the new HookCallback trait with HookInput/HookJSONOutput instead"
)]
#[allow(dead_code)]
#[async_trait]
pub trait HookCallbackLegacy: Send + Sync {
    /// Execute the hook with JSON values (legacy)
    async fn execute_legacy(
        &self,
        input: &serde_json::Value,
        tool_use_id: Option<&str>,
        context: &HookContext,
    ) -> serde_json::Value;
}

/// Hook matcher configuration
#[derive(Clone)]
pub struct HookMatcher {
    /// Matcher criteria
    pub matcher: Option<serde_json::Value>,
    /// Callbacks to invoke
    pub hooks: Vec<Arc<dyn HookCallback>>,
}

/// Setting source for configuration loading
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingSource {
    /// User-level settings
    User,
    /// Project-level settings
    Project,
    /// Local settings
    Local,
}

/// Agent definition for programmatic agents
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    /// Agent description
    pub description: String,
    /// Agent prompt
    pub prompt: String,
    /// Allowed tools for this agent
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Model to use
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// System prompt configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    /// Simple string prompt
    String(String),
    /// Preset-based prompt with optional append
    Preset {
        #[serde(rename = "type")]
        preset_type: String, // "preset"
        preset: String, // e.g., "claude_code"
        #[serde(skip_serializing_if = "Option::is_none")]
        append: Option<String>,
    },
}

/// Configuration options for Claude Code SDK
#[derive(Clone, Default)]
pub struct ClaudeCodeOptions {
    /// System prompt configuration (simplified in v0.1.12+)
    /// Can be either a string or a preset configuration
    /// Replaces the old system_prompt and append_system_prompt fields
    pub system_prompt_v2: Option<SystemPrompt>,
    /// \[DEPRECATED\] System prompt to prepend to all messages
    /// Use system_prompt_v2 instead
    #[deprecated(since = "0.1.12", note = "Use system_prompt_v2 instead")]
    pub system_prompt: Option<String>,
    /// \[DEPRECATED\] Additional system prompt to append
    /// Use system_prompt_v2 instead
    #[deprecated(since = "0.1.12", note = "Use system_prompt_v2 instead")]
    pub append_system_prompt: Option<String>,
    /// List of allowed tools (auto-approval permissions only)
    ///
    /// **IMPORTANT**: This only controls which tool invocations are auto-approved
    /// (bypass permission prompts). It does NOT disable or restrict which tools
    /// the AI can use. Use `disallowed_tools` to completely disable tools.
    ///
    /// Example: `allowed_tools: vec!["Bash(git:*)".to_string()]` allows auto-approval
    /// for git commands in Bash, but doesn't prevent AI from using other tools.
    pub allowed_tools: Vec<String>,
    /// List of disallowed tools (completely disabled)
    ///
    /// **IMPORTANT**: This completely disables the specified tools. The AI will
    /// not be able to use these tools at all. Use this to restrict which tools
    /// the AI has access to.
    ///
    /// Example: `disallowed_tools: vec!["Bash".to_string(), "WebSearch".to_string()]`
    /// prevents the AI from using Bash or WebSearch tools entirely.
    pub disallowed_tools: Vec<String>,
    /// Permission mode for tool execution
    pub permission_mode: PermissionMode,
    /// MCP server configurations
    pub mcp_servers: HashMap<String, McpServerConfig>,
    /// MCP tools to enable
    pub mcp_tools: Vec<String>,
    /// Maximum number of conversation turns
    pub max_turns: Option<i32>,
    /// Maximum thinking tokens
    pub max_thinking_tokens: i32,
    /// Maximum output tokens per response (1-32000, overrides CLAUDE_CODE_MAX_OUTPUT_TOKENS env var)
    pub max_output_tokens: Option<u32>,
    /// Model to use
    pub model: Option<String>,
    /// Working directory
    pub cwd: Option<PathBuf>,
    /// Continue from previous conversation
    pub continue_conversation: bool,
    /// Resume from a specific conversation ID
    pub resume: Option<String>,
    /// Custom permission prompt tool name
    pub permission_prompt_tool_name: Option<String>,
    /// Settings file path for Claude Code CLI
    pub settings: Option<String>,
    /// Additional directories to add as working directories
    pub add_dirs: Vec<PathBuf>,
    /// Extra arbitrary CLI flags
    pub extra_args: HashMap<String, Option<String>>,
    /// Environment variables to pass to the process
    pub env: HashMap<String, String>,
    /// Debug output stream (e.g., stderr)
    pub debug_stderr: Option<Arc<Mutex<dyn Write + Send + Sync>>>,
    /// Include partial assistant messages in streaming output
    pub include_partial_messages: bool,
    /// Tool permission callback
    pub can_use_tool: Option<Arc<dyn CanUseTool>>,
    /// Hook configurations
    pub hooks: Option<HashMap<String, Vec<HookMatcher>>>,
    /// Control protocol format (defaults to Legacy for compatibility)
    pub control_protocol_format: ControlProtocolFormat,

    // ========== Phase 2 Enhancements ==========
    /// Setting sources to load (user, project, local)
    /// When None, no filesystem settings are loaded (matches Python SDK v0.1.0 behavior)
    pub setting_sources: Option<Vec<SettingSource>>,
    /// Fork session when resuming instead of continuing
    /// When true, creates a new branch from the resumed session
    pub fork_session: bool,
    /// Programmatic agent definitions
    /// Define agents inline without filesystem dependencies
    pub agents: Option<HashMap<String, AgentDefinition>>,
    /// CLI channel buffer size for internal communication channels
    /// Controls the size of message, control, and stdin buffers (default: 100)
    /// Increase for high-throughput scenarios to prevent message lag
    pub cli_channel_buffer_size: Option<usize>,

    // ========== Phase 3 Enhancements (Python SDK v0.1.12+ sync) ==========
    /// Tools configuration for controlling available tools
    ///
    /// This controls the base set of tools available to Claude, distinct from
    /// `allowed_tools` which only controls auto-approval permissions.
    ///
    /// # Examples
    /// ```rust
    /// use nexus_claude::{ClaudeCodeOptions, ToolsConfig};
    ///
    /// // Enable specific tools only
    /// let options = ClaudeCodeOptions::builder()
    ///     .tools(ToolsConfig::list(vec!["Read".into(), "Edit".into()]))
    ///     .build();
    ///
    /// // Disable all built-in tools
    /// let options = ClaudeCodeOptions::builder()
    ///     .tools(ToolsConfig::none())
    ///     .build();
    ///
    /// // Use claude_code preset
    /// let options = ClaudeCodeOptions::builder()
    ///     .tools(ToolsConfig::claude_code_preset())
    ///     .build();
    /// ```
    pub tools: Option<ToolsConfig>,
    /// SDK beta features to enable
    /// See <https://docs.anthropic.com/en/api/beta-headers>
    pub betas: Vec<SdkBeta>,
    /// Maximum spending limit in USD for the session
    /// When exceeded, the session will automatically terminate
    pub max_budget_usd: Option<f64>,
    /// Fallback model to use when primary model is unavailable
    pub fallback_model: Option<String>,
    /// Output format for structured outputs
    /// Example: `{"type": "json_schema", "schema": {"type": "object", "properties": {...}}}`
    pub output_format: Option<serde_json::Value>,
    /// Enable file checkpointing to track file changes during the session
    /// When enabled, files can be rewound to their state at any user message
    /// using `ClaudeSDKClient::rewind_files()`
    pub enable_file_checkpointing: bool,
    /// Sandbox configuration for bash command isolation
    /// Filesystem and network restrictions are derived from permission rules
    pub sandbox: Option<SandboxSettings>,
    /// Plugin configurations for custom plugins
    pub plugins: Vec<SdkPluginConfig>,
    /// Run the CLI subprocess as a specific OS user (Unix-only).
    ///
    /// This matches Python SDK behavior (`anyio.open_process(user=...)`).
    ///
    /// - Supported on Unix platforms only (non-Unix returns `SdkError::NotSupported`)
    /// - Typically requires elevated privileges to switch users
    /// - Accepts a username (e.g. `"nobody"`) or a numeric uid string (e.g. `"1000"`)
    pub user: Option<String>,
    /// Stderr callback (alternative to debug_stderr)
    /// Called with each line of stderr output from the CLI
    pub stderr_callback: Option<StderrCallback>,
    /// Automatically download Claude Code CLI if not found
    ///
    /// When enabled, the SDK will automatically download and cache the Claude Code
    /// CLI binary if it's not found in the system PATH or common installation locations.
    ///
    /// The CLI is cached in:
    /// - macOS: `~/Library/Caches/cc-sdk/cli/`
    /// - Linux: `~/.cache/cc-sdk/cli/`
    /// - Windows: `%LOCALAPPDATA%\cc-sdk\cli\`
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nexus_claude::ClaudeCodeOptions;
    /// let options = ClaudeCodeOptions::builder()
    ///     .auto_download_cli(true)
    ///     .build();
    /// ```
    pub auto_download_cli: bool,

    /// Explicit path to the Claude CLI binary
    ///
    /// When set, the SDK will use this path directly instead of searching
    /// via `find_claude_cli()`. Useful when the CLI is installed in a
    /// non-standard location.
    pub cli_path: Option<PathBuf>,

    // ========== Memory System Options ==========
    /// Enable persistent memory for cross-conversation context
    ///
    /// When enabled, the SDK will:
    /// 1. Store messages in Meilisearch for later retrieval
    /// 2. Retrieve relevant context before each request
    /// 3. Inject context into the system prompt
    ///
    /// Requires the `memory` feature and a running Meilisearch instance.
    /// Default: false
    pub memory_enabled: bool,

    /// Minimum relevance score for context injection (0.0-1.0)
    ///
    /// Messages with scores below this threshold are not included in context.
    /// Higher values = more selective, fewer irrelevant results.
    /// Default: 0.3
    pub memory_threshold: Option<f64>,

    /// Maximum number of context items to inject per request
    ///
    /// Limits how many previous messages are included in context.
    /// Default: 5
    pub max_context_items: Option<usize>,

    /// Token budget for injected context (~4 chars per token)
    ///
    /// Limits the total size of injected context to avoid overwhelming the prompt.
    /// Default: 2000
    pub memory_token_budget: Option<usize>,
}

impl std::fmt::Debug for ClaudeCodeOptions {
    #[allow(deprecated)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCodeOptions")
            .field("system_prompt", &self.system_prompt)
            .field("append_system_prompt", &self.append_system_prompt)
            .field("allowed_tools", &self.allowed_tools)
            .field("disallowed_tools", &self.disallowed_tools)
            .field("permission_mode", &self.permission_mode)
            .field("mcp_servers", &self.mcp_servers)
            .field("mcp_tools", &self.mcp_tools)
            .field("max_turns", &self.max_turns)
            .field("max_thinking_tokens", &self.max_thinking_tokens)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("model", &self.model)
            .field("cwd", &self.cwd)
            .field("continue_conversation", &self.continue_conversation)
            .field("resume", &self.resume)
            .field(
                "permission_prompt_tool_name",
                &self.permission_prompt_tool_name,
            )
            .field("settings", &self.settings)
            .field("add_dirs", &self.add_dirs)
            .field("extra_args", &self.extra_args)
            .field("env", &self.env)
            .field("debug_stderr", &self.debug_stderr.is_some())
            .field("include_partial_messages", &self.include_partial_messages)
            .field("can_use_tool", &self.can_use_tool.is_some())
            .field("hooks", &self.hooks.is_some())
            .field("control_protocol_format", &self.control_protocol_format)
            .finish()
    }
}

impl ClaudeCodeOptions {
    /// Create a new options builder
    pub fn builder() -> ClaudeCodeOptionsBuilder {
        ClaudeCodeOptionsBuilder::default()
    }
}

/// Builder for ClaudeCodeOptions
#[derive(Debug, Default)]
pub struct ClaudeCodeOptionsBuilder {
    options: ClaudeCodeOptions,
}

impl ClaudeCodeOptionsBuilder {
    /// Set system prompt
    #[allow(deprecated)]
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.options.system_prompt = Some(prompt.into());
        self
    }

    /// Set append system prompt
    #[allow(deprecated)]
    pub fn append_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.options.append_system_prompt = Some(prompt.into());
        self
    }

    /// Set allowed tools (auto-approval permissions only)
    ///
    /// **IMPORTANT**: This only controls which tool invocations bypass permission
    /// prompts. It does NOT disable or restrict which tools the AI can use.
    /// To completely disable tools, use `disallowed_tools()` instead.
    ///
    /// Example: `vec!["Bash(git:*)".to_string()]` auto-approves git commands.
    pub fn allowed_tools(mut self, tools: Vec<String>) -> Self {
        self.options.allowed_tools = tools;
        self
    }

    /// Add a single allowed tool (auto-approval permission)
    ///
    /// See `allowed_tools()` for important usage notes.
    pub fn allow_tool(mut self, tool: impl Into<String>) -> Self {
        self.options.allowed_tools.push(tool.into());
        self
    }

    /// Set disallowed tools (completely disabled)
    ///
    /// **IMPORTANT**: This completely disables the specified tools. The AI will
    /// not be able to use these tools at all. This is the correct way to restrict
    /// which tools the AI has access to.
    ///
    /// Example: `vec!["Bash".to_string(), "WebSearch".to_string()]` prevents
    /// the AI from using Bash or WebSearch entirely.
    pub fn disallowed_tools(mut self, tools: Vec<String>) -> Self {
        self.options.disallowed_tools = tools;
        self
    }

    /// Add a single disallowed tool (completely disabled)
    ///
    /// See `disallowed_tools()` for important usage notes.
    pub fn disallow_tool(mut self, tool: impl Into<String>) -> Self {
        self.options.disallowed_tools.push(tool.into());
        self
    }

    /// Set permission mode
    pub fn permission_mode(mut self, mode: PermissionMode) -> Self {
        self.options.permission_mode = mode;
        self
    }

    /// Add MCP server
    pub fn add_mcp_server(mut self, name: impl Into<String>, config: McpServerConfig) -> Self {
        self.options.mcp_servers.insert(name.into(), config);
        self
    }

    /// Set all MCP servers from a map
    pub fn mcp_servers(mut self, servers: HashMap<String, McpServerConfig>) -> Self {
        self.options.mcp_servers = servers;
        self
    }

    /// Set MCP tools
    pub fn mcp_tools(mut self, tools: Vec<String>) -> Self {
        self.options.mcp_tools = tools;
        self
    }

    /// Set max turns
    pub fn max_turns(mut self, turns: i32) -> Self {
        self.options.max_turns = Some(turns);
        self
    }

    /// Set max thinking tokens
    pub fn max_thinking_tokens(mut self, tokens: i32) -> Self {
        self.options.max_thinking_tokens = tokens;
        self
    }

    /// Set max output tokens (1-32000, overrides CLAUDE_CODE_MAX_OUTPUT_TOKENS env var)
    pub fn max_output_tokens(mut self, tokens: u32) -> Self {
        self.options.max_output_tokens = Some(tokens.clamp(1, 32000));
        self
    }

    /// Set model
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.options.model = Some(model.into());
        self
    }

    /// Set working directory
    pub fn cwd(mut self, path: impl Into<PathBuf>) -> Self {
        self.options.cwd = Some(path.into());
        self
    }

    /// Enable continue conversation
    pub fn continue_conversation(mut self, enable: bool) -> Self {
        self.options.continue_conversation = enable;
        self
    }

    /// Set resume conversation ID
    pub fn resume(mut self, id: impl Into<String>) -> Self {
        self.options.resume = Some(id.into());
        self
    }

    /// Set permission prompt tool name
    pub fn permission_prompt_tool_name(mut self, name: impl Into<String>) -> Self {
        self.options.permission_prompt_tool_name = Some(name.into());
        self
    }

    /// Set settings file path
    pub fn settings(mut self, settings: impl Into<String>) -> Self {
        self.options.settings = Some(settings.into());
        self
    }

    /// Add directories as working directories
    pub fn add_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.options.add_dirs = dirs;
        self
    }

    /// Add a single directory as working directory
    pub fn add_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.options.add_dirs.push(dir.into());
        self
    }

    /// Add extra CLI arguments
    pub fn extra_args(mut self, args: HashMap<String, Option<String>>) -> Self {
        self.options.extra_args = args;
        self
    }

    /// Add a single extra CLI argument
    pub fn add_extra_arg(mut self, key: impl Into<String>, value: Option<String>) -> Self {
        self.options.extra_args.insert(key.into(), value);
        self
    }

    /// Set control protocol format
    pub fn control_protocol_format(mut self, format: ControlProtocolFormat) -> Self {
        self.options.control_protocol_format = format;
        self
    }

    /// Include partial assistant messages in streaming output
    pub fn include_partial_messages(mut self, include: bool) -> Self {
        self.options.include_partial_messages = include;
        self
    }

    /// Enable fork_session behavior
    pub fn fork_session(mut self, fork: bool) -> Self {
        self.options.fork_session = fork;
        self
    }

    /// Set setting sources
    pub fn setting_sources(mut self, sources: Vec<SettingSource>) -> Self {
        self.options.setting_sources = Some(sources);
        self
    }

    /// Define programmatic agents
    pub fn agents(mut self, agents: HashMap<String, AgentDefinition>) -> Self {
        self.options.agents = Some(agents);
        self
    }

    /// Set hook configurations (replaces any existing hooks)
    ///
    /// Hooks allow intercepting CLI events (PreToolUse, PostToolUse, PreCompact, etc.)
    /// and executing custom callbacks before or after the event is processed.
    ///
    /// # Arguments
    ///
    /// * `hooks` - Map of event names (PascalCase) to their matchers and callbacks
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use nexus_claude::{ClaudeCodeOptions, HookMatcher};
    /// # use std::collections::HashMap;
    /// let mut hooks = HashMap::new();
    /// // hooks.insert("PreCompact".into(), vec![matcher]);
    /// let options = ClaudeCodeOptions::builder()
    ///     .hooks(hooks)
    ///     .build();
    /// ```
    pub fn hooks(mut self, hooks: HashMap<String, Vec<HookMatcher>>) -> Self {
        self.options.hooks = Some(hooks);
        self
    }

    /// Add a single hook matcher for an event
    ///
    /// Appends to existing matchers for this event, or creates a new entry.
    /// Event names must be PascalCase: "PreToolUse", "PostToolUse",
    /// "UserPromptSubmit", "Stop", "SubagentStop", "PreCompact".
    pub fn add_hook(mut self, event_name: impl Into<String>, matcher: HookMatcher) -> Self {
        let hooks = self.options.hooks.get_or_insert_with(HashMap::new);
        hooks.entry(event_name.into()).or_default().push(matcher);
        self
    }

    /// Set CLI channel buffer size
    ///
    /// Controls the size of internal communication channels (message, control, stdin buffers).
    /// Default is 100. Increase for high-throughput scenarios to prevent message lag.
    ///
    /// # Arguments
    ///
    /// * `size` - Buffer size (number of messages that can be queued)
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nexus_claude::ClaudeCodeOptions;
    /// let options = ClaudeCodeOptions::builder()
    ///     .cli_channel_buffer_size(500)
    ///     .build();
    /// ```
    pub fn cli_channel_buffer_size(mut self, size: usize) -> Self {
        self.options.cli_channel_buffer_size = Some(size);
        self
    }

    // ========== Phase 3 Builder Methods (Python SDK v0.1.12+ sync) ==========

    /// Set tools configuration
    ///
    /// Controls the base set of tools available to Claude. This is distinct from
    /// `allowed_tools` which only controls auto-approval permissions.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use nexus_claude::{ClaudeCodeOptions, ToolsConfig};
    /// // Enable specific tools only
    /// let options = ClaudeCodeOptions::builder()
    ///     .tools(ToolsConfig::list(vec!["Read".into(), "Edit".into()]))
    ///     .build();
    /// ```
    pub fn tools(mut self, config: ToolsConfig) -> Self {
        self.options.tools = Some(config);
        self
    }

    /// Add SDK beta features
    ///
    /// Enable Anthropic API beta features like extended context window.
    pub fn betas(mut self, betas: Vec<SdkBeta>) -> Self {
        self.options.betas = betas;
        self
    }

    /// Add a single SDK beta feature
    pub fn add_beta(mut self, beta: SdkBeta) -> Self {
        self.options.betas.push(beta);
        self
    }

    /// Set maximum spending limit in USD
    ///
    /// When the budget is exceeded, the session will automatically terminate.
    pub fn max_budget_usd(mut self, budget: f64) -> Self {
        self.options.max_budget_usd = Some(budget);
        self
    }

    /// Set fallback model
    ///
    /// Used when the primary model is unavailable.
    pub fn fallback_model(mut self, model: impl Into<String>) -> Self {
        self.options.fallback_model = Some(model.into());
        self
    }

    /// Set output format for structured outputs
    ///
    /// Enables JSON schema validation for Claude's responses.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nexus_claude::ClaudeCodeOptions;
    /// let options = ClaudeCodeOptions::builder()
    ///     .output_format(serde_json::json!({
    ///         "type": "json_schema",
    ///         "schema": {
    ///             "type": "object",
    ///             "properties": {
    ///                 "answer": {"type": "string"}
    ///             }
    ///         }
    ///     }))
    ///     .build();
    /// ```
    pub fn output_format(mut self, format: serde_json::Value) -> Self {
        self.options.output_format = Some(format);
        self
    }

    /// Enable file checkpointing
    ///
    /// When enabled, file changes are tracked and can be rewound to any
    /// user message using `ClaudeSDKClient::rewind_files()`.
    pub fn enable_file_checkpointing(mut self, enable: bool) -> Self {
        self.options.enable_file_checkpointing = enable;
        self
    }

    /// Set sandbox configuration
    ///
    /// Controls bash command sandboxing for filesystem and network isolation.
    pub fn sandbox(mut self, settings: SandboxSettings) -> Self {
        self.options.sandbox = Some(settings);
        self
    }

    /// Set plugin configurations
    pub fn plugins(mut self, plugins: Vec<SdkPluginConfig>) -> Self {
        self.options.plugins = plugins;
        self
    }

    /// Add a single plugin
    pub fn add_plugin(mut self, plugin: SdkPluginConfig) -> Self {
        self.options.plugins.push(plugin);
        self
    }

    /// Set user identifier
    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.options.user = Some(user.into());
        self
    }

    /// Set stderr callback
    ///
    /// Called with each line of stderr output from the CLI.
    pub fn stderr_callback(mut self, callback: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        self.options.stderr_callback = Some(callback);
        self
    }

    /// Enable automatic CLI download
    ///
    /// When enabled, the SDK will automatically download and cache the Claude Code
    /// CLI binary if it's not found in the system PATH or common installation locations.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nexus_claude::ClaudeCodeOptions;
    /// let options = ClaudeCodeOptions::builder()
    ///     .auto_download_cli(true)
    ///     .build();
    /// ```
    pub fn auto_download_cli(mut self, enable: bool) -> Self {
        self.options.auto_download_cli = enable;
        self
    }

    // ========== Environment & CLI Path ==========

    /// Set a single environment variable for the Claude Code subprocess
    ///
    /// The variable will be passed to the child process via `Command::env()`.
    /// Calling this multiple times accumulates entries. If the same key is set
    /// twice, the last value wins.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nexus_claude::ClaudeCodeOptions;
    /// let options = ClaudeCodeOptions::builder()
    ///     .env("PATH", "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin")
    ///     .env("RUST_LOG", "debug")
    ///     .build();
    /// ```
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.env.insert(key.into(), value.into());
        self
    }

    /// Set multiple environment variables for the Claude Code subprocess
    ///
    /// Merges the provided map into the existing environment variables.
    /// Existing keys are overwritten by the new values.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nexus_claude::ClaudeCodeOptions;
    /// # use std::collections::HashMap;
    /// let mut vars = HashMap::new();
    /// vars.insert("PATH".into(), "/usr/local/bin:/usr/bin".into());
    /// vars.insert("HOME".into(), "/home/user".into());
    /// let options = ClaudeCodeOptions::builder()
    ///     .envs(vars)
    ///     .build();
    /// ```
    pub fn envs(mut self, vars: HashMap<String, String>) -> Self {
        self.options.env.extend(vars);
        self
    }

    /// Set an explicit path to the Claude CLI binary
    ///
    /// When set, the SDK will use this path directly instead of searching
    /// via `find_claude_cli()`. Useful when the CLI is installed in a
    /// non-standard location or when you want to pin a specific binary.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nexus_claude::ClaudeCodeOptions;
    /// let options = ClaudeCodeOptions::builder()
    ///     .cli_path("/opt/homebrew/bin/claude")
    ///     .build();
    /// ```
    pub fn cli_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.options.cli_path = Some(path.into());
        self
    }

    // ========== Memory System Options ==========

    /// Enable persistent memory for cross-conversation context
    ///
    /// When enabled, the SDK will store and retrieve context across sessions.
    /// Requires the `memory` feature and a running Meilisearch instance.
    pub fn memory_enabled(mut self, enabled: bool) -> Self {
        self.options.memory_enabled = enabled;
        self
    }

    /// Set the minimum relevance score for context injection (0.0-1.0)
    ///
    /// Messages with scores below this threshold are not included.
    pub fn memory_threshold(mut self, threshold: f64) -> Self {
        self.options.memory_threshold = Some(threshold.clamp(0.0, 1.0));
        self
    }

    /// Set the maximum number of context items to inject per request
    pub fn max_context_items(mut self, max: usize) -> Self {
        self.options.max_context_items = Some(max);
        self
    }

    /// Set the token budget for injected context (~4 chars per token)
    pub fn memory_token_budget(mut self, budget: usize) -> Self {
        self.options.memory_token_budget = Some(budget);
        self
    }

    /// Build the options
    pub fn build(self) -> ClaudeCodeOptions {
        self.options
    }
}

/// Main message type enum
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Message {
    /// User message
    User {
        /// Message content
        message: UserMessage,
        /// Parent tool use ID — links this message to a parent Task tool call (sidechain).
        /// None = top-level message, Some(id) = message from a subagent execution.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
    },
    /// Assistant message
    Assistant {
        /// Message content
        message: AssistantMessage,
        /// Parent tool use ID — links this message to a parent Task tool call (sidechain).
        /// None = top-level message, Some(id) = message from a subagent execution.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
    },
    /// System message
    System {
        /// Subtype of system message
        subtype: String,
        /// Additional data
        data: serde_json::Value,
    },
    /// Result message indicating end of turn
    Result {
        /// Result subtype
        subtype: String,
        /// Duration in milliseconds
        duration_ms: i64,
        /// API duration in milliseconds
        duration_api_ms: i64,
        /// Whether an error occurred
        is_error: bool,
        /// Number of turns
        num_turns: i32,
        /// Session ID
        session_id: String,
        /// Total cost in USD
        #[serde(skip_serializing_if = "Option::is_none")]
        total_cost_usd: Option<f64>,
        /// Usage statistics
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<serde_json::Value>,
        /// Result message
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        /// Structured output (when output_format is set)
        /// Contains the validated JSON response matching the schema
        #[serde(skip_serializing_if = "Option::is_none", alias = "structuredOutput")]
        structured_output: Option<serde_json::Value>,
    },
    /// Stream event for real-time token streaming (requires --include-partial-messages)
    #[serde(rename = "stream_event")]
    StreamEvent {
        /// The streaming event data
        event: StreamEventData,
        /// Session ID
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Parent tool use ID — links this event to a parent Task tool call (sidechain).
        /// None = top-level event, Some(id) = event from a subagent execution.
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
    },
}

impl Message {
    /// Returns the parent_tool_use_id if this message is from a subagent sidechain.
    /// Returns None for top-level messages, System messages, and Result messages.
    pub fn parent_tool_use_id(&self) -> Option<&str> {
        match self {
            Message::User {
                parent_tool_use_id, ..
            } => parent_tool_use_id.as_deref(),
            Message::Assistant {
                parent_tool_use_id, ..
            } => parent_tool_use_id.as_deref(),
            Message::StreamEvent {
                parent_tool_use_id, ..
            } => parent_tool_use_id.as_deref(),
            Message::System { .. } | Message::Result { .. } => None,
        }
    }

    /// Returns true if this message is from a subagent sidechain (has a parent_tool_use_id).
    pub fn is_sidechain(&self) -> bool {
        self.parent_tool_use_id().is_some()
    }

    /// Returns true if this message is a top-level message (not from a subagent).
    pub fn is_top_level(&self) -> bool {
        !self.is_sidechain()
    }
}

/// Stream event data for real-time token streaming
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEventData {
    /// Message start event
    MessageStart {
        /// The initial message object
        message: serde_json::Value,
    },
    /// Content block start event
    ContentBlockStart {
        /// Block index
        index: usize,
        /// The content block being started
        content_block: serde_json::Value,
    },
    /// Content block delta event (contains the streaming token)
    ContentBlockDelta {
        /// Block index
        index: usize,
        /// The delta containing the token
        delta: StreamDelta,
    },
    /// Content block stop event
    ContentBlockStop {
        /// Block index
        index: usize,
    },
    /// Message delta event
    MessageDelta {
        /// The delta
        delta: serde_json::Value,
        /// Usage information
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<serde_json::Value>,
    },
    /// Message stop event
    MessageStop,
}

/// Delta in a content block stream event
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamDelta {
    /// Text delta containing a token
    TextDelta {
        /// The streamed text token
        text: String,
    },
    /// Thinking delta
    ThinkingDelta {
        /// The streamed thinking content
        thinking: String,
    },
    /// Input JSON delta (for tool use)
    InputJsonDelta {
        /// Partial JSON input
        partial_json: String,
    },
}

/// User message content.
///
/// The CLI emits two kinds of user messages:
/// 1. **Text prompts**: `{ "content": "Hello" }` — simple string content
/// 2. **Tool results**: `{ "content": [{ "type": "tool_result", ... }] }` — array of content blocks
///
/// The `content` field holds the text for simple messages (empty string for tool-result-only messages).
/// The `content_blocks` field holds the structured content blocks when present.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserMessage {
    /// Text content (empty for tool-result-only messages)
    pub content: String,
    /// Structured content blocks (tool_result, etc.) — present when the CLI
    /// sends a user message with array content (e.g. after executing a tool).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_blocks: Option<Vec<ContentBlock>>,
}

/// Assistant message content
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantMessage {
    /// Content blocks
    pub content: Vec<ContentBlock>,
}

/// Result message (re-export for convenience)  
pub use Message::Result as ResultMessage;
/// System message (re-export for convenience)
pub use Message::System as SystemMessage;

/// Content block types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ContentBlock {
    /// Text content
    Text(TextContent),
    /// Thinking content
    Thinking(ThinkingContent),
    /// Tool use request
    ToolUse(ToolUseContent),
    /// Tool result
    ToolResult(ToolResultContent),
}

/// Text content block
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextContent {
    /// Text content
    pub text: String,
}

/// Thinking content block
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThinkingContent {
    /// Thinking content
    pub thinking: String,
    /// Signature
    pub signature: String,
}

/// Tool use content block
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolUseContent {
    /// Tool use ID
    pub id: String,
    /// Tool name
    pub name: String,
    /// Tool input parameters
    pub input: serde_json::Value,
}

/// Tool result content block
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResultContent {
    /// Tool use ID this result corresponds to
    pub tool_use_id: String,
    /// Result content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentValue>,
    /// Whether this is an error result
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// Content value for tool results
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ContentValue {
    /// Text content
    Text(String),
    /// Structured content
    Structured(Vec<serde_json::Value>),
}

/// User content structure for internal use
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserContent {
    /// Role (always "user")
    pub role: String,
    /// Message content
    pub content: String,
}

/// Assistant content structure for internal use
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantContent {
    /// Role (always "assistant")
    pub role: String,
    /// Content blocks
    pub content: Vec<ContentBlock>,
}

/// SDK Control Protocol - Interrupt request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SDKControlInterruptRequest {
    /// Subtype
    pub subtype: String, // "interrupt"
}

/// SDK Control Protocol - Permission request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SDKControlPermissionRequest {
    /// Subtype
    pub subtype: String, // "can_use_tool"
    /// Tool name
    #[serde(alias = "toolName")]
    pub tool_name: String,
    /// Tool input
    pub input: serde_json::Value,
    /// Permission suggestions
    #[serde(
        skip_serializing_if = "Option::is_none",
        alias = "permissionSuggestions"
    )]
    pub permission_suggestions: Option<Vec<PermissionUpdate>>,
    /// Blocked path
    #[serde(skip_serializing_if = "Option::is_none", alias = "blockedPath")]
    pub blocked_path: Option<String>,
}

/// SDK Control Protocol - Initialize request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SDKControlInitializeRequest {
    /// Subtype
    pub subtype: String, // "initialize"
    /// Hooks configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HashMap<String, serde_json::Value>>,
}

/// SDK Control Protocol - Set permission mode request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SDKControlSetPermissionModeRequest {
    /// Subtype
    pub subtype: String, // "set_permission_mode"
    /// Permission mode
    pub mode: String,
}

/// SDK Control Protocol - Set model request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SDKControlSetModelRequest {
    /// Subtype
    pub subtype: String, // "set_model"
    /// Model to set (None to clear)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// SDK Hook callback request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SDKHookCallbackRequest {
    /// Subtype
    pub subtype: String, // "hook_callback"
    /// Callback ID
    #[serde(alias = "callbackId")]
    pub callback_id: String,
    /// Input data
    pub input: serde_json::Value,
    /// Tool use ID
    #[serde(skip_serializing_if = "Option::is_none", alias = "toolUseId")]
    pub tool_use_id: Option<String>,
}

/// SDK Control Protocol - MCP message request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SDKControlMcpMessageRequest {
    /// Subtype
    pub subtype: String, // "mcp_message"
    /// MCP server name
    #[serde(
        rename = "server_name",
        alias = "mcpServerName",
        alias = "mcp_server_name"
    )]
    pub mcp_server_name: String,
    /// Message to send
    pub message: serde_json::Value,
}

/// SDK Control Protocol - Rewind files request (Python SDK v0.1.14+)
///
/// Rewinds tracked files to their state at a specific user message.
/// Requires `enable_file_checkpointing` to be enabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SDKControlRewindFilesRequest {
    /// Subtype (always "rewind_files")
    pub subtype: String,
    /// UUID of the user message to rewind to
    #[serde(alias = "userMessageId")]
    pub user_message_id: String,
}

impl SDKControlRewindFilesRequest {
    /// Create a new rewind files request
    pub fn new(user_message_id: impl Into<String>) -> Self {
        Self {
            subtype: "rewind_files".to_string(),
            user_message_id: user_message_id.into(),
        }
    }
}

/// SDK Control Protocol request types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SDKControlRequest {
    /// Interrupt request
    #[serde(rename = "interrupt")]
    Interrupt(SDKControlInterruptRequest),
    /// Permission request
    #[serde(rename = "can_use_tool")]
    CanUseTool(SDKControlPermissionRequest),
    /// Initialize request
    #[serde(rename = "initialize")]
    Initialize(SDKControlInitializeRequest),
    /// Set permission mode
    #[serde(rename = "set_permission_mode")]
    SetPermissionMode(SDKControlSetPermissionModeRequest),
    /// Set model
    #[serde(rename = "set_model")]
    SetModel(SDKControlSetModelRequest),
    /// Hook callback
    #[serde(rename = "hook_callback")]
    HookCallback(SDKHookCallbackRequest),
    /// MCP message
    #[serde(rename = "mcp_message")]
    McpMessage(SDKControlMcpMessageRequest),
    /// Rewind files (Python SDK v0.1.14+)
    #[serde(rename = "rewind_files")]
    RewindFiles(SDKControlRewindFilesRequest),
}

/// Control request types (legacy, keeping for compatibility)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ControlRequest {
    /// Interrupt the current operation
    Interrupt {
        /// Request ID
        request_id: String,
    },
}

/// Control response types (legacy, keeping for compatibility)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ControlResponse {
    /// Interrupt acknowledged
    InterruptAck {
        /// Request ID
        request_id: String,
        /// Whether interrupt was successful
        success: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permission_mode_serialization() {
        let mode = PermissionMode::AcceptEdits;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, r#""acceptEdits""#);

        let deserialized: PermissionMode = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, mode);

        // Test Plan mode
        let plan_mode = PermissionMode::Plan;
        let plan_json = serde_json::to_string(&plan_mode).unwrap();
        assert_eq!(plan_json, r#""plan""#);

        let plan_deserialized: PermissionMode = serde_json::from_str(&plan_json).unwrap();
        assert_eq!(plan_deserialized, plan_mode);
    }

    #[test]
    fn test_message_serialization() {
        let msg = Message::User {
            message: UserMessage {
                content: "Hello".to_string(),
                content_blocks: None,
            },
            parent_tool_use_id: None,
        };

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"user""#));
        assert!(json.contains(r#""content":"Hello""#));

        let deserialized: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, msg);
    }

    #[test]
    #[allow(deprecated)]
    fn test_options_builder() {
        let options = ClaudeCodeOptions::builder()
            .system_prompt("Test prompt")
            .model("claude-3-opus")
            .permission_mode(PermissionMode::AcceptEdits)
            .allow_tool("read")
            .allow_tool("write")
            .max_turns(10)
            .build();

        assert_eq!(options.system_prompt, Some("Test prompt".to_string()));
        assert_eq!(options.model, Some("claude-3-opus".to_string()));
        assert_eq!(options.permission_mode, PermissionMode::AcceptEdits);
        assert_eq!(options.allowed_tools, vec!["read", "write"]);
        assert_eq!(options.max_turns, Some(10));
    }

    #[test]
    fn test_extra_args() {
        let mut extra_args = HashMap::new();
        extra_args.insert("custom-flag".to_string(), Some("value".to_string()));
        extra_args.insert("boolean-flag".to_string(), None);

        let options = ClaudeCodeOptions::builder()
            .extra_args(extra_args.clone())
            .add_extra_arg("another-flag", Some("another-value".to_string()))
            .build();

        assert_eq!(options.extra_args.len(), 3);
        assert_eq!(
            options.extra_args.get("custom-flag"),
            Some(&Some("value".to_string()))
        );
        assert_eq!(options.extra_args.get("boolean-flag"), Some(&None));
        assert_eq!(
            options.extra_args.get("another-flag"),
            Some(&Some("another-value".to_string()))
        );
    }

    #[test]
    fn test_thinking_content_serialization() {
        let thinking = ThinkingContent {
            thinking: "Let me think about this...".to_string(),
            signature: "sig123".to_string(),
        };

        let json = serde_json::to_string(&thinking).unwrap();
        assert!(json.contains(r#""thinking":"Let me think about this...""#));
        assert!(json.contains(r#""signature":"sig123""#));

        let deserialized: ThinkingContent = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.thinking, thinking.thinking);
        assert_eq!(deserialized.signature, thinking.signature);
    }

    // ============== v0.4.0 New Feature Tests ==============

    #[test]
    fn test_tools_config_list_serialization() {
        let tools = ToolsConfig::List(vec![
            "Read".to_string(),
            "Write".to_string(),
            "Bash".to_string(),
        ]);
        let json = serde_json::to_string(&tools).unwrap();

        // List variant serializes as JSON array
        assert!(json.contains("Read"));
        assert!(json.contains("Write"));
        assert!(json.contains("Bash"));

        let deserialized: ToolsConfig = serde_json::from_str(&json).unwrap();
        match deserialized {
            ToolsConfig::List(list) => {
                assert_eq!(list.len(), 3);
                assert!(list.contains(&"Read".to_string()));
            },
            _ => panic!("Expected List variant"),
        }
    }

    #[test]
    fn test_tools_config_preset_serialization() {
        // Test claude_code preset using the helper method
        let preset = ToolsConfig::claude_code_preset();
        let json = serde_json::to_string(&preset).unwrap();
        assert!(json.contains("preset"));
        assert!(json.contains("claude_code"));

        // Test Preset variant with custom values
        let custom_preset = ToolsConfig::Preset(ToolsPreset {
            preset_type: "preset".to_string(),
            preset: "custom".to_string(),
        });
        let json = serde_json::to_string(&custom_preset).unwrap();
        assert!(json.contains("custom"));

        // Test deserialization
        let deserialized: ToolsConfig = serde_json::from_str(&json).unwrap();
        match deserialized {
            ToolsConfig::Preset(p) => assert_eq!(p.preset, "custom"),
            _ => panic!("Expected Preset variant"),
        }
    }

    #[test]
    fn test_tools_config_helper_methods() {
        // Test list() helper
        let tools = ToolsConfig::list(vec!["Read".to_string(), "Write".to_string()]);
        match tools {
            ToolsConfig::List(list) => assert_eq!(list.len(), 2),
            _ => panic!("Expected List variant"),
        }

        // Test none() helper (empty list)
        let empty = ToolsConfig::none();
        match empty {
            ToolsConfig::List(list) => assert!(list.is_empty()),
            _ => panic!("Expected empty List variant"),
        }

        // Test claude_code_preset() helper
        let preset = ToolsConfig::claude_code_preset();
        match preset {
            ToolsConfig::Preset(p) => {
                assert_eq!(p.preset_type, "preset");
                assert_eq!(p.preset, "claude_code");
            },
            _ => panic!("Expected Preset variant"),
        }
    }

    #[test]
    fn test_sdk_beta_serialization() {
        let beta = SdkBeta::Context1M;
        let json = serde_json::to_string(&beta).unwrap();
        // The enum uses rename = "context-1m-2025-08-07"
        assert_eq!(json, r#""context-1m-2025-08-07""#);

        // Test Display trait
        let display = format!("{}", beta);
        assert_eq!(display, "context-1m-2025-08-07");

        // Test deserialization
        let deserialized: SdkBeta = serde_json::from_str(r#""context-1m-2025-08-07""#).unwrap();
        assert!(matches!(deserialized, SdkBeta::Context1M));
    }

    #[test]
    fn test_sandbox_settings_serialization() {
        let sandbox = SandboxSettings {
            enabled: Some(true),
            auto_allow_bash_if_sandboxed: Some(true),
            excluded_commands: Some(vec!["git".to_string(), "docker".to_string()]),
            allow_unsandboxed_commands: Some(false),
            network: Some(SandboxNetworkConfig {
                allow_unix_sockets: Some(vec!["/tmp/ssh-agent.sock".to_string()]),
                allow_all_unix_sockets: Some(false),
                allow_local_binding: Some(true),
                http_proxy_port: Some(8080),
                socks_proxy_port: Some(1080),
            }),
            ignore_violations: Some(SandboxIgnoreViolations {
                file: Some(vec!["/tmp".to_string(), "/var/log".to_string()]),
                network: Some(vec!["localhost".to_string()]),
            }),
            enable_weaker_nested_sandbox: Some(false),
        };

        let json = serde_json::to_string(&sandbox).unwrap();
        assert!(json.contains("enabled"));
        assert!(json.contains("autoAllowBashIfSandboxed")); // camelCase
        assert!(json.contains("excludedCommands"));
        assert!(json.contains("httpProxyPort"));
        assert!(json.contains("8080"));

        let deserialized: SandboxSettings = serde_json::from_str(&json).unwrap();
        assert!(deserialized.enabled.unwrap());
        assert!(deserialized.network.is_some());
        assert_eq!(
            deserialized.network.as_ref().unwrap().http_proxy_port,
            Some(8080)
        );
    }

    #[test]
    fn test_sandbox_network_config() {
        let config = SandboxNetworkConfig {
            allow_unix_sockets: Some(vec!["/run/user/1000/keyring/ssh".to_string()]),
            allow_all_unix_sockets: Some(false),
            allow_local_binding: Some(true),
            http_proxy_port: Some(3128),
            socks_proxy_port: Some(1080),
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: SandboxNetworkConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.http_proxy_port, Some(3128));
        assert_eq!(deserialized.socks_proxy_port, Some(1080));
        assert_eq!(deserialized.allow_local_binding, Some(true));
    }

    #[test]
    fn test_sandbox_ignore_violations() {
        let violations = SandboxIgnoreViolations {
            file: Some(vec!["/tmp".to_string(), "/var/cache".to_string()]),
            network: Some(vec!["127.0.0.1".to_string(), "localhost".to_string()]),
        };

        let json = serde_json::to_string(&violations).unwrap();
        assert!(json.contains("file"));
        assert!(json.contains("/tmp"));

        let deserialized: SandboxIgnoreViolations = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.file.as_ref().unwrap().len(), 2);
        assert_eq!(deserialized.network.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn test_sandbox_settings_default() {
        let sandbox = SandboxSettings::default();
        assert!(sandbox.enabled.is_none());
        assert!(sandbox.network.is_none());
        assert!(sandbox.ignore_violations.is_none());
    }

    #[test]
    fn test_sdk_plugin_config_serialization() {
        let plugin = SdkPluginConfig::Local {
            path: "/path/to/plugin".to_string(),
        };

        let json = serde_json::to_string(&plugin).unwrap();
        assert!(json.contains("local")); // lowercase due to rename_all
        assert!(json.contains("/path/to/plugin"));

        let deserialized: SdkPluginConfig = serde_json::from_str(&json).unwrap();
        match deserialized {
            SdkPluginConfig::Local { path } => {
                assert_eq!(path, "/path/to/plugin");
            },
        }
    }

    #[test]
    fn test_sdk_control_rewind_files_request() {
        let request = SDKControlRewindFilesRequest {
            subtype: "rewind_files".to_string(),
            user_message_id: "msg_12345".to_string(),
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("user_message_id"));
        assert!(json.contains("msg_12345"));
        assert!(json.contains("subtype"));
        assert!(json.contains("rewind_files"));

        let deserialized: SDKControlRewindFilesRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.user_message_id, "msg_12345");
        assert_eq!(deserialized.subtype, "rewind_files");
    }

    #[test]
    fn test_options_builder_with_new_fields() {
        let options = ClaudeCodeOptions::builder()
            .tools(ToolsConfig::claude_code_preset())
            .add_beta(SdkBeta::Context1M)
            .max_budget_usd(10.0)
            .fallback_model("claude-3-haiku")
            .output_format(serde_json::json!({"type": "object"}))
            .enable_file_checkpointing(true)
            .sandbox(SandboxSettings::default())
            .add_plugin(SdkPluginConfig::Local {
                path: "/plugin".to_string(),
            })
            .auto_download_cli(true)
            .build();

        // Verify tools
        assert!(options.tools.is_some());
        match options.tools.as_ref().unwrap() {
            ToolsConfig::Preset(preset) => assert_eq!(preset.preset, "claude_code"),
            _ => panic!("Expected Preset variant"),
        }

        // Verify betas
        assert_eq!(options.betas.len(), 1);
        assert!(matches!(options.betas[0], SdkBeta::Context1M));

        // Verify max_budget_usd
        assert_eq!(options.max_budget_usd, Some(10.0));

        // Verify fallback_model
        assert_eq!(options.fallback_model, Some("claude-3-haiku".to_string()));

        // Verify output_format
        assert!(options.output_format.is_some());

        // Verify enable_file_checkpointing
        assert!(options.enable_file_checkpointing);

        // Verify sandbox
        assert!(options.sandbox.is_some());

        // Verify plugins
        assert_eq!(options.plugins.len(), 1);

        // Verify auto_download_cli
        assert!(options.auto_download_cli);
    }

    #[test]
    fn test_options_builder_with_tools_list() {
        let options = ClaudeCodeOptions::builder()
            .tools(ToolsConfig::List(vec![
                "Read".to_string(),
                "Bash".to_string(),
            ]))
            .build();

        match options.tools.as_ref().unwrap() {
            ToolsConfig::List(list) => {
                assert_eq!(list.len(), 2);
                assert!(list.contains(&"Read".to_string()));
                assert!(list.contains(&"Bash".to_string()));
            },
            _ => panic!("Expected List variant"),
        }
    }

    #[test]
    fn test_options_builder_multiple_betas() {
        let options = ClaudeCodeOptions::builder()
            .add_beta(SdkBeta::Context1M)
            .betas(vec![SdkBeta::Context1M])
            .build();

        // betas() replaces, add_beta() appends - so only 1 from betas()
        assert_eq!(options.betas.len(), 1);
    }

    #[test]
    fn test_options_builder_add_beta_accumulates() {
        let options = ClaudeCodeOptions::builder()
            .add_beta(SdkBeta::Context1M)
            .add_beta(SdkBeta::Context1M)
            .build();

        // add_beta() accumulates
        assert_eq!(options.betas.len(), 2);
    }

    #[test]
    fn test_options_builder_multiple_plugins() {
        let options = ClaudeCodeOptions::builder()
            .add_plugin(SdkPluginConfig::Local {
                path: "/plugin1".to_string(),
            })
            .add_plugin(SdkPluginConfig::Local {
                path: "/plugin2".to_string(),
            })
            .plugins(vec![SdkPluginConfig::Local {
                path: "/plugin3".to_string(),
            }])
            .build();

        // plugins() replaces previous, so only 1
        assert_eq!(options.plugins.len(), 1);
    }

    #[test]
    fn test_options_builder_add_plugin_accumulates() {
        let options = ClaudeCodeOptions::builder()
            .add_plugin(SdkPluginConfig::Local {
                path: "/plugin1".to_string(),
            })
            .add_plugin(SdkPluginConfig::Local {
                path: "/plugin2".to_string(),
            })
            .add_plugin(SdkPluginConfig::Local {
                path: "/plugin3".to_string(),
            })
            .build();

        // add_plugin() accumulates
        assert_eq!(options.plugins.len(), 3);
    }

    #[test]
    fn test_message_result_with_structured_output() {
        // Test parsing result message with structured_output (snake_case)
        let json = r#"{
            "type": "result",
            "subtype": "success",
            "cost_usd": 0.05,
            "duration_ms": 1500,
            "duration_api_ms": 1200,
            "is_error": false,
            "num_turns": 3,
            "session_id": "session_123",
            "structured_output": {"answer": 42}
        }"#;

        let msg: Message = serde_json::from_str(json).unwrap();
        match msg {
            Message::Result {
                structured_output, ..
            } => {
                assert!(structured_output.is_some());
                let output = structured_output.unwrap();
                assert_eq!(output["answer"], 42);
            },
            _ => panic!("Expected Result message"),
        }
    }

    #[test]
    fn test_message_result_with_structured_output_camel_case() {
        // Test parsing result message with structuredOutput (camelCase alias)
        let json = r#"{
            "type": "result",
            "subtype": "success",
            "cost_usd": 0.05,
            "duration_ms": 1500,
            "duration_api_ms": 1200,
            "is_error": false,
            "num_turns": 3,
            "session_id": "session_123",
            "structuredOutput": {"name": "test", "value": true}
        }"#;

        let msg: Message = serde_json::from_str(json).unwrap();
        match msg {
            Message::Result {
                structured_output, ..
            } => {
                assert!(structured_output.is_some());
                let output = structured_output.unwrap();
                assert_eq!(output["name"], "test");
                assert_eq!(output["value"], true);
            },
            _ => panic!("Expected Result message"),
        }
    }

    #[test]
    fn test_default_options_new_fields() {
        let options = ClaudeCodeOptions::default();

        // Verify defaults for new fields
        assert!(options.tools.is_none());
        assert!(options.betas.is_empty());
        assert!(options.max_budget_usd.is_none());
        assert!(options.fallback_model.is_none());
        assert!(options.output_format.is_none());
        assert!(!options.enable_file_checkpointing);
        assert!(options.sandbox.is_none());
        assert!(options.plugins.is_empty());
        assert!(options.user.is_none());
        // Note: auto_download_cli defaults to false (Rust bool default)
        // Users should explicitly enable it with .auto_download_cli(true)
        assert!(!options.auto_download_cli);
        assert!(options.cli_path.is_none());
    }

    #[test]
    fn test_builder_env_single() {
        let options = ClaudeCodeOptions::builder()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .build();

        assert_eq!(options.env.len(), 1);
        assert_eq!(
            options.env.get("PATH"),
            Some(&"/usr/local/bin:/usr/bin:/bin".to_string())
        );
    }

    #[test]
    fn test_builder_env_multiple() {
        let options = ClaudeCodeOptions::builder()
            .env("PATH", "/usr/local/bin:/usr/bin")
            .env("RUST_LOG", "debug")
            .env("HOME", "/home/user")
            .build();

        assert_eq!(options.env.len(), 3);
        assert_eq!(
            options.env.get("PATH"),
            Some(&"/usr/local/bin:/usr/bin".to_string())
        );
        assert_eq!(options.env.get("RUST_LOG"), Some(&"debug".to_string()));
        assert_eq!(options.env.get("HOME"), Some(&"/home/user".to_string()));
    }

    #[test]
    fn test_builder_env_overwrite() {
        let options = ClaudeCodeOptions::builder()
            .env("PATH", "/first")
            .env("PATH", "/second")
            .build();

        assert_eq!(options.env.len(), 1);
        assert_eq!(options.env.get("PATH"), Some(&"/second".to_string()));
    }

    #[test]
    fn test_builder_envs_hashmap() {
        let mut vars = HashMap::new();
        vars.insert("PATH".into(), "/usr/bin".into());
        vars.insert("HOME".into(), "/home/user".into());

        let options = ClaudeCodeOptions::builder()
            .env("EXISTING", "value")
            .envs(vars)
            .build();

        assert_eq!(options.env.len(), 3);
        assert_eq!(options.env.get("EXISTING"), Some(&"value".to_string()));
        assert_eq!(options.env.get("PATH"), Some(&"/usr/bin".to_string()));
        assert_eq!(options.env.get("HOME"), Some(&"/home/user".to_string()));
    }

    #[test]
    fn test_builder_cli_path() {
        let options = ClaudeCodeOptions::builder()
            .cli_path("/opt/homebrew/bin/claude")
            .build();

        assert_eq!(
            options.cli_path,
            Some(PathBuf::from("/opt/homebrew/bin/claude"))
        );
    }

    #[test]
    fn test_builder_cli_path_default_none() {
        let options = ClaudeCodeOptions::builder().build();
        assert!(options.cli_path.is_none());
    }

    // ============== Coverage expansion tests ==============

    // --- PermissionMode: all variants ---
    #[test]
    fn test_permission_mode_all_variants_serde() {
        let cases = vec![
            (PermissionMode::Default, r#""default""#),
            (PermissionMode::AcceptEdits, r#""acceptEdits""#),
            (PermissionMode::Plan, r#""plan""#),
            (PermissionMode::BypassPermissions, r#""bypassPermissions""#),
        ];
        for (variant, expected_json) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_json, "serialize {:?}", variant);
            let round: PermissionMode = serde_json::from_str(&json).unwrap();
            assert_eq!(round, variant, "round-trip {:?}", variant);
        }
    }

    #[test]
    fn test_permission_mode_default_trait() {
        assert_eq!(PermissionMode::default(), PermissionMode::Default);
    }

    // --- SdkBeta: Display + round-trip ---
    #[test]
    fn test_sdk_beta_display_and_roundtrip() {
        let beta = SdkBeta::Context1M;
        assert_eq!(beta.to_string(), "context-1m-2025-08-07");
        let json = serde_json::to_value(&beta).unwrap();
        assert_eq!(json, serde_json::json!("context-1m-2025-08-07"));
        let back: SdkBeta = serde_json::from_value(json).unwrap();
        assert_eq!(back, SdkBeta::Context1M);
    }

    // --- ToolsConfig: serde round-trip for both variants ---
    #[test]
    fn test_tools_config_serde_roundtrip() {
        // List variant
        let list = ToolsConfig::list(vec!["Read".into(), "Edit".into(), "Bash".into()]);
        let val = serde_json::to_value(&list).unwrap();
        assert_eq!(val, serde_json::json!(["Read", "Edit", "Bash"]));
        let back: ToolsConfig = serde_json::from_value(val).unwrap();
        match back {
            ToolsConfig::List(v) => assert_eq!(v, vec!["Read", "Edit", "Bash"]),
            _ => panic!("expected List"),
        }

        // none()
        let none = ToolsConfig::none();
        let val = serde_json::to_value(&none).unwrap();
        assert_eq!(val, serde_json::json!([]));

        // Preset variant
        let preset = ToolsConfig::claude_code_preset();
        let val = serde_json::to_value(&preset).unwrap();
        assert_eq!(
            val,
            serde_json::json!({"type": "preset", "preset": "claude_code"})
        );
        let back: ToolsConfig = serde_json::from_value(val).unwrap();
        match back {
            ToolsConfig::Preset(p) => {
                assert_eq!(p.preset_type, "preset");
                assert_eq!(p.preset, "claude_code");
            },
            _ => panic!("expected Preset"),
        }
    }

    // --- SandboxNetworkConfig: Default ---
    #[test]
    fn test_sandbox_network_config_default() {
        let cfg = SandboxNetworkConfig::default();
        assert!(cfg.allow_unix_sockets.is_none());
        assert!(cfg.allow_all_unix_sockets.is_none());
        assert!(cfg.allow_local_binding.is_none());
        assert!(cfg.http_proxy_port.is_none());
        assert!(cfg.socks_proxy_port.is_none());

        // default serializes to empty object (skip_serializing_if)
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json, serde_json::json!({}));
    }

    // --- SandboxSettings: Default + full serde ---
    #[test]
    fn test_sandbox_settings_default_serde() {
        let s = SandboxSettings::default();
        let val = serde_json::to_value(&s).unwrap();
        assert_eq!(val, serde_json::json!({}));
        let back: SandboxSettings = serde_json::from_value(val).unwrap();
        assert!(back.enabled.is_none());
        assert!(back.network.is_none());
        assert!(back.ignore_violations.is_none());
        assert!(back.enable_weaker_nested_sandbox.is_none());
    }

    #[test]
    fn test_sandbox_settings_full_serde_roundtrip() {
        let val = serde_json::json!({
            "enabled": true,
            "autoAllowBashIfSandboxed": false,
            "excludedCommands": ["git"],
            "allowUnsandboxedCommands": true,
            "network": {
                "allowUnixSockets": ["/tmp/sock"],
                "allowAllUnixSockets": true,
                "allowLocalBinding": false,
                "httpProxyPort": 9090,
                "socksProxyPort": 1081
            },
            "ignoreViolations": {
                "file": ["/tmp"],
                "network": ["example.com"]
            },
            "enableWeakerNestedSandbox": true
        });
        let s: SandboxSettings = serde_json::from_value(val.clone()).unwrap();
        assert_eq!(s.enabled, Some(true));
        assert_eq!(s.auto_allow_bash_if_sandboxed, Some(false));
        assert_eq!(
            s.excluded_commands.as_ref().unwrap(),
            &vec!["git".to_string()]
        );
        assert_eq!(s.allow_unsandboxed_commands, Some(true));
        assert_eq!(s.enable_weaker_nested_sandbox, Some(true));
        let net = s.network.as_ref().unwrap();
        assert_eq!(net.http_proxy_port, Some(9090));
        assert_eq!(net.socks_proxy_port, Some(1081));
        assert_eq!(net.allow_all_unix_sockets, Some(true));
        assert_eq!(net.allow_local_binding, Some(false));
        let ig = s.ignore_violations.as_ref().unwrap();
        assert_eq!(ig.file.as_ref().unwrap(), &vec!["/tmp".to_string()]);
        assert_eq!(
            ig.network.as_ref().unwrap(),
            &vec!["example.com".to_string()]
        );

        // re-serialize and verify round-trip
        let back = serde_json::to_value(&s).unwrap();
        assert_eq!(back, val);
    }

    // --- SdkPluginConfig::Local serde ---
    #[test]
    fn test_sdk_plugin_config_local_roundtrip() {
        let plugin = SdkPluginConfig::Local {
            path: "/home/user/my-plugin".to_string(),
        };
        let val = serde_json::to_value(&plugin).unwrap();
        assert_eq!(
            val,
            serde_json::json!({"type": "local", "path": "/home/user/my-plugin"})
        );
        let back: SdkPluginConfig = serde_json::from_value(val).unwrap();
        match back {
            SdkPluginConfig::Local { path } => assert_eq!(path, "/home/user/my-plugin"),
        }
    }

    // --- ControlProtocolFormat: Default + equality ---
    #[test]
    fn test_control_protocol_format_default_and_eq() {
        let d = ControlProtocolFormat::default();
        assert_eq!(d, ControlProtocolFormat::Legacy);
        assert_ne!(d, ControlProtocolFormat::Control);
        assert_ne!(d, ControlProtocolFormat::Auto);
        assert_eq!(
            ControlProtocolFormat::Control,
            ControlProtocolFormat::Control
        );
        assert_eq!(ControlProtocolFormat::Auto, ControlProtocolFormat::Auto);
    }

    // --- McpServerConfig: Debug for all variants ---
    #[test]
    fn test_mcp_server_config_debug_stdio() {
        let cfg = McpServerConfig::Stdio {
            command: "node".into(),
            args: Some(vec!["server.js".into()]),
            env: Some({
                let mut m = HashMap::new();
                m.insert("PORT".into(), "3000".into());
                m
            }),
        };
        let dbg = format!("{:?}", cfg);
        assert!(dbg.contains("Stdio"));
        assert!(dbg.contains("node"));
        assert!(dbg.contains("server.js"));
        assert!(dbg.contains("PORT"));
    }

    #[test]
    fn test_mcp_server_config_debug_sse() {
        let cfg = McpServerConfig::Sse {
            url: "https://example.com/sse".into(),
            headers: Some({
                let mut m = HashMap::new();
                m.insert("Authorization".into(), "Bearer tok".into());
                m
            }),
        };
        let dbg = format!("{:?}", cfg);
        assert!(dbg.contains("Sse"));
        assert!(dbg.contains("https://example.com/sse"));
        assert!(dbg.contains("Authorization"));
    }

    #[test]
    fn test_mcp_server_config_debug_http() {
        let cfg = McpServerConfig::Http {
            url: "https://example.com/http".into(),
            headers: None,
        };
        let dbg = format!("{:?}", cfg);
        assert!(dbg.contains("Http"));
        assert!(dbg.contains("https://example.com/http"));
    }

    #[test]
    fn test_mcp_server_config_debug_sdk() {
        let cfg = McpServerConfig::Sdk {
            name: "my-server".into(),
            instance: Arc::new(42_u32),
        };
        let dbg = format!("{:?}", cfg);
        assert!(dbg.contains("Sdk"));
        assert!(dbg.contains("my-server"));
        assert!(dbg.contains("<Arc<dyn Any>>"));
    }

    // --- McpServerConfig: Serialize all variants ---
    #[test]
    fn test_mcp_server_config_serialize_stdio() {
        let cfg = McpServerConfig::Stdio {
            command: "npx".into(),
            args: Some(vec!["-y".into(), "server".into()]),
            env: Some({
                let mut m = HashMap::new();
                m.insert("KEY".into(), "VAL".into());
                m
            }),
        };
        let val = serde_json::to_value(&cfg).unwrap();
        assert_eq!(val["type"], "stdio");
        assert_eq!(val["command"], "npx");
        assert_eq!(val["args"], serde_json::json!(["-y", "server"]));
        assert_eq!(val["env"]["KEY"], "VAL");
    }

    #[test]
    fn test_mcp_server_config_serialize_stdio_no_optionals() {
        let cfg = McpServerConfig::Stdio {
            command: "cmd".into(),
            args: None,
            env: None,
        };
        let val = serde_json::to_value(&cfg).unwrap();
        assert_eq!(val["type"], "stdio");
        assert_eq!(val["command"], "cmd");
        assert!(val.get("args").is_none());
        assert!(val.get("env").is_none());
    }

    #[test]
    fn test_mcp_server_config_serialize_sse() {
        let cfg = McpServerConfig::Sse {
            url: "https://sse.example.com".into(),
            headers: Some({
                let mut m = HashMap::new();
                m.insert("X-Custom".into(), "val".into());
                m
            }),
        };
        let val = serde_json::to_value(&cfg).unwrap();
        assert_eq!(val["type"], "sse");
        assert_eq!(val["url"], "https://sse.example.com");
        assert_eq!(val["headers"]["X-Custom"], "val");
    }

    #[test]
    fn test_mcp_server_config_serialize_sse_no_headers() {
        let cfg = McpServerConfig::Sse {
            url: "https://sse.example.com".into(),
            headers: None,
        };
        let val = serde_json::to_value(&cfg).unwrap();
        assert_eq!(val["type"], "sse");
        assert!(val.get("headers").is_none());
    }

    #[test]
    fn test_mcp_server_config_serialize_http() {
        let cfg = McpServerConfig::Http {
            url: "https://http.example.com".into(),
            headers: Some({
                let mut m = HashMap::new();
                m.insert("Auth".into(), "Bearer x".into());
                m
            }),
        };
        let val = serde_json::to_value(&cfg).unwrap();
        assert_eq!(val["type"], "http");
        assert_eq!(val["url"], "https://http.example.com");
        assert_eq!(val["headers"]["Auth"], "Bearer x");
    }

    #[test]
    fn test_mcp_server_config_serialize_http_no_headers() {
        let cfg = McpServerConfig::Http {
            url: "https://http.example.com".into(),
            headers: None,
        };
        let val = serde_json::to_value(&cfg).unwrap();
        assert_eq!(val["type"], "http");
        assert!(val.get("headers").is_none());
    }

    #[test]
    fn test_mcp_server_config_serialize_sdk() {
        let cfg = McpServerConfig::Sdk {
            name: "sdk-srv".into(),
            instance: Arc::new("opaque"),
        };
        let val = serde_json::to_value(&cfg).unwrap();
        assert_eq!(val["type"], "sdk");
        assert_eq!(val["name"], "sdk-srv");
        // instance is not serialized (no way to serialize Arc<dyn Any>)
        assert!(val.get("instance").is_none());
    }

    // --- McpServerConfig: Deserialize Stdio/Sse/Http ---
    #[test]
    fn test_mcp_server_config_deserialize_stdio() {
        let val = serde_json::json!({
            "type": "stdio",
            "command": "node",
            "args": ["index.js"],
            "env": {"NODE_ENV": "production"}
        });
        let cfg: McpServerConfig = serde_json::from_value(val).unwrap();
        match cfg {
            McpServerConfig::Stdio { command, args, env } => {
                assert_eq!(command, "node");
                assert_eq!(args.unwrap(), vec!["index.js".to_string()]);
                assert_eq!(env.unwrap().get("NODE_ENV").unwrap(), "production");
            },
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn test_mcp_server_config_deserialize_sse() {
        let val = serde_json::json!({
            "type": "sse",
            "url": "https://sse.test.com",
            "headers": {"X-Key": "abc"}
        });
        let cfg: McpServerConfig = serde_json::from_value(val).unwrap();
        match cfg {
            McpServerConfig::Sse { url, headers } => {
                assert_eq!(url, "https://sse.test.com");
                assert_eq!(headers.unwrap().get("X-Key").unwrap(), "abc");
            },
            _ => panic!("expected Sse"),
        }
    }

    #[test]
    fn test_mcp_server_config_deserialize_http() {
        let val = serde_json::json!({
            "type": "http",
            "url": "https://http.test.com"
        });
        let cfg: McpServerConfig = serde_json::from_value(val).unwrap();
        match cfg {
            McpServerConfig::Http { url, headers } => {
                assert_eq!(url, "https://http.test.com");
                assert!(headers.is_none());
            },
            _ => panic!("expected Http"),
        }
    }

    // --- PermissionUpdateDestination: all variants ---
    #[test]
    fn test_permission_update_destination_serde() {
        let cases = vec![
            (PermissionUpdateDestination::UserSettings, "userSettings"),
            (
                PermissionUpdateDestination::ProjectSettings,
                "projectSettings",
            ),
            (PermissionUpdateDestination::LocalSettings, "localSettings"),
            (PermissionUpdateDestination::Session, "session"),
        ];
        for (variant, expected) in cases {
            let val = serde_json::to_value(&variant).unwrap();
            assert_eq!(val, serde_json::json!(expected), "serialize {:?}", variant);
            let back: PermissionUpdateDestination = serde_json::from_value(val).unwrap();
            assert_eq!(back, variant);
        }
    }

    // --- PermissionBehavior: all variants ---
    #[test]
    fn test_permission_behavior_serde() {
        let cases = vec![
            (PermissionBehavior::Allow, "allow"),
            (PermissionBehavior::Deny, "deny"),
            (PermissionBehavior::Ask, "ask"),
        ];
        for (variant, expected) in cases {
            let val = serde_json::to_value(&variant).unwrap();
            assert_eq!(val, serde_json::json!(expected), "serialize {:?}", variant);
            let back: PermissionBehavior = serde_json::from_value(val).unwrap();
            assert_eq!(back, variant);
        }
    }

    // --- PermissionUpdateType: all variants ---
    #[test]
    fn test_permission_update_type_serde() {
        let cases = vec![
            (PermissionUpdateType::AddRules, "addRules"),
            (PermissionUpdateType::ReplaceRules, "replaceRules"),
            (PermissionUpdateType::RemoveRules, "removeRules"),
            (PermissionUpdateType::SetMode, "setMode"),
            (PermissionUpdateType::AddDirectories, "addDirectories"),
            (PermissionUpdateType::RemoveDirectories, "removeDirectories"),
        ];
        for (variant, expected) in cases {
            let val = serde_json::to_value(&variant).unwrap();
            assert_eq!(val, serde_json::json!(expected), "serialize {:?}", variant);
            let back: PermissionUpdateType = serde_json::from_value(val).unwrap();
            assert_eq!(back, variant);
        }
    }

    // --- HookInput: serde round-trip for all variants ---
    #[test]
    fn test_hook_input_pre_tool_use_serde() {
        let input = HookInput::PreToolUse(PreToolUseHookInput {
            session_id: "s1".into(),
            transcript_path: "/tmp/t.json".into(),
            cwd: "/home".into(),
            permission_mode: Some("default".into()),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
        });
        let val = serde_json::to_value(&input).unwrap();
        assert_eq!(val["hook_event_name"], "PreToolUse");
        assert_eq!(val["tool_name"], "Bash");
        assert_eq!(val["tool_input"]["command"], "ls");
        let back: HookInput = serde_json::from_value(val).unwrap();
        match back {
            HookInput::PreToolUse(p) => {
                assert_eq!(p.tool_name, "Bash");
                assert_eq!(p.session_id, "s1");
            },
            _ => panic!("expected PreToolUse"),
        }
    }

    #[test]
    fn test_hook_input_post_tool_use_serde() {
        let input = HookInput::PostToolUse(PostToolUseHookInput {
            session_id: "s2".into(),
            transcript_path: "/tmp/t2.json".into(),
            cwd: "/work".into(),
            permission_mode: None,
            tool_name: "Read".into(),
            tool_input: serde_json::json!({"path": "/etc/hosts"}),
            tool_response: serde_json::json!({"content": "127.0.0.1 localhost"}),
        });
        let val = serde_json::to_value(&input).unwrap();
        assert_eq!(val["hook_event_name"], "PostToolUse");
        assert_eq!(val["tool_response"]["content"], "127.0.0.1 localhost");
        let back: HookInput = serde_json::from_value(val).unwrap();
        match back {
            HookInput::PostToolUse(p) => assert_eq!(p.tool_name, "Read"),
            _ => panic!("expected PostToolUse"),
        }
    }

    #[test]
    fn test_hook_input_user_prompt_submit_serde() {
        let input = HookInput::UserPromptSubmit(UserPromptSubmitHookInput {
            session_id: "s3".into(),
            transcript_path: "/tmp/t3.json".into(),
            cwd: "/proj".into(),
            permission_mode: Some("plan".into()),
            prompt: "fix the bug".into(),
        });
        let val = serde_json::to_value(&input).unwrap();
        assert_eq!(val["hook_event_name"], "UserPromptSubmit");
        assert_eq!(val["prompt"], "fix the bug");
        let back: HookInput = serde_json::from_value(val).unwrap();
        match back {
            HookInput::UserPromptSubmit(p) => assert_eq!(p.prompt, "fix the bug"),
            _ => panic!("expected UserPromptSubmit"),
        }
    }

    #[test]
    fn test_hook_input_stop_serde() {
        let input = HookInput::Stop(StopHookInput {
            session_id: "s4".into(),
            transcript_path: "/tmp/t4.json".into(),
            cwd: "/".into(),
            permission_mode: None,
            stop_hook_active: true,
        });
        let val = serde_json::to_value(&input).unwrap();
        assert_eq!(val["hook_event_name"], "Stop");
        assert_eq!(val["stop_hook_active"], true);
        let back: HookInput = serde_json::from_value(val).unwrap();
        match back {
            HookInput::Stop(s) => assert!(s.stop_hook_active),
            _ => panic!("expected Stop"),
        }
    }

    #[test]
    fn test_hook_input_subagent_stop_serde() {
        let input = HookInput::SubagentStop(SubagentStopHookInput {
            session_id: "s5".into(),
            transcript_path: "/tmp/t5.json".into(),
            cwd: "/sub".into(),
            permission_mode: None,
            stop_hook_active: false,
        });
        let val = serde_json::to_value(&input).unwrap();
        assert_eq!(val["hook_event_name"], "SubagentStop");
        assert_eq!(val["stop_hook_active"], false);
        let back: HookInput = serde_json::from_value(val).unwrap();
        match back {
            HookInput::SubagentStop(s) => assert!(!s.stop_hook_active),
            _ => panic!("expected SubagentStop"),
        }
    }

    #[test]
    fn test_hook_input_pre_compact_serde() {
        let input = HookInput::PreCompact(PreCompactHookInput {
            session_id: "s6".into(),
            transcript_path: "/tmp/t6.json".into(),
            cwd: "/compact".into(),
            permission_mode: None,
            trigger: "auto".into(),
            custom_instructions: Some("keep tool calls".into()),
        });
        let val = serde_json::to_value(&input).unwrap();
        assert_eq!(val["hook_event_name"], "PreCompact");
        assert_eq!(val["trigger"], "auto");
        assert_eq!(val["custom_instructions"], "keep tool calls");
        let back: HookInput = serde_json::from_value(val).unwrap();
        match back {
            HookInput::PreCompact(p) => {
                assert_eq!(p.trigger, "auto");
                assert_eq!(p.custom_instructions.unwrap(), "keep tool calls");
            },
            _ => panic!("expected PreCompact"),
        }
    }

    #[test]
    fn test_hook_input_pre_compact_no_custom_instructions() {
        let input = HookInput::PreCompact(PreCompactHookInput {
            session_id: "s7".into(),
            transcript_path: "/tmp/t7.json".into(),
            cwd: "/".into(),
            permission_mode: None,
            trigger: "manual".into(),
            custom_instructions: None,
        });
        let val = serde_json::to_value(&input).unwrap();
        assert!(val.get("custom_instructions").is_none());
        let back: HookInput = serde_json::from_value(val).unwrap();
        match back {
            HookInput::PreCompact(p) => assert!(p.custom_instructions.is_none()),
            _ => panic!("expected PreCompact"),
        }
    }

    // --- HookJSONOutput: Async and Sync variants ---
    #[test]
    fn test_hook_json_output_async_serde() {
        let output = HookJSONOutput::Async(AsyncHookJSONOutput {
            async_: true,
            async_timeout: Some(5000),
        });
        let val = serde_json::to_value(&output).unwrap();
        assert_eq!(val["async"], true);
        assert_eq!(val["asyncTimeout"], 5000);
        let back: HookJSONOutput = serde_json::from_value(val).unwrap();
        match back {
            // untagged enum: async field present -> Async variant tried first
            HookJSONOutput::Async(a) => {
                assert!(a.async_);
                assert_eq!(a.async_timeout, Some(5000));
            },
            _ => panic!("expected Async"),
        }
    }

    #[test]
    fn test_hook_json_output_sync_serde() {
        let output = HookJSONOutput::Sync(SyncHookJSONOutput {
            continue_: Some(false),
            suppress_output: Some(true),
            stop_reason: Some("hook blocked".into()),
            decision: Some("block".into()),
            system_message: Some("Blocked by policy".into()),
            reason: Some("security".into()),
            hook_specific_output: None,
        });
        let val = serde_json::to_value(&output).unwrap();
        assert_eq!(val["continue"], false);
        assert_eq!(val["suppressOutput"], true);
        assert_eq!(val["stopReason"], "hook blocked");
        assert_eq!(val["decision"], "block");
        assert_eq!(val["systemMessage"], "Blocked by policy");
        assert_eq!(val["reason"], "security");
        let back: HookJSONOutput = serde_json::from_value(val).unwrap();
        match back {
            HookJSONOutput::Sync(s) => {
                assert_eq!(s.continue_, Some(false));
                assert_eq!(s.decision.as_deref(), Some("block"));
            },
            _ => panic!("expected Sync"),
        }
    }

    #[test]
    fn test_hook_json_output_sync_default() {
        let output = SyncHookJSONOutput::default();
        assert!(output.continue_.is_none());
        assert!(output.suppress_output.is_none());
        assert!(output.stop_reason.is_none());
        assert!(output.decision.is_none());
        assert!(output.system_message.is_none());
        assert!(output.reason.is_none());
        assert!(output.hook_specific_output.is_none());
    }

    // --- HookSpecificOutput: all variants ---
    #[test]
    fn test_hook_specific_output_pre_tool_use_serde() {
        let out = HookSpecificOutput::PreToolUse(PreToolUseHookSpecificOutput {
            permission_decision: Some("deny".into()),
            permission_decision_reason: Some("not allowed".into()),
            updated_input: Some(serde_json::json!({"command": "echo hi"})),
            additional_context: Some("extra info".into()),
        });
        let val = serde_json::to_value(&out).unwrap();
        assert_eq!(val["hookEventName"], "PreToolUse");
        assert_eq!(val["permissionDecision"], "deny");
        assert_eq!(val["permissionDecisionReason"], "not allowed");
        assert_eq!(val["updatedInput"]["command"], "echo hi");
        assert_eq!(val["additionalContext"], "extra info");
        let back: HookSpecificOutput = serde_json::from_value(val).unwrap();
        match back {
            HookSpecificOutput::PreToolUse(p) => {
                assert_eq!(p.permission_decision.as_deref(), Some("deny"));
            },
            _ => panic!("expected PreToolUse"),
        }
    }

    #[test]
    fn test_hook_specific_output_post_tool_use_serde() {
        let out = HookSpecificOutput::PostToolUse(PostToolUseHookSpecificOutput {
            additional_context: Some("post context".into()),
        });
        let val = serde_json::to_value(&out).unwrap();
        assert_eq!(val["hookEventName"], "PostToolUse");
        assert_eq!(val["additionalContext"], "post context");
        let back: HookSpecificOutput = serde_json::from_value(val).unwrap();
        match back {
            HookSpecificOutput::PostToolUse(p) => {
                assert_eq!(p.additional_context.as_deref(), Some("post context"));
            },
            _ => panic!("expected PostToolUse"),
        }
    }

    #[test]
    fn test_hook_specific_output_user_prompt_submit_serde() {
        let out = HookSpecificOutput::UserPromptSubmit(UserPromptSubmitHookSpecificOutput {
            additional_context: None,
        });
        let val = serde_json::to_value(&out).unwrap();
        assert_eq!(val["hookEventName"], "UserPromptSubmit");
        assert!(val.get("additionalContext").is_none());
        let back: HookSpecificOutput = serde_json::from_value(val).unwrap();
        match back {
            HookSpecificOutput::UserPromptSubmit(p) => assert!(p.additional_context.is_none()),
            _ => panic!("expected UserPromptSubmit"),
        }
    }

    #[test]
    fn test_hook_specific_output_session_start_serde() {
        let out = HookSpecificOutput::SessionStart(SessionStartHookSpecificOutput {
            additional_context: Some("session ctx".into()),
        });
        let val = serde_json::to_value(&out).unwrap();
        assert_eq!(val["hookEventName"], "SessionStart");
        assert_eq!(val["additionalContext"], "session ctx");
        let back: HookSpecificOutput = serde_json::from_value(val).unwrap();
        match back {
            HookSpecificOutput::SessionStart(s) => {
                assert_eq!(s.additional_context.as_deref(), Some("session ctx"));
            },
            _ => panic!("expected SessionStart"),
        }
    }

    // --- SystemPrompt: String and Preset variants ---
    #[test]
    fn test_system_prompt_string_serde() {
        let prompt = SystemPrompt::String("You are a helpful assistant.".into());
        let val = serde_json::to_value(&prompt).unwrap();
        assert_eq!(val, serde_json::json!("You are a helpful assistant."));
        let back: SystemPrompt = serde_json::from_value(val).unwrap();
        match back {
            SystemPrompt::String(s) => assert_eq!(s, "You are a helpful assistant."),
            _ => panic!("expected String variant"),
        }
    }

    #[test]
    fn test_system_prompt_preset_serde() {
        let prompt = SystemPrompt::Preset {
            preset_type: "preset".into(),
            preset: "claude_code".into(),
            append: Some("Also be concise.".into()),
        };
        let val = serde_json::to_value(&prompt).unwrap();
        assert_eq!(val["type"], "preset");
        assert_eq!(val["preset"], "claude_code");
        assert_eq!(val["append"], "Also be concise.");
        let back: SystemPrompt = serde_json::from_value(val).unwrap();
        match back {
            SystemPrompt::Preset {
                preset_type,
                preset,
                append,
            } => {
                assert_eq!(preset_type, "preset");
                assert_eq!(preset, "claude_code");
                assert_eq!(append.as_deref(), Some("Also be concise."));
            },
            _ => panic!("expected Preset variant"),
        }
    }

    #[test]
    fn test_system_prompt_preset_no_append_serde() {
        let prompt = SystemPrompt::Preset {
            preset_type: "preset".into(),
            preset: "claude_code".into(),
            append: None,
        };
        let val = serde_json::to_value(&prompt).unwrap();
        assert!(val.get("append").is_none());
        let back: SystemPrompt = serde_json::from_value(val).unwrap();
        match back {
            SystemPrompt::Preset { append, .. } => assert!(append.is_none()),
            _ => panic!("expected Preset variant"),
        }
    }

    // --- SettingSource: all variants ---
    #[test]
    fn test_setting_source_serde() {
        let cases = vec![
            (SettingSource::User, "user"),
            (SettingSource::Project, "project"),
            (SettingSource::Local, "local"),
        ];
        for (variant, expected) in cases {
            let val = serde_json::to_value(&variant).unwrap();
            assert_eq!(val, serde_json::json!(expected), "serialize {:?}", variant);
            let back: SettingSource = serde_json::from_value(val).unwrap();
            assert_eq!(back, variant);
        }
    }

    // --- AgentDefinition: serde round-trip ---
    #[test]
    fn test_agent_definition_serde_full() {
        let agent = AgentDefinition {
            description: "A code review agent".into(),
            prompt: "Review the code for bugs.".into(),
            tools: Some(vec!["Read".into(), "Grep".into()]),
            model: Some("claude-3-opus".into()),
        };
        let val = serde_json::to_value(&agent).unwrap();
        assert_eq!(val["description"], "A code review agent");
        assert_eq!(val["prompt"], "Review the code for bugs.");
        assert_eq!(val["tools"], serde_json::json!(["Read", "Grep"]));
        assert_eq!(val["model"], "claude-3-opus");

        let back: AgentDefinition = serde_json::from_value(val).unwrap();
        assert_eq!(back.description, "A code review agent");
        assert_eq!(back.tools.unwrap(), vec!["Read", "Grep"]);
        assert_eq!(back.model.unwrap(), "claude-3-opus");
    }

    #[test]
    fn test_agent_definition_serde_minimal() {
        let agent = AgentDefinition {
            description: "Minimal".into(),
            prompt: "Do something.".into(),
            tools: None,
            model: None,
        };
        let val = serde_json::to_value(&agent).unwrap();
        assert!(val.get("tools").is_none());
        assert!(val.get("model").is_none());

        let back: AgentDefinition = serde_json::from_value(val).unwrap();
        assert!(back.tools.is_none());
        assert!(back.model.is_none());
    }

    // --- Message helpers: is_sidechain, is_top_level, parent_tool_use_id ---
    #[test]
    fn test_message_user_top_level() {
        let msg = Message::User {
            message: UserMessage {
                content: "hi".into(),
                content_blocks: None,
            },
            parent_tool_use_id: None,
        };
        assert!(msg.is_top_level());
        assert!(!msg.is_sidechain());
        assert!(msg.parent_tool_use_id().is_none());
    }

    #[test]
    fn test_message_user_sidechain() {
        let msg = Message::User {
            message: UserMessage {
                content: "sub".into(),
                content_blocks: None,
            },
            parent_tool_use_id: Some("tool_123".into()),
        };
        assert!(msg.is_sidechain());
        assert!(!msg.is_top_level());
        assert_eq!(msg.parent_tool_use_id(), Some("tool_123"));
    }

    #[test]
    fn test_message_assistant_top_level() {
        let msg = Message::Assistant {
            message: AssistantMessage { content: vec![] },
            parent_tool_use_id: None,
        };
        assert!(msg.is_top_level());
        assert!(msg.parent_tool_use_id().is_none());
    }

    #[test]
    fn test_message_assistant_sidechain() {
        let msg = Message::Assistant {
            message: AssistantMessage { content: vec![] },
            parent_tool_use_id: Some("tool_456".into()),
        };
        assert!(msg.is_sidechain());
        assert_eq!(msg.parent_tool_use_id(), Some("tool_456"));
    }

    #[test]
    fn test_message_system_always_top_level() {
        let msg = Message::System {
            subtype: "info".into(),
            data: serde_json::json!({}),
        };
        assert!(msg.is_top_level());
        assert!(!msg.is_sidechain());
        assert!(msg.parent_tool_use_id().is_none());
    }

    #[test]
    fn test_message_result_always_top_level() {
        let msg = Message::Result {
            subtype: "success".into(),
            duration_ms: 100,
            duration_api_ms: 80,
            is_error: false,
            num_turns: 1,
            session_id: "sess".into(),
            total_cost_usd: Some(0.01),
            usage: None,
            result: Some("done".into()),
            structured_output: None,
        };
        assert!(msg.is_top_level());
        assert!(!msg.is_sidechain());
        assert!(msg.parent_tool_use_id().is_none());
    }

    #[test]
    fn test_message_stream_event_top_level() {
        let msg = Message::StreamEvent {
            event: StreamEventData::MessageStop,
            session_id: Some("s1".into()),
            parent_tool_use_id: None,
        };
        assert!(msg.is_top_level());
        assert!(msg.parent_tool_use_id().is_none());
    }

    #[test]
    fn test_message_stream_event_sidechain() {
        let msg = Message::StreamEvent {
            event: StreamEventData::MessageStop,
            session_id: None,
            parent_tool_use_id: Some("tool_789".into()),
        };
        assert!(msg.is_sidechain());
        assert_eq!(msg.parent_tool_use_id(), Some("tool_789"));
    }

    // --- Builder methods not yet tested ---
    #[test]
    #[allow(deprecated)]
    fn test_builder_append_system_prompt() {
        let opts = ClaudeCodeOptions::builder()
            .append_system_prompt("extra instructions")
            .build();
        assert_eq!(
            opts.append_system_prompt,
            Some("extra instructions".to_string())
        );
    }

    #[test]
    fn test_builder_disallowed_tools() {
        let opts = ClaudeCodeOptions::builder()
            .disallowed_tools(vec!["Bash".into(), "WebSearch".into()])
            .build();
        assert_eq!(opts.disallowed_tools, vec!["Bash", "WebSearch"]);
    }

    #[test]
    fn test_builder_disallow_tool() {
        let opts = ClaudeCodeOptions::builder()
            .disallow_tool("Bash")
            .disallow_tool("WebSearch")
            .build();
        assert_eq!(opts.disallowed_tools, vec!["Bash", "WebSearch"]);
    }

    #[test]
    fn test_builder_mcp_servers() {
        let mut servers = HashMap::new();
        servers.insert(
            "test".into(),
            McpServerConfig::Stdio {
                command: "node".into(),
                args: None,
                env: None,
            },
        );
        let opts = ClaudeCodeOptions::builder().mcp_servers(servers).build();
        assert_eq!(opts.mcp_servers.len(), 1);
        assert!(opts.mcp_servers.contains_key("test"));
    }

    #[test]
    fn test_builder_add_mcp_server() {
        let opts = ClaudeCodeOptions::builder()
            .add_mcp_server(
                "srv1",
                McpServerConfig::Sse {
                    url: "https://example.com".into(),
                    headers: None,
                },
            )
            .build();
        assert_eq!(opts.mcp_servers.len(), 1);
    }

    #[test]
    fn test_builder_mcp_tools() {
        let opts = ClaudeCodeOptions::builder()
            .mcp_tools(vec!["mcp_tool1".into(), "mcp_tool2".into()])
            .build();
        assert_eq!(opts.mcp_tools, vec!["mcp_tool1", "mcp_tool2"]);
    }

    #[test]
    fn test_builder_max_thinking_tokens() {
        let opts = ClaudeCodeOptions::builder()
            .max_thinking_tokens(8000)
            .build();
        assert_eq!(opts.max_thinking_tokens, 8000);
    }

    #[test]
    fn test_builder_max_output_tokens_clamp() {
        // Within range
        let opts = ClaudeCodeOptions::builder()
            .max_output_tokens(16000)
            .build();
        assert_eq!(opts.max_output_tokens, Some(16000));

        // Above max, should clamp to 32000
        let opts = ClaudeCodeOptions::builder()
            .max_output_tokens(50000)
            .build();
        assert_eq!(opts.max_output_tokens, Some(32000));

        // Below min, should clamp to 1
        let opts = ClaudeCodeOptions::builder().max_output_tokens(0).build();
        assert_eq!(opts.max_output_tokens, Some(1));
    }

    #[test]
    fn test_builder_cwd() {
        let opts = ClaudeCodeOptions::builder().cwd("/tmp/work").build();
        assert_eq!(opts.cwd, Some(PathBuf::from("/tmp/work")));
    }

    #[test]
    fn test_builder_continue_conversation() {
        let opts = ClaudeCodeOptions::builder()
            .continue_conversation(true)
            .build();
        assert!(opts.continue_conversation);
    }

    #[test]
    fn test_builder_resume() {
        let opts = ClaudeCodeOptions::builder()
            .resume("session-abc-123")
            .build();
        assert_eq!(opts.resume, Some("session-abc-123".to_string()));
    }

    #[test]
    fn test_builder_permission_prompt_tool_name() {
        let opts = ClaudeCodeOptions::builder()
            .permission_prompt_tool_name("my_tool")
            .build();
        assert_eq!(
            opts.permission_prompt_tool_name,
            Some("my_tool".to_string())
        );
    }

    #[test]
    fn test_builder_settings() {
        let opts = ClaudeCodeOptions::builder()
            .settings("/path/to/settings.json")
            .build();
        assert_eq!(opts.settings, Some("/path/to/settings.json".to_string()));
    }

    #[test]
    fn test_builder_add_dirs() {
        let opts = ClaudeCodeOptions::builder()
            .add_dirs(vec![PathBuf::from("/dir1"), PathBuf::from("/dir2")])
            .build();
        assert_eq!(opts.add_dirs.len(), 2);
    }

    #[test]
    fn test_builder_add_dir() {
        let opts = ClaudeCodeOptions::builder()
            .add_dir("/dir1")
            .add_dir("/dir2")
            .build();
        assert_eq!(
            opts.add_dirs,
            vec![PathBuf::from("/dir1"), PathBuf::from("/dir2")]
        );
    }

    #[test]
    fn test_builder_control_protocol_format() {
        let opts = ClaudeCodeOptions::builder()
            .control_protocol_format(ControlProtocolFormat::Control)
            .build();
        assert_eq!(opts.control_protocol_format, ControlProtocolFormat::Control);
    }

    #[test]
    fn test_builder_include_partial_messages() {
        let opts = ClaudeCodeOptions::builder()
            .include_partial_messages(true)
            .build();
        assert!(opts.include_partial_messages);
    }

    #[test]
    fn test_builder_fork_session() {
        let opts = ClaudeCodeOptions::builder().fork_session(true).build();
        assert!(opts.fork_session);
    }

    #[test]
    fn test_builder_setting_sources() {
        let opts = ClaudeCodeOptions::builder()
            .setting_sources(vec![SettingSource::User, SettingSource::Project])
            .build();
        let sources = opts.setting_sources.unwrap();
        assert_eq!(sources, vec![SettingSource::User, SettingSource::Project]);
    }

    #[test]
    fn test_builder_agents() {
        let mut agents = HashMap::new();
        agents.insert(
            "reviewer".into(),
            AgentDefinition {
                description: "Code reviewer".into(),
                prompt: "Review code.".into(),
                tools: None,
                model: None,
            },
        );
        let opts = ClaudeCodeOptions::builder().agents(agents).build();
        assert!(opts.agents.unwrap().contains_key("reviewer"));
    }

    #[test]
    fn test_builder_cli_channel_buffer_size() {
        let opts = ClaudeCodeOptions::builder()
            .cli_channel_buffer_size(500)
            .build();
        assert_eq!(opts.cli_channel_buffer_size, Some(500));
    }

    #[test]
    fn test_builder_user() {
        let opts = ClaudeCodeOptions::builder().user("nobody").build();
        assert_eq!(opts.user, Some("nobody".to_string()));
    }

    #[test]
    fn test_builder_memory_options() {
        let opts = ClaudeCodeOptions::builder()
            .memory_enabled(true)
            .memory_threshold(0.5)
            .max_context_items(10)
            .memory_token_budget(4000)
            .build();
        assert!(opts.memory_enabled);
        assert_eq!(opts.memory_threshold, Some(0.5));
        assert_eq!(opts.max_context_items, Some(10));
        assert_eq!(opts.memory_token_budget, Some(4000));
    }

    #[test]
    fn test_builder_memory_threshold_clamp() {
        let opts = ClaudeCodeOptions::builder().memory_threshold(1.5).build();
        assert_eq!(opts.memory_threshold, Some(1.0));

        let opts = ClaudeCodeOptions::builder().memory_threshold(-0.5).build();
        assert_eq!(opts.memory_threshold, Some(0.0));
    }

    #[test]
    fn test_builder_system_prompt_v2() {
        let opts = ClaudeCodeOptions::builder().build();
        // system_prompt_v2 is set via tools field, verify default is None
        assert!(opts.system_prompt_v2.is_none());
    }

    // --- ClaudeCodeOptions Debug impl ---
    #[test]
    fn test_claude_code_options_debug() {
        let opts = ClaudeCodeOptions::builder()
            .model("claude-3-opus")
            .max_turns(5)
            .build();
        let dbg = format!("{:?}", opts);
        assert!(dbg.contains("ClaudeCodeOptions"));
        assert!(dbg.contains("claude-3-opus"));
        assert!(dbg.contains("max_turns"));
    }

    // --- Stream event data serde ---
    #[test]
    fn test_stream_event_data_message_stop_serde() {
        let data = StreamEventData::MessageStop;
        let val = serde_json::to_value(&data).unwrap();
        assert_eq!(val["type"], "message_stop");
        let back: StreamEventData = serde_json::from_value(val).unwrap();
        assert_eq!(back, StreamEventData::MessageStop);
    }

    #[test]
    fn test_stream_delta_text_delta_serde() {
        let delta = StreamDelta::TextDelta {
            text: "Hello".into(),
        };
        let val = serde_json::to_value(&delta).unwrap();
        assert_eq!(val["type"], "text_delta");
        assert_eq!(val["text"], "Hello");
        let back: StreamDelta = serde_json::from_value(val).unwrap();
        assert_eq!(back, delta);
    }

    #[test]
    fn test_stream_delta_thinking_delta_serde() {
        let delta = StreamDelta::ThinkingDelta {
            thinking: "pondering...".into(),
        };
        let val = serde_json::to_value(&delta).unwrap();
        assert_eq!(val["type"], "thinking_delta");
        assert_eq!(val["thinking"], "pondering...");
        let back: StreamDelta = serde_json::from_value(val).unwrap();
        assert_eq!(back, delta);
    }

    #[test]
    fn test_stream_delta_input_json_delta_serde() {
        let delta = StreamDelta::InputJsonDelta {
            partial_json: r#"{"key":"#.into(),
        };
        let val = serde_json::to_value(&delta).unwrap();
        assert_eq!(val["type"], "input_json_delta");
        assert_eq!(val["partial_json"], r#"{"key":"#);
        let back: StreamDelta = serde_json::from_value(val).unwrap();
        assert_eq!(back, delta);
    }

    // --- PermissionUpdate serde ---
    #[test]
    fn test_permission_update_serde_roundtrip() {
        let update = PermissionUpdate {
            update_type: PermissionUpdateType::AddRules,
            rules: Some(vec![PermissionRuleValue {
                tool_name: "Bash".into(),
                rule_content: Some("git:*".into()),
            }]),
            behavior: Some(PermissionBehavior::Allow),
            mode: None,
            directories: None,
            destination: Some(PermissionUpdateDestination::ProjectSettings),
        };
        let val = serde_json::to_value(&update).unwrap();
        assert_eq!(val["type"], "addRules");
        assert_eq!(val["rules"][0]["tool_name"], "Bash");
        assert_eq!(val["rules"][0]["rule_content"], "git:*");
        assert_eq!(val["behavior"], "allow");
        assert_eq!(val["destination"], "projectSettings");
        assert!(val.get("mode").is_none());
        assert!(val.get("directories").is_none());

        let back: PermissionUpdate = serde_json::from_value(val).unwrap();
        assert_eq!(back.update_type, PermissionUpdateType::AddRules);
        assert_eq!(back.rules.as_ref().unwrap().len(), 1);
        assert_eq!(back.behavior, Some(PermissionBehavior::Allow));
    }

    // --- SDKControlRewindFilesRequest::new ---
    #[test]
    fn test_sdk_control_rewind_files_request_new() {
        let req = SDKControlRewindFilesRequest::new("msg_abc");
        assert_eq!(req.subtype, "rewind_files");
        assert_eq!(req.user_message_id, "msg_abc");
    }
}
