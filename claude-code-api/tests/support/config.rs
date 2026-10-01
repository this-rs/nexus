//! Settings construction for tests.
//!
//! [`Settings::new`](claude_code_api::core::config::Settings::new) reads
//! `config/<RUN_MODE>.toml`, `config/local.toml` and `CLAUDE_CODE__*` environment
//! variables. None of that is wanted in a unit test: the files depend on the
//! current working directory and the environment is process-global, so two tests
//! running concurrently would see each other's variables.
//!
//! [`TestSettings`] therefore builds the `Settings` struct directly — no
//! filesystem, no environment, nothing shared between tests. Use [`EnvGuard`]
//! (with `#[serial_test::serial]`) only when the code under test is
//! `Settings::new` itself.

use claude_code_api::core::config::{
    AuthConfig, ClaudeConfig, FileAccessConfig, MCPConfig, ProcessPoolConfig, ServerConfig,
    Settings,
};

/// Builder for a [`Settings`] value that is safe in a test process.
///
/// The defaults are deliberately *not* the production defaults:
///
/// * `claude.command` points at a path that cannot exist, so nothing can ever
///   launch the real `claude` binary by accident. Handlers that try will fail
///   to spawn and return `500 claude_process_error`.
/// * `process_pool.min_idle = 0`, because [`ProcessPool::new`] spawns a
///   background task that pre-warms `min_idle` CLI processes every 10 seconds.
///   With the production default of 2 every `test_app()` would try to spawn two
///   subprocesses.
/// * `claude.timeout_seconds = 5`, so a test that waits on a CLI that never
///   answers fails in seconds instead of five minutes.
#[derive(Debug, Clone)]
pub struct TestSettings {
    inner: Settings,
}

impl Default for TestSettings {
    fn default() -> Self {
        Self {
            inner: Settings {
                server: ServerConfig {
                    host: "127.0.0.1".to_string(),
                    // 0 = never bound; tests drive the router in-process.
                    port: 0,
                },
                claude: ClaudeConfig {
                    command: no_such_command(),
                    timeout_seconds: 5,
                    max_concurrent_sessions: 4,
                    use_interactive_sessions: false,
                },
                auth: AuthConfig {
                    enabled: false,
                    secret_key: "test-secret-not-used".to_string(),
                    token_expiry_hours: 1,
                },
                file_access: FileAccessConfig {
                    skip_permissions: false,
                    additional_dirs: Vec::new(),
                },
                mcp: MCPConfig::default(),
                process_pool: ProcessPoolConfig {
                    size: 1,
                    min_idle: 0,
                    max_idle: 0,
                },
            },
        }
    }
}

impl TestSettings {
    pub fn new() -> Self {
        Self::default()
    }

    /// Point the gateway at `command` instead of the unspawnable default.
    ///
    /// Use with [`crate::support::fake_cli::FakeClaudeCli::command`].
    pub fn command(mut self, command: impl Into<String>) -> Self {
        self.inner.claude.command = command.into();
        self
    }

    /// Route chat completions through `InteractiveSessionManager` instead of
    /// `ProcessPool` (the `claude.use_interactive_sessions` switch).
    pub fn interactive_sessions(mut self, enabled: bool) -> Self {
        self.inner.claude.use_interactive_sessions = enabled;
        self
    }

    /// Wall-clock budget for a non-streaming completion.
    ///
    /// Note: `handle_non_streaming_response` polls the CLI channel in 5-second
    /// slices and only then compares against this budget, so any value below 5
    /// still costs ~5 seconds.
    pub fn timeout_seconds(mut self, secs: u64) -> Self {
        self.inner.claude.timeout_seconds = secs;
        self
    }

    pub fn skip_permissions(mut self, skip: bool) -> Self {
        self.inner.file_access.skip_permissions = skip;
        self
    }

    pub fn mcp(mut self, mcp: MCPConfig) -> Self {
        self.inner.mcp = mcp;
        self
    }

    pub fn process_pool(mut self, size: usize, min_idle: usize, max_idle: usize) -> Self {
        self.inner.process_pool = ProcessPoolConfig {
            size,
            min_idle,
            max_idle,
        };
        self
    }

    pub fn auth(mut self, enabled: bool, secret_key: impl Into<String>) -> Self {
        self.inner.auth.enabled = enabled;
        self.inner.auth.secret_key = secret_key.into();
        self
    }

    pub fn build(self) -> Settings {
        self.inner
    }
}

/// A path that is guaranteed not to be an executable, on every platform.
///
/// Spawning it fails with `NotFound`, which is how a test reaches the
/// `ApiError::ClaudeProcess` branch of `chat_completions` without a real CLI.
pub fn no_such_command() -> String {
    "nexus-test-claude-does-not-exist".to_string()
}

/// Scoped `std::env` mutation, for the few tests that exercise
/// `Settings::new()` / `ModelRegistry::new()` and therefore must touch the
/// process environment.
///
/// Restores the previous value (or removes the variable) on drop. Always pair
/// with `#[serial_test::serial]`: the environment is process-global and
/// `std::env::set_var` is `unsafe` in edition 2024 precisely because of that.
pub struct EnvGuard {
    saved: Vec<(String, Option<String>)>,
}

impl EnvGuard {
    pub fn new() -> Self {
        Self { saved: Vec::new() }
    }

    pub fn set(mut self, key: &str, value: &str) -> Self {
        self.saved.push((key.to_string(), std::env::var(key).ok()));
        // SAFETY: callers are `#[serial]`, so no other test thread is reading
        // or writing the environment concurrently.
        unsafe { std::env::set_var(key, value) };
        self
    }

    pub fn remove(mut self, key: &str) -> Self {
        self.saved.push((key.to_string(), std::env::var(key).ok()));
        // SAFETY: see `set`.
        unsafe { std::env::remove_var(key) };
        self
    }
}

impl Default for EnvGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..).rev() {
            // SAFETY: see `EnvGuard::set`.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(&key, v),
                    None => std::env::remove_var(&key),
                }
            }
        }
    }
}
