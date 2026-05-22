//! `panopt` - the PANopt cockpit launcher.
//!
//! Brings up the coordination daemon on demand and the Zellij cockpit, and
//! opens agent panes - each with a stable per-agent identity (DESIGN.md §9).

mod agent;
mod daemon;
mod edit;
mod mcp;
mod mcpclient;
mod paths;
mod todo;
mod up;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "panopt", about = "PANopt cockpit launcher", version)]
struct Cli {
    /// Port the panopt daemon listens on.
    #[arg(long, default_value_t = 7600, global = true)]
    port: u16,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Boot the cockpit: the daemon, a Zellij session, the sidebar, and an
    /// agent. Attaches if the cockpit is already running.
    Up {
        /// Path to the panopt-zellij plugin `.wasm` (auto-detected from a dev
        /// build when omitted).
        #[arg(long)]
        plugin: Option<PathBuf>,
    },
    /// Open a new agent pane in the running cockpit.
    Agent {
        /// Optional readable name for the agent.
        name: Option<String>,
    },
    /// Inspect and edit the project's shared todos.
    Todo {
        /// Project root the todos belong to (default: the current directory).
        #[arg(long, global = true)]
        ws: Option<PathBuf>,
        #[command(subcommand)]
        action: todo::TodoCmd,
    },
    /// Internal: the entrypoint that runs inside an agent pane.
    #[command(name = "_agent", hide = true)]
    AgentExec {
        #[arg(long)]
        ws: Option<PathBuf>,
        #[arg(long)]
        id: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Up { plugin } => up::run(plugin, cli.port),
        Cmd::Agent { name } => agent::spawn(name),
        Cmd::Todo { ws, action } => todo::run(ws, action, cli.port),
        Cmd::AgentExec { ws, id } => agent::exec_in_pane(ws, id, cli.port),
    }
}
