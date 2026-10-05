//! The optional browser (N23): Obscura attached as an **external MCP server**, never compiled in.
//!
//! Obscura runs JavaScript through V8, built from C++ sources; embedding it would put a C++
//! toolchain in our build and V8 in our process (its own documentation says it does not contain a
//! hostile page that exploits V8). So it is a separate executable the operator installs and
//! authorises (`BrowserTools` configured on the instance), launched like any MCP server —
//! empty environment, allow-list, a `HOME` of its own — and spoken to over stdio.
//!
//! What this module decides:
//! - **Absent is absent.** No executable, no `browser_*` tool and a typed notice; never a tool
//!   that fails silently when the model reaches for it.
//! - **Stealth is never asked for.** `--stealth` (anti-detection) and `--allow-private-network`
//!   are refused when the configuration is built; the server is started as `obscura mcp` with
//!   `OBSCURA_ALLOW_PRIVATE_NETWORK=0`. What a site's terms and the local law allow is the
//!   operator's business, not something nexus switches on.
//! - **Private networks are refused twice**: by Obscura (its SSRF protection is on by default)
//!   and by the harness before the call, for the addresses it can judge without a DNS lookup.
//! - **Reading is not interacting.** The documented tools are classified by name — Obscura
//!   does not annotate them — into reads (no approval), navigation (asks like `WebFetch`) and
//!   interactions (click, type, evaluate JavaScript, cookies: always a decision).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::agent::{McpServerSpec, ProviderError, ToolCategory};
use crate::model::guard::{IpClass, classify_ip};

/// The name under which the browser is attached; its tools are `mcp__browser__browser_*`.
pub const BROWSER_SERVER: &str = "browser";

/// Flags nexus refuses to pass.
const FORBIDDEN_FLAGS: &[&str] = &["--stealth", "--allow-private-network"];

/// How to start the browser.
#[derive(Debug, Clone)]
pub struct BrowserTools {
    /// The `obscura` executable.
    pub program: PathBuf,
    args: Vec<String>,
    /// Environment of the server, on top of the safe defaults.
    pub env: BTreeMap<String, String>,
}

impl BrowserTools {
    /// The browser at `program`, started as `obscura mcp`.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    /// Extra arguments (a proxy, a user agent). Stealth and private-network flags are refused.
    pub fn with_args(
        mut self,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, ProviderError> {
        let args: Vec<String> = args.into_iter().map(Into::into).collect();
        if let Some(bad) = args.iter().find(|a| {
            FORBIDDEN_FLAGS
                .iter()
                .any(|f| a == f || a.starts_with(&format!("{f}=")))
        }) {
            return Err(ProviderError::invalid(format!(
                "the browser is never started with `{bad}`: stealth mode and private-network access are not switched on by nexus"
            )));
        }
        self.args = args;
        Ok(self)
    }

    /// Looks for `obscura` in the `PATH`.
    pub fn locate() -> Option<Self> {
        let name = format!("obscura{}", std::env::consts::EXE_SUFFIX);
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(&name))
                .find(|path| path.is_file())
                .map(Self::new)
        })
    }

    /// Whether the executable is there.
    pub fn is_installed(&self) -> bool {
        self.program.is_file()
    }

    /// The MCP server entry.
    pub fn server(&self) -> McpServerSpec {
        let mut args = vec!["mcp".to_owned()];
        args.extend(self.args.iter().cloned());
        let mut env = BTreeMap::new();
        env.insert("OBSCURA_ALLOW_PRIVATE_NETWORK".to_owned(), "0".to_owned());
        env.extend(self.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        // The value above is not negotiable.
        env.insert("OBSCURA_ALLOW_PRIVATE_NETWORK".to_owned(), "0".to_owned());
        McpServerSpec::Stdio {
            command: self.program.display().to_string(),
            args,
            env,
        }
    }
}

/// How the policy treats a documented `browser_*` tool: its category, and whether it is a
/// read (offered in `plan_only`, never asked about). Unknown `browser_*` tools are treated as
/// interactions: a tool we have not classified is not assumed harmless.
pub(crate) fn profile(tool: &str) -> (ToolCategory, bool) {
    match tool {
        // Inspect what is already loaded.
        "browser_snapshot"
        | "browser_markdown"
        | "browser_links"
        | "browser_extract"
        | "browser_interactive_elements"
        | "browser_detect_forms"
        | "browser_get_attribute"
        | "browser_count"
        | "browser_search"
        | "browser_network_requests"
        | "browser_console_messages"
        | "browser_screenshot"
        | "browser_pdf"
        | "browser_wait_for"
        | "browser_wait_for_text"
        | "browser_get_cookies"
        | "browser_storage_state"
        | "browser_tab_list" => (ToolCategory::Read, true),
        // Move to another page: network access, asked like `WebFetch`.
        "browser_navigate" | "browser_back" | "browser_forward" | "browser_reload"
        | "browser_tab_new" | "browser_tab_switch" | "browser_tab_close" | "browser_close" => {
            (ToolCategory::Web, false)
        },
        // Act on a page or the browser's state: click, type, fill, JavaScript, cookies.
        _ => (ToolCategory::Web, false),
    }
}

/// The tools whose `url` argument is a destination.
fn takes_url(tool: &str) -> bool {
    matches!(tool, "browser_navigate" | "browser_tab_new")
}

