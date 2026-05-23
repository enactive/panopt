//! `panopt _viewer` - a long-lived cockpit viewer pane.
//!
//! Renders one item - a todo, a scratchpad, or a section list - from the
//! project's `.panopt/` projection, scrollable and read-only. The sidebar
//! plugin re-points the viewer at a different item by writing a routing file
//! the viewer polls; switching item saves the outgoing item's scroll position
//! and restores the incoming one's (see [`crate::viewstate`]).
//!
//! Content always comes from the projected `.panopt/*.md` files, which the
//! daemon keeps current, so the viewer never holds a stale snapshot and needs
//! no MCP connection of its own. The pane is long-lived: it closes only when
//! the user presses `q`, never on its own.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use serde_json::Value;

use crate::viewstate::{self, ViewState};

/// How often the viewer wakes to poll the routing file and refresh content.
const TICK: Duration = Duration::from_millis(250);
/// The shortest gap between re-reads of the displayed content file.
const REFRESH: Duration = Duration::from_millis(800);

/// What a viewer pane is currently showing.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    Todo(u64),
    Scratchpad(u64),
    TodoList,
    ScratchpadList,
    Empty,
}

impl Target {
    /// Parse a routing payload's `kind`/`id` pair into a target. The `empty`
    /// kind lets the sidebar clear the viewer when nothing is selected.
    fn parse(kind: &str, id: Option<u64>) -> Option<Target> {
        match (kind, id) {
            ("todo", Some(id)) => Some(Target::Todo(id)),
            ("scratchpad", Some(id)) => Some(Target::Scratchpad(id)),
            ("todo-list", _) => Some(Target::TodoList),
            ("scratchpad-list", _) => Some(Target::ScratchpadList),
            ("empty", _) => Some(Target::Empty),
            _ => None,
        }
    }

    /// The [`crate::viewstate`] key under which this target's position is kept.
    fn key(&self) -> String {
        match self {
            Target::Todo(id) => format!("todo:{id}"),
            Target::Scratchpad(id) => format!("scratchpad:{id}"),
            Target::TodoList => "list:todos".to_string(),
            Target::ScratchpadList => "list:scratchpads".to_string(),
            Target::Empty => "empty".to_string(),
        }
    }

    fn is_list(&self) -> bool {
        matches!(self, Target::TodoList | Target::ScratchpadList)
    }

    /// The projected `.panopt/` file backing this target, relative to the
    /// project root.
    fn content_path(&self, ws: &Path) -> Option<PathBuf> {
        let panopt = ws.join(".panopt");
        match self {
            Target::Todo(id) => Some(panopt.join("todos").join(format!("{id}.md"))),
            Target::Scratchpad(id) => Some(panopt.join("scratchpad").join(format!("{id}.md"))),
            Target::TodoList => Some(panopt.join("todos.md")),
            Target::ScratchpadList => Some(panopt.join("scratchpads.md")),
            Target::Empty => None,
        }
    }
}

/// The loaded, render-ready content for the current target.
enum Content {
    /// A scrollable document: the lines of a projected `.md` file.
    Doc(Vec<String>),
    /// A navigable list of items.
    List(Vec<ListEntry>),
    /// A status line: an empty target or a missing file.
    Message(String),
}

/// One row of a list view: where selecting it routes, and its display label.
struct ListEntry {
    target: Target,
    label: String,
}

/// Run the viewer. `slot` is the routing token the plugin assigned this pane;
/// `kind`/`id` are the initial item. `_port` is reserved for in-pane editing.
pub fn run(
    ws: Option<PathBuf>,
    _port: u16,
    slot: String,
    kind: Option<String>,
    id: Option<u64>,
) -> Result<()> {
    let ws = crate::todo::resolve_ws(ws)?;
    let target = kind
        .as_deref()
        .and_then(|k| Target::parse(k, id))
        .unwrap_or(Target::Empty);

    let mut viewer = Viewer::new(ws, &slot, target);
    let mut terminal = ratatui::init();
    let outcome = viewer.event_loop(&mut terminal);
    ratatui::restore();

    // The viewer is long-lived, but an explicit `q` is the user closing it;
    // close the Zellij pane too so it does not linger as a spent command pane.
    if std::env::var_os("ZELLIJ").is_some() {
        let _ = std::process::Command::new("zellij")
            .args(["action", "close-pane"])
            .status();
    }
    outcome
}

