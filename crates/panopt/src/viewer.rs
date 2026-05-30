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

use std::io::stdout;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
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

/// File the cockpit's Todos gatekeeper publishes the live right-side pane
/// count to (under the project's `.panopt/.cockpit/`). A `_viewer` reads it to
/// learn whether it is the only content pane left - the one it must not close,
/// since closing it makes Zellij re-tile and the five plugin panes stop being
/// a sidebar. Outside the cockpit the file is absent and viewers stay closable.
/// See [`Viewer::close_gesture`].
const CONTENT_COUNT_FILE: &str = "content-count";

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
            Target::Todo(_) | Target::NewTodo | Target::Scratchpad(_) | Target::NewScratchpad => {
                None
            }
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
    /// rendering and key handling to it. Boxed because `TodoForm` is large
    /// (~3.7 KB) and otherwise dominates the enum's size.
    TodoForm(Box<TodoForm>),
    /// The editable scratchpad form. Sibling of [`Content::TodoForm`]; see
    /// [`crate::scratchpad_form`]. Boxed for the same reason.
    ScratchpadForm(Box<ScratchpadForm>),
}

/// One row of a list view: where selecting it routes, its display label, and
/// the metadata the filter and sort need.
///
/// `status`, `is_blocked`, `priority`, `created_at`, and `updated_at` are only
/// populated for todo entries loaded via MCP; scratchpad entries and the
/// fallback projection-reader leave them at `None`/default, and the
/// [`TodoFilter`] / [`TodoSort`] simply never hide or reorder them.
struct ListEntry {
    target: Target,
    label: String,
    /// The todo's status token (one of "open", "in_progress", "backlog",
    /// "draft", "completed", "not_done"). `None` for non-todo entries.
    status: Option<String>,
    /// True when this todo has at least one blocker. Always false for non-todo
    /// entries.
    is_blocked: bool,
    /// The todo's priority token (one of "high", "medium", "low"). `None`
    /// for non-todo entries and for projection-fallback rows that don't
    /// carry priority.
    priority: Option<String>,
    /// SQLite-formatted timestamp ("YYYY-MM-DD HH:MM:SS"); lexicographic
    /// order matches chronological order, so the sort comparator can compare
    /// the strings directly. `None` for non-todo entries.
    created_at: Option<String>,
    /// SQLite-formatted timestamp; see [`Self::created_at`].
    updated_at: Option<String>,
}

/// What subset of todos the list view shows. Applied to a todo `ListEntry`'s
/// `status` and `is_blocked`; non-todo entries are always shown.
///
/// `OpenUnblocked` is the default because that is the working set most of the
/// time - todos ready to pick up, with no upstream work waiting. The user
/// cycles through the other variants with `f` / `F` in the list view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum TodoFilter {
    All,
    Open,
    #[default]
    OpenUnblocked,
    InProgress,
    Backlog,
    Draft,
    Completed,
    NotDone,
}

impl TodoFilter {
    /// The wire-style label shown in the header.
    fn label(self) -> &'static str {
        match self {
            TodoFilter::All => "all",
            TodoFilter::Open => "open",
            TodoFilter::OpenUnblocked => "open-unblocked",
            TodoFilter::InProgress => "in_progress",
            TodoFilter::Backlog => "backlog",
            TodoFilter::Draft => "draft",
            TodoFilter::Completed => "completed",
            TodoFilter::NotDone => "not_done",
        }
    }

    /// The serialised form used by viewstate, identical to [`Self::label`].
    fn as_key(self) -> &'static str {
        self.label()
    }

    /// Parse a viewstate-stored token, defaulting to [`TodoFilter::default`]
    /// on an unrecognised value (a forward-compatibility hedge for stored
    /// filters whose meaning has since shifted).
    fn parse(s: &str) -> TodoFilter {
        ALL_FILTERS
            .iter()
            .copied()
            .find(|f| f.as_key() == s)
            .unwrap_or_default()
    }

    /// Next filter in the cycle, wrapping at the end.
    fn next(self) -> TodoFilter {
        let i = ALL_FILTERS.iter().position(|f| *f == self).unwrap_or(0);
        ALL_FILTERS[(i + 1) % ALL_FILTERS.len()]
    }

    /// Previous filter in the cycle, wrapping at the start.
    fn prev(self) -> TodoFilter {
        let i = ALL_FILTERS.iter().position(|f| *f == self).unwrap_or(0);
        ALL_FILTERS[(i + ALL_FILTERS.len() - 1) % ALL_FILTERS.len()]
    }

    /// Whether `entry` passes this filter. Non-todo entries (no `status`)
    /// always pass; this keeps the same filter knob safe to leave on when the
    /// pane switches to a non-todo list.
    fn includes(self, entry: &ListEntry) -> bool {
        let Some(status) = entry.status.as_deref() else {
            return true;
        };
        match self {
            TodoFilter::All => true,
            TodoFilter::Open => status == "open",
            TodoFilter::OpenUnblocked => status == "open" && !entry.is_blocked,
            TodoFilter::InProgress => status == "in_progress",
            TodoFilter::Backlog => status == "backlog",
            TodoFilter::Draft => status == "draft",
            TodoFilter::Completed => status == "completed",
            TodoFilter::NotDone => status == "not_done",
        }
    }
}

/// Every [`TodoFilter`] variant in cycle order, used by the `f` / `F` keys
/// and by [`TodoFilter::parse`].
const ALL_FILTERS: [TodoFilter; 8] = [
    TodoFilter::All,
    TodoFilter::Open,
    TodoFilter::OpenUnblocked,
    TodoFilter::InProgress,
    TodoFilter::Backlog,
    TodoFilter::Draft,
    TodoFilter::Completed,
    TodoFilter::NotDone,
];

/// One axis of the two-level todo sort. The list view carries two of these
/// (level 1 / level 2) and applies them as a stable two-pass sort, so equal
/// keys on level 1 are broken by level 2. Each axis maps a [`ListEntry`] to
/// an `Ordering` via [`Self::cmp_entries`].
///
/// `PriorityDesc` is the only priority axis - low → high is rarely useful.
/// Created and modified date each get both directions because either makes
/// sense depending on what the user is hunting for (newest activity vs.
/// oldest unfinished).
/// Level 1's default is `PriorityDesc`; level 2 defaults to
/// [`TodoSort::CreatedAsc`] in [`Viewer::new`] - the two levels have
/// different defaults, so the `Default` value alone doesn't tell the whole
/// story.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum TodoSort {
    #[default]
    PriorityDesc,
    CreatedAsc,
    CreatedDesc,
    ModifiedAsc,
    ModifiedDesc,
}

