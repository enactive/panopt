//! The `panopt todo` subcommand: a thin MCP client of the daemon's todo tools.
//!
//! Every invocation ensures the daemon is up, opens a one-shot MCP session as
//! an `observer` (so the CLI never lands in the agent registry), calls one
//! tool, prints the result, and closes. It is what the cockpit's editable todo
//! form shells out to.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Subcommand;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Map, Value};

use crate::daemon;
use crate::mcpclient::Client;
use crate::paths;

/// What to do to the project's todos.
#[derive(Subcommand)]
pub enum TodoCmd {
    /// List every todo in the project.
    List,
    /// Show one todo in full - body, comments and all.
    Get {
        /// Numeric id of the todo.
        id: u64,
    },
    /// Create a new todo.
    Create {
        /// Title of the new todo.
        title: String,
    },
    /// Open the interactive todo form. Pass a todo id to edit it, or --new to
    /// create one. The cockpit sidebar launches this in a floating pane.
    Edit {
        /// Numeric id of the todo to edit.
        id: Option<u64>,
        /// Start a fresh todo instead of editing an existing one.
        #[arg(long)]
        new: bool,
    },
    /// Edit a todo's fields. Every option is independent; omitted ones are
    /// left unchanged.
    Set {
        /// Numeric id of the todo to edit.
        id: u64,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        /// One of open, in_progress, backlog, draft, completed, not_done.
        #[arg(long)]
        status: Option<String>,
        /// One of high, medium, low.
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        /// New tag list, comma-separated. Pass an empty string to clear tags.
        #[arg(long)]
        tags: Option<String>,
    },
    /// Mark a todo complete.
    Done { id: u64 },
    /// Delete a todo.
    Rm { id: u64 },
    /// Record that one todo is blocked by another.
    Block {
        /// The blocked todo.
        id: u64,
        /// The todo that blocks it.
        #[arg(long)]
        by: u64,
    },
    /// Remove a blocker link.
    Unblock {
        id: u64,
        #[arg(long)]
        by: u64,
    },
    /// Add a comment to a todo.
    Comment {
        id: u64,
        /// The comment text.
        body: String,
        /// Author name to record (default: panopt-cli).
        #[arg(long = "as")]
        author: Option<String>,
    },
}

/// Run a `panopt todo` subcommand against the daemon for project `ws`.
pub fn run(ws: Option<PathBuf>, cmd: TodoCmd, port: u16) -> Result<()> {
    // The form is a TUI, not a one-shot tool call - it owns its own session.
    if let TodoCmd::Edit { id, new } = cmd {
        return crate::edit::run(ws, id, new, port);
    }
    daemon::ensure(port)?;
    let client = Client::connect(&observer_url(ws, port)?)?;
    let outcome = dispatch(&client, cmd);
    client.close();
    outcome
}

/// The daemon MCP endpoint for project `ws`, scoped to a non-registering
/// `observer` connection and carrying the bearer token as a query parameter.
/// Shared by every `panopt` subcommand that talks to the daemon (todo,
/// scratchpad, agent-tool, process, id-kind, viewer panes, the form).
///
/// The token sits on the URL rather than in a header because the minimal
/// `mcpclient` we ship only knows how to set the session-id header; pinning
/// auth to a header would mean teaching every internal client to set it.
/// Logs of these URLs are local-only.
pub(crate) fn observer_url(ws: Option<PathBuf>, port: u16) -> Result<String> {
    let ws = resolve_ws(ws)?;
    let encoded_ws = utf8_percent_encode(&ws.to_string_lossy(), NON_ALPHANUMERIC).to_string();
    let token = panopt_core::auth::read_token(&paths::token()?)
        .context("reading the panopt token (start the daemon with `panopt up`)")?;
    let encoded_token = utf8_percent_encode(&token, NON_ALPHANUMERIC).to_string();
    Ok(format!(
        "http://127.0.0.1:{port}/mcp?ws={encoded_ws}&observer=1&token={encoded_token}"
    ))
}

/// The project root: the given path, or the current directory, canonicalized.
pub(crate) fn resolve_ws(ws: Option<PathBuf>) -> Result<PathBuf> {
    let ws = match ws {
        Some(ws) => ws,
        None => std::env::current_dir().context("reading the current directory")?,
    };
    std::fs::canonicalize(&ws).with_context(|| format!("no such directory: {}", ws.display()))
}

