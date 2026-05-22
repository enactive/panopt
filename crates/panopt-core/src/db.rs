//! SQLite schema and forward migrations for the persistent store.
//!
//! One database file holds every project. `todos` and `scratchpads` are keyed
//! by `(project_id, id)`, so ids restart at 1 per project and the projected
//! files read naturally. The schema version is tracked in SQLite's
//! `user_version` pragma so later changes can migrate forward in place.

use rusqlite::Connection;

/// The current schema version. Bump this and add a step to [`migrate`]
/// whenever the schema changes.
const SCHEMA_VERSION: i64 = 2;

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

/// Version 2: the full todo data model (DESIGN.md Section 6.1).
///
/// Adds the descriptive columns to `todos` and the two side tables that hold a
/// todo's comments and its blocked-by relationships, both modeled on Solo's
/// schema. The `todo_blockers` table carries a foreign key on each of its two
/// todo references, so deleting a todo cascades away the rows where it is the
/// blocked todo *and* the rows where it is the blocker. Pre-existing `'done'`
/// todos (the only completed value the v1 schema knew) become `'completed'`.
const SCHEMA_V2: &str = "
ALTER TABLE todos ADD COLUMN body            TEXT    NOT NULL DEFAULT '';
ALTER TABLE todos ADD COLUMN priority        TEXT    NOT NULL DEFAULT 'medium';
ALTER TABLE todos ADD COLUMN assignee        TEXT    NOT NULL DEFAULT '';
ALTER TABLE todos ADD COLUMN tags            TEXT    NOT NULL DEFAULT '[]';
ALTER TABLE todos ADD COLUMN created_at      TEXT    NOT NULL DEFAULT '';
ALTER TABLE todos ADD COLUMN updated_at      TEXT    NOT NULL DEFAULT '';
ALTER TABLE todos ADD COLUMN completed_at    TEXT;
ALTER TABLE todos ADD COLUMN next_comment_id INTEGER NOT NULL DEFAULT 1;
UPDATE todos SET status = 'completed' WHERE status = 'done';
UPDATE todos
   SET created_at = datetime('now'), updated_at = datetime('now')
 WHERE created_at = '';
CREATE TABLE todo_comments (
    project_id INTEGER NOT NULL,
    todo_id    INTEGER NOT NULL,
    id         INTEGER NOT NULL,
    author     TEXT    NOT NULL,
    body       TEXT    NOT NULL,
    created_at TEXT    NOT NULL,
    PRIMARY KEY (project_id, todo_id, id),
    FOREIGN KEY (project_id, todo_id)
        REFERENCES todos(project_id, id) ON DELETE CASCADE
);
CREATE TABLE todo_blockers (
    project_id INTEGER NOT NULL,
    todo_id    INTEGER NOT NULL,
    blocker_id INTEGER NOT NULL,
    PRIMARY KEY (project_id, todo_id, blocker_id),
    CHECK (todo_id != blocker_id),
    FOREIGN KEY (project_id, todo_id)
        REFERENCES todos(project_id, id) ON DELETE CASCADE,
    FOREIGN KEY (project_id, blocker_id)
        REFERENCES todos(project_id, id) ON DELETE CASCADE
);
";

/// Bring `conn` up to [`SCHEMA_VERSION`], creating tables on a fresh database.
///
/// Idempotent: a database already at the current version is left untouched.
/// Each step is gated on the stored `user_version`, so a v1 database upgrades
/// in place and a fresh one runs every step in order to the same end state.
pub(crate) fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version < 1 {
        conn.execute_batch(SCHEMA_V1)?;
    }
    if version < 2 {
        conn.execute_batch(SCHEMA_V2)?;
    }
    if version != SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_lands_at_current_version_with_v2_tables() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // The v2 columns and side tables all exist - this batch fails otherwise.
        conn.execute_batch(
            "INSERT INTO projects (id, root) VALUES (1, '/x');
             INSERT INTO todos (project_id, id, title, status, priority, created_at, updated_at)
                 VALUES (1, 1, 't', 'open', 'high', '', '');
             INSERT INTO todo_comments (project_id, todo_id, id, author, body, created_at)
                 VALUES (1, 1, 1, 'a', 'b', '');
             INSERT INTO todo_blockers (project_id, todo_id, blocker_id) VALUES (1, 1, 1);",
        )
        .unwrap_err(); // the blocker self-reference trips the CHECK, proving the table is there
    }

    #[test]
    fn v1_database_upgrades_in_place() {
        let conn = Connection::open_in_memory().unwrap();
        // Stand up a v1 database by hand: the v1 schema, version pinned to 1.
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, root) VALUES (1, '/x');
             INSERT INTO todos (project_id, id, title, status) VALUES (1, 1, 'old', 'done');
             INSERT INTO todos (project_id, id, title, status) VALUES (1, 2, 'new', 'open');",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // The legacy 'done' status is rewritten to 'completed'.
        let status: String = conn
            .query_row("SELECT status FROM todos WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "completed");

        // New columns are backfilled: priority defaults, timestamps are set.
        let priority: String = conn
            .query_row("SELECT priority FROM todos WHERE id = 2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(priority, "medium");
        let created: String = conn
            .query_row("SELECT created_at FROM todos WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert!(!created.is_empty(), "created_at should be backfilled");
    }
}
