use config::{Config, ConfigError, Environment, File};
use serde::{Deserialize, Serialize};
use std::env;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Settings {
    pub server: ServerConfig,
    pub claude: ClaudeConfig,
    pub auth: AuthConfig,
    #[serde(default)]
    pub file_access: FileAccessConfig,
    #[serde(default)]
    pub mcp: MCPConfig,
    #[serde(default)]
    pub process_pool: ProcessPoolConfig,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ClaudeConfig {
    pub command: String,
    pub timeout_seconds: u64,
    pub max_concurrent_sessions: usize,
    #[serde(default)]
    pub use_interactive_sessions: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AuthConfig {
    pub enabled: bool,
    pub secret_key: String,
    pub token_expiry_hours: i64,
}

/// The value `auth.secret_key` defaults to, i.e. the one every reader of this
/// public repository knows.
pub const PLACEHOLDER_SECRET_KEY: &str = "change-me-in-production";

/// Why the gateway refuses to start.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthConfigError {
    #[error(
        "auth.enabled is true but auth.secret_key is still the placeholder shipped in the \
         repository: anyone can mint a token this gateway would accept. Set auth.secret_key \
         (config/<RUN_MODE>.toml, config/local.toml or CLAUDE_CODE__AUTH__SECRET_KEY), or set \
         auth.enabled = false to serve anonymously on purpose."
    )]
    PlaceholderSecretKey,
    #[error(
        "auth.enabled is true but auth.secret_key is empty: an empty HMAC key is a known key. \
         Set auth.secret_key, or set auth.enabled = false to serve anonymously on purpose."
    )]
    EmptySecretKey,
}

impl AuthConfig {
    /// Refuse a configuration that claims to authenticate and cannot.
    ///
    /// `auth.enabled = true` with the shipped placeholder secret is strictly
    /// worse than `auth.enabled = false`: both serve every caller, but the
    /// former tells the operator it does not. A warning in the log would not
    /// change that — this very gap was *documented* for a release and stayed
    /// open — so the gateway stops instead.
    ///
    /// Nothing that works today breaks: the shipped default is
    /// `auth.enabled = false`, which is never rejected, and the only
    /// configuration refused is the one that is already unauthenticated while
    /// believing otherwise. The remedy is one line of configuration.
    ///
    /// The check deliberately does not judge the *strength* of a secret an
    /// operator chose; it only refuses the two values that are public knowledge.
    pub fn validate(&self) -> Result<(), AuthConfigError> {
        if !self.enabled {
            return Ok(());
        }
        if self.secret_key.is_empty() {
            return Err(AuthConfigError::EmptySecretKey);
        }
        if self.secret_key == PLACEHOLDER_SECRET_KEY {
            return Err(AuthConfigError::PlaceholderSecretKey);
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct FileAccessConfig {
    pub skip_permissions: bool,
    pub additional_dirs: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct MCPConfig {
    pub enabled: bool,
    pub config_file: Option<String>,
    pub config_json: Option<String>,
    pub strict: bool,
    pub debug: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ProcessPoolConfig {
    pub size: usize,
    pub min_idle: usize,
    pub max_idle: usize,
}

impl Default for ProcessPoolConfig {
    fn default() -> Self {
        Self {
            size: 5,
            min_idle: 2,
            max_idle: 5,
        }
    }
}

impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        let run_mode = env::var("RUN_MODE").unwrap_or_else(|_| "development".into());

        let s = Config::builder()
            .set_default("server.host", "0.0.0.0")?
            .set_default("server.port", 8080)?
            .set_default("claude.command", "claude")?
            .set_default("claude.timeout_seconds", 300)?
            .set_default("claude.max_concurrent_sessions", 10)?
            .set_default("claude.use_interactive_sessions", false)?
            .set_default("auth.enabled", false)?
            .set_default("auth.secret_key", PLACEHOLDER_SECRET_KEY)?
            .set_default("auth.token_expiry_hours", 24)?
            .set_default("file_access.skip_permissions", false)?
            .set_default("file_access.additional_dirs", Vec::<String>::new())?
            .set_default("mcp.enabled", false)?
            .set_default("mcp.strict", false)?
            .set_default("mcp.debug", false)?
            .add_source(File::with_name(&format!("config/{run_mode}")).required(false))
            .add_source(File::with_name("config/local").required(false))
            .add_source(Environment::with_prefix("CLAUDE_CODE").separator("__"))
            .build()?;

        s.try_deserialize()
    }
}
