//! `WebFetch` (N21): the SSRF guard (one red test per vector), redirects, caps, content types,
//! the cache and the conversion, against a local HTTP server and fake resolvers.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use nexus_tools::web::{Clock, Connect, FetchConfig, Fetcher, Io, Resolve, Target, WebFetchTool};
use nexus_tools::{CallContext, SessionState, Tool, ToolResult};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---------------------------------------------------------------------------
// A local server that answers what it is told to
// ---------------------------------------------------------------------------

type Handler = Arc<dyn Fn(&str, &str) -> Vec<u8> + Send + Sync>;

struct Server {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
}

fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn ok(content_type: &str, body: &str) -> Vec<u8> {
    response("200 OK", &[("Content-Type", content_type)], body.as_bytes())
}

fn redirect(to: &str) -> Vec<u8> {
    response("302 Found", &[("Location", to)], b"")
}

/// `handler(request_target, host_header)` gives the raw response.
async fn serve(handler: impl Fn(&str, &str) -> Vec<u8> + Send + Sync + 'static) -> Server {
    let handler: Handler = Arc::new(handler);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (count, log) = (Arc::clone(&connections), Arc::clone(&requests));
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            count.fetch_add(1, Ordering::SeqCst);
            let (handler, log) = (Arc::clone(&handler), Arc::clone(&log));
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&buffer).into_owned();
                log.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(text.clone());
                let target = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let host = text
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("host: ")
                            .or_else(|| l.strip_prefix("Host: "))
                    })
                    .unwrap_or_default()
                    .to_owned();
                let answer = handler(&target, &host);
                let _ = stream.write_all(&answer).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    Server {
        addr,
        connections,
        requests,
    }
}

// ---------------------------------------------------------------------------
// Fakes: names answered by the test, connections steered to the local server
// ---------------------------------------------------------------------------

/// Answers names from a table; a name may give a different answer each time it is asked.
#[derive(Default, Clone)]
struct Names {
    answers: Arc<Mutex<HashMap<String, Vec<Vec<IpAddr>>>>>,
    asked: Arc<AtomicUsize>,
}

impl Names {
    fn with(self, name: &str, answers: &[&[&str]]) -> Self {
        self.answers.lock().unwrap().insert(
            name.to_owned(),
            answers
                .iter()
                .map(|a| a.iter().map(|ip| ip.parse().unwrap()).collect())
                .collect(),
        );
        self
    }
}

#[async_trait]
impl Resolve for Names {
    async fn resolve(&self, host: &str, _port: u16) -> std::io::Result<Vec<IpAddr>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let mut table = self.answers.lock().unwrap();
        let answers = table
            .get_mut(host)
            .ok_or_else(|| std::io::Error::other(format!("no such host: {host}")))?;
        // The first answer is used until the last one is reached: then it sticks.
        Ok(if answers.len() > 1 {
            answers.remove(0)
        } else {
            answers[0].clone()
        })
    }
}

/// Connects every target to the local server, and remembers what it was asked to reach.
#[derive(Clone)]
struct Steered {
    to: SocketAddr,
    targets: Arc<Mutex<Vec<Target>>>,
}

#[async_trait]
impl Connect for Steered {
    async fn connect(&self, target: &Target) -> std::io::Result<Box<dyn Io>> {
        self.targets.lock().unwrap().push(target.clone());
        Ok(Box::new(tokio::net::TcpStream::connect(self.to).await?))
    }
}

struct Counting(Arc<AtomicUsize>);

#[async_trait]
impl Connect for Counting {
    async fn connect(&self, _target: &Target) -> std::io::Result<Box<dyn Io>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::other(
            "the test connector refuses to connect",
        ))
    }
}

struct ManualClock(AtomicU64);

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn context() -> CallContext {
    CallContext::new("s", Arc::new(SessionState::default()))
}

async fn call(tool: &WebFetchTool, url: &str) -> ToolResult {
    tool.call(&context(), json!({"url": url, "prompt": "summarise"}))
        .await
}

/// A tool whose every connection goes to `server`, for public-looking names.
fn steered(server: &Server, names: Names, config: FetchConfig) -> (WebFetchTool, Steered) {
    let connector = Steered {
        to: server.addr,
        targets: Arc::default(),
    };
    let fetcher = Fetcher::new(config)
        .with_resolver(names)
        .with_connector(connector.clone());
    (WebFetchTool::new(fetcher), connector)
}