/// The running state of a viewer pane.
struct Viewer {
    /// Project root - the `.panopt/` tree and the viewstate namespace.
    ws: PathBuf,
    /// Routing file the sidebar plugin writes to re-point this pane.
    routing_path: PathBuf,
    /// Last-seen mtime of the routing file, to detect a re-point.
    routing_mtime: Option<SystemTime>,
    target: Target,
    content: Content,
    /// Last-seen mtime of the content file, to skip needless re-reads.
    content_mtime: Option<SystemTime>,
    /// First visible row, in document mode.
    scroll: u16,
    /// Selected row, in list mode.
    cursor: usize,
    /// Content-area height from the last draw, for scroll clamping.
    viewport: u16,
    last_refresh: Instant,
    needs_draw: bool,
}

impl Viewer {
    fn new(ws: PathBuf, slot: &str, target: Target) -> Viewer {
        let routing_path = ws
            .join(".panopt")
            .join(".cockpit")
            .join(format!("viewer-{slot}.json"));
        let routing_mtime = mtime(&routing_path);
        let vs = viewstate::get(&ws, &target.key());
        let mut viewer = Viewer {
            ws,
            routing_path,
            routing_mtime,
            target,
            content: Content::Message(String::new()),
            content_mtime: None,
            scroll: vs.scroll,
            cursor: vs.cursor,
            viewport: 1,
            last_refresh: Instant::now(),
            needs_draw: true,
        };
        viewer.reload_content();
        viewer
    }