impl TodoSort {
    /// Display label. `/` indicates ascending (low → high), `\` indicates
    /// descending (high → low); priority is single-direction (high → low)
    /// so it carries no suffix. Also the viewstate-stored token via
    /// [`Self::as_key`] - the display form doubles as the persistence
    /// form, so a label tweak invalidates any persisted prefs and falls
    /// back to the default through [`Self::parse`].
    fn label(self) -> &'static str {
        match self {
            TodoSort::PriorityDesc => "priority",
            TodoSort::CreatedAsc => "created-/",
            TodoSort::CreatedDesc => "created-\\",
            TodoSort::ModifiedAsc => "modified-/",
            TodoSort::ModifiedDesc => "modified-\\",
        }
    }

    /// The serialised form used by viewstate, identical to [`Self::label`].
    fn as_key(self) -> &'static str {
        self.label()
    }

    /// Parse a viewstate-stored token, defaulting to [`TodoSort::default`]
    /// on an unrecognised value.
    fn parse(s: &str) -> TodoSort {
        ALL_SORTS
            .iter()
            .copied()
            .find(|x| x.as_key() == s)
            .unwrap_or_default()
    }

    /// Next sort in the cycle, wrapping at the end.
    fn next(self) -> TodoSort {
        let i = ALL_SORTS.iter().position(|x| *x == self).unwrap_or(0);
        ALL_SORTS[(i + 1) % ALL_SORTS.len()]
    }

    /// Previous sort in the cycle, wrapping at the start.
    fn prev(self) -> TodoSort {
        let i = ALL_SORTS.iter().position(|x| *x == self).unwrap_or(0);
        ALL_SORTS[(i + ALL_SORTS.len() - 1) % ALL_SORTS.len()]
    }

    /// Compare two entries on this axis. Entries that lack the field this
    /// axis reads sort after entries that have it (so missing data lands at
    /// the end of the list rather than mixed in).
    fn cmp_entries(self, a: &ListEntry, b: &ListEntry) -> std::cmp::Ordering {
        match self {
            TodoSort::PriorityDesc => {
                let rank = |p: &Option<String>| match p.as_deref() {
                    Some("high") => 3,
                    Some("medium") => 2,
                    Some("low") => 1,
                    _ => 0,
                };
                rank(&b.priority).cmp(&rank(&a.priority))
            }
            TodoSort::CreatedAsc => cmp_opt_str(&a.created_at, &b.created_at, true),
            TodoSort::CreatedDesc => cmp_opt_str(&a.created_at, &b.created_at, false),
            TodoSort::ModifiedAsc => cmp_opt_str(&a.updated_at, &b.updated_at, true),
            TodoSort::ModifiedDesc => cmp_opt_str(&a.updated_at, &b.updated_at, false),
        }
    }
}

/// Compare two `Option<String>` slots. `Some` always orders before `None`
/// (data first, missing data sinks to the end) regardless of direction; the
/// `ascending` flag only affects the `Some`/`Some` case.
fn cmp_opt_str(a: &Option<String>, b: &Option<String>, ascending: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Some(x), Some(y)) => {
            if ascending {
                x.cmp(y)
            } else {
                y.cmp(x)
            }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Every [`TodoSort`] variant in cycle order, used by the `s` / `S` and
/// `d` / `D` keys and by [`TodoSort::parse`].
const ALL_SORTS: [TodoSort; 5] = [
    TodoSort::PriorityDesc,
    TodoSort::CreatedAsc,
    TodoSort::CreatedDesc,
    TodoSort::ModifiedAsc,
    TodoSort::ModifiedDesc,
];

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
    // Bracketed paste turns a clipboard paste into a single `Event::Paste(s)`
    // instead of a stream of synthetic key events. Without this, multi-line
    // pastes arrive as a series of `Ctrl-J` events (the raw `\n` byte in raw
    // mode), which `tui_textarea` interprets as `delete_line_by_head` - the
    // pasted text scrubs itself across the field as each line break fires the
    // shortcut. Failing to enable is non-fatal: typed input still works.
    // Mouse capture lets the viewer receive clicks on list rows so the user
    // can open todos and scratchpads by clicking, matching the sidebar's
    // click-to-activate behaviour. Failing to enable is non-fatal: keyboard
    // navigation still works.
    let _ = execute!(stdout(), EnableBracketedPaste, EnableMouseCapture);
    let outcome = viewer.event_loop(&mut terminal);
    let _ = execute!(stdout(), DisableBracketedPaste, DisableMouseCapture);
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
    /// Active status filter for the todo list. Restored from viewstate when
    /// the pane points at `Target::TodoList`; for other targets the field is
    /// kept around but ignored, so leaving the list and coming back lands on
    /// the same filter the user last set.
    todo_filter: TodoFilter,
    /// Primary sort axis for the todo list. Restored from viewstate with the
    /// same scoping as [`Self::todo_filter`]. Default is
    /// [`TodoSort::PriorityDesc`].
    todo_sort_1: TodoSort,
    /// Secondary sort axis, used as a stable tiebreaker for level 1.
    /// Default is [`TodoSort::CreatedAsc`] (oldest first), which together
    /// with the priority-desc level 1 surfaces the highest-priority
    /// oldest-open todo at the top - the most likely thing to pick up next.
    todo_sort_2: TodoSort,
    last_refresh: Instant,
    needs_draw: bool,
    /// True when this viewer is the only right-side content pane left, per the
    /// count the cockpit gatekeeper publishes. Refreshed each tick; gates
    /// [`Viewer::close_gesture`] so the last pane cannot be closed out from
    /// under the sidebar. Always false outside the cockpit.
    sole_content_pane: bool,
}

