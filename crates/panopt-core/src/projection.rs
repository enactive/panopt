//! Filesystem projection: rendering state to markdown under `.panopt/`.
//!
//! Every write goes through [`atomic_write`] - write to a temp file in the same
//! directory, then `rename` over the target - so a reader (an editor, the
//! Zellij plugin) never observes a half-written file.
//!
//! Todos project as one file per todo under `.panopt/todos/<id>.md`, each a
//! self-contained record with a `---` frontmatter block of structured fields
//! and a markdown body, plus a `.panopt/todos.md` index linking them all.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::model::{Agent, AgentTool, Lock, Process, Scratchpad, Todo, TodoStatus};

/// Per-process counter giving each temp file a unique name, so two concurrent
/// writes to the same target never collide on the temp path.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn panopt_dir(ws: &Path) -> PathBuf {
    ws.join(".panopt")
}

/// The todo index file: a checklist linking every per-todo file.
fn todos_index_path(ws: &Path) -> PathBuf {
    panopt_dir(ws).join("todos.md")
}

/// The directory holding one `<id>.md` file per todo.
fn todos_dir(ws: &Path) -> PathBuf {
    panopt_dir(ws).join("todos")
}

fn todo_path(ws: &Path, id: u64) -> PathBuf {
    todos_dir(ws).join(format!("{id}.md"))
}

fn scratchpad_dir(ws: &Path) -> PathBuf {
    panopt_dir(ws).join("scratchpad")
}

fn scratchpad_path(ws: &Path, id: u64) -> PathBuf {
    scratchpad_dir(ws).join(format!("{id}.md"))
}

/// The scratchpad index file: one line linking every per-scratchpad file. The
/// cockpit plugin reads it to list scratchpads (the per-pad files carry no
/// index of their own).
fn scratchpads_index_path(ws: &Path) -> PathBuf {
    panopt_dir(ws).join("scratchpads.md")
}

/// The agent-tools projection: durable agent configurations (config layer).
fn agent_tools_path(ws: &Path) -> PathBuf {
    panopt_dir(ws).join("agent_tools.md")
}

/// The processes projection: per-project process instances (instance layer).
fn processes_path(ws: &Path) -> PathBuf {
    panopt_dir(ws).join("processes.md")
}

/// The pre-V6 roster projection, kept only so [`bootstrap`] can remove it
/// from older installs migrating in place.
fn legacy_roster_path(ws: &Path) -> PathBuf {
    panopt_dir(ws).join("roster.md")
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
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "target has no parent directory",
        )
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

/// Render the todo index: a GitHub-style checklist, one line per todo, each
/// linking to that todo's own file.
pub(crate) fn render_todos_index_md(todos: &[Todo]) -> String {
    let mut out = String::from("# Todos\n\n");
    if todos.is_empty() {
        out.push_str("_(no todos)_\n");
        return out;
    }
    for todo in todos {
        let mark = match todo.status {
            TodoStatus::Completed => 'x',
            // Closed-but-not-done renders as `[-]`, a common markdown
            // convention for cancelled / won't-do items. It distinguishes
            // these from still-open todos without claiming they're done.
            TodoStatus::NotDone => '-',
            _ => ' ',
        };
        out.push_str(&format!(
            "- [{mark}] [#{id}](todos/{id}.md) {title} - {status}, {priority}\n",
            id = todo.id,
            title = todo.title,
            status = todo.status.as_str(),
            priority = todo.priority.as_str(),
        ));
    }
    out
}

