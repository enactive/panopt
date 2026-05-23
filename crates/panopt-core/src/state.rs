//! [`Store`] - PANopt's coordination state: persistent todos and scratchpads,
//! plus the in-memory registry of connected agents and their advisory locks.
//!
//! Todos and scratchpads live in a single SQLite database, scoped by a
//! `project_id`. The agent registry and the lock table are in-memory only -
//! they track *currently connected* agents and the locks they hold, which a
//! daemon restart correctly forgets. Every mutating method commits its database
//! transaction (where it has one) and then re-projects the affected `.panopt/`
//! file, so the state and the projected files can never drift: there is no code
//! path that mutates without projecting.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};

use crate::db;
use crate::error::CoreError;
use crate::locks::Locks;
use crate::model::{
    Agent, Lock, Priority, ProjectId, RosterEntry, RosterKind, RosterPatch, Scratchpad, Todo,
    TodoComment, TodoPatch, TodoStatus,
};
use crate::projection;
use crate::registry::Registry;

/// How long an agent may go without any tool call before the registry treats
/// it as gone and prunes it (releasing its locks).
const AGENT_MAX_IDLE: Duration = Duration::from_secs(300);

/// All of PANopt's coordination state.
///
/// The daemon wraps one `Store` in a `Mutex`, so a `&mut self` method is the
/// natural unit of serialization: a database transaction and the file
/// re-projection that follows it complete with no other writer interleaving.
pub struct Store {
    conn: Connection,
    /// In-memory roster of connected agents. Not persisted - see [`Registry`].
    registry: Registry,
    /// In-memory advisory locks. Not persisted - see [`Locks`].
    locks: Locks,
    /// Projects whose `.panopt/` files have already been re-projected once in
    /// this process. The first touch of a project re-projects every file from
    /// the database - initializing a new project and self-healing a restarted
    /// one whose last projection may have been lost to a crash. Later touches
    /// skip that and re-project only what they change.
    reprojected: HashSet<i64>,
}

impl Store {
    /// Open (creating if absent) the SQLite database at `db_path` and migrate
    /// it to the current schema.
    pub fn open(db_path: &Path) -> Result<Self, CoreError> {
        let conn = Connection::open(db_path)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        db::migrate(&conn)?;
        Ok(Self {
            conn,
            registry: Registry::default(),
            locks: Locks::default(),
            reprojected: HashSet::new(),
        })
    }

    /// Resolve the project rooted at `root`, creating its row on first sight.
    ///
    /// `root` must exist; it is canonicalized, so symlinks and trailing
    /// slashes collapse onto one project. The first call for a project in this
    /// process bootstraps its `.panopt/` tree and re-projects every file from
    /// current state.
    pub fn ensure_project(&mut self, root: &Path) -> Result<ProjectId, CoreError> {
        let canonical = std::fs::canonicalize(root)
            .map_err(|_| CoreError::Workspace(root.to_path_buf()))?;
        let root_str = canonical.to_string_lossy().into_owned();

        let existing: Option<i64> = self
            .conn
            .query_row("SELECT id FROM projects WHERE root = ?1", [&root_str], |r| r.get(0))
            .optional()?;
        let id = match existing {
            Some(id) => id,
            None => {
                self.conn
                    .execute("INSERT INTO projects (root) VALUES (?1)", [&root_str])?;
                self.conn.last_insert_rowid()
            }
        };

        let project = ProjectId(id);
        if self.reprojected.insert(id) {
            projection::bootstrap(&canonical)?;
            self.reproject_all(&canonical, project)?;
        }
        Ok(project)
    }

    // --- agents ---

    /// Record activity from agent `key` in `project`, registering it on first
    /// sight, and prune any agents that have gone silent. Re-projects whatever
    /// changed.
    pub fn agent_touch(&mut self, project: ProjectId, key: &str) -> Result<(), CoreError> {
        let added = self.registry.touch(project.0, key);
        let pruned_any = self.prune_agents(project)?;
        // `prune_agents` re-projects the roster when it prunes; if it did not
        // prune but this call added a new agent, the roster still changed.
        if added && !pruned_any {
            self.reproject_agents(project)?;
        }
        Ok(())
    }

    /// Set agent `key`'s name and, if given, its self-reported status.
    pub fn agent_identify(
        &mut self,
        project: ProjectId,
        key: &str,
        name: String,
        status: Option<String>,
    ) -> Result<(), CoreError> {
        self.registry.identify(project.0, key, name, status);
        self.reproject_agents(project)
    }

    /// The registry entry for agent `key`, if it is registered in `project`.
    pub fn agent_whoami(&self, project: ProjectId, key: &str) -> Option<Agent> {
        self.registry.get(project.0, key)
    }

    /// Every agent registered in `project`, after pruning silent ones.
    pub fn agent_list(&mut self, project: ProjectId) -> Result<Vec<Agent>, CoreError> {
        self.prune_agents(project)?;
        Ok(self.registry.list(project.0))
    }

    /// Total number of agents currently connected, across every project. The
    /// daemon's SIGTERM guard uses this to decide whether shutting down
    /// would drop live MCP clients. Does not prune - the caller is the
    /// signal handler and a stale count is preferable to running prune
    /// logic in the signal path.
    pub fn connected_agent_count(&self) -> usize {
        self.registry.total()
    }

    /// `(project_id, agent_count)` for every project with at least one
    /// connected agent. The daemon logs this on SIGTERM so the operator can
    /// see what a second SIGTERM would drop.
    pub fn connected_agents_by_project(&self) -> Vec<(ProjectId, usize)> {
        self.registry
            .counts_by_project()
            .into_iter()
            .map(|(pid, n)| (ProjectId(pid), n))
            .collect()
    }

    /// Prune silent agents across *every* project, release their locks, and
    /// re-project what changed. Returns the keys removed.
    ///
    /// The daemon calls this on a timer so a closed agent leaves the roster
    /// even when no other agent is active to trigger a prune.
    pub fn sweep_idle_agents(&mut self) -> Result<Vec<String>, CoreError> {
        let pruned = self.registry.prune_all(AGENT_MAX_IDLE);
        if pruned.is_empty() {
            return Ok(Vec::new());
        }
        let mut affected: HashSet<i64> = HashSet::new();
        for (pid, key) in &pruned {
            self.locks.release_all(*pid, key);
            affected.insert(*pid);
        }
        for pid in affected {
            let project = ProjectId(pid);
            self.reproject_agents(project)?;
            self.reproject_locks(project)?;
        }
        Ok(pruned.into_iter().map(|(_, key)| key).collect())
    }

    /// Prune agents in `project` that have gone silent, release any locks they
    /// held, and re-project whatever changed. Returns whether anything was
    /// pruned.
    fn prune_agents(&mut self, project: ProjectId) -> Result<bool, CoreError> {
        let pruned = self.registry.prune(project.0, AGENT_MAX_IDLE);
        if pruned.is_empty() {
            return Ok(false);
        }
        let mut released = 0;
        for gone in &pruned {
            released += self.locks.release_all(project.0, gone);
        }
        self.reproject_agents(project)?;
        if released > 0 {
            self.reproject_locks(project)?;
        }
        Ok(true)
    }