fn public() -> FetchConfig {
    FetchConfig {
        upgrade_http: false,
        ..FetchConfig::default()
    }
}

fn names() -> Names {
    Names::default()
        .with("site.test", &[&["93.184.216.34"]])
        .with("other.test", &[&["93.184.216.35"]])
        .with("www.site.test", &[&["93.184.216.34"]])
}

// ---------------------------------------------------------------------------
// SSRF: one red test per vector, each proving no connection was made
// ---------------------------------------------------------------------------

async fn refused_without_connecting(url: &str, names: Names, kind_text: &str) {
    let connects = Arc::new(AtomicUsize::new(0));
    let fetcher = Fetcher::new(public())
        .with_resolver(names)
        .with_connector(Counting(Arc::clone(&connects)));
    let tool = WebFetchTool::new(fetcher);
    let r = call(&tool, url).await;
    assert!(r.is_error, "{url} was not refused: {}", r.text);
    assert!(r.text.contains("blocked_address"), "{url}: {}", r.text);
    assert!(r.text.contains(kind_text), "{url}: {}", r.text);
    assert_eq!(
        connects.load(Ordering::SeqCst),
        0,
        "{url}: a connection was attempted"
    );
}

#[tokio::test]
async fn private_addresses_are_refused() {
    for url in [
        "http://10.1.2.3/",
        "http://172.16.0.9/x",
        "http://192.168.0.1:8080/admin",
    ] {
        refused_without_connecting(url, Names::default(), "private network").await;
    }
}

#[tokio::test]
async fn loopback_is_refused_in_every_spelling() {
    for url in [
        "http://127.0.0.1/",
        "http://127.1.2.3/",
        "http://localhost/",
        "http://[::1]/",
        // The URL parser reduces these to 127.0.0.1: the guard must see the real address.
        "http://2130706433/",
        "http://0x7f.0.0.1/",
        "http://017700000001/",
        "http://0177.0.0.1/",
        "http://127.1/",
    ] {
        let names = Names::default().with("localhost", &[&["127.0.0.1"]]);
        refused_without_connecting(url, names, "loopback").await;
    }
}

#[tokio::test]
async fn link_local_and_cloud_metadata_are_refused() {
    for url in [
        "http://169.254.169.254/latest/meta-data/",
        "http://[fe80::1]/",
        "http://169.254.0.1/",
    ] {
        refused_without_connecting(url, Names::default(), "link-local").await;
    }
}

#[tokio::test]
async fn a_name_that_resolves_to_a_private_address_is_refused() {
    let names = Names::default().with("internal.example.test", &[&["10.0.0.5"]]);
    refused_without_connecting("http://internal.example.test/", names, "10.0.0.5").await;
}

#[tokio::test]
async fn one_private_record_among_public_ones_refuses_the_whole_name() {
    let names = Names::default().with(
        "mixed.example.test",
        &[&["93.184.216.34", "10.0.0.5", "8.8.8.8"]],
    );
    refused_without_connecting("http://mixed.example.test/", names, "10.0.0.5").await;
}

#[tokio::test]
async fn ipv6_forms_of_private_addresses_are_refused() {
    for (url, text) in [
        ("http://[::ffff:127.0.0.1]/", "IPv4-mapped"),
        ("http://[::ffff:10.0.0.1]/", "IPv4-mapped"),
        ("http://[::ffff:a9fe:a9fe]/", "IPv4-mapped"),
        ("http://[fc00::1]/", "unique local"),
        ("http://[64:ff9b::7f00:1]/", "IPv6 form"),
        ("http://[2002:7f00:1::1]/", "6to4"),
    ] {
        refused_without_connecting(url, Names::default(), text).await;
    }
}

