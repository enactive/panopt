//! SQLite schema and forward migrations for the persistent store.
//!
//! One database file holds every project. `todos` and `notes` are keyed
//! by `(project_id, id)`, so ids restart at 1 per project and the projected
//! files read naturally. The schema version is tracked in SQLite's
//! `user_version` pragma so later changes can migrate forward in place.
//!
//! Note: the `notes` table was called `scratchpads` through V8; the V1-V8
//! migration steps below keep that historical name and V9 renames it. Only
//! runtime code (and the schema as of V9) speaks of `notes`.

use rusqlite::Connection;

/// The current schema version. Bump this and add a step to [`migrate`]
/// whenever the schema changes.
const SCHEMA_VERSION: i64 = 9;

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
    add_column_if_missing(
        conn,
        "scratchpads",
        "created_at",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(
        conn,
        "scratchpads",
        "updated_at",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    conn.execute_batch(
        "UPDATE scratchpads
            SET created_at = datetime('now'), updated_at = datetime('now')
          WHERE created_at = '';",
    )
}

/// Version 5: unify per-resource id counters into one global `next_id` (todo #16).
///
/// Before V5 every project row carried three independent counters -
/// `next_todo_id`, `next_scratchpad_id`, `next_roster_id` - so the same number
/// could be handed out as a todo *and* a scratchpad *and* a roster entry. V5
/// replaces them with a single `next_id` so a `#N` reference points to exactly
/// one resource type forever. Existing rows are left in place: SQLite's
/// `(project_id, id)` primary keys are per-table, so any collision in the
/// pre-V5 data persists as-is and only the fresh allocations after the
/// migration are guaranteed unique.
///
/// The seed for `next_id` is `max(old three counters)`, which is the lowest
/// value safe against every pre-existing id in any of the three tables. The
/// three old columns are then dropped.
fn apply_v5(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "ALTER TABLE projects ADD COLUMN next_id INTEGER NOT NULL DEFAULT 1;
         UPDATE projects
            SET next_id = MAX(next_todo_id, next_scratchpad_id, next_roster_id);
         ALTER TABLE projects DROP COLUMN next_todo_id;
         ALTER TABLE projects DROP COLUMN next_scratchpad_id;
         ALTER TABLE projects DROP COLUMN next_roster_id;",
    )
}

/// Version 6: split the hybrid `roster` table into a config layer
/// (`agent_tools`) and an instance layer (`processes`) (todo #27).
///
/// A V5 roster row was both a config (name, command, cwd) and the implicit
/// single instance of that config; liveness was derived from Zellij pane state,
/// never stored. V6 mirrors Solo's two-layer model: `agent_tools` holds the
/// durable configurations, `processes` holds per-project instances and carries
/// a nullable FK back to its source tool plus nullable lifecycle columns
/// (`pid`, `status`, `agent_state`, `last_seen`) so a follow-up that owns
/// process spawn can populate them without another migration.
///
/// Both new tables keep drawing ids from the unified `projects.next_id`
/// counter introduced by V5, so a `#N` reference still resolves to exactly one
/// row across todos, scratchpads, agent_tools, and processes.
///
/// Migration of existing rows: `kind='agent'` becomes an `agent_tools` row
/// (configuration), and `kind='command'` / `kind='terminal'` become
/// `processes` rows (instances - the only thing the pre-split roster ever was
/// for those kinds). Ids are preserved verbatim since both new tables share
/// the same `next_id` sequence the originals were drawn from.
fn apply_v6(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE agent_tools (
             project_id   INTEGER NOT NULL REFERENCES projects(id),
             id           INTEGER NOT NULL,
             name         TEXT    NOT NULL,
             display_name TEXT    NOT NULL DEFAULT '',
             command      TEXT    NOT NULL DEFAULT '',
             cwd          TEXT    NOT NULL DEFAULT '',
             tool_type    TEXT    NOT NULL DEFAULT 'agent',
             enabled      INTEGER NOT NULL DEFAULT 1,
             position     INTEGER NOT NULL DEFAULT 0,
             created_at   TEXT    NOT NULL,
             PRIMARY KEY (project_id, id)
         );
         CREATE TABLE processes (
             project_id    INTEGER NOT NULL REFERENCES projects(id),
             id            INTEGER NOT NULL,
             kind          TEXT    NOT NULL CHECK (kind IN ('agent','command','terminal')),
             name          TEXT    NOT NULL DEFAULT '',
             display_name  TEXT    NOT NULL DEFAULT '',
             command       TEXT    NOT NULL DEFAULT '',
             cwd           TEXT    NOT NULL DEFAULT '',
             position      INTEGER NOT NULL DEFAULT 0,
             agent_tool_id INTEGER,
             pid           INTEGER,
             status        TEXT,
             agent_state   TEXT,
             last_seen     TEXT,
             created_at    TEXT    NOT NULL,
             PRIMARY KEY (project_id, id)
         );
         INSERT INTO agent_tools
             (project_id, id, name, display_name, command, cwd, tool_type, enabled, position, created_at)
             SELECT project_id, id, name, display_name, command, cwd, 'agent', 1, position, created_at
               FROM roster WHERE kind = 'agent';
         INSERT INTO processes
             (project_id, id, kind, name, display_name, command, cwd, position, created_at)
             SELECT project_id, id, kind, name, display_name, command, cwd, position, created_at
               FROM roster WHERE kind IN ('command','terminal');
         DROP TABLE roster;",
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
    if version < 5 {
        apply_v5(conn)?;
    }
    if version < 6 {
        apply_v6(conn)?;
    }
    if version < 7 {
        apply_v7(conn)?;
    }
    if version < 8 {
        apply_v8(conn)?;
    }
    if version < 9 {
        apply_v9(conn)?;
    }
    if version != SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}

