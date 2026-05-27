//! PANopt coordination sidebar - a Zellij plugin.
//!
//! Each plugin pane renders one kind of resource - todos, agents, terminals,
//! commands, or scratchpads - selected by the `mode` value in its layout
//! config. The five panes stack vertically in the cockpit's left column, each
//! pinned to its own fixed proportion so adding or removing panes on the
//! right cannot reshape any of them. A keyboard cursor walks the pane's items
//! and scrolls when it hits the visible window's edge; the mouse clicks any
//! row.
//!
//! The cockpit is these five panes plus one content pane on the right.
//! Selecting an item swaps its pane into that one slot and suppresses
//! whatever was there - a suppressed pane keeps running, just hidden, no
//! stack and no title bar. Documents (todos, scratchpads, lists) all share
//! one re-pointable `panopt _viewer` pane; agents, commands, and terminals
//! are each their own pane. Moving the cursor previews the selected item in
//! the slot - or clears the slot when the row has nothing to show - always
//! without taking focus off the plugin pane. A click does the same; Enter
//! additionally focuses the pane.
//!
//! If the user splits the content pane, a selection swaps into whichever
//! pane was focused last before any plugin pane took focus - the designated
//! slot. Each of the five panes derives the slot independently from the same
//! `PaneUpdate` manifest, so they all converge on the same target.
//!
//! The Todos plugin pane doubles as the cockpit gatekeeper: it is the only
//! pane that handles the close-request and spawn pipes (see `up::render_config`).
//! When an active agent, command, or terminal would be lost, the Todos pane
//! refuses by showing a floating dialog with a `close anyway` override; any
//! of the five plugin panes themselves cannot be closed.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use zellij_tile::prelude::*;

/// Routing slot prefix for viewer panes the plugin spawns ad hoc. The layout
/// boots one viewer with `--slot main`; further viewers spawned by
/// [`PanoptPane::ensure_viewer_in_slot`] get unique names `v<mode-letter><n>`
/// (`vt1`, `vs1`, `va1`, ...) so each pane has its own
/// `.panopt/.cockpit/viewer-<slot>.json` routing file and the five plugin
/// instances cannot collide on the same suffix. Per-pane routing keeps
/// sidebar navigation single-pane: only the slot's viewer re-points on a
/// preview, leaving any other split's viewer on whatever it was last showing
/// (the user's "kept doc" pattern).
const SPAWNED_VIEWER_SLOT_PREFIX: &str = "v";

/// Per-pane file the cockpit projects agent labels into. Each plugin instance
/// writes the labels it owns (the Todos pane gets named agents via the
/// `panopt:spawn-agent` pipe) and every instance reads the file so labels
/// stay consistent across the five panes.
const AGENT_LABELS_PATH: &str = "/host/.panopt/.cockpit/agent-labels.json";

/// Which kind of resource one plugin pane renders. Five plugin instances run
/// in parallel, one per `Mode`, each configured by the `mode "<kind>"` value
/// in the layout's plugin block. Zellij keys plugin identity on
/// `(URL, configuration)`, so the five panes are five distinct instances and
/// can be addressed individually by `zellij action pipe --plugin-configuration
/// "mode=<kind>"`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Mode {
    #[default]
    Todos,
    Agents,
    Terminals,
    Commands,
    Scratchpads,
}

impl Mode {
    fn parse(s: &str) -> Option<Mode> {
        match s {
            "todos" => Some(Mode::Todos),
            "agents" => Some(Mode::Agents),
            "terminals" => Some(Mode::Terminals),
            "commands" => Some(Mode::Commands),
            "scratchpads" => Some(Mode::Scratchpads),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Mode::Todos => "Todos",
            Mode::Agents => "Agents",
            Mode::Terminals => "Terminals",
            Mode::Commands => "Commands",
            Mode::Scratchpads => "Scratchpads",
        }
    }

    /// One-letter slug that prefixes spawned viewer slot names so the five
    /// plugin instances cannot collide on the same `v<N>` suffix.
    fn letter(self) -> char {
        match self {
            Mode::Todos => 't',
            Mode::Agents => 'a',
            Mode::Terminals => 'r',
            Mode::Commands => 'c',
            Mode::Scratchpads => 's',
        }
    }
}

/// Status filter applied to the Todos pane. Each variant matches a wire
/// token from the projection's `- <status>, <priority>` suffix.
///
/// `OpenUnblocked` is the default working-set filter, but the sidebar reads
/// only the projection (no MCP), and the index doesn't carry blocker info;
/// in this pane `OpenUnblocked` degrades to "open" until the projection
/// learns to record blockers per row. The viewer pane on the right uses MCP
/// and applies the full blocker-aware filter, so the precise unblocked
/// view is available there.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum TodoFilter {
    All,
    Open,
    #[default]
    OpenUnblocked,
    InProgress,
    Backlog,
    Completed,
    NotDone,
}

const ALL_TODO_FILTERS: [TodoFilter; 7] = [
    TodoFilter::All,
    TodoFilter::Open,
    TodoFilter::OpenUnblocked,
    TodoFilter::InProgress,
    TodoFilter::Backlog,
    TodoFilter::Completed,
    TodoFilter::NotDone,
];

impl TodoFilter {
    fn label(self) -> &'static str {
        match self {
            TodoFilter::All => "all",
            TodoFilter::Open => "open",
            TodoFilter::OpenUnblocked => "open-unblocked",
            TodoFilter::InProgress => "in_progress",
            TodoFilter::Backlog => "backlog",
            TodoFilter::Completed => "completed",
            TodoFilter::NotDone => "not_done",
        }
    }

    fn next(self) -> TodoFilter {
        let i = ALL_TODO_FILTERS
            .iter()
            .position(|f| *f == self)
            .unwrap_or(0);
        ALL_TODO_FILTERS[(i + 1) % ALL_TODO_FILTERS.len()]
    }

    fn prev(self) -> TodoFilter {
        let i = ALL_TODO_FILTERS
            .iter()
            .position(|f| *f == self)
            .unwrap_or(0);
        ALL_TODO_FILTERS[(i + ALL_TODO_FILTERS.len() - 1) % ALL_TODO_FILTERS.len()]
    }

    /// Whether the projection-index label passes this filter. The label is
    /// the trailing text after the link, e.g. `"the title - open, high"`.
    /// Without blocker info, `OpenUnblocked` is approximated as `Open`.
    fn includes_label(self, label: &str) -> bool {
        if matches!(self, TodoFilter::All) {
            return true;
        }
        let Some(status) = parse_status_suffix(label) else {
            // Missing / unparsable status: leave the entry visible so a
            // stray projection format never silently hides a real todo.
            return true;
        };
        match self {
            TodoFilter::All => true,
            TodoFilter::Open | TodoFilter::OpenUnblocked => status == "open",
            TodoFilter::InProgress => status == "in_progress",
            TodoFilter::Backlog => status == "backlog",
            TodoFilter::Completed => status == "completed",
            TodoFilter::NotDone => status == "not_done",
        }
    }
}

/// Extract the wire status token from a projection-index label suffix like
/// `wire up auth - open, high`. Returns `None` for labels without a known
/// suffix; callers treat that as "do not hide."
fn parse_status_suffix(label: &str) -> Option<&str> {
    let dash = label.rfind(" - ")?;
    let rest = &label[dash + 3..];
    let comma = rest.find(',').unwrap_or(rest.len());
    let token = rest[..comma].trim();
    matches!(
        token,
        "open" | "in_progress" | "backlog" | "completed" | "not_done"
    )
    .then_some(token)
}

/// Extract the wire priority token from a projection-index label suffix
/// like `wire up auth - open, high`. Returns `None` for labels without a
/// known suffix.
fn parse_priority_suffix(label: &str) -> Option<&str> {
    let dash = label.rfind(" - ")?;
    let rest = &label[dash + 3..];
    let comma = rest.find(',')?;
    let token = rest[comma + 1..].trim();
    matches!(token, "high" | "medium" | "low").then_some(token)
}

/// One axis of the two-level todo sort. The sidebar carries two of these
/// (level 1 / level 2) and applies them as a stable two-pass sort, so equal
/// keys on level 1 are broken by level 2.
///
/// The sidebar reads only the projection index, which carries status and
/// priority but no per-todo timestamps. The created/modified axes therefore
/// degrade to **id order** (project ids are monotonic, so id asc ≡
/// creation order asc; modified date falls back to the same proxy). This
/// mirrors the existing `OpenUnblocked → Open` degradation in
/// [`TodoFilter::includes_label`]. The viewer pane on the right uses MCP
/// and applies the full timestamp-aware sort.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum TodoSort {
    #[default]
    PriorityDesc,
    CreatedAsc,
    CreatedDesc,
    ModifiedAsc,
    ModifiedDesc,
}

const ALL_TODO_SORTS: [TodoSort; 5] = [
    TodoSort::PriorityDesc,
    TodoSort::CreatedAsc,
    TodoSort::CreatedDesc,
    TodoSort::ModifiedAsc,
    TodoSort::ModifiedDesc,
];

impl TodoSort {
    /// Display label. `/` indicates ascending (low → high), `\` indicates
    /// descending (high → low); priority is single-direction (high → low)
    /// so it carries no suffix.
    fn label(self) -> &'static str {
        match self {
            TodoSort::PriorityDesc => "priority",
            TodoSort::CreatedAsc => "created-/",
            TodoSort::CreatedDesc => "created-\\",
            TodoSort::ModifiedAsc => "modified-/",
            TodoSort::ModifiedDesc => "modified-\\",
        }
    }

    fn next(self) -> TodoSort {
        let i = ALL_TODO_SORTS.iter().position(|x| *x == self).unwrap_or(0);
        ALL_TODO_SORTS[(i + 1) % ALL_TODO_SORTS.len()]
    }

    fn prev(self) -> TodoSort {
        let i = ALL_TODO_SORTS.iter().position(|x| *x == self).unwrap_or(0);
        ALL_TODO_SORTS[(i + ALL_TODO_SORTS.len() - 1) % ALL_TODO_SORTS.len()]
    }

    /// Compare two projection rows on this axis. The sidebar's row shape is
    /// `(id, label)`; priority comes from [`parse_priority_suffix`], and the
    /// time-based axes degrade to comparing ids (see the type doc).
    fn cmp_rows(self, a: &(u64, String), b: &(u64, String)) -> std::cmp::Ordering {
        match self {
            TodoSort::PriorityDesc => {
                let rank = |label: &str| match parse_priority_suffix(label) {
                    Some("high") => 3,
                    Some("medium") => 2,
                    Some("low") => 1,
                    _ => 0,
                };
                rank(&b.1).cmp(&rank(&a.1))
            }
            TodoSort::CreatedAsc | TodoSort::ModifiedAsc => a.0.cmp(&b.0),
            TodoSort::CreatedDesc | TodoSort::ModifiedDesc => b.0.cmp(&a.0),
        }
    }
}

#[derive(Default)]
struct PanoptPane {
    /// Which resource kind this plugin instance renders. Set in
    /// [`PanoptPane::load`] from the `mode` config key.
    mode: Mode,
    /// Whether the `mode` config value was a recognized kind. False means we
    /// fell back to [`Mode::Todos`] and the frame title carries a warning so
    /// a misconfigured pane is visible rather than silently broken.
    mode_known: bool,

    /// Absolute project root, from the layout's plugin config. The cwd for
    /// spawned panes.
    ws: Option<String>,
    /// Absolute path to the `panopt` binary, from the layout's plugin config.
    panopt_bin: String,
    /// The daemon port, from the layout's plugin config.
    port: String,
    /// Whether Zellij has granted the requested permissions.
    permitted: bool,

    /// Todos parsed from `.panopt/todos.md`: `(id, label)`. Populated only
    /// when this pane's mode actually displays todos.
    todos: Vec<(u64, String)>,
    /// Scratchpads parsed from `.panopt/scratchpads.md`: `(id, label)`.
    scratchpads: Vec<(u64, String)>,
    /// Process instances parsed from `.panopt/processes.md`.
    /// TODO(#27): render agent_tools.md alongside processes once a spawn UI
    /// exists; until then the sidebar shows only live instances, same as the
    /// pre-V6 roster view.
    processes: Vec<ProcessRow>,
    /// Live (and suppressed) content panes flattened from Zellij's manifest.
    panes: Vec<PaneRow>,