/// Refuses, before the call, a destination that is not an ordinary public web address that can
/// be judged without a DNS lookup: another scheme (`file:`, `javascript:`, `data:`), a name that
/// means "this machine or this network", an IP literal outside the public ranges (loopback
/// included: the browser is not a way to read the developer's own services).
pub(crate) fn guard_call(tool: &str, input: &Value) -> Result<(), String> {
    if !takes_url(tool) {
        return Ok(());
    }
    let Some(text) = input.get("url").and_then(Value::as_str) else {
        return Ok(()); // `browser_tab_new` may open a blank tab
    };
    let url = url::Url::parse(text.trim()).map_err(|_| "the URL is not valid".to_owned())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "only http and https addresses can be opened, not `{}`",
            url.scheme()
        ));
    }
    let refuse = |what: &str| {
        Err(format!(
            "the browser does not open {what}: private networks are off limits"
        ))
    };
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip_check(std::net::IpAddr::V4(ip)).or_else(|c| refuse(&c)),
        Some(url::Host::Ipv6(ip)) => ip_check(std::net::IpAddr::V6(ip)).or_else(|c| refuse(&c)),
        Some(url::Host::Domain(name)) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            let local = name == "localhost"
                || name.ends_with(".localhost")
                || name.ends_with(".local")
                || name.ends_with(".internal")
                || name.ends_with(".lan")
                || !name.contains('.');
            if local {
                refuse("a local or internal name")
            } else {
                Ok(())
            }
        },
        None => Err("the URL has no host".to_owned()),
    }
}

fn ip_check(ip: std::net::IpAddr) -> Result<(), String> {
    match classify_ip(ip) {
        IpClass::Public => Ok(()),
        other => Err(format!("a {other} address")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn guard(url: &str) -> Result<(), String> {
        guard_call("browser_navigate", &json!({"url": url}))
    }

    #[test]
    fn stealth_and_private_network_flags_are_refused_at_configuration() {
        for bad in ["--stealth", "--allow-private-network", "--stealth=1"] {
            let error = BrowserTools::new("obscura").with_args([bad]).unwrap_err();
            assert_eq!(error.kind(), "invalid_request", "{bad}");
        }
        assert!(
            BrowserTools::new("obscura")
                .with_args(["--user-agent", "x"])
                .is_ok()
        );
    }

    #[test]
    fn the_server_is_started_as_obscura_mcp_with_private_networks_off() {
        let McpServerSpec::Stdio { command, args, env } =
            BrowserTools::new("/opt/obscura").server()
        else {
            panic!()
        };
        assert_eq!(command, "/opt/obscura");
        assert_eq!(args, ["mcp"]);
        assert_eq!(
            env.get("OBSCURA_ALLOW_PRIVATE_NETWORK").map(String::as_str),
            Some("0")
        );
        // An environment entry cannot turn it back on.
        let mut tools = BrowserTools::new("/opt/obscura");
        tools
            .env
            .insert("OBSCURA_ALLOW_PRIVATE_NETWORK".into(), "1".into());
        let McpServerSpec::Stdio { env, .. } = tools.server() else {
            panic!()
        };
        assert_eq!(env["OBSCURA_ALLOW_PRIVATE_NETWORK"], "0");
    }

    #[test]
    fn reads_are_read_only_navigation_is_web_and_everything_else_is_an_interaction() {
        for read in [
            "browser_snapshot",
            "browser_markdown",
            "browser_links",
            "browser_extract",
            "browser_get_cookies",
        ] {
            assert_eq!(profile(read), (ToolCategory::Read, true), "{read}");
        }
        for web in ["browser_navigate", "browser_back", "browser_tab_new"] {
            assert_eq!(profile(web), (ToolCategory::Web, false), "{web}");
        }
        for act in [
            "browser_click",
            "browser_fill",
            "browser_type",
            "browser_evaluate",
            "browser_set_cookie",
            "browser_clear_cookies",
            "browser_press_key",
            "browser_not_yet_known",
        ] {
            assert_eq!(profile(act), (ToolCategory::Web, false), "{act}");
        }
    }

    #[test]
    fn private_and_non_web_destinations_are_refused_before_the_call() {
        for url in [
            "http://127.0.0.1/",
            "http://localhost:8080/",
            "http://app.localhost/",
            "http://10.0.0.5/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://2130706433/",
            "http://0x7f.1/",
            "http://intranet/",
            "http://printer.local/",
            "http://db.internal/",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,hi",
            "ftp://example.com/",
            "chrome://settings",
        ] {
            assert!(guard(url).is_err(), "{url} should be refused");
        }
        for url in [
            "https://example.com/",
            "http://93.184.216.34/",
            "https://docs.rs/serde?q=1",
        ] {
            assert_eq!(guard(url), Ok(()), "{url}");
        }
        // Only the tools that take a destination are checked; a blank tab has no URL.
        assert_eq!(
            guard_call("browser_click", &json!({"url": "http://127.0.0.1/"})),
            Ok(())
        );
        assert_eq!(guard_call("browser_tab_new", &json!({})), Ok(()));
        assert!(guard_call("browser_navigate", &json!({"url": "not a url"})).is_err());
    }
}
