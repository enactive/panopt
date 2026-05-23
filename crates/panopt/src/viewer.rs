//! `panopt _viewer` - a long-lived cockpit content pane.
//!
//! Renders one item from the project's `.panopt/` projection - a list of
//! items - or, for the todo and scratchpad kinds, hosts the matching shared
//! editable form ([`crate::todo_form`] / [`crate::scratchpad_form`]). The
//! sidebar plugin re-points the pane at a different item by writing a routing
//! file the viewer polls; switching item saves the outgoing item's scroll
//! position and restores the incoming one's (see [`crate::viewstate`]).
//!
//! Index views come from the projected `.panopt/*.md` files, which the daemon
//! keeps current. Todos and scratchpads are special: the viewer opens an MCP
//! session and renders the matching form against live `todo_get` /
//! `scratchpad_get` data, autosaving on a debounce. The pane is long-lived:
//! it closes only when the user presses Ctrl-C, never on its own.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use serde_json::{json, Value};

use crate::mcpclient::Client;
use crate::scratchpad_form::ScratchpadForm;
use crate::todo::observer_url;
use crate::todo_form::TodoForm;
use crate::viewstate::{self, ViewState};

/// How often the viewer wakes to poll the routing file and refresh content.
const TICK: Duration = Duration::from_millis(250);
/// The shortest gap between re-reads of the displayed content file.
const REFRESH: Duration = Duration::from_millis(800);
/// How long an unsaved scalar-field edit waits before the viewer flushes it.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// What a viewer pane is currently showing.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    Todo(u64),
    /// A brand-new todo, not yet persisted. The form sends `todo_create` on
    /// the first autosave with a non-empty title; from then on it behaves
    /// like a `Todo(id)`.
    NewTodo,
    Scratchpad(u64),
    /// A brand-new scratchpad, not yet persisted. The form sends
    /// `scratchpad_create` on the first autosave with a non-empty title; from
    /// then on it behaves like a `Scratchpad(id)`.
    NewScratchpad,
    TodoList,
    ScratchpadList,
    Empty,
}

impl Target {
    /// Parse a routing payload's `kind`/`id` pair into a target. The `empty`
    /// kind lets the sidebar clear the viewer when nothing is selected; the
    /// `new-todo` / `new-scratchpad` kinds open blank forms.
    fn parse(kind: &str, id: Option<u64>) -> Option<Target> {
        match (kind, id) {
            ("todo", Some(id)) => Some(Target::Todo(id)),
            ("new-todo", _) => Some(Target::NewTodo),
            ("scratchpad", Some(id)) => Some(Target::Scratchpad(id)),
            ("new-scratchpad", _) => Some(Target::NewScratchpad),
            ("todo-list", _) => Some(Target::TodoList),
            ("scratchpad-list", _) => Some(Target::ScratchpadList),
            ("empty", _) => Some(Target::Empty),
            _ => None,
        }
    }

    /// The [`crate::viewstate`] key under which this target's position is kept.
    /// The form modes have no scroll position so their keys are stable but
    /// unused for restoration.
    fn key(&self) -> String {
        match self {
            Target::Todo(id) => format!("todo:{id}"),
            Target::NewTodo => "todo:new".to_string(),
            Target::Scratchpad(id) => format!("scratchpad:{id}"),
            Target::NewScratchpad => "scratchpad:new".to_string(),
            Target::TodoList => "list:todos".to_string(),
            Target::ScratchpadList => "list:scratchpads".to_string(),
            Target::Empty => "empty".to_string(),
        }
    }

    fn is_list(&self) -> bool {
        matches!(self, Target::TodoList | Target::ScratchpadList)
    }

    fn is_form(&self) -> bool {
        matches!(
            self,
            Target::Todo(_) | Target::NewTodo | Target::Scratchpad(_) | Target::NewScratchpad,
        )
    }

    /// The projected `.panopt/` file backing this target, relative to the
    /// project root. The form-backed targets render through their forms and
    /// don't read the projection, so they return `None`.
    fn content_path(&self, ws: &Path) -> Option<PathBuf> {
        let panopt = ws.join(".panopt");
        match self {
            Target::Todo(_)
            | Target::NewTodo
            | Target::Scratchpad(_)
            | Target::NewScratchpad => None,
            Target::TodoList => Some(panopt.join("todos.md")),
            Target::ScratchpadList => Some(panopt.join("scratchpads.md")),
            Target::Empty => None,
        }
    }
}

