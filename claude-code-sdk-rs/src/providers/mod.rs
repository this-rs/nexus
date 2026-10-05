//! Provider adapters behind the agent contract ([`crate::agent`]).
//!
//! Each adapter implements [`AgentProvider`](crate::agent::AgentProvider) and
//! [`AgentSession`](crate::agent::AgentSession) for one family of provider. An
//! adapter never builds a process by itself: the single launcher is
//! [`crate::transport::spawn::isolated_command`], and a guard test
//! (`tests/providers_spawn_guard.rs`) refuses any other spawn under this
//! directory.

#[cfg(feature = "provider-acp")]
pub mod acp;
pub mod claude_code;
#[cfg(feature = "provider-codex")]
pub mod codex;
#[cfg(feature = "provider-native")]
pub mod native;
