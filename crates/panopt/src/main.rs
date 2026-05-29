//! `panopt` - the PANopt cockpit launcher.
//!
//! Brings up the coordination daemon on demand and the Zellij cockpit, and
//! opens agent panes - each with a stable per-agent identity (DESIGN.md §9).

mod agent;
mod agent_config;
mod agent_tool;
mod clip;
mod close_gate;
mod daemon;
mod delete_gate;
mod edit;
mod id_kind;
mod mcp;
mod mcp_proxy;
mod mcpclient;
mod paths;
mod process;
mod scratchpad;
mod scratchpad_form;
mod search;
mod todo;
mod todo_form;
mod up;
mod viewer;
mod viewstate;
mod wrap;

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
        /// Bind address for the daemon when this call starts it. Defaults to
        /// `127.0.0.1`; pass `0.0.0.0` to accept connections from agents on
        /// other machines (the bearer-token gate still applies). Ignored if
        /// the daemon is already running.
        #[arg(long)]
        host: Option<String>,
    },
    /// Open a new agent pane in the running cockpit.
    Agent {
        /// Optional readable name for the agent.
        name: Option<String>,
    },
    /// Print a `--mcp-config` JSON for a hand-launched Claude Code session.
    ///
    /// The emitted config gives the session a stable agent id, a friendly
    /// display name, and the bearer token, so a manually started agent is
    /// indistinguishable from a cockpit-spawned one in the coordination plane.
    ///
    /// Example:
    ///   claude --mcp-config "$(panopt agent-config --name greg-main)"
    #[command(name = "agent-config")]
    AgentConfig {
        /// Friendly display name shown to other agents (default: the id).
        #[arg(long)]
        name: Option<String>,
        /// Stable agent id put on the MCP URL (default: $USER-$HOSTNAME).
        #[arg(long)]
        id: Option<String>,
        /// Daemon host (default: 127.0.0.1).
        #[arg(long)]
        host: Option<String>,
        /// Bearer token for the daemon's auth gate (default: read from
        /// `~/.local/share/panopt/token`). Pass inline when configuring an
        /// agent on another machine - get the value from the daemon host
        /// with `panopt token`.
        #[arg(long)]
        token: Option<String>,
        /// Project root (default: the current directory).
        #[arg(long)]
        ws: Option<PathBuf>,
    },
    /// Print the daemon's bearer token to stdout.
    ///
    /// Useful when configuring an agent on another machine: read the token
    /// from the daemon host (e.g. `ssh nixos panopt token`) and pass it to
    /// `panopt agent-config --token <value>` on the agent host, which then
    /// does not need a local token file.
    Token,
    /// Inspect and edit the project's shared todos.
    Todo {
        /// Project root the todos belong to (default: the current directory).
        #[arg(long, global = true)]
        ws: Option<PathBuf>,
        #[command(subcommand)]
        action: todo::TodoCmd,
    },
    /// Inspect and edit the project's agent tools (durable spawn configs).
    #[command(name = "agent-tool")]
    AgentTool {
        /// Project root the agent tools belong to (default: the current directory).
        #[arg(long, global = true)]
        ws: Option<PathBuf>,
        #[command(subcommand)]
        action: agent_tool::AgentToolCmd,
    },
    /// Inspect and edit the project's processes (agent/command/terminal instances).
    Process {
        /// Project root the processes belong to (default: the current directory).
        #[arg(long, global = true)]
        ws: Option<PathBuf>,
        #[command(subcommand)]
        action: process::ProcessCmd,
    },
    /// Operate on the project's scratchpads from the CLI. Currently exposes
    /// only `rm`; the rest of the surface lives in the cockpit's editor form.
    Scratchpad {
        /// Project root the scratchpads belong to (default: the current directory).
        #[arg(long, global = true)]
        ws: Option<PathBuf>,
        #[command(subcommand)]
        action: scratchpad::ScratchpadCmd,
    },
    /// Popup search across the project's todos and scratchpads. Intended for
    /// the cockpit (the sidebar plugin spawns this in a floating pane via the
    /// search keybind), but also runs standalone — Enter prints `kind:id` to
    /// stdout when not under Zellij.
    Search {
        /// Project root to search (default: the current directory).
        #[arg(long)]
        ws: Option<PathBuf>,
    },
    /// Resolve a numeric id to its resource kind (todo / scratchpad /
    /// agent-tool / process) via the daemon's `id_kind` MCP tool.
    #[command(name = "id-kind")]
    IdKind {
        /// Project root the id belongs to (default: the current directory).
        #[arg(long)]
        ws: Option<PathBuf>,
        /// Numeric id to resolve.
        id: u64,
    },
    /// Internal: the entrypoint that runs inside an agent pane.
    #[command(name = "_agent", hide = true)]
    AgentExec {
        #[arg(long)]
        ws: Option<PathBuf>,
        #[arg(long)]
        id: Option<String>,
    },
    /// Internal: call `agent_leave` on the daemon as `id`, so its registry
    /// entry and advisory locks clear immediately. Used by the plugin's
    /// pane-death hook to tear down a cockpit-spawned agent the moment its
    /// pane closes, without waiting for the idle sweep.
    #[command(name = "_agent-leave", hide = true)]
    AgentLeave {
        #[arg(long)]
        ws: Option<PathBuf>,
        /// Stable agent id (the `?agent=` value the dead pane connected with).
        #[arg(long)]
        id: String,
    },
    /// Internal: long-lived stdio MCP server that forwards to panoptd over
    /// HTTP. Claude Code spawns this via `--mcp-config` so its connection
    /// stays up across panoptd restarts.
    #[command(name = "_mcp-proxy", hide = true)]
    McpProxy {
        /// Daemon host (default: 127.0.0.1).
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Project root the agent is scoped to.
        #[arg(long)]
        ws: PathBuf,
        /// Stable agent id (becomes `?agent=` on the panoptd URL).
        #[arg(long)]
        id: String,
        /// Friendly display name (becomes `?name=` on the panoptd URL).
        #[arg(long)]
        name: String,
        /// Bearer token for the daemon's auth gate.
        #[arg(long)]
        token: String,
    },
    /// Internal: start a process in this pane.
    #[command(name = "_process-run", hide = true)]
    ProcessRun {
        #[arg(long)]
        ws: Option<PathBuf>,
        /// Numeric id of the process to start.
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
    /// Internal: the floating delete-confirmation dialog the sidebar plugin
    /// spawns when the user presses `x` on a row.
    #[command(name = "_delete-gate", hide = true)]
    DeleteGateExec {
        /// Item kind: todo, scratchpad, agent-tool, or process.
        #[arg(long)]
        kind: String,
        /// Numeric id of the item the user wants to delete.
        #[arg(long)]
        id: u64,
        /// Human label for the row (title / name); shown in the dialog.
        #[arg(long, default_value = "")]
        label: String,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Up { plugin, host } => up::run(plugin, host, cli.port),
        Cmd::Agent { name } => agent::spawn(name),
        Cmd::AgentConfig {
            name,
            id,
            host,
            token,
            ws,
        } => agent_config::run(host, cli.port, id, name, token, ws),
        Cmd::Token => {
            use anyhow::Context;
            let token = panopt_core::auth::read_token(&paths::token()?)
                .context("reading the panopt token (start the daemon with `panopt up`)")?;
            println!("{token}");
            Ok(())
        }
        Cmd::Todo { ws, action } => todo::run(ws, action, cli.port),
        Cmd::AgentTool { ws, action } => agent_tool::run(ws, action, cli.port),
        Cmd::Process { ws, action } => process::run(ws, action, cli.port),
        Cmd::Scratchpad { ws, action } => scratchpad::run(ws, action, cli.port),
        Cmd::Search { ws } => search::run(ws, cli.port),
        Cmd::IdKind { ws, id } => id_kind::run(ws, id, cli.port),
        Cmd::AgentExec { ws, id } => agent::exec_in_pane(ws, id, cli.port),
        Cmd::AgentLeave { ws, id } => agent::leave(ws, id, cli.port),
        Cmd::McpProxy {
            host,
            ws,
            id,
            name,
            token,
        } => mcp_proxy::run(host, cli.port, ws, id, name, token),
        Cmd::ProcessRun { ws, id } => process::exec_entry(ws, id, cli.port),
        Cmd::ViewerExec { ws, slot, kind, id } => viewer::run(ws, cli.port, slot, kind, id),
        Cmd::CloseGateExec {
            scope,
            items,
            target_pane,
        } => close_gate::run(&scope, &items, target_pane, cli.port),
        Cmd::DeleteGateExec { kind, id, label } => delete_gate::run(&kind, id, &label, cli.port),
    }
}
