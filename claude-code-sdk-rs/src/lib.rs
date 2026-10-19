//! # Claude Code SDK for Rust
//!
//! A Rust SDK for interacting with the Claude Code CLI, providing both simple query
//! and interactive client interfaces.
//!
//! ## Features
//!
//! - **Simple Query Interface**: One-shot queries with the `query` function
//! - **Interactive Client**: Stateful conversations with `ClaudeSDKClient`
//! - **Streaming Support**: Async streaming of responses
//! - **Type Safety**: Strongly typed messages and errors
//! - **Flexible Configuration**: Extensive options for customization
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use nexus_claude::{query, Result};
//! use futures::StreamExt;
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     let mut messages = query("What is 2 + 2?", None).await?;
//!     
//!     while let Some(msg) = messages.next().await {
//!         println!("{:?}", msg?);
//!     }
//!     
//!     Ok(())
//! }
//! ```

#![warn(missing_docs)]
#![warn(rustdoc::missing_crate_level_docs)]

pub mod agent;
/// CLI download and management utilities
pub mod cli_download;
mod client;
// mod client_v2;  // Has compilation errors
// mod client_final;  // Has compilation errors
mod errors;
mod interactive;
mod internal_query;
mod message_parser;
pub mod model;
pub mod providers;
mod query;
mod sdk_mcp;
#[cfg(feature = "testkit")]
pub mod testkit;
pub mod token_tracker;
pub mod transport;
mod types;

/// Memory module for persistent conversation context
pub mod memory;

// Re-export main types and functions
pub use client::ClaudeSDKClient;
// pub use client_v2::ClaudeSDKClientV2;  // Has compilation errors
// pub use client_final::ClaudeSDKClientFinal;  // Has compilation errors
pub use errors::{Result, SdkError};
pub use interactive::InteractiveClient;
pub use interactive::{build_hook_response_json, dispatch_hook_from_registry, is_hook_callback};
pub use internal_query::Query;
pub use query::query;
pub use transport::EnvPolicy;
pub use transport::SECRET_BEARING_ARGS;
pub use transport::UserContentBlock;
pub use transport::describe_command_redacted;
// Keep the old name as an alias for backward compatibility
pub use interactive::InteractiveClient as SimpleInteractiveClient;
pub use token_tracker::{BudgetLimit, BudgetManager, BudgetStatus, TokenUsageTracker};
/// Default interactive client - the recommended client for interactive use
pub type ClaudeSDKClientDefault = InteractiveClient;
pub use types::{
    AgentDefinition,
    AssistantContent,
    AssistantMessage,
    AsyncHookJSONOutput,
    // Hook Input types (strongly-typed)
    BaseHookInput,
    CanUseTool,
    ClaudeCodeOptions,
    ContentBlock,
    ContentValue,
    ControlProtocolFormat,
    ControlRequest,
    ControlResponse,
    // Hook types (v0.3.0 - strongly-typed hooks)
    HookCallback,
    HookContext,
    HookInput,
    // Hook Output types (strongly-typed)
    HookJSONOutput,
    HookMatcher,
    HookSpecificOutput,
    McpServerConfig,
    Message,
    NotificationHookInput,
    // Permission types
    PermissionBehavior,
    PermissionMode,
    PermissionResult,
    PermissionResultAllow,
    PermissionResultDeny,
    PermissionRuleValue,
    PermissionUpdate,
    PermissionUpdateDestination,
    PermissionUpdateType,
    PostToolUseHookInput,
    PostToolUseHookSpecificOutput,
    PreCompactHookInput,
    PreToolUseHookInput,
    PreToolUseHookSpecificOutput,
    ResultMessage,
    // SDK Control Protocol types
    SDKControlInitializeRequest,
    SDKControlInterruptRequest,
    SDKControlMcpMessageRequest,
    SDKControlPermissionRequest,
    SDKControlRequest,
    SDKControlRewindFilesRequest,
    SDKControlSetPermissionModeRequest,
    SDKHookCallbackRequest,
    SandboxIgnoreViolations,
    SandboxNetworkConfig,
    SandboxSettings,
    SdkBeta,
    SdkPluginConfig,
    SessionStartHookInput,
    SessionStartHookSpecificOutput,
    // Phase 2 enhancements
    SettingSource,
    StopHookInput,
    // Streaming types (for real-time token streaming)
    StreamDelta,
    StreamEventData,
    SubagentStopHookInput,
    SyncHookJSONOutput,
    SystemMessage,
    SystemPrompt,
    TextContent,
    ThinkingContent,
    ToolPermissionContext,
    ToolResultContent,
    ToolUseContent,
    // Phase 3 enhancements (Python SDK v0.1.12+ sync)
    ToolsConfig,
    ToolsPreset,
    UserContent,
    UserMessage,
    UserPromptSubmitHookInput,
    UserPromptSubmitHookSpecificOutput,
};