    /// Items currently shown by this mode, rebuilt on every change. Always a
    /// single flat list; no sections.
    items: Vec<Item>,
    /// Status filter the Todos pane applies when building [`Self::items`].
    /// Cycled with `f` / `F`; held across rebuilds so the user keeps their
    /// view as the projection changes underneath. Ignored by every other
    /// mode.
    todo_filter: TodoFilter,
    /// Primary sort axis for the Todos pane. Cycled with `1` / `!`; default
    /// [`TodoSort::PriorityDesc`]. Held in-memory only - reset to the
    /// default on every Zellij restart, same as [`Self::todo_filter`].
    todo_sort_1: TodoSort,
    /// Secondary sort axis, applied as a stable tiebreaker. Cycled with
    /// `2` / `@`. Defaults to [`TodoSort::CreatedAsc`] (oldest first) which
    /// together with the priority-desc level 1 surfaces the highest-priority
    /// oldest-open todo at the top.
    todo_sort_2: TodoSort,

    /// Index of the keyboard-selected item. Stays in `0..items.len()`.
    cursor: usize,
    /// Index of the topmost item rendered in the visible window. The window
    /// is `last_rows - 1` items (one row reserved for the title).
    scroll: usize,
    /// Last `rows` value passed to [`PanoptPane::render`]. Cached so the key
    /// and mouse handlers can clamp scroll using a single source of truth.
    last_rows: usize,
    /// Last `cols` value passed to [`PanoptPane::render`]. Cached so
    /// [`PanoptPane::frame_title`] can fit the title to the pane's width
    /// (dropping the counts segment when the pane is too narrow to also
    /// show the filter/sort label, rather than letting Zellij truncate the
    /// middle of the label - the part the user most wants to read).
    last_cols: usize,

    /// This plugin's own pane id, learned at load - used to return focus to
    /// the plugin pane after a swap.
    plugin_pane: Option<PaneId>,
    /// The pane occupying the designated content slot: the pane a selection
    /// swaps against. It is the last non-plugin pane focused before any
    /// plugin pane took focus, updated in place whenever the plugin swaps
    /// the slot itself.
    slot_pane: Option<PaneId>,
    /// How many ad-hoc agents this instance has numbered. Only meaningful in
    /// the Todos (gatekeeper) pane, which is the only pane that handles the
    /// `panopt:spawn-agent` pipe; other panes pick up labels from the
    /// projection file at [`AGENT_LABELS_PATH`].
    next_agent: u32,
    /// Counter for allocating unique routing slot names for viewer panes the
    /// plugin spawns. Combined with [`Mode::letter`] so two plugin instances
    /// cannot allocate the same name. The boot viewer keeps its `--slot main`
    /// from the layout.
    next_viewer_slot: u32,
    /// Sidebar label for each agent pane, keyed by terminal pane id. The
    /// gatekeeper writes user-supplied labels here and projects the map to
    /// [`AGENT_LABELS_PATH`]; every other pane reads from that file. Never
    /// participates in ordering - agent rows order by pane id (creation
    /// order), so a label change cannot reshuffle the list.
    agent_labels: BTreeMap<u32, String>,

    /// Whether any plugin pane is currently the focused pane in its tab.
    /// Updated by [`PanoptPane::ingest_panes`] but only from a non-transient
    /// manifest: a transient `zellij action pipe` pane briefly steals focus
    /// while the close-gate pipes fly, and we must not let that flicker make
    /// the gate think the user has moved off the cockpit panes.
    /// Used by [`PanoptPane::gate_close_focus`] to refuse closing any plugin
    /// pane absolutely.
    sidebar_focused: bool,
    /// The tab position with a focused pane, derived from the same manifest
    /// snapshot that drives `sidebar_focused`. Scopes the CloseTab gate.
    focused_tab: Option<usize>,
    /// The last gate refusal: what was refused (a label), set when the gate
    /// blocks an action because active items would be lost. Surfaced through
    /// the pane's frame title so the user knows their keypress was
    /// intercepted. Cleared on the next successful navigation. Only the Todos
    /// pane runs the gate, so only the Todos pane ever sets this.
    last_gate_refusal: Option<String>,
    /// The frame title most recently pushed to Zellij via
    /// [`rename_plugin_pane`]. Kept so we only re-issue the host call when
    /// the title actually changes (gate refusal appears/clears, scroll
    /// position shifts, ...) rather than on every render tick.
    last_frame_title: String,
    /// Last terminal-pane title we pushed via [`rename_terminal_pane`],
    /// keyed by Zellij terminal pane id. Lets the Todos pane (the only one
    /// that titles right-pane terminals) re-issue the host call only when
    /// the title actually changes, rather than on every manifest tick.
    last_pane_titles: BTreeMap<u32, String>,
    /// Whether the initial preview has been shown on startup. Only the Todos
    /// pane drives the initial preview; the others remain idle until the
    /// user navigates them.
    initial_preview_done: bool,
    /// Counter for delaying the initial preview until the UI is ready.
    initial_preview_delay: u32,
    /// When `true`, the pane body shows the per-mode key cheat-sheet instead
    /// of the item list. Toggled by `?`; any other keypress dismisses it.
    /// Holds the same set of keys across modes so the UI never carries
    /// per-mode hint clutter - the help is the only place keys are listed.
    show_help: bool,
}

/// A parsed `.panopt/processes.md` line. The line format is preserved from
/// the pre-V6 `roster.md` so the existing `[kind] #id label` parser still
/// works; any trailing `(from #N)` is dropped from `label`.
struct ProcessRow {
    kind: String,
    id: u64,
    label: String,
}

/// A content pane flattened from Zellij's manifest.
struct PaneRow {
    id: PaneId,
    title: String,
    focused: bool,
    /// A suppressed pane is hidden but still running - swapped out of the
    /// slot by an earlier selection. Used by
    /// [`PanoptPane::route_pane_to_slot`] and
    /// [`PanoptPane::ensure_viewer_in_slot`] to tell whether a target pane
    /// is already on screen.
    suppressed: bool,
    exited: bool,
    role: PaneRole,
    /// For [`PaneRole::Viewer`] panes only: the `--slot X` token from the
    /// launch command, used as the routing file name
    /// `.panopt/.cockpit/viewer-<slot>.json`. `None` on any other role.
    viewer_slot: Option<String>,
    /// Tab position from the `PaneManifest`. Used by the CloseTab gate to
    /// scope active-item aggregation to a single tab.
    tab: usize,
}

/// What a content pane is, derived from the command it was launched with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PaneRole {
    /// The shared `panopt _viewer` document pane.
    Viewer,
    /// An ad-hoc `panopt _agent` pane, started with `a`.
    Agent,
    /// A `panopt _process-run <id>` pane, by process id.
    Process(u64),
    /// A plain terminal the user opened.
    Shell,
}

/// One item rendered in the pane.
struct Item {
    label: String,
    target: ItemTarget,
    /// A live marker: a running process, or the Zellij-focused pane.
    live: bool,
}

/// What selecting an item does.
#[derive(Clone)]
enum ItemTarget {
    Todo(u64),
    Scratchpad(u64),
    /// A process agent or command, by process id.
    Process(u64),
    /// An existing pane: an ad-hoc agent or a plain terminal.
    Pane(PaneId),
}

register_plugin!(PanoptPane);

impl ZellijPlugin for PanoptPane {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        match configuration
            .get("mode")
            .map(String::as_str)
            .and_then(Mode::parse)
        {
            Some(mode) => {
                self.mode = mode;
                self.mode_known = true;
            }
            None => {
                self.mode = Mode::Todos;
                self.mode_known = false;
            }
        }
        self.ws = configuration.get("ws").cloned();
        self.panopt_bin = configuration
            .get("panopt_bin")
            .cloned()
            .unwrap_or_else(|| "panopt".to_string());
        self.port = configuration
            .get("port")
            .cloned()
            .unwrap_or_else(|| "7600".to_string());
        // Override the per-field Default for level 2: `TodoSort::default()`
        // is `PriorityDesc`, but we want a distinct level-2 default so the
        // initial sort is "priority desc, then oldest first" rather than
        // both levels being the same axis.
        self.todo_sort_2 = TodoSort::CreatedAsc;
        self.plugin_pane = Some(PaneId::Plugin(get_plugin_ids().plugin_id));
        // The Todos pane is the only instance that boots a fresh cockpit (it
        // is the first pane Zellij loads in the layout); have it clear stale
        // routing files left by a previous session so all the other panes see
        // a clean `.cockpit/` mount on startup.
        if self.mode == Mode::Todos {
            let _ = fs::remove_dir_all("/host/.panopt/.cockpit");
        }
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::RunCommands,
        ]);
        subscribe(&[
            EventType::PaneUpdate,
            EventType::Key,
            EventType::Mouse,
            EventType::Timer,
            EventType::PermissionRequestResult,
        ]);
        self.reload_data();
        self.rebuild_items();
        self.sync_frame_title();
        set_timeout(1.0);
    }

    fn update(&mut self, event: Event) -> bool {
        let dirty = match event {
            Event::PermissionRequestResult(status) => {
                self.permitted = matches!(status, PermissionStatus::Granted);
                true
            }
            Event::PaneUpdate(manifest) => {
                self.ingest_panes(manifest);
                self.rebuild_items();
                true
            }
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Timer(_) => {
                // Only the Todos pane drives the initial preview - one preview
                // per cockpit boot is enough, and the other panes have nothing
                // useful to preview until the user navigates them.
                if self.mode == Mode::Todos
                    && !self.initial_preview_done
                    && self.slot_pane.is_some()
                    && self.permitted
                {
                    self.initial_preview_delay += 1;
                    if self.initial_preview_delay >= 2 {
                        self.preview_cursor();
                        if let Some(plugin) = self.plugin_pane {
                            focus_pane_with_id(plugin, false, false);
                        }
                        self.initial_preview_done = true;
                    }
                }
                self.reload_data();
                self.rebuild_items();
                // PaneUpdate-driven `sync_pane_titles` only fires when Zellij
                // sends a pane manifest - typing into the form does not. Without
                // this call, every right-pane title (most visibly a freshly
                // promoted "Todo #N - ...") would freeze at whatever value it
                // held when the last PaneUpdate landed, even after autosaves
                // have refreshed `self.todos` from the projection.
                self.sync_pane_titles();
                set_timeout(1.0);
                true
            }
            _ => false,
        };
        // The frame title is set via the `rename_plugin_pane` plugin command,
        // which serializes itself onto stdout for the host to read. The host
        // only consumes those command bytes BETWEEN events - issuing a plugin
        // command from inside `render` makes the JSON leak in as a phantom
        // content row and shifts every item down by one. So sync the title
        // only here, never in `render`.
        if dirty {
            self.sync_frame_title();
        }
        dirty
    }

    /// Only the Todos pane handles the cockpit-wide pipes. With five plugin
    /// instances running, every `zellij action pipe` invocation reaches all
    /// of them; the keybinds in `up::render_config` narrow delivery with
    /// `--plugin-configuration "mode=todos"`, and this guard provides
    /// belt-and-braces idempotency if a custom config slips that filter.
    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        if self.mode != Mode::Todos {
            return false;
        }
        match pipe_message.name.as_str() {
            "panopt:spawn-agent" => {
                self.spawn_agent_pane(pipe_message.payload.as_deref());
                true
            }
            "panopt:spawn-blank-pane" => {
                self.spawn_blank_pane();
                true
            }
            "panopt:close-focus-request" => {
                self.gate_close_focus();
                true
            }
            "panopt:close-tab-request" => {
                self.gate_close_tab();
                true
            }
            "panopt:quit-request" => {
                self.gate_quit();
                true
            }
            "panopt:close-gate-decision" => {
                self.handle_gate_decision(pipe_message.payload.as_deref());
                true
            }
            "panopt:delete-gate-decision" => {
                self.handle_delete_decision(pipe_message.payload.as_deref());
                true
            }
            _ => false,
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        self.last_rows = rows;
        self.last_cols = cols;
        // A resize can shrink the title budget; re-evaluate so the title
        // sheds its counts segment (rather than letting Zellij mid-truncate)
        // when the pane narrows past the filter/sort label width.
        self.sync_frame_title();
        // The plugin's stdout becomes the pane content. The mode label
        // lives in Zellij's frame title (set by `sync_frame_title` from
        // `update`); the pane body is just the item list.
        //
        // Each row is written with absolute cursor positioning (`\x1b[r;1H`)
        // and the line is cleared (`\x1b[2K`) before the new content lands.
        // We never advance the cursor with `\r\n`, so it cannot cross the
        // bottom edge of the visible area - which is what was growing the
        // pane's scrollback (and the "n/m" indicator Zellij overlays on the
        // frame) by one row per render.
        let total = self.items.len();
        // Item area = body minus the reserved status-line row (Todos only).
        let visible = self.list_rows();
        let max_scroll = total.saturating_sub(visible);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
        // Wipe every body row first, so a shrinking list (or stale content
        // from a previous render) cannot leak through. `\x1b[3J` also drops
        // anything sitting in the pane's scrollback buffer - pane resizes
        // (and any stray newline that leaks past the visible bottom) push
        // rows into scrollback, and Zellij overlays that row count on the
        // frame as `n/m`; clearing it each render keeps the indicator at
        // zero.
        print!("\u{1b}[3J");
        let body_rows = rows.max(1);
        for row in 1..=body_rows {
            print!("\u{1b}[{row};1H\u{1b}[2K");
        }
        if self.show_help {
            for (i, line) in self.help_lines().iter().take(body_rows).enumerate() {
                print!(
                    "\u{1b}[{};1H{}",
                    i + 1,
                    paint(line, cols, Style::Dim, false)
                );
            }
            return;
        }
        if total == 0 {
            print!("\u{1b}[1;1H{}", paint("  (none)", cols, Style::Dim, false));
        } else {
            let end = (self.scroll + visible).min(total);
            for (slot, idx) in (self.scroll..end).enumerate() {
                let item = &self.items[idx];
                let marker = if item.live { '*' } else { ' ' };
                let line = format!(" {marker}{}", item.label);
                let focused = idx == self.cursor;
                print!(
                    "\u{1b}[{};1H{}",
                    slot + 1,
                    paint(&line, cols, Style::Normal, focused)
                );
            }
        }
        // Status line: the very bottom body row, reserved by `list_rows()`
        // in Todos mode. Other modes don't reserve, so don't draw here.
        if self.mode == Mode::Todos && body_rows >= 2 {
            let status = self.status_line();
            print!(
                "\u{1b}[{};1H{}",
                body_rows,
                paint(&status, cols, Style::Dim, false)
            );
        }
    }
}