/// Version 7: soft delete (todo #54).
///
/// Each item-bearing table grows a nullable `deleted_at` column. `*_delete`
/// stamps it with `datetime('now')` instead of issuing a `DELETE FROM`, and
/// every read path (`*_list`, `*_get`, `fetch_*`, the blocker-resolution helper)
/// filters on `deleted_at IS NULL`. The row stays behind so a future undelete
/// surface has something to revive; recovery / cleanup tooling is deferred.
fn apply_v7(conn: &Connection) -> Result<(), rusqlite::Error> {
    add_column_if_missing(conn, "todos", "deleted_at", "TEXT")?;
    add_column_if_missing(conn, "scratchpads", "deleted_at", "TEXT")?;
    add_column_if_missing(conn, "agent_tools", "deleted_at", "TEXT")?;
    add_column_if_missing(conn, "processes", "deleted_at", "TEXT")?;
    Ok(())
}

/// Version 8: tags on scratchpads (todo #61).
///
/// Scratchpads grow the same `tags` column todos got in V2, stored as a JSON
/// array string with default `'[]'`. The vocabulary is shared with todos -
/// `state::tags_list` returns the union across both kinds - so a tag chosen on
/// one surface is offered up on the other. The column uses
/// [`add_column_if_missing`] for the same dev-database drift case the earlier
/// scratchpad migration (V4) guarded against.
fn apply_v8(conn: &Connection) -> Result<(), rusqlite::Error> {
    add_column_if_missing(conn, "scratchpads", "tags", "TEXT NOT NULL DEFAULT '[]'")
}

/// Version 9: rename the `scratchpads` table to `notes` (todo #79).
///
/// "Scratchpad" implied an ephemerality the concept never had - these are
/// durable, append-oriented, shared documents that agents and humans both read
/// and write. The rename is the whole change: the columns, the unified
/// `projects.next_id` counter, and the tag vocabulary shared with todos are all
/// untouched, and every pre-existing scratchpad survives as a note with its id
/// and body intact. Whether notes should additionally carry a `type`
/// (note/plan/memory/inter-agent) is deferred to a follow-up.
///
/// Guarded like the column migrations against dev-database drift: an
/// in-development V9 binary may have renamed the table before its
/// `user_version` bump landed, so a database reporting V8 might already have a
/// `notes` table. Skip the rename in that case instead of failing on
/// "no such table: scratchpads".
fn apply_v9(conn: &Connection) -> Result<(), rusqlite::Error> {
    if table_exists(conn, "notes")? {
        return Ok(());
    }
    conn.execute_batch("ALTER TABLE scratchpads RENAME TO notes;")
}

