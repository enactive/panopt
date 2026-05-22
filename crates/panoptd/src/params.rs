//! Request parameter structs for the MCP tools.
//!
//! Each derives `Deserialize` + `JsonSchema` so rmcp can generate the tool's
//! input schema. `schemars` is used via rmcp's re-export to avoid a version
//! skew with whatever `schemars` rmcp itself depends on. Doc comments on the
//! fields become the parameter descriptions agents see.

use rmcp::schemars;
use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScratchpadCreateArgs {
    /// Human-readable title for the new scratchpad.
    pub title: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScratchpadAppendArgs {
    /// Numeric id of the scratchpad to append to.
    pub scratchpad_id: u64,
    /// Text to append. It is placed on its own line after existing content.
    pub content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScratchpadReadArgs {
    /// Numeric id of the scratchpad to read.
    pub scratchpad_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TodoCreateArgs {
    /// Short description of the todo.
    pub title: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TodoCompleteArgs {
    /// Numeric id of the todo to mark complete.
    pub todo_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TodoGetArgs {
    /// Numeric id of the todo to fetch.
    pub todo_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TodoDeleteArgs {
    /// Numeric id of the todo to delete. Its comments and blocker links go too.
    pub todo_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TodoUpdateArgs {
    /// Numeric id of the todo to edit.
    pub todo_id: u64,
    /// New title. Omit to leave unchanged.
    #[serde(default)]
    pub title: Option<String>,
    /// New free-form description body. Omit to leave unchanged.
    #[serde(default)]
    pub body: Option<String>,
    /// New status: one of open, in_progress, backlog, completed. Omit to leave
    /// unchanged.
    #[serde(default)]
    pub status: Option<String>,
    /// New priority: one of high, medium, low. Omit to leave unchanged.
    #[serde(default)]
    pub priority: Option<String>,
    /// New assignee name, or an empty string to clear it. Omit to leave
    /// unchanged.
    #[serde(default)]
    pub assignee: Option<String>,
    /// New complete tag list, replacing the old one. Omit to leave unchanged.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TodoBlockerArgs {
    /// Numeric id of the blocked todo.
    pub todo_id: u64,
    /// Numeric id of the todo that blocks it.
    pub blocker_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TodoCommentAddArgs {
    /// Numeric id of the todo to comment on.
    pub todo_id: u64,
    /// Comment text.
    pub body: String,
    /// Author name to record. Omit to use the calling agent's registered name;
    /// a non-agent caller (the `panopt` CLI) supplies this explicitly.
    #[serde(default)]
    pub author: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IdentifyArgs {
    /// Human-readable name for this agent, shown to others in the registry.
    pub name: String,
    /// Optional free-form status, for example "implementing auth" or "blocked".
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LockAcquireArgs {
    /// Name of the advisory lock to acquire - an agreed-on string such as a
    /// path, a task, or a phase of work.
    pub name: String,
    /// Optional reason for holding the lock, shown to other agents.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LockReleaseArgs {
    /// Name of the advisory lock to release.
    pub name: String,
}