impl PanoptPane {
    /// Build the pane's frame title: the mode label plus any status the
    /// pane wants to surface to the user (permission prompt, mode-config
    /// warning, gate refusal, or scroll position when the list overflows).
    /// The frame title is the only place these statuses live now - the pane
    /// body is just the item list.
    fn frame_title(&self) -> String {
        let base = self.mode.label();
        if !self.permitted {
            return format!("{base} - grant permissions");
        }
        if !self.mode_known {
            return format!("{base} (mode config missing - defaulted)");
        }
        if self.mode == Mode::Todos {
            if let Some(refusal) = &self.last_gate_refusal {
                return format!("{base} - blocked: {refusal}");
            }
        }
        let total = self.items.len();
        let visible = self.list_rows();
        // Todos pane: surface the current filter value in the title (no
        // key hint - those live in `?` help) so the user always knows
        // which slice of the projection they're looking at. The sort axes
        // ride along on the bottom status line.
        let filter_seg = if self.mode == Mode::Todos {
            format!(" [{}]", self.todo_filter.label())
        } else {
            String::new()
        };
        let counts_seg = if total == 0 {
            String::new()
        } else if total <= visible {
            format!(" ({total})")
        } else {
            let start = self.scroll + 1;
            let end = (self.scroll + visible).min(total);
            format!(" ({start}-{end}/{total})")
        };
        let full = format!("{base}{filter_seg}{counts_seg}");
        // Zellij decorates the frame title with a couple of chars on each
        // side (`┤ ... ├` plus padding); a small margin keeps us from
        // tripping the host's mid-string truncation right at the boundary.
        // `last_cols == 0` is the pre-render state - keep the full title in
        // that case rather than aggressively trimming on hypothetical width.
        const FRAME_MARGIN: usize = 4;
        let budget = self.last_cols.saturating_sub(FRAME_MARGIN);
        if self.last_cols == 0 || full.chars().count() <= budget {
            full
        } else {
            // Drop the counts; keep the filter value so the user always
            // sees the slice they're on. The counts are recoverable from
            // the body itself.
            format!("{base}{filter_seg}")
        }
    }

    /// The status-line text drawn on the reserved bottom row of the Todos
    /// pane body. Shows the two sort axes' current values, no key hints
    /// (those live in `?` help). Zellij gives plugins no API to write the
    /// bottom *border* (only the top frame title via `rename_plugin_pane`),
    /// so this is the closest we can get to a "bottom title" - a dim status
    /// line at the foot of the body.
    fn status_line(&self) -> String {
        format!(
            " [{}] [{}]",
            self.todo_sort_1.label(),
            self.todo_sort_2.label(),
        )
    }

