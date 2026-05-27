//! The `panopt scratchpad` subcommand: a thin MCP client of the daemon's
//! `scratchpad_*` tools.
//!
//! Only the destructive surface lives here for now - the rest of the
//! scratchpad UX runs through the in-cockpit editor form. `rm` exists so the
//! sidebar's delete confirmation dialog has a CLI to dispatch to, mirroring
//! how `todo rm` and `process delete` are wired (see [`crate::delete_gate`]).

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;
use serde_json::{json, Map, Value};

use crate::daemon;
use crate::mcpclient::Client;
use crate::todo::{insert_opt, observer_url};

/// What to do to the project's scratchpads.
#[derive(Subcommand)]
pub enum ScratchpadCmd {
    /// Delete a scratchpad.
    Rm {
        /// Numeric id of the scratchpad to delete.
        id: u64,
    },
    /// Edit a scratchpad's title, body, or tags. Omitted options are left unchanged.
    Set {
        /// Numeric id of the scratchpad to edit.
        id: u64,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        /// New tag list, comma-separated. Pass an empty string to clear tags.
        #[arg(long)]
        tags: Option<String>,
    },
}

/// Run a `panopt scratchpad` subcommand against the daemon for project `ws`.
pub fn run(ws: Option<PathBuf>, cmd: ScratchpadCmd, port: u16) -> Result<()> {
    daemon::ensure(port)?;
    let client = Client::connect(&observer_url(ws, port)?)?;
    let outcome = dispatch(&client, cmd);
    client.close();
    outcome
}

fn dispatch(client: &Client, cmd: ScratchpadCmd) -> Result<()> {
    match cmd {
        ScratchpadCmd::Rm { id } => {
            client.call("scratchpad_delete", json!({ "scratchpad_id": id }))?;
            println!("deleted scratchpad #{id}");
        }
        ScratchpadCmd::Set {
            id,
            title,
            body,
            tags,
        } => {
            let mut args = Map::new();
            args.insert("scratchpad_id".into(), json!(id));
            insert_opt(&mut args, "title", title);
            insert_opt(&mut args, "body", body);
            if let Some(tags) = tags {
                let list: Vec<&str> = tags
                    .split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .collect();
                args.insert("tags".into(), json!(list));
            }
            client.call("scratchpad_update", Value::Object(args))?;
            println!("updated scratchpad #{id}");
        }
    }
    Ok(())
}
