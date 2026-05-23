//! The `panopt roster` subcommand: a thin MCP client of the daemon's roster
//! tools.
//!
//! The roster is the project's persistent agents, commands, and terminals. Like
//! `panopt todo`, every invocation ensures the daemon is up, opens a one-shot
//! `observer` MCP session, calls one tool, prints the result, and closes.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::{json, Map, Value};

use crate::daemon;
use crate::mcpclient::Client;
use crate::todo::{insert_opt, observer_url, render_scalar};

/// What to do to the project's roster.
#[derive(Subcommand)]
pub enum RosterCmd {
    /// List every roster entry.
    List,
    /// Show one roster entry in full.
    Get {
        /// Numeric id of the entry.
        id: u64,
    },
    /// Add a roster entry.
    Add {
        /// Kind of entry: agent, command, or terminal.
        kind: String,
        /// Identifier-style name for the entry.
        name: String,
        /// Human label shown in the cockpit (default: the name).
        #[arg(long = "display-name")]
        display_name: Option<String>,
        /// Shell command the entry launches.
        #[arg(long)]
        command: Option<String>,
        /// Working directory for the launched command.
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Edit a roster entry. Omitted options are left unchanged.
    Set {
        /// Numeric id of the entry to edit.
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
    /// Delete a roster entry.
    Rm {
        /// Numeric id of the entry to delete.
        id: u64,
    },
}

/// Run a `panopt roster` subcommand against the daemon for project `ws`.
pub fn run(ws: Option<PathBuf>, cmd: RosterCmd, port: u16) -> Result<()> {
    daemon::ensure(port)?;
    let client = Client::connect(&observer_url(ws, port)?)?;
    let outcome = dispatch(&client, cmd);
    client.close();
    outcome
}

/// Call the tool that backs `cmd` and print a human-readable result.
fn dispatch(client: &Client, cmd: RosterCmd) -> Result<()> {
    match cmd {
        RosterCmd::List => {
            print_list(&client.call("roster_list", json!({}))?);
        }
        RosterCmd::Get { id } => {
            print_entry(&client.call("roster_get", json!({ "roster_id": id }))?);
        }
        RosterCmd::Add { kind, name, display_name, command, cwd } => {
            let mut args = Map::new();
            args.insert("kind".into(), json!(kind));
            args.insert("name".into(), json!(name));
            insert_opt(&mut args, "display_name", display_name);
            insert_opt(&mut args, "command", command);
            insert_opt(&mut args, "cwd", cwd);
            let id = client.call("roster_create", Value::Object(args))?;
            println!("created roster entry #{}", render_scalar(&id));
        }
        RosterCmd::Set { id, name, display_name, command, cwd } => {
            let mut args = Map::new();
            args.insert("roster_id".into(), json!(id));
            insert_opt(&mut args, "name", name);
            insert_opt(&mut args, "display_name", display_name);
            insert_opt(&mut args, "command", command);
            insert_opt(&mut args, "cwd", cwd);
            client.call("roster_update", Value::Object(args))?;
            println!("updated roster entry #{id}");
        }
        RosterCmd::Rm { id } => {
            client.call("roster_delete", json!({ "roster_id": id }))?;
            println!("deleted roster entry #{id}");
        }
    }
    Ok(())
}

/// `panopt _roster-run` - start a roster entry in the current pane.
///
/// Looks the entry up in the daemon and `exec`s its command, so the Zellij
/// command pane the sidebar plugin opened becomes the agent, command, or shell
/// itself - with no PANopt wrapper left around it. Rerunning the exited pane
/// through Zellij re-runs this shim, which re-fetches and re-execs.
pub fn exec_entry(ws: Option<PathBuf>, id: u64, port: u16) -> Result<()> {
    daemon::ensure(port)?;
    let client = Client::connect(&observer_url(ws.clone(), port)?)?;
    let entry = client.call("roster_get", json!({ "roster_id": id }));
    client.close();
    let entry = entry.with_context(|| format!("looking up roster entry #{id}"))?;

    let command = entry["command"].as_str().unwrap_or("").trim().to_string();
    let cwd = entry["cwd"].as_str().unwrap_or("").trim().to_string();

    // An entry with no command is a bare terminal: run an interactive shell.
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
    Err(err).context("could not start the roster entry's command")
}

/// Print `roster_list` output as one indented entry per row.
fn print_list(v: &Value) {
    let entries = v.as_array().cloned().unwrap_or_default();
    if entries.is_empty() {
        println!("(no roster entries)");
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
        println!("#{id}  [{kind}] {label}");
        if let Some(c) = e["command"].as_str().filter(|c| !c.is_empty()) {
            println!("     {c}");
        }
    }
}

/// Print `roster_get` output as a labeled record.
fn print_entry(e: &Value) {
    let id = e["id"].as_u64().unwrap_or(0);
    println!("#{id}  {}", e["name"].as_str().unwrap_or(""));
    println!("  kind:     {}", e["kind"].as_str().unwrap_or("?"));
    if let Some(d) = e["display_name"].as_str().filter(|s| !s.is_empty()) {
        println!("  display:  {d}");
    }
    if let Some(c) = e["command"].as_str().filter(|s| !s.is_empty()) {
        println!("  command:  {c}");
    }
    if let Some(c) = e["cwd"].as_str().filter(|s| !s.is_empty()) {
        println!("  cwd:      {c}");
    }
    println!("  created:  {}", e["created_at"].as_str().unwrap_or("?"));
}
