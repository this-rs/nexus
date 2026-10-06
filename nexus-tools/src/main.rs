//! The `nexus-tools` binary: an MCP server on stdio, or on HTTP with `--listen`.
//!
//! ```text
//! nexus-tools                      # stdio; profile from NEXUS_TOOLS_PROFILE (a signed token)
//! nexus-tools --unrestricted       # stdio, every tool, development only
//! nexus-tools --listen 127.0.0.1:0 # HTTP; every request carries a bearer token
//! nexus-tools --trust-harness --cwd DIR --tools Read,Grep  # one harness session, stdio only
//! ```
//!
//! One process per session, bounded at launch (N27): `--cwd`/`--add-dir` are its whole file
//! scope and `--tools` the only tools it serves, listed or called, whatever the profile or the
//! client asks (an intersection with a signed profile, never wider). `--trust-harness` (no
//! signed profile: the harness is the only client and enforces the policy) is refused with
//! `--listen`: a socket is not a private pipe.
//!
//! Secrets arrive in the **environment**, never on the command line: `NEXUS_TOOLS_KEY` (the
//! token signing key, at least 32 bytes) and `NEXUS_TOOLS_PROFILE` (the session's token).

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use nexus_tools::files::{FileConfig, Scope};
use nexus_tools::http;
use nexus_tools::limits::DEFAULT_MAX_OUTPUT_CHARS;
use nexus_tools::registry::CANONICAL_TOOLS;
use nexus_tools::{Profile, Server, Session, SigningKey, ToolRegistry, serve_lines, verify};
use tokio::io::BufReader;

const USAGE: &str = "usage: nexus-tools [--listen ADDR] [--allow-origin ORIGIN]... \
[--max-output-chars N] [--cwd DIR] [--add-dir DIR]... [--backup-dir DIR] [--search-engine SPEC]... [--search-allow-private] [--brave-endpoint URL] [--tools NAME,...]... [--unrestricted | --trust-harness]\n\
environment: NEXUS_TOOLS_KEY (signing key, >= 32 bytes), NEXUS_TOOLS_PROFILE (stdio token), \
NEXUS_TOOLS_LOG";

struct Options {
    listen: Option<SocketAddr>,
    allow_origins: Vec<String>,
    max_output_chars: usize,
    unrestricted: bool,
    /// Served to one harness over a private pipe: it enforces the policy, no signed profile.
    trust_harness: bool,
    cwd: Option<std::path::PathBuf>,
    add_dirs: Vec<std::path::PathBuf>,
    backup_dir: Option<std::path::PathBuf>,
    /// `brave:ENVVAR`, `searxng:URL` or `html`, in the order they are tried.
    search_engines: Vec<String>,
    search_allow_private: bool,
    brave_endpoint: Option<String>,
    /// `--tools`: the only tools this process serves, whatever the profile. Repeated, each
    /// narrows the previous (an intersection). `None`: every tool of the build.
    tools: Option<BTreeSet<String>>,
}

/// The one-line reason `--trust-harness` and `--listen` do not go together.
const TRUST_HARNESS_IS_STDIO: &str =
    "--trust-harness is for one harness over a private pipe; it cannot be combined with --listen";

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
        trust_harness: false,
        cwd: None,
        add_dirs: Vec::new(),
        backup_dir: None,
        search_engines: Vec::new(),
        search_allow_private: false,
        brave_endpoint: None,
        tools: None,
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
            "--cwd" => options.cwd = Some(value("--cwd")?.into()),
            "--add-dir" => options.add_dirs.push(value("--add-dir")?.into()),
            "--backup-dir" => options.backup_dir = Some(value("--backup-dir")?.into()),
            "--search-engine" => options.search_engines.push(value("--search-engine")?),
            "--search-allow-private" => options.search_allow_private = true,
            "--brave-endpoint" => options.brave_endpoint = Some(value("--brave-endpoint")?),
            "--tools" => {
                let named: BTreeSet<String> = value("--tools")?
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect();
                options.tools = Some(match options.tools.take() {
                    Some(previous) => previous.intersection(&named).cloned().collect(),
                    None => named,
                });
            },
            "--unrestricted" => options.unrestricted = true,
            "--trust-harness" => options.trust_harness = true,
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
    // The harness is trusted because it is the only client of a private pipe; a socket has no
    // such guarantee, so HTTP always verifies a signed token per request.
    if options.trust_harness && options.listen.is_some() {
        return Err(TRUST_HARNESS_IS_STDIO.to_owned());
    }
    Ok(options)
}

