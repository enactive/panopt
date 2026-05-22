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

/// Completion state of a [`Todo`].
///
/// A two-variant enum rather than a `bool` so rendering is a `match` and an
/// `InProgress` variant can be added later without an API break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Open,
    Done,
}

/// A todo item identified by a numeric id, unique within its project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    pub id: u64,
    pub title: String,
    pub status: TodoStatus,
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
