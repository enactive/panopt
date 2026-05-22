//! PANopt coordination sidebar - a Zellij plugin.
//!
//! Renders a sidebar pane with two sections: the live list of terminal panes
//! (the agents) and the project's todos, read from PANopt's `.panopt/todos.md`
//! index. Up/Down move one cursor through both lists; Enter focuses a selected
//! pane, or opens the todo form for a selected todo.
//!
//! The sidebar is a list-and-launch surface, never an editor. The todo form is
//! `panopt todo edit`, opened in a *floating* pane (`a` spawns an agent, `c`
//! creates a todo, Enter edits the selected one) - so the form gets real room
//! and the narrow sidebar stays a list. Spawning lives here because a plugin
//! runs inside the session and opens panes through the Zellij API directly.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use zellij_tile::prelude::*;

#[derive(Default)]
struct PanoptSidebar {
    /// Absolute project root, from the layout's plugin config. The cwd for
    /// spawned agent panes and todo forms.
    ws: Option<String>,
    /// Absolute path to the `panopt` binary, from the layout's plugin config.
    panopt_bin: String,
    /// The daemon port, from the layout's plugin config. Passed to the todo
    /// form so it reaches the same daemon the cockpit booted.
    port: String,
    /// Whether Zellij has granted the requested permissions. Until it has,
    /// opening a pane would panic in the host shim, so launching waits.
    permitted: bool,
    /// Terminal panes flattened across tabs, in stable (tab, id) order.
    panes: Vec<PaneRow>,
    /// The project's todos, parsed from the `.panopt/todos.md` index.
    todos: Vec<TodoRow>,
    /// Selection cursor over the panes followed by the todos.
    cursor: usize,
}

struct PaneRow {
    id: PaneId,
    title: String,
    focused: bool,
}

struct TodoRow {
    id: u64,
    label: String,
}

register_plugin!(PanoptSidebar);

impl ZellijPlugin for PanoptSidebar {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.ws = configuration.get("ws").cloned();
        self.panopt_bin = configuration
            .get("panopt_bin")
            .cloned()
            .unwrap_or_else(|| "panopt".to_string());
        self.port = configuration
            .get("port")
            .cloned()
            .unwrap_or_else(|| "7600".to_string());
        // `RunCommands` is needed to open panes for agents and todo forms.
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::RunCommands,
        ]);
        subscribe(&[
            EventType::PaneUpdate,
            EventType::Key,
            EventType::Timer,
            EventType::PermissionRequestResult,
        ]);
        self.read_todos();
        set_timeout(1.0);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(status) => {
                self.permitted = matches!(status, PermissionStatus::Granted);
                true
            }
            Event::PaneUpdate(manifest) => {
                self.ingest_panes(manifest);
                true
            }
            Event::Key(key) => self.handle_key(key),
            Event::Timer(_) => {
                self.read_todos();
                set_timeout(1.0);
                true
            }
            _ => false,
        }
    }

    /// A `panopt:spawn-agent` pipe message opens a new agent pane. An optional
    /// payload is the agent id to use; without one, `panopt _agent` mints its
    /// own.
    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        if pipe_message.name == "panopt:spawn-agent" {
            self.spawn_agent_pane(pipe_message.payload.as_deref());
            return true;
        }
        false
    }

    fn render(&mut self, rows: usize, cols: usize) {
        // The plugin's stdout becomes the pane content directly. Every emitted
        // line must carry non-whitespace content - Zellij's parser rejects a
        // blank line - so blank rows are dropped and the layout stays compact.
        let mut lines: Vec<String> = Vec::new();
        lines.push("PANopt".to_string());

        let hint = if self.permitted {
            "[a] agent  [c] todo"
        } else {
            "grant permissions in the Zellij prompt"
        };
        lines.push(format!("AGENTS  {hint}"));
        if self.panes.is_empty() {
            lines.push("  (none)".to_string());
        }
        for (i, p) in self.panes.iter().enumerate() {
            let cursor = if i == self.cursor { '>' } else { ' ' };
            let focus = if p.focused { '*' } else { ' ' };
            let title = if p.title.trim().is_empty() {
                "(untitled)"
            } else {
                p.title.trim()
            };
            lines.push(format!("{cursor}{focus} {title}"));
        }

        lines.push("TODOS  [Enter] edit".to_string());
        if self.todos.is_empty() {
            lines.push("  (none)".to_string());
        }
        let todo_base = self.panes.len();
        for (i, t) in self.todos.iter().enumerate() {
            let cursor = if todo_base + i == self.cursor { '>' } else { ' ' };
            lines.push(format!("{cursor} #{} {}", t.id, t.label));
        }

        for line in lines.into_iter().take(rows) {
            let truncated: String = line.chars().take(cols).collect();
            print!("{truncated}\r\n");
        }
    }
}

/// What the cursor currently points at.
enum Selection {
    Pane(usize),
    Todo(usize),
}

impl PanoptSidebar {
    /// Read and parse the `.panopt/todos.md` index from Zellij's `/host` mount.
    fn read_todos(&mut self) {
        self.todos = match fs::read_to_string("/host/.panopt/todos.md") {
            Ok(body) => body.lines().filter_map(parse_todo_line).collect(),
            Err(_) => Vec::new(),
        };
        self.clamp_cursor();
    }

