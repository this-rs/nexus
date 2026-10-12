//! Obscura's one-shot keyless search (`<obscura> serp`), as an **external executable** (N22).
//!
//! Obscura is a headless browser built on V8 (C++): it is never linked into nexus-tools (N17,
//! N23). The operator installs it and names it (`--search-engine obscura:/path/to/binary`, or
//! `obscura` alone to find it on the `PATH`); each query starts it once:
//!
//! ```text
//! <program> serp --max-results N -- <query>
//! ```
//!
//! - **No shell.** The query is ONE argument, after `--`: a query that starts with `-` is not a
//!   flag, and quotes, `;` or `$(...)` are just text.
//! - **Its own process group, killed on timeout and on cancel** (N20): a call that is abandoned
//!   (the request was cancelled, the session ended) does not leave a browser running.
//! - **A clean environment**: an allow-list (`PATH`, locale, `TZ`), and
//!   `OBSCURA_ALLOW_PRIVATE_NETWORK=0` whatever the host says.
//! - **Absent is said**: a missing executable is a `not_configured` error that names it, never a
//!   silent empty answer.
//!
//! The answer is a JSON array of `{url, title, snippet}`; domain filters, merging and labelling
//! are the tool's, as for every engine.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::backend::{SearchBackend, SearchError, SearchHit, SearchQuery};
use super::util::plain_text;

/// Executable names looked for on the `PATH` when the engine is named without a path: the
/// distribution that ships `serp` first, then the plain name.
pub const OBSCURA_NAMES: [&str; 2] = ["dsh-obscura-nav", "obscura"];

/// How long one query may take: Obscura starts a browser and loads a results page.
pub const OBSCURA_TIMEOUT: Duration = Duration::from_secs(30);

/// The most we read of its standard output; more is not a search answer.
const MAX_STDOUT: u64 = 2 * 1024 * 1024;
/// The most we keep of its standard error, for the message.
const MAX_STDERR: u64 = 16 * 1024;

/// Host variables the browser may see. Everything else is dropped.
const INHERITED: [&str; 5] = ["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TZ"];

/// Obscura's `serp` subcommand, at `program`.
#[derive(Debug, Clone)]
pub struct ObscuraBackend {
    program: PathBuf,
    timeout: Duration,
}

impl ObscuraBackend {
    /// The engine at `program`. Nothing is checked here: a missing executable is reported when
    /// it is asked, by name.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            timeout: OBSCURA_TIMEOUT,
        }
    }

    /// The executable found on `path` (a `PATH`-style list) under one of [`OBSCURA_NAMES`].
    pub fn locate_in(path: &std::ffi::OsStr) -> Option<Self> {
        let dirs: Vec<PathBuf> = std::env::split_paths(path).collect();
        OBSCURA_NAMES.iter().find_map(|name| {
            let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
            dirs.iter()
                .map(|dir| dir.join(&file))
                .find(|candidate| candidate.is_file())
                .map(Self::new)
        })
    }

    /// [`Self::locate_in`] the process's `PATH`.
    pub fn locate() -> Option<Self> {
        std::env::var_os("PATH").and_then(|path| Self::locate_in(&path))
    }

    /// Another time limit (tests).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The executable.
    pub fn program(&self) -> &Path {
        &self.program
    }

    fn command(&self, query: &SearchQuery) -> Command {
        let mut command = Command::new(&self.program);
        command
            .arg("serp")
            .arg("--max-results")
            .arg(query.limit.max(1).to_string())
            .arg("--")
            .arg(&query.text)
            .env_clear()
            .envs(
                INHERITED.iter().filter_map(|name| {
                    std::env::var_os(name).map(|value| (name.to_owned(), value))
                }),
            )
            .env("OBSCURA_ALLOW_PRIVATE_NETWORK", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            command.process_group(0).kill_on_drop(false);
        }
        #[cfg(not(unix))]
        {
            command.kill_on_drop(true);
        }
        command
    }
}

/// Kills the browser's process group when dropped, unless disarmed: the future of a cancelled
/// call is dropped, and so is the browser it started.
struct Reaper {
    #[cfg(unix)]
    pgid: Option<u32>,
}

impl Reaper {
    fn new(_pid: Option<u32>) -> Self {
        Self {
            #[cfg(unix)]
            pgid: _pid,
        }
    }

    fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            crate::shell::kill_group_now(pgid);
        }
    }

    fn disarm(&mut self) {
        #[cfg(unix)]
        {
            self.pgid = None;
        }
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn read_capped(
    stream: Option<impl tokio::io::AsyncRead + Unpin>,
    cap: u64,
) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    if let Some(stream) = stream {
        stream.take(cap).read_to_end(&mut out).await?;
    }
    Ok(out)
}

