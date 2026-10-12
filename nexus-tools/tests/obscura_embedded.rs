//! Obscura embedded in nexus-tools (`obscura` feature, decision 1e9d7501). Offline: every page
//! is a `data:` URL, so no test touches the network.
//!
//! - the keyless engine reads a FIXED results page (DuckDuckGo's HTML markup) through the real
//!   browser and V8;
//! - a page stuck in a synchronous loop is stopped at the time limit, and when the call is
//!   cancelled: the browser thread is gone well before Obscura's own 5 s script watchdog;
//! - `nexus-tools obscura mcp` answers, stops a cancelled call without answering it, and answers
//!   a call past its limit with an error;
//! - the binary says what it embeds (`--capabilities`) and refuses stealth / private networks.
//!
//! The real engine (network) is checked by hand: `nexus-tools obscura serp "some query"`, and the
//! `#[ignore]`d test at the end.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nexus_tools::obscura::{live_browser_threads, private_network_refusal_for};
use nexus_tools::search::{EmbeddedObscuraBackend, SearchBackend, SearchError, SearchQuery};
use serde_json::{Value, json};

/// The browser-thread count is process-wide: the tests that read it run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A DuckDuckGo HTML results page, reduced to the markup Obscura reads.
const SERP_PAGE: &str = r#"<!doctype html><html><body><div id="links">
<div class="result results_links web-result"><h2><a class="result__a" href="https://docs.rs/obscura">Obscura &amp; friends</a></h2>
<a class="result__snippet" href="https://docs.rs/obscura">A headless browser in Rust.</a></div>
<div class="result"><h2><a class="result__a" href="https://duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Fpage&amp;rut=x">Redirected result</a></h2>
<div class="result__snippet">Through the redirect.</div></div>
<div class="result"><h2><a class="result__a" href="https://third.example/">Third</a></h2></div>
</div></body></html>"#;

/// A page whose script never yields: only terminating V8 ends it.
const STUCK_PAGE: &str =
    "<!doctype html><html><body><script>while (true) {}</script></body></html>";

