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
//! - [`input`]: a turn's input → the CLI's user message, images checked (§5);
//! - [`options`]: instance configuration, `SessionSpec` → `ClaudeCodeOptions`,
//!   the six permission modes at launch (§6);
//! - [`cancel`]: the CLI's process tree — descendants, signals, pid claim (§10);
//! - [`tasks`]: the table of background tasks behind `background_tasks` (§4);
//! - [`session`]: the provider, the session and its pump (§9).
//!
//! # Images
//!
//! `images` is declared, local and over SSH (A12, revised): a turn with an image
//! is written as content blocks in the user's order, after the checks of
//! [`input`]; a text-only turn is the same string message as before.

pub mod cancel;
pub mod control;
pub mod error_map;
pub mod input;
pub mod map_events;
pub mod options;
pub mod policy_map;
pub mod session;
pub mod tasks;

pub use error_map::classify_result_error;
pub use map_events::{MapState, map_message, permission_event};
pub use options::{ClaudeCodeConfig, build_options};
pub use session::{ClaudeCodeProvider, ClientFactory};