/// The loaded, render-ready content for the current target.
enum Content {
    /// A scrollable document: the lines of a projected `.md` file. No target
    /// currently renders as a `Doc` - both todos and scratchpads now go
    /// through their form views - but the variant remains so the legacy
    /// scroll/render path is one step away if a plain-doc target is added.
    #[allow(dead_code)]
    Doc(Vec<String>),
    /// A navigable list of items.
    List(Vec<ListEntry>),
    /// A status line: an empty target or a missing file.
    Message(String),
    /// The editable todo form. The viewer owns the form state but delegates
    /// rendering and key handling to it.
    TodoForm(TodoForm),
    /// The editable scratchpad form. Sibling of [`Content::TodoForm`]; see
    /// [`crate::scratchpad_form`].
    ScratchpadForm(ScratchpadForm),
}

/// One row of a list view: where selecting it routes, and its display label.
struct ListEntry {
    target: Target,
    label: String,
}

/// Run the viewer. `slot` is the routing token the plugin assigned this pane;
/// `kind`/`id` are the initial item. The MCP URL is built from `ws` + `port`
/// so the form modes can call the daemon.
pub fn run(
    ws: Option<PathBuf>,
    port: u16,
    slot: String,
    kind: Option<String>,
    id: Option<u64>,
) -> Result<()> {
    let ws = crate::todo::resolve_ws(ws)?;
    // Build the observer URL up front; every form load reuses it. An observer
    // connection cannot acquire locks, which is fine for this pass - the
    // form only displays `locked_by` from todo_get and does not call
    // todo_lock. Active lock acquisition is a follow-up.
    let url = observer_url(Some(ws.clone()), port)?;
    let target = kind
        .as_deref()
        .and_then(|k| Target::parse(k, id))
        .unwrap_or(Target::Empty);

    let mut viewer = Viewer::new(ws, url, &slot, target);
    let mut terminal = ratatui::init();
    let outcome = viewer.event_loop(&mut terminal);
    ratatui::restore();

    // The viewer is long-lived, but an explicit close is the user closing it;
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
    /// Daemon MCP URL, used by the form's MCP calls.
    url: String,
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
    fn new(ws: PathBuf, url: String, slot: &str, target: Target) -> Viewer {
        let routing_path = ws
            .join(".panopt")
            .join(".cockpit")
            .join(format!("viewer-{slot}.json"));
        let routing_mtime = mtime(&routing_path);
        let vs = viewstate::get(&ws, &target.key());
        let mut viewer = Viewer {
            ws,
            url,
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

    /// Draw, wait briefly for input, poll for re-points and content changes,
    /// flush any pending autosave; repeat until the user closes the pane.
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
            self.maybe_autosave();
        }
    }

    /// Handle one key press; return `true` to quit. In form mode the form
    /// handles most keys; Ctrl-C is reserved as the close gesture so it does
    /// not collide with typed input.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match &mut self.content {
            Content::TodoForm(form) => {
                if ctrl && matches!(key.code, KeyCode::Char('c')) {
                    // Flush any unsaved edits before the pane goes away.
                    let _ = form.flush();
                    return true;
                }
                match form.handle_key(key) {
                    crate::todo_form::TodoFormAction::Close => {
                        let _ = form.flush();
                        return true;
                    }
                    crate::todo_form::TodoFormAction::Dirty | crate::todo_form::TodoFormAction::Idle => {}
                }
                self.needs_draw = true;
                false
            }
            Content::ScratchpadForm(form) => {
                if ctrl && matches!(key.code, KeyCode::Char('c')) {
                    let _ = form.flush();
                    return true;
                }
                match form.handle_key(key) {
                    crate::scratchpad_form::ScratchpadFormAction::Close => {
                        let _ = form.flush();
                        return true;
                    }
                    crate::scratchpad_form::ScratchpadFormAction::Dirty
                    | crate::scratchpad_form::ScratchpadFormAction::Idle => {}
                }
                self.needs_draw = true;
                false
            }
            _ => {
                match key.code {
                    KeyCode::Char('c') if ctrl => return true,
                    // `q` closes only outside the form; in form mode the user
                    // can type a `q` into the title.
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
        }
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
            Content::Message(_) | Content::TodoForm(_) | Content::ScratchpadForm(_) => {}
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
    /// position, restoring the incoming one's, and flushing the form if we
    /// are leaving one.
    fn switch(&mut self, target: Target) {
        if target == self.target {
            return;
        }
        // Flush a pending form edit before it disappears.
        match &mut self.content {
            Content::TodoForm(form) => {
                let _ = form.flush();
            }
            Content::ScratchpadForm(form) => {
                let _ = form.flush();
            }
            _ => {}
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

    /// Re-read the content file if it changed since the last read. Form modes
    /// have no backing file - their refresh runs through the MCP client and
    /// is currently load-once-on-switch; see the autosave path for writes.
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

    /// If a form edit has been pending for at least [`DEBOUNCE`], flush it.
    /// Errors are surfaced in the form's message line by the form itself.
    fn maybe_autosave(&mut self) {
        match &mut self.content {
            Content::TodoForm(form) => {
                if form.dirty_since.map_or(false, |t| t.elapsed() >= DEBOUNCE) {
                    if let Err(e) = form.flush() {
                        form.message = format!("autosave failed: {e:#}");
                    }
                    self.needs_draw = true;
                }
            }
            Content::ScratchpadForm(form) => {
                if form.dirty_since.map_or(false, |t| t.elapsed() >= DEBOUNCE) {
                    if let Err(e) = form.flush() {
                        form.message = format!("autosave failed: {e:#}");
                    }
                    self.needs_draw = true;
                }
            }
            _ => {}
        }
    }

    /// Load the current target's content. For form targets this calls the
    /// daemon (`todo_get`, `scratchpad_get`) or constructs a blank form; for
    /// everything else it reads the `.panopt/` projection.
    fn reload_content(&mut self) {
        let path = self.target.content_path(&self.ws);
        self.content_mtime = path.as_deref().and_then(mtime);
        self.content = match &self.target {
            Target::Empty => Content::Message("Select an item in the sidebar.".to_string()),
            Target::NewTodo => Content::TodoForm(TodoForm::blank(&self.url)),
            Target::Todo(id) => match load_todo(&self.url, *id) {
                Ok(todo) => {
                    let url = self.url.clone();
                    let blocker_titles = |bid: u64| resolve_blocker_title(&url, bid);
                    match TodoForm::from_todo(&self.url, &todo, &blocker_titles) {
                        Ok(form) => Content::TodoForm(form),
                        Err(e) => Content::Message(format!("could not parse todo #{id}: {e:#}")),
                    }
                }
                Err(e) => Content::Message(format!("could not load todo #{id}: {e:#}")),
            },
            Target::NewScratchpad => Content::ScratchpadForm(ScratchpadForm::blank(&self.url)),
            Target::Scratchpad(id) => match load_scratchpad(&self.url, *id) {
                Ok(pad) => Content::ScratchpadForm(ScratchpadForm::from_parts(
                    &self.url,
                    *id,
                    pad["title"].as_str().unwrap_or(""),
                    pad["body"].as_str().unwrap_or(""),
                    pad["created_at"].as_str().unwrap_or(""),
                    pad["updated_at"].as_str().unwrap_or(""),
                )),
                Err(e) => Content::Message(format!("could not load scratchpad #{id}: {e:#}")),
            },
            Target::TodoList => Content::List(read_index(path.as_deref(), |id| Target::Todo(id))),
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
            Content::Message(_) | Content::TodoForm(_) | Content::ScratchpadForm(_) => {}
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        // Form modes own the entire pane - their draw renders header, fields,
        // sections, and footer themselves. Other modes use the legacy 3-row
        // layout (header / content / footer).
        if let Content::TodoForm(form) = &mut self.content {
            form.draw(frame, frame.area());
            return;
        }
        if let Content::ScratchpadForm(form) = &mut self.content {
            form.draw(frame, frame.area());
            return;
        }

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

        self.draw_body(frame, rows[1]);

        frame.render_widget(
            Paragraph::new(self.footer()).style(Style::default().fg(Color::Yellow)),
            rows[2],
        );
    }

    fn draw_body(&self, frame: &mut Frame, area: Rect) {
        match &self.content {
            Content::Doc(lines) => {
                let text =
                    Text::from(lines.iter().map(|l| Line::from(l.clone())).collect::<Vec<_>>());
                frame.render_widget(
                    Paragraph::new(text)
                        .wrap(Wrap { trim: false })
                        .scroll((self.scroll, 0)),
                    area,
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
                frame.render_widget(Paragraph::new(body), area);
            }
            Content::Message(msg) => {
                frame.render_widget(
                    Paragraph::new(format!(" {msg}"))
                        .style(Style::default().fg(Color::DarkGray)),
                    area,
                );
            }
            Content::TodoForm(_) | Content::ScratchpadForm(_) => {
                unreachable!("form modes short-circuit draw")
            }
        }
    }

    fn header(&self) -> String {
        match &self.target {
            Target::Todo(id) => format!(" Todo #{id}"),
            Target::NewTodo => " New todo".to_string(),
            Target::Scratchpad(id) => format!(" Scratchpad #{id}"),
            Target::NewScratchpad => " New scratchpad".to_string(),
            Target::TodoList => " Todos".to_string(),
            Target::ScratchpadList => " Scratchpads".to_string(),
            Target::Empty => " PANopt viewer".to_string(),
        }
    }

    fn footer(&self) -> String {
        if self.target.is_form() {
            // The form draws its own footer; the viewer-level footer is unused
            // in form mode, but we still produce a string for the type's sake.
            String::new()
        } else if self.target.is_list() {
            " j/k move   Enter open   q close".to_string()
        } else {
            " j/k scroll   g/G top/bottom   q close".to_string()
        }
    }
}

/// One-shot `todo_get` against the daemon.
fn load_todo(url: &str, id: u64) -> Result<Value> {
    let client = Client::connect(url)?;
    let outcome = client.call("todo_get", json!({ "todo_id": id }));
    client.close();
    outcome
}

/// One-shot `scratchpad_get` against the daemon. Returns the JSON object
/// `{id, title, body, created_at, updated_at}`.
fn load_scratchpad(url: &str, id: u64) -> Result<Value> {
    let client = Client::connect(url)?;
    let outcome = client.call("scratchpad_get", json!({ "scratchpad_id": id }));
    client.close();
    outcome
}

/// Resolve a blocker id's display title via a one-shot `todo_get`. Failure
/// (e.g. the blocker was deleted) yields `None` so the form falls back to
/// just the id.
fn resolve_blocker_title(url: &str, id: u64) -> Option<String> {
    let client = Client::connect(url).ok()?;
    let title = client
        .call("todo_get", json!({ "todo_id": id }))
        .ok()
        .and_then(|v| v["title"].as_str().map(str::to_string));
    client.close();
    title
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
    fn parses_a_scratchpad_index_line_with_updated_timestamp() {
        // The post-V4 index line ends in `- updated <ts>`; the parser keeps
        // that suffix as the label, which the viewer's list mode displays.
        let (id, label) = parse_index_line(
            "- [#1](scratchpad/1.md) Sample Notes - updated 2026-05-23 18:05:21",
        )
        .unwrap();
        assert_eq!(id, 1);
        assert_eq!(label, "Sample Notes - updated 2026-05-23 18:05:21");
    }

    #[test]
    fn ignores_non_index_lines() {
        assert!(parse_index_line("# Todos").is_none());
        assert!(parse_index_line("_(no todos)_").is_none());
    }

    #[test]
    fn target_parse_and_key_round_trip() {
        assert_eq!(Target::parse("todo", Some(4)), Some(Target::Todo(4)));
        assert_eq!(Target::parse("new-todo", None), Some(Target::NewTodo));
        assert_eq!(Target::parse("scratchpad-list", None), Some(Target::ScratchpadList));
        assert_eq!(Target::parse("empty", None), Some(Target::Empty));
        assert_eq!(Target::parse("todo", None), None);
        assert_eq!(Target::Todo(4).key(), "todo:4");
        assert_eq!(Target::NewTodo.key(), "todo:new");
        assert_eq!(Target::TodoList.key(), "list:todos");
    }

    #[test]
    fn list_scroll_keeps_the_cursor_visible() {
        assert_eq!(list_scroll(0, 5, 20), 0);
        assert_eq!(list_scroll(19, 5, 20), 15);
        assert_eq!(list_scroll(3, 10, 4), 0);
    }

    #[test]
    fn target_form_check_covers_todos_and_scratchpads() {
        assert!(Target::Todo(1).is_form());
        assert!(Target::NewTodo.is_form());
        assert!(Target::Scratchpad(1).is_form());
        assert!(Target::NewScratchpad.is_form());
        assert!(!Target::TodoList.is_form());
        assert!(!Target::ScratchpadList.is_form());
    }

    #[test]
    fn content_path_is_none_for_form_targets() {
        let ws = Path::new("/tmp/x");
        assert!(Target::Todo(1).content_path(ws).is_none());
        assert!(Target::NewTodo.content_path(ws).is_none());
        assert!(Target::Scratchpad(1).content_path(ws).is_none());
        assert!(Target::NewScratchpad.content_path(ws).is_none());
        assert!(Target::TodoList.content_path(ws).is_some());
        assert!(Target::ScratchpadList.content_path(ws).is_some());
    }

    #[test]
    fn parse_recognizes_new_scratchpad() {
        assert_eq!(Target::parse("new-scratchpad", None), Some(Target::NewScratchpad));
        assert_eq!(Target::parse("scratchpad", Some(7)), Some(Target::Scratchpad(7)));
    }
}
