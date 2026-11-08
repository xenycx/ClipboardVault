use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::Duration,
};

use anyhow::{Context, bail};
use clipboard_vault::{AppState, build_router, config::Config, db::Db, identity::oauth, import};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

const USAGE: &str = "\
Clipboard Vault

Usage:
  clipboard-vault [serve]          Run the web server (default)
  clipboard-vault healthcheck      Exit 0 when the local server answers /health/live
  clipboard-vault backup <file>    Write a consistent copy of DATA_DIR/vault.db to <file>
  clipboard-vault import-postgres  Copy a 2.x PostgreSQL install into DATA_DIR
                                   (run with --help for its options)
  clipboard-vault --version
";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("serve") => runtime()?.block_on(serve()),
        Some("healthcheck") => healthcheck(),
        Some("backup") => {
            let destination = args
                .get(1)
                .context("usage: clipboard-vault backup <file>")?;
            let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into());
            let source = std::path::Path::new(&data_dir).join(clipboard_vault::db::DATABASE_FILE);
            runtime()?.block_on(clipboard_vault::db::snapshot_file(
                &source,
                std::path::Path::new(destination),
            ))?;
            println!("wrote {destination}");
            Ok(())
        }
        Some("import-postgres") => {
            init_logging(false);
            runtime()?.block_on(import::run(&args[1..]))
        }
        Some("--version" | "-V") => {
            println!("clipboard-vault {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--help" | "-h" | "help") => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown command {other:?}\n\n{USAGE}"),
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("start async runtime")
}

fn init_logging(json: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "clipboard_vault=info,tower_http=info".into());
    let registry = tracing_subscriber::registry().with(filter);
    if json {
        registry
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    } else {
        registry
            .with(tracing_subscriber::fmt::layer().with_target(false))
            .init();
    }
}

async fn serve() -> anyhow::Result<()> {
    init_logging(true);
    let config = Config::from_env()?;
    for name in Config::retired_variables_present() {
        tracing::warn!(
            variable = name,
            "ignoring a 2.x setting that 3.0 no longer uses"
        );
    }
    tokio::fs::create_dir_all(&config.data_dir)
        .await
        .with_context(|| format!("create {}", config.data_dir.display()))?;
    tokio::fs::create_dir_all(config.upload_root.join("tmp"))
        .await
        .context("create upload directory")?;
    let db = Db::open(&config.data_dir, config.database_read_connections).await?;
    let providers = oauth::Registry::from_env()?;

    let address = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = TcpListener::bind(address)
        .await
        .context("bind HTTP listener")?;
    info!(
        %address,
        database = %db.path().display(),
        version = env!("CARGO_PKG_VERSION"),
        "Clipboard Vault started"
    );

    let state = AppState::new(&config, db, providers);
    tokio::spawn(clipboard_vault::uploads::maintenance_worker(state.clone()));
    tokio::spawn(clipboard_vault::backup::worker(state.clone()));
    axum::serve(
        listener,
        build_router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("serve HTTP")?;
    Ok(())
}

/// Container health probe. Distroless images have no curl, so the binary checks
/// itself over plain HTTP on the loopback interface.
fn healthcheck() -> anyhow::Result<()> {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8080);
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(3))
        .with_context(|| format!("connect to {address}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    stream
        .write_all(b"GET /health/live HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut head = [0_u8; 32];
    let read = stream.read(&mut head)?;
    let status = String::from_utf8_lossy(&head[..read]);
    if status.starts_with("HTTP/1.1 200") || status.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        bail!(
            "unhealthy: {}",
            status.lines().next().unwrap_or("no response")
        )
    }
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
