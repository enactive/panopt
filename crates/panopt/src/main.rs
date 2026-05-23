//! `panopt` - the PANopt cockpit launcher.
//!
//! Brings up the coordination daemon on demand and the Zellij cockpit, and
//! opens agent panes - each with a stable per-agent identity (DESIGN.md §9).

mod agent;
mod close_gate;
mod daemon;
mod edit;
mod mcp;
mod mcpclient;
mod paths;
mod roster;
mod scratchpad_form;
mod todo;
mod todo_form;
mod up;
mod viewer;
mod viewstate;

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
    /// Inspect and edit the project's roster of agents, commands, and terminals.
    Roster {
        /// Project root the roster belongs to (default: the current directory).
        #[arg(long, global = true)]
        ws: Option<PathBuf>,
        #[command(subcommand)]
        action: roster::RosterCmd,
    },
    /// Internal: the entrypoint that runs inside an agent pane.
    #[command(name = "_agent", hide = true)]
    AgentExec {
        #[arg(long)]
        ws: Option<PathBuf>,
        #[arg(long)]
        id: Option<String>,
    },
    /// Internal: start a roster entry in this pane.
    #[command(name = "_roster-run", hide = true)]
    RosterRun {
        #[arg(long)]
        ws: Option<PathBuf>,
        /// Numeric id of the roster entry to start.
        id: u64,
    },
    /// Internal: a long-lived cockpit viewer pane.
    #[command(name = "_viewer", hide = true)]
    ViewerExec {
        #[arg(long)]
        ws: Option<PathBuf>,
        /// Routing-file token the sidebar plugin assigned this pane.
        #[arg(long)]
        slot: String,
        /// Initial item kind: todo, scratchpad, todo-list, scratchpad-list.
        #[arg(long)]
        kind: Option<String>,
        /// Initial item id, for the todo and scratchpad kinds.
        #[arg(long)]
        id: Option<u64>,
    },
    /// Internal: the floating close-gate dialog the sidebar plugin spawns
    /// when a destructive action would lose active items.
    #[command(name = "_close-gate", hide = true)]
    CloseGateExec {
        /// What the user tried to do: focus, tab, or quit.
        #[arg(long)]
        scope: String,
        /// Active items the dialog shows: `kind:label;kind:label;...`.
        #[arg(long, default_value = "")]
        items: String,
        /// Terminal pane id to close when scope is `focus`.
        #[arg(long)]
        target_pane: Option<u32>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Up { plugin } => up::run(plugin, cli.port),
        Cmd::Agent { name } => agent::spawn(name),
        Cmd::Todo { ws, action } => todo::run(ws, action, cli.port),
        Cmd::Roster { ws, action } => roster::run(ws, action, cli.port),
        Cmd::AgentExec { ws, id } => agent::exec_in_pane(ws, id, cli.port),
        Cmd::RosterRun { ws, id } => roster::exec_entry(ws, id, cli.port),
        Cmd::ViewerExec { ws, slot, kind, id } => viewer::run(ws, cli.port, slot, kind, id),
        Cmd::CloseGateExec { scope, items, target_pane } => {
            close_gate::run(&scope, &items, target_pane, cli.port)
        }
    }
}