    /// Lines for the `?` help overlay, tailored to the active mode. The
    /// navigation block and the always-available `a` / `?` are shown
    /// everywhere; per-mode bindings are listed only where they actually
    /// do something so the cheat-sheet matches the handler.
    fn help_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(" Keys - {}", self.mode.label()), String::new()];
        lines.push("  up/down       move cursor".to_string());
        lines.push("  PgUp/PgDn     page".to_string());
        lines.push("  Home/End      first/last".to_string());
        lines.push("  Enter         open / focus".to_string());
        lines.push(String::new());
        match self.mode {
            Mode::Todos => {
                lines.push("  n             new todo".to_string());
                lines.push("  e             edit (open + focus)".to_string());
                lines.push("  x             delete todo".to_string());
                lines.push(format!(
                    "  f / F         filter forward / back  [{}]",
                    self.todo_filter.label()
                ));
                lines.push(format!(
                    "  1 / !         sort 1 forward / back  [{}]",
                    self.todo_sort_1.label()
                ));
                lines.push(format!(
                    "  2 / @         sort 2 forward / back  [{}]",
                    self.todo_sort_2.label()
                ));
            }
            Mode::Scratchpads => {
                lines.push("  n             new scratchpad".to_string());
            }
            Mode::Agents => {
                lines.push("  n             new agent".to_string());
                lines.push("  u             start / focus".to_string());
                lines.push("  d             stop (close pane)".to_string());
                lines.push("  x             delete agent".to_string());
            }
            Mode::Commands => {
                lines.push("  u             start / focus".to_string());
                lines.push("  d             stop (close pane)".to_string());
                lines.push("  x             delete command".to_string());
            }
            Mode::Terminals => {
                lines.push("  x / d         close terminal".to_string());
            }
        }
        lines.push(String::new());
        lines.push("  ?             toggle this help".to_string());
        lines
    }

    /// Push the current [`Self::frame_title`] to Zellij as the pane's frame
    /// title - but only when the text actually changes, since renaming is a
    /// host call and `update` runs on every event.
    fn sync_frame_title(&mut self) {
        let title = self.frame_title();
        if title == self.last_frame_title {
            return;
        }
        if let Some(PaneId::Plugin(pid)) = self.plugin_pane {
            rename_plugin_pane(pid, &title);
        }
        self.last_frame_title = title;
    }

    /// Title every right-pane terminal so the cockpit shows what a pane is
    /// (e.g. `Todo #30 - fixup pane titles`, `Agent: panopt-bot`,
    /// `Command: just check`) instead of the raw launch command. Only the
    /// Todos pane runs this - it is the cockpit gatekeeper and the only
    /// instance that loads every projection a title can reference; the
    /// other four plugin panes leaving terminal panes alone avoids duplicate
    /// host calls and racy clobbers.
    fn sync_pane_titles(&mut self) {
        if self.mode != Mode::Todos {
            return;
        }
        let mut alive: BTreeMap<u32, String> = BTreeMap::new();
        for p in &self.panes {
            let PaneId::Terminal(tid) = p.id else {
                continue;
            };
            let Some(title) = self.compose_pane_title(p) else {
                continue;
            };
            if self.last_pane_titles.get(&tid).map(String::as_str) != Some(title.as_str()) {
                rename_terminal_pane(tid, &title);
            }
            alive.insert(tid, title);
        }
        // Drop entries for panes that have gone away so the cache cannot
        // grow without bound across long sessions.
        self.last_pane_titles = alive;
    }

    /// Build the right-pane terminal title for one pane, or `None` to leave
    /// Zellij's default in place (plain shells: the running command is a
    /// fine title; we have nothing to add).
    fn compose_pane_title(&self, p: &PaneRow) -> Option<String> {
        match p.role {
            PaneRole::Viewer => Some(self.viewer_pane_title(p)),
            PaneRole::Agent => Some(self.agent_pane_title(p)),
            PaneRole::Process(id) => Some(self.process_pane_title(id)),
            PaneRole::Shell => None,
        }
    }

    /// Title for a viewer pane, derived from its routing file. Falls back to
    /// `Viewer` when the routing has not been written yet (the boot viewer
    /// before the first navigation) or the kind is unrecognized.
    fn viewer_pane_title(&self, p: &PaneRow) -> String {
        let Some(slot) = &p.viewer_slot else {
            return "Viewer".to_string();
        };
        let path = format!("/host/.panopt/.cockpit/viewer-{slot}.json");
        let body = fs::read_to_string(&path).unwrap_or_default();
        let (kind, id) = parse_viewer_routing(&body);
        viewer_title_for(kind.as_deref(), id, &self.todos, &self.scratchpads)
    }

    /// Title for an ad-hoc agent pane (one spawned by `n` in the Agents pane
    /// or the `panopt:spawn-agent` pipe). Uses the user-supplied label - or
    /// the `Agent N` fallback assigned by [`Self::sync_agent_labels`] - and
    /// adds an `Agent:` prefix only when the label does not already carry it.
    fn agent_pane_title(&self, p: &PaneRow) -> String {
        let label = self.agent_label(p);
        kind_prefixed_title("Agent", &label)
    }

    /// Title for a `panopt _process-run` pane. The process's kind in
    /// `processes.md` (`agent`/`command`/`terminal`) drives the prefix.
    fn process_pane_title(&self, id: u64) -> String {
        let Some(row) = self.processes.iter().find(|r| r.id == id) else {
            return format!("Process #{id}");
        };
        let prefix = match row.kind.as_str() {
            "agent" => "Agent",
            "command" => "Command",
            "terminal" => "Terminal",
            _ => return format!("Process #{id}: {}", row.label),
        };
        kind_prefixed_title(prefix, &row.label)
    }

    // --- data ---

    /// Re-read whichever projected index files this mode needs. The Todos
    /// pane reads only todos; Scratchpads only scratchpads; the Agents and
    /// Commands modes share processes.md plus the agent label projection.
    fn reload_data(&mut self) {
        match self.mode {
            Mode::Todos => {
                self.todos = read_index("/host/.panopt/todos.md");
                // The Todos pane is the cockpit gatekeeper and titles every
                // right-pane terminal in `sync_pane_titles`; it needs every
                // projection a title can reference, not just its own list.
                self.scratchpads = read_index("/host/.panopt/scratchpads.md");
                self.processes = read_processes("/host/.panopt/processes.md");
                self.read_agent_labels();
            }
            Mode::Scratchpads => {
                self.scratchpads = read_index("/host/.panopt/scratchpads.md");
            }
            Mode::Agents | Mode::Commands => {
                self.processes = read_processes("/host/.panopt/processes.md");
                self.read_agent_labels();
            }
            Mode::Terminals => {
                self.read_agent_labels();
            }
        }
    }

    /// Flatten the pane manifest into the content-pane list - suppressed
    /// panes included, since they are the hidden agents and terminals the
    /// sidebar still lists - and keep the designated slot pane pointing at a
    /// live pane.
    fn ingest_panes(&mut self, manifest: PaneManifest) {
        let mut tabs: Vec<&usize> = manifest.panes.keys().collect();
        tabs.sort();
        let mut rows = Vec::new();
        let mut focused_non_plugin: Option<PaneId> = None;
        let mut sidebar_focused_this_update = false;
        let mut saw_focused_pane = false;
        let mut focused_tab_this_update: Option<usize> = None;
        for tab in tabs {
            for p in &manifest.panes[tab] {
                // The transient `zellij action pipe` pane briefly steals
                // focus while a close-request pipe is in flight. Skip it
                // from focus tracking and from the pane list.
                if is_transient_pipe_pane(p) {
                    continue;
                }
                if p.is_focused {
                    saw_focused_pane = true;
                    focused_tab_this_update = Some(*tab);
                    if p.is_plugin {
                        // Any plugin pane focused = a cockpit plugin pane is
                        // focused. The cockpit is the only place plugins
                        // run, and the gate refuses close on any of the five.
                        sidebar_focused_this_update = true;
                    }
                }
                if p.is_plugin || !p.is_selectable {
                    continue;
                }
                let id = PaneId::Terminal(p.id);
                if p.is_focused {
                    focused_non_plugin = Some(id);
                }
                let role = classify_pane(p.terminal_command.as_deref());
                let viewer_slot = if matches!(role, PaneRole::Viewer) {
                    parse_viewer_slot(p.terminal_command.as_deref())
                } else {
                    None
                };
                rows.push(PaneRow {
                    id,
                    title: p.title.clone(),
                    focused: p.is_focused,
                    suppressed: p.is_suppressed,
                    exited: p.exited,
                    role,
                    viewer_slot,
                    tab: *tab,
                });
            }
        }
        rows.sort_by_key(|p| p.id);
        self.panes = rows;
        self.sync_agent_labels();
        if let Some(pane) = focused_non_plugin {
            self.slot_pane = Some(pane);
        }
        if saw_focused_pane {
            self.sidebar_focused = sidebar_focused_this_update;
            self.focused_tab = focused_tab_this_update;
        }
        if let Some(slot) = self.slot_pane {
            if !self.panes.iter().any(|p| p.id == slot) {
                self.slot_pane = None;
            }
        }
        if self.slot_pane.is_none() {
            self.slot_pane = self
                .panes
                .iter()
                .find(|p| !p.suppressed && p.role == PaneRole::Viewer)
                .or_else(|| self.panes.iter().find(|p| !p.suppressed))
                .map(|p| p.id);
        }
        self.sync_pane_titles();
    }

    fn pane_is_visible(&self, pane: PaneId) -> bool {
        self.panes.iter().any(|p| p.id == pane && !p.suppressed)
    }

    fn viewer_slot_of(&self, pane: PaneId) -> Option<String> {
        self.panes
            .iter()
            .find(|p| p.id == pane)
            .and_then(|p| p.viewer_slot.clone())
    }

    fn first_suppressed_viewer(&self) -> Option<PaneId> {
        self.panes
            .iter()
            .find(|p| p.role == PaneRole::Viewer && p.suppressed)
            .map(|p| p.id)
    }

    /// Allocate the next unique routing slot name for a viewer the plugin is
    /// about to spawn. The mode letter scopes the counter per-plugin-instance,
    /// so two panes spawning a viewer on the same tick still produce distinct
    /// names (e.g. `vt1` from Todos, `vs1` from Scratchpads).
    fn allocate_viewer_slot(&mut self) -> String {
        self.next_viewer_slot += 1;
        format!(
            "{SPAWNED_VIEWER_SLOT_PREFIX}{}{}",
            self.mode.letter(),
            self.next_viewer_slot
        )
    }

    fn process_pane(&self, id: u64) -> Option<PaneId> {
        self.panes
            .iter()
            .find(|p| p.role == PaneRole::Process(id) && !p.exited)
            .map(|p| p.id)
    }

    /// Keep `agent_labels` in step with the live agent panes: forget closed
    /// ones, give any agent still unlabelled a stable "Agent N" fallback.
    /// The Todos pane (the gatekeeper) projects the resulting map to the
    /// shared file so the other four panes pick up user-supplied labels.
    fn sync_agent_labels(&mut self) {
        let agent_ids: Vec<u32> = self
            .panes
            .iter()
            .filter(|p| p.role == PaneRole::Agent)
            .filter_map(|p| match p.id {
                PaneId::Terminal(tid) => Some(tid),
                PaneId::Plugin(_) => None,
            })
            .collect();
        self.agent_labels.retain(|tid, _| agent_ids.contains(tid));
        for tid in agent_ids {
            if !self.agent_labels.contains_key(&tid) {
                self.next_agent += 1;
                self.agent_labels
                    .insert(tid, format!("Agent {}", self.next_agent));
            }
        }
        if self.mode == Mode::Todos {
            write_agent_labels(&self.agent_labels);
        }
    }

    /// Read agent labels written by the gatekeeper from
    /// [`AGENT_LABELS_PATH`]. Merges user-supplied labels into this
    /// instance's `agent_labels` so non-Todos panes can display them.
    /// Defensive: a missing or malformed file leaves the map untouched.
    fn read_agent_labels(&mut self) {
        let Ok(body) = fs::read_to_string(AGENT_LABELS_PATH) else {
            return;
        };
        for (tid, label) in parse_agent_labels(&body) {
            self.agent_labels.insert(tid, label);
        }
    }

    fn agent_label(&self, p: &PaneRow) -> String {
        match p.id {
            PaneId::Terminal(tid) => self
                .agent_labels
                .get(&tid)
                .cloned()
                .unwrap_or_else(|| pane_label(p)),
            PaneId::Plugin(_) => pane_label(p),
        }
    }

    /// Step the todo filter forward (`forward=true`) or backward, rebuild
    /// the visible items, and refresh the frame title so the user sees the
    /// new filter without an extra keypress.
    fn cycle_todo_filter(&mut self, forward: bool) {
        if self.mode != Mode::Todos {
            return;
        }
        self.todo_filter = if forward {
            self.todo_filter.next()
        } else {
            self.todo_filter.prev()
        };
        self.rebuild_items();
        self.sync_frame_title();
    }

    /// Step one sort level forward or backward and refresh. `level` is 1
    /// (`1` / `!`) or 2 (`2` / `@`); other values are no-ops. The cursor
    /// resets to the top so the user lands on the head of the new ordering
    /// rather than wherever the previously-selected todo wound up.
    fn cycle_todo_sort(&mut self, level: u8, forward: bool) {
        if self.mode != Mode::Todos {
            return;
        }
        let slot = match level {
            1 => &mut self.todo_sort_1,
            2 => &mut self.todo_sort_2,
            _ => return,
        };
        *slot = if forward { slot.next() } else { slot.prev() };
        self.cursor = 0;
        self.scroll = 0;
        self.rebuild_items();
        self.sync_frame_title();
    }

    /// Rebuild this pane's item list from parsed data + live panes. The list
    /// is always a single flat sequence for the pane's mode.
    fn rebuild_items(&mut self) {
        let items: Vec<Item> = match self.mode {
            Mode::Todos => {
                let mut rows: Vec<&(u64, String)> = self
                    .todos
                    .iter()
                    .filter(|(_, label)| self.todo_filter.includes_label(label))
                    .collect();
                // Stable two-pass sort: level 2 first, then level 1, so
                // ties on level 1 keep the level-2 ordering.
                rows.sort_by(|a, b| self.todo_sort_2.cmp_rows(a, b));
                rows.sort_by(|a, b| self.todo_sort_1.cmp_rows(a, b));
                rows.into_iter()
                    .map(|(id, label)| Item {
                        label: format!("#{id} {label}"),
                        target: ItemTarget::Todo(*id),
                        live: false,
                    })
                    .collect()
            }
            Mode::Scratchpads => self
                .scratchpads
                .iter()
                .map(|(id, label)| Item {
                    label: format!("#{id} {label}"),
                    target: ItemTarget::Scratchpad(*id),
                    live: false,
                })
                .collect(),
            Mode::Agents => {
                // Agent-kind processes, plus ad-hoc `a`-spawned agent panes
                // that have no backing process row.
                let mut items: Vec<Item> = self
                    .processes
                    .iter()
                    .filter(|r| r.kind == "agent")
                    .map(|r| Item {
                        label: r.label.clone(),
                        target: ItemTarget::Process(r.id),
                        live: self.process_pane(r.id).is_some(),
                    })
                    .collect();
                for p in self.panes.iter().filter(|p| p.role == PaneRole::Agent) {
                    items.push(Item {
                        label: self.agent_label(p),
                        target: ItemTarget::Pane(p.id),
                        live: true,
                    });
                }
                items
            }
            Mode::Commands => self
                .processes
                .iter()
                .filter(|r| r.kind == "command")
                .map(|r| Item {
                    label: r.label.clone(),
                    target: ItemTarget::Process(r.id),
                    live: self.process_pane(r.id).is_some(),
                })
                .collect(),
            Mode::Terminals => self
                .panes
                .iter()
                .filter(|p| p.role == PaneRole::Shell)
                .map(|p| Item {
                    label: pane_label(p),
                    target: ItemTarget::Pane(p.id),
                    live: p.focused,
                })
                .collect(),
        };
        self.items = items;
        self.clamp_cursor();
    }

    /// Keep cursor + scroll inside the item bounds after a rebuild.
    fn clamp_cursor(&mut self) {
        if self.items.is_empty() {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }
        if self.cursor >= self.items.len() {
            self.cursor = self.items.len() - 1;
        }
        let visible = self.list_rows();
        let max_scroll = self.items.len().saturating_sub(visible);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }

    // --- cursor / scroll ---

    /// Step the cursor by `delta` rows and auto-scroll when the cursor
    /// reaches either edge of the visible window. Returns `true` when the
    /// cursor moved.
    fn move_cursor(&mut self, delta: i64) -> bool {
        if self.items.is_empty() {
            return false;
        }
        let count = self.items.len();
        let visible = self.list_rows();
        let new = (self.cursor as i64 + delta).clamp(0, count as i64 - 1) as usize;
        let moved = new != self.cursor;
        self.cursor = new;
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + visible {
            self.scroll = self.cursor + 1 - visible;
        }
        moved
    }

    /// The target of the cursor's current item, or `None` when the list is
    /// empty.
    fn focused_target(&self) -> Option<ItemTarget> {
        self.items.get(self.cursor).map(|i| i.target.clone())
    }

    /// Preview the cursor's row in the slot, leaving focus on this plugin
    /// pane. A document re-points every viewer; a running pane is routed
    /// into the slot or - when it is already visible in another split - the
    /// slot clears instead, since a TTY cannot be in two places at once.
    fn preview_cursor(&mut self) {
        match self.focused_target() {
            Some(ItemTarget::Todo(id)) => self.open_document("todo", Some(id), false),
            Some(ItemTarget::Scratchpad(id)) => self.open_document("scratchpad", Some(id), false),
            Some(ItemTarget::Process(id)) => match self.process_pane(id) {
                Some(pane) => self.route_pane_to_slot(pane, false),
                None => self.clear_slot(),
            },
            Some(ItemTarget::Pane(pane)) => self.route_pane_to_slot(pane, false),
            None => self.clear_slot(),
        }
    }

    // --- input ---

    fn handle_key(&mut self, key: KeyWithModifier) -> bool {
        self.clear_gate_refusal();
        // While the help overlay is up, every key dismisses it - including
        // a second `?`. The key still counts as handled so the dismissal
        // alone is not also interpreted as an action.
        if self.show_help {
            self.show_help = false;
            return true;
        }
        match key.bare_key {
            BareKey::Up => {
                if self.move_cursor(-1) {
                    self.preview_cursor();
                }
            }
            BareKey::Down => {
                if self.move_cursor(1) {
                    self.preview_cursor();
                }
            }
            BareKey::PageUp => {
                let step = self.page_step();
                if self.move_cursor(-(step as i64)) {
                    self.preview_cursor();
                }
            }
            BareKey::PageDown => {
                let step = self.page_step();
                if self.move_cursor(step as i64) {
                    self.preview_cursor();
                }
            }
            BareKey::Home => {
                if self.move_cursor(-(self.items.len() as i64)) {
                    self.preview_cursor();
                }
            }
            BareKey::End => {
                if self.move_cursor(self.items.len() as i64) {
                    self.preview_cursor();
                }
            }
            BareKey::Enter => self.activate_cursor(),
            BareKey::Char('e') if self.mode == Mode::Todos => self.edit_focused_todo(),
            // `n` creates a new item of the current pane type. The kind
            // tracks the mode so a single binding gives the user "new"
            // semantics everywhere it makes sense.
            BareKey::Char('n') if self.mode == Mode::Todos => {
                self.open_document("new-todo", None, true)
            }
            BareKey::Char('n') if self.mode == Mode::Scratchpads => {
                self.open_document("new-scratchpad", None, true)
            }
            BareKey::Char('n') if self.mode == Mode::Agents => self.spawn_agent_pane(None),
            BareKey::Char('L') => self.open_mode_list(true),
            // Filter (Todos only). Forward = `f`, backward = `F`.
            BareKey::Char('f') if self.mode == Mode::Todos => self.cycle_todo_filter(true),
            BareKey::Char('F') if self.mode == Mode::Todos => self.cycle_todo_filter(false),
            // Two-level sort (Todos only). Forward = `1`/`2`, backward =
            // `!`/`@` (the shifted versions of the same digit). This keeps
            // the sort and filter bindings disjoint from any letter key so
            // the per-mode letter actions (`u` / `d` / `x`) never have to
            // be qualified by mode.
            BareKey::Char('1') if self.mode == Mode::Todos => self.cycle_todo_sort(1, true),
            BareKey::Char('!') if self.mode == Mode::Todos => self.cycle_todo_sort(1, false),
            BareKey::Char('2') if self.mode == Mode::Todos => self.cycle_todo_sort(2, true),
            BareKey::Char('@') if self.mode == Mode::Todos => self.cycle_todo_sort(2, false),
            // Delete the focused item. Dispatches by mode; see [`delete_focused`].
            BareKey::Char('x') => self.delete_focused(),
            // Start / focus the focused runnable. Identical to Enter for
            // process-backed and pane-backed items; no-op for docs.
            BareKey::Char('u') => self.start_focused(),
            // Stop the focused runnable: close its pane. The process row
            // (if any) stays - the user can `u` to relaunch it.
            BareKey::Char('d') => self.stop_focused(),
            // Help overlay: any subsequent key dismisses it. Handled above
            // the match so `?` falls through the dismissal path on a second
            // press.
            BareKey::Char('?') => self.show_help = true,
            // `/` is a stub - the key is reserved for future filter behavior
            // but does nothing today.
            BareKey::Char('/') => {}
            _ => return false,
        }
        true
    }

    fn handle_mouse(&mut self, mouse: Mouse) -> bool {
        match mouse {
            Mouse::LeftClick(line, _col) => {
                if line < 0 {
                    return false;
                }
                let idx = self.scroll + line as usize;
                if idx >= self.items.len() {
                    return false;
                }
                self.cursor = idx;
                self.activate_item(idx, false);
                if let Some(plugin) = self.plugin_pane {
                    focus_pane_with_id(plugin, false, false);
                }
                true
            }
            Mouse::ScrollUp(_) => {
                if self.move_cursor(-1) {
                    self.preview_cursor();
                }
                true
            }
            Mouse::ScrollDown(_) => {
                if self.move_cursor(1) {
                    self.preview_cursor();
                }
                true
            }
            _ => false,
        }
    }

    /// Step size for PageUp/PageDown - one screenful of visible items.
    fn page_step(&self) -> usize {
        self.list_rows()
    }

    /// Number of body rows available for items. In the Todos pane the
    /// bottom body row is reserved as a status line showing the two sort
    /// axes' current values, so the list area is one row shorter than what
    /// Zellij hands us. Every scroll / clamp / page-step computation goes
    /// through this single source of truth so the status-line row never
    /// gets covered by an item or counted toward the visible window.
    fn list_rows(&self) -> usize {
        let reserve = if self.mode == Mode::Todos { 1 } else { 0 };
        self.last_rows.saturating_sub(reserve).max(1)
    }

    /// Act on the cursor's row from the keyboard (Enter): focus moves onto
    /// the content pane.
    fn activate_cursor(&mut self) {
        if let Some(idx) = self.items.get(self.cursor).map(|_| self.cursor) {
            self.activate_item(idx, true);
        }
    }

    /// Act on item `idx`. `focus` moves keyboard focus onto the content
    /// pane (Enter); a click passes `false` to stay on this plugin pane.
    fn activate_item(&mut self, idx: usize, focus: bool) {
        let Some(target) = self.items.get(idx).map(|i| i.target.clone()) else {
            return;
        };
        match target {
            ItemTarget::Todo(id) => self.open_document("todo", Some(id), focus),
            ItemTarget::Scratchpad(id) => self.open_document("scratchpad", Some(id), focus),
            ItemTarget::Process(id) => self.activate_process(id, focus),
            ItemTarget::Pane(pane) => self.route_pane_to_slot(pane, focus),
        }
    }

    /// Open the full-list view in the slot for modes that have one. Todos
    /// and Scratchpads display their respective lists; the agent/command/
    /// terminal modes are no-ops because their lists are already shown whole.
    fn open_mode_list(&mut self, focus: bool) {
        match self.mode {
            Mode::Todos => self.open_document("todo-list", None, focus),
            Mode::Scratchpads => self.open_document("scratchpad-list", None, focus),
            _ => {}
        }
    }

    /// Open the in-slot todo form for the focused todo, if one is focused.
    /// Identical to pressing Enter on the same row, but with focus forced
    /// into the form so the user can type immediately.
    fn edit_focused_todo(&mut self) {
        if let Some(ItemTarget::Todo(id)) = self.focused_target() {
            self.open_document("todo", Some(id), true);
        }
    }

    /// Delete the focused item. Todos and process rows go through the
    /// `panopt` CLI (the daemon owns the durable state); ad-hoc agent panes
    /// and plain shell terminals are just closed via the host. Scratchpad
    /// delete has no CLI yet - surface a refusal so the user knows the
    /// keypress was seen.
    fn delete_focused(&mut self) {
        let Some(item) = self.items.get(self.cursor) else {
            return;
        };
        let target = item.target.clone();
        let label = item.label.clone();
        let Some(cwd) = self.launch_cwd() else {
            return;
        };
        match target {
            ItemTarget::Todo(id) => self.spawn_delete_gate_dialog("todo", id, &label, cwd),
            ItemTarget::Scratchpad(_) => {
                self.refuse_gate("scratchpad delete not yet supported");
            }
            ItemTarget::Process(id) => self.spawn_delete_gate_dialog("process", id, &label, cwd),
            // A "pane" target is a transient view (a terminal pane, an ad-hoc
            // agent pane) - closing it does not delete any persistent record,
            // so no confirmation is needed.
            ItemTarget::Pane(pane) => {
                close_pane_with_id(pane);
            }
        }
    }

    /// Float the delete-confirmation dialog (`panopt _delete-gate`). On `y`
    /// the dialog pipes `panopt:delete-gate-decision` back; the actual delete
    /// then runs from [`Self::handle_delete_decision`] so the dialog stays
    /// purely advisory.
    fn spawn_delete_gate_dialog(&mut self, kind: &str, id: u64, label: &str, cwd: PathBuf) {
        let args = vec![
            "_delete-gate".to_string(),
            "--kind".to_string(),
            kind.to_string(),
            "--id".to_string(),
            id.to_string(),
            "--label".to_string(),
            label.to_string(),
            "--port".to_string(),
            self.port.clone(),
        ];
        open_command_pane_floating(
            CommandToRun {
                path: PathBuf::from(&self.panopt_bin),
                args,
                cwd: Some(cwd),
            },
            None,
            BTreeMap::new(),
        );
    }

    /// Handle the `panopt:delete-gate-decision` pipe: parse `kind`/`id`/
    /// `decision` and, when the user confirmed, run the matching destructive
    /// CLI. Mirrors [`Self::handle_gate_decision`] in shape so a reader who
    /// knows one knows the other.
    fn handle_delete_decision(&mut self, payload: Option<&str>) {
        let Some(payload) = payload else { return };
        let mut kind: Option<&str> = None;
        let mut id: Option<u64> = None;
        let mut decision: Option<&str> = None;
        for kv in payload.split(';') {
            let (k, v) = match kv.split_once('=') {
                Some(pair) => pair,
                None => continue,
            };
            match k {
                "kind" => kind = Some(v),
                "id" => id = v.parse().ok(),
                "decision" => decision = Some(v),
                _ => {}
            }
        }
        if decision != Some("delete") {
            return;
        }
        let (Some(kind), Some(id)) = (kind, id) else {
            return;
        };
        let Some(cwd) = self.launch_cwd() else { return };
        match kind {
            "todo" => {
                self.run_panopt(&["todo", "rm", &id.to_string(), "--port", &self.port], cwd);
            }
            "process" => {
                // Process delete also tears down its live pane (if any), the
                // same way the pre-gate `delete_focused` used to. The pane
                // close runs before the daemon delete so the user does not
                // see a stale row briefly.
                if let Some(pane) = self.process_pane(id) {
                    close_pane_with_id(pane);
                }
                self.run_panopt(
                    &["process", "delete", &id.to_string(), "--port", &self.port],
                    cwd,
                );
            }
            // `scratchpad` and `agent-tool` are not yet reachable through the
            // sidebar's delete keybind, so a stray pipe for them is ignored
            // rather than executed against a missing CLI.
            _ => {}
        }
    }

    /// Start (or focus) the focused runnable. Same effect as Enter but
    /// without shifting keyboard focus onto the content pane - the user
    /// stays on the sidebar so the next key still drives the list.
    fn start_focused(&mut self) {
        if let Some(idx) = self.items.get(self.cursor).map(|_| self.cursor) {
            self.activate_item(idx, false);
        }
    }

    /// Stop the focused runnable: close its content pane. The underlying
    /// process row (when any) is left intact so `u` can spawn it again. For
    /// docs (todos / scratchpads) this is a no-op since they have nothing
    /// to stop.
    fn stop_focused(&mut self) {
        match self.focused_target() {
            Some(ItemTarget::Process(id)) => {
                if let Some(pane) = self.process_pane(id) {
                    close_pane_with_id(pane);
                }
            }
            Some(ItemTarget::Pane(pane)) => {
                close_pane_with_id(pane);
            }
            _ => {}
        }
    }

    /// Run `panopt <args>` in the background, scoped to the project cwd.
    /// The plugin does not subscribe to `RunCommandResult` - the 1-second
    /// reload timer picks up the mutated projection within a tick.
    fn run_panopt(&self, args: &[&str], cwd: PathBuf) {
        let mut full: Vec<&str> = Vec::with_capacity(args.len() + 1);
        full.push(self.panopt_bin.as_str());
        full.extend_from_slice(args);
        run_command_with_env_variables_and_cwd(&full, BTreeMap::new(), cwd, BTreeMap::new());
    }

    // --- slot routing ---

    fn open_document(&mut self, kind: &str, id: Option<u64>, focus: bool) {
        if !self.permitted {
            return;
        }
        self.ensure_viewer_in_slot(kind, id, focus);
    }

    fn clear_slot(&mut self) {
        self.ensure_viewer_in_slot("empty", None, false);
    }

    fn activate_process(&mut self, id: u64, focus: bool) {
        if let Some(pane) = self.process_pane(id) {
            self.route_pane_to_slot(pane, focus);
            return;
        }
        let args = vec![
            "_process-run".to_string(),
            "--port".to_string(),
            self.port.clone(),
            id.to_string(),
        ];
        self.spawn_in_slot(args, focus);
    }

    fn route_pane_to_slot(&mut self, pane: PaneId, focus: bool) {
        if self.pane_is_visible(pane) && self.slot_pane != Some(pane) {
            if focus {
                focus_pane_with_id(pane, false, false);
            } else {
                self.clear_slot();
            }
            return;
        }
        self.show_in_slot(pane, focus);
    }

    fn ensure_viewer_in_slot(&mut self, kind: &str, id: Option<u64>, focus: bool) {
        if let Some(slot) = self.slot_pane {
            if self.pane_is_visible(slot) {
                if let Some(slot_name) = self.viewer_slot_of(slot) {
                    write_routing(kind, id, &slot_name);
                    if focus {
                        focus_pane_with_id(slot, false, false);
                    }
                    // Arrowing through items re-routes the existing viewer in
                    // place; no PaneUpdate fires, so the pane title would
                    // otherwise stay frozen on the previously-routed item
                    // until the next focus change.
                    self.sync_pane_titles();
                    return;
                }
            }
        }
        if let Some(viewer) = self.first_suppressed_viewer() {
            if let Some(slot_name) = self.viewer_slot_of(viewer) {
                write_routing(kind, id, &slot_name);
            }
            self.show_in_slot(viewer, focus);
            self.sync_pane_titles();
            return;
        }
        let slot_name = self.allocate_viewer_slot();
        write_routing(kind, id, &slot_name);
        let mut args = vec![
            "_viewer".to_string(),
            "--slot".to_string(),
            slot_name,
            "--port".to_string(),
            self.port.clone(),
            "--kind".to_string(),
            kind.to_string(),
        ];
        if let Some(id) = id {
            args.push("--id".to_string());
            args.push(id.to_string());
        }
        self.spawn_in_slot(args, focus);
    }

    fn show_in_slot(&mut self, pane: PaneId, focus: bool) {
        let is_slot = self.slot_pane == Some(pane);
        if !is_slot {
            match self.slot_pane {
                Some(slot) => replace_pane_with_existing_pane(slot, pane, true),
                None => show_pane_with_id(pane, false, false),
            }
            self.slot_pane = Some(pane);
        }
        if focus {
            focus_pane_with_id(pane, false, false);
        } else if !is_slot {
            if let Some(plugin) = self.plugin_pane {
                focus_pane_with_id(plugin, false, false);
            }
        }
    }

    fn spawn_in_slot(&mut self, args: Vec<String>, focus: bool) -> Option<PaneId> {
        let ws = self.launch_cwd()?;
        let command = CommandToRun {
            path: PathBuf::from(&self.panopt_bin),
            args,
            cwd: Some(ws),
        };
        let new = match self.slot_pane {
            Some(slot) => {
                open_command_pane_in_place_of_pane_id(slot, command, false, BTreeMap::new())
            }
            None => open_command_pane(command, BTreeMap::new()),
        };
        if let Some(pane) = new {
            self.slot_pane = Some(pane);
            if focus {
                focus_pane_with_id(pane, false, false);
            } else if let Some(plugin) = self.plugin_pane {
                focus_pane_with_id(plugin, false, false);
            }
        }
        new
    }

    fn spawn_blank_pane(&mut self) {
        // The five sidebar panes are part of the cockpit shell, not slots
        // a new pane belongs in - splitting them shreds the fixed layout.
        // Refuse here so Alt-N and the rewritten pane-mode keys (`n`/`d`/
        // `r`/`s`) all funnel through the same gate.
        if self.sidebar_focused {
            self.refuse_gate("cannot create panes from the sidebar");
            return;
        }
        let Some(ws) = self.launch_cwd() else {
            return;
        };
        let slot_name = self.allocate_viewer_slot();
        write_routing("empty", None, &slot_name);
        let args = vec![
            "_viewer".to_string(),
            "--slot".to_string(),
            slot_name,
            "--port".to_string(),
            self.port.clone(),
            "--kind".to_string(),
            "empty".to_string(),
        ];
        open_command_pane(
            CommandToRun {
                path: PathBuf::from(&self.panopt_bin),
                args,
                cwd: Some(ws),
            },
            BTreeMap::new(),
        );
    }

    /// Spawn a new agent pane and label it. Only the Todos (gatekeeper) pane
    /// reaches this from a pipe; from the keyboard, the Agents pane spawns
    /// an unnamed agent via `n`.
    fn spawn_agent_pane(&mut self, id: Option<&str>) {
        let mut args = vec!["_agent".to_string()];
        if let Some(id) = id {
            args.push("--id".to_string());
            args.push(id.to_string());
        }
        let Some(PaneId::Terminal(tid)) = self.spawn_in_slot(args, true) else {
            return;
        };
        let label = match id {
            Some(given) => given.to_string(),
            None => {
                self.next_agent += 1;
                format!("Agent {}", self.next_agent)
            }
        };
        self.agent_labels.insert(tid, label);
        // Project labels right away so the other four panes pick up the
        // new agent's name on their next reload tick.
        write_agent_labels(&self.agent_labels);
    }

    fn launch_cwd(&self) -> Option<PathBuf> {
        if !self.permitted {
            return None;
        }
        self.ws.as_ref().map(PathBuf::from)
    }

    // --- close gate ---

    fn gate_close_focus(&mut self) {
        if !self.permitted {
            return;
        }
        if self.sidebar_focused {
            // Any of the five plugin panes is part of the cockpit shell -
            // not a closeable artifact. Absolute refusal; no dialog.
            self.refuse_gate("cannot close the sidebar");
            return;
        }
        let Some(target) = self.slot_pane else {
            return;
        };
        if let Some(item) = self.pane_active(target) {
            self.spawn_close_gate_dialog("focus", Some(target), &[item]);
            return;
        }
        close_pane_with_id(target);
    }

    fn gate_close_tab(&mut self) {
        if !self.permitted {
            return;
        }
        let Some(tab) = self.focused_tab else {
            return;
        };
        let active = self.active_in_tab(tab);
        if !active.is_empty() {
            self.spawn_close_gate_dialog("tab", None, &active);
            return;
        }
        close_focused_tab();
    }

    fn gate_quit(&mut self) {
        if !self.permitted {
            return;
        }
        let active = self.active_anywhere();
        if !active.is_empty() {
            self.spawn_close_gate_dialog("quit", None, &active);
            return;
        }
        quit_zellij();
    }

    fn spawn_close_gate_dialog(
        &mut self,
        scope: &str,
        target: Option<PaneId>,
        active: &[ActiveItem],
    ) {
        let Some(ws) = self.launch_cwd() else {
            self.refuse_gate(&format!(
                "{} active - permissions not yet granted",
                active.len()
            ));
            return;
        };
        let items_arg = active
            .iter()
            .map(|a| {
                format!(
                    "{}:{}",
                    a.kind.label(),
                    a.label.replace(';', ",").replace(':', "-")
                )
            })
            .collect::<Vec<_>>()
            .join(";");
        let mut args = vec![
            "_close-gate".to_string(),
            "--scope".to_string(),
            scope.to_string(),
            "--items".to_string(),
            items_arg,
            "--port".to_string(),
            self.port.clone(),
        ];
        if let Some(PaneId::Terminal(tid)) = target {
            args.push("--target-pane".to_string());
            args.push(tid.to_string());
        }
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

    fn handle_gate_decision(&mut self, payload: Option<&str>) {
        let Some(payload) = payload else { return };
        let mut scope: Option<&str> = None;
        let mut target_pane: Option<u32> = None;
        let mut decision: Option<&str> = None;
        for kv in payload.split(';') {
            let (k, v) = match kv.split_once('=') {
                Some(pair) => pair,
                None => continue,
            };
            match k {
                "scope" => scope = Some(v),
                "target_pane" => target_pane = v.parse().ok(),
                "decision" => decision = Some(v),
                _ => {}
            }
        }
        if decision != Some("close") {
            return;
        }
        self.clear_gate_refusal();
        match scope {
            Some("focus") => {
                if let Some(tid) = target_pane {
                    close_pane_with_id(PaneId::Terminal(tid));
                }
            }
            Some("tab") => close_focused_tab(),
            Some("quit") => quit_zellij(),
            _ => {}
        }
    }

    fn pane_active(&self, pane: PaneId) -> Option<ActiveItem> {
        let p = self.panes.iter().find(|p| p.id == pane)?;
        if p.exited {
            return None;
        }
        if matches!(p.role, PaneRole::Viewer) {
            return None;
        }
        if let PaneRole::Process(rid) = p.role {
            if let Some(r) = self.processes.iter().find(|r| r.id == rid) {
                return match r.kind.as_str() {
                    "agent" => Some(ActiveItem {
                        label: r.label.clone(),
                        kind: ActiveKind::Agent,
                        pane,
                    }),
                    "command" => Some(ActiveItem {
                        label: r.label.clone(),
                        kind: ActiveKind::Command,
                        pane,
                    }),
                    "terminal" => self.pane_active_terminal(pane, &r.label),
                    _ => None,
                };
            }
        }
        let label = if matches!(p.role, PaneRole::Agent) {
            self.agent_label(p)
        } else {
            pane_label(p)
        };
        self.pane_active_terminal(pane, &label)
    }

    fn pane_active_terminal(&self, pane: PaneId, label: &str) -> Option<ActiveItem> {
        let argv = get_pane_running_command(pane).ok()?;
        let exe = argv.first()?;
        let basename = std::path::Path::new(exe)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(exe.as_str());
        if is_user_shell(basename) {
            None
        } else {
            Some(ActiveItem {
                label: label.to_string(),
                kind: ActiveKind::Terminal,
                pane,
            })
        }
    }

    fn active_in_tab(&self, tab: usize) -> Vec<ActiveItem> {
        self.panes
            .iter()
            .filter(|p| p.tab == tab)
            .filter_map(|p| self.pane_active(p.id))
            .collect()
    }

    fn active_anywhere(&self) -> Vec<ActiveItem> {
        self.panes
            .iter()
            .filter_map(|p| self.pane_active(p.id))
            .collect()
    }

    fn refuse_gate(&mut self, reason: &str) {
        self.last_gate_refusal = Some(reason.to_string());
        self.sync_frame_title();
    }

    fn clear_gate_refusal(&mut self) {
        if self.last_gate_refusal.is_none() {
            return;
        }
        self.last_gate_refusal = None;
        self.sync_frame_title();
    }
}