    /// Draw, wait briefly for input, poll for re-points and content changes;
    /// repeat until the user quits.
    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            if self.needs_draw {
                terminal.draw(|frame| self.draw(frame))?;
                self.needs_draw = false;
            }
            if event::poll(TICK).context("polling for input")? {
                match event::read().context("reading an input event")? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if self.handle_key(key) {
                            return Ok(());
                        }
                    }
                    Event::Resize(_, _) => self.needs_draw = true,
                    _ => {}
                }
            }
            self.poll_routing();
            self.maybe_refresh();
        }
    }

    /// Handle one key press; return `true` to quit.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => return true,
            KeyCode::Char('q') => return true,
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(self.viewport.max(1) as i64)),
            KeyCode::PageDown => self.move_by(self.viewport.max(1) as i64),
            KeyCode::Home | KeyCode::Char('g') => self.move_by(i64::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_by(i64::MAX / 2),
            KeyCode::Enter => self.open_selected(),
            _ => return false,
        }
        self.needs_draw = true;
        false
    }

    /// Move the cursor (list mode) or the scroll offset (document mode) by
    /// `delta` rows, clamped to the content.
    fn move_by(&mut self, delta: i64) {
        match &self.content {
            Content::List(entries) => {
                let max = entries.len().saturating_sub(1) as i64;
                self.cursor = (self.cursor as i64 + delta).clamp(0, max.max(0)) as usize;
            }
            Content::Doc(lines) => {
                let max = (lines.len() as i64 - self.viewport as i64).max(0);
                self.scroll = (self.scroll as i64 + delta).clamp(0, max) as u16;
            }
            Content::Message(_) => {}
        }
    }

    /// In list mode, route the viewer to the selected item.
    fn open_selected(&mut self) {
        if let Content::List(entries) = &self.content {
            if let Some(entry) = entries.get(self.cursor) {
                let target = entry.target.clone();
                self.switch(target);
            }
        }
    }

    /// Re-point the viewer at `target`, persisting the outgoing item's
    /// position and restoring the incoming one's.
    fn switch(&mut self, target: Target) {
        if target == self.target {
            return;
        }
        viewstate::set(
            &self.ws,
            &self.target.key(),
            ViewState { scroll: self.scroll, cursor: self.cursor },
        );
        self.target = target;
        let vs = viewstate::get(&self.ws, &self.target.key());
        self.scroll = vs.scroll;
        self.cursor = vs.cursor;
        self.reload_content();
        self.needs_draw = true;
    }

    /// If the routing file changed, switch to the item it now names.
    fn poll_routing(&mut self) {
        let current = mtime(&self.routing_path);
        if current == self.routing_mtime {
            return;
        }
        self.routing_mtime = current;
        let Ok(text) = std::fs::read_to_string(&self.routing_path) else {
            return;
        };
        let Ok(payload) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
        let id = payload.get("id").and_then(Value::as_u64);
        if let Some(target) = Target::parse(kind, id) {
            self.switch(target);
        }
    }

    /// Re-read the content file if it changed since the last read.
    fn maybe_refresh(&mut self) {
        if self.last_refresh.elapsed() < REFRESH {
            return;
        }
        self.last_refresh = Instant::now();
        let current = self.target.content_path(&self.ws).and_then(|p| mtime(&p));
        if current != self.content_mtime {
            self.reload_content();
            self.needs_draw = true;
        }
    }

    /// Load the current target's content from the `.panopt/` projection.
    fn reload_content(&mut self) {
        let path = self.target.content_path(&self.ws);
        self.content_mtime = path.as_deref().and_then(mtime);
        self.content = match &self.target {
            Target::Empty => Content::Message("Select an item in the sidebar.".to_string()),
            Target::Todo(id) | Target::Scratchpad(id) => {
                let id = *id;
                match path.and_then(|p| std::fs::read_to_string(p).ok()) {
                    Some(text) => Content::Doc(text.lines().map(str::to_string).collect()),
                    None => Content::Message(format!("#{id} is no longer present.")),
                }
            }
            Target::TodoList => {
                Content::List(read_index(path.as_deref(), |id| Target::Todo(id)))
            }
            Target::ScratchpadList => {
                Content::List(read_index(path.as_deref(), |id| Target::Scratchpad(id)))
            }
        };
        self.clamp();
    }

    /// Keep the scroll offset and list cursor within the loaded content.
    fn clamp(&mut self) {
        match &self.content {
            Content::List(entries) => {
                let max = entries.len().saturating_sub(1);
                if self.cursor > max {
                    self.cursor = max;
                }
            }
            Content::Doc(lines) => {
                let max = lines.len() as u16;
                if self.scroll > max {
                    self.scroll = max;
                }
            }
            Content::Message(_) => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let rows = Layout::vertical([
            Constraint::Length(1), // header
            Constraint::Min(0),    // content
            Constraint::Length(1), // footer
        ])
        .split(frame.area());
        self.viewport = rows[1].height;

        frame.render_widget(
            Paragraph::new(self.header()).style(Style::default().add_modifier(Modifier::BOLD)),
            rows[0],
        );

        match &self.content {
            Content::Doc(lines) => {
                let text = Text::from(
                    lines.iter().map(|l| Line::from(l.clone())).collect::<Vec<_>>(),
                );
                frame.render_widget(
                    Paragraph::new(text)
                        .wrap(Wrap { trim: false })
                        .scroll((self.scroll, 0)),
                    rows[1],
                );
            }
            Content::List(entries) => {
                let viewport = self.viewport as usize;
                let start = list_scroll(self.cursor, viewport, entries.len());
                let lines: Vec<Line> = entries
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(viewport.max(1))
                    .map(|(i, entry)| {
                        let style = if i == self.cursor {
                            Style::default().add_modifier(Modifier::REVERSED)
                        } else {
                            Style::default()
                        };
                        Line::styled(format!(" {}", entry.label), style)
                    })
                    .collect();
                let body = if entries.is_empty() {
                    Text::from(" (empty)")
                } else {
                    Text::from(lines)
                };
                frame.render_widget(Paragraph::new(body), rows[1]);
            }
            Content::Message(msg) => {
                frame.render_widget(
                    Paragraph::new(format!(" {msg}"))
                        .style(Style::default().fg(Color::DarkGray)),
                    rows[1],
                );
            }
        }

        frame.render_widget(
            Paragraph::new(self.footer()).style(Style::default().fg(Color::Yellow)),
            rows[2],
        );
    }

    fn header(&self) -> String {
        match &self.target {
            Target::Todo(id) => format!(" Todo #{id}"),
            Target::Scratchpad(id) => format!(" Scratchpad #{id}"),
            Target::TodoList => " Todos".to_string(),
            Target::ScratchpadList => " Scratchpads".to_string(),
            Target::Empty => " PANopt viewer".to_string(),
        }
    }

    fn footer(&self) -> String {
        if self.target.is_list() {
            " j/k move   Enter open   q close".to_string()
        } else {
            " j/k scroll   g/G top/bottom   q close".to_string()
        }
    }
}

