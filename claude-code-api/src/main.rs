use anyhow::Result;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use claude_code_api::{
    core::config::{ServerConfig, Settings},
    create_app,
};

/// The address the gateway binds, from the `[server]` section.
///
/// `server.host` is parsed as an IP literal. Anything that is not one — a DNS
/// name, an empty string — falls back to `0.0.0.0` with a warning, because
/// resolving a name here would mean a DNS lookup during start-up and the
/// previous behaviour was to listen on every interface regardless.
///
/// That previous behaviour was the bug: the address was hardcoded to
/// `0.0.0.0` while the start-up log printed `settings.server.host`, so a
/// deployment that set `host = "127.0.0.1"` was told it was bound to loopback
/// and was in fact reachable from every interface. Every `config/*.toml` in the
/// repository already says `0.0.0.0`, so honouring the field changes nothing
/// for the shipped configurations and only starts obeying an operator who asked
/// for something narrower.
fn listen_addr(server: &ServerConfig) -> SocketAddr {
    match server.host.parse::<IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, server.port),
        Err(_) => {
            warn!(
                "server.host = {:?} is not an IP address; binding every interface (0.0.0.0)",
                server.host
            );
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), server.port)
        },
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let settings = Settings::new()?;

    info!(
        "Starting Claude Code API Gateway on {}:{}",
        settings.server.host, settings.server.port
    );

    let app = create_app(settings.clone()).await?;

    let addr = listen_addr(&settings.server);
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!("Server running on http://{}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(host: &str, port: u16) -> ServerConfig {
        ServerConfig {
            host: host.to_string(),
            port,
        }
    }

    /// The shipped default (`config.rs` and every `config/*.toml`): unchanged.
    #[test]
    fn wildcard_host_binds_every_interface() {
        assert_eq!(
            listen_addr(&server("0.0.0.0", 8080)),
            SocketAddr::from(([0, 0, 0, 0], 8080))
        );
    }

    /// The regression this function exists for: before `listen_addr`, `main`
    /// built `SocketAddr::from(([0, 0, 0, 0], port))` and a configured host was
    /// silently discarded, so this asserted `0.0.0.0` and the operator's
    /// loopback-only intent was ignored.
    #[test]
    fn a_configured_host_is_honoured_instead_of_being_discarded() {
        let addr = listen_addr(&server("127.0.0.1", 9000));
        assert_eq!(addr, SocketAddr::from(([127, 0, 0, 1], 9000)));
        assert!(
            addr.ip().is_loopback(),
            "host = 127.0.0.1 must not reach beyond loopback, got {addr}"
        );
    }

    #[test]
    fn an_ipv6_host_is_honoured() {
        let addr = listen_addr(&server("::1", 7000));
        assert!(addr.is_ipv6(), "::1 must bind an IPv6 socket, got {addr}");
        assert_eq!(addr.port(), 7000);
    }

    /// A DNS name is not resolved at start-up; the gateway keeps the old
    /// behaviour rather than failing to boot.
    #[test]
    fn a_dns_name_falls_back_to_the_wildcard_address() {
        assert_eq!(
            listen_addr(&server("localhost", 8080)),
            SocketAddr::from(([0, 0, 0, 0], 8080))
        );
    }

    #[test]
    fn an_empty_host_falls_back_to_the_wildcard_address() {
        assert_eq!(
            listen_addr(&server("", 1)),
            SocketAddr::from(([0, 0, 0, 0], 1))
        );
    }

    /// `port = 0` means "any free port" to the OS; `listen_addr` passes it
    /// through rather than substituting a default.
    #[test]
    fn port_zero_is_passed_through_unchanged() {
        assert_eq!(listen_addr(&server("127.0.0.1", 0)).port(), 0);
    }
}
