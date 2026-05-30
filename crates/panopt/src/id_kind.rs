//! The `panopt id-kind` subcommand: a thin MCP client of the daemon's
//! `id_kind` tool.
//!
//! Resolves a numeric id to its resource kind (todo / note /
//! agent-tool / process) and a short label, using the unified per-project
//! id counter (see `panopt-core` V5 migration). Errors out with the daemon's
//! "id N not found" when the id is unallocated or soft-deleted.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::json;

use crate::daemon;
use crate::mcpclient::Client;
use crate::todo::observer_url;

pub fn run(ws: Option<PathBuf>, id: u64, port: u16) -> Result<()> {
    daemon::ensure(None, port)?;
    let client = Client::connect(&observer_url(ws, port)?)?;
    let outcome = client.call("id_kind", json!({ "id": id }));
    client.close();
    let v = outcome?;
    let kind = v["kind"].as_str().unwrap_or("?");
    let label = v["label"].as_str().unwrap_or("");
    if label.is_empty() {
        println!("#{id}  [{kind}]");
    } else {
        println!("#{id}  [{kind}] {label}");
    }
    Ok(())
}
