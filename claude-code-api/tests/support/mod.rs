//! Test harness for `claude-code-api`.
//!
//! Add `mod support;` to a file in `tests/` and you get:
//!
//! * [`test_app`] / [`test_app_with`] / [`test_app_with_components`] — the **real**
//!   production router (`claude_code_api::build_router`), wrapped in an
//!   `axum_test::TestServer`. Nothing here re-declares routes: if a route moves in
//!   `lib.rs`, these tests move with it.
//! * [`config::TestSettings`] — a `Settings` value built in memory, with defaults
//!   chosen so no test can spawn a CLI process or bind a port.
//! * [`fake_cli::FakeClaudeCli`] — a scripted stand-in for the `claude` binary,
//!   injected through the only seam the crate has: `settings.claude.command`.
//! * [`fakes`] — storage and memory backends with a fault table.
//! * [`openai`] / [`claude_output`] — request, response and CLI-transcript builders.
//! * [`sse`] — `text/event-stream` parsing and assertions.
//! * [`http_mocks`] — `wiremock` servers for the Anthropic Models API,
//!   Meilisearch and project-orchestrator.
//!
//! # Minimal example
//!
//! ```no_run
//! mod support;
//! use support::{openai, test_app};
//!
//! #[tokio::test]
//! async fn completion_without_a_cli_is_a_500() {
//!     let server = test_app().await;
//!     let response = server
//!         .post("/v1/chat/completions")
//!         .json(&openai::chat_request("Bonjour"))
//!         .await;
//!     response.assert_status(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
//! }
//! ```

// Each test target only uses part of the harness; without this, `clippy
// --all-targets -- -D warnings` fails on the rest.
#![allow(dead_code, unused_imports)]

pub mod claude_output;
pub mod config;
pub mod fake_cli;
pub mod fakes;
pub mod http_mocks;
pub mod openai;
pub mod sse;

use axum_test::TestServer;
use claude_code_api::core::config::Settings;
use claude_code_api::{AppComponents, build_components, build_router};

pub use config::TestSettings;
pub use fake_cli::FakeClaudeCli;

/// A `TestServer` over the production router, with settings from
/// [`TestSettings::default`].
///
/// `claude.command` points at a path that does not exist, so any request that
/// reaches the CLI fails to spawn and comes back as `500 claude_process_error`.
/// Give the server a [`FakeClaudeCli`] (see [`test_app_with_cli`]) when you need
/// a successful completion.
pub async fn test_app() -> TestServer {
    test_app_with(TestSettings::new().build()).await
}

/// A `TestServer` over the production router, built from `settings`.
pub async fn test_app_with(settings: Settings) -> TestServer {
    let components = build_components(settings)
        .await
        .expect("build_components must not fail for in-memory settings");
    test_app_with_components(components)
}

/// A `TestServer` over the production router with hand-assembled state.
///
/// Use this to swap a single component — typically
/// `components.model_registry = http_mocks::registry_for(&server, Some("k"))`.
pub fn test_app_with_components(components: AppComponents) -> TestServer {
    TestServer::new(build_router(components)).expect("TestServer over the gateway router")
}

/// A `TestServer` whose `claude.command` is `cli`.
///
/// The caller must keep `cli` alive: it owns the temporary directory holding the
/// script.
pub async fn test_app_with_cli(cli: &FakeClaudeCli) -> TestServer {
    test_app_with(TestSettings::new().command(cli.command()).build()).await
}

/// Same, routed through `InteractiveSessionManager` instead of `ProcessPool`.
pub async fn test_app_with_interactive_cli(cli: &FakeClaudeCli) -> TestServer {
    test_app_with(
        TestSettings::new()
            .command(cli.command())
            .interactive_sessions(true)
            .build(),
    )
    .await
}

/// The production application state for `settings`, before it is turned into a
/// router — for tests that want to poke at a component directly.
pub async fn test_components(settings: Settings) -> AppComponents {
    build_components(settings)
        .await
        .expect("build_components must not fail for in-memory settings")
}