/// `data:text/html,` + `html`, every byte that is not alphanumeric percent-encoded, then an open
/// HTML comment: the engine appends the query to the endpoint, and it lands inside the comment.
fn data_endpoint(html: &str) -> String {
    let mut url = String::from("data:text/html,");
    for byte in html.bytes() {
        if byte.is_ascii_alphanumeric() {
            url.push(byte as char);
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url.push_str("%3C!--");
    url
}

fn query(text: &str, limit: usize) -> SearchQuery {
    SearchQuery {
        text: text.into(),
        limit,
    }
}

/// Waits up to `within` for no browser thread to be left.
fn threads_gone(within: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < within {
        if live_browser_threads() == 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    live_browser_threads() == 0
}

#[tokio::test(flavor = "multi_thread")]
async fn the_engine_reads_a_fixed_results_page_through_the_real_browser() {
    let _serial = SERIAL.lock().await;
    let engine = EmbeddedObscuraBackend::new().with_endpoint(data_endpoint(SERP_PAGE));
    assert_eq!(engine.id(), "obscura");
    let hits = engine.search(&query("rust & v8", 10)).await.unwrap();
    assert_eq!(hits.len(), 3, "{hits:?}");
    assert_eq!(hits[0].url, "https://docs.rs/obscura");
    assert_eq!(hits[0].title, "Obscura & friends");
    assert_eq!(
        hits[0].snippet.as_deref(),
        Some("A headless browser in Rust.")
    );
    // DuckDuckGo's redirect is unwrapped to the page it points to.
    assert_eq!(hits[1].url, "https://example.org/page");
    assert_eq!(hits[2].snippet, None);
    // The limit holds.
    let two = engine.search(&query("rust", 2)).await.unwrap();
    assert_eq!(two.len(), 2);
    assert!(threads_gone(Duration::from_secs(2)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stuck_page_is_stopped_at_the_time_limit_not_by_obscuras_own_watchdog() {
    let _serial = SERIAL.lock().await;
    let engine = EmbeddedObscuraBackend::new()
        .with_endpoint(data_endpoint(STUCK_PAGE))
        .with_timeout(Duration::from_millis(500));
    let start = Instant::now();
    let error = engine.search(&query("q", 5)).await.unwrap_err();
    assert_eq!(error, SearchError::Timeout);
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
    // Obscura's watchdog would end the script after 5 s; ours ends it at once.
    assert!(
        threads_gone(Duration::from_secs(2)),
        "the browser thread outlived the call by 2 s"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_search_stops_the_browser() {
    let _serial = SERIAL.lock().await;
    let engine = EmbeddedObscuraBackend::new().with_endpoint(data_endpoint(STUCK_PAGE));
    let search = tokio::spawn(async move { engine.search(&query("q", 5)).await });
    let start = Instant::now();
    while live_browser_threads() == 0 && start.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        live_browser_threads(),
        1,
        "the browser thread never started"
    );
    // Let the page reach its loop, then cancel the call the way the server does (N18).
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!search.is_finished(), "the stuck page answered");
    search.abort();
    let _ = search.await;
    assert!(
        threads_gone(Duration::from_secs(2)),
        "the browser kept running after the call was cancelled"
    );
}

#[test]
fn private_networks_asked_for_in_the_environment_refuse_the_browser() {
    for on in ["1", "true", " YES ", "on"] {
        assert!(private_network_refusal_for(Some(on)).is_some(), "{on}");
    }
    for off in ["0", "false", "", "no"] {
        assert!(private_network_refusal_for(Some(off)).is_none(), "{off}");
    }
    assert!(private_network_refusal_for(None).is_none());
}

// ---------------------------------------------------------------------------
// The binary
// ---------------------------------------------------------------------------

fn binary() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nexus-tools"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        // Under `cargo llvm-cov`, where the binary writes its coverage profile: without this, the
        // cleared environment sends it to the working directory and the coverage job never sees
        // what the binary ran. Absent in an ordinary run.
        .envs(std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("LLVM_PROFILE")))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[test]
fn the_binary_says_it_embeds_obscura_and_refuses_stealth_and_private_networks() {
    let out = binary().arg("--capabilities").output().unwrap();
    let caps: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(caps["obscura"], true, "{caps}");
    assert_eq!(caps["version"], env!("CARGO_PKG_VERSION"));

    for flag in ["--stealth", "--allow-private-network"] {
        let out = binary().args(["obscura", "mcp", flag]).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{flag}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("never started with"));
    }
    let out = binary()
        .env("OBSCURA_ALLOW_PRIVATE_NETWORK", "1")
        .args(["obscura", "mcp"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("OBSCURA_ALLOW_PRIVATE_NETWORK"));
}

#[test]
fn obscura_alone_is_the_embedded_engine_with_no_warning_about_the_path() {
    let empty = tempfile::tempdir().unwrap();
    let mut child = binary()
        .env("PATH", empty.path())
        .args(["--unrestricted", "--search-engine", "obscura"])
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    drop(stdin);
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    child.wait().unwrap();
    let answer: Value = serde_json::from_str(&line).unwrap();
    let tools: Vec<&str> = answer["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(tools.contains(&"WebSearch"), "{tools:?}");
    assert!(!stderr.contains("none of"), "{stderr}");
}

/// A running `nexus-tools obscura mcp`, read line by line on a thread (with a deadline).
struct Browser {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    lines: std::sync::mpsc::Receiver<Value>,
}

impl Browser {
    fn start(extra: &[&str]) -> Self {
        let mut child = binary()
            .args(["obscura", "mcp"])
            .args(extra)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let value: Value = serde_json::from_str(&line).expect("stdout is JSON lines only");
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn next(&self, within: Duration) -> Option<Value> {
        self.lines.recv_timeout(within).ok()
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn navigate(id: u64, html: &str) -> Value {
    // The stuck page itself, without the trailing comment the search endpoint needs.
    let url = data_endpoint(html);
    let url = url.trim_end_matches("%3C!--");
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
           "params":{"name":"browser_navigate","arguments":{"url":url}}})
}

#[test]
fn the_browser_server_lists_obscuras_tools_and_stops_a_cancelled_call_without_answering_it() {
    let mut browser = Browser::start(&[]);
    browser.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
    let init = browser.next(Duration::from_secs(10)).expect("initialize");
    assert_eq!(init["result"]["serverInfo"]["name"], "obscura");
    browser.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    let list = browser.next(Duration::from_secs(10)).expect("tools/list");
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for tool in ["browser_navigate", "browser_snapshot", "browser_click"] {
        assert!(names.contains(&tool), "{tool} missing: {names:?}");
    }

    browser.send(navigate(3, STUCK_PAGE));
    std::thread::sleep(Duration::from_millis(500));
    browser.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled",
                        "params":{"requestId":3,"reason":"test"}}));
    let start = Instant::now();
    browser.send(json!({"jsonrpc":"2.0","id":4,"method":"ping"}));
    let next = browser
        .next(Duration::from_secs(4))
        .expect("an answer after the cancel");
    assert_eq!(next["id"], 4, "the cancelled call was answered: {next}");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );

    // The browser still works after the stopped call.
    browser.send(navigate(
        5,
        "<html><head><title>after</title></head><body>ok</body></html>",
    ));
    let after = browser
        .next(Duration::from_secs(10))
        .expect("a navigation after the cancel");
    assert_eq!(after["id"], 5);
    assert_ne!(after["result"]["isError"], true, "{after}");
}

#[test]
fn a_browser_call_past_its_limit_is_stopped_and_answered_with_an_error() {
    let mut browser = Browser::start(&["--call-timeout-secs", "1"]);
    browser.send(navigate(1, STUCK_PAGE));
    let start = Instant::now();
    let answer = browser
        .next(Duration::from_secs(4))
        .expect("an answer at the limit");
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(answer["id"], 1);
    assert_eq!(answer["result"]["isError"], true, "{answer}");
    let text = answer["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("did not finish in 1 s"), "{text}");
    browser.send(json!({"jsonrpc":"2.0","id":2,"method":"ping"}));
    assert_eq!(browser.next(Duration::from_secs(4)).expect("ping")["id"], 2);
}

/// The real engine, on the network: run by hand (`cargo test … -- --ignored`).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "network: queries html.duckduckgo.com"]
async fn the_real_engine_answers_a_live_query() {
    let _serial = SERIAL.lock().await;
    let hits = EmbeddedObscuraBackend::new()
        .search(&query("rust headless browser", 3))
        .await
        .unwrap();
    assert!(!hits.is_empty());
    assert!(hits.iter().all(|h| h.url.starts_with("http")));
}
