//! The error type for `panopt-core`.

use std::path::PathBuf;

/// Errors returned by [`crate::Store`] operations.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// No scratchpad exists with the given id in the project.
    #[error("scratchpad {0} not found")]
    ScratchpadNotFound(u64),

    /// No todo exists with the given id in the project.
    #[error("todo {0} not found")]
    TodoNotFound(u64),

    /// No roster entry exists with the given id in the project.
    #[error("roster entry {0} not found")]
    RosterNotFound(u64),

    /// A caller-supplied argument was rejected - for example, a todo asked to
    /// block itself. The message is safe to surface to the caller.
    #[error("{0}")]
    BadRequest(String),

    /// No project row exists with the given internal id. Indicates a stale
    /// [`crate::ProjectId`] used after its row vanished - a bug, not a user
    /// error.
    #[error("project {0} not found")]
    ProjectNotFound(i64),

    /// A project's workspace path does not exist or is not accessible, so its
    /// `.panopt/` projection cannot be located.
    #[error("workspace path not found or inaccessible: {0}")]
    Workspace(PathBuf),

    /// The SQLite store returned an error.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    /// A projected file could not be written. The committed database row is
    /// already authoritative; the daemon surfaces this rather than silently
    /// dropping it.
    #[error("projection i/o error: {0}")]
    Projection(#[from] std::io::Error),
}