impl Viewer {
    fn new(ws: PathBuf, url: String, slot: &str, target: Target) -> Viewer {
        let routing_path = ws
            .join(".panopt")
            .join(".cockpit")
            .join(format!("viewer-{slot}.json"));
        let routing_mtime = mtime(&routing_path);
        let vs = viewstate::get(&ws, &target.key());
        let todo_filter = vs
            .extras
            .get("todo_filter")
            .and_then(Value::as_str)
            .map(TodoFilter::parse)
            .unwrap_or_default();
        let todo_sort_1 = vs
            .extras
            .get("todo_sort_1")
            .and_then(Value::as_str)
            .map(TodoSort::parse)
            .unwrap_or(TodoSort::PriorityDesc);
        let todo_sort_2 = vs
            .extras
            .get("todo_sort_2")
            .and_then(Value::as_str)
            .map(TodoSort::parse)
            .unwrap_or(TodoSort::CreatedAsc);
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
            todo_filter,
            todo_sort_1,
            todo_sort_2,
            last_refresh: Instant::now(),
            needs_draw: true,
            sole_content_pane: false,
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
                    Event::Key(key) if key.kind == KeyEventKind::Press && self.handle_key(key) => {
                        return Ok(());
                    }
                    Event::Paste(s) => self.handle_paste(&s),
                    Event::Mouse(m) => self.handle_mouse(m),
                    Event::Resize(_, _) => self.needs_draw = true,
                    _ => {}
                }
            }
            self.poll_routing();
            self.maybe_refresh();
            self.maybe_autosave();
            self.refresh_sole_content();
        }
    }

    /// Re-read the gatekeeper's published right-side pane count and cache
    /// whether this viewer is now the only one left. Only meaningful inside
    /// the cockpit: a stand-alone `panopt _viewer` runs outside Zellij with no
    /// gatekeeper publishing the count, so it must always stay closable.
    fn refresh_sole_content(&mut self) {
        self.sole_content_pane = std::env::var_os("ZELLIJ").is_some()
            && read_content_count(&self.ws).is_some_and(|n| n <= 1);
    }

    /// Handle one key press; return `true` to quit. In form mode the form
    /// handles most keys; Ctrl-C is reserved as the close gesture so it does
    /// not collide with typed input.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Each arm yields `true` to mean "the user asked to close". The actual
        // close is deferred to `close_gesture` after the match so it runs once
        // the `&mut self.content` borrow the form arms hold has been released.
        let close = match &mut self.content {
            Content::TodoForm(form) => {
                if ctrl && matches!(key.code, KeyCode::Char('c')) {
                    // Flush any unsaved edits before the pane goes away.
                    let _ = form.flush();
                    true
                } else {
                    match form.handle_key(key) {
                        crate::todo_form::TodoFormAction::Close => {
                            let _ = form.flush();
                            true
                        }
                        crate::todo_form::TodoFormAction::Dirty
                        | crate::todo_form::TodoFormAction::Idle => {
                            self.needs_draw = true;
                            false
                        }
                    }
                }
            }
            Content::ScratchpadForm(form) => {
                if ctrl && matches!(key.code, KeyCode::Char('c')) {
                    let _ = form.flush();
                    true
                } else {
                    match form.handle_key(key) {
                        crate::scratchpad_form::ScratchpadFormAction::Close => {
                            let _ = form.flush();
                            true
                        }
                        crate::scratchpad_form::ScratchpadFormAction::Dirty
                        | crate::scratchpad_form::ScratchpadFormAction::Idle => {
                            self.needs_draw = true;
                            false
                        }
                    }
                }
            }
            _ => {
                match key.code {
                    KeyCode::Char('c') if ctrl => return self.close_gesture(),
                    // `q` closes only outside the form; in form mode the user
                    // can type a `q` into the title.
                    KeyCode::Char('q') => return self.close_gesture(),
                    KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
                    KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
                    KeyCode::PageUp => self.move_by(-(self.viewport.max(1) as i64)),
                    KeyCode::PageDown => self.move_by(self.viewport.max(1) as i64),
                    KeyCode::Home | KeyCode::Char('g') => self.move_by(i64::MIN / 2),
                    KeyCode::End | KeyCode::Char('G') => self.move_by(i64::MAX / 2),
                    KeyCode::Enter => self.open_selected(),
                    // Status filter for the todo list. Lowercase `f` cycles
                    // forward, capital `F` cycles back; both are no-ops for
                    // other targets so the binding does not collide there.
                    KeyCode::Char('f') => self.cycle_filter(true),
                    KeyCode::Char('F') => self.cycle_filter(false),
                    // Two-level sort: `s`/`S` cycles level 1, `d`/`D` cycles
                    // level 2. Same target-scoping as the filter keys above.
                    KeyCode::Char('s') => self.cycle_sort(1, true),
                    KeyCode::Char('S') => self.cycle_sort(1, false),
                    KeyCode::Char('d') => self.cycle_sort(2, true),
                    KeyCode::Char('D') => self.cycle_sort(2, false),
                    _ => return false,
                }
                self.needs_draw = true;
                false
            }
        };
        if close {
            self.close_gesture()
        } else {
            false
        }
    }

    /// The user's close gesture - Ctrl-C, or `q`/the form's own close outside
    /// it. Returns `true` to quit the event loop (the process exits and the
    /// pane closes). When this is the only right-side content pane left,
    /// closing it would make Zellij re-tile and the sidebar plugin panes stop
    /// being a sidebar - so it never quits: it clears the pane back to `Empty`
    /// (any form edits already flushed by the caller) and keeps the process,
    /// and the layout, alive. With another content pane present it quits,
    /// closing its pane as before.
    fn close_gesture(&mut self) -> bool {
        if self.sole_content_pane {
            self.target = Target::Empty;
            self.reload_content();
            self.needs_draw = true;
            false
        } else {
            true
        }
    }

    /// Handle a bracketed-paste payload. Only the form modes can receive a
    /// paste; the list / doc views ignore it.
    fn handle_paste(&mut self, s: &str) {
        match &mut self.content {
            Content::TodoForm(form) => {
                let _ = form.handle_paste(s);
                self.needs_draw = true;
            }
            Content::ScratchpadForm(form) => {
                let _ = form.handle_paste(s);
                self.needs_draw = true;
            }
            _ => {}
        }
    }

    /// Handle a mouse event. In list mode a left-click on a row both selects
    /// and opens it, matching the sidebar's click-to-activate behaviour;
    /// scroll wheel moves the cursor or scroll offset by one. In form mode
    /// a left-click is forwarded to the form so click-to-position-cursor in
    /// the body field works; other mouse events on forms are dropped
    /// (drag-to-select and OSC52 copy land in later sub-pieces of #95).
    fn handle_mouse(&mut self, m: MouseEvent) {
        match &mut self.content {
            Content::TodoForm(form) => {
                let _ = form.handle_mouse(m);
                self.needs_draw = true;
            }
            Content::ScratchpadForm(form) => {
                let _ = form.handle_mouse(m);
                self.needs_draw = true;
            }
            _ => match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(idx) = self.list_row_at(m.row) {
                        self.cursor = idx;
                        self.open_selected();
                        self.needs_draw = true;
                    }
                }
                MouseEventKind::ScrollUp => {
                    if matches!(self.content, Content::List(_) | Content::Doc(_)) {
                        self.move_by(-1);
                        self.needs_draw = true;
                    }
                }
                MouseEventKind::ScrollDown => {
                    if matches!(self.content, Content::List(_) | Content::Doc(_)) {
                        self.move_by(1);
                        self.needs_draw = true;
                    }
                }
                _ => {}
            },
        }
    }

    /// The filtered list cursor's position in terminal row `row`, or `None`
    /// when the row falls on the header, the footer, or past the last visible
    /// entry. The body always starts at row 1 (right after the 1-row header)
    /// and is `viewport` rows tall.
    fn list_row_at(&self, row: u16) -> Option<usize> {
        if !matches!(self.content, Content::List(_)) {
            return None;
        }
        if row == 0 || row > self.viewport {
            return None;
        }
        let visible_len = self.visible_indices().len();
        let body_row = (row - 1) as usize;
        let start = list_scroll(self.cursor, self.viewport as usize, visible_len);
        let idx = start + body_row;
        (idx < visible_len).then_some(idx)
    }

    /// Move the cursor (list mode) or the scroll offset (document mode) by
    /// `delta` rows, clamped to the content. The list cursor is in *visible*
    /// space - it skips entries the filter hides.
    fn move_by(&mut self, delta: i64) {
        match &self.content {
            Content::List(_) => {
                let max = self.visible_indices().len().saturating_sub(1) as i64;
                self.cursor = (self.cursor as i64 + delta).clamp(0, max.max(0)) as usize;
            }
            Content::Doc(lines) => {
                let max = (lines.len() as i64 - self.viewport as i64).max(0);
                self.scroll = (self.scroll as i64 + delta).clamp(0, max) as u16;
            }
            Content::Message(_) | Content::TodoForm(_) | Content::ScratchpadForm(_) => {}
        }
    }

    /// In list mode, route the viewer to the selected (visible) item.
    fn open_selected(&mut self) {
        if let Content::List(entries) = &self.content {
            let visible = self.visible_indices();
            if let Some(&idx) = visible.get(self.cursor) {
                if let Some(entry) = entries.get(idx) {
                    let target = entry.target.clone();
                    self.switch(target);
                }
            }
        }
    }

    /// The indices of `Content::List` entries that pass the current filter,
    /// in display order. For [`Target::TodoList`] the indices are reordered
    /// by [`Self::todo_sort_1`] then [`Self::todo_sort_2`] (a stable sort,
    /// so equal level-1 keys keep their level-2 ordering). For other targets
    /// the filter is the identity and no sort is applied, so this returns
    /// every index in source order.
    fn visible_indices(&self) -> Vec<usize> {
        let Content::List(entries) = &self.content else {
            return Vec::new();
        };
        let is_todo_list = matches!(self.target, Target::TodoList);
        let mut visible: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                if !is_todo_list || self.todo_filter.includes(e) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        if is_todo_list {
            // Stable sort by level 2 first, then by level 1: ties on level 1
            // preserve the level-2 ordering established by the first pass.
            visible.sort_by(|&i, &j| self.todo_sort_2.cmp_entries(&entries[i], &entries[j]));
            visible.sort_by(|&i, &j| self.todo_sort_1.cmp_entries(&entries[i], &entries[j]));
        }
        visible
    }

    /// Cycle the todo filter forward (`f`) or backward (`F`). No-op when the
    /// active target isn't the todo list.
    fn cycle_filter(&mut self, forward: bool) {
        if !matches!(self.target, Target::TodoList) {
            return;
        }
        self.todo_filter = if forward {
            self.todo_filter.next()
        } else {
            self.todo_filter.prev()
        };
        // The visible-entry set just changed; keep the cursor on the new
        // visible range and persist the choice for next time.
        let visible_len = self.visible_indices().len();
        if self.cursor >= visible_len {
            self.cursor = visible_len.saturating_sub(1);
        }
        self.persist_viewstate();
        self.needs_draw = true;
    }

    /// Cycle one sort level forward or backward. `level` is 1 (`s` / `S`)
    /// or 2 (`d` / `D`); any other value is a no-op. No-op when the active
    /// target isn't the todo list. Reordering does not change which entries
    /// are visible, but the selected row may now point at a different todo;
    /// reset the cursor to the top so the user sees the new ordering from
    /// its head rather than scrolled into the middle.
    fn cycle_sort(&mut self, level: u8, forward: bool) {
        if !matches!(self.target, Target::TodoList) {
            return;
        }
        let slot = match level {
            1 => &mut self.todo_sort_1,
            2 => &mut self.todo_sort_2,
            _ => return,
        };
        *slot = if forward { slot.next() } else { slot.prev() };
        self.cursor = 0;
        self.persist_viewstate();
        self.needs_draw = true;
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
        self.persist_viewstate();
        self.target = target;
        let vs = viewstate::get(&self.ws, &self.target.key());
        self.scroll = vs.scroll;
        self.cursor = vs.cursor;
        self.todo_filter = vs
            .extras
            .get("todo_filter")
            .and_then(Value::as_str)
            .map(TodoFilter::parse)
            .unwrap_or_default();
        self.todo_sort_1 = vs
            .extras
            .get("todo_sort_1")
            .and_then(Value::as_str)
            .map(TodoSort::parse)
            .unwrap_or(TodoSort::PriorityDesc);
        self.todo_sort_2 = vs
            .extras
            .get("todo_sort_2")
            .and_then(Value::as_str)
            .map(TodoSort::parse)
            .unwrap_or(TodoSort::CreatedAsc);
        self.reload_content();
        self.needs_draw = true;
    }

    /// Persist the current target's scroll, cursor, filter, and sort so
    /// reopening the same item or list lands on the same row and view.
    fn persist_viewstate(&self) {
        let mut extras = serde_json::Map::new();
        if matches!(self.target, Target::TodoList) {
            extras.insert(
                "todo_filter".into(),
                Value::String(self.todo_filter.as_key().to_string()),
            );
            extras.insert(
                "todo_sort_1".into(),
                Value::String(self.todo_sort_1.as_key().to_string()),
            );
            extras.insert(
                "todo_sort_2".into(),
                Value::String(self.todo_sort_2.as_key().to_string()),
            );
        }
        viewstate::set(
            &self.ws,
            &self.target.key(),
            ViewState {
                scroll: self.scroll,
                cursor: self.cursor,
                extras,
            },
        );
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

    /// Re-read the content file if it changed since the last read. Projection-
    /// backed targets compare mtimes; form-backed targets (which have no
    /// projection file) call `refresh_from_daemon` on the form itself, which
    /// pulls a fresh `todo_get` snapshot, replays the local Baseline-diff
    /// rules to keep pending edits intact, and updates the Baseline so the
    /// next [`Self::maybe_autosave`] flushes only what is still divergent.
    fn maybe_refresh(&mut self) {
        if self.last_refresh.elapsed() < REFRESH {
            return;
        }
        self.last_refresh = Instant::now();
        if let Some(path) = self.target.content_path(&self.ws) {
            let current = mtime(&path);
            if current != self.content_mtime {
                self.reload_content();
                self.needs_draw = true;
            }
            return;
        }
        // No projection file - form targets refresh through the daemon.
        match &mut self.content {
            Content::TodoForm(form) => match form.refresh_from_daemon() {
                Ok(true) => self.needs_draw = true,
                Ok(false) => {}
                Err(e) => {
                    form.message = format!("refresh failed: {e:#}");
                    self.needs_draw = true;
                }
            },
            Content::ScratchpadForm(form) => match form.refresh_from_daemon() {
                Ok(true) => self.needs_draw = true,
                Ok(false) => {}
                Err(e) => {
                    form.message = format!("refresh failed: {e:#}");
                    self.needs_draw = true;
                }
            },
            _ => {}
        }
    }

    /// If a form edit has been pending for at least [`DEBOUNCE`], flush it.
    /// Errors are surfaced in the form's message line by the form itself.
    fn maybe_autosave(&mut self) {
        let promote = match &mut self.content {
            Content::TodoForm(form)
                if form.dirty_since.is_some_and(|t| t.elapsed() >= DEBOUNCE) =>
            {
                if let Err(e) = form.flush() {
                    form.message = format!("autosave failed: {e:#}");
                }
                self.needs_draw = true;
                promotion_target(&self.target, form.id)
            }
            Content::ScratchpadForm(form)
                if form.dirty_since.is_some_and(|t| t.elapsed() >= DEBOUNCE) =>
            {
                if let Err(e) = form.flush() {
                    form.message = format!("autosave failed: {e:#}");
                }
                self.needs_draw = true;
                promotion_target(&self.target, form.id)
            }
            _ => None,
        };
        if let Some(target) = promote {
            self.promote_form_target(target);
        }
    }

    /// Once a new-todo / new-scratchpad form's first save assigns an id,
    /// rewrite the routing file so the sidebar plugin re-titles the pane from
    /// "New todo" to "Todo #N - ...". The form content is already in sync with
    /// the daemon's view, so this only updates the pane title surface - no
    /// reload, no switch (which would clobber the form state).
    fn promote_form_target(&mut self, target: Target) {
        let (kind, id) = match &target {
            Target::Todo(id) => ("todo", *id),
            Target::Scratchpad(id) => ("scratchpad", *id),
            _ => return,
        };
        write_routing_to(&self.routing_path, kind, Some(id));
        // Refresh the mtime we already saw so the next poll_routing doesn't
        // treat our own write as an external re-point.
        self.routing_mtime = mtime(&self.routing_path);
        self.target = target;
    }

    /// Load the current target's content. For form targets this calls the
    /// daemon (`todo_get`, `scratchpad_get`) or constructs a blank form; for
    /// everything else it reads the `.panopt/` projection.
    fn reload_content(&mut self) {
        let path = self.target.content_path(&self.ws);
        self.content_mtime = path.as_deref().and_then(mtime);
        self.content = match &self.target {
            Target::Empty => Content::Message("Select an item in the sidebar.".to_string()),
            Target::NewTodo => Content::TodoForm(Box::new(TodoForm::blank(&self.url))),
            Target::Todo(id) => match load_todo(&self.url, *id) {
                Ok(todo) => {
                    let url = self.url.clone();
                    let blocker_titles = |bid: u64| resolve_blocker_title(&url, bid);
                    match TodoForm::from_todo(&self.url, &todo, &blocker_titles) {
                        Ok(form) => Content::TodoForm(Box::new(form)),
                        Err(e) => Content::Message(format!("could not parse todo #{id}: {e:#}")),
                    }
                }
                Err(e) => Content::Message(format!("could not load todo #{id}: {e:#}")),
            },
            Target::NewScratchpad => {
                Content::ScratchpadForm(Box::new(ScratchpadForm::blank(&self.url)))
            }
            Target::Scratchpad(id) => match load_scratchpad(&self.url, *id) {
                Ok(pad) => {
                    let tags: Vec<String> = pad["tags"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    Content::ScratchpadForm(Box::new(ScratchpadForm::from_parts(
                        &self.url,
                        *id,
                        pad["title"].as_str().unwrap_or(""),
                        pad["body"].as_str().unwrap_or(""),
                        &tags,
                        pad["created_at"].as_str().unwrap_or(""),
                        pad["updated_at"].as_str().unwrap_or(""),
                    )))
                }
                Err(e) => Content::Message(format!("could not load scratchpad #{id}: {e:#}")),
            },
            Target::TodoList => match load_todo_list(&self.url) {
                Ok(entries) => Content::List(entries),
                // Fall back to the projection: it carries less detail (no
                // blocker info), but it keeps the pane useful when the
                // daemon is unreachable. The `open-unblocked` filter is
                // approximated as `open` in this mode.
                Err(_) => Content::List(read_todo_index(path.as_deref())),
            },
            Target::ScratchpadList => {
                Content::List(read_index(path.as_deref(), Target::Scratchpad))
            }
        };
        self.clamp();
    }

    /// Keep the scroll offset and list cursor within the loaded content.
    /// The list cursor is in *visible* (post-filter) space, so an empty
    /// filtered view collapses it to 0.
    fn clamp(&mut self) {
        match &self.content {
            Content::List(_) => {
                let max = self.visible_indices().len().saturating_sub(1);
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
                let text = Text::from(
                    lines
                        .iter()
                        .map(|l| Line::from(l.clone()))
                        .collect::<Vec<_>>(),
                );
                frame.render_widget(
                    Paragraph::new(text)
                        .wrap(Wrap { trim: false })
                        .scroll((self.scroll, 0)),
                    area,
                );
            }
            Content::List(entries) => {
                let visible = self.visible_indices();
                let viewport = self.viewport as usize;
                let start = list_scroll(self.cursor, viewport, visible.len());
                let lines: Vec<Line> = visible
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(viewport.max(1))
                    .filter_map(|(visible_i, &raw_i)| {
                        let entry = entries.get(raw_i)?;
                        let style = if visible_i == self.cursor {
                            Style::default().add_modifier(Modifier::REVERSED)
                        } else {
                            Style::default()
                        };
                        Some(Line::styled(format!(" {}", entry.label), style))
                    })
                    .collect();
                let body = if visible.is_empty() {
                    Text::from(" (empty)")
                } else {
                    Text::from(lines)
                };
                frame.render_widget(Paragraph::new(body), area);
            }
            Content::Message(msg) => {
                frame.render_widget(
                    Paragraph::new(format!(" {msg}")).style(Style::default().fg(Color::DarkGray)),
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
            Target::TodoList => {
                let (visible, total) = self.list_counts();
                format!(
                    " Todos  ·  {}  ·  {}/{}  ({visible}/{total})",
                    self.todo_filter.label(),
                    self.todo_sort_1.label(),
                    self.todo_sort_2.label(),
                )
            }
            Target::ScratchpadList => " Scratchpads".to_string(),
            Target::Empty => " PANopt viewer".to_string(),
        }
    }

    /// `(visible_count, total_count)` for the current list, or `(0, 0)` when
    /// the content isn't a list.
    fn list_counts(&self) -> (usize, usize) {
        let Content::List(entries) = &self.content else {
            return (0, 0);
        };
        (self.visible_indices().len(), entries.len())
    }

    fn footer(&self) -> String {
        if self.target.is_form() {
            // The form draws its own footer; the viewer-level footer is unused
            // in form mode, but we still produce a string for the type's sake.
            String::new()
        } else if matches!(self.target, Target::TodoList) {
            " j/k move   Enter open   f/F filter   s/S sort1   d/D sort2   q close".to_string()
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
/// `into_target` to the target selecting it should open. Used for the
/// scratchpad list, which has no status/blocker filter to honour.
fn read_index(path: Option<&Path>, into_target: impl Fn(u64) -> Target) -> Vec<ListEntry> {
    let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(parse_index_line)
        .map(|(id, title)| ListEntry {
            target: into_target(id),
            label: format!("#{id} {title}"),
            status: None,
            is_blocked: false,
            priority: None,
            created_at: None,
            updated_at: None,
        })
        .collect()
}

/// Fallback projection reader for the todo list, used when the MCP call to
/// `todo_list` fails. Each line carries the status token (parsed out of the
/// `- <status>, <priority>` suffix the projection writes), but blocker info
/// is absent here - the projection does not record blockers per-line. With
/// no blocker info, `OpenUnblocked` degrades to "open" (we never hide a
/// todo whose blocker state we cannot determine).
fn read_todo_index(path: Option<&Path>) -> Vec<ListEntry> {
    let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(parse_index_line)
        .map(|(id, label)| {
            let status = parse_status_suffix(&label);
            ListEntry {
                target: Target::Todo(id),
                label: format!("#{id} {label}"),
                status,
                is_blocked: false,
                // Projection fallback path: priority and timestamps aren't
                // recorded per row in the index. Sort axes that read these
                // fields will treat all entries as missing-data and leave
                // them in id order from the projection.
                priority: None,
                created_at: None,
                updated_at: None,
            }
        })
        .collect()
}

/// Pull the wire status token out of a projection label like
/// `wire the form - open, high`. Returns `None` when the suffix is missing
/// or doesn't match a known token, so the entry simply isn't filtered by
/// status (it remains visible under every filter, including `All`).
fn parse_status_suffix(label: &str) -> Option<String> {
    let dash = label.rfind(" - ")?;
    let rest = &label[dash + 3..];
    let comma = rest.find(',').unwrap_or(rest.len());
    let token = rest[..comma].trim();
    matches!(
        token,
        "open" | "in_progress" | "backlog" | "draft" | "completed" | "not_done"
    )
    .then(|| token.to_string())
}

/// One-shot `todo_list` against the daemon, returning enriched list entries
/// that the filter can act on. Each entry's display label matches the
/// projection's `#<id> <title> - <status>, <priority>` shape so switching
/// between MCP-backed and file-backed loading is visually seamless.
fn load_todo_list(url: &str) -> Result<Vec<ListEntry>> {
    let client = Client::connect(url)?;
    let outcome = client.call("todo_list", json!({}));
    client.close();
    let arr = outcome?.as_array().cloned().unwrap_or_default();
    Ok(arr
        .iter()
        .filter_map(|t| {
            let id = t["id"].as_u64()?;
            let title = t["title"].as_str().unwrap_or("");
            let status = t["status"].as_str().unwrap_or("open").to_string();
            let priority = t["priority"].as_str().unwrap_or("medium").to_string();
            let blockers = t["blockers"].as_array().map(|a| a.len()).unwrap_or(0);
            let created_at = t["created_at"].as_str().map(str::to_string);
            let updated_at = t["updated_at"].as_str().map(str::to_string);
            Some(ListEntry {
                target: Target::Todo(id),
                label: format!("#{id} {title} - {status}, {priority}"),
                status: Some(status),
                is_blocked: blockers > 0,
                priority: Some(priority),
                created_at,
                updated_at,
            })
        })
        .collect())
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

/// Read the right-side pane count the cockpit gatekeeper publishes under
/// `.panopt/.cockpit/`. `None` when the file is absent (no cockpit) or
/// unparseable, which callers treat as "not the sole pane" - i.e. closable.
fn read_content_count(ws: &Path) -> Option<usize> {
    let path = ws.join(".panopt").join(".cockpit").join(CONTENT_COUNT_FILE);
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Decide whether the just-flushed form has earned a kind transition. Pure
/// function: takes the viewer's current [`Target`] and the form's id after
/// flush, returns `Some(Target::Todo(id) | Target::Scratchpad(id))` only when
/// a `new-*` target now has an id. Everything else (the form was already
/// promoted, the flush didn't assign an id, or we're not on a form target)
/// returns `None`. Kept separate from [`Viewer::maybe_autosave`] so the
/// scratchpad/todo symmetry can be exercised in unit tests.
fn promotion_target(target: &Target, form_id: Option<u64>) -> Option<Target> {
    match (target, form_id) {
        (Target::NewTodo, Some(id)) => Some(Target::Todo(id)),
        (Target::NewScratchpad, Some(id)) => Some(Target::Scratchpad(id)),
        _ => None,
    }
}

/// Atomically write a viewer routing file. Mirrors the sidebar plugin's
/// `write_routing` so both writers produce a byte-identical payload; the
/// viewer reaches this only on the new-todo / new-scratchpad promotion path
/// to update its own pane title without going through the plugin.
fn write_routing_to(path: &Path, kind: &str, id: Option<u64>) {
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let payload = match id {
        Some(id) => format!("{{\"kind\":\"{kind}\",\"id\":{id}}}"),
        None => format!("{{\"kind\":\"{kind}\"}}"),
    };
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("viewer");
    let tmp = match path.parent() {
        Some(p) => p.join(format!(".{name}.tmp")),
        None => PathBuf::from(format!(".{name}.tmp")),
    };
    if std::fs::write(&tmp, payload).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
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
        let (id, label) =
            parse_index_line("- [#1](scratchpad/1.md) Sample Notes - updated 2026-05-23 18:05:21")
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
        assert_eq!(
            Target::parse("scratchpad-list", None),
            Some(Target::ScratchpadList)
        );
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
        assert_eq!(
            Target::parse("new-scratchpad", None),
            Some(Target::NewScratchpad)
        );
        assert_eq!(
            Target::parse("scratchpad", Some(7)),
            Some(Target::Scratchpad(7))
        );
    }

    /// A viewer with `n` list entries and the given viewport height, used to
    /// exercise the click-to-row mapping without touching the filesystem or
    /// the daemon.
    fn viewer_with_list(n: usize, viewport: u16, cursor: usize) -> Viewer {
        let entries = (0..n)
            .map(|i| ListEntry {
                target: Target::Todo(i as u64),
                label: format!("#{i}"),
                // The existing list_row_at tests run with the `All` filter,
                // so every entry is visible regardless of status.
                status: Some("open".to_string()),
                is_blocked: false,
                priority: None,
                created_at: None,
                updated_at: None,
            })
            .collect();
        Viewer {
            ws: PathBuf::from("/tmp"),
            url: String::new(),
            routing_path: PathBuf::from("/tmp/route.json"),
            routing_mtime: None,
            target: Target::TodoList,
            content: Content::List(entries),
            content_mtime: None,
            scroll: 0,
            cursor,
            viewport,
            todo_filter: TodoFilter::All,
            todo_sort_1: TodoSort::PriorityDesc,
            todo_sort_2: TodoSort::CreatedAsc,
            last_refresh: Instant::now(),
            needs_draw: false,
            sole_content_pane: false,
        }
    }

    #[test]
    fn sole_content_pane_refuses_to_close() {
        // When this is the only right-side pane left, closing it would make
        // Zellij re-tile and the sidebar plugin panes stop being a sidebar.
        // Ctrl-C and `q` must therefore not quit it - they clear it back to
        // Empty and keep the process (and the layout) alive.
        for key in [
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty()),
        ] {
            let mut viewer = viewer_with_list(3, 5, 0);
            viewer.sole_content_pane = true;
            assert!(
                !viewer.handle_key(key),
                "sole pane must not quit on {key:?}"
            );
            assert_eq!(
                viewer.target,
                Target::Empty,
                "sole pane should clear to Empty on {key:?}"
            );
        }
    }

    #[test]
    fn viewer_closes_when_another_pane_remains() {
        // With another content pane present this viewer is not the last one,
        // so the close gesture quits as before: handle_key returns true and
        // the event loop exits, closing the pane.
        for key in [
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty()),
        ] {
            let mut viewer = viewer_with_list(3, 5, 0);
            assert!(
                viewer.handle_key(key),
                "non-sole pane should quit on {key:?}"
            );
        }
    }

    #[test]
    fn list_row_at_maps_terminal_row_to_entry() {
        // Body rows 1..=viewport map to entries; row 0 is the header, anything
        // past `viewport` is the footer or below.
        let viewer = viewer_with_list(5, 5, 0);
        assert_eq!(viewer.list_row_at(0), None);
        assert_eq!(viewer.list_row_at(1), Some(0));
        assert_eq!(viewer.list_row_at(5), Some(4));
        assert_eq!(viewer.list_row_at(6), None);
    }

    #[test]
    fn list_row_at_respects_scroll_offset() {
        // With more entries than viewport rows, the cursor near the bottom
        // scrolls the visible window; a click at body row 0 then resolves to
        // the first visible entry, not entry 0.
        let viewer = viewer_with_list(20, 5, 19);
        let start = list_scroll(viewer.cursor, viewer.viewport as usize, 20);
        assert_eq!(viewer.list_row_at(1), Some(start));
        assert_eq!(viewer.list_row_at(5), Some(start + 4));
    }

    #[test]
    fn list_row_at_returns_none_past_the_last_entry() {
        // A 2-entry list in a 5-row viewport: rows 1 and 2 are entries; rows
        // 3..=5 are empty body space and resolve to None.
        let viewer = viewer_with_list(2, 5, 0);
        assert_eq!(viewer.list_row_at(1), Some(0));
        assert_eq!(viewer.list_row_at(2), Some(1));
        assert_eq!(viewer.list_row_at(3), None);
    }

    #[test]
    fn list_row_at_is_none_outside_list_mode() {
        let mut viewer = viewer_with_list(5, 5, 0);
        viewer.content = Content::Message("hello".to_string());
        assert_eq!(viewer.list_row_at(1), None);
    }

    /// Build a viewer whose list carries four todos with the given
    /// `(status, is_blocked)` pairs.
    fn viewer_with_statuses(rows: &[(&str, bool)], filter: TodoFilter) -> Viewer {
        let entries = rows
            .iter()
            .enumerate()
            .map(|(i, (status, blocked))| ListEntry {
                target: Target::Todo(i as u64 + 1),
                label: format!("#{} t", i + 1),
                status: Some((*status).to_string()),
                is_blocked: *blocked,
                priority: None,
                created_at: None,
                updated_at: None,
            })
            .collect();
        Viewer {
            ws: PathBuf::from("/tmp"),
            url: String::new(),
            routing_path: PathBuf::from("/tmp/r.json"),
            routing_mtime: None,
            target: Target::TodoList,
            content: Content::List(entries),
            content_mtime: None,
            scroll: 0,
            cursor: 0,
            viewport: 5,
            todo_filter: filter,
            todo_sort_1: TodoSort::PriorityDesc,
            todo_sort_2: TodoSort::CreatedAsc,
            last_refresh: Instant::now(),
            needs_draw: false,
            sole_content_pane: false,
        }
    }

    #[test]
    fn open_unblocked_hides_blocked_open_todos_and_other_statuses() {
        let viewer = viewer_with_statuses(
            &[
                ("open", false),        // visible
                ("open", true),         // hidden: blocked
                ("in_progress", false), // hidden: wrong status
                ("completed", false),   // hidden: wrong status
            ],
            TodoFilter::OpenUnblocked,
        );
        assert_eq!(viewer.visible_indices(), vec![0]);
    }

    #[test]
    fn all_filter_shows_every_entry() {
        let viewer = viewer_with_statuses(
            &[("open", true), ("completed", false), ("not_done", false)],
            TodoFilter::All,
        );
        assert_eq!(viewer.visible_indices(), vec![0, 1, 2]);
    }

    #[test]
    fn filter_cycle_visits_every_variant_and_wraps() {
        let mut seen = vec![TodoFilter::default()];
        let mut cur = TodoFilter::default();
        for _ in 0..ALL_FILTERS.len() {
            cur = cur.next();
            seen.push(cur);
        }
        // After exactly one full cycle we're back to the default; every
        // variant has appeared.
        assert_eq!(seen.last(), Some(&TodoFilter::default()));
        // ALL_FILTERS is the source of truth for the cycle; check that the
        // length matches the number of distinct variants by string label.
        let labels: std::collections::HashSet<&str> =
            ALL_FILTERS.iter().map(|f| f.label()).collect();
        assert_eq!(labels.len(), ALL_FILTERS.len());
    }

    #[test]
    fn parse_status_suffix_recognises_known_tokens() {
        assert_eq!(
            parse_status_suffix("wire up auth - open, high"),
            Some("open".to_string())
        );
        assert_eq!(
            parse_status_suffix("ship it - not_done, medium"),
            Some("not_done".to_string())
        );
        // No suffix at all - leave the entry unfiltered by status.
        assert_eq!(parse_status_suffix("just a title"), None);
        // Unknown token doesn't tag the entry; the projection grammar can
        // grow new tokens later without crashing this parser.
        assert_eq!(parse_status_suffix("future - archived, medium"), None);
    }

    #[test]
    fn cycle_filter_is_a_noop_when_not_on_todo_list() {
        let mut viewer = viewer_with_statuses(&[("open", false)], TodoFilter::All);
        viewer.target = Target::ScratchpadList;
        viewer.cycle_filter(true);
        assert_eq!(viewer.todo_filter, TodoFilter::All);
    }

    #[test]
    fn promotion_target_flips_new_kinds_once_an_id_lands() {
        // Both new-* kinds promote symmetrically once the daemon assigns an
        // id; this is the contract sync_pane_titles relies on to re-title the
        // pane from "New scratchpad" / "New todo" to the id-bearing form.
        assert_eq!(
            promotion_target(&Target::NewTodo, Some(7)),
            Some(Target::Todo(7))
        );
        assert_eq!(
            promotion_target(&Target::NewScratchpad, Some(7)),
            Some(Target::Scratchpad(7))
        );
        // Flush hasn't assigned an id yet (empty-title autosave is a no-op):
        // stay on the new-* kind so the plugin keeps the placeholder title.
        assert_eq!(promotion_target(&Target::NewTodo, None), None);
        assert_eq!(promotion_target(&Target::NewScratchpad, None), None);
        // Already promoted: a subsequent autosave must not re-flip the target
        // (which would clobber a user-driven re-point of the same pane).
        assert_eq!(promotion_target(&Target::Todo(7), Some(9)), None);
        assert_eq!(promotion_target(&Target::Scratchpad(7), Some(9)), None);
        // Non-form targets never promote regardless of an incoming id.
        assert_eq!(promotion_target(&Target::TodoList, Some(7)), None);
        assert_eq!(promotion_target(&Target::Empty, Some(7)), None);
    }

    #[test]
    fn write_routing_to_emits_the_plugin_compatible_payload() {
        // The viewer rewrites its own routing file on promotion; the payload
        // must match what the plugin writes byte-for-byte so `parse_viewer_-
        // routing` and the plugin's title path round-trip cleanly.
        let dir = tempdir();
        let path = dir.join("viewer-tx.json");
        write_routing_to(&path, "scratchpad", Some(42));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"kind":"scratchpad","id":42}"#
        );
        write_routing_to(&path, "todo", Some(9));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"kind":"todo","id":9}"#
        );
    }

    #[test]
    fn promote_form_target_rewrites_routing_and_flips_target() {
        // End-to-end of the #66 / #57 fix: a NewScratchpad pane whose form
        // just earned id 42 must (a) overwrite its routing file with the id-
        // bearing kind so the sidebar plugin re-titles it on the next tick,
        // and (b) flip self.target so the cockpit's own status bar follows.
        let dir = tempdir();
        let routing_path = dir.join("viewer-promo.json");
        std::fs::write(&routing_path, r#"{"kind":"new-scratchpad"}"#).unwrap();
        let mut viewer = Viewer {
            ws: dir.clone(),
            url: String::new(),
            routing_path: routing_path.clone(),
            routing_mtime: mtime(&routing_path),
            target: Target::NewScratchpad,
            content: Content::Message(String::new()),
            content_mtime: None,
            scroll: 0,
            cursor: 0,
            viewport: 1,
            todo_filter: TodoFilter::All,
            todo_sort_1: TodoSort::PriorityDesc,
            todo_sort_2: TodoSort::CreatedAsc,
            last_refresh: Instant::now(),
            needs_draw: false,
            sole_content_pane: false,
        };
        viewer.promote_form_target(Target::Scratchpad(42));
        assert_eq!(viewer.target, Target::Scratchpad(42));
        assert_eq!(
            std::fs::read_to_string(&routing_path).unwrap(),
            r#"{"kind":"scratchpad","id":42}"#
        );
        // routing_mtime tracks the write we just did, so the next poll_routing
        // sees no change and doesn't reload (which would tear the open form).
        assert_eq!(viewer.routing_mtime, mtime(&routing_path));
    }

    fn tempdir() -> PathBuf {
        // Tests can run concurrently; nanos + pid keeps paths unique without
        // pulling in the `tempfile` crate just for two unit tests.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("panopt-viewer-test-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
