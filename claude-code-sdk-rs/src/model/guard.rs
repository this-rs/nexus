//! Endpoint guard (decision A36).
//!
//! Before any request leaves, the instance URL goes through
//! [`EndpointGuard::check`]:
//!
//! - scheme must be `http` or `https`; credentials in the URL (`user:pass@`) are refused;
//! - `https` is required, except for the loopback interface (`127.0.0.0/8`, `::1`,
//!   `localhost`, `*.localhost`), where plain `http` is accepted;
//! - the host is resolved **here**, and every resolved address is classified
//!   ([`classify_ip`]): private, link-local, CGNAT, unspecified, multicast and
//!   reserved ranges are refused (so a public name pointing inside the network does
//!   not become a way to reach internal services);
//! - the validated addresses are returned in [`CheckedEndpoint`], and the HTTP client
//!   is **pinned** on them (`ClientBuilder::resolve_to_addrs`): the connection cannot
//!   re-resolve the name to something else (DNS rebinding).
//!
//! Redirects are cut by the client itself (`redirect::Policy::none()`), see `openai.rs`.
//!
//! # Deviation from A36
//!
//! A36 has no switch to reach a model on a private network, but a vLLM on the LAN
//! needs one. [`EndpointGuard::allow_private_network`] (default `false`, set explicitly
//! by the instance) accepts RFC 1918, unique-local IPv6 and CGNAT ranges. Link-local
//! (cloud metadata, `169.254.0.0/16`), unspecified, multicast and reserved ranges stay
//! refused. The `http`-only-on-loopback rule is unchanged: a LAN endpoint needs https.

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Url;

use crate::agent::ProviderError;

/// Range an address belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IpClass {
    /// Routable on the public Internet.
    Public,
    /// `127.0.0.0/8`, `::1`.
    Loopback,
    /// RFC 1918, unique-local `fc00::/7`, site-local `fec0::/10`.
    Private,
    /// `169.254.0.0/16`, `fe80::/10` (includes cloud metadata services).
    LinkLocal,
    /// Carrier-grade NAT `100.64.0.0/10` (also used by overlay networks).
    Cgnat,
    /// `0.0.0.0/8`, `::`.
    Unspecified,
    /// `224.0.0.0/4`, `ff00::/8`.
    Multicast,
    /// Broadcast, `240.0.0.0/4`, benchmarking, documentation and other reserved blocks.
    Reserved,
}

impl fmt::Display for IpClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Public => "public",
            Self::Loopback => "loopback",
            Self::Private => "private",
            Self::LinkLocal => "link-local",
            Self::Cgnat => "carrier-grade NAT",
            Self::Unspecified => "unspecified",
            Self::Multicast => "multicast",
            Self::Reserved => "reserved",
        })
    }
}

impl IpClass {
    /// Whether a connection to this range is acceptable.
    pub fn is_allowed(self, allow_private_network: bool) -> bool {
        match self {
            Self::Public | Self::Loopback => true,
            Self::Private | Self::Cgnat => allow_private_network,
            Self::LinkLocal | Self::Unspecified | Self::Multicast | Self::Reserved => false,
        }
    }
}

/// Classifies an address. IPv4-mapped, NAT64 (`64:ff9b::/96`), 6to4 and
/// IPv4-compatible IPv6 forms are classified by the IPv4 address they embed.
pub fn classify_ip(ip: IpAddr) -> IpClass {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(v6),
    }
}

fn classify_v4(ip: Ipv4Addr) -> IpClass {
    let [a, b, c, _] = ip.octets();
    match (a, b, c) {
        (0, _, _) => IpClass::Unspecified,
        (127, _, _) => IpClass::Loopback,
        (10, _, _) => IpClass::Private,
        (172, 16..=31, _) => IpClass::Private,
        (192, 168, _) => IpClass::Private,
        (169, 254, _) => IpClass::LinkLocal,
        (100, 64..=127, _) => IpClass::Cgnat,
        (224..=239, _, _) => IpClass::Multicast,
        (192, 0, 0) | (192, 0, 2) | (198, 51, 100) | (203, 0, 113) => IpClass::Reserved,
        (198, 18..=19, _) => IpClass::Reserved,
        (240..=255, _, _) => IpClass::Reserved,
        _ => IpClass::Public,
    }
}

