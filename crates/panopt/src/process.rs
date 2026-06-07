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
use panopt_core::agent_profiles::{build_launch, Facts, ProfileSet};
use serde_json::{json, Map, Value};

use crate::daemon;
use crate::mcpclient::Client;
use crate::todo::{insert_opt, observer_url, render_scalar, resolve_ws};
use crate::{paths, project_identity};

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
    /// Start an instance of an agent config (the instance lifecycle). Writes a
    /// `starting` process row the cockpit reconciles into a live pane.
    Start {
        /// Numeric id of the agent config (#N from `agent-tool list`).
        config_id: u64,
    },
    /// Stop a running instance: signal its process and mark the row stopped.
    /// The pane is left standing for the user to own.
    Stop {
        /// Numeric id of the process (instance) to stop.
        id: u64,
    },
}

/// Run a `panopt process` subcommand against the daemon for project `ws`.
pub fn run(ws: Option<PathBuf>, cmd: ProcessCmd, port: u16) -> Result<()> {
    daemon::ensure(None, port)?;
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
        ProcessCmd::Start { config_id } => {
            let row = client.call("process_start", json!({ "agent_tool_id": config_id }))?;
            let id = row["id"].as_u64().unwrap_or(0);
            let status = row["status"].as_str().unwrap_or("?");
            println!("started process #{id} ({status}) from config #{config_id}");
        }
        ProcessCmd::Stop { id } => {
            client.call("process_stop", json!({ "process_id": id }))?;
            println!("stopped process #{id}");
        }
    }
    Ok(())
}