    // --- locks ---

    /// Acquire the advisory lock `name` in `project` for agent `key`.
    ///
    /// Non-blocking. Returns `None` if the caller now holds the lock, or
    /// `Some(holder_name)` if another agent holds it. Re-acquiring a lock you
    /// already hold succeeds and updates its note when one is given.
    pub fn lock_acquire(
        &mut self,
        project: ProjectId,
        key: &str,
        name: String,
        note: Option<String>,
    ) -> Result<Option<String>, CoreError> {
        match self.locks.acquire(project.0, key, name, note) {
            None => {
                self.reproject_locks(project)?;
                Ok(None)
            }
            Some(holder_key) => Ok(Some(self.resolve_agent_name(project, &holder_key))),
        }
    }

    /// Release the advisory lock `name` in `project` on behalf of agent `key`.
    ///
    /// Returns `None` if the lock is now free, or `Some(holder_name)` if
    /// another agent holds it (and it was left untouched).
    pub fn lock_release(
        &mut self,
        project: ProjectId,
        key: &str,
        name: &str,
    ) -> Result<Option<String>, CoreError> {
        match self.locks.release(project.0, key, name) {
            None => {
                self.reproject_locks(project)?;
                Ok(None)
            }
            Some(holder_key) => Ok(Some(self.resolve_agent_name(project, &holder_key))),
        }
    }

    /// Every advisory lock held in `project`, holder names resolved.
    pub fn lock_list(&self, project: ProjectId) -> Vec<Lock> {
        let mut locks = self.locks.list(project.0);
        for lock in &mut locks {
            lock.holder_name = self.resolve_agent_name(project, &lock.holder_key);
        }
        locks
    }

    /// The display name for an agent key: its registered name, or the key
    /// itself if it is not (or no longer) in the registry.
    fn resolve_agent_name(&self, project: ProjectId, key: &str) -> String {
        self.registry
            .get(project.0, key)
            .map(|a| a.name)
            .unwrap_or_else(|| key.to_string())
    }

    // --- scratchpads ---

    /// Create a new, empty scratchpad in `project` and return its id.
    pub fn scratchpad_create(
        &mut self,
        project: ProjectId,
        title: String,
    ) -> Result<u64, CoreError> {
        let pid = project.0;
        let id = {
            let tx = self.conn.transaction()?;
            let next = next_id(&tx, pid, "next_scratchpad_id")?;
            tx.execute(
                "INSERT INTO scratchpads (project_id, id, title, body, created_at, updated_at)
                 VALUES (?1, ?2, ?3, '', datetime('now'), datetime('now'))",
                params![pid, next, title],
            )?;
            tx.execute(
                "UPDATE projects SET next_scratchpad_id = ?1 WHERE id = ?2",
                params![next + 1, pid],
            )?;
            tx.commit()?;
            next as u64
        };
        self.reproject_scratchpad_full(project, id)?;
        Ok(id)
    }

