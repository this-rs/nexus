//! `nexus-tools`: the base tools of a Nexus agent harness, as an MCP server in pure Rust.
//!
//! The native harness speaks only MCP, so every tool it can use arrives through a server.
//! This crate is that server for the tools every coding agent needs (files, shell, web,
//! search), at parity with those of Claude Code (plan Nexus N17 to N25,
//! `docs/agent-tools-parity.md`). **Rust only, by decision**: nothing in this crate's
//! dependency tree needs a C or C++ toolchain, Node, Python or a browser.
//!
//! This is the skeleton the tools plug into: the protocol, the registry, the **profile** a
//! session is limited to (a tool outside it is never listed and never run, decision A35), the
//! signed token that carries it, output caps that always say when they cut, and the two
//! transports (stdio, streamable HTTP).

pub mod http;
pub mod limits;
pub mod profile;
pub mod protocol;
pub mod registry;
pub mod server;
#[cfg(feature = "test-tools")]
pub mod testing;
pub mod token;
pub mod tool;

pub use profile::{Profile, ToolSet};
pub use registry::ToolRegistry;
pub use server::{Server, Session, serve_lines};
pub use token::{Claims, InvalidToken, SigningKey, issue, verify};
pub use tool::{Annotations, CallContext, SessionState, Tool, ToolResult};
