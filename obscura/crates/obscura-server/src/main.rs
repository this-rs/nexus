//! obscura-server — Obscura (the Rust headless browser) as one command.
//!
//! Two entry points:
//! - `mcp`  — MCP stdio server exposing the `browser_*` tools.
//! - `serp` — one-shot keyless web search printing a JSON array of
//!   `{url, title, snippet}`; this is what nexus-tools' `obscura:<path>` search engine runs.

use clap::{Parser, Subcommand};
use obscura_mcp::{serp_search, BrowserState};

#[derive(Parser)]
#[command(
    name = "obscura-server",
    version,
    about = "Headless navigation + keyless web search, powered by Obscura"
)]
struct Cli {
    /// HTTP(S) or SOCKS proxy URL applied to all navigation (e.g. socks5://127.0.0.1:1080).
    #[arg(long, global = true)]
    proxy: Option<String>,

    /// Override the browser User-Agent.
    #[arg(long, global = true)]
    user_agent: Option<String>,

    /// Enable stealth mode (anti-detection + tracker blocking). Requires a
    /// stealth build (`--features stealth`).
    #[arg(long, global = true)]
    stealth: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the MCP server over stdio (JSON-RPC 2.0, newline-delimited).
    Mcp,
    /// One-shot keyless web search; prints a JSON array of {url,title,snippet}.
    Serp {
        /// Search query.
        query: String,
        /// Max results (default 10).
        #[arg(long, default_value_t = 10)]
        max_results: usize,
        /// Search engine (default duckduckgo).
        #[arg(long, default_value = "duckduckgo")]
        engine: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Mcp => {
            obscura_mcp::run(cli.proxy, cli.user_agent, cli.stealth).await?;
            Ok(())
        }
        Command::Serp { query, max_results, engine } => {
            // Fresh state per invocation: one-shot, no session persistence.
            let mut state = BrowserState::new(cli.proxy, cli.user_agent, cli.stealth);
            let results = serp_search(&mut state, &query, &engine, max_results)
                .await
                .map_err(anyhow::Error::msg)?;
            println!("{}", serde_json::to_string_pretty(&results)?);
            Ok(())
        }
    }
}