/// The last non-empty line of its standard error, shortened: what the operator needs to see
/// (Obscura's own message), never a page of logs.
fn last_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no message");
    let mut short: String = line.chars().take(300).collect();
    if short.len() < line.len() {
        short.push('…');
    }
    short
}

/// Reads `serp`'s answer: a JSON array of `{url, title, snippet}`. A result without a URL is
/// dropped; a missing title falls back to the URL.
pub fn parse_serp(stdout: &[u8], limit: usize) -> Result<Vec<SearchHit>, SearchError> {
    let document: Value = serde_json::from_slice(stdout)
        .map_err(|_| SearchError::BadResponse("obscura did not print JSON".into()))?;
    let results = document.as_array().ok_or_else(|| {
        SearchError::BadResponse("obscura printed JSON that is not a list of results".into())
    })?;
    Ok(results
        .iter()
        .filter_map(|r| {
            let url = r.get("url")?.as_str()?.trim();
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return None;
            }
            let title = r
                .get("title")
                .and_then(Value::as_str)
                .map(plain_text)
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| url.to_owned());
            let snippet = r
                .get("snippet")
                .and_then(Value::as_str)
                .map(plain_text)
                .filter(|s| !s.is_empty());
            Some(SearchHit {
                title,
                url: url.to_owned(),
                snippet,
            })
        })
        .take(limit)
        .collect())
}

#[async_trait]
impl SearchBackend for ObscuraBackend {
    fn id(&self) -> &str {
        "obscura"
    }

    async fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>, SearchError> {
        let mut child = match self.command(query).spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(SearchError::NotConfigured(format!(
                    "the Obscura executable `{}` is not installed (install dsh-obscura-nav, or name it with --search-engine obscura:/path/to/binary)",
                    self.program.display()
                )));
            },
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(SearchError::NotConfigured(format!(
                    "the Obscura executable `{}` cannot be run (permission denied)",
                    self.program.display()
                )));
            },
            Err(error) => {
                return Err(SearchError::Unavailable(format!(
                    "obscura could not be started: {error}"
                )));
            },
        };
        let mut reaper = Reaper::new(child.id());
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let run = async {
            let (out, err, status) = tokio::join!(
                read_capped(stdout, MAX_STDOUT),
                read_capped(stderr, MAX_STDERR),
                child.wait()
            );
            (out, err, status)
        };
        let (out, err, status) = match tokio::time::timeout(self.timeout, run).await {
            Ok(done) => done,
            Err(_) => {
                reaper.kill();
                return Err(SearchError::Timeout);
            },
        };
        reaper.disarm();
        let status =
            status.map_err(|e| SearchError::Unavailable(format!("obscura did not finish: {e}")))?;
        let err = err.unwrap_or_default();
        if !status.success() {
            return Err(SearchError::Unavailable(format!(
                "obscura exited with {}: {}",
                status
                    .code()
                    .map_or_else(|| "a signal".to_owned(), |c| format!("code {c}")),
                last_line(&err)
            )));
        }
        let out = out.map_err(|e| SearchError::Unavailable(format!("obscura's output: {e}")))?;
        parse_serp(&out, query.limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_is_read_and_cleaned() {
        let hits = parse_serp(
            br#"[{"url":"https://a.test/x","title":"A &amp; B","snippet":"one"},
                 {"url":"javascript:alert(1)","title":"bad"},
                 {"title":"no url"},
                 {"url":"https://b.test/","title":"","snippet":""}]"#,
            10,
        )
        .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "A & B");
        assert_eq!(hits[0].snippet.as_deref(), Some("one"));
        assert_eq!(hits[1].title, "https://b.test/");
        assert_eq!(hits[1].snippet, None);
    }

    #[test]
    fn the_limit_holds_and_shapes_are_checked() {
        let many = serde_json::to_vec(
            &(0..9)
                .map(|i| serde_json::json!({"url": format!("https://x.test/{i}"), "title": "t"}))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(parse_serp(&many, 3).unwrap().len(), 3);
        assert_eq!(parse_serp(b"[]", 3).unwrap(), vec![]);
        assert_eq!(
            parse_serp(b"not json", 3).unwrap_err().kind(),
            "bad_response"
        );
        assert_eq!(parse_serp(b"{}", 3).unwrap_err().kind(), "bad_response");
    }

    #[test]
    fn stderr_is_shortened_to_its_last_line() {
        assert_eq!(last_line(b"log\nError: boom\n\n"), "Error: boom");
        assert_eq!(last_line(b""), "no message");
        assert!(last_line(&[b'x'; 1000]).ends_with('…'));
    }
}
