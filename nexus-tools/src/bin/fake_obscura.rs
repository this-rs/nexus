//! `fake_obscura mcp`: an MCP server that imitates the **tool list of `obscura mcp`** as its
//! documentation gives it (names only, no annotations: Obscura does not mark reads), to test how
//! the harness attaches and classifies a browser without having one (N23).
//!
//! `FAKE_OBSCURA_LOG` names a file; the server appends one JSON line at start (its argv and the
//! NAMES of its environment variables) and one per tool call.

use std::io::Write;
use std::sync::Arc;

use async_trait::async_trait;
use nexus_tools::{
    CallContext, Profile, Server, Session, Tool, ToolRegistry, ToolResult, serve_lines,
};
use serde_json::{Value, json};

const TOOLS: &[&str] = &[
    "browser_navigate",
    "browser_back",
    "browser_forward",
    "browser_reload",
    "browser_close",
    "browser_snapshot",
    "browser_markdown",
    "browser_links",
    "browser_extract",
    "browser_interactive_elements",
    "browser_detect_forms",
    "browser_get_attribute",
    "browser_count",
    "browser_search",
    "browser_network_requests",
    "browser_console_messages",
    "browser_screenshot",
    "browser_pdf",
    "browser_click",
    "browser_fill",
    "browser_fill_form",
    "browser_type",
    "browser_press_key",
    "browser_select_option",
    "browser_scroll",
    "browser_wait_for",
    "browser_wait_for_text",
    "browser_evaluate",
    "browser_get_cookies",
    "browser_storage_state",
    "browser_set_cookie",
    "browser_clear_cookies",
    "browser_set_storage_state",
    "browser_tab_new",
    "browser_tab_list",
    "browser_tab_switch",
    "browser_tab_close",
];

fn log(line: &Value) {
    if let Ok(path) = std::env::var("FAKE_OBSCURA_LOG")
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

struct Fake(&'static str);

#[async_trait]
impl Tool for Fake {
    fn name(&self) -> &str {
        self.0
    }

    fn description(&self) -> &str {
        "A browser tool (fake)."
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn call(&self, _context: &CallContext, arguments: Value) -> ToolResult {
        log(&json!({"call": self.0, "arguments": arguments}));
        ToolResult::ok(format!("ok: {}", self.0))
    }
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let names: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    let private = std::env::var("OBSCURA_ALLOW_PRIVATE_NETWORK").ok();
    log(&json!({"start": {"argv": argv, "env_names": names, "allow_private": private}}));
    if argv.first().map(String::as_str) != Some("mcp") {
        eprintln!("fake_obscura: usage: fake_obscura mcp");
        std::process::exit(2);
    }
    let mut registry = ToolRegistry::new();
    for name in TOOLS {
        registry = registry.with(Fake(name));
    }
    serve_lines(
        Arc::new(Server::new(registry)),
        Session::new(Profile::unrestricted("fake")),
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await;
}
