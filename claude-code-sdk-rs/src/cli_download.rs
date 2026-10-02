//! Automatic Claude Code CLI download and management
//!
//! This module provides functionality to automatically download and manage
//! the Claude Code CLI binary, similar to Python SDK's bundling approach.
//!
//! # Download Strategy
//!
//! 1. First, check if CLI is already installed (PATH, common locations)
//! 2. If not found, check the SDK's local cache directory
//! 3. If not cached, download from official source and cache locally
//!
//! # Cache Location
//!
//! - Unix: `~/.cache/cc-sdk/cli/`
//! - macOS: `~/Library/Caches/cc-sdk/cli/`
//! - Windows: `%LOCALAPPDATA%\cc-sdk\cli\`
//!
//! # Feature Flag
//!
//! The download functionality requires the `auto-download` feature (enabled by default).
//! To disable, use `default-features = false` in your Cargo.toml.

use crate::errors::{Result, SdkError};
use std::path::PathBuf;
#[allow(unused_imports)]
use tracing::{debug, info, warn};

/// Progress callback type for download operations.
/// Called with (bytes_downloaded, total_bytes) where total_bytes may be None if unknown.
pub type ProgressCallback = Box<dyn Fn(u64, Option<u64>) + Send + Sync>;

/// Minimum CLI version recommended by this SDK.
///
/// Kept in sync with `transport::subprocess::MIN_CLI_VERSION`. See that constant
/// for why this tracks the newest known-good CLI rather than a true floor.
pub const MIN_CLI_VERSION: &str = "2.1.280";

/// Default CLI version to download if not specified
pub const DEFAULT_CLI_VERSION: &str = "latest";

/// URL of the official Unix install script.
#[cfg(all(unix, feature = "auto-download"))]
const UNIX_INSTALL_SCRIPT_URL: &str = "https://claude.ai/install.sh";

/// URL of the official Windows install script.
#[cfg(all(windows, feature = "auto-download"))]
const WINDOWS_INSTALL_SCRIPT_URL: &str = "https://claude.ai/install.ps1";

/// npm registry endpoint used to discover the latest published CLI version.
#[cfg(feature = "auto-download")]
const NPM_REGISTRY_LATEST_URL: &str = "https://registry.npmjs.org/@anthropic-ai/claude-code/latest";

/// Environment variables honoured **only in test builds** (see [`test_override`]).
const ENV_CACHE_DIR: &str = "CC_SDK_TEST_CACHE_DIR";
/// Value of [`ENV_CACHE_DIR`] that makes [`get_cache_dir`] answer `None`, as it
/// would on a platform where neither a home nor a cache directory resolves.
///
/// Compiled **only in test builds**. There is no other way to reach that branch:
/// `dirs::home_dir()` falls back to `getpwuid_r`, so it cannot fail for a real
/// process — yet the `ConfigError` it produces is part of [`download_cli`]'s
/// documented contract, so it has to be assertable.
#[cfg(test)]
const ENV_CACHE_DIR_UNRESOLVABLE: &str = "<unresolvable>";
#[cfg(feature = "auto-download")]
const ENV_INSTALL_SCRIPT_URL: &str = "CC_SDK_TEST_INSTALL_SCRIPT_URL";
#[cfg(feature = "auto-download")]
const ENV_NPM_REGISTRY_URL: &str = "CC_SDK_TEST_NPM_REGISTRY_URL";
#[cfg(feature = "auto-download")]
const ENV_NPM_BIN: &str = "CC_SDK_TEST_NPM_BIN";

/// Read a test-only override for a hard-coded endpoint or directory.
///
/// Release builds compile the `#[cfg(not(test))]` twin, which always returns
/// `None`: every caller then falls back to the constant it would have used
/// anyway, so shipped behaviour is unchanged. The `#[cfg(test)]` twin is what
/// lets the unit tests point the downloader at a local mock server, a scratch
/// cache directory and a fake `npm`, instead of the network and the user's
/// real cache.
#[cfg(not(test))]
fn test_override(_key: &str) -> Option<String> {
    None
}

#[cfg(test)]
fn test_override(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Endpoint `install_cli_unix` fetches the install script from.
#[cfg(all(unix, feature = "auto-download"))]
fn unix_install_script_url() -> String {
    test_override(ENV_INSTALL_SCRIPT_URL).unwrap_or_else(|| UNIX_INSTALL_SCRIPT_URL.to_string())
}

/// Endpoint `install_cli_windows` hands to PowerShell.
#[cfg(all(windows, feature = "auto-download"))]
fn windows_install_script_url() -> String {
    test_override(ENV_INSTALL_SCRIPT_URL).unwrap_or_else(|| WINDOWS_INSTALL_SCRIPT_URL.to_string())
}

/// Endpoint `check_latest_npm_version` queries for the newest published version.
#[cfg(feature = "auto-download")]
fn npm_registry_latest_url() -> String {
    test_override(ENV_NPM_REGISTRY_URL).unwrap_or_else(|| NPM_REGISTRY_LATEST_URL.to_string())
}

/// Name (or absolute path) of the `npm` executable used by the fallback install.
#[cfg(feature = "auto-download")]
fn npm_binary() -> String {
    test_override(ENV_NPM_BIN).unwrap_or_else(|| "npm".to_string())
}

/// Get the cache directory for the SDK
pub fn get_cache_dir() -> Option<PathBuf> {
    if let Some(dir) = test_override(ENV_CACHE_DIR) {
        #[cfg(test)]
        if dir == ENV_CACHE_DIR_UNRESOLVABLE {
            return None;
        }
        return Some(PathBuf::from(dir));
    }
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|h| h.join("Library/Caches/cc-sdk/cli"))
    }
    #[cfg(target_os = "windows")]
    {
        dirs::cache_dir().map(|c| c.join("cc-sdk").join("cli"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        dirs::cache_dir().map(|c| c.join("cc-sdk").join("cli"))
    }
}

/// Get the path to the cached CLI binary
pub fn get_cached_cli_path() -> Option<PathBuf> {
    let cache_dir = get_cache_dir()?;
    // `#[cfg]`, not `cfg!()`: with the runtime form both arms are compiled on
    // every target, so whichever one the platform cannot take is reported as a
    // permanently uncovered line — on Windows it was `"claude"` that never ran.
    #[cfg(windows)]
    let cli_name = "claude.exe";
    #[cfg(not(windows))]
    let cli_name = "claude";
    Some(cache_dir.join(cli_name))
}

/// Check if the cached CLI exists and is executable
#[allow(dead_code)]
pub fn is_cli_cached() -> bool {
    if let Some(path) = get_cached_cli_path()
        && path.exists()
        && path.is_file()
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // `is_ok_and`, not `if let Ok(..)`: the metadata call cannot fail
            // here (`exists()` and `is_file()` above just stat'ed the same path),
            // so the former `if let` left an unreachable fall-through branch.
            return path
                .metadata()
                .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0);
        }
        #[cfg(not(unix))]
        {
            return true;
        }
    }
    false
}

/// Download the Claude Code CLI to the cache directory
///
/// # Arguments
///
/// * `version` - Version to download ("latest" or specific version like "2.0.62")
/// * `on_progress` - Optional callback for download progress (bytes_downloaded, total_bytes)
///
/// # Returns
///
/// Path to the downloaded CLI binary
///
/// # Feature Flag
///
/// This function requires the `auto-download` feature to be enabled.
/// When disabled, it returns an error directing users to install manually.
#[cfg(feature = "auto-download")]
pub async fn download_cli(
    version: Option<&str>,
    on_progress: Option<ProgressCallback>,
) -> Result<PathBuf> {
    let version = version.unwrap_or(DEFAULT_CLI_VERSION);
    info!("Downloading Claude Code CLI version: {}", version);

    let cache_dir = get_cache_dir().ok_or_else(|| {
        SdkError::ConfigError("Cannot determine cache directory for CLI download".to_string())
    })?;

    // Create cache directory if it doesn't exist
    std::fs::create_dir_all(&cache_dir)
        .map_err(|e| SdkError::ConfigError(format!("Failed to create cache directory: {}", e)))?;

    let cli_path = get_cached_cli_path()
        .ok_or_else(|| SdkError::ConfigError("Cannot determine CLI path".to_string()))?;

    // Determine platform-specific download URL and installation method
    let install_result = install_cli_for_platform(version, &cli_path, on_progress).await?;

    info!("Claude Code CLI installed to: {}", install_result.display());
    Ok(install_result)
}

