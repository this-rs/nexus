//! Fetching one URL: guard, pinned connection, redirects, caps (N21).

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::header::{
    ACCEPT, ACCEPT_ENCODING, CONNECTION, CONTENT_TYPE, HOST, LOCATION, USER_AGENT,
};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use url::{Host, Url};

use super::ssrf::blocked_reason;

/// A byte stream to a server.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// Where a connection goes: the address that passed the guard, and the name to present.
#[derive(Debug, Clone)]
pub struct Target {
    /// The host name (for TLS server name and the `Host` header).
    pub host: String,
    /// The address to connect to — pinned: never resolved a second time.
    pub addr: SocketAddr,
    /// Whether the scheme is `https`.
    pub tls: bool,
}

/// Opens connections.
#[async_trait]
pub trait Connect: Send + Sync {
    /// Connects to the target.
    async fn connect(&self, target: &Target) -> std::io::Result<Box<dyn Io>>;

    /// Whether `https` targets can be served. A connector that cannot says so here, so a build
    /// that was meant to speak TLS can be checked without a network.
    fn supports_tls(&self) -> bool {
        true
    }
}

/// Plain TCP. `https` is refused until a TLS backend is chosen and compiled in.
#[derive(Debug, Default)]
pub struct PlainConnector;

#[async_trait]
impl Connect for PlainConnector {
    fn supports_tls(&self) -> bool {
        false
    }

    async fn connect(&self, target: &Target) -> std::io::Result<Box<dyn Io>> {
        if target.tls {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "this build has no TLS support",
            ));
        }
        Ok(Box::new(tokio::net::TcpStream::connect(target.addr).await?))
    }
}

/// Turns a host name into addresses.
#[async_trait]
pub trait Resolve: Send + Sync {
    /// Every address `host` has.
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<IpAddr>>;
}

/// The system resolver.
#[derive(Debug, Default)]
pub struct SystemResolver;

#[async_trait]
impl Resolve for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<IpAddr>> {
        Ok(tokio::net::lookup_host((host, port))
            .await?
            .map(|a| a.ip())
            .collect())
    }
}

/// What can go wrong, by kind: the model is told which, and may act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebError {
    InvalidUrl(String),
    UnsupportedScheme(String),
    CredentialsInUrl,
    BlockedAddress {
        host: String,
        ip: IpAddr,
        reason: &'static str,
    },
    DnsFailure(String),
    ConnectFailed(String),
    Timeout,
    TooManyRedirects,
    BadRedirect(String),
    HttpStatus {
        status: u16,
        url: String,
    },
    UnsupportedContentType(String),
    Protocol(String),
}

impl WebError {
    /// A short stable name.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidUrl(_) => "invalid_url",
            Self::UnsupportedScheme(_) => "unsupported_scheme",
            Self::CredentialsInUrl => "credentials_in_url",
            Self::BlockedAddress { .. } => "blocked_address",
            Self::DnsFailure(_) => "dns_failure",
            Self::ConnectFailed(_) => "connect_failed",
            Self::Timeout => "timeout",
            Self::TooManyRedirects => "too_many_redirects",
            Self::BadRedirect(_) => "bad_redirect",
            Self::HttpStatus { .. } => "http_status",
            Self::UnsupportedContentType(_) => "unsupported_content_type",
            Self::Protocol(_) => "protocol",
        }
    }
}

impl std::fmt::Display for WebError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(why) => write!(f, "not a valid URL: {why}"),
            Self::UnsupportedScheme(s) => {
                write!(f, "only http and https URLs can be fetched, not `{s}`")
            },
            Self::CredentialsInUrl => f.write_str("URLs with a user name or password are refused"),
            Self::BlockedAddress { host, ip, reason } => write!(
                f,
                "{host} resolves to {ip}, which is not a public address: {reason}. Fetching it is refused."
            ),
            Self::DnsFailure(why) => write!(f, "could not resolve the host: {why}"),
            Self::ConnectFailed(why) => write!(f, "could not connect: {why}"),
            Self::Timeout => f.write_str("the request took too long"),
            Self::TooManyRedirects => f.write_str("too many redirects"),
            Self::BadRedirect(why) => write!(f, "the redirect is unusable: {why}"),
            Self::HttpStatus { status, url } => write!(f, "{url} answered with status {status}"),
            Self::UnsupportedContentType(t) => {
                write!(f, "content type `{t}` cannot be read as text")
            },
            Self::Protocol(why) => write!(f, "protocol error: {why}"),
        }
    }
}

/// Limits and switches of the fetcher.
#[derive(Debug, Clone)]
pub struct FetchConfig {
    /// Allow private, loopback and link-local destinations. Off, always, outside tests.
    pub allow_private_network: bool,
    /// Rewrite `http://` to `https://` (Claude Code does).
    pub upgrade_http: bool,
    /// Redirects followed within a host.
    pub max_redirects: usize,
    /// Time to open a connection.
    pub connect_timeout: Duration,
    /// Time for one request, headers and body.
    pub request_timeout: Duration,
    /// Body bytes kept; the rest is dropped and the page is marked truncated.
    pub max_body_bytes: usize,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            allow_private_network: false,
            upgrade_http: true,
            max_redirects: 5,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            max_body_bytes: 5 * 1024 * 1024,
        }
    }
}