/// DNS rebinding: the name is public when first looked at and private the second time. Every
/// hop is checked again, and the connection goes to the address that was checked.
#[tokio::test]
async fn a_redirect_that_lands_on_a_private_address_is_refused() {
    let server = serve(|target, _| match target {
        "/start" => redirect("/next"),
        _ => ok("text/plain", "should never be served"),
    })
    .await;
    // Public for the first hop, private for the second.
    let names = Names::default().with("rebind.test", &[&["93.184.216.34"], &["10.0.0.7"]]);
    let (tool, connector) = steered(&server, names, public());
    let r = call(&tool, "http://rebind.test/start").await;
    assert!(
        r.is_error && r.text.contains("blocked_address") && r.text.contains("10.0.0.7"),
        "{}",
        r.text
    );
    assert_eq!(
        connector.targets.lock().unwrap().len(),
        1,
        "only the first hop connected"
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_connection_goes_to_the_address_that_was_checked() {
    let server = serve(|_, _| ok("text/plain", "hello")).await;
    let names = names();
    let (tool, connector) = steered(&server, names.clone(), public());
    let r = call(&tool, "http://site.test/").await;
    assert_eq!(r.text, "hello", "{}", r.text);
    let targets = connector.targets.lock().unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(
        targets[0].addr.ip(),
        "93.184.216.34".parse::<IpAddr>().unwrap()
    );
    assert_eq!(targets[0].host, "site.test");
    assert_eq!(
        names.asked.load(Ordering::SeqCst),
        1,
        "resolved once, not again at connection time"
    );
}

#[tokio::test]
async fn a_redirect_to_a_private_literal_on_another_host_is_reported_not_followed() {
    let server = serve(|_, _| redirect("http://169.254.169.254/latest/meta-data/")).await;
    let (tool, connector) = steered(&server, names(), public());
    let r = call(&tool, "http://site.test/").await;
    assert!(!r.is_error);
    assert!(r.text.starts_with("REDIRECT DETECTED"), "{}", r.text);
    assert!(r.text.contains("http://169.254.169.254/latest/meta-data/"));
    assert_eq!(
        connector.targets.lock().unwrap().len(),
        1,
        "the target was not contacted"
    );
}

#[tokio::test]
async fn urls_that_are_not_plain_http_or_that_carry_credentials_are_refused() {
    let connects = Arc::new(AtomicUsize::new(0));
    let tool =
        WebFetchTool::new(Fetcher::new(public()).with_connector(Counting(Arc::clone(&connects))));
    for (url, kind) in [
        ("file:///etc/passwd", "unsupported_scheme"),
        ("ftp://site.test/x", "unsupported_scheme"),
        ("gopher://site.test/", "unsupported_scheme"),
        ("http://user:secret@site.test/", "credentials_in_url"),
        ("http://:secret@site.test/", "credentials_in_url"),
        ("not a url", "invalid_url"),
        ("", "invalid_url"),
        ("http://", "invalid_url"),
    ] {
        let r = call(&tool, url).await;
        assert!(r.is_error && r.text.contains(kind), "{url}: {}", r.text);
        assert!(
            !r.text.contains("secret"),
            "the credential is echoed: {}",
            r.text
        );
    }
    assert_eq!(connects.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_page_comes_back_as_markdown_with_absolute_links() {
    let server = serve(|_, _| {
        ok("text/html; charset=utf-8", "<html><head><title>T</title></head><body><h1>Hello</h1><p><a href=\"/about\">About</a> <script>x()</script></p></body></html>")
    })
    .await;
    let (tool, _) = steered(&server, names(), public());
    let r = call(&tool, "http://site.test/docs/page").await;
    assert!(!r.is_error, "{}", r.text);
    assert_eq!(r.text, "# Hello\n\n[About](http://site.test/about)");
    // The instruction is for the session's model: no hidden model call, nothing added.
    assert!(!r.text.contains("summarise"));
}

#[tokio::test]
async fn the_request_says_what_it_is_and_what_it_accepts() {
    let server = serve(|_, _| ok("text/plain", "x")).await;
    let (tool, _) = steered(&server, names(), public());
    call(&tool, "http://site.test:8081/a/b?q=1").await;
    let request = server.requests.lock().unwrap()[0].to_ascii_lowercase();
    assert!(request.starts_with("get /a/b?q=1 http/1.1"), "{request}");
    assert!(request.contains("host: site.test:8081"), "{request}");
    assert!(request.contains("user-agent: nexus-tools/"), "{request}");
    assert!(request.contains("accept-encoding: identity"), "{request}");
    assert!(
        !request.contains("cookie") && !request.contains("authorization"),
        "{request}"
    );
}

#[tokio::test]
async fn text_json_and_markdown_pass_through_untouched() {
    for (content_type, body) in [
        ("text/plain; charset=utf-8", "plain *text*"),
        ("application/json", "{\"a\": [1, 2]}"),
        ("text/markdown", "# already markdown"),
        ("application/vnd.api+json", "{\"data\": null}"),
    ] {
        let server = serve(move |_, _| ok(content_type, body)).await;
        let (tool, _) = steered(&server, names(), public());
        let r = call(&tool, "http://site.test/").await;
        assert_eq!(r.text, body, "{content_type}");
    }
}

#[tokio::test]
async fn binary_content_is_refused_with_a_typed_error() {
    for content_type in [
        "image/png",
        "application/pdf",
        "application/octet-stream",
        "application/zip",
        "video/mp4",
    ] {
        let server = serve(move |_, _| {
            response("200 OK", &[("Content-Type", content_type)], b"\x89PNG\0\0")
        })
        .await;
        let (tool, _) = steered(&server, names(), public());
        let r = call(&tool, "http://site.test/f").await;
        assert!(r.is_error, "{content_type}");
        assert!(
            r.text.contains("unsupported_content_type") && r.text.contains(content_type),
            "{}",
            r.text
        );
    }
    // No type at all and a NUL byte: binary too.
    let server = serve(|_, _| response("200 OK", &[], b"abc\0def")).await;
    let (tool, _) = steered(&server, names(), public());
    let r = call(&tool, "http://site.test/f").await;
    assert!(
        r.is_error && r.text.contains("unsupported_content_type"),
        "{}",
        r.text
    );
}

#[tokio::test]
async fn http_errors_are_typed() {
    for status in [
        "404 Not Found",
        "500 Internal Server Error",
        "403 Forbidden",
    ] {
        let server =
            serve(move |_, _| response(status, &[("Content-Type", "text/plain")], b"nope")).await;
        let (tool, _) = steered(&server, names(), public());
        let r = call(&tool, "http://site.test/x").await;
        assert!(
            r.is_error && r.text.contains("http_status"),
            "{status}: {}",
            r.text
        );
        assert!(r.text.contains(&status[..3]), "{}", r.text);
    }
}

#[tokio::test]
async fn a_charset_other_than_utf8_is_decoded() {
    let server = serve(|_, _| {
        response(
            "200 OK",
            &[("Content-Type", "text/plain; charset=iso-8859-1")],
            b"caf\xe9",
        )
    })
    .await;
    let (tool, _) = steered(&server, names(), public());
    assert_eq!(call(&tool, "http://site.test/").await.text, "café");
}

// ---------------------------------------------------------------------------
// Redirects
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_redirect_within_the_host_is_followed_and_rechecked() {
    let server = serve(|target, _| match target {
        "/a" => redirect("/b"),
        "/b" => redirect("http://www.site.test/c"),
        "/c" => ok("text/plain", "arrived"),
        _ => response("404 Not Found", &[], b""),
    })
    .await;
    let names = names();
    let (tool, connector) = steered(&server, names.clone(), public());
    let r = call(&tool, "http://site.test/a").await;
    assert_eq!(r.text, "arrived", "{}", r.text);
    assert_eq!(connector.targets.lock().unwrap().len(), 3, "three hops");
    assert_eq!(
        names.asked.load(Ordering::SeqCst),
        3,
        "each hop was resolved and checked"
    );
}

#[tokio::test]
async fn a_redirect_to_another_host_is_reported_with_its_target() {
    let server = serve(|_, _| redirect("http://other.test/landing?x=1")).await;
    let (tool, connector) = steered(&server, names(), public());
    let r = tool
        .call(
            &context(),
            json!({"url": "http://site.test/old", "prompt": "find the price"}),
        )
        .await;
    assert!(!r.is_error, "{}", r.text);
    assert!(r.text.starts_with("REDIRECT DETECTED"), "{}", r.text);
    for part in [
        "http://site.test/old",
        "http://other.test/landing?x=1",
        "Status: 302",
        "find the price",
    ] {
        assert!(r.text.contains(part), "missing {part:?}: {}", r.text);
    }
    assert_eq!(
        connector.targets.lock().unwrap().len(),
        1,
        "the other host was not contacted"
    );
}

#[tokio::test]
async fn an_upgrade_to_https_on_the_same_host_is_followed_and_a_downgrade_is_reported() {
    // The steered connector accepts tls targets: it stands in for a TLS backend.
    let server = serve(|target, _| match target {
        "/up" => redirect("https://site.test/secure"),
        "/secure" => ok("text/plain", "over tls"),
        "/down" => redirect("http://site.test/plain"),
        _ => response("404 Not Found", &[], b""),
    })
    .await;
    let (tool, connector) = steered(&server, names(), public());
    let r = call(&tool, "http://site.test/up").await;
    assert_eq!(r.text, "over tls", "{}", r.text);
    let targets = connector.targets.lock().unwrap().clone();
    assert_eq!(
        (targets[0].tls, targets[1].tls),
        (false, true),
        "second hop is https"
    );

    let r = call(&tool, "https://site.test/down").await;
    assert!(
        !r.is_error && r.text.starts_with("REDIRECT DETECTED"),
        "{}",
        r.text
    );
    assert!(r.text.contains("http://site.test/plain"), "{}", r.text);
}

#[cfg(not(feature = "tls"))]
#[tokio::test]
async fn without_a_tls_backend_https_fails_with_a_typed_error_before_any_data_moves() {
    let tool = WebFetchTool::new(Fetcher::new(FetchConfig::default()).with_resolver(names()));
    let r = call(&tool, "https://site.test/").await;
    assert!(
        r.is_error && r.text.contains("connect_failed") && r.text.contains("no TLS support"),
        "{}",
        r.text
    );
    // http is upgraded to https by default, so it fails the same way, never in the clear.
    let r = call(&tool, "http://site.test/").await;
    assert!(
        r.is_error && r.text.contains("no TLS support"),
        "{}",
        r.text
    );
}

#[tokio::test]
async fn a_redirect_loop_stops() {
    let server = serve(|_, _| redirect("/again")).await;
    let (tool, connector) = steered(
        &server,
        names(),
        FetchConfig {
            max_redirects: 3,
            ..public()
        },
    );
    let r = call(&tool, "http://site.test/").await;
    assert!(
        r.is_error && r.text.contains("too_many_redirects"),
        "{}",
        r.text
    );
    assert_eq!(
        connector.targets.lock().unwrap().len(),
        4,
        "the first request and three redirects"
    );
}

#[tokio::test]
async fn an_unusable_redirect_is_an_error() {
    let server = serve(|_, _| response("302 Found", &[], b"")).await;
    let (tool, _) = steered(&server, names(), public());
    let r = call(&tool, "http://site.test/").await;
    assert!(r.is_error && r.text.contains("bad_redirect"), "{}", r.text);
    let server = serve(|_, _| redirect("ftp://site.test/x")).await;
    let (tool, _) = steered(&server, names(), public());
    let r = call(&tool, "http://site.test/").await;
    assert!(
        r.is_error && r.text.contains("unsupported_scheme"),
        "{}",
        r.text
    );
}

// ---------------------------------------------------------------------------
// Caps and time
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_body_over_the_download_cap_is_cut_and_the_cut_is_said() {
    let big = "x".repeat(2_000);
    let server = serve(move |_, _| ok("text/plain", &big)).await;
    let (tool, _) = steered(
        &server,
        names(),
        FetchConfig {
            max_body_bytes: 500,
            ..public()
        },
    );
    let r = call(&tool, "http://site.test/").await;
    assert!(!r.is_error);
    assert!(r.text.starts_with(&"x".repeat(500)));
    assert!(
        r.text.contains("larger than the download cap"),
        "{}",
        &r.text[500..]
    );
    assert_eq!(r.text.matches('x').count(), 500);
}

#[tokio::test]
async fn text_over_the_text_cap_is_cut_with_a_marker() {
    let long = "y".repeat(150_000);
    let server = serve(move |_, _| ok("text/plain", &long)).await;
    let (tool, _) = steered(&server, names(), public());
    let r = call(&tool, "http://site.test/").await;
    assert!(
        r.text
            .contains("[output truncated: 50000 characters omitted of 150000]"),
        "{}",
        &r.text[100_000..]
    );
}

#[tokio::test]
async fn a_server_that_stalls_times_out() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(stream);
            });
        }
    });
    let connector = Steered {
        to: addr,
        targets: Arc::default(),
    };
    let config = FetchConfig {
        request_timeout: Duration::from_millis(300),
        ..public()
    };
    let tool = WebFetchTool::new(
        Fetcher::new(config)
            .with_resolver(names())
            .with_connector(connector),
    );
    let started = std::time::Instant::now();
    let r = call(&tool, "http://site.test/").await;
    assert!(r.is_error && r.text.contains("timeout"), "{}", r.text);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_name_that_does_not_resolve_is_a_typed_error() {
    let (tool, _) = steered(
        &serve(|_, _| ok("text/plain", "x")).await,
        Names::default(),
        public(),
    );
    let r = call(&tool, "http://nowhere.test/").await;
    assert!(r.is_error && r.text.contains("dns_failure"), "{}", r.text);
}