#[derive(Clone, Debug)]
struct ActiveItem {
    label: String,
    kind: ActiveKind,
    #[allow(dead_code)]
    pane: PaneId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActiveKind {
    Agent,
    Command,
    Terminal,
}

impl ActiveKind {
    fn label(self) -> &'static str {
        match self {
            ActiveKind::Agent => "agent",
            ActiveKind::Command => "command",
            ActiveKind::Terminal => "terminal",
        }
    }
}

fn is_user_shell(basename: &str) -> bool {
    matches!(
        basename,
        "zsh" | "bash" | "fish" | "sh" | "dash" | "ksh" | "tcsh" | "nu" | "ash" | "elvish"
    )
}

fn is_transient_pipe_pane(p: &PaneInfo) -> bool {
    p.terminal_command.as_deref().is_some_and(|c| {
        c.contains("zellij") && c.contains("action") && c.contains("pipe") && c.contains("panopt:")
    })
}

fn classify_pane(command: Option<&str>) -> PaneRole {
    let Some(cmd) = command else {
        return PaneRole::Shell;
    };
    if cmd.contains("_viewer") {
        PaneRole::Viewer
    } else if cmd.contains("_process-run") {
        match cmd
            .split_whitespace()
            .filter_map(|t| t.parse::<u64>().ok())
            .next_back()
        {
            Some(id) => PaneRole::Process(id),
            None => PaneRole::Shell,
        }
    } else if cmd.contains("_agent") {
        PaneRole::Agent
    } else {
        PaneRole::Shell
    }
}