/// Render one todo as a self-contained markdown file: a `---` frontmatter block
/// of structured fields, the title as an H1, the body, then any comments.
pub(crate) fn render_todo_md(todo: &Todo) -> String {
    let blockers = todo
        .blockers
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    let mut out = String::from("---\n");
    out.push_str(&format!("status: {}\n", todo.status.as_str()));
    out.push_str(&format!("priority: {}\n", todo.priority.as_str()));
    out.push_str(&format!("assignee: {}\n", todo.assignee));
    out.push_str(&format!("tags: {}\n", todo.tags.join(", ")));
    out.push_str(&format!("blockers: {blockers}\n"));
    out.push_str(&format!("created: {}\n", todo.created_at));
    out.push_str(&format!("updated: {}\n", todo.updated_at));
    if let Some(completed) = &todo.completed_at {
        out.push_str(&format!("completed: {completed}\n"));
    }
    out.push_str("---\n\n");

    out.push_str(&format!("# {}\n", todo.title));

    if !todo.body.trim().is_empty() {
        out.push('\n');
        out.push_str(todo.body.trim_end());
        out.push('\n');
    }

    if !todo.comments.is_empty() {
        out.push_str("\n## Comments\n");
        for comment in &todo.comments {
            out.push_str(&format!(
                "\n**{}** - {}\n\n",
                comment.author, comment.created_at
            ));
            out.push_str(comment.body.trim_end());
            out.push('\n');
        }
    }
    out
}

/// Render a scratchpad as a `---` frontmatter block of created/updated
/// timestamps, the title as an H1, then its body verbatim. The frontmatter
/// mirrors `render_todo_md` so every per-item file in the projection carries
/// the same structured-then-prose shape.
pub(crate) fn render_scratchpad_md(pad: &Scratchpad) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("created: {}\n", pad.created_at));
    out.push_str(&format!("updated: {}\n", pad.updated_at));
    out.push_str("---\n\n");
    out.push_str(&format!("# {}\n\n", pad.title));
    out.push_str(&pad.body);
    if !pad.body.is_empty() && !pad.body.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Render the scratchpad index: one line per scratchpad, each linking to that
/// scratchpad's own file. `pads` is `(id, title, updated_at)`, id-ascending;
/// the `updated_at` is embedded in the trailing label so every append changes
/// the rendered bytes, which is how the cockpit sidebar (a 1s file poller)
/// notices the change.
pub(crate) fn render_scratchpads_index_md(pads: &[(u64, String, String)]) -> String {
    let mut out = String::from("# Scratchpads\n\n");
    if pads.is_empty() {
        out.push_str("_(no scratchpads)_\n");
        return out;
    }
    for (id, title, updated) in pads {
        out.push_str(&format!(
            "- [#{id}](scratchpad/{id}.md) {title} - updated {updated}\n",
        ));
    }
    out
}

/// Render the agent-tools projection as a markdown list, one line per
/// configured tool: id, label, enabled flag, and the command it would
/// launch. This is the config-layer view of the two-layer model; the cockpit
/// reads it for a future spawn picker.
pub(crate) fn render_agent_tools_md(tools: &[AgentTool]) -> String {
    let mut out = String::from("# Agent tools\n\n");
    if tools.is_empty() {
        out.push_str("_(no agent tools)_\n");
        return out;
    }
    for t in tools {
        let label = if t.display_name.is_empty() {
            &t.name
        } else {
            &t.display_name
        };
        let flag = if t.enabled { "enabled" } else { "disabled" };
        let command = if t.command.is_empty() {
            String::new()
        } else {
            format!(" {}", t.command)
        };
        out.push_str(&format!("- #{} {} [{}]{}\n", t.id, label, flag, command));
    }
    out
}