    /// Flatten the pane manifest into the terminal-pane list.
    fn ingest_panes(&mut self, manifest: PaneManifest) {
        let mut tabs: Vec<&usize> = manifest.panes.keys().collect();
        tabs.sort();
        let mut rows = Vec::new();
        for tab in tabs {
            for p in &manifest.panes[tab] {
                if p.is_plugin || p.is_suppressed {
                    continue;
                }
                rows.push(PaneRow {
                    id: PaneId::Terminal(p.id),
                    title: p.title.clone(),
                    focused: p.is_focused,
                });
            }
        }
        self.panes = rows;
        self.clamp_cursor();
    }

    /// Keep the cursor within the combined panes-then-todos list.
    fn clamp_cursor(&mut self) {
        let total = self.panes.len() + self.todos.len();
        if self.cursor >= total {
            self.cursor = total.saturating_sub(1);
        }
    }

    /// Resolve the cursor to a pane or a todo, if it points at one.
    fn selection(&self) -> Option<Selection> {
        let total = self.panes.len() + self.todos.len();
        if total == 0 || self.cursor >= total {
            return None;
        }
        if self.cursor < self.panes.len() {
            Some(Selection::Pane(self.cursor))
        } else {
            Some(Selection::Todo(self.cursor - self.panes.len()))
        }
    }

    fn handle_key(&mut self, key: KeyWithModifier) -> bool {
        let total = self.panes.len() + self.todos.len();
        match key.bare_key {
            BareKey::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                true
            }
            BareKey::Down => {
                if self.cursor + 1 < total {
                    self.cursor += 1;
                }
                true
            }
            BareKey::Enter => {
                match self.selection() {
                    Some(Selection::Pane(i)) => focus_pane_with_id(self.panes[i].id, false, false),
                    Some(Selection::Todo(i)) => self.open_todo_form(Some(self.todos[i].id)),
                    None => {}
                }
                true
            }
            BareKey::Char('a') => {
                self.spawn_agent_pane(None);
                true
            }
            BareKey::Char('c') => {
                self.open_todo_form(None);
                true
            }
            _ => false,
        }
    }

    /// Open a new agent pane: a command pane running `panopt _agent` in the
    /// project root. `id`, when given, becomes the agent's id; otherwise
    /// `panopt _agent` mints one.
    fn spawn_agent_pane(&self, id: Option<&str>) {
        let Some(ws) = self.launch_cwd() else {
            return;
        };
        let mut args = vec!["_agent".to_string()];
        if let Some(id) = id {
            args.push("--id".to_string());
            args.push(id.to_string());
        }
        open_command_pane(
            CommandToRun {
                path: PathBuf::from(&self.panopt_bin),
                args,
                cwd: Some(ws),
            },
            BTreeMap::new(),
        );
    }

    /// Open the todo form in a floating pane: `panopt todo edit <id>`, or
    /// `panopt todo edit --new` when `id` is `None`. Floating, so the form gets
    /// real room without disturbing the cockpit layout.
    fn open_todo_form(&self, id: Option<u64>) {
        let Some(ws) = self.launch_cwd() else {
            return;
        };
        let mut args = vec!["todo".to_string(), "edit".to_string()];
        match id {
            Some(id) => args.push(id.to_string()),
            None => args.push("--new".to_string()),
        }
        args.push("--port".to_string());
        args.push(self.port.clone());
        open_command_pane_floating(
            CommandToRun {
                path: PathBuf::from(&self.panopt_bin),
                args,
                cwd: Some(ws),
            },
            None,
            BTreeMap::new(),
        );
    }

    /// The cwd for a launched pane: the project root. `None` - so the caller
    /// does nothing - when permissions are not yet granted (opening a pane
    /// would panic in the host shim) or no project is configured.
    fn launch_cwd(&self) -> Option<PathBuf> {
        if !self.permitted {
            return None;
        }
        self.ws.as_ref().map(PathBuf::from)
    }
}

/// Parse one line of the `.panopt/todos.md` index into a [`TodoRow`].
///
/// Index lines read `- [ ] [#3](todos/3.md) the title - status, priority`;
/// any other line (the heading, the empty-state placeholder) yields `None`.
fn parse_todo_line(line: &str) -> Option<TodoRow> {
    let line = line.trim();
    if !line.starts_with("- [") {
        return None;
    }
    let hash = line.find("[#")? + 2;
    let close = line[hash..].find(']')? + hash;
    let id: u64 = line[hash..close].parse().ok()?;
    let label_at = line[close..].find(") ")? + close + 2;
    let label = line.get(label_at..).unwrap_or("").trim().to_string();
    Some(TodoRow { id, label })
}

#[cfg(test)]
mod tests {
    use super::parse_todo_line;

    #[test]
    fn parses_an_index_line() {
        let row = parse_todo_line("- [ ] [#3](todos/3.md) wire the form - open, high").unwrap();
        assert_eq!(row.id, 3);
        assert_eq!(row.label, "wire the form - open, high");
    }

    #[test]
    fn ignores_non_todo_lines() {
        assert!(parse_todo_line("# Todos").is_none());
        assert!(parse_todo_line("_(no todos)_").is_none());
        assert!(parse_todo_line("").is_none());
    }
}
