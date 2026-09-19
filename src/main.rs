//! haven_server — authoritative RTS Chess game server.
//!
//! Axum + Tokio + WebSocket. All game logic runs here; clients are dumb
//! renderers that send inputs and apply snapshots. See AGENTS.md for the
//! environment guide and README.md for the API.

use haven_server::api;
use haven_server::config::Config;
use haven_server::match_manager::MatchManager;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,tower=warn")),
        )
        .init();

    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            eprintln!("hint: run from the repository root (config.toml lives there) or set HAVEN_CONFIG=/path/to/config.toml");
            std::process::exit(1);
        }
    };

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);
    let manager = MatchManager::new(cfg);
    haven_server::match_manager::spawn_gc_task(Arc::clone(&manager));

    let app = api::build_router(manager);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    tracing::info!("haven_server listening on http://{addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutting down");
}
