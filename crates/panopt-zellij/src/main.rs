//! PANopt coordination sidebar - a Zellij plugin.
//!
//! Renders a sidebar pane with two sections: the live list of terminal panes
//! (the agents) and the todos read from PANopt's `.panopt/todos.md` projection.
//! Up/Down move a cursor; Enter focuses the selected pane; `a` opens a new
//! agent pane.
//!
//! The plugin is also the cockpit's spawner: pressing `a`, or receiving a
//! `panopt:spawn-agent` pipe message (sent by `panopt agent`), opens a command
//! pane running `panopt _agent`. Spawning lives here because a plugin runs
//! inside the session and opens panes through the Zellij API directly - no
//! `zellij action` shelling, no "run it in the right place" (DESIGN.md §9).

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use zellij_tile::prelude::*;

#[derive(Default)]
struct PanoptSidebar {
    /// Absolute project root, from the layout's plugin config. The cwd for
    /// spawned agent panes.
    ws: Option<String>,
    /// Absolute path to the `panopt` binary, from the layout's plugin config.
    panopt_bin: String,
    /// Whether Zellij has granted the requested permissions. Until it has,
    /// `open_command_pane` would panic in the host shim, so spawning waits.
    permitted: bool,
    /// Terminal panes flattened across tabs, in stable (tab, id) order.
    panes: Vec<PaneRow>,
    /// Lines of `.panopt/todos.md`.
    todos: Vec<String>,
    /// Selection cursor into `panes`.
    cursor: usize,
}

struct PaneRow {
    id: PaneId,
    title: String,
    focused: bool,
}

register_plugin!(PanoptSidebar);

impl ZellijPlugin for PanoptSidebar {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.ws = configuration.get("ws").cloned();
        self.panopt_bin = configuration
            .get("panopt_bin")
            .cloned()
            .unwrap_or_else(|| "panopt".to_string());
        // `RunCommands` is needed to open command panes for new agents.
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
            "[a] new agent"
        } else {
            "grant permissions in the Zellij prompt"
        };
        lines.push(format!("PANES  {hint}"));
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

        lines.push("TODOS".to_string());
        for t in &self.todos {
            let t = t.trim_end();
            if !t.trim().is_empty() {
                lines.push(t.to_string());
            }
        }

        for line in lines.into_iter().take(rows) {
            let truncated: String = line.chars().take(cols).collect();
            print!("{truncated}\r\n");
        }
    }
}

impl PanoptSidebar {
    /// Read `.panopt/todos.md` from Zellij's `/host` mount.
    fn read_todos(&mut self) {
        self.todos = match fs::read_to_string("/host/.panopt/todos.md") {
            Ok(body) => body.lines().map(|l| l.to_string()).collect(),
            Err(_) => vec!["(no /host/.panopt/todos.md)".to_string()],
        };
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
        if self.cursor >= self.panes.len() {
            self.cursor = self.panes.len().saturating_sub(1);
        }
    }

    fn handle_key(&mut self, key: KeyWithModifier) -> bool {
        match key.bare_key {
            BareKey::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                true
            }
            BareKey::Down => {
                if self.cursor + 1 < self.panes.len() {
                    self.cursor += 1;
                }
                true
            }
            BareKey::Enter => {
                if let Some(row) = self.panes.get(self.cursor) {
                    focus_pane_with_id(row.id, false, false);
                }
                true
            }
            BareKey::Char('a') => {
                self.spawn_agent_pane(None);
                true
            }
            _ => false,
        }
    }

    /// Open a new agent pane: a command pane running `panopt _agent` in the
    /// project root. `id`, when given, becomes the agent's id; otherwise
    /// `panopt _agent` mints one.
    fn spawn_agent_pane(&self, id: Option<&str>) {
        // Opening a command pane needs the RunCommands permission; without it
        // the host shim panics. Wait until the permission request is granted.
        if !self.permitted {
            return;
        }
        let Some(ws) = &self.ws else {
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
                cwd: Some(PathBuf::from(ws)),
            },
            BTreeMap::new(),
        );
    }
}