/// Stub for download_cli when auto-download feature is disabled
#[cfg(not(feature = "auto-download"))]
pub async fn download_cli(
    _version: Option<&str>,
    _on_progress: Option<ProgressCallback>,
) -> Result<PathBuf> {
    Err(SdkError::ConfigError(
        "Auto-download feature is not enabled. \
        Either enable it with `features = [\"auto-download\"]` in Cargo.toml, \
        or install Claude CLI manually: npm install -g @anthropic-ai/claude-code"
            .to_string(),
    ))
}

/// Install CLI using platform-specific method
#[cfg(feature = "auto-download")]
async fn install_cli_for_platform(
    version: &str,
    target_path: &PathBuf,
    on_progress: Option<ProgressCallback>,
) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        install_cli_unix(version, target_path, on_progress).await
    }
    #[cfg(windows)]
    {
        install_cli_windows(version, target_path, on_progress).await
    }
}

/// Check known installation locations for the Claude CLI binary.
///
/// The official Anthropic install script typically installs to `~/.local/bin/claude`.
/// This function checks common locations that may not be in the current process PATH
/// (especially when running inside a Tauri desktop app).
#[cfg(all(unix, feature = "auto-download"))]
fn find_cli_in_known_locations() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let known_paths = [
        home.join(".local/bin/claude"),
        home.join(".claude/local/claude"),
        PathBuf::from("/usr/local/bin/claude"),
    ];

    for path in &known_paths {
        if path.exists() && path.is_file() {
            info!("CLI found in known location: {}", path.display());
            return Some(path.clone());
        }
    }

    None
}

/// Check known installation locations for the Claude CLI binary on Windows.
///
/// Checks common Windows installation paths that may not be in the current process PATH
/// (especially when running inside a Tauri desktop app).
#[cfg(all(windows, feature = "auto-download"))]
fn find_cli_in_known_locations() -> Option<PathBuf> {
    let known_paths: Vec<PathBuf> = vec![
        // Anthropic official installer (PowerShell)
        dirs::data_local_dir().map(|d| d.join("Programs").join("claude").join("claude.exe")),
        // npm global install (%APPDATA%\npm\claude.cmd)
        dirs::config_dir().map(|d| d.join("npm").join("claude.cmd")),
        // User-local compat path
        dirs::home_dir().map(|h| h.join(".local").join("bin").join("claude.exe")),
        // Claude local directory
        dirs::home_dir().map(|h| h.join(".claude").join("local").join("claude.exe")),
    ]
    .into_iter()
    .flatten()
    .collect();

    for path in &known_paths {
        if path.exists() && path.is_file() {
            info!("CLI found in known Windows location: {}", path.display());
            return Some(path.clone());
        }
    }

    None
}

/// Install CLI on Unix systems (macOS, Linux)
///
/// Tries the official Anthropic install script first (no Node.js dependency),
/// then falls back to npm if the script fails.
#[cfg(all(unix, feature = "auto-download"))]
async fn install_cli_unix(
    version: &str,
    target_path: &PathBuf,
    on_progress: Option<ProgressCallback>,
) -> Result<PathBuf> {
    use tokio::process::Command;

    if let Some(ref progress) = on_progress {
        progress(0, None);
    }

    // Method 1: Try using the official install script (curl — no Node.js required)
    debug!("Attempting to install via official Anthropic install script...");

    let install_script_url = unix_install_script_url();

    let script_result: Option<PathBuf> = async {
        let client = reqwest::Client::new();
        let response = client.get(&install_script_url).send().await.ok()?;

        if !response.status().is_success() {
            warn!("Install script HTTP {}", response.status());
            return None;
        }

        let script_content = response.text().await.ok()?;

        let parent_dir = target_path.parent()?;

        let output = Command::new("bash")
            .arg("-c")
            .arg(&script_content)
            .env("CLAUDE_INSTALL_DIR", parent_dir)
            .output()
            .await
            .ok()?;

        if output.status.success() {
            // The official script installs to ~/.local/bin/claude — check both
            // the target_path (cc-sdk cache) and the standard install location.
            if target_path.exists() {
                info!(
                    "Official install script succeeded → {}",
                    target_path.display()
                );
                return Some(target_path.clone());
            }

            // The script may have installed to ~/.local/bin/claude instead of target_path.
            // Try to find it via find_claude_cli (process PATH) after install.
            if let Ok(found) = crate::find_claude_cli() {
                info!(
                    "Official install script succeeded → found CLI at {}",
                    found.display()
                );
                return Some(found);
            }

            // Last resort: check known installation locations directly.
            // This handles the case where the Tauri desktop process PATH does not
            // include ~/.local/bin (common on macOS/Linux desktop environments).
            //
            // NOTE: unreachable today. `find_claude_cli()` above already probes
            // every path `find_cli_in_known_locations()` knows about
            // (~/.local/bin/claude, ~/.claude/local/claude, /usr/local/bin/claude)
            // with the same `exists() && is_file()` test, so this block can only
            // run if that list ever shrinks. Left in place as a safety net.
            if let Some(found) = find_cli_in_known_locations() {
                info!(
                    "Official install script succeeded → found CLI in known location: {}",
                    found.display()
                );
                return Some(found);
            }
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("Official install script failed: {}", stderr);
        }
        None
    }
    .await;

    if let Some(path) = script_result {
        if let Some(ref progress) = on_progress {
            progress(100, Some(100));
        }
        return Ok(path);
    }

    // Method 2: Fallback — try using npm to install and copy
    if which::which(npm_binary()).is_ok() {
        debug!("Falling back to npm install...");

        let npm_package = if version == "latest" {
            "@anthropic-ai/claude-code".to_string()
        } else {
            format!("@anthropic-ai/claude-code@{}", version)
        };

        let temp_dir = std::env::temp_dir().join("cc-sdk-npm-install");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).map_err(|e| {
            SdkError::ConfigError(format!("Failed to create temp directory: {}", e))
        })?;

        let output = Command::new(npm_binary())
            .args([
                "install",
                "--prefix",
                temp_dir.to_str().unwrap(),
                &npm_package,
            ])
            .output()
            .await
            .map_err(SdkError::ProcessError)?;

        if output.status.success() {
            let npm_bin_path = temp_dir.join("node_modules/.bin/claude");
            if npm_bin_path.exists() {
                std::fs::copy(&npm_bin_path, target_path).map_err(|e| {
                    SdkError::ConfigError(format!("Failed to copy CLI to cache: {}", e))
                })?;

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    // The previous mode is irrelevant: `set_mode(0o755)` replaced
                    // it wholesale. Reading the metadata back only added a stat
                    // and an error branch no test could ever provoke.
                    let perms = std::fs::Permissions::from_mode(0o755);
                    // NOTE: the error arm below stays uncovered. `chmod` on a
                    // regular file this process just created, in a directory it
                    // just wrote to, does not fail.
                    std::fs::set_permissions(target_path, perms).map_err(|e| {
                        SdkError::ConfigError(format!("Failed to set file permissions: {}", e))
                    })?;
                }

                let _ = std::fs::remove_dir_all(&temp_dir);

                if let Some(ref progress) = on_progress {
                    progress(100, Some(100));
                }

                return Ok(target_path.clone());
            }
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("npm install failed: {}", stderr);
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    Err(SdkError::CliNotFound {
        searched_paths: "Failed to automatically download Claude Code CLI.\n\
            Please install manually:\n\n\
            Option 1 (recommended — official script):\n\
            curl -fsSL https://claude.ai/install.sh | bash\n\n\
            Option 2 (npm):\n\
            npm install -g @anthropic-ai/claude-code\n\n\
            Error details: install script and npm both failed"
            .to_string(),
    })
}

