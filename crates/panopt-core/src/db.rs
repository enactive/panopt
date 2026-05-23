//! SQLite schema and forward migrations for the persistent store.
//!
//! One database file holds every project. `todos` and `scratchpads` are keyed
//! by `(project_id, id)`, so ids restart at 1 per project and the projected
//! files read naturally. The schema version is tracked in SQLite's
//! `user_version` pragma so later changes can migrate forward in place.

use rusqlite::Connection;

/// The current schema version. Bump this and add a step to [`migrate`]
/// whenever the schema changes.
const SCHEMA_VERSION: i64 = 4;

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

/// Version 3: the persistent agent/command/terminal roster (todo #67).
///
/// Adds one `roster` table holding all three kinds, distinguished by a `kind`
/// column, modeled on Solo's `processes` table. Like `todos` and `scratchpads`
/// it is keyed `(project_id, id)` with a per-project id counter on `projects`.
/// Columns are deliberately minimal; per-entry settings (auto-start, env, an
/// agent-tool link) are left for later additions.
const SCHEMA_V3: &str = "
ALTER TABLE projects ADD COLUMN next_roster_id INTEGER NOT NULL DEFAULT 1;
CREATE TABLE roster (
    project_id   INTEGER NOT NULL REFERENCES projects(id),
    id           INTEGER NOT NULL,
    kind         TEXT    NOT NULL,
    name         TEXT    NOT NULL,
    display_name TEXT    NOT NULL DEFAULT '',
    command      TEXT    NOT NULL DEFAULT '',
    cwd          TEXT    NOT NULL DEFAULT '',
    position     INTEGER NOT NULL DEFAULT 0,
    created_at   TEXT    NOT NULL,
    PRIMARY KEY (project_id, id)
);
";

/// Version 4: timestamps on scratchpads (todo #76).
///
/// Adds `created_at` and `updated_at` to `scratchpads` so every mutation -
/// including append - can bump `updated_at` and the projected index line can
/// surface it. That makes the cockpit sidebar visibly refresh on every
/// scratchpad change, the same way every todo mutation already changes the
/// todo index line through its status/priority fields. The backfill mirrors
/// the v1->v2 todo backfill: any pre-existing scratchpad lands with both
/// timestamps set to `datetime('now')`.
///
/// The `ALTER`s run through [`add_column_if_missing`] rather than a raw
/// `execute_batch`, so a database stuck at `user_version = 3` whose
/// `scratchpads` table already carries the v4 columns - the transitional
/// state of any machine that ran an in-development v4 binary before the
/// `user_version` bump landed - upgrades cleanly instead of failing on
/// "duplicate column name". The backfill is unconditionally safe.
fn apply_v4(conn: &Connection) -> Result<(), rusqlite::Error> {
    add_column_if_missing(conn, "scratchpads", "created_at", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(conn, "scratchpads", "updated_at", "TEXT NOT NULL DEFAULT ''")?;
    conn.execute_batch(
        "UPDATE scratchpads
            SET created_at = datetime('now'), updated_at = datetime('now')
          WHERE created_at = '';",
    )
}

/// Add `column` to `table` only if it is not already there. Used by migrations
/// that may collide with a transitional dev-database state where the column
/// landed before its `user_version` bump did. `decl` is the type+constraint
/// text after the column name, exactly as it would appear in an `ALTER TABLE`.
///
/// `column` and `table` are fixed in-crate identifiers, not caller input, so
/// interpolating them into the SQL is safe.
fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let existing: String = row.get(1)?;
        if existing == column {
            return Ok(());
        }
    }
    drop(rows);
    drop(stmt);
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))
}

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
    if version < 3 {
        conn.execute_batch(SCHEMA_V3)?;
    }
    if version < 4 {
        apply_v4(conn)?;
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

    #[test]
    fn fresh_database_has_the_v3_roster_table() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, root) VALUES (1, '/x');
             INSERT INTO roster (project_id, id, kind, name, created_at)
                 VALUES (1, 1, 'agent', 'Claude', '');",
        )
        .unwrap();
        let next: i64 = conn
            .query_row("SELECT next_roster_id FROM projects WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(next, 1);
    }

    #[test]
    fn fresh_database_has_v4_scratchpad_timestamps() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // The v4 columns exist and accept inserts that set both timestamps.
        conn.execute_batch(
            "INSERT INTO projects (id, root) VALUES (1, '/x');
             INSERT INTO scratchpads (project_id, id, title, body, created_at, updated_at)
                 VALUES (1, 1, 't', '', datetime('now'), datetime('now'));",
        )
        .unwrap();
        let updated: String = conn
            .query_row("SELECT updated_at FROM scratchpads WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(!updated.is_empty(), "fresh scratchpad has updated_at set");
    }

    #[test]
    fn v3_database_upgrades_to_v4_in_place() {
        let conn = Connection::open_in_memory().unwrap();
        // Stand up a v3 database by hand: v1+v2+v3, version pinned to 3.
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, root) VALUES (1, '/x');
             INSERT INTO scratchpads (project_id, id, title, body)
                 VALUES (1, 1, 'old', 'note');",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // The pre-existing scratchpad is backfilled with real timestamps.
        let (created, updated): (String, String) = conn
            .query_row(
                "SELECT created_at, updated_at FROM scratchpads WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(!created.is_empty(), "created_at backfilled");
        assert!(!updated.is_empty(), "updated_at backfilled");
    }

    #[test]
    fn v4_migration_tolerates_a_transitional_database_with_columns_already_present() {
        // Models the local-dev drift that prompted this guard: an earlier
        // in-development v4 binary ran the ALTER on `scratchpads` but did not
        // bump `user_version`, so the database now reports v3 yet already
        // carries the v4 columns. `migrate` should upgrade it cleanly instead
        // of failing on a duplicate-column ALTER.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        conn.execute_batch(
            "ALTER TABLE scratchpads ADD COLUMN created_at TEXT NOT NULL DEFAULT '';
             ALTER TABLE scratchpads ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, root) VALUES (1, '/x');
             INSERT INTO scratchpads (project_id, id, title, body, created_at, updated_at)
                 VALUES (1, 1, 'pre-bumped', 'note', '', '');",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        // The backfill still runs, so the empty timestamps land at real ones.
        let updated: String = conn
            .query_row("SELECT updated_at FROM scratchpads WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert!(!updated.is_empty(), "updated_at backfilled even in the drift case");
    }

    #[test]
    fn v2_database_upgrades_to_v3_in_place() {
        let conn = Connection::open_in_memory().unwrap();
        // Stand up a v2 database by hand: the v1+v2 schema, version pinned to 2.
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
        conn.execute_batch("INSERT INTO projects (id, root) VALUES (1, '/x');")
            .unwrap();

        migrate(&conn).unwrap();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // The roster table and the new per-project counter both exist.
        conn.execute_batch(
            "INSERT INTO roster (project_id, id, kind, name, created_at)
                 VALUES (1, 1, 'command', 'Run', '');",
        )
        .unwrap();
        let next: i64 = conn
            .query_row("SELECT next_roster_id FROM projects WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(next, 1);
    }
}