fn pane_label(p: &PaneRow) -> String {
    let title = if p.title.trim().is_empty() {
        "(untitled)"
    } else {
        p.title.trim()
    };
    if p.exited {
        format!("{title} (exited)")
    } else {
        title.to_string()
    }
}

/// Write the viewer's routing file `.panopt/.cockpit/viewer-<slot>.json`.
/// Each viewer pane owns its own `slot` token, so writes target one viewer.
fn write_routing(kind: &str, id: Option<u64>, slot: &str) {
    let dir = "/host/.panopt/.cockpit";
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let payload = match id {
        Some(id) => format!("{{\"kind\":\"{kind}\",\"id\":{id}}}"),
        None => format!("{{\"kind\":\"{kind}\"}}"),
    };
    let target = format!("{dir}/viewer-{slot}.json");
    let tmp = format!("{dir}/.viewer-{slot}.tmp");
    if fs::write(&tmp, payload).is_ok() {
        let _ = fs::rename(&tmp, &target);
    }
}

/// Project the agent-label map to [`AGENT_LABELS_PATH`] atomically (temp +
/// rename). Tiny JSON-ish format: `{"<tid>":"<label>",...}`. Labels never
/// embed `"` so a hand-rolled serializer is enough and avoids dragging in a
/// JSON dep for one tiny file.
fn write_agent_labels(labels: &BTreeMap<u32, String>) {
    let dir = "/host/.panopt/.cockpit";
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let mut body = String::from("{");
    for (i, (tid, label)) in labels.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        let safe = label.replace('\\', "\\\\").replace('"', "\\\"");
        body.push_str(&format!("\"{tid}\":\"{safe}\""));
    }
    body.push('}');
    let tmp = format!("{dir}/.agent-labels.tmp");
    if fs::write(&tmp, body).is_ok() {
        let _ = fs::rename(&tmp, AGENT_LABELS_PATH);
    }
}

