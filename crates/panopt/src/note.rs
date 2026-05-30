//! The `panopt note` subcommand: a thin MCP client of the daemon's
//! `note_*` tools.
//!
//! Only the destructive surface lives here for now - the rest of the
//! note UX runs through the in-cockpit editor form. `rm` exists so the
//! sidebar's delete confirmation dialog has a CLI to dispatch to, mirroring
//! how `todo rm` and `process delete` are wired (see [`crate::delete_gate`]).

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;
use serde_json::{json, Map, Value};

use crate::daemon;
use crate::mcpclient::Client;
use crate::todo::{insert_opt, observer_url};

/// What to do to the project's notes.
#[derive(Subcommand)]
pub enum NoteCmd {
    /// Delete a note.
    Rm {
        /// Numeric id of the note to delete.
        id: u64,
    },
    /// Edit a note's title, body, or tags. Omitted options are left unchanged.
    Set {
        /// Numeric id of the note to edit.
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

/// Run a `panopt note` subcommand against the daemon for project `ws`.
pub fn run(ws: Option<PathBuf>, cmd: NoteCmd, port: u16) -> Result<()> {
    daemon::ensure(None, port)?;
    let client = Client::connect(&observer_url(ws, port)?)?;
    let outcome = dispatch(&client, cmd);
    client.close();
    outcome
}

fn dispatch(client: &Client, cmd: NoteCmd) -> Result<()> {
    match cmd {
        NoteCmd::Rm { id } => {
            client.call("note_delete", json!({ "note_id": id }))?;
            println!("deleted note #{id}");
        }
        NoteCmd::Set {
            id,
            title,
            body,
            tags,
        } => {
            let mut args = Map::new();
            args.insert("note_id".into(), json!(id));
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
            client.call("note_update", Value::Object(args))?;
            println!("updated note #{id}");
        }
    }
    Ok(())
}
