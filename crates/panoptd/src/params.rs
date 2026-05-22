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