// ---------------------------------------------------------------------------
// The cache
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_request_within_15_minutes_makes_no_network_request() {
    let server = serve(|_, _| ok("text/plain", "page")).await;
    let clock = Arc::new(ManualClock(AtomicU64::new(1_000)));
    let (tool, _) = steered(&server, names(), public());
    let tool = tool.with_clock(Arc::clone(&clock) as Arc<dyn Clock>);

    assert_eq!(call(&tool, "http://site.test/p").await.text, "page");
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);

    clock.0.store(1_000 + 14 * 60 * 1000, Ordering::SeqCst);
    assert_eq!(call(&tool, "http://site.test/p").await.text, "page");
    assert_eq!(
        server.connections.load(Ordering::SeqCst),
        1,
        "served from the cache"
    );

    // Another URL is another page.
    call(&tool, "http://site.test/other").await;
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);

    clock.0.store(1_000 + 15 * 60 * 1000, Ordering::SeqCst);
    call(&tool, "http://site.test/p").await;
    assert_eq!(
        server.connections.load(Ordering::SeqCst),
        3,
        "stale after 15 minutes"
    );
}

#[tokio::test]
async fn failures_are_not_cached() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&hits);
    let server = serve(move |_, _| {
        if counted.fetch_add(1, Ordering::SeqCst) == 0 {
            response("500 Internal Server Error", &[], b"")
        } else {
            ok("text/plain", "recovered")
        }
    })
    .await;
    let (tool, _) = steered(&server, names(), public());
    assert!(call(&tool, "http://site.test/").await.is_error);
    assert_eq!(call(&tool, "http://site.test/").await.text, "recovered");
}