/// True if a table named `table` exists. Used by migrations that must stay
/// idempotent against a transitional dev-database that already applied a
/// rename or create before its `user_version` bump landed.
fn table_exists(conn: &Connection, table: &str) -> Result<bool, rusqlite::Error> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get(0),
    )?;
    Ok(count > 0)
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
            .query_row("SELECT created_at FROM todos WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(!created.is_empty(), "created_at should be backfilled");
    }

    #[test]
    fn fresh_database_has_the_v6_agent_tools_and_processes_tables() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, root) VALUES (1, '/x');
             INSERT INTO agent_tools (project_id, id, name, created_at)
                 VALUES (1, 1, 'Claude', '');
             INSERT INTO processes (project_id, id, kind, name, agent_tool_id, created_at)
                 VALUES (1, 2, 'agent', 'claude-1', 1, '');",
        )
        .unwrap();
        let next: i64 = conn
            .query_row("SELECT next_id FROM projects WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(next, 1);
    }

    #[test]
    fn fresh_database_has_v4_note_timestamps() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // The v4 columns exist and accept inserts that set both timestamps.
        // A fresh database is fully migrated, so the table is `notes` (V9).
        conn.execute_batch(
            "INSERT INTO projects (id, root) VALUES (1, '/x');
             INSERT INTO notes (project_id, id, title, body, created_at, updated_at)
                 VALUES (1, 1, 't', '', datetime('now'), datetime('now'));",
        )
        .unwrap();
        let updated: String = conn
            .query_row("SELECT updated_at FROM notes WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(!updated.is_empty(), "fresh note has updated_at set");
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

        // The pre-existing scratchpad is backfilled with real timestamps and,
        // post-migration, lives in the renamed `notes` table (V9).
        let (created, updated): (String, String) = conn
            .query_row(
                "SELECT created_at, updated_at FROM notes WHERE id = 1",
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
        // The table is `notes` post-migration (V9).
        let updated: String = conn
            .query_row("SELECT updated_at FROM notes WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            !updated.is_empty(),
            "updated_at backfilled even in the drift case"
        );
    }

    #[test]
    fn v2_database_upgrades_through_v6_in_place() {
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

        // The post-V6 tables and the unified counter all exist; the old
        // `roster` table is gone.
        conn.execute_batch(
            "INSERT INTO agent_tools (project_id, id, name, created_at)
                 VALUES (1, 1, 'Run', '');
             INSERT INTO processes (project_id, id, kind, name, created_at)
                 VALUES (1, 2, 'command', 'Run', '');",
        )
        .unwrap();
        conn.execute_batch("INSERT INTO roster (project_id, id, kind, name, created_at) VALUES (1, 3, 'agent', 'x', '');")
            .unwrap_err();
        let next: i64 = conn
            .query_row("SELECT next_id FROM projects WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(next, 1);
    }

    #[test]
    fn v4_database_upgrades_through_v6_in_place() {
        // Stand up a V4 database by hand: V1+V2+V3 schema, V4 column adds, then
        // pin user_version=4. Three resources at id 4 in three tables - the
        // collision V5 forever prevents going forward - and the three counters
        // disagree so we can prove V5 takes the max. The single agent roster
        // row then lands in `agent_tools` after V6, not in `processes`.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        apply_v4(&conn).unwrap();
        conn.pragma_update(None, "user_version", 4).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, root, next_todo_id, next_scratchpad_id, next_roster_id)
                 VALUES (1, '/x', 5, 3, 7);
             INSERT INTO todos (project_id, id, title, status, priority, created_at, updated_at)
                 VALUES (1, 4, 't', 'open', 'high', '', '');
             INSERT INTO scratchpads (project_id, id, title, body, created_at, updated_at)
                 VALUES (1, 4, 's', '', datetime('now'), datetime('now'));
             INSERT INTO roster (project_id, id, kind, name, created_at)
                 VALUES (1, 4, 'agent', 'r', '');",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // The unified counter is the max of the three old ones.
        let next: i64 = conn
            .query_row("SELECT next_id FROM projects WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(next, 7);

        // The three old columns are gone.
        for col in ["next_todo_id", "next_scratchpad_id", "next_roster_id"] {
            let mut stmt = conn.prepare("PRAGMA table_info(projects)").unwrap();
            let mut rows = stmt.query([]).unwrap();
            let mut present = false;
            while let Some(row) = rows.next().unwrap() {
                let name: String = row.get(1).unwrap();
                if name == col {
                    present = true;
                }
            }
            assert!(!present, "{col} should have been dropped");
        }

        // V6: the agent row landed in agent_tools, nothing in processes, and
        // the roster table is gone.
        let (tool_id, tool_name): (i64, String) = conn
            .query_row(
                "SELECT id, name FROM agent_tools WHERE project_id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(tool_id, 4);
        assert_eq!(tool_name, "r");
        let process_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM processes WHERE project_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(process_count, 0);
        let roster_present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'roster'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(roster_present, 0, "roster table should have been dropped");
    }

    #[test]
    fn v5_database_upgrades_to_v6_in_place() {
        // Stand up a V5 database by hand: V1..V3 schema + V4/V5 functions, pin
        // user_version=5. Three roster rows of mixed kinds at ids 1/2/3, and a
        // next_id of 4 to prove the unified counter is preserved across V6.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        apply_v4(&conn).unwrap();
        apply_v5(&conn).unwrap();
        conn.pragma_update(None, "user_version", 5).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, root, next_id) VALUES (1, '/x', 4);
             INSERT INTO roster (project_id, id, kind, name, display_name, command, cwd, position, created_at)
                 VALUES (1, 1, 'agent', 'claude', 'Claude', 'claude', '', 1, '2026-01-01');
             INSERT INTO roster (project_id, id, kind, name, display_name, command, cwd, position, created_at)
                 VALUES (1, 2, 'command', 'build', '', 'cargo build', '/tmp', 2, '2026-01-02');
             INSERT INTO roster (project_id, id, kind, name, display_name, command, cwd, position, created_at)
                 VALUES (1, 3, 'terminal', 'shell', '', '', '', 3, '2026-01-03');",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // The agent row landed in agent_tools at its original id.
        let (tool_id, tool_name, tool_type, enabled): (i64, String, String, i64) = conn
            .query_row(
                "SELECT id, name, tool_type, enabled FROM agent_tools WHERE project_id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(tool_id, 1);
        assert_eq!(tool_name, "claude");
        assert_eq!(tool_type, "agent");
        assert_eq!(enabled, 1, "migrated agent tools default to enabled");

        // The command + terminal rows landed in processes at their original
        // ids, with nullable agent_tool_id left NULL since pre-V6 had no link.
        let mut stmt = conn
            .prepare(
                "SELECT id, kind, name, agent_tool_id FROM processes
                  WHERE project_id = 1 ORDER BY id",
            )
            .unwrap();
        let rows: Vec<(i64, String, String, Option<i64>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], (2, "command".into(), "build".into(), None));
        assert_eq!(rows[1], (3, "terminal".into(), "shell".into(), None));

        // next_id is preserved, so the next allocation lands at 4 just like
        // pre-migration.
        let next: i64 = conn
            .query_row("SELECT next_id FROM projects WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(next, 4);

        // The old roster table is gone.
        let roster_present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'roster'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(roster_present, 0);
    }

    #[test]
    fn v6_database_upgrades_to_v7_with_soft_delete_columns() {
        // Stand up a V6 database, populate one row per item-bearing table,
        // and confirm V7 adds the `deleted_at` column without disturbing the
        // existing rows. The columns are nullable, so backfill is a no-op.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        apply_v4(&conn).unwrap();
        apply_v5(&conn).unwrap();
        apply_v6(&conn).unwrap();
        conn.pragma_update(None, "user_version", 6).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, root, next_id) VALUES (1, '/x', 5);
             INSERT INTO todos (project_id, id, title, status, created_at, updated_at)
                 VALUES (1, 1, 't', 'open', '2026-01-01', '2026-01-01');
             INSERT INTO scratchpads (project_id, id, title, body, created_at, updated_at)
                 VALUES (1, 2, 's', '', '2026-01-01', '2026-01-01');
             INSERT INTO agent_tools
                 (project_id, id, name, position, created_at)
                 VALUES (1, 3, 'claude', 1, '2026-01-01');
             INSERT INTO processes
                 (project_id, id, kind, name, created_at)
                 VALUES (1, 4, 'agent', 'claude-1', '2026-01-01');",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // Every pre-V7 row is live (deleted_at IS NULL) post-migration. The
        // scratchpads table is `notes` after V9.
        for table in ["todos", "notes", "agent_tools", "processes"] {
            let alive: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE deleted_at IS NULL"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(alive, 1, "{table} row survives V7 migration as live");
        }
    }

    #[test]
    fn v7_database_upgrades_to_v8_with_empty_note_tags() {
        // Stand up a V7 database with one pre-existing scratchpad. V8 adds the
        // tags column with default '[]', so the row keeps its body untouched
        // and reads back an empty JSON array - no backfill needed.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        apply_v4(&conn).unwrap();
        apply_v5(&conn).unwrap();
        apply_v6(&conn).unwrap();
        apply_v7(&conn).unwrap();
        conn.pragma_update(None, "user_version", 7).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, root, next_id) VALUES (1, '/x', 2);
             INSERT INTO scratchpads (project_id, id, title, body, created_at, updated_at)
                 VALUES (1, 1, 'pre-v8', 'note', '2026-01-01', '2026-01-01');",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // Post-V9 the table is `notes`.
        let tags: String = conn
            .query_row("SELECT tags FROM notes WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tags, "[]", "pre-V8 scratchpad gets the empty-tags default");
    }

    #[test]
    fn v8_migration_tolerates_a_transitional_database_with_tags_already_present() {
        // The same drift case V4 guards against: an in-development V8 binary
        // ran the ALTER on `scratchpads` but did not bump `user_version`, so
        // the database now reports v7 yet already carries the tags column.
        // `migrate` should upgrade it cleanly instead of failing on duplicate.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        apply_v4(&conn).unwrap();
        apply_v5(&conn).unwrap();
        apply_v6(&conn).unwrap();
        apply_v7(&conn).unwrap();
        conn.execute_batch("ALTER TABLE scratchpads ADD COLUMN tags TEXT NOT NULL DEFAULT '[]';")
            .unwrap();
        conn.pragma_update(None, "user_version", 7).unwrap();

        migrate(&conn).unwrap();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn v8_database_renames_scratchpads_to_notes_preserving_rows() {
        // Stand up a V8 database with one scratchpad, then migrate. V9 renames
        // the table to `notes`; the row keeps its id, title, body, and tags,
        // and the old `scratchpads` name no longer resolves.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        apply_v4(&conn).unwrap();
        apply_v5(&conn).unwrap();
        apply_v6(&conn).unwrap();
        apply_v7(&conn).unwrap();
        apply_v8(&conn).unwrap();
        conn.pragma_update(None, "user_version", 8).unwrap();
        conn.execute_batch(
            "INSERT INTO projects (id, root, next_id) VALUES (1, '/x', 2);
             INSERT INTO scratchpads (project_id, id, title, body, tags, created_at, updated_at)
                 VALUES (1, 1, 'kept', 'body text', '[\"a\"]', '2026-01-01', '2026-01-01');",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // The row survives verbatim under the new table name.
        let (title, body, tags): (String, String, String) = conn
            .query_row(
                "SELECT title, body, tags FROM notes WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(title, "kept");
        assert_eq!(body, "body text");
        assert_eq!(tags, "[\"a\"]");

        // The old name is gone.
        assert!(!table_exists(&conn, "scratchpads").unwrap());
        assert!(table_exists(&conn, "notes").unwrap());
    }

    #[test]
    fn v9_rename_tolerates_a_transitional_database_already_renamed() {
        // The drift case the V9 guard exists for: an in-development V9 binary
        // renamed the table to `notes` but did not bump `user_version`, so the
        // database reports V8 yet already has `notes` and no `scratchpads`.
        // `migrate` should finish cleanly instead of failing on the rename.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        apply_v4(&conn).unwrap();
        apply_v5(&conn).unwrap();
        apply_v6(&conn).unwrap();
        apply_v7(&conn).unwrap();
        apply_v8(&conn).unwrap();
        conn.execute_batch("ALTER TABLE scratchpads RENAME TO notes;")
            .unwrap();
        conn.pragma_update(None, "user_version", 8).unwrap();

        migrate(&conn).unwrap();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(table_exists(&conn, "notes").unwrap());
    }

    #[test]
    fn v6_processes_kind_check_rejects_invalid_values() {
        // The CHECK constraint guards against typos in clients writing
        // directly to the table without going through `state::process_create`.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch("INSERT INTO projects (id, root) VALUES (1, '/x');")
            .unwrap();
        conn.execute_batch(
            "INSERT INTO processes (project_id, id, kind, name, created_at)
                 VALUES (1, 1, 'bogus', 'x', '');",
        )
        .unwrap_err();
    }
}
