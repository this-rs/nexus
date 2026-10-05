//! https for WebFetch (feature `tls`): a trusted certificate works, an untrusted one and a name
//! mismatch fail before any data is exchanged, and `http` is upgraded to it.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use nexus_tools::web::{
    Connect, FetchConfig, Fetcher, Io, Resolve, Target, TlsConnector, WebFetchTool,
};
use nexus_tools::{CallContext, SessionState, Tool};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

struct Names;

#[async_trait]
impl Resolve for Names {
    async fn resolve(&self, _host: &str, _port: u16) -> std::io::Result<Vec<IpAddr>> {
        Ok(vec!["93.184.216.34".parse().unwrap()])
    }
}

/// The real TLS connector, with the connection steered to the local server.
struct Steer(TlsConnector, SocketAddr);

#[async_trait]
impl Connect for Steer {
    async fn connect(&self, target: &Target) -> std::io::Result<Box<dyn Io>> {
        self.0
            .connect(&Target {
                addr: self.1,
                ..target.clone()
            })
            .await
    }
}

/// A TLS server whose certificate is for `name`; returns its address and the certificate.
async fn server(name: &str) -> (SocketAddr, CertificateDer<'static>) {
    let cert = rcgen::generate_simple_self_signed(vec![name.to_owned()]).unwrap();
    let der = cert.cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut buf = [0u8; 4096];
                let _ = tls.read(&mut buf).await;
                let body = "secure hello";
                let _ = tls
                    .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
                    .await;
                let _ = tls.shutdown().await;
            });
        }
    });
    (addr, der)
}

fn tool(addr: SocketAddr, trusted: &[CertificateDer<'static>]) -> WebFetchTool {
    let connector = Steer(TlsConnector::with_extra_roots(trusted), addr);
    WebFetchTool::new(
        Fetcher::new(FetchConfig::default())
            .with_resolver(Names)
            .with_connector(connector),
    )
}

async fn fetch(tool: &WebFetchTool, url: &str) -> nexus_tools::ToolResult {
    tool.call(
        &CallContext::new("s", Arc::new(SessionState::default())),
        json!({"url": url}),
    )
    .await
}

#[tokio::test]
async fn a_trusted_certificate_for_the_right_name_works_and_http_is_upgraded_to_it() {
    let (addr, cert) = server("secure.test").await;
    let tool = tool(addr, &[cert]);
    assert_eq!(
        fetch(&tool, "https://secure.test/").await.text,
        "secure hello"
    );
    // The default config upgrades http to https: the same page, over TLS.
    assert_eq!(
        fetch(&tool, "http://secure.test/page").await.text,
        "secure hello"
    );
}

#[tokio::test]
async fn an_untrusted_certificate_is_refused_and_no_page_comes_back() {
    let (addr, _cert) = server("secure.test").await;
    let tool = tool(addr, &[]); // the test certificate is not a root
    let r = fetch(&tool, "https://secure.test/").await;
    assert!(
        r.is_error && r.text.contains("connect_failed"),
        "{}",
        r.text
    );
    assert!(!r.text.contains("secure hello"));
}

#[tokio::test]
async fn a_certificate_for_another_name_is_refused() {
    let (addr, cert) = server("other.test").await;
    let tool = tool(addr, &[cert]); // trusted, but not for secure.test
    let r = fetch(&tool, "https://secure.test/").await;
    assert!(
        r.is_error && r.text.contains("connect_failed"),
        "{}",
        r.text
    );
}
