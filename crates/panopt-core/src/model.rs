//! Plain data types for PANopt's shared state.
//!
//! These carry no serialization or transport concerns; `panoptd` maps them onto
//! its own wire shapes at the protocol boundary.

use std::time::SystemTime;

/// Opaque handle to a project in the [`crate::Store`].
///
/// Minted by [`crate::Store::ensure_project`] and passed back into store
/// methods to scope them to one project. Callers treat it as a token - they
/// never construct or inspect it - so the inner row id stays private.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProjectId(pub(crate) i64);

/// A shared, append-oriented note identified by a stable numeric id.
///
/// The `id` (not the `title`) is the durable handle and the projected filename,
/// so renaming a scratchpad never moves its file. The id is unique within its
/// project and restarts at 1 in each project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scratchpad {
    pub id: u64,
    pub title: String,
    pub body: String,
}

/// Lifecycle state of a [`Todo`].
///
/// The four variants mirror Solo's `todos.status` column (DESIGN.md Section
/// 6.1): `Backlog` is not yet scheduled, `Open` is ready to pick up,
/// `InProgress` is being worked, `Completed` is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TodoStatus {
    #[default]
    Open,
    InProgress,
    Backlog,
    Completed,
}

impl TodoStatus {
    /// The token stored in SQLite and used on the wire and in the projection.
    pub fn as_str(self) -> &'static str {
        match self {
            TodoStatus::Open => "open",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Backlog => "backlog",
            TodoStatus::Completed => "completed",
        }
    }

    /// Parse a stored or wire token; `None` for an unrecognized string.
    pub fn parse(s: &str) -> Option<TodoStatus> {
        match s {
            "open" => Some(TodoStatus::Open),
            "in_progress" => Some(TodoStatus::InProgress),
            "backlog" => Some(TodoStatus::Backlog),
            "completed" => Some(TodoStatus::Completed),
            _ => None,
        }
    }
}

/// Importance of a [`Todo`], mirroring Solo's `todos.priority` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Priority {
    High,
    #[default]
    Medium,
    Low,
}

impl Priority {
    /// The token stored in SQLite and used on the wire and in the projection.
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::High => "high",
            Priority::Medium => "medium",
            Priority::Low => "low",
        }
    }

    /// Parse a stored or wire token; `None` for an unrecognized string.
    pub fn parse(s: &str) -> Option<Priority> {
        match s {
            "high" => Some(Priority::High),
            "medium" => Some(Priority::Medium),
            "low" => Some(Priority::Low),
            _ => None,
        }
    }
}

/// One comment on a [`Todo`], mirroring a row of Solo's `todo_comments` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoComment {
    /// Id unique within the parent todo, restarting at 1 in each todo.
    pub id: u64,
    /// Display name of the agent that posted it.
    pub author: String,
    pub body: String,
    /// SQLite `datetime('now')` text (UTC, `YYYY-MM-DD HH:MM:SS`).
    pub created_at: String,
}

/// A todo item identified by a numeric id, unique within its project.
///
/// The field set mirrors a trimmed subset of Solo's `todos` table plus its
/// `todo_comments` and `todo_blockers` side tables (DESIGN.md Section 6.1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Todo {
    pub id: u64,
    pub title: String,
    /// Free-form description. Empty until set.
    pub body: String,
    pub status: TodoStatus,
    pub priority: Priority,
    /// Free-text owner; empty when unassigned. Unlike Solo's agent foreign key
    /// this is a plain name: PANopt's agent registry is in-memory and ephemeral
    /// (DESIGN.md Section 6.3), so a persisted agent id would dangle.
    pub assignee: String,
    pub tags: Vec<String>,
    /// Ids of other todos in the same project that block this one.
    pub blockers: Vec<u64>,
    pub comments: Vec<TodoComment>,
    /// SQLite `datetime('now')` text (UTC).
    pub created_at: String,
    pub updated_at: String,
    /// Set while `status` is `Completed`, `None` otherwise.
    pub completed_at: Option<String>,
}

/// A set of optional edits to a [`Todo`], applied by [`crate::Store::todo_update`].
///
/// Each `None` field leaves that attribute untouched; each `Some` replaces it.
#[derive(Debug, Default, Clone)]
pub struct TodoPatch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub status: Option<TodoStatus>,
    pub priority: Option<Priority>,
    pub assignee: Option<String>,
    pub tags: Option<Vec<String>>,
}

/// A connected agent, as tracked by the in-memory registry.
///
/// Agents are ephemeral - the registry holds only those currently connected -
/// so an `Agent` is never persisted. `first_seen` / `last_seen` drive both
/// idle reporting and the pruning of agents that have gone silent.
#[derive(Debug, Clone)]
pub struct Agent {
    /// Opaque per-connection key (the MCP session id). Internal: it is the
    /// registry's map key, but is neither projected nor sent on the wire.
    pub key: String,
    /// Human-readable name. Defaults to the key until `identify` sets it.
    pub name: String,
    /// Free-form self-reported status, set via `identify`. Empty until then.
    pub status: String,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
}

/// An advisory lock: a named claim one agent holds to coordinate exclusive
/// work. The daemon never enforces it - agents cooperate voluntarily.
///
/// Like an [`Agent`], a lock is ephemeral and never persisted.
#[derive(Debug, Clone)]
pub struct Lock {
    /// What is locked - an arbitrary agreed-on name (a path, a task, a phase).
    pub name: String,
    /// Connection key of the holding agent.
    pub holder_key: String,
    /// The holder's display name, resolved from the registry when the lock is
    /// read or projected. Empty in stored entries - it is a read-time join.
    pub holder_name: String,
    /// Optional free-form reason the holder gave when acquiring.
    pub note: String,
    pub acquired_at: SystemTime,
}
