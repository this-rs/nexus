//! https for `WebFetch` and the search engines (feature `tls`) (N21).
//!
//! rustls with the `ring` provider and the Mozilla root set (`webpki-roots`): no system trust
//! store is read, so what is trusted does not depend on the machine. Certificates are checked
//! against the host name of the URL, never against the pinned address.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_rustls::TlsConnector as Rustls;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

use super::fetch::{Connect, Io, Target};

/// Plain TCP for `http`, TLS for `https`.
pub struct TlsConnector {
    config: Arc<ClientConfig>,
}

impl std::fmt::Debug for TlsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TlsConnector")
    }
}

fn config(extra_roots: &[CertificateDer<'static>]) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for root in extra_roots {
        // A root that cannot be parsed is not added: the connection then fails verification.
        let _ = roots.add(root.clone());
    }
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

impl TlsConnector {
    /// The Mozilla roots.
    pub fn new() -> Self {
        Self {
            config: config(&[]),
        }
    }

    /// The Mozilla roots plus `roots` (a private CA, or a test certificate).
    pub fn with_extra_roots(roots: &[CertificateDer<'static>]) -> Self {
        Self {
            config: config(roots),
        }
    }
}

impl Default for TlsConnector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Connect for TlsConnector {
    async fn connect(&self, target: &Target) -> std::io::Result<Box<dyn Io>> {
        let tcp = tokio::net::TcpStream::connect(target.addr).await?;
        if !target.tls {
            return Ok(Box::new(tcp));
        }
        let name = ServerName::try_from(target.host.clone()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a valid server name")
        })?;
        let stream = Rustls::from(Arc::clone(&self.config))
            .connect(name, tcp)
            .await?;
        Ok(Box::new(stream))
    }
}