fn classify_v6(ip: Ipv6Addr) -> IpClass {
    if ip.is_unspecified() {
        return IpClass::Unspecified;
    }
    if ip.is_loopback() {
        return IpClass::Loopback;
    }
    let segments = ip.segments();
    let octets = ip.octets();
    let embedded = |from: usize| {
        classify_v4(Ipv4Addr::new(
            octets[from],
            octets[from + 1],
            octets[from + 2],
            octets[from + 3],
        ))
    };
    // ::ffff:a.b.c.d (mapped), ::a.b.c.d (deprecated compatible form)
    if segments[..5] == [0; 5] && (segments[5] == 0xffff || segments[5] == 0) {
        return embedded(12);
    }
    // 64:ff9b::/96 (NAT64)
    if segments[0] == 0x64 && segments[1] == 0xff9b && segments[2..6] == [0; 4] {
        return embedded(12);
    }
    // 2002::/16 (6to4): the IPv4 address sits in bits 16..48
    if segments[0] == 0x2002 {
        return embedded(2);
    }
    match segments[0] {
        0xff00..=0xffff if octets[0] == 0xff => IpClass::Multicast,
        s if s & 0xffc0 == 0xfe80 => IpClass::LinkLocal,
        s if s & 0xffc0 == 0xfec0 => IpClass::Private,
        s if s & 0xfe00 == 0xfc00 => IpClass::Private,
        0x2001 if segments[1] == 0x0db8 => IpClass::Reserved,
        _ => IpClass::Public,
    }
}

/// Name resolution used by the guard. Replaceable so tests (and hosts with their
/// own resolver) control what a name maps to.
#[async_trait]
pub trait DnsResolver: Send + Sync {
    /// Resolves `host` to socket addresses carrying `port`.
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
}

/// Resolver backed by the operating system (`tokio::net::lookup_host`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemResolver;

#[async_trait]
impl DnsResolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }
}

/// A URL that passed the guard, with the addresses it was validated against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedEndpoint {
    /// The validated URL.
    pub url: Url,
    /// Host part (domain name or IP literal), without brackets.
    pub host: String,
    /// Port (explicit or the scheme's default).
    pub port: u16,
    /// Resolved addresses, all acceptable. The connection must be pinned on them.
    pub addrs: Vec<SocketAddr>,
    /// The host is an IP literal: nothing to pin.
    pub ip_literal: bool,
}

/// The endpoint guard. Cheap to clone.
#[derive(Clone)]
pub struct EndpointGuard {
    allow_private_network: bool,
    resolver: Arc<dyn DnsResolver>,
}

impl fmt::Debug for EndpointGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EndpointGuard")
            .field("allow_private_network", &self.allow_private_network)
            .finish_non_exhaustive()
    }
}

impl Default for EndpointGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl EndpointGuard {
    /// The strict guard: private ranges refused, system DNS.
    pub fn new() -> Self {
        Self {
            allow_private_network: false,
            resolver: Arc::new(SystemResolver),
        }
    }

    /// Accepts private (RFC 1918, ULA) and CGNAT ranges. Off by default; see the
    /// module documentation for why this exists.
    pub fn allow_private_network(mut self, allow: bool) -> Self {
        self.allow_private_network = allow;
        self
    }

    /// Uses another resolver.
    pub fn with_resolver(mut self, resolver: Arc<dyn DnsResolver>) -> Self {
        self.resolver = resolver;
        self
    }

