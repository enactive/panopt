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
use crate::model::{Agent, Lock, Priority, ProjectId, Scratchpad, Todo, TodoComment, TodoPatch, TodoStatus};
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
                "INSERT INTO scratchpads (project_id, id, title, body) VALUES (?1, ?2, ?3, '')",
                params![pid, next, title],
            )?;
            tx.execute(
                "UPDATE projects SET next_scratchpad_id = ?1 WHERE id = ?2",
                params![next + 1, pid],
            )?;
            tx.commit()?;
            next as u64
        };
        self.reproject_scratchpad(project, id)?;
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
            "UPDATE scratchpads SET body = ?1 WHERE project_id = ?2 AND id = ?3",
            params![body, project.0, id as i64],
        )?;
        self.reproject_scratchpad(project, id)
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
                "SELECT title, body FROM scratchpads WHERE project_id = ?1 AND id = ?2",
                params![project.0, id as i64],
                |r| Ok(Scratchpad { id, title: r.get(0)?, body: r.get(1)? }),
            )
            .optional()?
            .ok_or(CoreError::ScratchpadNotFound(id))
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
        projection::project_agents(root, &self.registry.list(project.0))?;
        projection::project_locks(root, &self.lock_list(project))?;
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
        assert_eq!(sp_md, "# notes\n\nfirst\nsecond\n");
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
}