// Phase 3: Type aliases for naming consistency
/// Alias for ClaudeCodeOptions (matches Python SDK naming)
pub type ClaudeAgentOptions = ClaudeCodeOptions;
/// Alias for ClaudeCodeOptionsBuilder (matches Python SDK naming)
pub type ClaudeAgentOptionsBuilder = ClaudeCodeOptionsBuilder;

// Re-export builder
pub use types::ClaudeCodeOptionsBuilder;

// Re-export transport types for convenience
pub use transport::SubprocessTransport;
pub use transport::subprocess::{SemVer, find_claude_cli, get_cli_version};

// Re-export CLI download utilities
pub use cli_download::{
    check_latest_npm_version, download_cli, get_cached_cli_path, is_cli_cached,
};

// Re-export SDK MCP types
pub use sdk_mcp::{
    SdkMcpServer, SdkMcpServerBuilder, ToolDefinition, ToolHandler, ToolInputSchema, ToolResult,
    ToolResultContent as SdkToolResultContent, create_simple_tool,
};

/// Prelude module for convenient imports
pub mod prelude {
    pub use crate::{
        ClaudeCodeOptions, ClaudeSDKClient, Message, PermissionMode, Result, SdkError, query,
    };
}

/// Deterministic log capture for unit tests that assert on `tracing` output.
///
/// Why not `tracing::subscriber::with_default`: `tracing-core` caches each
/// callsite's `Interest` process-wide. While at most one dispatcher is
/// registered, a callsite hit for the first time is registered against the
/// *registering thread's* default dispatcher. Under the parallel test runner
/// another test thread (no subscriber of its own) can be first to hit a
/// callsite while a capturing test's scoped subscriber is live: the callsite is
/// cached as `Interest::never` and the capturing test silently loses that line.
///
/// Here a single global subscriber is installed once per test binary, so every
/// thread registers callsites against it; it forwards an event only to the
/// buffer of the thread currently inside [`test_log::capture`].
#[cfg(test)]
pub(crate) mod test_log {
    use std::cell::RefCell;
    use std::sync::Once;
    use tracing::Level;

    thread_local! {
        static CAPTURE: RefCell<Option<(Level, Vec<u8>)>> = const { RefCell::new(None) };
    }

    struct ThreadWriter(bool);

    impl std::io::Write for ThreadWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.0 {
                CAPTURE.with(|c| {
                    if let Some((_, out)) = c.borrow_mut().as_mut() {
                        out.extend_from_slice(buf);
                    }
                });
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct ThreadMakeWriter;

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ThreadMakeWriter {
        type Writer = ThreadWriter;
        fn make_writer(&'a self) -> Self::Writer {
            ThreadWriter(false)
        }
        fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
            // `Level` orders TRACE > DEBUG > ... > ERROR: keep events at least
            // as severe as the level the capturing thread asked for.
            let keep = CAPTURE.with(|c| {
                c.borrow()
                    .as_ref()
                    .is_some_and(|(min, _)| *meta.level() <= *min)
            });
            ThreadWriter(keep)
        }
    }

    /// Run `body` and return what it logged on this thread at `level` or above.
    pub(crate) fn capture<R>(level: Level, body: impl FnOnce() -> R) -> (R, String) {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(Level::DEBUG)
                .with_ansi(false)
                .with_writer(ThreadMakeWriter)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other global subscriber in the nexus-claude unit tests");
        });
        CAPTURE.with(|c| *c.borrow_mut() = Some((level, Vec::new())));
        let out = body();
        let bytes = CAPTURE
            .with(|c| c.borrow_mut().take())
            .map(|(_, b)| b)
            .unwrap_or_default();
        (out, String::from_utf8(bytes).expect("utf-8 log output"))
    }
}