/// A page that came back.
#[derive(Debug)]
pub struct Page {
    /// The URL the content came from, after same-host redirects.
    pub url: Url,
    /// The `Content-Type` header.
    pub content_type: Option<String>,
    /// The body, at most `max_body_bytes`.
    pub body: Vec<u8>,
    /// Whether the body was cut at the cap.
    pub truncated: bool,
}

/// What a fetch ended with.
#[derive(Debug)]
pub enum Outcome {
    /// The page.
    Page(Page),
    /// The server redirected to **another host**: not followed, reported.
    Redirect { from: Url, to: Url, status: u16 },
}

#[cfg(feature = "tls")]
fn default_connector() -> Box<dyn Connect> {
    Box::new(super::tls::TlsConnector::new())
}

#[cfg(not(feature = "tls"))]
fn default_connector() -> Box<dyn Connect> {
    Box::new(PlainConnector)
}

/// Fetches pages.
pub struct Fetcher {
    pub(crate) config: FetchConfig,
    resolver: Box<dyn Resolve>,
    connector: Box<dyn Connect>,
}

impl Fetcher {
    /// A fetcher with the system resolver. `https` works when the crate is built with the `tls`
    /// feature; without it only plain TCP is available and `https` fails with a typed error.
    pub fn new(config: FetchConfig) -> Self {
        Self {
            config,
            resolver: Box::new(SystemResolver),
            connector: default_connector(),
        }
    }

    /// Whether this fetcher can reach `https` URLs.
    pub fn supports_tls(&self) -> bool {
        self.connector.supports_tls()
    }

    /// Replaces the resolver (tests answer names themselves).
    #[must_use]
    pub fn with_resolver(mut self, resolver: impl Resolve + 'static) -> Self {
        self.resolver = Box::new(resolver);
        self
    }

    /// Replaces the connector.
    #[must_use]
    pub fn with_connector(mut self, connector: impl Connect + 'static) -> Self {
        self.connector = Box::new(connector);
        self
    }

    /// Fetches `url`, following redirects within the same host.
    pub async fn fetch(&self, url: &str) -> Result<Outcome, WebError> {
        self.get(url, &[]).await
    }

    /// Like [`fetch`](Self::fetch), with extra request headers. Values are marked sensitive
    /// (hyper never prints them) and go only to the host that was asked: a redirect to another
    /// host is not followed, so a key in a header cannot be carried away by one.
    pub async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Outcome, WebError> {
        let mut current = parse(url)?;
        if self.config.upgrade_http && current.scheme() == "http" {
            let _ = current.set_scheme("https");
        }
        for _ in 0..=self.config.max_redirects {
            let hop =
                tokio::time::timeout(self.config.request_timeout, self.hop(&current, headers))
                    .await
                    .map_err(|_| WebError::Timeout)??;
            match hop {
                Hop::Page(page) => return Ok(Outcome::Page(page)),
                Hop::Redirect { status, location } => {
                    let next = current
                        .join(&location)
                        .map_err(|e| WebError::BadRedirect(format!("`{location}`: {e}")))?;
                    validate(&next)?;
                    if !same_site(&current, &next) {
                        return Ok(Outcome::Redirect {
                            from: current,
                            to: next,
                            status,
                        });
                    }
                    current = next;
                },
            }
        }
        Err(WebError::TooManyRedirects)
    }

    /// Addresses of `url`'s host that passed the guard (all of them must).
    async fn guarded_addresses(&self, url: &Url) -> Result<Vec<SocketAddr>, WebError> {
        let port = url.port_or_known_default().unwrap_or(80);
        let host = url
            .host()
            .ok_or_else(|| WebError::InvalidUrl("no host".into()))?;
        let (name, ips) = match host {
            Host::Ipv4(ip) => (ip.to_string(), vec![IpAddr::V4(ip)]),
            Host::Ipv6(ip) => (ip.to_string(), vec![IpAddr::V6(ip)]),
            Host::Domain(domain) => {
                let ips = self
                    .resolver
                    .resolve(domain, port)
                    .await
                    .map_err(|e| WebError::DnsFailure(e.to_string()))?;
                (domain.to_owned(), ips)
            },
        };
        if ips.is_empty() {
            return Err(WebError::DnsFailure(format!("{name} has no address")));
        }
        if !self.config.allow_private_network {
            // Every address, not the first: a name may carry one public and one private record.
            for ip in &ips {
                if let Some(reason) = blocked_reason(*ip) {
                    return Err(WebError::BlockedAddress {
                        host: name,
                        ip: *ip,
                        reason,
                    });
                }
            }
        }
        Ok(ips
            .into_iter()
            .map(|ip| SocketAddr::new(ip, port))
            .collect())
    }

