use std::net::SocketAddr;

use anyhow::Context;
use clipboard_vault::{AppState, build_router, config::Config};
use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clipboard_vault=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer().json())
        .init();

    let config = Config::from_env()?;
    tokio::fs::create_dir_all(config.upload_root.join("tmp"))
        .await
        .context("create upload directory")?;

    let pool = PgPoolOptions::new()
        .max_connections(config.database_max_connections)
        .connect(&config.database_url)
        .await
        .context("connect to PostgreSQL")?;

    sqlx::migrate!()
        .run(&pool)
        .await
        .context("run vault migrations")?;

    let address = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = TcpListener::bind(address)
        .await
        .context("bind HTTP listener")?;
    info!(%address, "Clipboard Vault Rust service started");

    axum::serve(listener, build_router(AppState::new(&config, pool)))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve HTTP")?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler")
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}
