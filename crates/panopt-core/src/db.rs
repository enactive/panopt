//! SQLite schema and forward migration for the persistent store.
//!
//! One database file holds every project. `todos` and `scratchpads` are keyed
//! by `(project_id, id)`, so ids restart at 1 per project and the projected
//! files read naturally. The schema version is tracked in SQLite's
//! `user_version` pragma so later changes can migrate forward in place.

use rusqlite::Connection;

/// The current schema version. Bump this and add a step to [`migrate`]
/// whenever the schema changes.
const SCHEMA_VERSION: i64 = 1;

/// Version 1: the initial three-table schema.
///
/// Per-project id counters live on the `projects` row rather than being
/// derived from `MAX(id)`, so a deleted highest todo never causes id reuse.
const SCHEMA_V1: &str = "
CREATE TABLE projects (
    id                 INTEGER PRIMARY KEY,
    root               TEXT    NOT NULL UNIQUE,
    next_todo_id       INTEGER NOT NULL DEFAULT 1,
    next_scratchpad_id INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE todos (
    project_id INTEGER NOT NULL REFERENCES projects(id),
    id         INTEGER NOT NULL,
    title      TEXT    NOT NULL,
    status     TEXT    NOT NULL,
    PRIMARY KEY (project_id, id)
);
CREATE TABLE scratchpads (
    project_id INTEGER NOT NULL REFERENCES projects(id),
    id         INTEGER NOT NULL,
    title      TEXT    NOT NULL,
    body       TEXT    NOT NULL,
    PRIMARY KEY (project_id, id)
);
";

/// Bring `conn` up to [`SCHEMA_VERSION`], creating tables on a fresh database.
///
/// Idempotent: a database already at the current version is left untouched.
pub(crate) fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version < 1 {
        conn.execute_batch(SCHEMA_V1)?;
    }
    if version != SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}