    async fn hop(&self, url: &Url, extra: &[(&str, &str)]) -> Result<Hop, WebError> {
        let addresses = self.guarded_addresses(url).await?;
        let host = url.host_str().unwrap_or_default().to_owned();
        let tls = url.scheme() == "https";
        let mut last = None;
        let mut stream = None;
        for addr in addresses {
            let target = Target {
                host: host.clone(),
                addr,
                tls,
            };
            match tokio::time::timeout(self.config.connect_timeout, self.connector.connect(&target))
                .await
            {
                Ok(Ok(s)) => {
                    stream = Some(s);
                    break;
                },
                Ok(Err(e)) => last = Some(e.to_string()),
                Err(_) => last = Some("connection timed out".into()),
            }
        }
        let stream = stream.ok_or_else(|| WebError::ConnectFailed(last.unwrap_or_default()))?;

        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| WebError::Protocol(e.to_string()))?;
        let driver = tokio::spawn(connection);
        let _abort = AbortOnDrop(driver);

        let target = match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_owned(),
        };
        let host_header = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.clone(),
        };
        let mut builder = Request::builder()
            .method(Method::GET)
            .uri(target)
            .header(HOST, host_header)
            .header(USER_AGENT, concat!("nexus-tools/", env!("CARGO_PKG_VERSION")))
            // No compression: a decompressor is attack surface, and pages are small.
            .header(ACCEPT_ENCODING, "identity")
            .header(CONNECTION, "close");
        if !extra
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("accept"))
        {
            builder = builder.header(
                ACCEPT,
                "text/markdown, text/html, text/plain, application/json, */*;q=0.1",
            );
        }
        for (name, value) in extra {
            let mut value = hyper::header::HeaderValue::from_str(value)
                .map_err(|_| WebError::Protocol(format!("header `{name}` has an invalid value")))?;
            value.set_sensitive(true);
            builder = builder.header(*name, value);
        }
        let request = builder
            .body(Empty::<Bytes>::new())
            .map_err(|e| WebError::Protocol(e.to_string()))?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|e| WebError::Protocol(e.to_string()))?;

        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| WebError::BadRedirect("no Location header".into()))?
                .to_owned();
            return Ok(Hop::Redirect {
                status: status.as_u16(),
                location,
            });
        }
        if !status.is_success() {
            return Err(WebError::HttpStatus {
                status: status.as_u16(),
                url: url.to_string(),
            });
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if let Some(kind) = content_type.as_deref()
            && !readable_as_text(kind)
        {
            return Err(WebError::UnsupportedContentType(kind.to_owned()));
        }
        let mut body = Vec::new();
        let mut truncated = false;
        let mut stream = response.into_body();
        while let Some(frame) = stream.frame().await {
            let frame = frame.map_err(|e| WebError::Protocol(e.to_string()))?;
            if let Some(data) = frame.data_ref() {
                let room = self.config.max_body_bytes.saturating_sub(body.len());
                if data.len() > room {
                    body.extend_from_slice(&data[..room]);
                    truncated = true;
                    break;
                }
                body.extend_from_slice(data);
            }
        }
        Ok(Hop::Page(Page {
            url: url.clone(),
            content_type,
            body,
            truncated,
        }))
    }
}

enum Hop {
    Page(Page),
    Redirect { status: u16, location: String },
}

struct AbortOnDrop(tokio::task::JoinHandle<Result<(), hyper::Error>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Parses and checks a URL for fetching.
pub fn parse(text: &str) -> Result<Url, WebError> {
    let url = Url::parse(text.trim()).map_err(|e| WebError::InvalidUrl(e.to_string()))?;
    validate(&url)?;
    Ok(url)
}

fn validate(url: &Url) -> Result<(), WebError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(WebError::UnsupportedScheme(url.scheme().to_owned()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(WebError::CredentialsInUrl);
    }
    if url.host().is_none() {
        return Err(WebError::InvalidUrl("no host".into()));
    }
    Ok(())
}

/// The same site for the purpose of following a redirect: the same host (a leading `www.`
/// does not count) on the same explicit port, and not a downgrade from https to http.
/// `http://h/` to `https://h/` is the same site: it is what an upgrade looks like, and the
/// default ports (80, 443) are not written in the URL.
fn same_site(a: &Url, b: &Url) -> bool {
    let bare = |u: &Url| {
        u.host_str()
            .map(|h| h.trim_start_matches("www.").to_ascii_lowercase())
    };
    bare(a) == bare(b) && a.port() == b.port() && !(a.scheme() == "https" && b.scheme() == "http")
}

/// Whether a `Content-Type` is something we turn into text.
pub fn readable_as_text(content_type: &str) -> bool {
    let kind = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    kind.starts_with("text/")
        || matches!(
            kind.as_str(),
            "application/json"
                | "application/xml"
                | "application/xhtml+xml"
                | "application/javascript"
                | "application/x-yaml"
                | "application/yaml"
                | "application/toml"
                | "application/rss+xml"
                | "application/atom+xml"
                | "application/ld+json"
        )
        || kind.ends_with("+json")
        || kind.ends_with("+xml")
}