/// Parse the agent-label projection back into `(tid, label)` pairs. Tolerant
/// of an empty/malformed file: returns an empty iterator on any parse error.
fn parse_agent_labels(body: &str) -> Vec<(u32, String)> {
    let body = body.trim();
    let Some(inner) = body.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
        return Vec::new();
    };
    if inner.trim().is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for entry in split_top_level(inner, ',') {
        let entry = entry.trim();
        let Some(colon) = entry.find(':') else {
            continue;
        };
        let key = entry[..colon].trim();
        let value = entry[colon + 1..].trim();
        let Some(tid) = key
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(label) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
            continue;
        };
        let unescaped = label.replace("\\\"", "\"").replace("\\\\", "\\");
        out.push((tid, unescaped));
    }
    out
}

/// Split `body` on `sep`, respecting `"..."` strings so a separator inside a
/// label does not split the entry. The projection writer escapes `"` and `\`
/// in labels, so the only thing this needs to dodge is unescaped `,` inside
/// a string.
fn split_top_level(body: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut escape = false;
    for c in body.chars() {
        if escape {
            current.push(c);
            escape = false;
            continue;
        }
        if c == '\\' && in_string {
            current.push(c);
            escape = true;
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            current.push(c);
            continue;
        }
        if c == sep && !in_string {
            out.push(std::mem::take(&mut current));
            continue;
        }
        current.push(c);
    }
    out.push(current);
    out
}

fn parse_viewer_slot(command: Option<&str>) -> Option<String> {
    let cmd = command?;
    let mut tokens = cmd.split_whitespace();
    while let Some(t) = tokens.next() {
        if t == "--slot" {
            return tokens.next().map(|s| s.to_string());
        }
    }
    None
}

/// Parse a viewer routing file body - the JSON-ish payload written by
/// [`write_routing`] - back into `(kind, id)`. Tolerant of an empty or
/// malformed body: returns `(None, None)` so callers fall back to the
/// generic `Viewer` title.
fn parse_viewer_routing(body: &str) -> (Option<String>, Option<u64>) {
    let trimmed = body.trim();
    let Some(inner) = trimmed.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
        return (None, None);
    };
    let mut kind: Option<String> = None;
    let mut id: Option<u64> = None;
    for entry in inner.split(',') {
        let entry = entry.trim();
        let Some(colon) = entry.find(':') else {
            continue;
        };
        let key = entry[..colon].trim();
        let value = entry[colon + 1..].trim();
        let key = key.strip_prefix('"').and_then(|s| s.strip_suffix('"'));
        match key {
            Some("kind") => {
                kind = value
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .map(|s| s.to_string());
            }
            Some("id") => {
                id = value.parse::<u64>().ok();
            }
            _ => {}
        }
    }
    (kind, id)
}

/// Compose the viewer-pane title for a `(kind, id)` routing pair. The
/// projection indexes are looked up so the title carries the resource's
/// own name (e.g. `Todo #30 - fixup pane titles`) rather than just its id.
fn viewer_title_for(
    kind: Option<&str>,
    id: Option<u64>,
    todos: &[(u64, String)],
    scratchpads: &[(u64, String)],
) -> String {
    match (kind, id) {
        (None, _) | (Some("empty"), _) => "Viewer".to_string(),
        (Some("todo"), Some(id)) => match lookup_title(todos, id) {
            Some(t) => format!("Todo #{id} - {t}"),
            None => format!("Todo #{id}"),
        },
        (Some("scratchpad"), Some(id)) => match lookup_title(scratchpads, id) {
            Some(t) => format!("Scratchpad #{id} - {t}"),
            None => format!("Scratchpad #{id}"),
        },
        (Some("todo-list"), _) => "Todos".to_string(),
        (Some("scratchpad-list"), _) => "Scratchpads".to_string(),
        (Some("new-todo"), _) => "New todo".to_string(),
        (Some("new-scratchpad"), _) => "New scratchpad".to_string(),
        _ => "Viewer".to_string(),
    }
}

