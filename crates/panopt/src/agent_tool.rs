//! The `panopt agent-tool` subcommand: a thin MCP client of the daemon's
//! `agent_tool_*` tools.
//!
//! Agent tools are the durable configuration layer of the two-layer process
//! model (todo #27). Like `panopt todo`, every invocation ensures the daemon
//! is up, opens a one-shot `observer` MCP session, calls one tool, prints the
//! result, and closes.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;
use serde_json::{json, Map, Value};

use crate::daemon;
use crate::mcpclient::Client;
use crate::todo::{insert_opt, observer_url, render_scalar};

/// What to do to the project's agent-tool list.
#[derive(Subcommand)]
pub enum AgentToolCmd {
    /// List every agent tool.
    List,
    /// Show one agent tool in full.
    Get {
        /// Numeric id of the tool.
        id: u64,
    },
    /// Add an agent tool.
    Add {
        /// Identifier-style name for the tool.
        name: String,
        /// Human label shown in the cockpit (default: the name).
        #[arg(long = "display-name")]
        display_name: Option<String>,
        /// Shell command this tool launches when spawning a process.
        #[arg(long)]
        command: Option<String>,
        /// Working directory for the launched command.
        #[arg(long)]
        cwd: Option<String>,
        /// Free-form tag for categorization. Defaults to "agent".
        #[arg(long = "tool-type")]
        tool_type: Option<String>,
    },
    /// Edit an agent tool. Omitted options are left unchanged.
    Set {
        /// Numeric id of the tool to edit.
        id: u64,
        #[arg(long)]
        name: Option<String>,
        #[arg(long = "display-name")]
        display_name: Option<String>,
        #[arg(long)]
        command: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long = "tool-type")]
        tool_type: Option<String>,
    },
    /// Mark a tool as offered in spawn UIs.
    Enable {
        /// Numeric id of the tool.
        id: u64,
    },
    /// Hide a tool from spawn UIs without deleting its configuration.
    Disable {
        /// Numeric id of the tool.
        id: u64,
    },
    /// Delete an agent tool.
    Rm {
        /// Numeric id of the tool to delete.
        id: u64,
    },
}

/// Run a `panopt agent-tool` subcommand against the daemon for project `ws`.
pub fn run(ws: Option<PathBuf>, cmd: AgentToolCmd, port: u16) -> Result<()> {
    daemon::ensure(None, port)?;
    let client = Client::connect(&observer_url(ws, port)?)?;
    let outcome = dispatch(&client, cmd);
    client.close();
    outcome
}

fn dispatch(client: &Client, cmd: AgentToolCmd) -> Result<()> {
    match cmd {
        AgentToolCmd::List => {
            print_list(&client.call("agent_tool_list", json!({}))?);
        }
        AgentToolCmd::Get { id } => {
            print_entry(&client.call("agent_tool_get", json!({ "agent_tool_id": id }))?);
        }
        AgentToolCmd::Add {
            name,
            display_name,
            command,
            cwd,
            tool_type,
        } => {
            let mut args = Map::new();
            args.insert("name".into(), json!(name));
            insert_opt(&mut args, "display_name", display_name);
            insert_opt(&mut args, "command", command);
            insert_opt(&mut args, "cwd", cwd);
            insert_opt(&mut args, "tool_type", tool_type);
            let id = client.call("agent_tool_create", Value::Object(args))?;
            println!("created agent tool #{}", render_scalar(&id));
        }
        AgentToolCmd::Set {
            id,
            name,
            display_name,
            command,
            cwd,
            tool_type,
        } => {
            let mut args = Map::new();
            args.insert("agent_tool_id".into(), json!(id));
            insert_opt(&mut args, "name", name);
            insert_opt(&mut args, "display_name", display_name);
            insert_opt(&mut args, "command", command);
            insert_opt(&mut args, "cwd", cwd);
            insert_opt(&mut args, "tool_type", tool_type);
            client.call("agent_tool_update", Value::Object(args))?;
            println!("updated agent tool #{id}");
        }
        AgentToolCmd::Enable { id } => {
            client.call(
                "agent_tool_update",
                json!({ "agent_tool_id": id, "enabled": true }),
            )?;
            println!("enabled agent tool #{id}");
        }
        AgentToolCmd::Disable { id } => {
            client.call(
                "agent_tool_update",
                json!({ "agent_tool_id": id, "enabled": false }),
            )?;
            println!("disabled agent tool #{id}");
        }
        AgentToolCmd::Rm { id } => {
            client.call("agent_tool_delete", json!({ "agent_tool_id": id }))?;
            println!("deleted agent tool #{id}");
        }
    }
    Ok(())
}

fn print_list(v: &Value) {
    let entries = v.as_array().cloned().unwrap_or_default();
    if entries.is_empty() {
        println!("(no agent tools)");
        return;
    }
    for e in &entries {
        let id = e["id"].as_u64().unwrap_or(0);
        let label = e["display_name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| e["name"].as_str())
            .unwrap_or("");
        let enabled = if e["enabled"].as_bool().unwrap_or(true) {
            "enabled"
        } else {
            "disabled"
        };
        println!("#{id}  {label} [{enabled}]");
        if let Some(c) = e["command"].as_str().filter(|c| !c.is_empty()) {
            println!("     {c}");
        }
    }
}

fn print_entry(e: &Value) {
    let id = e["id"].as_u64().unwrap_or(0);
    println!("#{id}  {}", e["name"].as_str().unwrap_or(""));
    if let Some(d) = e["display_name"].as_str().filter(|s| !s.is_empty()) {
        println!("  display:    {d}");
    }
    if let Some(c) = e["command"].as_str().filter(|s| !s.is_empty()) {
        println!("  command:    {c}");
    }
    if let Some(c) = e["cwd"].as_str().filter(|s| !s.is_empty()) {
        println!("  cwd:        {c}");
    }
    println!(
        "  tool_type:  {}",
        e["tool_type"].as_str().unwrap_or("agent")
    );
    println!("  enabled:    {}", e["enabled"].as_bool().unwrap_or(true));
    println!("  created:    {}", e["created_at"].as_str().unwrap_or("?"));
}
