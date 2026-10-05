//! The `nexus-tools` binary: an MCP server on stdio, or on HTTP with `--listen`.
//!
//! ```text
//! nexus-tools                      # stdio; profile from NEXUS_TOOLS_PROFILE (a signed token)
//! nexus-tools --unrestricted       # stdio, every tool, development only
//! nexus-tools --listen 127.0.0.1:0 # HTTP; every request carries a bearer token
//! ```
//!
//! Secrets arrive in the **environment**, never on the command line: `NEXUS_TOOLS_KEY` (the
//! token signing key, at least 32 bytes) and `NEXUS_TOOLS_PROFILE` (the session's token).

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use nexus_tools::http;
use nexus_tools::limits::DEFAULT_MAX_OUTPUT_CHARS;
use nexus_tools::{Profile, Server, Session, SigningKey, ToolRegistry, serve_lines, verify};
use tokio::io::BufReader;

const USAGE: &str = "usage: nexus-tools [--listen ADDR] [--allow-origin ORIGIN]... \
[--max-output-chars N] [--unrestricted]\n\
environment: NEXUS_TOOLS_KEY (signing key, >= 32 bytes), NEXUS_TOOLS_PROFILE (stdio token), \
NEXUS_TOOLS_LOG";

struct Options {
    listen: Option<SocketAddr>,
    allow_origins: Vec<String>,
    max_output_chars: usize,
    unrestricted: bool,
}

fn fail(message: &str) -> ExitCode {
    eprintln!("nexus-tools: {message}");
    ExitCode::from(2)
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        listen: None,
        allow_origins: Vec::new(),
        max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
        unrestricted: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--listen" => {
                options.listen = Some(
                    value("--listen")?
                        .parse()
                        .map_err(|_| "--listen takes an address like 127.0.0.1:8765")?,
                );
            },
            "--allow-origin" => options.allow_origins.push(value("--allow-origin")?),
            "--max-output-chars" => {
                options.max_output_chars = value("--max-output-chars")?
                    .parse()
                    .map_err(|_| "--max-output-chars takes a number")?;
            },
            "--unrestricted" => options.unrestricted = true,
            "--version" => {
                println!("nexus-tools {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            },
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            },
            other => return Err(format!("unknown argument: {other}\n{USAGE}")),
        }
    }
    Ok(options)
}

fn registry() -> ToolRegistry {
    #[allow(unused_mut)]
    let mut registry = ToolRegistry::new();
    #[cfg(feature = "test-tools")]
    {
        registry = registry
            .with(nexus_tools::testing::EchoTool)
            .with(nexus_tools::testing::WriteTool);
    }
    registry
}

/// The signing key from the environment. The value is never printed, not even on failure.
fn key_from_env() -> Result<SigningKey, String> {
    let raw = std::env::var("NEXUS_TOOLS_KEY").map_err(|_| {
        "NEXUS_TOOLS_KEY is required (a signing key of at least 32 bytes)".to_owned()
    })?;
    SigningKey::new(raw.into_bytes()).map_err(|error| format!("NEXUS_TOOLS_KEY: {error}"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() -> ExitCode {
    let options = match parse_args() {
        Ok(options) => options,
        Err(message) => return fail(&message),
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("NEXUS_TOOLS_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(&format!("cannot start the runtime: {error}")),
    };
    runtime.block_on(run(options))
}

async fn run(options: Options) -> ExitCode {
    let server = Arc::new(Server::new(registry()).with_max_output_chars(options.max_output_chars));
    if let Some(address) = options.listen {
        // HTTP always needs the key: without it the server could not tell a session from a
        // stranger, on a loopback address as much as on a public one.
        let key = match key_from_env() {
            Ok(key) => key,
            Err(message) => return fail(&message),
        };
        let listener = match tokio::net::TcpListener::bind(address).await {
            Ok(listener) => listener,
            Err(error) => return fail(&format!("cannot listen on {address}: {error}")),
        };
        let bound = match listener.local_addr() {
            Ok(bound) => bound,
            Err(error) => return fail(&format!("cannot read the bound address: {error}")),
        };
        println!("LISTENING {bound}");
        let router = http::router(server, key, options.allow_origins, bound);
        return match http::serve(listener, router).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&format!("server error: {error}")),
        };
    }

    // stdio
    let profile = match std::env::var("NEXUS_TOOLS_PROFILE") {
        Ok(token) => {
            let key = match key_from_env() {
                Ok(key) => key,
                Err(message) => return fail(&message),
            };
            match verify(&key, &token, now()) {
                Ok(profile) => profile,
                Err(_) => return fail("NEXUS_TOOLS_PROFILE is not a valid, unexpired token"),
            }
        },
        Err(_) if options.unrestricted => {
            eprintln!(
                "nexus-tools: WARNING: --unrestricted gives the session EVERY tool; development only"
            );
            Profile::unrestricted("dev")
        },
        Err(_) => {
            return fail("stdio mode needs NEXUS_TOOLS_PROFILE (a signed token) or --unrestricted");
        },
    };
    serve_lines(
        server,
        Session::new(profile),
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await;
    ExitCode::SUCCESS
}