/// Read a `.panopt/` index file into list entries, mapping each id through
/// `into_target` to the target selecting it should open.
fn read_index(path: Option<&Path>, into_target: impl Fn(u64) -> Target) -> Vec<ListEntry> {
    let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(parse_index_line)
        .map(|(id, title)| ListEntry {
            target: into_target(id),
            label: format!("#{id} {title}"),
        })
        .collect()
}

/// Parse one `.panopt/` index line - `- [ ] [#3](todos/3.md) the title ...` or
/// `- [#1](scratchpad/1.md) the title` - into its id and trailing label.
fn parse_index_line(line: &str) -> Option<(u64, String)> {
    let line = line.trim();
    if !line.starts_with("- [") {
        return None;
    }
    let hash = line.find("[#")? + 2;
    let close = line[hash..].find(']')? + hash;
    let id: u64 = line[hash..close].parse().ok()?;
    let label_at = line[close..].find(") ")? + close + 2;
    let label = line.get(label_at..).unwrap_or("").trim().to_string();
    Some((id, label))
}

/// The first visible row of a list so the cursor stays on screen.
fn list_scroll(cursor: usize, viewport: usize, len: usize) -> usize {
    if viewport == 0 || len <= viewport {
        return 0;
    }
    cursor.saturating_sub(viewport / 2).min(len - viewport)
}

/// The modification time of `path`, or `None` when it cannot be read.
fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_todo_index_line() {
        let (id, label) =
            parse_index_line("- [ ] [#3](todos/3.md) wire the form - open, high").unwrap();
        assert_eq!(id, 3);
        assert_eq!(label, "wire the form - open, high");
    }

    #[test]
    fn parses_a_scratchpad_index_line() {
        let (id, label) = parse_index_line("- [#7](scratchpad/7.md) design notes").unwrap();
        assert_eq!(id, 7);
        assert_eq!(label, "design notes");
    }

    #[test]
    fn ignores_non_index_lines() {
        assert!(parse_index_line("# Todos").is_none());
        assert!(parse_index_line("_(no todos)_").is_none());
    }

    #[test]
    fn target_parse_and_key_round_trip() {
        assert_eq!(Target::parse("todo", Some(4)), Some(Target::Todo(4)));
        assert_eq!(Target::parse("scratchpad-list", None), Some(Target::ScratchpadList));
        assert_eq!(Target::parse("empty", None), Some(Target::Empty));
        assert_eq!(Target::parse("todo", None), None);
        assert_eq!(Target::Todo(4).key(), "todo:4");
        assert_eq!(Target::TodoList.key(), "list:todos");
    }

    #[test]
    fn list_scroll_keeps_the_cursor_visible() {
        assert_eq!(list_scroll(0, 5, 20), 0);
        assert_eq!(list_scroll(19, 5, 20), 15);
        assert_eq!(list_scroll(3, 10, 4), 0);
    }
}
