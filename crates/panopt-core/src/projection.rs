//! Filesystem projection: rendering state to markdown under `.panopt/`.
//!
//! Every write goes through [`atomic_write`] - write to a temp file in the same
//! directory, then `rename` over the target - so a reader (Zed) never observes a
//! half-written file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::model::{Agent, Lock, Scratchpad, Todo, TodoStatus};

/// Per-process counter giving each temp file a unique name, so two concurrent
/// writes to the same target never collide on the temp path.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn panopt_dir(ws: &Path) -> PathBuf {
    ws.join(".panopt")
}

fn todos_path(ws: &Path) -> PathBuf {
    panopt_dir(ws).join("todos.md")
}

fn scratchpad_dir(ws: &Path) -> PathBuf {
    panopt_dir(ws).join("scratchpad")
}

fn scratchpad_path(ws: &Path, id: u64) -> PathBuf {
    scratchpad_dir(ws).join(format!("{id}.md"))
}

fn agents_path(ws: &Path) -> PathBuf {
    panopt_dir(ws).join("agents.md")
}

fn locks_path(ws: &Path) -> PathBuf {
    panopt_dir(ws).join("locks.md")
}

/// Atomically replace `target` with `contents`.
///
/// The temp file is created in `target`'s own directory, which guarantees it is
/// on the same filesystem - a precondition for `rename` to be atomic.
fn atomic_write(target: &Path, contents: &str) -> io::Result<()> {
    let parent = target.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "target has no parent directory")
    })?;
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no file name"))?;

    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{file_name}.tmp.{}.{seq}", std::process::id()));

    match fs::write(&tmp, contents).and_then(|()| fs::rename(&tmp, target)) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Best-effort cleanup so a failed write leaves no `.tmp` litter.
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Render the whole todo list as a GitHub-style markdown checklist.
pub(crate) fn render_todos_md(todos: &[Todo]) -> String {
    let mut out = String::from("# Todos\n\n");
    if todos.is_empty() {
        out.push_str("_(no todos)_\n");
        return out;
    }
    for todo in todos {
        let mark = match todo.status {
            TodoStatus::Open => ' ',
            TodoStatus::Done => 'x',
        };
        out.push_str(&format!("- [{mark}] (#{}) {}\n", todo.id, todo.title));
    }
    out
}

