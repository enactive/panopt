//! `panoptd` - the PANopt coordination daemon.
//!
//! Runs one MCP server over Streamable HTTP on localhost. Every connected agent
//! shares one SQLite-backed store; each connection is scoped to a project by
//! the `ws` query parameter on its MCP URL, and that project's state is
//! mirrored to `.panopt/*.md` under the project root.

mod handler;
mod params;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use clap::Parser;
use panopt_core::Store;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};

use crate::handler::Handler;

#[derive(Parser)]
#[command(name = "panoptd", about = "PANopt coordination daemon")]
struct Cli {
    /// SQLite database file holding every project's state. Defaults to
    /// `panopt/panopt.db` under the per-user data directory.
    #[arg(long)]
    db: Option<PathBuf>,

    /// Localhost TCP port for the MCP server.
    #[arg(long, default_value_t = 7600)]
    port: u16,
}

/// The default database location: `<data-dir>/panopt/panopt.db`, where
/// `<data-dir>` is the platform's per-user data directory.
fn default_db_path() -> anyhow::Result<PathBuf> {
    let dir = dirs::data_dir()
        .context("could not locate a per-user data directory; pass --db explicitly")?;
    Ok(dir.join("panopt").join("panopt.db"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let db_path = match cli.db {
        Some(p) => p,
        None => default_db_path()?,
    };
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create data directory {}", parent.display()))?;
    }

    // One Store, wrapped once. The factory closure below clones this Arc per
    // MCP session; all clones point at this single Mutex<Store>.
    let shared = Arc::new(Mutex::new(
        Store::open(&db_path).context("failed to open the panopt database")?,
    ));

    tracing::info!(db = %db_path.display(), "panoptd starting");
    tracing::info!(
        "MCP endpoint: http://127.0.0.1:{}/mcp?ws=<project path>",
        cli.port
    );

    // Drop agents that have gone silent, every 30s, so a closed agent leaves
    // the registry even when no other agent is active to trigger a prune.
    let sweep_state = shared.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            ticker.tick().await;
            let swept = {
                let mut st = sweep_state.lock().expect("state mutex poisoned");
                st.sweep_idle_agents()
            };
            match swept {
                Ok(keys) => {
                    for key in keys {
                        tracing::info!(agent = %key, "agent left (idle)");
                    }
                }
                Err(e) => tracing::warn!("agent sweep failed: {e}"),
            }
        }
    });

    let factory_state = shared.clone();
    let service = StreamableHttpService::new(
        move || Ok(Handler::new(factory_state.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", cli.port))
        .await
        .with_context(|| format!("failed to bind 127.0.0.1:{}", cli.port))?;

    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutdown signal received");
        })
        .await
        .context("server error")?;

    Ok(())
}
