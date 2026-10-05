//! Claude Code behind the agent contract (`docs/agent-contract.md`).
//!
//! [`ClaudeCodeProvider`] is a façade over [`InteractiveClient`](crate::InteractiveClient):
//! it spawns the same CLI with the same options and writes the same lines to its
//! stdin as a host driving the client by hand. What it adds is the contract: one
//! turn at a time, one destination per event, typed errors, capabilities.
//!
//! - [`control`]: every control JSON line written to the CLI, and the parsing of
//!   the CLI's control requests (§15);
//! - [`policy_map`]: neutral policy ↔ Claude Code modes, tool categories (§6);
//! - [`map_events`]: `Message` → `AgentEvent`, the public [`map_message`] (§14.1);
//! - [`error_map`]: failures → `ProviderError` (§7);
//! - [`options`]: instance configuration, `SessionSpec` → `ClaudeCodeOptions`;
//! - [`session`]: the provider, the session and its pump (§9).
//!
//! # What this slice does not do yet
//!
//! `tool_cancel` and `background_tasks` are declared absent (their fallbacks
//! apply: `cancel_tools` answers `Unsupported`, task messages travel as
//! `provider_notice`); `interrupt` writes the same request for both scopes; a
//! session asked for the native modes `auto`, `dontAsk` or `manual` starts in the
//! closest of the SDK's four modes (they can be set on the live session).

pub mod control;
pub mod error_map;
pub mod map_events;
pub mod options;
pub mod policy_map;
pub mod session;

pub use error_map::classify_result_error;
pub use map_events::{MapState, map_message, permission_event};
pub use options::{ClaudeCodeConfig, build_options};
pub use session::{ClaudeCodeProvider, ClientFactory};