/// Look up an index entry's label by id, stripping the trailing
/// `" - status, priority"` (todos) or `" - updated ..."` (scratchpads)
/// suffix that the projection format appends. The result is the bare title
/// the user typed.
fn lookup_title(index: &[(u64, String)], id: u64) -> Option<String> {
    let label = index.iter().find(|(i, _)| *i == id).map(|(_, l)| l)?;
    let trimmed = match label.rfind(" - ") {
        Some(dash) => label[..dash].trim(),
        None => label.trim(),
    };
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Prefix `label` with `kind: ` unless `label` already begins with that
/// kind (case-insensitive). Avoids the silly `Agent: Agent 1` for the
/// default ad-hoc-agent label while still tagging user-named ones like
/// `Agent: panopt-bot`.
fn kind_prefixed_title(kind: &str, label: &str) -> String {
    let label = label.trim();
    if label.is_empty() {
        return kind.to_string();
    }
    let lk = label.to_lowercase();
    if lk == kind.to_lowercase() || lk.starts_with(&format!("{} ", kind.to_lowercase())) {
        label.to_string()
    } else {
        format!("{kind}: {label}")
    }
}

fn read_index(path: &str) -> Vec<(u64, String)> {
    match fs::read_to_string(path) {
        Ok(body) => body.lines().filter_map(parse_index_line).collect(),
        Err(_) => Vec::new(),
    }
}

fn read_processes(path: &str) -> Vec<ProcessRow> {
    match fs::read_to_string(path) {
        Ok(body) => body.lines().filter_map(parse_process_line).collect(),
        Err(_) => Vec::new(),
    }
}

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

/// Parse one `- [kind] #id label [(from #N)]` line from `processes.md`. The
/// trailing `(from #N)` (when present) names the source agent tool and is
/// dropped from `label`.
fn parse_process_line(line: &str) -> Option<ProcessRow> {
    let rest = line.trim().strip_prefix("- [")?;
    let close = rest.find(']')?;
    let kind = rest[..close].to_string();
    let after = rest[close + 1..].trim_start().strip_prefix('#')?;
    let space = after.find(' ')?;
    let id: u64 = after[..space].parse().ok()?;
    let mut label = after[space + 1..].trim().to_string();
    if let Some(from_at) = label.rfind(" (from #") {
        if label.ends_with(')') {
            label.truncate(from_at);
        }
    }
    Some(ProcessRow { kind, id, label })
}

/// The ANSI styling a printed row carries.
#[derive(Clone, Copy)]
enum Style {
    Normal,
    Dim,
}

/// Truncate `content` to `cols` and wrap it in the SGR codes for `style`,
/// with the focused row reversed. The codes are added after truncation so
/// they never count toward the width.
fn paint(content: &str, cols: usize, style: Style, focused: bool) -> String {
    let truncated: String = content.chars().take(cols).collect();
    let mut codes: Vec<&str> = Vec::new();
    if focused {
        codes.push("7");
    }
    match style {
        Style::Dim => codes.push("2"),
        Style::Normal => {}
    }
    if codes.is_empty() {
        truncated
    } else {
        format!("\u{1b}[{}m{}\u{1b}[0m", codes.join(";"), truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_with(mode: Mode) -> PanoptPane {
        PanoptPane {
            mode,
            mode_known: true,
            permitted: true,
            last_rows: 10,
            ..PanoptPane::default()
        }
    }

    fn todos_pane(n: usize) -> PanoptPane {
        let mut pane = pane_with(Mode::Todos);
        pane.todos = (0..n).map(|i| (i as u64, format!("todo {i}"))).collect();
        pane.rebuild_items();
        pane
    }

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
        assert!(parse_index_line("").is_none());
    }

    #[test]
    fn parses_a_process_line() {
        let row = parse_process_line("- [agent] #1 NASTL-Mediator").unwrap();
        assert_eq!(row.kind, "agent");
        assert_eq!(row.id, 1);
        assert_eq!(row.label, "NASTL-Mediator");
    }

    #[test]
    fn parses_a_process_line_with_a_from_suffix() {
        let row = parse_process_line("- [agent] #4 NASTL-Mediator (from #3)").unwrap();
        assert_eq!(row.kind, "agent");
        assert_eq!(row.id, 4);
        assert_eq!(row.label, "NASTL-Mediator");
    }

    #[test]
    fn ignores_non_process_lines() {
        assert!(parse_process_line("# Processes").is_none());
        assert!(parse_process_line("_(no processes)_").is_none());
    }

    #[test]
    fn classify_pane_reads_the_launch_command() {
        assert_eq!(
            classify_pane(Some("/bin/panopt _viewer --slot main --port 7600")),
            PaneRole::Viewer
        );
        assert_eq!(
            classify_pane(Some("/bin/panopt _process-run --port 7600 5")),
            PaneRole::Process(5)
        );
        assert_eq!(
            classify_pane(Some("/bin/panopt _agent --id mediator-1a2b")),
            PaneRole::Agent
        );
        assert_eq!(classify_pane(Some("/bin/zsh -l")), PaneRole::Shell);
        assert_eq!(classify_pane(None), PaneRole::Shell);
    }

    #[test]
    fn parse_viewer_routing_reads_kind_and_id() {
        assert_eq!(
            parse_viewer_routing(r#"{"kind":"todo","id":30}"#),
            (Some("todo".to_string()), Some(30))
        );
        assert_eq!(
            parse_viewer_routing(r#"{"kind":"empty"}"#),
            (Some("empty".to_string()), None)
        );
        // Order is not constrained by the writer, but be tolerant anyway.
        assert_eq!(
            parse_viewer_routing(r#"{"id":7,"kind":"scratchpad"}"#),
            (Some("scratchpad".to_string()), Some(7))
        );
    }

    #[test]
    fn parse_viewer_routing_tolerates_garbage() {
        assert_eq!(parse_viewer_routing(""), (None, None));
        assert_eq!(parse_viewer_routing("not json"), (None, None));
        // Missing kind: returns just the id, the caller falls back to "Viewer".
        assert_eq!(parse_viewer_routing(r#"{"id":3}"#), (None, Some(3)));
    }

    #[test]
    fn viewer_title_for_each_known_kind() {
        let todos = vec![(30u64, "fixup pane titles - open, high".to_string())];
        let pads = vec![(5u64, "design notes - updated 2026-05-23".to_string())];
        assert_eq!(viewer_title_for(None, None, &todos, &pads), "Viewer");
        assert_eq!(
            viewer_title_for(Some("empty"), None, &todos, &pads),
            "Viewer"
        );
        assert_eq!(
            viewer_title_for(Some("todo"), Some(30), &todos, &pads),
            "Todo #30 - fixup pane titles"
        );
        // Unknown id: still useful, just no name.
        assert_eq!(
            viewer_title_for(Some("todo"), Some(99), &todos, &pads),
            "Todo #99"
        );
        assert_eq!(
            viewer_title_for(Some("scratchpad"), Some(5), &todos, &pads),
            "Scratchpad #5 - design notes"
        );
        assert_eq!(
            viewer_title_for(Some("todo-list"), None, &todos, &pads),
            "Todos"
        );
        assert_eq!(
            viewer_title_for(Some("scratchpad-list"), None, &todos, &pads),
            "Scratchpads"
        );
        assert_eq!(
            viewer_title_for(Some("new-todo"), None, &todos, &pads),
            "New todo"
        );
        assert_eq!(
            viewer_title_for(Some("new-scratchpad"), None, &todos, &pads),
            "New scratchpad"
        );
        // Unknown kind: do not invent a name, just label generically.
        assert_eq!(
            viewer_title_for(Some("rumor"), None, &todos, &pads),
            "Viewer"
        );
    }

    #[test]
    fn kind_prefixed_title_avoids_doubling_the_kind_word() {
        // Default ad-hoc agent label already names itself "Agent N" - the
        // prefix would duplicate, so use the label verbatim.
        assert_eq!(kind_prefixed_title("Agent", "Agent 1"), "Agent 1");
        // User-named: the prefix carries the kind, so the user sees both.
        assert_eq!(
            kind_prefixed_title("Agent", "panopt-bot"),
            "Agent: panopt-bot"
        );
        // Process-row labels from `panopt process add` typically lack the
        // kind word, so the prefix is what makes the role legible.
        assert_eq!(
            kind_prefixed_title("Command", "just check"),
            "Command: just check"
        );
        // Case-insensitive match so user typing `agent foo` still gets
        // collapsed onto the canonical "Agent foo".
        assert_eq!(kind_prefixed_title("Agent", "agent foo"), "agent foo");
    }

    #[test]
    fn parse_viewer_slot_extracts_the_slot_token() {
        assert_eq!(
            parse_viewer_slot(Some("/bin/panopt _viewer --slot main --port 7600")),
            Some("main".to_string())
        );
        assert_eq!(
            parse_viewer_slot(Some("/bin/panopt _viewer --port 7600 --slot vt2")),
            Some("vt2".to_string())
        );
        assert_eq!(parse_viewer_slot(Some("/bin/zsh -l")), None);
        assert_eq!(parse_viewer_slot(None), None);
    }

    #[test]
    fn cursor_walks_each_item_and_clamps_at_the_ends() {
        let mut pane = todos_pane(3);
        assert_eq!(pane.cursor, 0);
        assert!(!pane.move_cursor(-1));
        assert!(pane.move_cursor(1));
        assert_eq!(pane.cursor, 1);
        assert!(pane.move_cursor(1));
        assert_eq!(pane.cursor, 2);
        assert!(!pane.move_cursor(1));
        assert_eq!(pane.cursor, 2);
    }

    #[test]
    fn cursor_no_movement_on_empty_list() {
        let mut pane = pane_with(Mode::Todos);
        assert!(!pane.move_cursor(1));
        assert!(!pane.move_cursor(-1));
    }

    #[test]
    fn scroll_pages_when_cursor_passes_the_visible_window() {
        // last_rows = 10, Todos pane reserves the bottom row for the sort
        // status line, so list_rows() = 9 items visible.
        let mut pane = todos_pane(20);
        // Move cursor down through the visible window; the 9th step lands
        // on cursor 8, which is the last row still in view (scroll stays).
        for _ in 0..8 {
            pane.move_cursor(1);
        }
        assert_eq!(pane.cursor, 8);
        assert_eq!(pane.scroll, 0, "scroll: {}", pane.scroll);
        // The next step pushes cursor past the bottom edge, scroll jumps to 1.
        pane.move_cursor(1);
        assert_eq!(pane.cursor, 9);
        assert_eq!(pane.scroll, 1);
        // Continue past the end: scroll keeps pace.
        for _ in 0..10 {
            pane.move_cursor(1);
        }
        assert_eq!(pane.cursor, 19);
        assert_eq!(pane.scroll, 11); // cursor(19) + 1 - visible(9) = 11
    }

    #[test]
    fn scroll_resets_when_list_shrinks_under_cursor() {
        let mut pane = todos_pane(20);
        for _ in 0..15 {
            pane.move_cursor(1);
        }
        assert_eq!(pane.cursor, 15);
        // The list shrinks below the cursor; clamp_cursor keeps things sane.
        pane.todos.truncate(5);
        pane.rebuild_items();
        assert_eq!(pane.cursor, 4);
        let visible = pane.last_rows.saturating_sub(1).max(1);
        assert!(pane.scroll <= pane.items.len().saturating_sub(visible));
    }

    #[test]
    fn focused_target_reads_the_cursor() {
        let mut pane = todos_pane(2);
        assert!(matches!(pane.focused_target(), Some(ItemTarget::Todo(0))));
        pane.move_cursor(1);
        assert!(matches!(pane.focused_target(), Some(ItemTarget::Todo(1))));
    }

    #[test]
    fn ingest_panes_orders_content_panes_by_id() {
        use std::collections::HashMap;
        let pane = |id: u32, cmd: &str| PaneInfo {
            id,
            is_selectable: true,
            terminal_command: Some(cmd.to_string()),
            ..Default::default()
        };
        let mut panes = HashMap::new();
        panes.insert(
            0usize,
            vec![
                pane(9, "/bin/panopt _agent --id b"),
                pane(4, "/bin/panopt _agent --id a"),
            ],
        );
        let mut sidebar = pane_with(Mode::Agents);
        sidebar.ingest_panes(PaneManifest { panes });
        sidebar.rebuild_items();
        assert!(matches!(
            sidebar.items[0].target,
            ItemTarget::Pane(PaneId::Terminal(4))
        ));
        assert!(matches!(
            sidebar.items[1].target,
            ItemTarget::Pane(PaneId::Terminal(9))
        ));
    }

    #[test]
    fn ingest_panes_captures_each_viewer_slot_name() {
        use std::collections::HashMap;
        let mut panes = HashMap::new();
        panes.insert(
            0usize,
            vec![
                PaneInfo {
                    id: 3,
                    is_selectable: true,
                    terminal_command: Some(
                        "/bin/panopt _viewer --slot main --port 7600".to_string(),
                    ),
                    ..Default::default()
                },
                PaneInfo {
                    id: 7,
                    is_selectable: true,
                    is_suppressed: true,
                    terminal_command: Some(
                        "/bin/panopt _viewer --slot vt1 --port 7600".to_string(),
                    ),
                    ..Default::default()
                },
                PaneInfo {
                    id: 9,
                    is_selectable: true,
                    terminal_command: Some("/bin/panopt _agent".to_string()),
                    ..Default::default()
                },
            ],
        );
        let mut sidebar = pane_with(Mode::Todos);
        sidebar.ingest_panes(PaneManifest { panes });
        assert_eq!(
            sidebar.viewer_slot_of(PaneId::Terminal(3)),
            Some("main".to_string())
        );
        assert_eq!(
            sidebar.viewer_slot_of(PaneId::Terminal(7)),
            Some("vt1".to_string())
        );
        assert_eq!(sidebar.first_suppressed_viewer(), Some(PaneId::Terminal(7)));
        assert!(sidebar.viewer_slot_of(PaneId::Terminal(9)).is_none());
    }

    #[test]
    fn pane_is_visible_tracks_the_suppressed_flag() {
        use std::collections::HashMap;
        let make_pane = |id: u32, suppressed: bool| PaneInfo {
            id,
            is_selectable: true,
            is_suppressed: suppressed,
            terminal_command: Some("/bin/panopt _agent".to_string()),
            ..Default::default()
        };
        let mut panes = HashMap::new();
        panes.insert(0usize, vec![make_pane(4, false), make_pane(9, true)]);
        let mut sidebar = pane_with(Mode::Todos);
        sidebar.ingest_panes(PaneManifest { panes });
        assert!(sidebar.pane_is_visible(PaneId::Terminal(4)));
        assert!(!sidebar.pane_is_visible(PaneId::Terminal(9)));
        assert!(!sidebar.pane_is_visible(PaneId::Terminal(99)));
    }

    #[test]
    fn agent_panes_get_stable_distinct_labels() {
        use std::collections::HashMap;
        let agent = |id: u32| PaneInfo {
            id,
            is_selectable: true,
            terminal_command: Some("/bin/panopt _agent".to_string()),
            ..Default::default()
        };
        let manifest = |ids: &[u32]| {
            let mut panes = HashMap::new();
            panes.insert(0usize, ids.iter().map(|&id| agent(id)).collect());
            PaneManifest { panes }
        };
        let mut sidebar = pane_with(Mode::Agents);
        sidebar.ingest_panes(manifest(&[4, 9]));
        sidebar.rebuild_items();
        assert_eq!(sidebar.items[0].label, "Agent 1");
        assert_eq!(sidebar.items[1].label, "Agent 2");
        sidebar.ingest_panes(manifest(&[12, 9, 4]));
        sidebar.rebuild_items();
        assert_eq!(sidebar.items[0].label, "Agent 1");
        assert_eq!(sidebar.items[1].label, "Agent 2");
        assert_eq!(sidebar.items[2].label, "Agent 3");
    }

    #[test]
    fn mode_parse_recognizes_each_kind() {
        for (s, m) in [
            ("todos", Mode::Todos),
            ("agents", Mode::Agents),
            ("terminals", Mode::Terminals),
            ("commands", Mode::Commands),
            ("scratchpads", Mode::Scratchpads),
        ] {
            assert_eq!(Mode::parse(s), Some(m));
        }
        assert_eq!(Mode::parse("bogus"), None);
        assert_eq!(Mode::parse(""), None);
    }

    #[test]
    fn mode_letters_are_distinct() {
        use std::collections::HashSet;
        let letters: HashSet<char> = [
            Mode::Todos,
            Mode::Agents,
            Mode::Terminals,
            Mode::Commands,
            Mode::Scratchpads,
        ]
        .into_iter()
        .map(|m| m.letter())
        .collect();
        assert_eq!(letters.len(), 5, "every mode needs its own slot prefix");
    }

    #[test]
    fn viewer_slot_carries_mode_letter() {
        let mut todos = pane_with(Mode::Todos);
        let mut scratchpads = pane_with(Mode::Scratchpads);
        let a = todos.allocate_viewer_slot();
        let b = scratchpads.allocate_viewer_slot();
        // Two panes both allocating their first slot - the mode letter is
        // what stops them from colliding on the same `v1`.
        assert_ne!(a, b);
        assert!(a.contains('t'));
        assert!(b.contains('s'));
    }

    #[test]
    fn agent_labels_roundtrip_through_the_projection_format() {
        let mut input = BTreeMap::new();
        input.insert(4u32, "Mediator".to_string());
        input.insert(9u32, "Edge \"case\" with, commas".to_string());
        let mut body = String::from("{");
        for (i, (tid, label)) in input.iter().enumerate() {
            if i > 0 {
                body.push(',');
            }
            let safe = label.replace('\\', "\\\\").replace('"', "\\\"");
            body.push_str(&format!("\"{tid}\":\"{safe}\""));
        }
        body.push('}');
        let parsed: BTreeMap<u32, String> = parse_agent_labels(&body).into_iter().collect();
        assert_eq!(parsed, input);
    }

    #[test]
    fn agent_labels_parser_tolerates_malformed_input() {
        assert!(parse_agent_labels("").is_empty());
        assert!(parse_agent_labels("not json").is_empty());
        assert!(parse_agent_labels("{}").is_empty());
    }
}