    /// Validates `raw` and resolves its host. Policy violations are
    /// [`ProviderError::InvalidRequest`]; a name that does not resolve is
    /// [`ProviderError::EndpointUnreachable`]. No message echoes the URL (it may
    /// hold a credential).
    pub async fn check(&self, raw: &str) -> Result<CheckedEndpoint, ProviderError> {
        let url = Url::parse(raw.trim())
            .map_err(|_| ProviderError::invalid("endpoint URL is not valid"))?;
        let https = match url.scheme() {
            "https" => true,
            "http" => false,
            _ => {
                return Err(ProviderError::invalid(
                    "endpoint URL must use http or https",
                ));
            },
        };
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ProviderError::invalid(
                "endpoint URL must not contain credentials",
            ));
        }
        let host_name = url
            .host_str()
            .ok_or_else(|| ProviderError::invalid("endpoint URL has no host"))?;
        let host = host_name
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let mut url = url;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| ProviderError::invalid("endpoint URL has no port"))?;

        let literal: Option<IpAddr> = host.parse().ok();
        let loopback_name = host == "localhost" || host.ends_with(".localhost");
        let loopback_literal = literal.is_some_and(|ip| classify_ip(ip) == IpClass::Loopback);
        if !https && !(loopback_literal || loopback_name) {
            return Err(ProviderError::invalid(
                "plain http is only allowed to the loopback interface; use https",
            ));
        }

        let addrs = match literal {
            Some(ip) => vec![SocketAddr::new(ip, port)],
            None => self
                .resolver
                .resolve(&host, port)
                .await
                .map_err(|_| ProviderError::unreachable("DNS resolution failed"))?,
        };
        if addrs.is_empty() {
            return Err(ProviderError::unreachable(
                "DNS resolution returned no address",
            ));
        }
        for addr in &addrs {
            let class = classify_ip(addr.ip());
            if !class.is_allowed(self.allow_private_network) {
                return Err(ProviderError::invalid(format!(
                    "endpoint resolves to a {class} address, which is not allowed"
                )));
            }
            if !https && class != IpClass::Loopback {
                return Err(ProviderError::invalid(
                    "plain http is only allowed to the loopback interface; use https",
                ));
            }
        }
        if literal.is_none() && url.host_str() != Some(host.as_str()) {
            // Pin and request must agree on the exact host spelling (trailing dot, case).
            url.set_host(Some(&host))
                .map_err(|_| ProviderError::invalid("endpoint URL has no valid host"))?;
        }
        Ok(CheckedEndpoint {
            url,
            host,
            port,
            addrs,
            ip_literal: literal.is_some(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn classify_ip_table() {
        use IpClass::*;
        let table = [
            ("8.8.8.8", Public),
            ("1.1.1.1", Public),
            ("93.184.216.34", Public),
            ("127.0.0.1", Loopback),
            ("127.255.255.254", Loopback),
            ("10.0.0.1", Private),
            ("172.16.0.1", Private),
            ("172.31.255.255", Private),
            ("172.32.0.1", Public),
            ("172.15.255.255", Public),
            ("192.168.1.10", Private),
            ("169.254.169.254", LinkLocal),
            ("100.64.0.1", Cgnat),
            ("100.127.255.255", Cgnat),
            ("100.128.0.1", Public),
            ("100.63.255.255", Public),
            ("0.0.0.0", Unspecified),
            ("0.1.2.3", Unspecified),
            ("224.0.0.1", Multicast),
            ("239.255.255.255", Multicast),
            ("255.255.255.255", Reserved),
            ("240.0.0.1", Reserved),
            ("198.18.0.1", Reserved),
            ("192.0.2.1", Reserved),
            ("::1", Loopback),
            ("::", Unspecified),
            ("fe80::1", LinkLocal),
            ("fc00::1", Private),
            ("fd12:3456::1", Private),
            ("fec0::1", Private),
            ("ff02::1", Multicast),
            ("2001:db8::1", Reserved),
            ("2606:4700:4700::1111", Public),
            ("::ffff:127.0.0.1", Loopback),
            ("::ffff:10.1.2.3", Private),
            ("::ffff:8.8.8.8", Public),
            ("::ffff:169.254.169.254", LinkLocal),
            ("64:ff9b::a00:1", Private),
            ("2002:a00:1::1", Private),
            ("::10.0.0.1", Private),
        ];
        for (text, expected) in table {
            assert_eq!(classify_ip(ip(text)), expected, "{text}");
        }
    }

    #[test]
    fn allowance_depends_on_the_private_switch() {
        assert!(IpClass::Public.is_allowed(false));
        assert!(IpClass::Loopback.is_allowed(false));
        assert!(!IpClass::Private.is_allowed(false));
        assert!(IpClass::Private.is_allowed(true));
        assert!(IpClass::Cgnat.is_allowed(true));
        for never in [
            IpClass::LinkLocal,
            IpClass::Unspecified,
            IpClass::Multicast,
            IpClass::Reserved,
        ] {
            assert!(!never.is_allowed(true), "{never}");
        }
    }

    struct Fixed(Vec<IpAddr>);

    #[async_trait]
    impl DnsResolver for Fixed {
        async fn resolve(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            Ok(self.0.iter().map(|ip| SocketAddr::new(*ip, port)).collect())
        }
    }

    fn guard_to(addresses: &[&str]) -> EndpointGuard {
        EndpointGuard::new()
            .with_resolver(Arc::new(Fixed(addresses.iter().map(|a| ip(a)).collect())))
    }

    fn detail(error: ProviderError) -> String {
        match error {
            ProviderError::InvalidRequest { detail }
            | ProviderError::EndpointUnreachable { detail } => detail,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn https_to_a_public_address_passes_and_returns_the_pin() {
        let checked = guard_to(&["93.184.216.34"])
            .check("https://api.example.com/v1")
            .await
            .unwrap();
        assert_eq!(checked.port, 443);
        assert_eq!(checked.addrs, vec!["93.184.216.34:443".parse().unwrap()]);
        assert!(!checked.ip_literal);
    }

    #[tokio::test]
    async fn plain_http_is_refused_outside_loopback() {
        let error = guard_to(&["93.184.216.34"])
            .check("http://api.example.com/v1")
            .await
            .unwrap_err();
        assert!(detail(error).contains("use https"));
        // even when the name happens to resolve to loopback
        assert!(
            guard_to(&["127.0.0.1"])
                .check("http://api.example.com")
                .await
                .is_err()
        );
        assert!(guard_to(&[]).check("http://10.0.0.5:8000").await.is_err());
    }

    #[tokio::test]
    async fn plain_http_is_accepted_on_loopback() {
        for url in [
            "http://127.0.0.1:8080/v1",
            "http://localhost:11434/v1",
            "http://[::1]:9/v1",
            "http://x.localhost/v1",
        ] {
            guard_to(&["127.0.0.1"])
                .check(url)
                .await
                .unwrap_or_else(|e| panic!("{url}: {e}"));
        }
    }

    #[tokio::test]
    async fn localhost_name_resolving_elsewhere_is_refused() {
        assert!(
            guard_to(&["93.184.216.34"])
                .check("http://localhost:1")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn internal_ranges_are_refused_after_resolution() {
        for target in [
            "10.0.0.7",
            "192.168.0.2",
            "169.254.169.254",
            "100.64.1.1",
            "0.0.0.0",
            "fd00::1",
        ] {
            let error = guard_to(&[target])
                .check("https://models.example.com")
                .await
                .unwrap_err();
            assert!(
                matches!(error, ProviderError::InvalidRequest { .. }),
                "{target}"
            );
        }
        // one bad address among good ones is enough to refuse
        assert!(
            guard_to(&["93.184.216.34", "10.0.0.7"])
                .check("https://m.example.com")
                .await
                .is_err()
        );
        // IP literal
        assert!(guard_to(&[]).check("https://192.168.1.5/v1").await.is_err());
    }

    #[tokio::test]
    async fn private_switch_accepts_lan_over_https_but_not_link_local() {
        let lan = guard_to(&["192.168.1.20"]).allow_private_network(true);
        lan.check("https://vllm.lan:8000/v1").await.unwrap();
        assert!(lan.check("http://vllm.lan:8000/v1").await.is_err());
        let metadata = guard_to(&["169.254.169.254"]).allow_private_network(true);
        assert!(metadata.check("https://m.example.com").await.is_err());
    }

    #[tokio::test]
    async fn other_schemes_and_embedded_credentials_are_refused() {
        let guard = guard_to(&["93.184.216.34"]);
        for url in [
            "ftp://x.example.com",
            "file:///etc/passwd",
            "ws://127.0.0.1",
            "not a url",
            "https://",
        ] {
            assert!(guard.check(url).await.is_err(), "{url}");
        }
        let error = guard
            .check("https://alice:hunter2@x.example.com")
            .await
            .unwrap_err();
        let text = format!("{error:?} {error}");
        assert!(!text.contains("hunter2") && !text.contains("alice"));
    }

    #[tokio::test]
    async fn resolution_failure_is_unreachable() {
        struct Failing;
        #[async_trait]
        impl DnsResolver for Failing {
            async fn resolve(&self, _: &str, _: u16) -> io::Result<Vec<SocketAddr>> {
                Err(io::Error::other("nxdomain"))
            }
        }
        let guard = EndpointGuard::new().with_resolver(Arc::new(Failing));
        let error = guard.check("https://nope.example.com").await.unwrap_err();
        assert!(matches!(error, ProviderError::EndpointUnreachable { .. }));
        assert!(error.retryable());
    }
}
