//! What the REAL `claude` CLI does with `ANTHROPIC_*` when it is pointed at a gateway
//! (contract §13.2). It backs the rule `ProviderRegistry::open_session` applies to a
//! Claude Code session bound to a third-party model provider: the *other* authentication
//! variable is set empty, because with both present the CLI sends **both** headers.
//!
//! Ignored by default: it needs `claude` installed. Run it with
//! `cargo test -p nexus-claude --test real_claude_gateway -- --ignored --test-threads=1`.
//!
//! Nothing leaves the machine: the base URL is a local listener that answers `500` to
//! everything. The listener compares each credential header with the value the test
//! passed and keeps only **which one it matched** — a header value is never stored in a
//! string that is printed or asserted on, so this cannot leak the user's own login even
//! if the CLI chose to send it (in which case the credential reads as `Other`).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

const TEST_TOKEN: &str = "tok-test-A";
const TEST_KEY: &str = "key-test-B";
const HOST_KEY: &str = "host-key-should-not-leak";

/// Which of the known test values a header carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Carried {
    Token,
    Key,
    HostKey,
    /// Anything else, including a credential the test did not pass.
    Other,
}

/// What the listener saw on the first `/v1/messages` request.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Seen {
    /// The `Authorization` header: whether it was a Bearer, and what it carried.
    authorization: Option<(bool, Carried)>,
    /// The `x-api-key` header, and what it carried.
    x_api_key: Option<Carried>,
}

fn classify(value: &str) -> Carried {
    match value {
        v if v == TEST_TOKEN => Carried::Token,
        v if v == TEST_KEY => Carried::Key,
        v if v == HOST_KEY => Carried::HostKey,
        _ => Carried::Other,
    }
}

fn handle(mut stream: TcpStream, seen: &Mutex<Option<Seen>>) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
    }
    let text = String::from_utf8_lossy(&buffer).into_owned();
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_owned();
    if request_line.contains("/v1/messages") {
        let mut found = Seen::default();
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "authorization" => {
                    let bearer = value.to_ascii_lowercase().starts_with("bearer ");
                    let raw = if bearer { &value[7..] } else { value };
                    found.authorization = Some((bearer, classify(raw)));
                },
                "x-api-key" => found.x_api_key = Some(classify(value)),
                _ => {},
            }
        }
        seen.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(found);
    }
    let body = br#"{"type":"error","error":{"type":"api_error","message":"probe"}}"#;
    let _ = write!(
        stream,
        "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(body);
}

/// Runs the real CLI with `env` and returns what its first model request carried.
fn run_claude(env: &BTreeMap<&str, &str>) -> Option<Seen> {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a local port");
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Option<Seen>>> = Arc::new(Mutex::new(None));
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let accept = {
        let (seen, stop) = (Arc::clone(&seen), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let seen = Arc::clone(&seen);
                        std::thread::spawn(move || handle(stream, &seen));
                    },
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        })
    };

    let mut command = Command::new("claude");
    command
        .args(["-p", "say hi", "--output-format", "json"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (name, _) in std::env::vars() {
        if name.starts_with("ANTHROPIC_") || name.starts_with("CLAUDE_CODE_USE_") {
            command.env_remove(name);
        }
    }
    command.env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"));
    command.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
    for (name, value) in env {
        command.env(name, value);
    }
    let mut child = command.spawn().ok()?;
    // The CLI retries a failing endpoint for a while; the first request is all we need.
    let deadline = Instant::now() + Duration::from_secs(40);
    while Instant::now() < deadline {
        if seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
        {
            break;
        }
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = accept.join();
    seen.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

fn claude_is_installed() -> bool {
    Command::new("claude")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
#[ignore = "needs the real `claude` CLI; run with --ignored"]
fn the_gateway_variables_behave_as_the_contract_says() {
    assert!(claude_is_installed(), "`claude` is not installed");

    // A. A bearer token alone.
    let a = run_claude(&BTreeMap::from([("ANTHROPIC_AUTH_TOKEN", TEST_TOKEN)]))
        .expect("the CLI sent a model request to the base URL");
    assert_eq!(a.authorization, Some((true, Carried::Token)), "A: {a:?}");
    assert_eq!(a.x_api_key, None, "A: no x-api-key without a key");

    // B. An API key alone.
    let b = run_claude(&BTreeMap::from([("ANTHROPIC_API_KEY", TEST_KEY)]))
        .expect("the CLI sent a model request to the base URL");
    assert_eq!(b.x_api_key, Some(Carried::Key), "B: {b:?}");

    // C. THE HAZARD: a token for the gateway AND a key already in the host environment.
    // The CLI sends both, so the host's own key reaches the base URL. If this ever stops
    // being true the shadowing in `open_session` may become unnecessary: update §13.2.
    let c = run_claude(&BTreeMap::from([
        ("ANTHROPIC_AUTH_TOKEN", TEST_TOKEN),
        ("ANTHROPIC_API_KEY", HOST_KEY),
    ]))
    .expect("the CLI sent a model request to the base URL");
    assert_eq!(c.authorization, Some((true, Carried::Token)), "C: {c:?}");
    assert_eq!(
        c.x_api_key,
        Some(Carried::HostKey),
        "C: the host's key is sent to the gateway when nothing shadows it"
    );

    // D. THE REMEDY: the same host key, shadowed by an empty value.
    let d = run_claude(&BTreeMap::from([
        ("ANTHROPIC_AUTH_TOKEN", TEST_TOKEN),
        ("ANTHROPIC_API_KEY", ""),
    ]))
    .expect("the CLI sent a model request to the base URL");
    assert_eq!(d.authorization, Some((true, Carried::Token)), "D: {d:?}");
    assert_eq!(d.x_api_key, None, "D: an empty value removes the header");
}