/// `panopt _process-run` - run a process instance in the current pane.
///
/// This is the edge effector of the instance lifecycle (todo #141): the cockpit
/// plugin opens a pane running this shim, which resolves the spawn plan *here*,
/// on the executing host, and `exec`s it - so the Zellij pane becomes the agent
/// itself, with no PANopt wrapper left around it.
///
/// Two paths, keyed on whether the process has a backing agent config:
///
/// - **Agent-backed** (`agent_tool_id` set): look up the config's `tool_type`,
///   render its profile's spawn template (`build_launch`) against host-local
///   facts (this binary's path, the on-disk token, the daemon host/port). The
///   resolution must happen on this host - the daemon can't know our binary
///   path or token for a remote pane - which is why the interpreter runs at the
///   edge rather than in the daemon. The pid is reported back *before* the
///   `exec` (the pid survives `exec`, so the agent inherits it), which flips the
///   row `starting -> running`.
/// - **Bare** (no config): run the row's `command` via the shell, or an
///   interactive shell for a command-less terminal row. Unchanged behavior.
///
/// Rerunning the exited pane through Zellij re-runs this shim, which re-fetches
/// and re-execs.
pub fn exec_entry(ws: Option<PathBuf>, id: u64, port: u16) -> Result<()> {
    daemon::ensure(None, port)?;
    let client = Client::connect(&observer_url(ws.clone(), port)?)?;
    let entry = client.call("process_get", json!({ "process_id": id }));
    let entry = match entry {
        Ok(e) => e,
        Err(e) => {
            client.close();
            return Err(e).with_context(|| format!("looking up process #{id}"));
        }
    };

    if let Some(tool_id) = entry["agent_tool_id"].as_u64() {
        return exec_agent_instance(client, &entry, id, tool_id, ws, port);
    }
    client.close();

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

/// Resolve and exec an agent-backed instance (the `agent_tool_id` branch of
/// [`exec_entry`]). Consumes `client` so the daemon session is closed before the
/// `exec` replaces this process image.
fn exec_agent_instance(
    client: Client,
    entry: &Value,
    id: u64,
    tool_id: u64,
    ws: Option<PathBuf>,
    port: u16,
) -> Result<()> {
    // The config supplies the profile key; the row supplies the per-instance
    // identity (copied from the config at start, so edits don't perturb us).
    let config = client.call("agent_tool_get", json!({ "agent_tool_id": tool_id }));
    let report = |pid: u32| {
        // Best-effort: report the pid (which survives the exec below) so the
        // daemon flips the row to `running`. A failure here just leaves the row
        // `starting`; it must not block the spawn.
        let _ = client.call("process_report", json!({ "process_id": id, "pid": pid }));
    };
    let config = match config {
        Ok(c) => c,
        Err(e) => {
            client.close();
            return Err(e).with_context(|| format!("looking up agent config #{tool_id}"));
        }
    };
    let tool_type = config["tool_type"].as_str().unwrap_or("").to_string();

    let ws_path = resolve_ws(ws)?;
    let token = panopt_core::auth::read_token(&paths::token()?)
        .context("reading the panopt token (start the daemon with `panopt up`)")?;
    let panopt_bin = std::env::current_exe()
        .context("looking up panopt's own path for the agent's MCP config")?;
    let host = std::env::var("PANOPT_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    // The row's name is the stable agent id (copied from the config); fall back
    // to a synthetic id only if a row somehow has no name.
    let agent_id = entry["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("agent-{id}"));
    let name = entry["display_name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(&agent_id)
        .to_string();

    let profiles = ProfileSet::load().context("loading agent-type profiles")?;
    let profile = profiles.get(&tool_type).ok_or_else(|| {
        anyhow::anyhow!("agent config #{tool_id} names unknown agent type '{tool_type}'")
    })?;

    let facts = Facts {
        panopt_bin: panopt_bin.to_string_lossy().into_owned(),
        host,
        port,
        ws: ws_path.to_string_lossy().into_owned(),
        project: project_identity::resolve(&ws_path).key,
        agent_id,
        name,
        token,
        model: profile.default_model.clone(),
        process_id: Some(id),
    };
    let dir = paths::instance_dir(id)?;
    let mut launch = build_launch(profile, &facts, &dir)
        .with_context(|| format!("rendering the spawn plan for process #{id}"))?;
    // Per-launch extra args (#159): appended after the profile's own argv +
    // default_args, so an orchestrator's `spawn_agent(extra_args=...)` reaches
    // the real command line. They live on the instance row (copy-on-spawn), so
    // re-running this shim re-applies the same args without touching the config.
    if let Some(extra) = entry["extra_args"].as_array() {
        launch
            .argv
            .extend(extra.iter().filter_map(|v| v.as_str().map(str::to_string)));
    }

    report(std::process::id());
    client.close();

    let (program, rest) = launch
        .argv
        .split_first()
        .context("the resolved spawn plan has an empty argv")?;
    let mut cmd = Command::new(program);
    cmd.args(rest);
    cmd.envs(&launch.env);
    let cwd = entry["cwd"].as_str().unwrap_or("").trim();
    if !cwd.is_empty() {
        cmd.current_dir(cwd);
    } else {
        cmd.current_dir(&ws_path);
    }
    let err = cmd.exec();
    Err(err).with_context(|| format!("could not exec the agent ({})", program.clone()))
}

/// `panopt _process-report` - report runtime facts for an instance.
///
/// The cockpit plugin shells this after it reconciles a `starting` row into a
/// pane, to hand the daemon the Zellij pane id it landed in (the pid is
/// reported separately by the in-pane wrapper). Best-effort, like
/// [`crate::agent::leave`]: a missing daemon is logged by the caller, not
/// surfaced.
pub fn exec_report(
    ws: Option<PathBuf>,
    id: u64,
    pane_id: Option<String>,
    agent_state: Option<String>,
    port: u16,
) -> Result<()> {
    daemon::ensure(None, port)?;
    let client = Client::connect(&observer_url(ws, port)?)?;
    let mut args = Map::new();
    args.insert("process_id".into(), json!(id));
    insert_opt(&mut args, "pane_id", pane_id);
    insert_opt(&mut args, "agent_state", agent_state);
    let result = client.call("process_report", Value::Object(args));
    client.close();
    result
        .map(|_| ())
        .context("reporting process runtime facts")
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
    if let Some(pid) = e["pid"].as_i64() {
        println!("  pid:            {pid}");
    }
    if let Some(p) = e["pane_id"].as_str().filter(|s| !s.is_empty()) {
        println!("  pane_id:        {p}");
    }
    if let Some(s) = e["agent_state"].as_str().filter(|s| !s.is_empty()) {
        println!("  agent_state:    {s}");
    }
    println!(
        "  created:        {}",
        e["created_at"].as_str().unwrap_or("?")
    );
}