fn registry(options: &Options) -> Result<ToolRegistry, String> {
    let cwd = match &options.cwd {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().map_err(|e| format!("no working directory: {e}"))?,
    };
    // Outputs of shell commands live here; it is part of the scope so that `Read` can open
    // the output file of a background task.
    let output_dir = std::env::temp_dir().join(format!("nexus-tools-{}", std::process::id()));
    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("cannot create {}: {e}", output_dir.display()))?;
    let mut extra = options.add_dirs.clone();
    extra.push(output_dir.clone());
    let scope =
        Scope::new(&cwd, extra).map_err(|e| format!("cannot use the session directories: {e}"))?;
    let mut files = FileConfig::new(scope);
    if let Some(dir) = &options.backup_dir {
        files = files.with_backup_dir(dir);
    }
    #[allow(unused_mut)]
    let mut registry = nexus_tools::files::register(ToolRegistry::new(), &files);
    #[cfg(unix)]
    {
        let shell = nexus_tools::shell::ShellConfig::new(files.scope(), &output_dir)
            .map_err(|e| format!("cannot prepare the shell tools: {e}"))?;
        registry = nexus_tools::shell::register(registry, &shell);
    }
    #[cfg(feature = "test-tools")]
    {
        registry = registry
            .with(nexus_tools::testing::EchoTool)
            .with(nexus_tools::testing::WriteTool);
    }
    // WebFetch: strict by construction (public addresses only, http upgraded to https). This
    // build has no TLS backend yet, so https fetches fail with a typed error (N21).
    registry =
        nexus_tools::web::register(registry, nexus_tools::web::Fetcher::new(Default::default()));
    registry = nexus_tools::search::register(registry, search_engine(options)?);
    match &options.tools {
        Some(names) => bounded(registry, names),
        None => Ok(registry),
    }
}

/// The registry cut down to `names` (`--tools`). A name that is neither canonical nor served by
/// this build is a mistake, refused at start with the valid names; a canonical name this platform
/// does not serve (`Bash` on Windows) is accepted and simply absent.
fn bounded(registry: ToolRegistry, names: &BTreeSet<String>) -> Result<ToolRegistry, String> {
    let valid: BTreeSet<&str> = CANONICAL_TOOLS
        .iter()
        .copied()
        .chain(registry.names())
        .collect();
    let unknown: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|name| !valid.contains(name))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "--tools: unknown tool name(s): {}; valid names (canonical, case included): {}",
            unknown.join(", "),
            valid.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(registry.retain_only(names))
}

/// An environment variable name: letters, digits and underscores, not starting with a digit. A
/// secret pasted here by mistake (`sk-live-…` has dashes) is refused without being echoed.
fn is_variable_name(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with(|c: char| c.is_ascii_digit())
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The search engines asked for on the command line. A key is never given here, only the NAME of
/// the environment variable that holds it. `html` (a keyless, fragile scraper) is built only
/// because it was named.
fn search_engine(options: &Options) -> Result<nexus_tools::search::Engine, String> {
    use nexus_tools::search::{
        BRAVE_ENDPOINT, DDG_HTML_ENDPOINT, Engine, HtmlBackend, KeySource, KeyedApiBackend,
        Protection, SearxngBackend,
    };
    use nexus_tools::web::{FetchConfig, Fetcher, SystemClock};
    let fetcher = || {
        Arc::new(Fetcher::new(FetchConfig {
            allow_private_network: options.search_allow_private,
            // The operator wrote the scheme; do not rewrite it.
            upgrade_http: false,
            ..FetchConfig::default()
        }))
    };
    let mut engine = Engine::new(Arc::new(SystemClock::default()));
    for spec in &options.search_engines {
        let protection = Protection::default();
        engine = match spec.split_once(':') {
            Some(("brave", variable)) if is_variable_name(variable) => engine.with_backend(
                KeyedApiBackend::new(
                    "brave",
                    options
                        .brave_endpoint
                        .clone()
                        .unwrap_or_else(|| BRAVE_ENDPOINT.to_owned()),
                    KeySource::Env(variable.to_owned()),
                    fetcher(),
                ),
                protection,
            ),
            Some(("searxng", url)) if !url.is_empty() => {
                engine.with_backend(SearxngBackend::new(url, fetcher()), protection)
            },
            None if spec == "html" => {
                eprintln!(
                    "nexus-tools: WARNING: the html search engine scrapes a page meant for people; it is fragile and the engine's terms may not allow it"
                );
                engine.with_backend(
                    HtmlBackend::explicitly_enabled(DDG_HTML_ENDPOINT, fetcher()),
                    protection,
                )
            },
            _ => {
                // The spec is NOT echoed: a key pasted by mistake must not reach a log.
                return Err(
                    "--search-engine takes brave:ENVVAR, searxng:URL or html (a key is never given here, only the NAME of the environment variable that holds it)"
                        .to_owned(),
                );
            },
        };
    }
    Ok(engine)
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
    let code = runtime.block_on(run(options));
    // Outputs of shell commands are scratch: they do not outlive the server.
    let _ = std::fs::remove_dir_all(
        std::env::temp_dir().join(format!("nexus-tools-{}", std::process::id())),
    );
    code
}

async fn run(options: Options) -> ExitCode {
    let registry = match registry(&options) {
        Ok(registry) => registry,
        Err(message) => return fail(&message),
    };
    let server = Arc::new(Server::new(registry).with_max_output_chars(options.max_output_chars));
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
        // One harness, over a private pipe (N24): it enforces the session's policy before any call
        // and is the only client, so there is no profile to sign and nothing to warn about.
        Err(_) if options.trust_harness => Profile::unrestricted("harness"),
        Err(_) if options.unrestricted => {
            eprintln!(
                "nexus-tools: WARNING: --unrestricted gives the session EVERY tool; development only"
            );
            Profile::unrestricted("dev")
        },
        Err(_) => {
            return fail(
                "stdio mode needs NEXUS_TOOLS_PROFILE (a signed token), --trust-harness or --unrestricted",
            );
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