/// Install CLI on Windows systems
#[cfg(all(windows, feature = "auto-download"))]
async fn install_cli_windows(
    version: &str,
    target_path: &PathBuf,
    on_progress: Option<ProgressCallback>,
) -> Result<PathBuf> {
    use tokio::process::Command;

    if let Some(ref progress) = on_progress {
        progress(0, None);
    }

    // Method 1: Try using npm
    if which::which(npm_binary()).is_ok() {
        debug!("Attempting to install via npm...");

        let npm_package = if version == "latest" {
            "@anthropic-ai/claude-code".to_string()
        } else {
            format!("@anthropic-ai/claude-code@{}", version)
        };

        let temp_dir = std::env::temp_dir().join("cc-sdk-npm-install");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).map_err(|e| {
            SdkError::ConfigError(format!("Failed to create temp directory: {}", e))
        })?;

        let output = Command::new(npm_binary())
            .args([
                "install",
                "--prefix",
                temp_dir.to_str().unwrap(),
                &npm_package,
            ])
            .output()
            .await
            .map_err(SdkError::ProcessError)?;

        if output.status.success() {
            let npm_bin_path = temp_dir.join("node_modules/.bin/claude.cmd");
            if npm_bin_path.exists() {
                std::fs::copy(&npm_bin_path, target_path).map_err(|e| {
                    SdkError::ConfigError(format!("Failed to copy CLI to cache: {}", e))
                })?;

                let _ = std::fs::remove_dir_all(&temp_dir);

                if let Some(ref progress) = on_progress {
                    progress(100, Some(100));
                }

                return Ok(target_path.clone());
            }
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    // Method 2: Try PowerShell install script
    debug!("Attempting to install via PowerShell script...");

    let install_script_url = windows_install_script_url();

    let parent_dir = target_path
        .parent()
        .ok_or_else(|| SdkError::ConfigError("Invalid target path".to_string()))?;

    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &format!(
                "$env:CLAUDE_INSTALL_DIR='{}'; iex (iwr -useb {})",
                parent_dir.display(),
                install_script_url
            ),
        ])
        .output()
        .await
        .map_err(SdkError::ProcessError)?;

    if output.status.success() && target_path.exists() {
        if let Some(ref progress) = on_progress {
            progress(100, Some(100));
        }
        return Ok(target_path.clone());
    }

    // Fallback: check known install locations (the install script may have
    // succeeded but installed to a location not in PATH)
    if let Some(found) = find_cli_in_known_locations() {
        if let Some(ref progress) = on_progress {
            progress(100, Some(100));
        }
        return Ok(found);
    }

    // Also try find_claude_cli() which checks PATH + SDK cache
    if let Ok(found) = crate::transport::subprocess::find_claude_cli() {
        if let Some(ref progress) = on_progress {
            progress(100, Some(100));
        }
        return Ok(found);
    }

    Err(SdkError::CliNotFound {
        searched_paths: format!(
            "Failed to automatically download Claude Code CLI.\n\
            Please install manually:\n\n\
            Option 1 (npm):\n\
            npm install -g @anthropic-ai/claude-code\n\n\
            Option 2 (PowerShell):\n\
            iwr -useb https://claude.ai/install.ps1 | iex\n\n\
            Error details: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    })
}

/// Query the npm registry for the latest published version of `@anthropic-ai/claude-code`.
///
/// Returns `None` if the registry is unreachable, the response is malformed,
/// or the version string cannot be parsed. Never panics.
///
/// # Example
///
/// ```rust,no_run
/// # async fn example() {
/// if let Some(latest) = nexus_claude::cli_download::check_latest_npm_version().await {
///     println!("Latest Claude Code CLI: {}", latest);
/// }
/// # }
/// ```
#[cfg(feature = "auto-download")]
pub async fn check_latest_npm_version() -> Option<crate::transport::subprocess::SemVer> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;

    let resp = client
        .get(npm_registry_latest_url())
        .header("Accept", "application/json")
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        debug!(
            "npm registry returned status {} for version check",
            resp.status()
        );
        return None;
    }

    let body = resp.text().await.ok()?;
    let json: serde_json::Value = serde_json::from_str(&body).ok()?;
    let version = json.get("version")?.as_str()?;
    crate::transport::subprocess::SemVer::parse(version)
}

/// Stub for `check_latest_npm_version` when the `auto-download` feature is disabled.
///
/// Without this the crate did not compile at all with `default-features = false`:
/// the real implementation builds a `reqwest::Client`, and `reqwest` is an
/// optional dependency pulled in by `auto-download`.
#[cfg(not(feature = "auto-download"))]
pub async fn check_latest_npm_version() -> Option<crate::transport::subprocess::SemVer> {
    None
}