/// Call the tool that backs `cmd` and print a human-readable result.
fn dispatch(client: &Client, cmd: TodoCmd) -> Result<()> {
    match cmd {
        TodoCmd::List => {
            print_list(&client.call("todo_list", json!({}))?);
        }
        TodoCmd::Get { id } => {
            print_todo(&client.call("todo_get", json!({ "todo_id": id }))?);
        }
        TodoCmd::Create { title } => {
            let id = client.call("todo_create", json!({ "title": title }))?;
            println!("created todo #{}", render_scalar(&id));
        }
        TodoCmd::Edit { .. } => unreachable!("Edit is dispatched before the MCP client opens"),
        TodoCmd::Set {
            id,
            title,
            body,
            status,
            priority,
            assignee,
            tags,
        } => {
            let mut args = Map::new();
            args.insert("todo_id".into(), json!(id));
            insert_opt(&mut args, "title", title);
            insert_opt(&mut args, "body", body);
            insert_opt(&mut args, "status", status);
            insert_opt(&mut args, "priority", priority);
            insert_opt(&mut args, "assignee", assignee);
            if let Some(tags) = tags {
                let list: Vec<&str> = tags
                    .split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .collect();
                args.insert("tags".into(), json!(list));
            }
            client.call("todo_update", Value::Object(args))?;
            println!("updated todo #{id}");
        }
        TodoCmd::Done { id } => {
            client.call("todo_complete", json!({ "todo_id": id }))?;
            println!("completed todo #{id}");
        }
        TodoCmd::Rm { id } => {
            client.call("todo_delete", json!({ "todo_id": id }))?;
            println!("deleted todo #{id}");
        }
        TodoCmd::Block { id, by } => {
            client.call(
                "todo_add_blocker",
                json!({ "todo_id": id, "blocker_id": by }),
            )?;
            println!("todo #{id} is now blocked by #{by}");
        }
        TodoCmd::Unblock { id, by } => {
            client.call(
                "todo_remove_blocker",
                json!({ "todo_id": id, "blocker_id": by }),
            )?;
            println!("todo #{id} is no longer blocked by #{by}");
        }
        TodoCmd::Comment { id, body, author } => {
            let author = author.unwrap_or_else(|| "panopt-cli".to_string());
            let comment = client.call(
                "todo_comment_add",
                json!({ "todo_id": id, "body": body, "author": author }),
            )?;
            println!("added comment {} to todo #{id}", render_scalar(&comment));
        }
    }
    Ok(())
}

/// Insert `key: value` into `args` only when `value` is `Some`.
pub(crate) fn insert_opt(args: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(v) = value {
        args.insert(key.to_string(), Value::String(v));
    }
}

/// Render a scalar tool result (a bare number or string) without JSON quotes.
pub(crate) fn render_scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Print `todo_list` output as one indented entry per todo.
fn print_list(v: &Value) {
    let todos = v.as_array().cloned().unwrap_or_default();
    if todos.is_empty() {
        println!("(no todos)");
        return;
    }
    for t in &todos {
        let id = t["id"].as_u64().unwrap_or(0);
        let status = t["status"].as_str().unwrap_or("?");
        let priority = t["priority"].as_str().unwrap_or("?");
        let title = t["title"].as_str().unwrap_or("");
        println!("#{id}  [{status}] {title}  ({priority})");

        let mut extra = Vec::new();
        if let Some(a) = t["assignee"].as_str().filter(|a| !a.is_empty()) {
            extra.push(format!("assignee {a}"));
        }
        let tags = string_list(&t["tags"]);
        if !tags.is_empty() {
            extra.push(format!("tags {}", tags.join(", ")));
        }
        let blockers = id_list(&t["blockers"]);
        if !blockers.is_empty() {
            extra.push(format!("blocked by {}", blockers.join(", ")));
        }
        if let Some(n) = t["comment_count"].as_u64().filter(|n| *n > 0) {
            extra.push(format!("{n} comment(s)"));
        }
        if !extra.is_empty() {
            println!("     {}", extra.join("  -  "));
        }
    }
}

/// Print `todo_get` output as a labeled record.
fn print_todo(t: &Value) {
    let id = t["id"].as_u64().unwrap_or(0);
    println!("#{id}  {}", t["title"].as_str().unwrap_or(""));
    println!("  status:   {}", t["status"].as_str().unwrap_or("?"));
    println!("  priority: {}", t["priority"].as_str().unwrap_or("?"));
    if let Some(a) = t["assignee"].as_str().filter(|a| !a.is_empty()) {
        println!("  assignee: {a}");
    }
    let tags = string_list(&t["tags"]);
    if !tags.is_empty() {
        println!("  tags:     {}", tags.join(", "));
    }
    let blockers = id_list(&t["blockers"]);
    if !blockers.is_empty() {
        println!("  blocked by: {}", blockers.join(", "));
    }
    println!("  created:  {}", t["created_at"].as_str().unwrap_or("?"));
    println!("  updated:  {}", t["updated_at"].as_str().unwrap_or("?"));
    if let Some(c) = t["completed_at"].as_str() {
        println!("  completed: {c}");
    }
    if let Some(body) = t["body"].as_str().filter(|b| !b.trim().is_empty()) {
        println!("\n{body}");
    }
    if let Some(comments) = t["comments"].as_array().filter(|c| !c.is_empty()) {
        println!("\ncomments:");
        for c in comments {
            let author = c["author"].as_str().unwrap_or("?");
            let at = c["created_at"].as_str().unwrap_or("");
            let body = c["body"].as_str().unwrap_or("");
            println!("  - {author} ({at}): {body}");
        }
    }
}

/// The non-empty strings of a JSON array value.
fn string_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The ids of a JSON array value, rendered as `#n`.
fn id_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_u64)
                .map(|n| format!("#{n}"))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_scalar_drops_json_quotes() {
        assert_eq!(render_scalar(&json!("ok")), "ok");
        assert_eq!(render_scalar(&json!(7)), "7");
    }

    #[test]
    fn insert_opt_skips_none_and_keeps_some() {
        let mut args = Map::new();
        insert_opt(&mut args, "title", Some("hi".into()));
        insert_opt(&mut args, "body", None);
        assert_eq!(args.get("title"), Some(&json!("hi")));
        assert!(!args.contains_key("body"));
    }

    #[test]
    fn list_helpers_extract_strings_and_ids() {
        assert_eq!(string_list(&json!(["a", "b"])), vec!["a", "b"]);
        assert_eq!(id_list(&json!([1, 3])), vec!["#1", "#3"]);
        assert!(string_list(&json!(null)).is_empty());
    }
}
