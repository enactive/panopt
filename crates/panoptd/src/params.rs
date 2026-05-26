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
pub struct ScratchpadGetArgs {
    /// Numeric id of the scratchpad to fetch in full.
    pub scratchpad_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScratchpadUpdateArgs {
    /// Numeric id of the scratchpad to edit.
    pub scratchpad_id: u64,
    /// New title. Omit to leave unchanged.
    #[serde(default)]
    pub title: Option<String>,
    /// Replacement body. Replaces the existing body in full. Omit to leave
    /// unchanged.
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScratchpadDeleteArgs {
    /// Numeric id of the scratchpad to delete.
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
pub struct TodoCommentUpdateArgs {
    /// Numeric id of the todo the comment lives on.
    pub todo_id: u64,
    /// Numeric id of the comment to edit (per-todo, restarts at 1 in each todo).
    pub comment_id: u64,
    /// Replacement body. The author and timestamp are preserved.
    pub body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TodoCommentDeleteArgs {
    /// Numeric id of the todo the comment lives on.
    pub todo_id: u64,
    /// Numeric id of the comment to delete.
    pub comment_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TodoSetBlockersArgs {
    /// Numeric id of the todo whose blocker set is being replaced.
    pub todo_id: u64,
    /// Replacement set of blocker ids. May be empty to clear all blockers.
    pub blocker_ids: Vec<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TodoLockArgs {
    /// Numeric id of the todo to claim. The advisory lock name is `todo:<id>`.
    pub todo_id: u64,
    /// Optional reason, shown to other agents in `lock_status`.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TodoUnlockArgs {
    /// Numeric id of the todo whose lock to release.
    pub todo_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AgentToolCreateArgs {
    /// Identifier-style name for the tool (e.g. "claude").
    pub name: String,
    /// Optional human label shown in the cockpit. Omit to use `name`.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Shell command this tool launches when a process is spawned from it.
    #[serde(default)]
    pub command: Option<String>,
    /// Working directory passed to the launched command. Omit to use project root.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Free-form tag for future categorization. Defaults to "agent".
    #[serde(default)]
    pub tool_type: Option<String>,
    /// Whether the tool is offered in spawn UIs. Defaults to true.
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AgentToolGetArgs {
    /// Numeric id of the agent tool to fetch.
    pub agent_tool_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AgentToolUpdateArgs {
    /// Numeric id of the agent tool to edit.
    pub agent_tool_id: u64,
    /// New name. Omit to leave unchanged.
    #[serde(default)]
    pub name: Option<String>,
    /// New display label. Omit to leave unchanged.
    #[serde(default)]
    pub display_name: Option<String>,
    /// New launch command. Omit to leave unchanged.
    #[serde(default)]
    pub command: Option<String>,
    /// New working directory. Omit to leave unchanged.
    #[serde(default)]
    pub cwd: Option<String>,
    /// New tool_type tag. Omit to leave unchanged.
    #[serde(default)]
    pub tool_type: Option<String>,
    /// New enabled flag. Omit to leave unchanged.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// New sort position. Omit to leave unchanged.
    #[serde(default)]
    pub position: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AgentToolDeleteArgs {
    /// Numeric id of the agent tool to delete.
    pub agent_tool_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProcessCreateArgs {
    /// Kind of process: one of agent, command, terminal.
    pub kind: String,
    /// Identifier-style name for the process.
    pub name: String,
    /// Optional human label shown in the cockpit. Omit to use `name`.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Shell command the process executes. Omit for a bare terminal.
    #[serde(default)]
    pub command: Option<String>,
    /// Working directory for the process. Omit to use project root.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Numeric id of the agent tool this process was spawned from. Omit for
    /// command and terminal processes that have no backing config.
    #[serde(default)]
    pub agent_tool_id: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProcessGetArgs {
    /// Numeric id of the process to fetch.
    pub process_id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProcessUpdateArgs {
    /// Numeric id of the process to edit.
    pub process_id: u64,
    /// New name. Omit to leave unchanged.
    #[serde(default)]
    pub name: Option<String>,
    /// New display label. Omit to leave unchanged.
    #[serde(default)]
    pub display_name: Option<String>,
    /// New launch command. Omit to leave unchanged.
    #[serde(default)]
    pub command: Option<String>,
    /// New working directory. Omit to leave unchanged.
    #[serde(default)]
    pub cwd: Option<String>,
    /// New sort position. Omit to leave unchanged.
    #[serde(default)]
    pub position: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProcessDeleteArgs {
    /// Numeric id of the process to delete.
    pub process_id: u64,
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