/// Render the processes projection as a markdown list, one line per instance:
/// kind, id, and label, with a trailing `(from #N)` for processes that carry
/// a back-reference to an agent tool. The line format preserves the shape the
/// cockpit plugin already parses for the pre-V6 roster file, so the existing
/// `(kind, id, label)` tuple keeps decoding cleanly.
pub(crate) fn render_processes_md(processes: &[Process]) -> String {
    let mut out = String::from("# Processes\n\n");
    if processes.is_empty() {
        out.push_str("_(no processes)_\n");
        return out;
    }
    for p in processes {
        let label = if !p.display_name.is_empty() {
            p.display_name.as_str()
        } else if !p.name.is_empty() {
            p.name.as_str()
        } else {
            "(unnamed)"
        };
        let tool_suffix = match p.agent_tool_id {
            Some(tid) => format!(" (from #{tid})"),
            None => String::new(),
        };
        out.push_str(&format!(
            "- [{}] #{} {}{}\n",
            p.kind.as_str(),
            p.id,
            label,
            tool_suffix
        ));
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
            out.push_str(&format!(
                "- `{}` - held by {}\n",
                lock.name, lock.holder_name
            ));
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
/// (`*`) so git ignores the whole projection, plus an empty `todos.md` index,
/// `agents.md`, and `locks.md` so the files a user pins in an editor exist
/// before any tool call.
pub(crate) fn bootstrap(ws: &Path) -> io::Result<()> {
    fs::create_dir_all(panopt_dir(ws))?;
    fs::create_dir_all(scratchpad_dir(ws))?;
    fs::create_dir_all(todos_dir(ws))?;
    atomic_write(&panopt_dir(ws).join(".gitignore"), "*\n")?;
    atomic_write(&todos_index_path(ws), &render_todos_index_md(&[]))?;
    atomic_write(
        &scratchpads_index_path(ws),
        &render_scratchpads_index_md(&[]),
    )?;
    atomic_write(&agent_tools_path(ws), &render_agent_tools_md(&[]))?;
    atomic_write(&processes_path(ws), &render_processes_md(&[]))?;
    atomic_write(&agents_path(ws), &render_agents_md(&[]))?;
    atomic_write(&locks_path(ws), &render_locks_md(&[]))?;
    // Drop any pre-V6 roster.md left behind by an older install; the V6
    // migration removed the table, and the two new files supersede it.
    let _ = fs::remove_file(legacy_roster_path(ws));
    Ok(())
}

/// Rewrite the whole todo projection: the `todos.md` index, one
/// `todos/<id>.md` per todo, and a sweep that deletes the per-todo files of
/// todos that no longer exist.
pub(crate) fn project_todos(ws: &Path, todos: &[Todo]) -> io::Result<()> {
    fs::create_dir_all(todos_dir(ws))?;
    atomic_write(&todos_index_path(ws), &render_todos_index_md(todos))?;
    for todo in todos {
        atomic_write(&todo_path(ws, todo.id), &render_todo_md(todo))?;
    }

    // Remove the per-todo file of any todo that has since been deleted.
    let live: HashSet<u64> = todos.iter().map(|t| t.id).collect();
    if let Ok(entries) = fs::read_dir(todos_dir(ws)) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let stem = match name.to_string_lossy().strip_suffix(".md") {
                Some(stem) => stem.to_string(),
                None => continue,
            };
            if let Ok(id) = stem.parse::<u64>() {
                if !live.contains(&id) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
    Ok(())
}

/// Rewrite the one `.panopt/scratchpad/<id>.md` for the given scratchpad.
pub(crate) fn project_scratchpad(ws: &Path, pad: &Scratchpad) -> io::Result<()> {
    atomic_write(&scratchpad_path(ws, pad.id), &render_scratchpad_md(pad))
}

/// Rewrite `.panopt/scratchpads.md` from the project's
/// `(id, title, updated_at)` list, and sweep per-scratchpad files whose
/// scratchpad has been deleted. Mirrors the sweep in [`project_todos`] so the
/// index is the source of truth for which files should exist.
pub(crate) fn project_scratchpads_index(
    ws: &Path,
    pads: &[(u64, String, String)],
) -> io::Result<()> {
    atomic_write(
        &scratchpads_index_path(ws),
        &render_scratchpads_index_md(pads),
    )?;

    let live: HashSet<u64> = pads.iter().map(|(id, _, _)| *id).collect();
    if let Ok(entries) = fs::read_dir(scratchpad_dir(ws)) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let stem = match name.to_string_lossy().strip_suffix(".md") {
                Some(stem) => stem.to_string(),
                None => continue,
            };
            if let Ok(id) = stem.parse::<u64>() {
                if !live.contains(&id) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
    Ok(())
}

/// Rewrite `.panopt/agent_tools.md` from the current agent-tool list.
pub(crate) fn project_agent_tools(ws: &Path, tools: &[AgentTool]) -> io::Result<()> {
    atomic_write(&agent_tools_path(ws), &render_agent_tools_md(tools))
}

/// Rewrite `.panopt/processes.md` from the current process list.
pub(crate) fn project_processes(ws: &Path, processes: &[Process]) -> io::Result<()> {
    atomic_write(&processes_path(ws), &render_processes_md(processes))
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
    use crate::model::{Priority, ProcessKind, TodoComment};

    /// A todo with the given identity and otherwise-default fields.
    fn todo(id: u64, title: &str, status: TodoStatus) -> Todo {
        Todo {
            id,
            title: title.into(),
            status,
            ..Default::default()
        }
    }

    #[test]
    fn empty_todo_index_renders_placeholder() {
        assert_eq!(render_todos_index_md(&[]), "# Todos\n\n_(no todos)_\n");
    }

    #[test]
    fn todo_index_links_each_todo() {
        let todos = vec![
            todo(1, "wire up auth", TodoStatus::Open),
            todo(2, "write readme", TodoStatus::Completed),
        ];
        assert_eq!(
            render_todos_index_md(&todos),
            "# Todos\n\n\
             - [ ] [#1](todos/1.md) wire up auth - open, medium\n\
             - [x] [#2](todos/2.md) write readme - completed, medium\n"
        );
    }

    #[test]
    fn todo_file_has_frontmatter_and_body() {
        let t = Todo {
            id: 3,
            title: "wire spawn_agent".into(),
            body: "Pipe the request to the cockpit plugin.".into(),
            status: TodoStatus::InProgress,
            priority: Priority::High,
            assignee: "claude-a1f".into(),
            tags: vec!["launcher".into(), "mcp".into()],
            blockers: vec![1, 2],
            comments: vec![TodoComment {
                id: 1,
                author: "claude-a1f".into(),
                body: "looking into this".into(),
                created_at: "2026-05-21 11:00:00".into(),
            }],
            created_at: "2026-05-21 10:00:00".into(),
            updated_at: "2026-05-21 11:00:00".into(),
            completed_at: None,
        };
        assert_eq!(
            render_todo_md(&t),
            "---\n\
             status: in_progress\n\
             priority: high\n\
             assignee: claude-a1f\n\
             tags: launcher, mcp\n\
             blockers: 1, 2\n\
             created: 2026-05-21 10:00:00\n\
             updated: 2026-05-21 11:00:00\n\
             ---\n\n\
             # wire spawn_agent\n\n\
             Pipe the request to the cockpit plugin.\n\n\
             ## Comments\n\n\
             **claude-a1f** - 2026-05-21 11:00:00\n\n\
             looking into this\n"
        );
    }

    #[test]
    fn completed_todo_file_carries_completed_line() {
        let mut t = todo(1, "done thing", TodoStatus::Completed);
        t.completed_at = Some("2026-05-21 12:00:00".into());
        assert!(render_todo_md(&t).contains("completed: 2026-05-21 12:00:00\n"));
    }

    #[test]
    fn project_todos_writes_index_and_per_todo_files() {
        let dir = tempfile::tempdir().unwrap();
        let todos = vec![
            todo(1, "first", TodoStatus::Open),
            todo(2, "second", TodoStatus::Open),
        ];
        project_todos(dir.path(), &todos).unwrap();

        assert!(dir.path().join(".panopt/todos.md").is_file());
        assert!(dir.path().join(".panopt/todos/1.md").is_file());
        assert!(dir.path().join(".panopt/todos/2.md").is_file());
    }

    #[test]
    fn project_todos_sweeps_deleted_per_todo_files() {
        let dir = tempfile::tempdir().unwrap();
        project_todos(
            dir.path(),
            &[
                todo(1, "a", TodoStatus::Open),
                todo(2, "b", TodoStatus::Open),
            ],
        )
        .unwrap();
        // Todo 2 is gone on the next projection; its file must be swept.
        project_todos(dir.path(), &[todo(1, "a", TodoStatus::Open)]).unwrap();

        assert!(dir.path().join(".panopt/todos/1.md").is_file());
        assert!(!dir.path().join(".panopt/todos/2.md").exists());
    }

    #[test]
    fn scratchpad_renders_frontmatter_title_then_body() {
        let pad = Scratchpad {
            id: 3,
            title: "notes".into(),
            body: "line one".into(),
            created_at: "2026-05-23 18:05:00".into(),
            updated_at: "2026-05-23 18:05:21".into(),
        };
        assert_eq!(
            render_scratchpad_md(&pad),
            "---\n\
             created: 2026-05-23 18:05:00\n\
             updated: 2026-05-23 18:05:21\n\
             ---\n\
             \n\
             # notes\n\
             \n\
             line one\n",
        );
    }

    #[test]
    fn empty_agent_roster_renders_placeholder() {
        assert_eq!(
            render_agents_md(&[]),
            "# Agents\n\n_(no agents connected)_\n"
        );
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
        assert!(dir.path().join(".panopt/todos").is_dir());
        assert_eq!(
            fs::read_to_string(dir.path().join(".panopt/.gitignore")).unwrap(),
            "*\n"
        );
        assert!(dir.path().join(".panopt/todos.md").is_file());
        assert!(dir.path().join(".panopt/scratchpads.md").is_file());
        assert!(dir.path().join(".panopt/agent_tools.md").is_file());
        assert!(dir.path().join(".panopt/processes.md").is_file());
        assert!(dir.path().join(".panopt/agents.md").is_file());
        assert!(dir.path().join(".panopt/locks.md").is_file());
    }

    #[test]
    fn bootstrap_removes_a_stale_pre_v6_roster_md() {
        let dir = tempfile::tempdir().unwrap();
        let panopt = dir.path().join(".panopt");
        fs::create_dir_all(&panopt).unwrap();
        fs::write(panopt.join("roster.md"), "# Roster\n\n_(stale)_\n").unwrap();
        bootstrap(dir.path()).unwrap();
        assert!(
            !panopt.join("roster.md").exists(),
            "bootstrap must drop the pre-V6 roster.md"
        );
        assert!(panopt.join("agent_tools.md").is_file());
        assert!(panopt.join("processes.md").is_file());
    }

    #[test]
    fn empty_scratchpads_index_renders_placeholder() {
        assert_eq!(
            render_scratchpads_index_md(&[]),
            "# Scratchpads\n\n_(no scratchpads)_\n"
        );
    }

    #[test]
    fn scratchpads_index_links_each_pad() {
        let pads = vec![
            (
                1,
                "design notes".to_string(),
                "2026-05-23 18:05:21".to_string(),
            ),
            (2, "scratch".to_string(), "2026-05-23 18:06:00".to_string()),
        ];
        assert_eq!(
            render_scratchpads_index_md(&pads),
            "# Scratchpads\n\n\
             - [#1](scratchpad/1.md) design notes - updated 2026-05-23 18:05:21\n\
             - [#2](scratchpad/2.md) scratch - updated 2026-05-23 18:06:00\n"
        );
    }

    #[test]
    fn empty_agent_tools_renders_placeholder() {
        assert_eq!(
            render_agent_tools_md(&[]),
            "# Agent tools\n\n_(no agent tools)_\n"
        );
    }

    #[test]
    fn agent_tools_render_with_label_enabled_flag_and_command() {
        let tools = vec![
            AgentTool {
                id: 1,
                name: "claude".into(),
                display_name: "Mediator".into(),
                command: "claude --model sonnet".into(),
                tool_type: "agent".into(),
                enabled: true,
                ..Default::default()
            },
            AgentTool {
                id: 2,
                name: "scratch".into(),
                tool_type: "agent".into(),
                enabled: false,
                ..Default::default()
            },
        ];
        assert_eq!(
            render_agent_tools_md(&tools),
            "# Agent tools\n\n\
             - #1 Mediator [enabled] claude --model sonnet\n\
             - #2 scratch [disabled]\n"
        );
    }

    #[test]
    fn empty_processes_renders_placeholder() {
        assert_eq!(
            render_processes_md(&[]),
            "# Processes\n\n_(no processes)_\n"
        );
    }

    #[test]
    fn processes_render_with_kind_id_label_and_optional_tool_ref() {
        let processes = vec![
            Process {
                id: 1,
                kind: ProcessKind::Agent,
                name: "claude-a".into(),
                display_name: "Mediator".into(),
                agent_tool_id: Some(7),
                ..Default::default()
            },
            Process {
                id: 2,
                kind: ProcessKind::Command,
                name: "Run server".into(),
                ..Default::default()
            },
        ];
        assert_eq!(
            render_processes_md(&processes),
            "# Processes\n\n\
             - [agent] #1 Mediator (from #7)\n\
             - [command] #2 Run server\n"
        );
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