    /// List a project's scratchpads as `(id, title)` pairs, id-ascending.
    pub fn scratchpad_list(&self, project: ProjectId) -> Result<Vec<(u64, String)>, CoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, title FROM scratchpads WHERE project_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map([project.0], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Append `content` to a scratchpad, separating it from existing content
    /// with a single newline.
    pub fn scratchpad_append(
        &mut self,
        project: ProjectId,
        id: u64,
        content: &str,
    ) -> Result<(), CoreError> {
        let mut body = self.scratchpad_body(project, id)?;
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(content);
        self.conn.execute(
            "UPDATE scratchpads
                SET body = ?1, updated_at = datetime('now')
              WHERE project_id = ?2 AND id = ?3",
            params![body, project.0, id as i64],
        )?;
        self.reproject_scratchpad_full(project, id)
    }

    /// Read the full body of a scratchpad.
    pub fn scratchpad_read(&self, project: ProjectId, id: u64) -> Result<String, CoreError> {
        self.scratchpad_body(project, id)
    }

    fn scratchpad_body(&self, project: ProjectId, id: u64) -> Result<String, CoreError> {
        self.conn
            .query_row(
                "SELECT body FROM scratchpads WHERE project_id = ?1 AND id = ?2",
                params![project.0, id as i64],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(CoreError::ScratchpadNotFound(id))
    }

    fn fetch_scratchpad(&self, project: ProjectId, id: u64) -> Result<Scratchpad, CoreError> {
        self.conn
            .query_row(
                "SELECT title, body, created_at, updated_at FROM scratchpads
                  WHERE project_id = ?1 AND id = ?2",
                params![project.0, id as i64],
                |r| {
                    Ok(Scratchpad {
                        id,
                        title: r.get(0)?,
                        body: r.get(1)?,
                        created_at: r.get(2)?,
                        updated_at: r.get(3)?,
                    })
                },
            )
            .optional()?
            .ok_or(CoreError::ScratchpadNotFound(id))
    }

    // --- roster ---

    /// Create a roster entry in `project` and return its id. `position` is set
    /// to the new id so entries default to creation order yet stay reorderable.
    pub fn roster_create(
        &mut self,
        project: ProjectId,
        kind: RosterKind,
        name: String,
        display_name: String,
        command: String,
        cwd: String,
    ) -> Result<u64, CoreError> {
        let pid = project.0;
        let id = {
            let tx = self.conn.transaction()?;
            let next = next_id(&tx, pid, "next_roster_id")?;
            tx.execute(
                "INSERT INTO roster
                    (project_id, id, kind, name, display_name, command, cwd, position, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now'))",
                params![pid, next, kind.as_str(), name, display_name, command, cwd, next],
            )?;
            tx.execute(
                "UPDATE projects SET next_roster_id = ?1 WHERE id = ?2",
                params![next + 1, pid],
            )?;
            tx.commit()?;
            next as u64
        };
        self.reproject_roster(project)?;
        Ok(id)
    }

    /// List a project's roster entries, ordered by `position` then `id`.
    pub fn roster_list(&self, project: ProjectId) -> Result<Vec<RosterEntry>, CoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, display_name, command, cwd, position, created_at
               FROM roster WHERE project_id = ?1 ORDER BY position, id",
        )?;
        let rows = stmt.query_map([project.0], |r| {
            let kind: String = r.get(1)?;
            Ok(RosterEntry {
                id: r.get::<_, i64>(0)? as u64,
                kind: RosterKind::parse(&kind).unwrap_or_default(),
                name: r.get(2)?,
                display_name: r.get(3)?,
                command: r.get(4)?,
                cwd: r.get(5)?,
                position: r.get(6)?,
                created_at: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Fetch one roster entry, or [`CoreError::RosterNotFound`] if it is absent.
    pub fn roster_get(&self, project: ProjectId, id: u64) -> Result<RosterEntry, CoreError> {
        self.fetch_roster(project, id)
    }

    /// Apply a [`RosterPatch`]: every `Some` field is written, every `None`
    /// field is left as-is.
    pub fn roster_update(
        &mut self,
        project: ProjectId,
        id: u64,
        patch: RosterPatch,
    ) -> Result<(), CoreError> {
        let mut entry = self.fetch_roster(project, id)?;
        if let Some(v) = patch.name {
            entry.name = v;
        }
        if let Some(v) = patch.display_name {
            entry.display_name = v;
        }
        if let Some(v) = patch.command {
            entry.command = v;
        }
        if let Some(v) = patch.cwd {
            entry.cwd = v;
        }
        if let Some(v) = patch.position {
            entry.position = v;
        }
        self.conn.execute(
            "UPDATE roster
                SET name = ?1, display_name = ?2, command = ?3, cwd = ?4, position = ?5
              WHERE project_id = ?6 AND id = ?7",
            params![
                entry.name,
                entry.display_name,
                entry.command,
                entry.cwd,
                entry.position,
                project.0,
                id as i64,
            ],
        )?;
        self.reproject_roster(project)
    }

    /// Delete a roster entry.
    pub fn roster_delete(&mut self, project: ProjectId, id: u64) -> Result<(), CoreError> {
        let changed = self.conn.execute(
            "DELETE FROM roster WHERE project_id = ?1 AND id = ?2",
            params![project.0, id as i64],
        )?;
        if changed == 0 {
            return Err(CoreError::RosterNotFound(id));
        }
        self.reproject_roster(project)
    }

    /// Fetch one roster entry by id within `project`.
    fn fetch_roster(&self, project: ProjectId, id: u64) -> Result<RosterEntry, CoreError> {
        self.conn
            .query_row(
                "SELECT kind, name, display_name, command, cwd, position, created_at
                   FROM roster WHERE project_id = ?1 AND id = ?2",
                params![project.0, id as i64],
                |r| {
                    let kind: String = r.get(0)?;
                    Ok(RosterEntry {
                        id,
                        kind: RosterKind::parse(&kind).unwrap_or_default(),
                        name: r.get(1)?,
                        display_name: r.get(2)?,
                        command: r.get(3)?,
                        cwd: r.get(4)?,
                        position: r.get(5)?,
                        created_at: r.get(6)?,
                    })
                },
            )
            .optional()?
            .ok_or(CoreError::RosterNotFound(id))
    }

    // --- todos ---

    /// Create a new open todo in `project` and return its id.
    pub fn todo_create(&mut self, project: ProjectId, title: String) -> Result<u64, CoreError> {
        let pid = project.0;
        let id = {
            let tx = self.conn.transaction()?;
            let next = next_id(&tx, pid, "next_todo_id")?;
            tx.execute(
                "INSERT INTO todos (project_id, id, title, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'open', datetime('now'), datetime('now'))",
                params![pid, next, title],
            )?;
            tx.execute(
                "UPDATE projects SET next_todo_id = ?1 WHERE id = ?2",
                params![next + 1, pid],
            )?;
            tx.commit()?;
            next as u64
        };
        self.reproject_todos(project)?;
        Ok(id)
    }

    /// List a project's todos in full - blockers and comments included -
    /// id-ascending.
    pub fn todo_list(&self, project: ProjectId) -> Result<Vec<Todo>, CoreError> {
        let ids: Vec<u64> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM todos WHERE project_id = ?1 ORDER BY id")?;
            let rows = stmt.query_map([project.0], |r| Ok(r.get::<_, i64>(0)? as u64))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        ids.into_iter().map(|id| self.fetch_todo(project, id)).collect()
    }

    /// Fetch one todo in full, or [`CoreError::TodoNotFound`] if it is absent.
    pub fn todo_get(&self, project: ProjectId, id: u64) -> Result<Todo, CoreError> {
        self.fetch_todo(project, id)
    }

    /// Apply a [`TodoPatch`]: every `Some` field is written, every `None` field
    /// is left as-is. `updated_at` is always bumped, and `completed_at` is
    /// reconciled with the resulting status.
    pub fn todo_update(
        &mut self,
        project: ProjectId,
        id: u64,
        patch: TodoPatch,
    ) -> Result<(), CoreError> {
        let mut todo = self.fetch_todo(project, id)?;
        if let Some(v) = patch.title {
            todo.title = v;
        }
        if let Some(v) = patch.body {
            todo.body = v;
        }
        if let Some(v) = patch.status {
            todo.status = v;
        }
        if let Some(v) = patch.priority {
            todo.priority = v;
        }
        if let Some(v) = patch.assignee {
            todo.assignee = v;
        }
        if let Some(v) = patch.tags {
            todo.tags = v;
        }
        let tags_json = serde_json::to_string(&todo.tags).unwrap_or_else(|_| "[]".into());
        self.conn.execute(
            "UPDATE todos
                SET title = ?1, body = ?2, status = ?3, priority = ?4,
                    assignee = ?5, tags = ?6, updated_at = datetime('now')
              WHERE project_id = ?7 AND id = ?8",
            params![
                todo.title,
                todo.body,
                todo.status.as_str(),
                todo.priority.as_str(),
                todo.assignee,
                tags_json,
                project.0,
                id as i64,
            ],
        )?;
        self.reconcile_completed_at(project, id, todo.status)?;
        self.reproject_todos(project)
    }

    /// Mark a todo complete. Idempotent: completing an already-done todo
    /// succeeds and re-projects.
    pub fn todo_complete(&mut self, project: ProjectId, id: u64) -> Result<(), CoreError> {
        let changed = self.conn.execute(
            "UPDATE todos SET status = 'completed', updated_at = datetime('now')
              WHERE project_id = ?1 AND id = ?2",
            params![project.0, id as i64],
        )?;
        if changed == 0 {
            return Err(CoreError::TodoNotFound(id));
        }
        self.reconcile_completed_at(project, id, TodoStatus::Completed)?;
        self.reproject_todos(project)
    }

    /// Delete a todo and, by foreign-key cascade, its comments and every
    /// blocker row in which it appears (as the blocked todo or the blocker).
    pub fn todo_delete(&mut self, project: ProjectId, id: u64) -> Result<(), CoreError> {
        let changed = self.conn.execute(
            "DELETE FROM todos WHERE project_id = ?1 AND id = ?2",
            params![project.0, id as i64],
        )?;
        if changed == 0 {
            return Err(CoreError::TodoNotFound(id));
        }
        self.reproject_todos(project)
    }

    /// Record that todo `id` is blocked by `blocker_id`. Both todos must exist
    /// and be distinct. Idempotent: an already-recorded blocker is left as-is.
    pub fn todo_add_blocker(
        &mut self,
        project: ProjectId,
        id: u64,
        blocker_id: u64,
    ) -> Result<(), CoreError> {
        if id == blocker_id {
            return Err(CoreError::BadRequest("a todo cannot block itself".into()));
        }
        self.fetch_todo(project, id)?;
        self.fetch_todo(project, blocker_id)?;
        self.conn.execute(
            "INSERT OR IGNORE INTO todo_blockers (project_id, todo_id, blocker_id)
             VALUES (?1, ?2, ?3)",
            params![project.0, id as i64, blocker_id as i64],
        )?;
        self.touch_todo(project, id)?;
        self.reproject_todos(project)
    }

    /// Remove the record that todo `id` is blocked by `blocker_id`. Idempotent:
    /// a blocker that was not recorded is a no-op. The blocked todo must exist.
    pub fn todo_remove_blocker(
        &mut self,
        project: ProjectId,
        id: u64,
        blocker_id: u64,
    ) -> Result<(), CoreError> {
        self.fetch_todo(project, id)?;
        self.conn.execute(
            "DELETE FROM todo_blockers
              WHERE project_id = ?1 AND todo_id = ?2 AND blocker_id = ?3",
            params![project.0, id as i64, blocker_id as i64],
        )?;
        self.touch_todo(project, id)?;
        self.reproject_todos(project)
    }

    /// Append a comment to a todo and return the new comment's id (unique
    /// within that todo, restarting at 1 in each todo).
    pub fn todo_comment_add(
        &mut self,
        project: ProjectId,
        id: u64,
        author: String,
        body: String,
    ) -> Result<u64, CoreError> {
        let pid = project.0;
        let comment_id = {
            let tx = self.conn.transaction()?;
            let next: Option<i64> = tx
                .query_row(
                    "SELECT next_comment_id FROM todos WHERE project_id = ?1 AND id = ?2",
                    params![pid, id as i64],
                    |r| r.get(0),
                )
                .optional()?;
            let next = next.ok_or(CoreError::TodoNotFound(id))?;
            tx.execute(
                "INSERT INTO todo_comments (project_id, todo_id, id, author, body, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
                params![pid, id as i64, next, author, body],
            )?;
            tx.execute(
                "UPDATE todos SET next_comment_id = ?1, updated_at = datetime('now')
                  WHERE project_id = ?2 AND id = ?3",
                params![next + 1, pid, id as i64],
            )?;
            tx.commit()?;
            next as u64
        };
        self.reproject_todos(project)?;
        Ok(comment_id)
    }

    /// Replace the body of an existing comment. The author and `created_at`
    /// timestamp are preserved - a comment edit is not a re-post.
    pub fn todo_comment_update(
        &mut self,
        project: ProjectId,
        todo_id: u64,
        comment_id: u64,
        body: String,
    ) -> Result<(), CoreError> {
        let changed = self.conn.execute(
            "UPDATE todo_comments SET body = ?1
              WHERE project_id = ?2 AND todo_id = ?3 AND id = ?4",
            params![body, project.0, todo_id as i64, comment_id as i64],
        )?;
        if changed == 0 {
            // The comment row is missing; surface whichever id is at fault.
            self.fetch_todo(project, todo_id)?;
            return Err(CoreError::TodoCommentNotFound { todo_id, comment_id });
        }
        self.touch_todo(project, todo_id)?;
        self.reproject_todos(project)
    }

    /// Remove a comment from a todo. The comment id is **not** reused - the
    /// per-todo `next_comment_id` counter keeps advancing, so a later
    /// `todo_comment_add` lands at the next fresh id.
    pub fn todo_comment_delete(
        &mut self,
        project: ProjectId,
        todo_id: u64,
        comment_id: u64,
    ) -> Result<(), CoreError> {
        let changed = self.conn.execute(
            "DELETE FROM todo_comments
              WHERE project_id = ?1 AND todo_id = ?2 AND id = ?3",
            params![project.0, todo_id as i64, comment_id as i64],
        )?;
        if changed == 0 {
            self.fetch_todo(project, todo_id)?;
            return Err(CoreError::TodoCommentNotFound { todo_id, comment_id });
        }
        self.touch_todo(project, todo_id)?;
        self.reproject_todos(project)
    }

    /// Replace a todo's blocker set with `blockers` in one transactional step.
    /// Convenience over the per-id `add`/`remove` calls, which the form uses
    /// during debounced autosave to avoid a half-applied state.
    pub fn todo_set_blockers(
        &mut self,
        project: ProjectId,
        id: u64,
        blockers: Vec<u64>,
    ) -> Result<(), CoreError> {
        // Reject self-blocking up front so the diff loop does not silently skip it.
        if blockers.iter().any(|b| *b == id) {
            return Err(CoreError::BadRequest("a todo cannot block itself".into()));
        }
        // Make sure the blocked todo and every requested blocker exists before
        // mutating anything; otherwise a half-applied set leaks on error.
        self.fetch_todo(project, id)?;
        for b in &blockers {
            self.fetch_todo(project, *b)?;
        }
        let desired: HashSet<u64> = blockers.into_iter().collect();
        let current: HashSet<u64> = self.todo_blockers(project, id)?.into_iter().collect();
        for &remove in current.difference(&desired) {
            self.conn.execute(
                "DELETE FROM todo_blockers
                  WHERE project_id = ?1 AND todo_id = ?2 AND blocker_id = ?3",
                params![project.0, id as i64, remove as i64],
            )?;
        }
        for &add in desired.difference(&current) {
            self.conn.execute(
                "INSERT OR IGNORE INTO todo_blockers (project_id, todo_id, blocker_id)
                 VALUES (?1, ?2, ?3)",
                params![project.0, id as i64, add as i64],
            )?;
        }
        self.touch_todo(project, id)?;
        self.reproject_todos(project)
    }

    /// The sorted, deduped union of every tag used by any todo in `project`.
    /// Cheap to recompute on demand; the form uses it for tag autocomplete.
    pub fn todo_tags_list(&self, project: ProjectId) -> Result<Vec<String>, CoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT tags FROM todos WHERE project_id = ?1")?;
        let rows = stmt.query_map([project.0], |r| r.get::<_, String>(0))?;
        let mut set: HashSet<String> = HashSet::new();
        for row in rows {
            let json = row?;
            // A bad tag JSON blob is treated as empty rather than aborting the
            // whole list - the form should not bomb out on a stray write.
            let tags: Vec<String> = serde_json::from_str(&json).unwrap_or_default();
            for t in tags {
                if !t.is_empty() {
                    set.insert(t);
                }
            }
        }
        let mut tags: Vec<String> = set.into_iter().collect();
        tags.sort();
        Ok(tags)
    }

    /// Set or clear `completed_at` to match `status`: a `Completed` todo keeps
    /// any existing timestamp or gets one now; any other status clears it.
    fn reconcile_completed_at(
        &self,
        project: ProjectId,
        id: u64,
        status: TodoStatus,
    ) -> Result<(), CoreError> {
        let sql = if status == TodoStatus::Completed {
            "UPDATE todos SET completed_at = COALESCE(completed_at, datetime('now'))
              WHERE project_id = ?1 AND id = ?2"
        } else {
            "UPDATE todos SET completed_at = NULL WHERE project_id = ?1 AND id = ?2"
        };
        self.conn.execute(sql, params![project.0, id as i64])?;
        Ok(())
    }

    /// Bump a todo's `updated_at` to now. Used by mutations of a todo's side
    /// tables, which do not otherwise touch the `todos` row.
    fn touch_todo(&self, project: ProjectId, id: u64) -> Result<(), CoreError> {
        self.conn.execute(
            "UPDATE todos SET updated_at = datetime('now') WHERE project_id = ?1 AND id = ?2",
            params![project.0, id as i64],
        )?;
        Ok(())
    }

    /// Fetch one todo with its blockers and comments, or
    /// [`CoreError::TodoNotFound`] when no such todo exists in `project`.
    fn fetch_todo(&self, project: ProjectId, id: u64) -> Result<Todo, CoreError> {
        let mut todo = self
            .conn
            .query_row(
                "SELECT title, body, status, priority, assignee, tags,
                        created_at, updated_at, completed_at
                   FROM todos WHERE project_id = ?1 AND id = ?2",
                params![project.0, id as i64],
                |r| {
                    let status: String = r.get(2)?;
                    let priority: String = r.get(3)?;
                    let tags: String = r.get(5)?;
                    Ok(Todo {
                        id,
                        title: r.get(0)?,
                        body: r.get(1)?,
                        status: TodoStatus::parse(&status).unwrap_or(TodoStatus::Open),
                        priority: Priority::parse(&priority).unwrap_or(Priority::Medium),
                        assignee: r.get(4)?,
                        tags: serde_json::from_str(&tags).unwrap_or_default(),
                        blockers: Vec::new(),
                        comments: Vec::new(),
                        created_at: r.get(6)?,
                        updated_at: r.get(7)?,
                        completed_at: r.get(8)?,
                    })
                },
            )
            .optional()?
            .ok_or(CoreError::TodoNotFound(id))?;
        todo.blockers = self.todo_blockers(project, id)?;
        todo.comments = self.todo_comments(project, id)?;
        Ok(todo)
    }

    /// The ids that block todo `id`, ascending.
    fn todo_blockers(&self, project: ProjectId, id: u64) -> Result<Vec<u64>, CoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT blocker_id FROM todo_blockers
              WHERE project_id = ?1 AND todo_id = ?2 ORDER BY blocker_id",
        )?;
        let rows = stmt
            .query_map(params![project.0, id as i64], |r| Ok(r.get::<_, i64>(0)? as u64))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The comments on todo `id`, in post order.
    fn todo_comments(&self, project: ProjectId, id: u64) -> Result<Vec<TodoComment>, CoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, author, body, created_at FROM todo_comments
              WHERE project_id = ?1 AND todo_id = ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![project.0, id as i64], |r| {
            Ok(TodoComment {
                id: r.get::<_, i64>(0)? as u64,
                author: r.get(1)?,
                body: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    // --- projection ---

    fn project_root(&self, project: ProjectId) -> Result<PathBuf, CoreError> {
        let root: String = self
            .conn
            .query_row("SELECT root FROM projects WHERE id = ?1", [project.0], |r| r.get(0))
            .optional()?
            .ok_or(CoreError::ProjectNotFound(project.0))?;
        Ok(PathBuf::from(root))
    }

    fn reproject_todos(&self, project: ProjectId) -> Result<(), CoreError> {
        let root = self.project_root(project)?;
        projection::project_todos(&root, &self.todo_list(project)?)?;
        Ok(())
    }

    fn reproject_scratchpad(&self, project: ProjectId, id: u64) -> Result<(), CoreError> {
        let root = self.project_root(project)?;
        projection::project_scratchpad(&root, &self.fetch_scratchpad(project, id)?)?;
        Ok(())
    }

    fn reproject_scratchpads_index(&self, project: ProjectId) -> Result<(), CoreError> {
        let root = self.project_root(project)?;
        projection::project_scratchpads_index(&root, &self.scratchpad_index_rows(project)?)?;
        Ok(())
    }

    /// Rewrite both the per-scratchpad file and the scratchpad index, so every
    /// mutation that touches a scratchpad refreshes the projection completely.
    /// The single helper makes the index step impossible for a caller to skip,
    /// matching the all-or-nothing shape `reproject_todos` already has.
    fn reproject_scratchpad_full(
        &self,
        project: ProjectId,
        id: u64,
    ) -> Result<(), CoreError> {
        self.reproject_scratchpad(project, id)?;
        self.reproject_scratchpads_index(project)
    }

    /// `(id, title, updated_at)` for every scratchpad in `project`, id-ascending.
    /// The projection layer renders the `updated_at` into each index line; the
    /// public `scratchpad_list` stays the narrow `(id, title)` shape the MCP
    /// wire summary expects.
    fn scratchpad_index_rows(
        &self,
        project: ProjectId,
    ) -> Result<Vec<(u64, String, String)>, CoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, updated_at FROM scratchpads
              WHERE project_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([project.0], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn reproject_roster(&self, project: ProjectId) -> Result<(), CoreError> {
        let root = self.project_root(project)?;
        projection::project_roster(&root, &self.roster_list(project)?)?;
        Ok(())
    }

    fn reproject_agents(&self, project: ProjectId) -> Result<(), CoreError> {
        let root = self.project_root(project)?;
        projection::project_agents(&root, &self.registry.list(project.0))?;
        Ok(())
    }

    fn reproject_locks(&self, project: ProjectId) -> Result<(), CoreError> {
        let root = self.project_root(project)?;
        projection::project_locks(&root, &self.lock_list(project))?;
        Ok(())
    }

    /// Re-project every file of a project from current state. Run once per
    /// project per process by [`Self::ensure_project`].
    fn reproject_all(&self, root: &Path, project: ProjectId) -> Result<(), CoreError> {
        projection::project_todos(root, &self.todo_list(project)?)?;
        projection::project_roster(root, &self.roster_list(project)?)?;
        projection::project_agents(root, &self.registry.list(project.0))?;
        projection::project_locks(root, &self.lock_list(project))?;
        projection::project_scratchpads_index(root, &self.scratchpad_index_rows(project)?)?;
        for (id, _) in self.scratchpad_list(project)? {
            projection::project_scratchpad(root, &self.fetch_scratchpad(project, id)?)?;
        }
        Ok(())
    }
}

/// Read a project's next-id counter, mapping a missing project row to
/// [`CoreError::ProjectNotFound`]. `column` is a fixed in-crate identifier, not
/// caller input, so interpolating it into the SQL is safe.
fn next_id(conn: &Connection, pid: i64, column: &str) -> Result<i64, CoreError> {
    conn.query_row(
        &format!("SELECT {column} FROM projects WHERE id = ?1"),
        [pid],
        |r| r.get(0),
    )
    .optional()?
    .ok_or(CoreError::ProjectNotFound(pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Store` on a throwaway database, plus the temp dir its projects and
    /// database file live in. Field order matters: `store` drops (closing the
    /// connection) before `dir` drops (deleting the files).
    struct Fixture {
        store: Store,
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(&dir.path().join("panopt.db")).unwrap();
            Fixture { store, dir }
        }

        /// Create a project directory under the temp dir and register it.
        fn project(&mut self, name: &str) -> (ProjectId, PathBuf) {
            let root = self.dir.path().join(name);
            std::fs::create_dir_all(&root).unwrap();
            let id = self.store.ensure_project(&root).unwrap();
            (id, root)
        }
    }

    #[test]
    fn todo_ids_are_monotonic_within_a_project() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        assert_eq!(fx.store.todo_create(p, "a".into()).unwrap(), 1);
        assert_eq!(fx.store.todo_create(p, "b".into()).unwrap(), 2);
        assert_eq!(fx.store.todo_create(p, "c".into()).unwrap(), 3);
    }

    #[test]
    fn scratchpad_ids_are_independent_of_todos() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        fx.store.todo_create(p, "a".into()).unwrap();
        assert_eq!(fx.store.scratchpad_create(p, "pad".into()).unwrap(), 1);
        assert_eq!(fx.store.scratchpad_create(p, "pad2".into()).unwrap(), 2);
    }

    #[test]
    fn append_concatenates_with_single_newline() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.scratchpad_create(p, "notes".into()).unwrap();
        fx.store.scratchpad_append(p, id, "first").unwrap();
        fx.store.scratchpad_append(p, id, "second").unwrap();
        assert_eq!(fx.store.scratchpad_read(p, id).unwrap(), "first\nsecond");
    }

    #[test]
    fn scratchpad_create_sets_created_and_updated_timestamps() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.scratchpad_create(p, "notes".into()).unwrap();
        let pad = fx.store.fetch_scratchpad(p, id).unwrap();
        assert!(!pad.created_at.is_empty(), "created_at is set on create");
        assert!(!pad.updated_at.is_empty(), "updated_at is set on create");
    }

    #[test]
    fn scratchpad_append_bumps_updated_at_and_rewrites_index() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        let id = fx.store.scratchpad_create(p, "notes".into()).unwrap();
        let before = fx.store.fetch_scratchpad(p, id).unwrap().updated_at;

        // datetime('now') has 1-second resolution, so cross a second boundary
        // to be sure the timestamp moves; the same constraint todos face.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fx.store.scratchpad_append(p, id, "more").unwrap();

        let after = fx.store.fetch_scratchpad(p, id).unwrap().updated_at;
        assert!(after > before, "updated_at must advance on append");

        // And the index file now carries the new timestamp - the bytes change
        // so the cockpit's 1s file poller observes the refresh.
        let index = std::fs::read_to_string(root.join(".panopt/scratchpads.md")).unwrap();
        assert!(
            index.contains(&format!("updated {after}")),
            "index reflects the bumped updated_at\n{index}",
        );
    }

    #[test]
    fn scratchpad_append_rewrites_the_index_even_after_deletion() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        let id = fx.store.scratchpad_create(p, "notes".into()).unwrap();
        // Remove the index to prove `scratchpad_append` rewrites it - this
        // is what guards against the pre-fix asymmetry where append skipped
        // the index reprojection entirely.
        std::fs::remove_file(root.join(".panopt/scratchpads.md")).unwrap();
        fx.store.scratchpad_append(p, id, "more").unwrap();
        assert!(root.join(".panopt/scratchpads.md").exists());
    }

    #[test]
    fn complete_flips_status_and_is_idempotent() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.todo_create(p, "task".into()).unwrap();
        assert_eq!(fx.store.todo_list(p).unwrap()[0].status, TodoStatus::Open);
        fx.store.todo_complete(p, id).unwrap();
        let done = fx.store.todo_get(p, id).unwrap();
        assert_eq!(done.status, TodoStatus::Completed);
        assert!(done.completed_at.is_some());
        fx.store.todo_complete(p, id).unwrap(); // idempotent
        assert_eq!(fx.store.todo_get(p, id).unwrap().status, TodoStatus::Completed);
    }

    #[test]
    fn todo_update_writes_only_the_some_fields() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.todo_create(p, "draft".into()).unwrap();
        fx.store
            .todo_update(
                p,
                id,
                TodoPatch {
                    body: Some("the description".into()),
                    priority: Some(Priority::High),
                    tags: Some(vec!["a".into(), "b".into()]),
                    ..Default::default()
                },
            )
            .unwrap();
        let t = fx.store.todo_get(p, id).unwrap();
        assert_eq!(t.title, "draft"); // None field left untouched
        assert_eq!(t.body, "the description");
        assert_eq!(t.priority, Priority::High);
        assert_eq!(t.tags, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn completed_at_tracks_status_through_updates() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.todo_create(p, "task".into()).unwrap();
        assert!(fx.store.todo_get(p, id).unwrap().completed_at.is_none());

        let to = |s| TodoPatch { status: Some(s), ..Default::default() };
        fx.store.todo_update(p, id, to(TodoStatus::Completed)).unwrap();
        assert!(fx.store.todo_get(p, id).unwrap().completed_at.is_some());
        fx.store.todo_update(p, id, to(TodoStatus::InProgress)).unwrap();
        assert!(fx.store.todo_get(p, id).unwrap().completed_at.is_none());
    }

    #[test]
    fn blockers_record_list_and_reject_self_reference() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let a = fx.store.todo_create(p, "a".into()).unwrap();
        let b = fx.store.todo_create(p, "b".into()).unwrap();

        fx.store.todo_add_blocker(p, b, a).unwrap();
        assert_eq!(fx.store.todo_get(p, b).unwrap().blockers, vec![a]);
        // Re-adding is idempotent; removing clears it.
        fx.store.todo_add_blocker(p, b, a).unwrap();
        assert_eq!(fx.store.todo_get(p, b).unwrap().blockers, vec![a]);
        fx.store.todo_remove_blocker(p, b, a).unwrap();
        assert!(fx.store.todo_get(p, b).unwrap().blockers.is_empty());

        assert!(matches!(
            fx.store.todo_add_blocker(p, a, a),
            Err(CoreError::BadRequest(_))
        ));
        assert!(matches!(
            fx.store.todo_add_blocker(p, a, 999),
            Err(CoreError::TodoNotFound(999))
        ));
    }

    #[test]
    fn deleting_a_todo_cascades_its_side_tables() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let a = fx.store.todo_create(p, "a".into()).unwrap();
        let b = fx.store.todo_create(p, "b".into()).unwrap();
        fx.store.todo_add_blocker(p, b, a).unwrap();
        fx.store.todo_comment_add(p, a, "me".into(), "note".into()).unwrap();

        // Deleting a (the blocker) cascades away the (b blocked-by a) row.
        fx.store.todo_delete(p, a).unwrap();
        assert!(fx.store.todo_get(p, b).unwrap().blockers.is_empty());
        assert!(matches!(fx.store.todo_get(p, a), Err(CoreError::TodoNotFound(_))));
    }

    #[test]
    fn comment_update_replaces_body_and_keeps_metadata() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.todo_create(p, "task".into()).unwrap();
        let cid = fx
            .store
            .todo_comment_add(p, id, "alice".into(), "first draft".into())
            .unwrap();
        let original = fx.store.todo_get(p, id).unwrap().comments[0].clone();

        fx.store.todo_comment_update(p, id, cid, "polished".into()).unwrap();
        let after = fx.store.todo_get(p, id).unwrap().comments[0].clone();
        assert_eq!(after.body, "polished");
        assert_eq!(after.author, original.author);
        assert_eq!(after.created_at, original.created_at);
    }

    #[test]
    fn comment_delete_removes_it_and_does_not_reuse_the_id() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.todo_create(p, "task".into()).unwrap();
        let c1 = fx.store.todo_comment_add(p, id, "a".into(), "1".into()).unwrap();
        let _c2 = fx.store.todo_comment_add(p, id, "a".into(), "2".into()).unwrap();

        fx.store.todo_comment_delete(p, id, c1).unwrap();
        let comments = fx.store.todo_get(p, id).unwrap().comments;
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].body, "2");

        // The next add lands at 3, not 1: ids never recycle.
        let c3 = fx.store.todo_comment_add(p, id, "a".into(), "3".into()).unwrap();
        assert_eq!(c3, 3);
    }

    #[test]
    fn comment_update_and_delete_errors_on_missing_ids() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let id = fx.store.todo_create(p, "task".into()).unwrap();
        // No such comment on an existing todo.
        assert!(matches!(
            fx.store.todo_comment_update(p, id, 999, "x".into()),
            Err(CoreError::TodoCommentNotFound { todo_id, comment_id })
                if todo_id == id && comment_id == 999
        ));
        // No such todo at all.
        assert!(matches!(
            fx.store.todo_comment_delete(p, 999, 1),
            Err(CoreError::TodoNotFound(999))
        ));
    }

    #[test]
    fn set_blockers_diffs_against_the_current_set() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let a = fx.store.todo_create(p, "a".into()).unwrap();
        let b = fx.store.todo_create(p, "b".into()).unwrap();
        let c = fx.store.todo_create(p, "c".into()).unwrap();
        let target = fx.store.todo_create(p, "t".into()).unwrap();

        // From empty -> {a, b}.
        fx.store.todo_set_blockers(p, target, vec![a, b]).unwrap();
        assert_eq!(fx.store.todo_get(p, target).unwrap().blockers, vec![a, b]);

        // From {a, b} -> {b, c}: a removed, c added.
        fx.store.todo_set_blockers(p, target, vec![b, c]).unwrap();
        assert_eq!(fx.store.todo_get(p, target).unwrap().blockers, vec![b, c]);

        // Empty clears.
        fx.store.todo_set_blockers(p, target, vec![]).unwrap();
        assert!(fx.store.todo_get(p, target).unwrap().blockers.is_empty());

        // Self-blocking is rejected; a missing blocker errors before any write.
        assert!(matches!(
            fx.store.todo_set_blockers(p, target, vec![target]),
            Err(CoreError::BadRequest(_))
        ));
        assert!(matches!(
            fx.store.todo_set_blockers(p, target, vec![a, 999]),
            Err(CoreError::TodoNotFound(999))
        ));
        assert!(fx.store.todo_get(p, target).unwrap().blockers.is_empty());
    }

    #[test]
    fn tags_list_unions_and_sorts_across_todos() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let a = fx.store.todo_create(p, "a".into()).unwrap();
        let b = fx.store.todo_create(p, "b".into()).unwrap();
        fx.store
            .todo_update(p, a, TodoPatch { tags: Some(vec!["zeta".into(), "alpha".into()]), ..Default::default() })
            .unwrap();
        fx.store
            .todo_update(p, b, TodoPatch { tags: Some(vec!["beta".into(), "alpha".into()]), ..Default::default() })
            .unwrap();
        assert_eq!(fx.store.todo_tags_list(p).unwrap(), vec!["alpha", "beta", "zeta"]);
    }

    #[test]
    fn comment_ids_restart_in_each_todo() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        let a = fx.store.todo_create(p, "a".into()).unwrap();
        let b = fx.store.todo_create(p, "b".into()).unwrap();
        assert_eq!(fx.store.todo_comment_add(p, a, "x".into(), "1".into()).unwrap(), 1);
        assert_eq!(fx.store.todo_comment_add(p, a, "x".into(), "2".into()).unwrap(), 2);
        assert_eq!(fx.store.todo_comment_add(p, b, "y".into(), "1".into()).unwrap(), 1);
        let comments = fx.store.todo_get(p, a).unwrap().comments;
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].body, "1");
        assert_eq!(comments[1].id, 2);
    }

    #[test]
    fn missing_ids_error() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        assert!(matches!(
            fx.store.todo_complete(p, 999),
            Err(CoreError::TodoNotFound(999))
        ));
        assert!(matches!(
            fx.store.scratchpad_append(p, 999, "x"),
            Err(CoreError::ScratchpadNotFound(999))
        ));
        assert!(matches!(
            fx.store.scratchpad_read(p, 999),
            Err(CoreError::ScratchpadNotFound(999))
        ));
    }

    #[test]
    fn todo_list_is_id_ascending() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        for t in ["a", "b", "c"] {
            fx.store.todo_create(p, t.into()).unwrap();
        }
        let ids: Vec<u64> = fx.store.todo_list(p).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn projects_are_isolated() {
        let mut fx = Fixture::new();
        let (a, _) = fx.project("alpha");
        let (b, _) = fx.project("beta");

        // Ids restart at 1 in each project.
        assert_eq!(fx.store.todo_create(a, "alpha task".into()).unwrap(), 1);
        assert_eq!(fx.store.todo_create(b, "beta task".into()).unwrap(), 1);

        let alpha = fx.store.todo_list(a).unwrap();
        let beta = fx.store.todo_list(b).unwrap();
        assert_eq!(alpha.len(), 1);
        assert_eq!(beta.len(), 1);
        assert_eq!(alpha[0].title, "alpha task");
        assert_eq!(beta[0].title, "beta task");
    }

    #[test]
    fn mutations_project_to_disk() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");

        let tid = fx.store.todo_create(p, "wire up auth".into()).unwrap();
        let index = std::fs::read_to_string(root.join(".panopt/todos.md")).unwrap();
        assert!(index.contains("- [ ] [#1](todos/1.md) wire up auth"), "{index}");
        let todo_md = std::fs::read_to_string(root.join(".panopt/todos/1.md")).unwrap();
        assert!(todo_md.contains("status: open"), "{todo_md}");
        assert!(todo_md.contains("# wire up auth"), "{todo_md}");

        fx.store.todo_complete(p, tid).unwrap();
        let index = std::fs::read_to_string(root.join(".panopt/todos.md")).unwrap();
        assert!(index.contains("- [x] [#1](todos/1.md) wire up auth"), "{index}");
        let todo_md = std::fs::read_to_string(root.join(".panopt/todos/1.md")).unwrap();
        assert!(todo_md.contains("status: completed"), "{todo_md}");

        let sid = fx.store.scratchpad_create(p, "notes".into()).unwrap();
        fx.store.scratchpad_append(p, sid, "first").unwrap();
        fx.store.scratchpad_append(p, sid, "second").unwrap();
        let sp_md =
            std::fs::read_to_string(root.join(format!(".panopt/scratchpad/{sid}.md"))).unwrap();
        // Per-pad files now carry a `created`/`updated` frontmatter block;
        // the wall-clock timestamps inside are checked structurally rather
        // than by exact match.
        assert!(sp_md.starts_with("---\n"), "{sp_md}");
        assert!(sp_md.contains("created: "), "{sp_md}");
        assert!(sp_md.contains("updated: "), "{sp_md}");
        assert!(sp_md.contains("# notes\n\nfirst\nsecond\n"), "{sp_md}");
    }

    #[test]
    fn state_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("panopt.db");
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();

        {
            let mut store = Store::open(&db).unwrap();
            let p = store.ensure_project(&root).unwrap();
            store.todo_create(p, "persist me".into()).unwrap();
            store.scratchpad_create(p, "kept".into()).unwrap();
        }
        {
            let mut store = Store::open(&db).unwrap();
            let p = store.ensure_project(&root).unwrap();
            let todos = store.todo_list(p).unwrap();
            assert_eq!(todos.len(), 1);
            assert_eq!(todos[0].title, "persist me");
            // The id counter resumes past the persisted row.
            assert_eq!(store.todo_create(p, "another".into()).unwrap(), 2);
        }
    }

    #[test]
    fn agents_register_identify_and_project() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");

        // A fresh project starts with an empty roster on disk.
        let agents_md = std::fs::read_to_string(root.join(".panopt/agents.md")).unwrap();
        assert!(agents_md.contains("_(no agents connected)_"), "{agents_md}");

        fx.store.agent_touch(p, "sess-1").unwrap();
        fx.store
            .agent_identify(p, "sess-1", "backend".into(), Some("coding".into()))
            .unwrap();

        let me = fx.store.agent_whoami(p, "sess-1").unwrap();
        assert_eq!(me.name, "backend");
        assert_eq!(me.status, "coding");
        assert_eq!(fx.store.agent_list(p).unwrap().len(), 1);

        let agents_md = std::fs::read_to_string(root.join(".panopt/agents.md")).unwrap();
        assert!(agents_md.contains("- backend - coding"), "{agents_md}");
    }

    #[test]
    fn agent_rosters_are_project_isolated() {
        let mut fx = Fixture::new();
        let (a, _) = fx.project("alpha");
        let (b, _) = fx.project("beta");

        fx.store.agent_touch(a, "s1").unwrap();
        fx.store.agent_touch(b, "s2").unwrap();
        fx.store.agent_touch(b, "s3").unwrap();

        assert_eq!(fx.store.agent_list(a).unwrap().len(), 1);
        assert_eq!(fx.store.agent_list(b).unwrap().len(), 2);
        assert!(fx.store.agent_whoami(a, "s2").is_none());
    }

    #[test]
    fn sweep_keeps_fresh_agents() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        fx.store.agent_touch(p, "fresh").unwrap();
        // Nothing is stale, so the sweep removes nothing.
        assert!(fx.store.sweep_idle_agents().unwrap().is_empty());
        assert_eq!(fx.store.agent_list(p).unwrap().len(), 1);
    }

    #[test]
    fn empty_locks_md_after_bootstrap() {
        let mut fx = Fixture::new();
        let (_p, root) = fx.project("proj");
        let locks_md = std::fs::read_to_string(root.join(".panopt/locks.md")).unwrap();
        assert!(locks_md.contains("_(no locks held)_"), "{locks_md}");
    }

    #[test]
    fn locks_acquire_release_and_project() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        fx.store.agent_touch(p, "a").unwrap();
        fx.store.agent_identify(p, "a", "backend".into(), None).unwrap();
        fx.store.agent_touch(p, "b").unwrap();

        // `a` acquires; `b` is denied and sees the holder's resolved name.
        assert_eq!(
            fx.store
                .lock_acquire(p, "a", "auth".into(), Some("token work".into()))
                .unwrap(),
            None
        );
        assert_eq!(
            fx.store.lock_acquire(p, "b", "auth".into(), None).unwrap(),
            Some("backend".to_string())
        );

        let locks_md = std::fs::read_to_string(root.join(".panopt/locks.md")).unwrap();
        assert!(
            locks_md.contains("- `auth` - held by backend - token work"),
            "{locks_md}"
        );

        // `a` releases; `b` can then take it.
        assert_eq!(fx.store.lock_release(p, "a", "auth").unwrap(), None);
        assert_eq!(fx.store.lock_acquire(p, "b", "auth".into(), None).unwrap(), None);

        let locks = fx.store.lock_list(p);
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].holder_key, "b");
    }

    #[test]
    fn locks_are_project_isolated() {
        let mut fx = Fixture::new();
        let (a, _) = fx.project("alpha");
        let (b, _) = fx.project("beta");

        assert_eq!(fx.store.lock_acquire(a, "x", "build".into(), None).unwrap(), None);
        // The same name in another project is unaffected.
        assert_eq!(fx.store.lock_acquire(b, "y", "build".into(), None).unwrap(), None);
        assert_eq!(fx.store.lock_list(a).len(), 1);
        assert_eq!(fx.store.lock_list(b).len(), 1);
    }

    #[test]
    fn roster_create_list_update_delete_and_project() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");

        let id = fx
            .store
            .roster_create(
                p,
                RosterKind::Agent,
                "claude".into(),
                "Mediator".into(),
                "claude --model sonnet".into(),
                String::new(),
            )
            .unwrap();
        assert_eq!(id, 1);

        let entries = fx.store.roster_list(p).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, RosterKind::Agent);
        assert_eq!(entries[0].display_name, "Mediator");

        // The roster is projected to disk for the cockpit plugin to read.
        let roster_md = std::fs::read_to_string(root.join(".panopt/roster.md")).unwrap();
        assert!(roster_md.contains("- [agent] #1 Mediator"), "{roster_md}");

        fx.store
            .roster_update(
                p,
                id,
                RosterPatch { command: Some("claude".into()), ..Default::default() },
            )
            .unwrap();
        assert_eq!(fx.store.roster_get(p, id).unwrap().command, "claude");

        fx.store.roster_delete(p, id).unwrap();
        assert!(fx.store.roster_list(p).unwrap().is_empty());
        assert!(matches!(
            fx.store.roster_delete(p, id),
            Err(CoreError::RosterNotFound(_))
        ));
    }

    #[test]
    fn roster_ids_are_independent_of_todos_and_scratchpads() {
        let mut fx = Fixture::new();
        let (p, _) = fx.project("proj");
        fx.store.todo_create(p, "a".into()).unwrap();
        fx.store.scratchpad_create(p, "pad".into()).unwrap();
        let kind = RosterKind::Command;
        assert_eq!(
            fx.store
                .roster_create(p, kind, "run".into(), String::new(), "make".into(), String::new())
                .unwrap(),
            1
        );
    }

    #[test]
    fn scratchpad_create_projects_the_index() {
        let mut fx = Fixture::new();
        let (p, root) = fx.project("proj");
        fx.store.scratchpad_create(p, "design notes".into()).unwrap();
        fx.store.scratchpad_create(p, "scratch".into()).unwrap();
        let index = std::fs::read_to_string(root.join(".panopt/scratchpads.md")).unwrap();
        assert!(index.contains("- [#1](scratchpad/1.md) design notes"), "{index}");
        assert!(index.contains("- [#2](scratchpad/2.md) scratch"), "{index}");
    }
}
