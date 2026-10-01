use anyhow::Result;
use std::net::SocketAddr;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use claude_code_api::{core::config::Settings, create_app};

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

    let addr = SocketAddr::from(([0, 0, 0, 0], settings.server.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!("Server running on http://{}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}
