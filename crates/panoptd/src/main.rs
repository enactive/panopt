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

    let shutdown_state = shared.clone();
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_when_safe(shutdown_state))
        .await
        .context("server error")?;

    Ok(())
}

/// Two-strike shutdown: the first SIGTERM (or Ctrl-C) refuses to exit if
/// MCP clients are still connected and logs a per-project count so the
/// operator can see what they would drop. A second SIGTERM within the
/// confirmation window exits regardless. SIGKILL bypasses this by design.
///
/// When no clients are connected the very first signal exits cleanly,
/// matching the prior behaviour - the guard only engages when there is
/// something to protect. Keeping the second-signal window short means a
/// determined operator can still exit in roughly the time it takes to
/// hit Ctrl-C twice.
async fn shutdown_when_safe(state: Arc<Mutex<Store>>) {
    use tokio::time::{timeout, Duration};

    /// How long the daemon waits for a second SIGTERM after refusing the
    /// first. Long enough for a deliberate "ok, I really mean it" repeat,
    /// short enough that a forgotten daemon does not stay refusing forever.
    const CONFIRM_WINDOW: Duration = Duration::from_secs(10);

    loop {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        let counts = {
            let st = state.lock().expect("state mutex poisoned");
            (st.connected_agent_count(), st.connected_agents_by_project())
        };
        if counts.0 == 0 {
            tracing::info!("shutdown signal received");
            return;
        }
        for (project, n) in &counts.1 {
            tracing::warn!(
                project = ?project,
                agents = *n,
                "SIGTERM refused: MCP clients still connected"
            );
        }
        tracing::warn!(
            total = counts.0,
            window_secs = CONFIRM_WINDOW.as_secs(),
            "send another SIGTERM within {}s to force shutdown",
            CONFIRM_WINDOW.as_secs()
        );
        match timeout(CONFIRM_WINDOW, tokio::signal::ctrl_c()).await {
            Ok(_) => {
                tracing::warn!("second shutdown signal received - exiting");
                return;
            }
            Err(_) => {
                tracing::info!("shutdown window elapsed; continuing to serve");
                // Loop and wait for the next signal afresh.
            }
        }
    }
}
