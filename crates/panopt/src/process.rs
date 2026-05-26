//! The `panopt process` subcommand: a thin MCP client of the daemon's
//! `process_*` tools.
//!
//! Processes are the per-project instance layer of the two-layer process
//! model (todo #27): each row is one running (or about to run) agent,
//! command, or terminal. Like the rest of the CLI surface, every invocation
//! ensures the daemon is up, opens a one-shot `observer` MCP session, calls
//! one tool, prints the result, and closes.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::{json, Map, Value};

use crate::daemon;
use crate::mcpclient::Client;
use crate::todo::{insert_opt, observer_url, render_scalar};

/// What to do to the project's processes.
#[derive(Subcommand)]
pub enum ProcessCmd {
    /// List every process.
    List,
    /// Show one process in full.
    Get {
        /// Numeric id of the process.
        id: u64,
    },
    /// Add a process.
    Add {
        /// Kind of process: agent, command, or terminal.
        kind: String,
        /// Identifier-style name for the process.
        name: String,
        /// Human label shown in the cockpit (default: the name).
        #[arg(long = "display-name")]
        display_name: Option<String>,
        /// Shell command the process runs.
        #[arg(long)]
        command: Option<String>,
        /// Working directory for the launched command.
        #[arg(long)]
        cwd: Option<String>,
        /// Numeric id of the agent tool this process was spawned from.
        #[arg(long = "agent-tool-id")]
        agent_tool_id: Option<u64>,
    },
    /// Edit a process. Omitted options are left unchanged.
    Set {
        /// Numeric id of the process to edit.
        id: u64,
        #[arg(long)]
        name: Option<String>,
        #[arg(long = "display-name")]
        display_name: Option<String>,
        #[arg(long)]
        command: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Delete a process.
    Rm {
        /// Numeric id of the process to delete.
        id: u64,
    },
}

/// Run a `panopt process` subcommand against the daemon for project `ws`.
pub fn run(ws: Option<PathBuf>, cmd: ProcessCmd, port: u16) -> Result<()> {
    daemon::ensure(port)?;
    let client = Client::connect(&observer_url(ws, port)?)?;
    let outcome = dispatch(&client, cmd);
    client.close();
    outcome
}

fn dispatch(client: &Client, cmd: ProcessCmd) -> Result<()> {
    match cmd {
        ProcessCmd::List => {
            print_list(&client.call("process_list", json!({}))?);
        }
        ProcessCmd::Get { id } => {
            print_entry(&client.call("process_get", json!({ "process_id": id }))?);
        }
        ProcessCmd::Add {
            kind,
            name,
            display_name,
            command,
            cwd,
            agent_tool_id,
        } => {
            let mut args = Map::new();
            args.insert("kind".into(), json!(kind));
            args.insert("name".into(), json!(name));
            insert_opt(&mut args, "display_name", display_name);
            insert_opt(&mut args, "command", command);
            insert_opt(&mut args, "cwd", cwd);
            if let Some(tid) = agent_tool_id {
                args.insert("agent_tool_id".into(), json!(tid));
            }
            let id = client.call("process_create", Value::Object(args))?;
            println!("created process #{}", render_scalar(&id));
        }
        ProcessCmd::Set {
            id,
            name,
            display_name,
            command,
            cwd,
        } => {
            let mut args = Map::new();
            args.insert("process_id".into(), json!(id));
            insert_opt(&mut args, "name", name);
            insert_opt(&mut args, "display_name", display_name);
            insert_opt(&mut args, "command", command);
            insert_opt(&mut args, "cwd", cwd);
            client.call("process_update", Value::Object(args))?;
            println!("updated process #{id}");
        }
        ProcessCmd::Rm { id } => {
            client.call("process_delete", json!({ "process_id": id }))?;
            println!("deleted process #{id}");
        }
    }
    Ok(())
}

/// `panopt _process-run` - start a process in the current pane.
///
/// Looks the process up in the daemon and `exec`s its command, so the Zellij
/// pane the sidebar plugin opened becomes the agent, command, or shell itself,
/// with no PANopt wrapper left around it. Rerunning the exited pane through
/// Zellij re-runs this shim, which re-fetches and re-execs.
pub fn exec_entry(ws: Option<PathBuf>, id: u64, port: u16) -> Result<()> {
    daemon::ensure(port)?;
    let client = Client::connect(&observer_url(ws.clone(), port)?)?;
    let entry = client.call("process_get", json!({ "process_id": id }));
    client.close();
    let entry = entry.with_context(|| format!("looking up process #{id}"))?;

    let command = entry["command"].as_str().unwrap_or("").trim().to_string();
    let cwd = entry["cwd"].as_str().unwrap_or("").trim().to_string();

    // A process with no command is a bare terminal: run an interactive shell.
    let mut cmd = if command.is_empty() {
        Command::new(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()))
    } else {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(&command);
        c
    };
    if !cwd.is_empty() {
        cmd.current_dir(&cwd);
    } else if let Some(ws) = ws {
        cmd.current_dir(ws);
    }
    let err = cmd.exec();
    Err(err).context("could not start the process's command")
}

fn print_list(v: &Value) {
    let entries = v.as_array().cloned().unwrap_or_default();
    if entries.is_empty() {
        println!("(no processes)");
        return;
    }
    for e in &entries {
        let id = e["id"].as_u64().unwrap_or(0);
        let kind = e["kind"].as_str().unwrap_or("?");
        let label = e["display_name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| e["name"].as_str())
            .unwrap_or("");
        let from = e["agent_tool_id"]
            .as_u64()
            .map(|tid| format!(" (from #{tid})"))
            .unwrap_or_default();
        println!("#{id}  [{kind}] {label}{from}");
        if let Some(c) = e["command"].as_str().filter(|c| !c.is_empty()) {
            println!("     {c}");
        }
    }
}

fn print_entry(e: &Value) {
    let id = e["id"].as_u64().unwrap_or(0);
    println!("#{id}  {}", e["name"].as_str().unwrap_or(""));
    println!("  kind:           {}", e["kind"].as_str().unwrap_or("?"));
    if let Some(d) = e["display_name"].as_str().filter(|s| !s.is_empty()) {
        println!("  display:        {d}");
    }
    if let Some(c) = e["command"].as_str().filter(|s| !s.is_empty()) {
        println!("  command:        {c}");
    }
    if let Some(c) = e["cwd"].as_str().filter(|s| !s.is_empty()) {
        println!("  cwd:            {c}");
    }
    if let Some(tid) = e["agent_tool_id"].as_u64() {
        println!("  agent_tool_id:  #{tid}");
    }
    if let Some(s) = e["status"].as_str().filter(|s| !s.is_empty()) {
        println!("  status:         {s}");
    }
    if let Some(s) = e["agent_state"].as_str().filter(|s| !s.is_empty()) {
        println!("  agent_state:    {s}");
    }
    println!(
        "  created:        {}",
        e["created_at"].as_str().unwrap_or("?")
    );
}
