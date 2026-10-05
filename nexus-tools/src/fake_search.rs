//! A local stand-in for search engines, to test engines and the tool without the internet
//! (N22). Behind the `test-tools` feature.
//!
//! One server speaks three dialects: `/brave` (a keyed JSON API), `/searxng/search` (SearXNG's
//! JSON) and `/html/` (a DuckDuckGo-like HTML page). Every answer is **leaky on purpose**: the
//! first result is on `blocked.test` and two results are the same page under different
//! addresses, so a test sees whether the tool filters and merges. A query that starts with a
//! directive misbehaves:
//!
//! | query starts with | the server answers |
//! |---|---|
//! | `!500` | HTTP 500 |
//! | `!429` | HTTP 429 |
//! | `!402` | HTTP 402 |
//! | `!403` | HTTP 403 |
//! | `!slow` | normally, after 3 s |
//! | `!garbage` | 200 with a body that is not the dialect |
//! | `!empty` | 200 with no results |

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

/// What the server was asked.
#[derive(Debug, Clone)]
pub struct Recorded {
    /// The request line, `GET /path?query HTTP/1.1`.
    pub request_line: String,
    /// Header names (lowercased) and values.
    pub headers: Vec<(String, String)>,
}

/// A running fake.
pub struct FakeSearch {
    /// Where it listens.
    pub addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl FakeSearch {
    /// Everything it was asked, in order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// `http://127.0.0.1:port`.
    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// Starts a fake on a free local port. `key` is what `/brave` accepts in `X-Subscription-Token`.
pub async fn start(key: &str) -> FakeSearch {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&requests);
    let key = key.to_owned();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (log, key) = (Arc::clone(&log), key.clone());
            tokio::spawn(async move { handle(stream, &log, &key).await });
        }
    });
    FakeSearch { addr, requests }
}

/// Serves forever on `addr` (for the `fake_search` binary).
pub async fn serve_forever(addr: &str, key: &str) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("listening on {}", listener.local_addr()?);
    let log = Arc::new(Mutex::new(Vec::new()));
    loop {
        let (stream, _) = listener.accept().await?;
        let (log, key) = (Arc::clone(&log), key.to_owned());
        tokio::spawn(async move { handle(stream, &log, &key).await });
    }
}

async fn handle(mut stream: tokio::net::TcpStream, log: &Mutex<Vec<Recorded>>, key: &str) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buffer.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
    }
    let text = String::from_utf8_lossy(&buffer).into_owned();
    let mut lines = text.lines();
    let request_line = lines.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = lines
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    log.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Recorded {
            request_line: request_line.clone(),
            headers: headers.clone(),
        });

    let target = request_line.split_whitespace().nth(1).unwrap_or("/");
    let parsed = Url::parse(&format!("http://fake{target}")).expect("target");
    let param = |name: &str| {
        parsed
            .query_pairs()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.into_owned())
    };
    let query = param("q").unwrap_or_default();
    let answer = respond(parsed.path(), &query, param("count"), &headers, key).await;
    let _ = stream.write_all(&answer).await;
    let _ = stream.shutdown().await;
}

fn raw(status: &str, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

struct Item {
    title: String,
    url: String,
    snippet: String,
}

fn items(query: &str) -> Vec<Item> {
    let slug: String = query
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let mut out = vec![
        Item {
            title: "Leaky result".into(),
            url: "https://blocked.test/leak".into(),
            snippet: "should be filtered".into(),
        },
        Item {
            title: format!("Result 1 for {query}"),
            url: format!("https://www.site1.test/{slug}?utm_source=fake#top"),
            snippet: format!("<strong>{query}</strong> matched &amp; more"),
        },
        Item {
            title: format!("Result 1 again for {query}"),
            url: format!("http://site1.test/{slug}"),
            snippet: "the same page, another address".into(),
        },
    ];
    for i in 2..=12 {
        out.push(Item {
            title: format!("Result {i} for {query}"),
            url: format!("https://site{i}.test/{slug}"),
            snippet: format!("<b>{query}</b> extract {i}"),
        });
    }
    out
}

async fn respond(
    path: &str,
    query: &str,
    count: Option<String>,
    headers: &[(String, String)],
    key: &str,
) -> Vec<u8> {
    let dialect = match path {
        "/brave" => "brave",
        "/searxng/search" => "searxng",
        "/html/" => "html",
        _ => return raw("404 Not Found", "text/plain", "no such route"),
    };
    if dialect == "brave" {
        let given = headers
            .iter()
            .find(|(n, _)| n == "x-subscription-token")
            .map(|(_, v)| v.as_str());
        if given != Some(key) {
            return raw(
                "401 Unauthorized",
                "application/json",
                "{\"error\":\"bad key\"}",
            );
        }
    }
    for (directive, answer) in [
        ("!500", "500 Internal Server Error"),
        ("!429", "429 Too Many Requests"),
        ("!402", "402 Payment Required"),
        ("!403", "403 Forbidden"),
    ] {
        if query.starts_with(directive) {
            return raw(answer, "text/plain", "refused");
        }
    }
    if query.starts_with("!slow") {
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    if query.starts_with("!garbage") {
        return raw(
            "200 OK",
            if dialect == "html" {
                "text/html"
            } else {
                "application/json"
            },
            "this is not what you asked for",
        );
    }
    let mut all = if query.starts_with("!empty") {
        Vec::new()
    } else {
        items(query)
    };
    if let Some(n) = count.and_then(|c| c.parse::<usize>().ok()) {
        all.truncate(n);
    }
    match dialect {
        "brave" => {
            let results: Vec<serde_json::Value> = all
                .iter()
                .map(|i| serde_json::json!({"title": i.title, "url": i.url, "description": i.snippet}))
                .collect();
            raw(
                "200 OK",
                "application/json",
                &serde_json::json!({"web": {"results": results}}).to_string(),
            )
        },
        "searxng" => {
            let results: Vec<serde_json::Value> = all
                .iter()
                .map(|i| serde_json::json!({"title": i.title, "url": i.url, "content": i.snippet}))
                .collect();
            raw(
                "200 OK",
                "application/json",
                &serde_json::json!({"results": results}).to_string(),
            )
        },
        _ => {
            let mut body = String::from("<html><body><div id=\"links\" class=\"results\">");
            for i in &all {
                let encoded: String =
                    url::form_urlencoded::byte_serialize(i.url.as_bytes()).collect();
                let title = i.title.replace('&', "&amp;").replace('<', "&lt;");
                body.push_str(&format!(
                    "<div class=\"result results_links\"><h2 class=\"result__title\"><a rel=\"nofollow\" class=\"result__a\" href=\"//duckduckgo.com/l/?uddg={encoded}&amp;rut=abc\">{title}</a></h2><a class=\"result__snippet\" href=\"//duckduckgo.com/l/?uddg={encoded}\">{}</a></div>",
                    i.snippet
                ));
            }
            body.push_str("</div></body></html>");
            raw("200 OK", "text/html; charset=utf-8", &body)
        },
    }
}
