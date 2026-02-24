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

/// Minimum CLI version required by this SDK
pub const MIN_CLI_VERSION: &str = "2.0.0";

/// Default CLI version to download if not specified
pub const DEFAULT_CLI_VERSION: &str = "latest";

/// Get the cache directory for the SDK
pub fn get_cache_dir() -> Option<PathBuf> {
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
    let cli_name = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
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
            if let Ok(metadata) = path.metadata() {
                return metadata.permissions().mode() & 0o111 != 0;
            }
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

    let install_script_url = "https://claude.ai/install.sh";

    let script_result: Option<PathBuf> = async {
        let client = reqwest::Client::new();
        let response = client.get(install_script_url).send().await.ok()?;

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
    if which::which("npm").is_ok() {
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

        let output = Command::new("npm")
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
                    let mut perms = std::fs::metadata(target_path)
                        .map_err(|e| {
                            SdkError::ConfigError(format!("Failed to get file permissions: {}", e))
                        })?
                        .permissions();
                    perms.set_mode(0o755);
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
    if which::which("npm").is_ok() {
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

        let output = Command::new("npm")
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

    let install_script_url = "https://claude.ai/install.ps1";

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
pub async fn check_latest_npm_version() -> Option<crate::transport::subprocess::SemVer> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;

    let resp = client
        .get("https://registry.npmjs.org/@anthropic-ai/claude-code/latest")
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

/// Ensure the CLI is available, downloading if necessary
///
/// This is the main entry point for CLI management.
#[allow(dead_code)]
pub async fn ensure_cli(auto_download: bool) -> Result<PathBuf> {
    // First, try to find existing CLI
    if let Ok(path) = crate::transport::subprocess::find_claude_cli() {
        return Ok(path);
    }

    // Check cached CLI
    if let Some(cached_path) = get_cached_cli_path()
        && cached_path.exists()
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

    #[test]
    fn test_get_cache_dir() {
        let cache_dir = get_cache_dir();
        assert!(cache_dir.is_some());
        let dir = cache_dir.unwrap();
        assert!(dir.to_string_lossy().contains("cc-sdk"));
    }

    #[test]
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
    fn test_is_cli_cached_when_not_cached() {
        // Since we haven't downloaded anything, CLI should not be cached
        // (unless running on a machine where it was already downloaded)
        // We can't assert false because it might be cached on some machines
        // Just verify the function doesn't panic
        let _ = is_cli_cached();
    }

    #[test]
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

    #[tokio::test]
    #[ignore] // Requires network access — run with `cargo test -- --ignored`
    async fn test_check_latest_npm_version_network() {
        let version = check_latest_npm_version().await;
        // Should successfully parse a version from npm
        assert!(version.is_some(), "Should get a version from npm registry");
        let v = version.unwrap();
        assert!(v.major >= 2, "Latest Claude CLI should be >= 2.0.0");
    }
}