/// Ensure the CLI is available, downloading if necessary
///
/// This is the main entry point for CLI management.
#[allow(dead_code)]
pub async fn ensure_cli(auto_download: bool) -> Result<PathBuf> {
    // First, try to find existing CLI
    if let Ok(path) = crate::transport::subprocess::find_claude_cli() {
        return Ok(path);
    }

    // Check cached CLI. `is_file()` matters: without it a *directory* named
    // `claude` in the cache directory was returned as if it were the binary,
    // and the caller only found out when the spawn failed.
    //
    // NOTE: unreachable today, and deliberately kept as a safety net.
    // `find_claude_cli()` above probes `get_cached_cli_path()` itself, with the
    // same `exists() && is_file()` test and *before* it can fail (see
    // `transport::subprocess::find_claude_cli`), so it already returned `Ok`
    // whenever this condition would hold. This block only comes back to life if
    // that probe is ever dropped from `find_claude_cli`.
    if let Some(cached_path) = get_cached_cli_path()
        && cached_path.exists()
        && cached_path.is_file()
    {
        debug!("Using cached CLI at: {}", cached_path.display());
        return Ok(cached_path);
    }

    // Download if auto_download is enabled
    if auto_download {
        info!("Claude Code CLI not found, downloading...");
        return download_cli(None, None).await;
    }

    Err(SdkError::CliNotFound {
        searched_paths: "Claude Code CLI not found.\n\n\
            To automatically download, create the client with auto_download enabled:\n\
            ```rust\n\
            let options = ClaudeCodeOptions::builder()\n\
                .auto_download_cli(true)\n\
                .build();\n\
            ```\n\n\
            Or install manually:\n\
            npm install -g @anthropic-ai/claude-code"
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::path::Path;
    // Only the `auto-download` helpers need shared mutable state.
    #[cfg(feature = "auto-download")]
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    // =====================================================================
    // Test plumbing
    //
    // Every hard-coded endpoint and directory in this module is routed through
    // `test_override()`, so the tests below drive the real download code paths
    // against a local `wiremock` server, a scratch cache directory and a fake
    // `npm`. Nothing here touches the network, the user's cache, or a real
    // Claude CLI. All of these tests mutate process environment variables and
    // are therefore `#[serial]`.
    //
    // What stays uncovered in this module, and why — so the next person does not
    // spend the afternoon re-deriving it:
    //
    // * The `find_cli_in_known_locations()` safety net inside the install-script
    //   branch. Unreachable by construction; the comment at that call site says
    //   why. The *condition* is exercised (see
    //   `test_script_success_with_no_cli_anywhere_reports_both_methods_failed`),
    //   only its `Some` arm is not.
    // * The cached-CLI branch of `ensure_cli`. Same story: `find_claude_cli()`
    //   runs the identical probe first. Documented at the call site.
    // * `set_permissions` failing on the file the npm fallback has just copied.
    //   `chmod` on a regular file the process owns, in a directory it just wrote
    //   to, does not fail; provoking it needs another owner, a file flag or a
    //   read-only mount, i.e. privileges a unit test does not have.
    // * The argument line of every *multi-line* `tracing` macro call — e.g.
    //   `target_path.display()` under `info!(`. `llvm-cov` reports a zero count
    //   for those lines even though the code runs: the argument tokens appear
    //   more than once in the macro expansion, and the counter it maps to the
    //   source line belongs to an expansion branch that is never taken. The
    //   proof is right here — `test_successful_install_is_logged_with_the_target_path`
    //   asserts the formatted path reaches a subscriber, and writing the same
    //   call on one line makes the line leave the report instead of flipping to
    //   covered. Do not chase these with more tests.
    // =====================================================================

    /// Sets environment variables and restores the previous values on drop,
    /// including when the test panics.
    struct EnvGuard {
        saved: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self { saved: Vec::new() }
        }

        fn set(&mut self, key: &str, value: impl AsRef<std::ffi::OsStr>) -> &mut Self {
            self.saved.push((key.to_string(), std::env::var(key).ok()));
            // SAFETY: guarded by `#[serial]`; the previous value is restored on drop.
            unsafe { std::env::set_var(key, value) };
            self
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..).rev() {
                match value {
                    // SAFETY: same as `set`.
                    Some(value) => unsafe { std::env::set_var(&key, value) },
                    None => unsafe { std::env::remove_var(&key) },
                }
            }
        }
    }

    /// Point `get_cache_dir()` at a scratch directory.
    fn with_cache_dir(dir: &Path) -> EnvGuard {
        let mut guard = EnvGuard::new();
        guard.set(ENV_CACHE_DIR, dir);
        guard
    }

    /// Make `get_cache_dir()` answer `None` — what a platform with neither a
    /// home nor a cache directory would do, and what no real platform does.
    fn with_unresolvable_cache_dir() -> EnvGuard {
        let mut guard = EnvGuard::new();
        guard.set(ENV_CACHE_DIR, ENV_CACHE_DIR_UNRESOLVABLE);
        guard
    }

    /// Records every `(downloaded, total)` pair the production code reports.
    #[cfg(feature = "auto-download")]
    type ProgressLog = Arc<Mutex<Vec<(u64, Option<u64>)>>>;

    #[cfg(feature = "auto-download")]
    fn progress_recorder() -> (ProgressLog, ProgressCallback) {
        let log: ProgressLog = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        (
            log,
            Box::new(move |done, total| sink.lock().unwrap().push((done, total))),
        )
    }

    /// Collects everything a thread-local subscriber is handed.
    ///
    /// Only the `auto-download` code paths log anything worth asserting on, so
    /// this and `capture_logs` below would be dead code without that feature.
    #[cfg(feature = "auto-download")]
    #[derive(Clone)]
    struct VecWriter(Arc<Mutex<Vec<u8>>>);

    #[cfg(feature = "auto-download")]
    impl std::io::Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[cfg(feature = "auto-download")]
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for VecWriter {
        type Writer = VecWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Run `body` on a current-thread runtime with a thread-local subscriber at
    /// `level`, and return both its value and what it logged.
    ///
    /// The `tracing` macros do not format their arguments unless a subscriber is
    /// listening, so this is the only way to assert on what the download code
    /// actually reports.
    #[cfg(feature = "auto-download")]
    fn capture_logs<T>(
        level: tracing::Level,
        body: impl std::future::Future<Output = T>,
    ) -> (T, String) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(level)
            .with_ansi(false)
            .with_writer(VecWriter(buffer.clone()))
            .finish();

        let value = tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime")
                .block_on(body)
        });

        (
            value,
            String::from_utf8(buffer.lock().unwrap().clone()).expect("utf-8 log output"),
        )
    }

    /// A path that cannot possibly resolve, used to prove that a code path
    /// never shells out to the real `npm`.
    #[cfg(feature = "auto-download")]
    fn unusable_npm(dir: &Path) -> std::path::PathBuf {
        dir.join("no-such-npm")
    }

    // =====================================================================
    // Cache directory resolution
    // =====================================================================

    #[test]
    #[serial]
    fn test_get_cache_dir() {
        let cache_dir = get_cache_dir();
        assert!(cache_dir.is_some());
        let dir = cache_dir.unwrap();
        assert!(dir.to_string_lossy().contains("cc-sdk"));
    }

    #[test]
    #[serial]
    fn test_get_cached_cli_path() {
        let cli_path = get_cached_cli_path();
        assert!(cli_path.is_some());
        let path = cli_path.unwrap();
        if cfg!(windows) {
            assert!(path.to_string_lossy().ends_with("claude.exe"));
        } else {
            assert!(path.to_string_lossy().ends_with("claude"));
        }
    }

    #[test]
    fn test_cli_version_constants() {
        // Verify version constants are set
        assert!(!MIN_CLI_VERSION.is_empty());
        assert!(!DEFAULT_CLI_VERSION.is_empty());
        assert_eq!(DEFAULT_CLI_VERSION, "latest");

        // Verify MIN_CLI_VERSION is valid semver-ish format
        let parts: Vec<&str> = MIN_CLI_VERSION.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "MIN_CLI_VERSION should be semver format x.y.z"
        );
    }

    #[test]
    #[serial]
    fn test_cache_dir_platform_specific() {
        let cache_dir = get_cache_dir().expect("Should get cache dir");

        #[cfg(target_os = "macos")]
        {
            assert!(cache_dir.to_string_lossy().contains("Library/Caches"));
            assert!(cache_dir.to_string_lossy().contains("cc-sdk/cli"));
        }

        #[cfg(all(unix, not(target_os = "macos")))]
        {
            assert!(
                cache_dir.to_string_lossy().contains(".cache")
                    || cache_dir.to_string_lossy().contains("cache")
            );
            assert!(cache_dir.to_string_lossy().contains("cc-sdk"));
        }

        #[cfg(target_os = "windows")]
        {
            assert!(cache_dir.to_string_lossy().contains("cc-sdk"));
        }
    }

    #[test]
    #[serial]
    fn test_cached_cli_path_is_in_cache_dir() {
        let cache_dir = get_cache_dir().expect("Should get cache dir");
        let cli_path = get_cached_cli_path().expect("Should get cli path");

        // CLI path should be inside cache dir
        assert!(cli_path.starts_with(&cache_dir));

        // CLI should be the executable name
        let cli_name = cli_path.file_name().expect("Should have file name");
        if cfg!(windows) {
            assert_eq!(cli_name, "claude.exe");
        } else {
            assert_eq!(cli_name, "claude");
        }
    }

    /// The scratch-directory seam the rest of the suite relies on: the cached
    /// binary is always `<cache dir>/claude[.exe]`.
    #[test]
    #[serial]
    fn test_cached_cli_path_follows_the_cache_dir() {
        let tmp = TempDir::new().unwrap();
        let _guard = with_cache_dir(tmp.path());

        assert_eq!(get_cache_dir().as_deref(), Some(tmp.path()));
        let expected = tmp.path().join(if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        });
        assert_eq!(get_cached_cli_path(), Some(expected));
    }

    /// Nothing downstream may invent a path when the platform resolves no cache
    /// directory at all.
    #[test]
    #[serial]
    fn test_an_unresolvable_cache_dir_yields_no_cached_cli_path() {
        let _guard = with_unresolvable_cache_dir();

        assert_eq!(get_cache_dir(), None);
        assert_eq!(get_cached_cli_path(), None);
        assert!(!is_cli_cached());
    }

    /// `download_cli` cannot invent one either: its documented answer is a
    /// `ConfigError` naming the cache directory, and it is produced before any
    /// endpoint is contacted.
    #[cfg(feature = "auto-download")]
    #[tokio::test]
    #[serial]
    async fn test_download_cli_reports_a_config_error_when_the_cache_dir_is_unresolvable() {
        let tmp = TempDir::new().unwrap();
        let mut guard = with_unresolvable_cache_dir();
        // Belt and braces: the error is raised before any installer runs, but a
        // regression there must still not reach the real endpoints. Nothing
        // listens on port 1, and the npm path cannot resolve.
        guard
            .set(ENV_INSTALL_SCRIPT_URL, "http://127.0.0.1:1/install.sh")
            .set(ENV_NPM_REGISTRY_URL, "http://127.0.0.1:1/latest")
            .set(ENV_NPM_BIN, unusable_npm(tmp.path()));

        match download_cli(None, None).await.unwrap_err() {
            SdkError::ConfigError(message) => assert_eq!(
                message, "Cannot determine cache directory for CLI download",
                "the error must name what could not be determined"
            ),
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }

    // =====================================================================
    // is_cli_cached
    // =====================================================================

    #[test]
    #[serial]
    fn test_is_cli_cached_when_not_cached() {
        let tmp = TempDir::new().unwrap();
        let _guard = with_cache_dir(tmp.path());
        assert!(
            !is_cli_cached(),
            "an empty cache directory must not report a cached CLI"
        );
    }

    #[test]
    #[serial]
    fn test_is_cli_cached_rejects_a_directory() {
        let tmp = TempDir::new().unwrap();
        let _guard = with_cache_dir(tmp.path());
        std::fs::create_dir_all(get_cached_cli_path().unwrap()).unwrap();

        assert!(
            !is_cli_cached(),
            "a directory named like the binary is not a cached CLI"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn test_is_cli_cached_requires_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let _guard = with_cache_dir(tmp.path());
        let cached = get_cached_cli_path().unwrap();
        std::fs::write(&cached, b"not executable yet").unwrap();

        std::fs::set_permissions(&cached, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_cli_cached(), "mode 0644 must not count as cached");

        std::fs::set_permissions(&cached, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_cli_cached(), "mode 0755 must count as cached");
    }

    // =====================================================================
    // check_latest_npm_version — against a local mock registry
    // =====================================================================

    #[cfg(feature = "auto-download")]
    mod npm_registry {
        use super::*;
        use wiremock::matchers::{header, method, path as url_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        async fn registry(response: ResponseTemplate) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(url_path("/latest"))
                .and(header("accept", "application/json"))
                .respond_with(response)
                .mount(&server)
                .await;
            server
        }

        fn point_at(server: &MockServer) -> EnvGuard {
            let mut guard = EnvGuard::new();
            guard.set(ENV_NPM_REGISTRY_URL, format!("{}/latest", server.uri()));
            guard
        }

        #[tokio::test]
        #[serial]
        async fn test_parses_the_version_published_by_the_registry() {
            let server = registry(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"version": "2.1.281"})),
            )
            .await;
            let _guard = point_at(&server);

            let version = check_latest_npm_version()
                .await
                .expect("a well-formed registry answer must parse");
            assert_eq!((version.major, version.minor, version.patch), (2, 1, 281));
        }

        /// A two-component version is accepted and the patch defaults to 0.
        #[tokio::test]
        #[serial]
        async fn test_accepts_a_two_component_version() {
            let server = registry(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"version": "3.0"})),
            )
            .await;
            let _guard = point_at(&server);

            let version = check_latest_npm_version()
                .await
                .expect("2 components parse");
            assert_eq!((version.major, version.minor, version.patch), (3, 0, 0));
        }

        #[tokio::test]
        #[serial]
        async fn test_http_error_status_yields_none() {
            let server = registry(ResponseTemplate::new(503)).await;
            let _guard = point_at(&server);

            assert!(
                check_latest_npm_version().await.is_none(),
                "a 503 must not be mistaken for a version"
            );
        }

        #[tokio::test]
        #[serial]
        async fn test_malformed_json_yields_none() {
            let server =
                registry(ResponseTemplate::new(200).set_body_string("<html>not json</html>")).await;
            let _guard = point_at(&server);

            assert!(check_latest_npm_version().await.is_none());
        }

        #[tokio::test]
        #[serial]
        async fn test_missing_version_field_yields_none() {
            let server = registry(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"name": "claude-code"})),
            )
            .await;
            let _guard = point_at(&server);

            assert!(check_latest_npm_version().await.is_none());
        }

        /// `"version"` must be a string: a number is rejected rather than
        /// stringified.
        #[tokio::test]
        #[serial]
        async fn test_non_string_version_yields_none() {
            let server = registry(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"version": 2})),
            )
            .await;
            let _guard = point_at(&server);

            assert!(check_latest_npm_version().await.is_none());
        }

        /// An unparsable version string is swallowed, not propagated.
        #[tokio::test]
        #[serial]
        async fn test_unparsable_version_yields_none() {
            let server = registry(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"version": "nightly"})),
            )
            .await;
            let _guard = point_at(&server);

            assert!(check_latest_npm_version().await.is_none());
        }

        /// The rejected status is reported in the log, not just swallowed.
        #[test]
        #[serial]
        fn test_http_error_status_is_logged_with_the_status_code() {
            let (version, logs) = capture_logs(tracing::Level::DEBUG, async {
                let server = registry(ResponseTemplate::new(503)).await;
                let _guard = point_at(&server);
                check_latest_npm_version().await
            });

            assert!(version.is_none());
            assert!(
                logs.contains("npm registry returned status 503"),
                "the rejected status must be logged, got: {logs}"
            );
        }

        /// Nothing is listening on the configured endpoint: the connection
        /// error is swallowed and reported as "unknown version".
        #[tokio::test]
        #[serial]
        async fn test_unreachable_registry_yields_none() {
            let server = MockServer::start().await;
            let uri = server.uri();
            drop(server); // the port is now closed
            let mut guard = EnvGuard::new();
            guard.set(ENV_NPM_REGISTRY_URL, format!("{uri}/latest"));

            assert!(check_latest_npm_version().await.is_none());
        }
    }

    #[tokio::test]
    #[ignore] // Requires network access — run with `cargo test -- --ignored`
    async fn test_check_latest_npm_version_network() {
        let version = check_latest_npm_version().await;
        // Should successfully parse a version from npm
        assert!(version.is_some(), "Should get a version from npm registry");
        let v = version.unwrap();
        assert!(v.major >= 2, "Latest Claude CLI should be >= 2.0.0");
    }

    // =====================================================================
    // Unix install path
    // =====================================================================

    #[cfg(all(unix, feature = "auto-download"))]
    mod unix_install {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use wiremock::matchers::{method, path as url_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        /// Serve `body` as `https://…/install.sh` would.
        async fn script_server(status: u16, body: &str) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(url_path("/install.sh"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&server)
                .await;
            server
        }

        /// Serve the official-installer stand-in that actually installs.
        pub(super) async fn installing_script_server() -> MockServer {
            script_server(200, INSTALLING_SCRIPT).await
        }

        /// Route the install-script URL at `server` and `npm` at `npm`.
        pub(super) fn point_install_at(server: &MockServer, npm: &Path) -> EnvGuard {
            point_at(server, npm)
        }

        fn point_at(server: &MockServer, npm: &Path) -> EnvGuard {
            let mut guard = EnvGuard::new();
            guard
                .set(ENV_INSTALL_SCRIPT_URL, format!("{}/install.sh", server.uri()))
                // No test may ever reach the real npm.
                .set(ENV_NPM_BIN, npm);
            guard
        }

        /// Shell snippet that behaves like the official installer: it drops a
        /// stub binary in `$CLAUDE_INSTALL_DIR`.
        const INSTALLING_SCRIPT: &str = concat!(
            "set -e\n",
            "mkdir -p \"$CLAUDE_INSTALL_DIR\"\n",
            "printf '#!/bin/sh\\nexit 1\\n' > \"$CLAUDE_INSTALL_DIR/claude\"\n",
            "chmod +x \"$CLAUDE_INSTALL_DIR/claude\"\n",
        );

        pub(super) fn write_executable(path: &Path, body: &str) {
            std::fs::write(path, body).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// A fake `npm` that logs its argv, optionally lays down
        /// `<prefix>/node_modules/.bin/claude`, then exits with `exit_code`.
        fn fake_npm(
            dir: &Path,
            args_log: &Path,
            lay_down_binary: bool,
            exit_code: i32,
        ) -> std::path::PathBuf {
            let create = if lay_down_binary {
                "mkdir -p \"$prefix/node_modules/.bin\"\n\
                 printf '#!/bin/sh\\nexit 1\\n' > \"$prefix/node_modules/.bin/claude\"\n\
                 chmod +x \"$prefix/node_modules/.bin/claude\"\n"
            } else {
                ""
            };
            let script = format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$@\" > '{log}'\n\
                 prefix=\"\"\n\
                 while [ \"$#\" -gt 0 ]; do\n\
                 \x20 if [ \"$1\" = \"--prefix\" ]; then prefix=\"$2\"; shift 2; else shift; fi\n\
                 done\n\
                 {create}\
                 exit {exit_code}\n",
                log = args_log.display(),
                create = create,
                exit_code = exit_code,
            );
            let npm = dir.join("npm");
            write_executable(&npm, &script);
            npm
        }

        /// The fixed scratch directory the npm fallback reuses for every run.
        fn npm_scratch_dir() -> std::path::PathBuf {
            std::env::temp_dir().join("cc-sdk-npm-install")
        }

        /// The smallest `PATH` that still resolves `bash` — the install script
        /// branch shells out to it — while containing no `claude` of its own.
        const BASH_ONLY_PATH: &str = "/usr/bin:/bin";

        /// `find_claude_cli()` and `find_cli_in_known_locations()` both probe a
        /// few absolute paths that no environment variable can hide. A machine
        /// with a CLI sitting there cannot run the "nothing is installed"
        /// scenarios at all.
        fn a_cli_sits_at_a_hard_coded_absolute_path() -> bool {
            [
                "/usr/local/bin/claude",
                "/usr/local/bin/claude-code",
                "/opt/homebrew/bin/claude",
                "/opt/homebrew/bin/claude-code",
            ]
            .iter()
            .any(|candidate| Path::new(candidate).is_file())
        }

        #[tokio::test]
        #[serial]
        async fn test_official_script_installing_into_the_cache_is_accepted() {
            let tmp = TempDir::new().unwrap();
            let target = tmp.path().join("claude");
            let server = script_server(200, INSTALLING_SCRIPT).await;
            let _guard = point_at(&server, &unusable_npm(tmp.path()));
            let (progress, callback) = progress_recorder();

            let installed = install_cli_unix("latest", &target, Some(callback))
                .await
                .expect("the install script created the binary at the target path");

            assert_eq!(installed, target);
            assert!(target.is_file());
            assert_eq!(
                *progress.lock().unwrap(),
                vec![(0, None), (100, Some(100))],
                "progress is reported once at the start and once at the end"
            );
        }

        /// A target path with no parent (`/`) aborts the script branch before
        /// anything is executed.
        #[tokio::test]
        #[serial]
        async fn test_target_path_without_a_parent_aborts_the_script_branch() {
            let tmp = TempDir::new().unwrap();
            let server = script_server(200, INSTALLING_SCRIPT).await;
            let _guard = point_at(&server, &unusable_npm(tmp.path()));

            let err = install_cli_unix("latest", &std::path::PathBuf::from("/"), None)
                .await
                .expect_err("`/` has no parent directory to install into");

            assert!(matches!(err, SdkError::CliNotFound { .. }), "got {err:?}");
        }

        /// HTTP failure on the install script: the error is swallowed, and with
        /// no usable npm the caller gets the manual instructions.
        #[tokio::test]
        #[serial]
        async fn test_script_http_error_then_no_npm_reports_both_methods_failed() {
            let tmp = TempDir::new().unwrap();
            let target = tmp.path().join("claude");
            let server = script_server(404, "not found").await;
            let _guard = point_at(&server, &unusable_npm(tmp.path()));

            let err = install_cli_unix("latest", &target, None)
                .await
                .expect_err("nothing could install the CLI");

            match err {
                SdkError::CliNotFound { searched_paths } => {
                    assert!(
                        searched_paths.contains("install script and npm both failed"),
                        "got: {searched_paths}"
                    );
                    assert!(searched_paths.contains("npm install -g @anthropic-ai/claude-code"));
                },
                other => panic!("expected CliNotFound, got {other:?}"),
            }
            assert!(!target.exists(), "nothing must be written on failure");
        }

        /// The script runs but exits non-zero: same outcome, and `find_claude_cli`
        /// is *not* consulted (the fallback chain only runs on success).
        #[tokio::test]
        #[serial]
        async fn test_script_exiting_non_zero_falls_through_to_the_error() {
            let tmp = TempDir::new().unwrap();
            let target = tmp.path().join("claude");
            let server = script_server(200, "echo boom >&2\nexit 3\n").await;
            let _guard = point_at(&server, &unusable_npm(tmp.path()));

            let err = install_cli_unix("latest", &target, None).await.unwrap_err();
            assert!(matches!(err, SdkError::CliNotFound { .. }), "got {err:?}");
        }

        /// The script claims success but installs nothing at the target path:
        /// the function then trusts whatever `find_claude_cli()` /
        /// `find_cli_in_known_locations()` turn up — i.e. a CLI it did not
        /// install — and only errors out when there is none.
        #[tokio::test]
        #[serial]
        async fn test_script_succeeding_without_installing_falls_back_to_lookup() {
            let tmp = TempDir::new().unwrap();
            let target = tmp.path().join("claude");
            let server = script_server(200, "exit 0\n").await;
            let _guard = point_at(&server, &unusable_npm(tmp.path()));

            let preexisting = crate::find_claude_cli()
                .ok()
                .or_else(find_cli_in_known_locations);
            let result = install_cli_unix("latest", &target, None).await;

            match (result, preexisting) {
                (Ok(found), Some(expected)) => {
                    assert_eq!(found, expected);
                    assert_ne!(found, target, "the target path was never created");
                },
                (Err(SdkError::CliNotFound { .. }), None) => {},
                (result, preexisting) => {
                    panic!("lookup said {preexisting:?} but install_cli_unix returned {result:?}")
                },
            }
        }

        /// Same situation, but with `PATH`, `HOME` and the cache directory all
        /// pinned, so the outcome no longer depends on what the machine happens
        /// to have installed: a CLI reachable through `PATH` is accepted, and the
        /// log says where it came from.
        ///
        /// The test above can only assert "whatever the lookup would have found";
        /// on a runner with no CLI at all it never exercises this branch.
        #[test]
        #[serial]
        fn test_script_success_without_installing_accepts_the_cli_on_path() {
            let tmp = TempDir::new().unwrap();
            let bin = tmp.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            write_executable(&bin.join("claude"), "#!/bin/sh\nexit 1\n");
            let cache = tmp.path().join("cache");
            let target = cache.join("claude");

            let (found, logs) = capture_logs(tracing::Level::INFO, async {
                // `exit 0`: the script claims success and installs nothing.
                let server = script_server(200, "exit 0\n").await;
                let mut guard = point_at(&server, &unusable_npm(tmp.path()));
                guard
                    .set("PATH", format!("{}:{}", bin.display(), BASH_ONLY_PATH))
                    .set("HOME", tmp.path())
                    .set(ENV_CACHE_DIR, &cache);
                install_cli_unix("latest", &target, None).await
            });

            let found = found.expect("the CLI reachable through PATH must be accepted");
            assert_eq!(
                std::fs::canonicalize(&found).unwrap(),
                std::fs::canonicalize(bin.join("claude")).unwrap(),
                "the CLI found on PATH must be the one returned"
            );
            assert!(
                !target.exists(),
                "the script installed nothing at the target path"
            );
            assert!(
                logs.contains("Official install script succeeded → found CLI at"),
                "the fallback must say the CLI was found, not installed, got: {logs}"
            );
        }

        /// The script claims success, installs nothing, and no CLI is reachable
        /// anywhere: the whole lookup chain runs to its end —
        /// `find_claude_cli()`, then `find_cli_in_known_locations()` — and the
        /// caller gets the manual instructions.
        #[tokio::test]
        #[serial]
        async fn test_script_success_with_no_cli_anywhere_reports_both_methods_failed() {
            if a_cli_sits_at_a_hard_coded_absolute_path() {
                eprintln!(
                    "skipped: a Claude CLI is installed at an absolute path hard-coded \
                     in the lookup chain, which no test can hide"
                );
                return;
            }

            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            std::fs::create_dir_all(&home).unwrap();
            let cache = tmp.path().join("cache");
            let target = cache.join("claude");

            let server = script_server(200, "exit 0\n").await;
            let mut guard = point_at(&server, &unusable_npm(tmp.path()));
            guard
                .set("PATH", BASH_ONLY_PATH)
                .set("HOME", &home)
                .set(ENV_CACHE_DIR, &cache);

            let err = install_cli_unix("latest", &target, None).await.unwrap_err();
            match err {
                SdkError::CliNotFound { searched_paths } => assert!(
                    searched_paths.contains("install script and npm both failed"),
                    "got: {searched_paths}"
                ),
                other => panic!("expected CliNotFound, got {other:?}"),
            }
            assert!(!target.exists(), "nothing must be written on failure");
        }

        #[tokio::test]
        #[serial]
        async fn test_npm_fallback_copies_the_binary_and_makes_it_executable() {
            let tmp = TempDir::new().unwrap();
            let target = tmp.path().join("claude");
            let args_log = tmp.path().join("npm-args.txt");
            let npm = fake_npm(tmp.path(), &args_log, true, 0);
            let server = script_server(500, "").await;
            let _guard = point_at(&server, &npm);
            let (progress, callback) = progress_recorder();

            let installed = install_cli_unix("2.0.62", &target, Some(callback))
                .await
                .expect("npm fallback should install the CLI");

            assert_eq!(installed, target);
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "the copy must be made executable");
            assert_eq!(*progress.lock().unwrap(), vec![(0, None), (100, Some(100))]);

            let argv = std::fs::read_to_string(&args_log).unwrap();
            assert!(
                argv.contains("@anthropic-ai/claude-code@2.0.62"),
                "an explicit version must be pinned in the npm spec, got: {argv}"
            );
            assert!(argv.contains("--prefix"), "got: {argv}");
            assert!(
                !npm_scratch_dir().exists(),
                "the npm scratch directory must be cleaned up"
            );
        }

        /// `"latest"` is translated to an unpinned package specifier.
        #[tokio::test]
        #[serial]
        async fn test_npm_fallback_requests_the_unpinned_package_for_latest() {
            let tmp = TempDir::new().unwrap();
            let target = tmp.path().join("claude");
            let args_log = tmp.path().join("npm-args.txt");
            let npm = fake_npm(tmp.path(), &args_log, true, 0);
            let server = script_server(500, "").await;
            let _guard = point_at(&server, &npm);

            install_cli_unix("latest", &target, None).await.unwrap();

            let argv = std::fs::read_to_string(&args_log).unwrap();
            assert!(
                argv.contains("@anthropic-ai/claude-code\n")
                    || argv.ends_with("@anthropic-ai/claude-code"),
                "got: {argv}"
            );
            assert!(!argv.contains("claude-code@"), "got: {argv}");
        }

        /// npm exits non-zero: its stderr is logged and the manual instructions
        /// are returned.
        #[tokio::test]
        #[serial]
        async fn test_npm_failure_reports_the_manual_instructions() {
            let tmp = TempDir::new().unwrap();
            let target = tmp.path().join("claude");
            let args_log = tmp.path().join("npm-args.txt");
            let npm = fake_npm(tmp.path(), &args_log, false, 1);
            let server = script_server(500, "").await;
            let _guard = point_at(&server, &npm);

            let err = install_cli_unix("latest", &target, None).await.unwrap_err();
            assert!(matches!(err, SdkError::CliNotFound { .. }), "got {err:?}");
            assert!(args_log.is_file(), "npm must actually have been invoked");
            assert!(!npm_scratch_dir().exists());
        }

        /// npm reports success but leaves no `node_modules/.bin/claude`: the
        /// copy is skipped and the caller gets the manual instructions.
        #[tokio::test]
        #[serial]
        async fn test_npm_success_without_binary_is_treated_as_a_failure() {
            let tmp = TempDir::new().unwrap();
            let target = tmp.path().join("claude");
            let args_log = tmp.path().join("npm-args.txt");
            let npm = fake_npm(tmp.path(), &args_log, false, 0);
            let server = script_server(500, "").await;
            let _guard = point_at(&server, &npm);

            let err = install_cli_unix("latest", &target, None).await.unwrap_err();
            assert!(matches!(err, SdkError::CliNotFound { .. }), "got {err:?}");
            assert!(!target.exists());
        }

        /// The success is logged with the path that was installed.
        #[test]
        #[serial]
        fn test_successful_install_is_logged_with_the_target_path() {
            let tmp = TempDir::new().unwrap();
            let target = tmp.path().join("claude");
            let (installed, logs) = capture_logs(tracing::Level::INFO, async {
                let server = installing_script_server().await;
                let _guard = point_install_at(&server, &unusable_npm(tmp.path()));
                install_cli_unix("latest", &target, None).await
            });

            assert_eq!(installed.unwrap(), target);
            assert!(
                logs.contains("Official install script succeeded"),
                "got: {logs}"
            );
            assert!(
                logs.contains(&target.display().to_string()),
                "the log must name the installed path, got: {logs}"
            );
        }

        /// npm installed the CLI but the destination directory does not exist,
        /// so the copy into the cache fails.
        #[tokio::test]
        #[serial]
        async fn test_npm_fallback_reports_a_config_error_when_the_copy_fails() {
            let tmp = TempDir::new().unwrap();
            // Nothing creates this directory: `fs::copy` into it cannot succeed.
            let target = tmp.path().join("missing-cache").join("claude");
            let args_log = tmp.path().join("npm-args.txt");
            let npm = fake_npm(tmp.path(), &args_log, true, 0);
            let server = script_server(500, "").await;
            let _guard = point_at(&server, &npm);

            let err = install_cli_unix("latest", &target, None).await.unwrap_err();
            match err {
                SdkError::ConfigError(message) => assert!(
                    message.starts_with("Failed to copy CLI to cache"),
                    "got: {message}"
                ),
                other => panic!("expected ConfigError, got {other:?}"),
            }
            assert!(!target.exists());
        }

        /// The npm fallback reuses one fixed scratch path and only ever calls
        /// `remove_dir_all` on it. A leftover *file* with that name is therefore
        /// never cleaned up, and the npm fallback stays broken until someone
        /// deletes it by hand.
        #[tokio::test]
        #[serial]
        async fn test_npm_fallback_is_blocked_by_a_file_at_its_scratch_path() {
            let tmp = TempDir::new().unwrap();
            let target = tmp.path().join("claude");
            let args_log = tmp.path().join("npm-args.txt");
            let npm = fake_npm(tmp.path(), &args_log, true, 0);
            let server = script_server(500, "").await;
            let _guard = point_at(&server, &npm);

            let blocker = npm_scratch_dir();
            let _ = std::fs::remove_dir_all(&blocker);
            std::fs::write(&blocker, b"leftover from an earlier run").unwrap();

            let outcome = install_cli_unix("latest", &target, None).await;
            // Clean up before asserting so a failure cannot poison later runs.
            let _ = std::fs::remove_file(&blocker);

            match outcome.unwrap_err() {
                SdkError::ConfigError(message) => assert!(
                    message.starts_with("Failed to create temp directory"),
                    "got: {message}"
                ),
                other => panic!("expected ConfigError, got {other:?}"),
            }
            assert!(!args_log.exists(), "npm must never have been invoked");
        }

        // -----------------------------------------------------------------
        // find_cli_in_known_locations
        // -----------------------------------------------------------------

        /// `$HOME/.local/bin/claude` — where the official script installs —
        /// wins over the other known locations.
        #[test]
        #[serial]
        fn test_known_locations_prefer_local_bin() {
            let home = TempDir::new().unwrap();
            let mut guard = EnvGuard::new();
            guard.set("HOME", home.path());

            let local_bin = home.path().join(".local/bin");
            std::fs::create_dir_all(&local_bin).unwrap();
            write_executable(&local_bin.join("claude"), "#!/bin/sh\nexit 1\n");
            let claude_local = home.path().join(".claude/local");
            std::fs::create_dir_all(&claude_local).unwrap();
            write_executable(&claude_local.join("claude"), "#!/bin/sh\nexit 1\n");

            assert_eq!(
                find_cli_in_known_locations(),
                Some(local_bin.join("claude")),
                "$HOME must be honoured and ~/.local/bin checked first"
            );
        }

        /// A *directory* at a known location is skipped, the next candidate wins.
        #[test]
        #[serial]
        fn test_known_locations_skip_directories() {
            let home = TempDir::new().unwrap();
            let mut guard = EnvGuard::new();
            guard.set("HOME", home.path());

            std::fs::create_dir_all(home.path().join(".local/bin/claude")).unwrap();
            let claude_local = home.path().join(".claude/local");
            std::fs::create_dir_all(&claude_local).unwrap();
            write_executable(&claude_local.join("claude"), "#!/bin/sh\nexit 1\n");

            assert_eq!(
                find_cli_in_known_locations(),
                Some(claude_local.join("claude"))
            );
        }

        /// Nothing in `$HOME`: only the hard-coded `/usr/local/bin/claude` can
        /// still answer, and it is absent on a normal CI runner.
        #[test]
        #[serial]
        fn test_known_locations_return_none_on_an_empty_home() {
            let home = TempDir::new().unwrap();
            let mut guard = EnvGuard::new();
            guard.set("HOME", home.path());

            let system_wide = std::path::PathBuf::from("/usr/local/bin/claude");
            if system_wide.is_file() {
                assert_eq!(find_cli_in_known_locations(), Some(system_wide));
            } else {
                assert_eq!(find_cli_in_known_locations(), None);
            }
        }
    }

    // =====================================================================
    // download_cli / ensure_cli
    // =====================================================================

    #[cfg(all(unix, feature = "auto-download"))]
    mod download {
        use super::unix_install::*;
        use super::*;

        #[tokio::test]
        #[serial]
        async fn test_download_cli_creates_the_cache_directory_and_installs_into_it() {
            let tmp = TempDir::new().unwrap();
            // Deliberately missing: download_cli must create it.
            let cache = tmp.path().join("nested").join("cli");
            let server = installing_script_server().await;
            let mut guard = point_install_at(&server, &unusable_npm(tmp.path()));
            guard.set(ENV_CACHE_DIR, &cache);

            let installed = download_cli(None, None)
                .await
                .expect("the mocked install script lays the binary down");

            assert_eq!(installed, cache.join("claude"));
            assert!(installed.is_file());
        }

        #[tokio::test]
        #[serial]
        async fn test_download_cli_reports_a_config_error_when_the_cache_cannot_be_created() {
            let tmp = TempDir::new().unwrap();
            let blocker = tmp.path().join("blocker");
            std::fs::write(&blocker, b"regular file").unwrap();
            let server = installing_script_server().await;
            let mut guard = point_install_at(&server, &unusable_npm(tmp.path()));
            // A cache directory *inside a regular file* can never be created.
            guard.set(ENV_CACHE_DIR, blocker.join("cli"));

            let err = download_cli(Some("latest"), None).await.unwrap_err();
            match err {
                SdkError::ConfigError(message) => assert!(
                    message.starts_with("Failed to create cache directory"),
                    "got: {message}"
                ),
                other => panic!("expected ConfigError, got {other:?}"),
            }
        }

        #[tokio::test]
        #[serial]
        async fn test_ensure_cli_returns_the_cached_binary_without_downloading() {
            let tmp = TempDir::new().unwrap();
            let cache = tmp.path().join("cli");
            std::fs::create_dir_all(&cache).unwrap();
            let cached = cache.join("claude");
            write_executable(&cached, "#!/bin/sh\nexit 1\n");

            let mut guard = EnvGuard::new();
            guard
                .set(ENV_CACHE_DIR, &cache)
                .set("HOME", tmp.path())
                .set("PATH", "")
                .set(ENV_NPM_BIN, unusable_npm(tmp.path()));

            // `find_claude_cli()` looks at the SDK cache too, so this asserts the
            // cached binary is honoured — by one of the two checks — and that no
            // download is attempted (there is no install-script URL configured).
            assert_eq!(ensure_cli(false).await.unwrap(), cached);
        }

        /// Regression: `ensure_cli` used to accept *any* existing path in the
        /// cache, so a directory called `claude` was returned as the binary.
        #[tokio::test]
        #[serial]
        async fn test_ensure_cli_refuses_a_directory_sitting_at_the_cached_path() {
            let tmp = TempDir::new().unwrap();
            let cache = tmp.path().join("cli");
            std::fs::create_dir_all(cache.join("claude")).unwrap();

            let mut guard = EnvGuard::new();
            guard
                .set(ENV_CACHE_DIR, &cache)
                .set("HOME", tmp.path())
                .set("PATH", "")
                .set(ENV_NPM_BIN, unusable_npm(tmp.path()));

            let unavoidable = crate::find_claude_cli().ok();
            match (ensure_cli(false).await, unavoidable) {
                (Ok(found), Some(expected)) => assert_eq!(
                    found, expected,
                    "only a CLI found at a hard-coded absolute path may be returned"
                ),
                (Err(SdkError::CliNotFound { searched_paths }), None) => assert!(
                    searched_paths.contains("auto_download_cli(true)"),
                    "got: {searched_paths}"
                ),
                (result, unavoidable) => {
                    panic!("lookup said {unavoidable:?} but ensure_cli returned {result:?}")
                },
            }
        }

        #[tokio::test]
        #[serial]
        async fn test_ensure_cli_downloads_when_auto_download_is_enabled() {
            let tmp = TempDir::new().unwrap();
            let cache = tmp.path().join("cli");
            let home = tmp.path().join("home");
            std::fs::create_dir_all(&home).unwrap();
            let server = installing_script_server().await;
            let mut guard = point_install_at(&server, &unusable_npm(tmp.path()));
            guard
                .set(ENV_CACHE_DIR, &cache)
                .set("HOME", &home)
                // `bash` must stay reachable for the install script to run.
                .set("PATH", "/usr/bin:/bin");

            let unavoidable = crate::find_claude_cli().ok();
            let installed = ensure_cli(true).await.expect("auto-download must succeed");

            match unavoidable {
                Some(expected) => assert_eq!(installed, expected),
                None => {
                    assert_eq!(installed, cache.join("claude"));
                    assert!(installed.is_file());
                },
            }
        }
    }
}