#[tokio::test]
async fn a_tool_call_without_a_url_is_refused() {
    let tool = WebFetchTool::new(Fetcher::new(public()));
    let r = tool.call(&context(), json!({})).await;
    assert!(r.is_error);
    let a = tool.annotations();
    assert!(a.read_only && a.open_world);
}

// ---------------------------------------------------------------------------
// A real name on the real resolver: loopback is refused even by its everyday name
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_system_resolver_cannot_be_used_to_reach_localhost() {
    let connects = Arc::new(AtomicUsize::new(0));
    let fetcher = Fetcher::new(public()).with_connector(Counting(Arc::clone(&connects)));
    let tool = WebFetchTool::new(fetcher);
    let r = call(&tool, "http://localhost:9/").await;
    assert!(
        r.is_error && r.text.contains("blocked_address"),
        "{}",
        r.text
    );
    assert_eq!(connects.load(Ordering::SeqCst), 0);
}

/// The shipped binary is built with `tls`; `Fetcher::new` must then really speak https. It once
/// kept `PlainConnector` whatever the feature said, and every https fetch failed with
/// "this build has no TLS support".
#[cfg(feature = "tls")]
#[test]
fn with_the_tls_feature_the_default_fetcher_can_reach_https() {
    assert!(Fetcher::new(FetchConfig::default()).supports_tls());
}

#[cfg(not(feature = "tls"))]
#[test]
fn without_the_tls_feature_the_default_fetcher_says_it_cannot_reach_https() {
    assert!(!Fetcher::new(FetchConfig::default()).supports_tls());
}

/// Real network, real certificate chain: `cargo test -p nexus-tools --features tls --test web -- --ignored`.
#[cfg(feature = "tls")]
#[tokio::test]
#[ignore = "needs the internet"]
async fn a_real_https_page_is_fetched_with_the_default_fetcher() {
    let tool = WebFetchTool::new(Fetcher::new(FetchConfig::default()));
    let r = call(&tool, "https://example.com/").await;
    assert!(
        !r.is_error && r.text.contains("for use in documentation examples"),
        "{}",
        r.text
    );
}