/// Render a scratchpad as its title (H1) followed by its body verbatim.
pub(crate) fn render_scratchpad_md(pad: &Scratchpad) -> String {
    let mut out = format!("# {}\n\n", pad.title);
    out.push_str(&pad.body);
    if !pad.body.is_empty() && !pad.body.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Render the connected-agent roster as a markdown list.
///
/// Each agent is shown as its name and, when set, its self-reported status.
/// No timestamp is rendered: the file holds the stable facts, and live idle
/// time is a tool query (`agent_list`).
pub(crate) fn render_agents_md(agents: &[Agent]) -> String {
    let mut out = String::from("# Agents\n\n");
    if agents.is_empty() {
        out.push_str("_(no agents connected)_\n");
        return out;
    }
    for agent in agents {
        if agent.status.is_empty() {
            out.push_str(&format!("- {}\n", agent.name));
        } else {
            out.push_str(&format!("- {} - {}\n", agent.name, agent.status));
        }
    }
    out
}

/// Render the advisory-lock table as a markdown list.
///
/// Each lock shows its name, holder, and - when set - the holder's note.
pub(crate) fn render_locks_md(locks: &[Lock]) -> String {
    let mut out = String::from("# Locks\n\n");
    if locks.is_empty() {
        out.push_str("_(no locks held)_\n");
        return out;
    }
    for lock in locks {
        if lock.note.is_empty() {
            out.push_str(&format!("- `{}` - held by {}\n", lock.name, lock.holder_name));
        } else {
            out.push_str(&format!(
                "- `{}` - held by {} - {}\n",
                lock.name, lock.holder_name, lock.note
            ));
        }
    }
    out
}

/// Create the `.panopt/` projection tree and its initial files.
///
/// Called from [`crate::Store::ensure_project`]. Writes `.panopt/.gitignore`
/// (`*`) so git ignores the whole projection, plus empty `todos.md`,
/// `agents.md`, and `locks.md` so the files a user pins in an editor exist
/// before any tool call.
pub(crate) fn bootstrap(ws: &Path) -> io::Result<()> {
    fs::create_dir_all(panopt_dir(ws))?;
    fs::create_dir_all(scratchpad_dir(ws))?;
    atomic_write(&panopt_dir(ws).join(".gitignore"), "*\n")?;
    atomic_write(&todos_path(ws), &render_todos_md(&[]))?;
    atomic_write(&agents_path(ws), &render_agents_md(&[]))?;
    atomic_write(&locks_path(ws), &render_locks_md(&[]))?;
    Ok(())
}

/// Rewrite `.panopt/todos.md` from the current todo list.
pub(crate) fn project_todos(ws: &Path, todos: &[Todo]) -> io::Result<()> {
    atomic_write(&todos_path(ws), &render_todos_md(todos))
}

/// Rewrite the one `.panopt/scratchpad/<id>.md` for the given scratchpad.
pub(crate) fn project_scratchpad(ws: &Path, pad: &Scratchpad) -> io::Result<()> {
    atomic_write(&scratchpad_path(ws, pad.id), &render_scratchpad_md(pad))
}

/// Rewrite `.panopt/agents.md` from the current agent roster.
pub(crate) fn project_agents(ws: &Path, agents: &[Agent]) -> io::Result<()> {
    atomic_write(&agents_path(ws), &render_agents_md(agents))
}

/// Rewrite `.panopt/locks.md` from the current advisory-lock table.
pub(crate) fn project_locks(ws: &Path, locks: &[Lock]) -> io::Result<()> {
    atomic_write(&locks_path(ws), &render_locks_md(locks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_todo_list_renders_placeholder() {
        let md = render_todos_md(&[]);
        assert_eq!(md, "# Todos\n\n_(no todos)_\n");
    }

    #[test]
    fn todos_render_with_id_and_checkbox() {
        let todos = vec![
            Todo { id: 1, title: "wire up auth".into(), status: TodoStatus::Open },
            Todo { id: 2, title: "write readme".into(), status: TodoStatus::Done },
        ];
        let md = render_todos_md(&todos);
        assert_eq!(
            md,
            "# Todos\n\n- [ ] (#1) wire up auth\n- [x] (#2) write readme\n"
        );
    }

    #[test]
    fn scratchpad_renders_title_then_body() {
        let pad = Scratchpad { id: 3, title: "notes".into(), body: "line one".into() };
        assert_eq!(render_scratchpad_md(&pad), "# notes\n\nline one\n");
    }

    #[test]
    fn empty_agent_roster_renders_placeholder() {
        assert_eq!(render_agents_md(&[]), "# Agents\n\n_(no agents connected)_\n");
    }

    #[test]
    fn agents_render_with_and_without_status() {
        let now = std::time::SystemTime::now();
        let agents = vec![
            Agent {
                key: "k1".into(),
                name: "alpha".into(),
                status: "working".into(),
                first_seen: now,
                last_seen: now,
            },
            Agent {
                key: "k2".into(),
                name: "beta".into(),
                status: String::new(),
                first_seen: now,
                last_seen: now,
            },
        ];
        assert_eq!(
            render_agents_md(&agents),
            "# Agents\n\n- alpha - working\n- beta\n"
        );
    }

    #[test]
    fn empty_lock_table_renders_placeholder() {
        assert_eq!(render_locks_md(&[]), "# Locks\n\n_(no locks held)_\n");
    }

    #[test]
    fn locks_render_with_and_without_a_note() {
        let now = std::time::SystemTime::now();
        let locks = vec![
            Lock {
                name: "auth".into(),
                holder_key: "k1".into(),
                holder_name: "backend".into(),
                note: "token work".into(),
                acquired_at: now,
            },
            Lock {
                name: "deploy".into(),
                holder_key: "k2".into(),
                holder_name: "frontend".into(),
                note: String::new(),
                acquired_at: now,
            },
        ];
        assert_eq!(
            render_locks_md(&locks),
            "# Locks\n\n- `auth` - held by backend - token work\n- `deploy` - held by frontend\n"
        );
    }

    #[test]
    fn bootstrap_creates_tree_and_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        bootstrap(dir.path()).unwrap();

        assert!(dir.path().join(".panopt/scratchpad").is_dir());
        assert_eq!(
            fs::read_to_string(dir.path().join(".panopt/.gitignore")).unwrap(),
            "*\n"
        );
        assert!(dir.path().join(".panopt/todos.md").is_file());
        assert!(dir.path().join(".panopt/agents.md").is_file());
        assert!(dir.path().join(".panopt/locks.md").is_file());
    }

    #[test]
    fn atomic_write_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.md");
        atomic_write(&target, "first").unwrap();
        atomic_write(&target, "second").unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "second");
        let stray: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(stray.is_empty(), "stray temp files: {stray:?}");
    }
}
