//! PANopt coordination sidebar - a Zellij plugin.
//!
//! Each plugin pane renders one kind of resource - todos, agents, terminals,
//! commands, or notes - selected by the `mode` value in its layout
//! config. The five panes stack vertically in the cockpit's left column, each
//! pinned to its own fixed proportion so adding or removing panes on the
//! right cannot reshape any of them. A keyboard cursor walks the pane's items
//! and scrolls when it hits the visible window's edge; the mouse clicks any
//! row.
//!
//! The cockpit is these five panes plus one content pane on the right.
//! Selecting an item swaps its pane into that one slot and suppresses
//! whatever was there - a suppressed pane keeps running, just hidden, no
//! stack and no title bar. Documents (todos, notes, lists) all share
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

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use zellij_tile::prelude::*;

use panopt_zellij_logic::*;

/// Routing slot prefix for viewer panes the plugin spawns ad hoc. The layout
/// boots one viewer with `--slot main`; further viewers spawned by
/// [`PanoptPane::ensure_viewer_in_slot`] get unique names `v<mode-letter><n>`
/// (`vt1`, `vn1`, `va1`, ...) so each pane has its own
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

/// The Todos gatekeeper publishes the live right-side content-pane count here.
/// A `_viewer` reads it at close time to learn whether it is the only pane
/// left - the one it must refuse to close, since losing it makes Zellij
/// re-tile and the sidebar stop being a sidebar. The plugin's own `x`/`Ctrl-q`
/// close gate already covers the keybind paths; this file covers the viewer's
/// in-pane Ctrl-c/`q`, which never routes through the plugin.
const CONTENT_COUNT_PATH: &str = "/host/.panopt/.cockpit/content-count";

/// The daemon's input queue (todo #160): one JSON object per line, bound for a
/// running instance's pane. The Todos gatekeeper reads this each poll, writes
/// each `content` into the addressed agent's pane, and acks the `seq` so the
/// daemon drops it. See `panopt_core::projection::project_inputs`.
const INPUTS_PATH: &str = "/host/.panopt/.cockpit/inputs.jsonl";

/// Foreground 256-colour SGR code for an agent row that needs attention (todo
/// #175): the agent finished a work cycle (busy -> idle) and the operator has
/// not yet looked at its pane. Amber, to read as "(may) need attention, work is
/// done". This is a latch, not a state mirror - it clears the moment the agent's
/// pane is focused (see [`PanoptPane::update_attention`]), so a long-idle agent
/// the user has already seen carries no colour.
const AGENT_ATTENTION_FG: u8 = 214;

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
    /// Notes parsed from `.panopt/notes.md`: `(id, label)`.
    notes: Vec<(u64, String)>,
    /// Process instances parsed from `.panopt/processes.md`.
    /// TODO(#27): render agent_tools.md alongside processes once a spawn UI
    /// exists; until then the sidebar shows only live instances, same as the
    /// pre-V6 roster view.
    processes: Vec<ProcessRow>,
    /// Agent configs parsed from `.panopt/agent_tools.md`. The Agents pane is
    /// config-centric (#27): each config is one agent, rendered joined to its
    /// single live instance in `processes`. Populated only in modes that show
    /// the Agents list.
    configs: Vec<ConfigRow>,
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
    /// The floating `panopt search` popup pane when one is open, so the
    /// plugin can close it from its own dispatch (avoiding a race with the
    /// search CLI's own `zellij action close-pane` against the focus-shift
    /// that lands on the viewer when a selection swaps). Cleared on close.
    search_pane: Option<PaneId>,
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

    /// Stable PANopt agent id for each cockpit-spawned agent pane, keyed by
    /// terminal pane id. Populated when [`Self::spawn_agent_pane`] launches
    /// `_agent --id <id>`; used by [`Self::sync_agent_labels`] to call
    /// `_agent-leave` on the daemon as that id the moment its pane closes,
    /// so the registry entry and any advisory locks clear without waiting
    /// on the idle sweep (todo #83). Independent of `agent_labels`: the
    /// label is what the human sees, this is what the daemon knows.
    agent_pane_ids: BTreeMap<u32, String>,

    /// Process ids the gatekeeper has already reconciled into a pane, keyed by
    /// process id -> the terminal pane it spawned (todo #141). A daemon-owned
    /// `starting` row is reconciled into a `_process-run` pane exactly once;
    /// this guards the window between the spawn and the next pane manifest
    /// (where [`Self::process_pane`] can't yet see the new pane) so a single
    /// start can't fan out into a pile of duplicate panes. Pruned to the live
    /// process set each poll. Only the Todos gatekeeper populates it, so the
    /// five plugin instances don't each spawn the same row.
    reconciled_panes: BTreeMap<u64, u32>,

    /// Compiled status patterns per `tool_type`, parsed from the daemon's
    /// `.panopt/agent-types.md` projection (todo #142). Rebuilt only when that
    /// file's text changes (see [`Self::agent_types_src`]) - regex compilation
    /// is not free, and the patterns are static for the daemon's lifetime.
    status_matchers: BTreeMap<String, StatusMatcher>,

    /// The raw text the `status_matchers` were last built from, so a reload can
    /// skip recompiling when `agent-types.md` is unchanged.
    agent_types_src: String,

    /// The last `agent_state` reported per process id (todo #142). The status
    /// observer reports only on a *change*, so a steady-state agent does not
    /// spawn a `_process-report` subprocess every poll. Pruned to the live set.
    reported_states: BTreeMap<u64, String>,

    /// Queue ids of inputs already written into a pane (todo #160). The daemon
    /// drops an input from `inputs.jsonl` once we ack it, but the projection
    /// lags our write by a poll or two; tracking delivered seqs here stops us
    /// from typing the same input twice in that window.
    delivered_inputs: std::collections::HashSet<i64>,

    /// Panes whose just-typed input still needs its submitting Enter, deferred
    /// to the *next* timer tick. Claude Code treats a fast burst (the body plus
    /// a trailing `\r` in one write) as a paste and inserts the newline instead
    /// of running the prompt; sending the carriage return ~1s later, as its own
    /// input event, lands as a real Enter. See [`Self::deliver_pending_inputs`].
    pending_submit: Vec<PaneId>,

    /// Whether any plugin pane is currently the focused pane in its tab.
    /// Updated by [`PanoptPane::ingest_panes`] but only from a non-transient
    /// manifest: a transient `zellij action pipe` pane briefly steals focus
    /// while the close-gate pipes fly, and we must not let that flicker make
    /// the gate think the user has moved off the cockpit panes.
    /// Used by [`PanoptPane::gate_close_focus`] to refuse closing any plugin
    /// pane absolutely.
    sidebar_focused: bool,
    /// Whether the currently focused pane is floating. Tracked from the same
    /// non-transient manifest as `sidebar_focused`. The cockpit's floating
    /// panes are popups - the `panopt search` dialog and the delete/close
    /// gate dialogs - all of which run in Locked mode, indistinguishable by
    /// input mode from a focused tiled content pane. The `panopt:focus-pane`
    /// pipe consults this so `Alt-<n>` does not yank focus out of a popup
    /// (todo #110): a focus request is a no-op while a floating pane is up.
    focused_is_floating: bool,
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
    /// The most recent `switch_to_mode` decision this plugin emitted:
    /// `Some(true)` after locking on content focus, `Some(false)` after
    /// restoring Normal on sidebar focus, `None` before any decision. Lets
    /// [`PanoptPane::ingest_panes`] fire `switch_to_mode` only on transitions
    /// and not on every manifest tick. Only the Todos pane writes this -
    /// see the gate in `ingest_panes`.
    last_emitted_locked: Option<bool>,
    /// Last content-pane count this instance published to [`CONTENT_COUNT_PATH`]
    /// so the gatekeeper rewrites the file only when the count changes, not on
    /// every manifest tick. Only the Todos pane writes it; `None` until the
    /// first publish.
    last_content_count: Option<usize>,
    /// Whether this instance has ever seen a live content pane in the manifest.
    /// Gates [`PanoptPane::ensure_content_slot`] so the reclaim only fires on a
    /// real drop-to-zero (a pane the user had, now gone or dead) and never at
    /// boot, where a PaneUpdate can momentarily show zero content before the
    /// layout's `--slot main` viewer is registered - acting there would race a
    /// duplicate pane in front of the real one.
    has_had_content: bool,

    /// Monotonic version of the presentation state this instance has published
    /// to the per-mode shared view file (todo #116). `mirror_session` mirrors
    /// focus and terminal panes across clients, but each client's sidebar is its
    /// own plugin instance; this is how the selection/filter/scroll stay in sync.
    /// Bumped on every local view change so peers can tell a newer snapshot from
    /// an older one (last-writer-wins).
    view_seq: u64,
    /// The highest view `seq` this instance has already applied - either one it
    /// wrote itself or one it adopted from a peer. The read-back guard in
    /// [`PanoptPane::adopt_view_state`] only adopts strictly-newer seqs, so this
    /// both skips our own writes and prevents regressing to a stale view.
    last_applied_seq: u64,

    /// Whether the idle bell is armed (todo #175). On by default; set in
    /// [`PanoptPane::load`] (the derived `Default` is `false`) and toggled with
    /// `b` on the Todos pane. Only gates the audible bell - the agent-row
    /// attention colour is always shown. Operationally only the Todos gatekeeper
    /// rings, so this is only consulted there.
    idle_bell: bool,
    /// One-shot: an agent settled to idle since the last render, so the next
    /// [`PanoptPane::render`] should emit a single BEL (todo #175). Armed in
    /// [`PanoptPane::observe_agent_states`], cleared when flushed.
    pending_bell: bool,

    /// Process ids of agents that finished a work cycle (busy -> idle) and whose
    /// pane the operator has not yet focused (todo #175). Their Agents-pane row
    /// paints [`AGENT_ATTENTION_FG`] until attention is paid. A latch, not a
    /// state mirror: set on the busy -> idle transition, cleared when the agent's
    /// pane is focused or it resumes work. Maintained by
    /// [`PanoptPane::update_attention`]; only the Agents instance renders agent
    /// rows, so only it keeps this.
    attention: BTreeSet<u64>,
    /// Last projected `agent_state` per agent process id, used by
    /// [`PanoptPane::update_attention`] to detect the busy -> idle transition
    /// from the `.panopt/processes.md` projection (the shared truth every
    /// instance reads). Distinct from `reported_states`, which is the
    /// gatekeeper's pane-scrape channel for the bell. Pruned to the live set.
    agent_state_seen: BTreeMap<u64, String>,
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
    /// A floating overlay (search popup, close/delete-gate dialog) rather than
    /// a tiled content pane. Excluded from the content-pane floor: a dialog on
    /// screen is not the content slot the cockpit must keep alive.
    floating: bool,
    role: PaneRole,
    /// For [`PaneRole::Viewer`] panes only: the `--slot X` token from the
    /// launch command, used as the routing file name
    /// `.panopt/.cockpit/viewer-<slot>.json`. `None` on any other role.
    viewer_slot: Option<String>,
    /// For agent panes: the stable agent id parsed from the launch command's
    /// `--id` (todo #142). The status observer matches this against an instance
    /// row's `agent_id` to bind the pane to its row - resilient to Zellij
    /// surfacing the agent's `_mcp-proxy` child rather than the `_process-run`
    /// shim. `None` on non-agent panes. See `agent_id_from_command`.
    agent_id: Option<String>,
    /// Tab position from the `PaneManifest`. Used by the CloseTab gate to
    /// scope active-item aggregation to a single tab.
    tab: usize,
}

/// One item rendered in the pane.
struct Item {
    label: String,
    target: ItemTarget,
    /// A live marker: a running process, or the Zellij-focused pane.
    live: bool,
    /// Precomputed foreground 256-colour SGR code for this row, or `None` to
    /// paint in the terminal default. Todos derive it from their status (todo
    /// #236, [`row_state_for`]); an agent row is [`AGENT_ATTENTION_FG`] while it
    /// needs attention (todo #175, [`PanoptPane::attention`]); other modes leave
    /// it `None`. Resolved at build time so [`PanoptPane::render`] just paints it.
    fg: Option<u8>,
}

/// What selecting an item does.
#[derive(Clone)]
enum ItemTarget {
    Todo(u64),
    Note(u64),
    /// A process agent or command, by process id.
    Process(u64),
    /// An agent config, by config (agent_tool) id. The Agents pane is
    /// config-centric (#27): activating one starts (or focuses) its single live
    /// instance; stopping/deleting acts on the config and that instance.
    Config(u64),
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
        // The idle bell (todo #175) is on by default - the derived `Default`
        // gives `false`, so set it here. A layout can opt out with
        // `idle_bell = "false"` (or `0`/`off`/`no`) in the plugin config; any
        // other value, or none, leaves it on.
        self.idle_bell = !matches!(
            configuration.get("idle_bell").map(String::as_str),
            Some("false") | Some("0") | Some("off") | Some("no")
        );
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
            PermissionType::WriteToClipboard,
            // The status observer reads each agent pane's viewport via
            // `get_pane_scrollback` (#142/#163); without this the host silently
            // drops the call (an EOF on stdin) and no `state:` is ever derived.
            PermissionType::ReadPaneContents,
            // The input deliverer (#160) types queued input into agent panes via
            // `write_chars_to_pane_id`; without this the host silently drops the
            // write and a spawned agent never receives its task.
            PermissionType::WriteToStdin,
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
            // Key and Mouse are the only events that carry local user intent to
            // change the view. Snapshot the presentation fields around the
            // handler and, if they moved, publish the new view so peer clients'
            // same-mode instances adopt it (todo #116). Timer and PaneUpdate
            // ticks deliberately do NOT publish - they re-clamp cursor/scroll
            // against a changed list identically on every client, which is not a
            // user action and must not race the shared seq.
            Event::Key(key) => {
                let before = self.view_fingerprint();
                let handled = self.handle_key(key);
                if self.view_fingerprint() != before {
                    self.bump_and_persist_view();
                }
                handled
            }
            Event::Mouse(mouse) => {
                let before = self.view_fingerprint();
                let handled = self.handle_mouse(mouse);
                if self.view_fingerprint() != before {
                    self.bump_and_persist_view();
                }
                handled
            }
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
                    if self.initial_preview_delay >= 1 {
                        self.preview_cursor();
                        if let Some(plugin) = self.plugin_pane {
                            focus_pane_with_id(plugin, false, false);
                        }
                        self.initial_preview_done = true;
                    }
                }
                self.reload_data();
                self.rebuild_items();
                // Hide the panes of instances disposed over MCP (#207) before the
                // spawn reconciler prunes their pane mapping. Same poll and
                // gatekeeper gating; suppress (never close) so the cockpit sheds
                // dead agent husks without breaking the never-close invariant.
                self.reconcile_disposed_processes();
                // Turn any daemon-owned `starting` row into a live pane. Driven
                // off the same 1s poll that refreshed `self.processes`, gated to
                // the Todos gatekeeper inside the method.
                self.reconcile_starting_processes();
                // Classify each running agent's pane output and report state
                // changes back to the daemon (todo #142). Same poll, same
                // gatekeeper gating - the observer mirrors the reconciler.
                self.observe_agent_states();
                // Type any queued input into agent panes (todo #160): the
                // cockpit half of send_input and of a spawn's opening prompt.
                // Same poll and gatekeeper gating as the observer above.
                self.deliver_pending_inputs();
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
        // Search lifecycle pipes route on every sidebar instance, not just
        // Todos: `Alt-/` from a non-Todos sidebar pane spawns the popup
        // locally on that instance, so the holder of `search_pane` is not
        // necessarily Todos. The search CLI broadcasts close-search and
        // show-result without a `mode=todos` filter so every instance
        // receives them; non-holders no-op via the `search_pane.is_none()`
        // guard inside `close_search_pane` / `handle_search_result`.
        match pipe_message.name.as_str() {
            "panopt:close-search" => {
                self.close_search_pane();
                return true;
            }
            "panopt:show-result" => {
                self.handle_search_result(pipe_message.payload.as_deref());
                return true;
            }
            "panopt:view-sync" => {
                // A peer moved the shared sidebar view; adopt it now instead of
                // waiting up to 1s for this instance's timer (todo #116).
                // Broadcast unnarrowed like `focus-pane`, self-filtered on the
                // payload (the originating mode's slug) so only matching
                // instances react. Our own broadcast no-ops via the seq guard.
                if pipe_message.payload.as_deref() == Some(self.mode.slug()) {
                    self.adopt_view_state();
                    self.rebuild_items();
                    return true;
                }
                return false;
            }
            "panopt:focus-pane" => {
                // `Alt-<n>` focus request (todo #110). Broadcast to every
                // sidebar instance (config narrowing proved unreliable for the
                // non-Todos modes - every digit routed to the gatekeeper); the
                // payload names the target mode's slug, and only that instance
                // focuses its own pane. Suppressed while a floating popup
                // (search / gate dialog) holds focus, so the gesture can never
                // yank the user out of a popup mid-input.
                if pipe_message.payload.as_deref() == Some(self.mode.slug())
                    && !self.focused_is_floating
                {
                    if let Some(plugin) = self.plugin_pane {
                        focus_pane_with_id(plugin, false, false);
                    }
                }
                return true;
            }
            _ => {}
        }
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
            "panopt:copy-to-clipboard" => {
                // `panopt _viewer` ships a selection here via
                // `zellij action pipe -- <text>`. The plugin holds the
                // `WriteToClipboard` permission (pre-granted in
                // `~/.cache/zellij/permissions.kdl` by `panopt up`'s
                // `ensure_clipboard_permission_granted`) and Zellij's
                // host machinery honours whatever `copy_command` /
                // `copy_clipboard` the user configured.
                if let Some(text) = pipe_message.payload.as_deref() {
                    copy_to_clipboard(text.to_string());
                }
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
            "panopt:open-search" => {
                self.spawn_search_dialog();
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
                    paint(line, cols, Style::Dim, None, false)
                );
            }
            return;
        }
        if total == 0 {
            print!(
                "\u{1b}[1;1H{}",
                paint("  (none)", cols, Style::Dim, None, false)
            );
        } else {
            let end = (self.scroll + visible).min(total);
            for (slot, idx) in (self.scroll..end).enumerate() {
                let item = &self.items[idx];
                let marker = if item.live { '*' } else { ' ' };
                let line = format!(" {marker}{}", item.label);
                let focused = idx == self.cursor;
                let fg = item.fg;
                print!(
                    "\u{1b}[{};1H{}",
                    slot + 1,
                    paint(&line, cols, Style::Normal, fg, focused)
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
                paint(&status, cols, Style::Dim, None, false)
            );
        }
        // Ring the terminal bell once when an agent has just settled to idle
        // (todo #175). Armed in `observe_agent_states` on a busy -> idle
        // transition and flushed here, after the cursor-positioned row writes
        // and the `\x1b[3J` clear, so the bare BEL can't disturb the layout
        // (it moves no cursor and prints no glyph). Only the Todos gatekeeper
        // observes agent state, so only it ever arms this - the bell rings
        // once, not once per sidebar instance.
        if self.pending_bell {
            print!("\u{07}");
            self.pending_bell = false;
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
        // Lead with the `Alt-<n>` focus hotkey (todo #110) so the gesture is
        // discoverable from the pane itself, lazygit-style: `[alt+1] Todos`.
        let base = format!("[{}] {}", self.mode.hotkey_hint(), self.mode.label());
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
        lines.push("  alt+1..5      focus sidebar pane".to_string());
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
                lines.push(format!(
                    "  b             idle bell on / off     [{}]",
                    if self.idle_bell { "on" } else { "off" }
                ));
            }
            Mode::Notes => {
                lines.push("  n             new note".to_string());
                lines.push("  x             delete note".to_string());
            }
            Mode::Agents => {
                lines.push("  Enter / u     start / focus instance".to_string());
                lines.push("  n             new config (form)".to_string());
                lines.push("  e             edit config (form)".to_string());
                lines.push("  d             stop instance (close pane)".to_string());
                lines.push("  x             delete config".to_string());
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
        viewer_title_for(kind.as_deref(), id, &self.todos, &self.notes)
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
        process_pane_title_for(prefix, id, &row.label)
    }

    // --- data ---

    /// Re-read whichever projected index files this mode needs. The Todos
    /// pane reads only todos; Notes only notes; the Agents and
    /// Commands modes share processes.md plus the agent label projection.
    fn reload_data(&mut self) {
        match self.mode {
            Mode::Todos => {
                self.todos = read_index("/host/.panopt/todos.md");
                // The Todos pane is the cockpit gatekeeper and titles every
                // right-pane terminal in `sync_pane_titles`; it needs every
                // projection a title can reference, not just its own list.
                self.notes = read_index("/host/.panopt/notes.md");
                self.processes = read_processes("/host/.panopt/processes.md");
                self.read_agent_labels();
                self.reload_status_matchers();
            }
            Mode::Notes => {
                self.notes = read_index("/host/.panopt/notes.md");
            }
            Mode::Agents | Mode::Commands => {
                self.processes = read_processes("/host/.panopt/processes.md");
                self.read_agent_labels();
                // The Agents pane lists configs joined to their live instances
                // (#27); Commands has no config layer, so only read it there.
                if self.mode == Mode::Agents {
                    self.configs = read_configs("/host/.panopt/agent_tools.md");
                }
            }
            Mode::Terminals => {
                self.read_agent_labels();
            }
        }
        // Pick up any newer presentation snapshot a peer published, so the
        // sidebar stays identical across clients even between the instant
        // `panopt:view-sync` pipes (todo #116). The caller re-runs
        // `rebuild_items` (and thus `clamp_cursor`) right after, so an adopted
        // cursor/scroll is clamped against this instance's current list.
        self.adopt_view_state();
    }

    // --- shared sidebar presentation (todo #116) ---

    /// The presentation fields that must stay identical across every client.
    /// Captured before and after each input event so a view is published only on
    /// a real local change, not on every keypress.
    fn view_fingerprint(&self) -> (usize, usize, u8, u8, u8, bool) {
        (
            self.cursor,
            self.scroll,
            self.todo_filter.to_wire(),
            self.todo_sort_1.to_wire(),
            self.todo_sort_2.to_wire(),
            self.show_help,
        )
    }

    /// Publish this instance's view to the per-mode shared file and nudge peer
    /// clients' same-mode instances to adopt it at once. Called only when an
    /// input event actually moved the view.
    fn bump_and_persist_view(&mut self) {
        self.view_seq += 1;
        // We are already showing this snapshot, so mark it applied: the
        // read-back in `adopt_view_state` then skips our own write.
        self.last_applied_seq = self.view_seq;
        let view = ViewState {
            cursor: self.cursor,
            scroll: self.scroll,
            filter: self.todo_filter,
            sort_1: self.todo_sort_1,
            sort_2: self.todo_sort_2,
            show_help: self.show_help,
            seq: self.view_seq,
        };
        write_view_state(self.mode, &view);
        self.broadcast_view_sync();
    }

    /// Adopt the shared view when a peer published a newer one. The strict `>`
    /// guard skips our own write (its seq equals `last_applied_seq`) and never
    /// regresses to a stale snapshot. The caller re-clamps via `rebuild_items`,
    /// so adopted bounds are validated against the local list.
    fn adopt_view_state(&mut self) {
        let Some(view) = read_view_state(self.mode) else {
            return;
        };
        if view.seq <= self.last_applied_seq {
            return;
        }
        self.cursor = view.cursor;
        self.scroll = view.scroll;
        self.todo_filter = view.filter;
        self.todo_sort_1 = view.sort_1;
        self.todo_sort_2 = view.sort_2;
        self.show_help = view.show_help;
        self.view_seq = view.seq;
        self.last_applied_seq = view.seq;
    }

    /// Broadcast a `panopt:view-sync` pipe so peer clients' instances of this
    /// mode re-read the shared file immediately instead of waiting for their 1s
    /// timer. Broadcast unnarrowed (config narrowing proved unreliable for the
    /// non-Todos modes - see [`PanoptPane::request_focus_pane`]); the payload
    /// carries the mode slug so only the matching instances adopt. The 1s timer
    /// is the correctness floor regardless - this pipe only trims latency.
    fn broadcast_view_sync(&self) {
        run_command(
            &[
                "zellij",
                "action",
                "pipe",
                "--name",
                "panopt:view-sync",
                "--",
                self.mode.slug(),
            ],
            BTreeMap::new(),
        );
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
        let mut focused_floating_this_update = false;
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
                    focused_floating_this_update = p.is_floating;
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
                // Floating panes - the search popup, the delete-gate and
                // close-gate dialogs - overlay on top of the tiled layout;
                // they are not the content the sidebar's selections route
                // into. Keeping them out of `focused_non_plugin` is what
                // stops e.g. `open_document` from misrouting through a
                // search popup that briefly held focus.
                if p.is_focused && !p.is_floating {
                    focused_non_plugin = Some(id);
                }
                let role = classify_pane(p.terminal_command.as_deref());
                let viewer_slot = if matches!(role, PaneRole::Viewer) {
                    parse_viewer_slot(p.terminal_command.as_deref())
                } else {
                    None
                };
                // The agent id stamped on the pane's `--id`, captured here at
                // ingest while the command is in hand (todo #142). It binds the
                // pane to its instance row for the status observer; see
                // `agent_id_from_command`.
                let agent_id = agent_id_from_command(p.terminal_command.as_deref());
                rows.push(PaneRow {
                    id,
                    title: p.title.clone(),
                    focused: p.is_focused,
                    suppressed: p.is_suppressed,
                    exited: p.exited,
                    floating: p.is_floating,
                    role,
                    viewer_slot,
                    agent_id,
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
            self.focused_is_floating = focused_floating_this_update;
            self.focused_tab = focused_tab_this_update;
            // Auto-lock the multiplexer when focus is on a content pane
            // (`panopt _viewer` form or an agent/terminal) so every Zellij
            // keybind drops out and keys flow straight to the inner program;
            // restore Normal mode when focus returns to any sidebar plugin
            // pane so the user can navigate the cockpit again. Only the Todos
            // pane emits this - the other four instances see the same focus
            // manifest and would issue four redundant `switch_to_mode` calls
            // per transition. Compare against the last emitted decision so a
            // PaneUpdate that doesn't cross the sidebar/content boundary
            // (e.g. a resize) is a no-op rather than yanking the user out of
            // an explicit Zellij mode.
            if self.mode == Mode::Todos {
                let want_locked = !sidebar_focused_this_update;
                if self.last_emitted_locked != Some(want_locked) {
                    switch_to_input_mode(if want_locked {
                        &InputMode::Locked
                    } else {
                        &InputMode::Normal
                    });
                    self.last_emitted_locked = Some(want_locked);
                }
            }
        }
        if let Some(slot) = self.slot_pane {
            // Drop the slot when its pane is gone *or merely suppressed*. Each
            // sidebar mode is its own plugin instance with its own `slot_pane`;
            // when one instance swaps a viewer into the content slot, the pane
            // it displaced is suppressed (still in the manifest, just hidden).
            // The other instances must not keep pointing at that hidden pane -
            // otherwise `show_in_slot` mistakes it for the live slot, calls
            // `focus_pane_with_id` on a suppressed pane, and Zellij resurfaces
            // it by splitting whatever sidebar pane is focused. Treating a
            // suppressed slot as no-longer-the-slot lets the re-adopt below
            // re-point at the visible content pane.
            if !self.panes.iter().any(|p| p.id == slot && !p.suppressed) {
                self.slot_pane = None;
            }
        }
        if let Some(search) = self.search_pane {
            if !self.panes.iter().any(|p| p.id == search) {
                self.search_pane = None;
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
        self.publish_content_count();
        self.ensure_content_slot();
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
    /// names (e.g. `vt1` from Todos, `vn1` from Notes).
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

    /// The live pane hosting agent instance `id`, for the status observer (#142).
    ///
    /// Two bindings, tried in order, because an agent pane's command surfaces
    /// inconsistently in Zellij's manifest. The robust one is by `agent_id`: the
    /// pane stamps its stable id onto the `--id` of the `_mcp-proxy`/`_agent`
    /// command (captured into [`PaneRow::agent_id`] at ingest), and that id is
    /// the instance row's `name`. The fallback is the numeric `Process(id)`
    /// role: the shape the pane carries before `_process-run` execs into the
    /// agent, and the shape non-agent process panes keep. An exited pane never
    /// matches.
    fn agent_pane(&self, id: u64, agent_id: Option<&str>) -> Option<PaneId> {
        if let Some(agent_id) = agent_id.filter(|s| !s.is_empty()) {
            if let Some(p) = self
                .panes
                .iter()
                .find(|p| !p.exited && p.agent_id.as_deref() == Some(agent_id))
            {
                return Some(p.id);
            }
        }
        self.process_pane(id)
    }

    /// Maintain the "needs attention" latch behind the agent-row colour (todo
    /// #175). The colour is not a passive idle indicator: it must mean "work is
    /// done, (may) need attention" and clear once attention has been paid. So a
    /// row latches on the busy -> idle transition and clears the moment its pane
    /// is focused (the operator looked) or the agent resumes work.
    ///
    /// Driven off the projected `agent_state` (the truth every instance shares
    /// via `.panopt/processes.md`) rather than the gatekeeper's pane scrape, so
    /// the Agents instance maintains it locally without any cross-instance
    /// plumbing. The transition is detected by diffing each agent's current
    /// state against `agent_state_seen`; a first observation (`None` previous) is
    /// the boot case and never latches - matching the bell's boot-settle
    /// suppression (#237).
    fn update_attention(&mut self) {
        // Snapshot the live agents and whether each one's pane is currently
        // focused, so the borrow of `self.panes`/`self.processes` is released
        // before mutating `self.attention` / `self.agent_state_seen`.
        let agents: Vec<(u64, String, bool)> = self
            .processes
            .iter()
            .filter(|r| r.kind == "agent" && r.status.as_deref() == Some("running"))
            .map(|r| {
                let state = r.agent_state.clone().unwrap_or_default();
                let focused = self
                    .agent_pane(r.id, r.agent_id.as_deref())
                    .and_then(|pane| self.panes.iter().find(|p| p.id == pane))
                    .map(|p| p.focused)
                    .unwrap_or(false);
                (r.id, state, focused)
            })
            .collect();
        let live: BTreeSet<u64> = agents.iter().map(|(id, _, _)| *id).collect();
        self.attention.retain(|id| live.contains(id));
        self.agent_state_seen.retain(|id, _| live.contains(id));

        for (id, state, focused) in agents {
            let prev = self.agent_state_seen.get(&id).map(String::as_str);
            // Latch on a genuine busy -> idle transition: the agent finished a
            // work cycle. `prev == None` (boot) and prev already idle do not
            // latch.
            if state == "idle" && matches!(prev, Some(p) if p != "idle") {
                self.attention.insert(id);
            }
            // Resumed work -> the "done" premise no longer holds, drop the latch.
            if state != "idle" {
                self.attention.remove(&id);
            }
            // Attention paid: the operator focused the agent's pane. Clears the
            // latch even while still idle (the whole point - it is not a passive
            // idle light). Also covers the "already watching when it finished"
            // case: the same pass latches then immediately clears, so no colour.
            if focused {
                self.attention.remove(&id);
            }
            self.agent_state_seen.insert(id, state);
        }
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

        // Tell the daemon about every cockpit-spawned agent whose pane just
        // disappeared, before we drop the local tracking. The launcher
        // (`panopt _agent-leave --id <id>`) calls the daemon's `agent_leave`
        // MCP tool on behalf of the dead agent, which releases its locks
        // and removes its registry entry immediately - no waiting on the
        // idle sweep, which never fires for declared identities anyway.
        let departed_ids: Vec<String> = self
            .agent_pane_ids
            .iter()
            .filter(|(tid, _)| !agent_ids.contains(tid))
            .map(|(_, id)| id.clone())
            .collect();
        if !departed_ids.is_empty() {
            if let Some(cwd) = self.launch_cwd() {
                for id in &departed_ids {
                    self.run_panopt(&["_agent-leave", "--id", id.as_str()], cwd.clone());
                }
            }
        }
        self.agent_pane_ids.retain(|tid, _| agent_ids.contains(tid));

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
        // Refresh the attention latch before building agent rows (todo #175):
        // only the Agents instance renders them and needs the colour, so other
        // modes skip the scan. Runs here because both the 1s poll (fresh agent
        // state) and PaneUpdate (fresh focus) route through `rebuild_items`.
        if self.mode == Mode::Agents {
            self.update_attention();
        }
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
                        // Classify from the raw projection label (it carries the
                        // `- status, …` suffix) before the `#id ` prefix is added.
                        fg: row_state_for(label).fg_code(),
                    })
                    .collect()
            }
            Mode::Notes => self
                .notes
                .iter()
                .map(|(id, label)| Item {
                    label: format!("#{id} {label}"),
                    target: ItemTarget::Note(*id),
                    live: false,
                    fg: None,
                })
                .collect(),
            Mode::Agents => {
                // Config-centric (#27): one row per agent config, joined to a
                // representative live instance for status/state and the live
                // marker. The agent *is* the config; starting it spawns an
                // instance, stopping it ends one, but the row persists. The
                // daemon is a 1:N factory (todo #190); rendering each instance as
                // its own row is a follow-up.
                self.configs
                    .iter()
                    .map(|c| {
                        let inst = self.config_instance(c.id);
                        let live = inst
                            .map(|r| self.process_pane(r.id).is_some())
                            .unwrap_or(false);
                        // Amber while this agent needs attention (todo #175):
                        // it finished a work cycle and its pane has not been
                        // focused yet. The latch is maintained in
                        // `update_attention`, keyed by the instance's process id.
                        let fg = inst
                            .filter(|r| self.attention.contains(&r.id))
                            .map(|_| AGENT_ATTENTION_FG);
                        Item {
                            label: agent_config_label(c, inst),
                            target: ItemTarget::Config(c.id),
                            live,
                            fg,
                        }
                    })
                    .collect()
            }
            Mode::Commands => self
                .processes
                .iter()
                .filter(|r| r.kind == "command")
                .map(|r| Item {
                    label: r.label.clone(),
                    target: ItemTarget::Process(r.id),
                    live: self.process_pane(r.id).is_some(),
                    fg: None,
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
                    fg: None,
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
            Some(ItemTarget::Note(id)) => self.open_document("note", Some(id), false),
            Some(ItemTarget::Process(id)) => match self.process_pane(id) {
                Some(pane) => self.route_pane_to_slot(pane, false),
                None => self.clear_slot(),
            },
            Some(ItemTarget::Config(id)) => match self.config_instance_pane(id) {
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
            BareKey::Char('e') if self.mode == Mode::Agents => self.edit_focused_config(),
            // `n` creates a new item of the current pane type. The kind
            // tracks the mode so a single binding gives the user "new"
            // semantics everywhere it makes sense.
            BareKey::Char('n') if self.mode == Mode::Todos => {
                self.open_document("new-todo", None, true)
            }
            BareKey::Char('n') if self.mode == Mode::Notes => {
                self.open_document("new-note", None, true)
            }
            BareKey::Char('n') if self.mode == Mode::Agents => {
                self.spawn_config_form("new-agent-config", None)
            }
            BareKey::Char('L') => self.open_mode_list(true),
            // `Alt-<1..5>` jumps focus to a sibling sidebar pane, lazygit-style
            // (todo #110). Guarded on the Alt modifier so it takes precedence
            // over the bare `1`/`2` sort bindings below - a plain `1` still
            // sorts. The keypress only reaches `handle_key` when a sidebar
            // plugin pane already has focus (Normal mode); the equivalent
            // gesture from a locked content pane rides Zellij keybinds (see
            // `up::FOCUS_PANE_BINDS`). Both emit the same `panopt:focus-pane`
            // pipe.
            BareKey::Char(c @ '1'..='5') if key.key_modifiers.contains(&KeyModifier::Alt) => {
                if let Some(target) = Mode::from_hotkey(c) {
                    self.request_focus_pane(target);
                }
            }
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
            // Toggle the idle bell (todo #175). Gated to the Todos pane because
            // it is the gatekeeper that observes agent state and rings; the flag
            // on any other instance would never fire.
            BareKey::Char('b') if self.mode == Mode::Todos => self.idle_bell = !self.idle_bell,
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
            // `Alt-/` opens the cockpit's popup search. Spawned locally on
            // whichever sidebar plugin instance has focus; the resulting
            // `search_pane` lives on that instance. The popup's
            // `panopt:close-search` and `panopt:show-result` pipes broadcast
            // to every sidebar instance (the search CLI omits the
            // `--plugin-configuration mode=todos` filter on these two pipes
            // specifically), so the holder always handles its own popup -
            // regardless of which sidebar pane invoked Alt-/. The
            // locked-content-pane keybind in `up::SEARCH_BIND_TO` still
            // routes through `mode=todos` so only Todos spawns from there.
            // Bare `/` stays a no-op stub so it can still reach an inner
            // program if the user's mental model expects that.
            BareKey::Char('/') if key.key_modifiers.contains(&KeyModifier::Alt) => {
                self.spawn_search_dialog()
            }
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
            ItemTarget::Note(id) => self.open_document("note", Some(id), focus),
            ItemTarget::Process(id) => self.activate_process(id, focus),
            ItemTarget::Config(id) => self.activate_config(id, focus),
            ItemTarget::Pane(pane) => self.route_pane_to_slot(pane, focus),
        }
    }

    /// Open the full-list view in the slot for modes that have one. Todos
    /// and Notes display their respective lists; the agent/command/
    /// terminal modes are no-ops because their lists are already shown whole.
    fn open_mode_list(&mut self, focus: bool) {
        match self.mode {
            Mode::Todos => self.open_document("todo-list", None, focus),
            Mode::Notes => self.open_document("note-list", None, focus),
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

    /// Delete the focused item. Todos, notes, and process rows go
    /// through the `panopt` CLI (the daemon owns the durable state); ad-hoc
    /// agent panes and plain shell terminals are just closed via the host.
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
            ItemTarget::Note(id) => self.spawn_delete_gate_dialog("note", id, &label, cwd),
            ItemTarget::Process(id) => self.spawn_delete_gate_dialog("process", id, &label, cwd),
            // Deleting an agent removes its config (the durable record); the
            // delete gate already speaks `agent-tool`. The config's label
            // carries a ` · state` suffix here, so strip it for the dialog.
            ItemTarget::Config(id) => {
                let name = label.split(" · ").next().unwrap_or(&label);
                self.spawn_delete_gate_dialog("agent-tool", id, name, cwd)
            }
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

    /// Spawn the cockpit-wide search popup: a floating pane running
    /// `panopt search`. Mirrors [`Self::spawn_delete_gate_dialog`]'s shape - a
    /// transient interactive CLI in a floating pane. The TUI pipes the
    /// selection back via `panopt:show-result` and the sidebar plugin (here)
    /// dispatches that to the viewer.
    ///
    /// Captures the spawned pane's id into [`Self::search_pane`] so
    /// [`Self::close_search_pane`] can shut it down without racing the
    /// search CLI's own exit (which is what made the user see Zellij's
    /// `EXIT CODE: 0 / <ENTER> re-run` prompt). Idempotent: if a search
    /// popup is already open, do nothing.
    fn spawn_search_dialog(&mut self) {
        if self.search_pane.is_some() {
            return;
        }
        let Some(cwd) = self.launch_cwd() else {
            return;
        };
        let args = vec![
            "search".to_string(),
            "--port".to_string(),
            self.port.clone(),
        ];
        let pane = open_command_pane_floating(
            CommandToRun {
                path: PathBuf::from(&self.panopt_bin),
                args,
                cwd: Some(cwd),
            },
            None,
            BTreeMap::new(),
        );
        self.search_pane = pane;
    }

    /// Ask the `target` sidebar pane to focus itself (todo #110). Broadcast as
    /// a `panopt:focus-pane` pipe carrying the target mode's slug as payload:
    /// every sidebar instance receives it, only the one whose own slug matches
    /// focuses its pane (and no-ops while a popup is up). We broadcast - rather
    /// than narrow with `--plugin-configuration mode=<slug>` - because that
    /// narrowing routed every digit to the gatekeeper instead of the named
    /// instance; the same self-filtering broadcast backs `close-search` /
    /// `show-result`. We go through `zellij action pipe` rather than the
    /// in-process `pipe_message_to_plugin` for the same reason the cockpit
    /// keybinds do: that API launches a fresh instance when no `(url, config)`
    /// match is found. Unlike the keybind path, a plugin-issued `run_command`
    /// spawns no transient focus-stealing pane.
    fn request_focus_pane(&self, target: Mode) {
        run_command(
            &[
                "zellij",
                "action",
                "pipe",
                "--name",
                "panopt:focus-pane",
                "--",
                target.slug(),
            ],
            BTreeMap::new(),
        );
    }

    /// Close the floating search popup pane (if any) and clear the tracking
    /// field. Used by both the show-result and close-search pipe arms.
    fn close_search_pane(&mut self) {
        if let Some(pane) = self.search_pane.take() {
            close_pane_with_id(pane);
        }
    }

    /// Handle the `panopt:show-result` pipe from `panopt search`: parse
    /// `kind`/`id`, close the search popup, then route the chosen item into
    /// the cockpit's viewer slot via the same path that an Enter on a
    /// sidebar row takes. The popup close happens before `open_document`'s
    /// focus shift so the close action lands on the search pane rather than
    /// the (about-to-be-focused) viewer.
    ///
    /// Early-returns on instances that don't hold the popup, because the
    /// search CLI broadcasts show-result without a `mode=todos` filter so
    /// the holder (which can be any sidebar instance) gets the message
    /// reliably. Only one instance has `search_pane = Some(_)`; the other
    /// four would otherwise duplicate-route the viewer.
    fn handle_search_result(&mut self, payload: Option<&str>) {
        if self.search_pane.is_none() {
            return;
        }
        let Some(payload) = payload else { return };
        let mut kind: Option<&str> = None;
        let mut id: Option<u64> = None;
        for kv in payload.split(';') {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            match k {
                "kind" => kind = Some(v),
                "id" => id = v.parse().ok(),
                _ => {}
            }
        }
        let (Some(kind), Some(id)) = (kind, id) else {
            return;
        };
        self.close_search_pane();
        match kind {
            "todo" => self.open_document("todo", Some(id), true),
            "note" => self.open_document("note", Some(id), true),
            _ => {}
        }
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
            "note" => {
                self.run_panopt(&["note", "rm", &id.to_string(), "--port", &self.port], cwd);
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
            // Deleting an agent config removes its durable slot. Stop any live
            // instance first (kill the process, close its pane) so we don't
            // leave an agent running with no config row backing it, then
            // soft-delete the config via the CLI.
            "agent-tool" => {
                self.stop_config(id);
                self.run_panopt(
                    &["agent-tool", "rm", &id.to_string(), "--port", &self.port],
                    cwd,
                );
            }
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
    /// docs (todos / notes) this is a no-op since they have nothing
    /// to stop.
    fn stop_focused(&mut self) {
        match self.focused_target() {
            Some(ItemTarget::Process(id)) => {
                if let Some(pane) = self.process_pane(id) {
                    close_pane_with_id(pane);
                }
            }
            Some(ItemTarget::Config(id)) => self.stop_config(id),
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

    /// A representative live agent instance of config `config_id`, if any. The
    /// daemon is a 1:N factory (todo #190), so a config can have several live
    /// instances; the config-centric Agents pane binds to the first. The join key
    /// is the instance's `agent_tool_id`, lifted from the ` (from #N)` suffix in
    /// processes.md.
    fn config_instance(&self, config_id: u64) -> Option<&ProcessRow> {
        self.processes
            .iter()
            .find(|r| r.kind == "agent" && r.agent_tool_id == Some(config_id))
    }

    /// The Zellij pane hosting config `config_id`'s live instance, if any.
    fn config_instance_pane(&self, config_id: u64) -> Option<PaneId> {
        self.config_instance(config_id)
            .and_then(|r| self.process_pane(r.id))
    }

    /// Activate an agent config: focus its live instance's pane if running,
    /// else start a fresh instance via `panopt process start <config_id>`. The
    /// daemon writes a `starting` row that `reconcile_starting_processes` turns
    /// into a pane on the next tick - the same path a manual start takes.
    fn activate_config(&mut self, config_id: u64, focus: bool) {
        if let Some(pane) = self.config_instance_pane(config_id) {
            self.route_pane_to_slot(pane, focus);
            return;
        }
        let Some(cwd) = self.launch_cwd() else {
            return;
        };
        let id_str = config_id.to_string();
        self.run_panopt(
            &["process", "start", id_str.as_str(), "--port", &self.port],
            cwd,
        );
    }

    /// Stop an agent config's live instance: signal the process via
    /// `panopt process stop <instance_id>` (which marks the row stopped, so it
    /// leaves the projection) and close its now-defunct pane. A no-op when the
    /// config has no live instance. The config row itself stays in the list.
    fn stop_config(&mut self, config_id: u64) {
        let Some(inst_id) = self.config_instance(config_id).map(|r| r.id) else {
            return;
        };
        let pane = self.process_pane(inst_id);
        if let Some(cwd) = self.launch_cwd() {
            let id_str = inst_id.to_string();
            self.run_panopt(
                &["process", "stop", id_str.as_str(), "--port", &self.port],
                cwd,
            );
        }
        if let Some(pane) = pane {
            close_pane_with_id(pane);
        }
    }

    /// Open the agent-config form for the focused config, if one is focused.
    /// Enter in the Agents pane starts/focuses the *instance* (lifecycle, #143)
    /// and the content slot holds that live agent pane, so editing the durable
    /// config gets its own `e` gesture - mirroring `e` on a todo.
    fn edit_focused_config(&mut self) {
        if let Some(ItemTarget::Config(id)) = self.focused_target() {
            self.spawn_config_form("agent-config", Some(id));
        }
    }

    /// Float the agent-config editor (`panopt _viewer --transient`). Unlike the
    /// todo/note forms, this is a floating overlay rather than a tiled content
    /// pane: the Agents content slot is occupied by the live agent pane, and
    /// swapping a viewer into it (then closing it) would suppress the agent and
    /// trip the content-slot floor into spawning replacement panes. A floating
    /// pane is excluded from the content-slot accounting (`!p.floating`), so it
    /// never disturbs the agent and closes cleanly. The viewer still autosaves
    /// and refreshes while open - it is the same `_viewer` process.
    fn spawn_config_form(&mut self, kind: &str, id: Option<u64>) {
        let Some(cwd) = self.launch_cwd() else {
            return;
        };
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
            "--transient".to_string(),
        ];
        if let Some(id) = id {
            args.push("--id".to_string());
            args.push(id.to_string());
        }
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

    /// Reconcile daemon-owned `starting` process rows into panes (todo #141).
    ///
    /// panoptd writes a `starting` row as the *desired* state; this gatekeeper
    /// is the effector that turns it into a live Zellij pane by spawning the
    /// same `_process-run` shim a manual activation uses. That shim resolves the
    /// spawn plan and reports its pid (flipping the row to `running`); here we
    /// report the pane it landed in. Each row is reconciled exactly once
    /// (`reconciled_panes` guards the gap before the new pane shows up in the
    /// manifest), and only by the Todos gatekeeper, so a single start yields a
    /// single pane rather than one per plugin instance.
    ///
    /// Stop is deliberately *not* reconciled: `process_stop` kills the process
    /// and leaves the pane standing (the plugin never closes panes), so a
    /// `stopped` row needs no pane action.
    fn reconcile_starting_processes(&mut self) {
        if self.mode != Mode::Todos || !self.permitted {
            return;
        }
        // Prune entries whose row is gone. A stopped instance restarts under a
        // fresh id (the daemon never reuses one), so a pruned id never returns.
        let live: Vec<u64> = self.processes.iter().map(|r| r.id).collect();
        self.reconciled_panes.retain(|id, _| live.contains(id));

        let starting: Vec<u64> = self
            .processes
            .iter()
            .filter(|r| r.status.as_deref() == Some("starting"))
            .map(|r| r.id)
            .collect();
        for id in starting {
            if self.reconciled_panes.contains_key(&id) || self.process_pane(id).is_some() {
                continue;
            }
            let id_str = id.to_string();
            let args = vec![
                "_process-run".to_string(),
                "--port".to_string(),
                self.port.clone(),
                id_str.clone(),
            ];
            // Reconcile in the background - a new instance appearing must not
            // yank keyboard focus off whatever pane the user is in.
            if let Some(PaneId::Terminal(tid)) = self.spawn_in_slot(args, false) {
                self.reconciled_panes.insert(id, tid);
                // Hand the daemon the pane the instance landed in. The pid is
                // reported separately by the in-pane shim; this is best-effort.
                if let Some(cwd) = self.launch_cwd() {
                    let tid_str = tid.to_string();
                    self.run_panopt(
                        &[
                            "_process-report",
                            "--port",
                            self.port.as_str(),
                            "--id",
                            id_str.as_str(),
                            "--pane-id",
                            tid_str.as_str(),
                        ],
                        cwd,
                    );
                }
            }
        }
    }

    /// The reconciled instances that have left the live set (todo #207): an
    /// `(id, terminal_pane_id)` for each `reconciled_panes` entry whose row is
    /// now gone (deleted) or terminal (`stopped`/`exited`). Pure — split out of
    /// [`Self::reconcile_disposed_processes`] so the disposal predicate is unit
    /// testable without the Zellij host. A still-`starting`/`running` row is
    /// never disposed.
    fn disposed_reconciled_panes(&self) -> Vec<(u64, u32)> {
        self.reconciled_panes
            .iter()
            .filter(
                |(id, _)| match self.processes.iter().find(|r| r.id == **id) {
                    None => true,
                    Some(r) => matches!(r.status.as_deref(), Some("stopped") | Some("exited")),
                },
            )
            .map(|(id, tid)| (*id, *tid))
            .collect()
    }

    /// Suppress the panes of instances that have left the live set (todo #207).
    ///
    /// `process_stop`/`process_delete` (e.g. an orchestrator disposing a
    /// sub-agent over MCP) change only the daemon record; the agent's Zellij
    /// pane is left standing - a dead husk cluttering the cockpit. This is the
    /// effector that reconciles disposal into the UI, the mirror of
    /// [`Self::reconcile_starting_processes`]: when a reconciled instance's row
    /// is gone or terminal but its pane is still drawn and unsuppressed, hide it.
    /// Suppress, never close (the never-close-panes invariant), so the user can
    /// still resurface the pane.
    ///
    /// Runs before `reconcile_starting_processes` prunes `reconciled_panes`, so a
    /// deleted row's pane mapping is still in hand. Idempotent: once a disposed
    /// pane is handled its entry is dropped, so a still-present `stopped` row is
    /// not re-hidden every tick (and the user re-surfacing it is not fought).
    /// Gatekeeper-only, like the spawn reconciler, so the five plugin instances
    /// don't all act on the same rows.
    fn reconcile_disposed_processes(&mut self) {
        if self.mode != Mode::Todos || !self.permitted {
            return;
        }
        for (id, tid) in self.disposed_reconciled_panes() {
            let pane = PaneId::Terminal(tid);
            match self.panes.iter().find(|p| p.id == pane) {
                // Drawn and on-screen: hide it, then forget the mapping.
                Some(p) if !p.suppressed && !p.floating => {
                    self.suppress_pane(pane);
                    self.reconciled_panes.remove(&id);
                }
                // Already suppressed (or a floating overlay): nothing to do, but
                // our work on this instance is done - stop tracking it.
                Some(_) => {
                    self.reconciled_panes.remove(&id);
                }
                // Not in this tick's manifest (gone, or its husk not surfaced
                // yet): leave the entry for a retry or the deleted-row prune.
                None => {}
            }
        }
    }

    /// Suppress (hide, never close) a disposed agent/command/terminal pane,
    /// honoring the never-close-panes invariant: spawn a fresh empty viewer in
    /// the pane's exact place. `open_command_pane_in_place_of_pane_id` keeps the
    /// tile's geometry and drops `target` onto Zellij's suppressed stack -
    /// off-screen, still running, resurfaceable - so the husk disappears
    /// without reshaping the layout. Focus stays on the sidebar so a background
    /// disposal never yanks the user off what they are doing.
    ///
    /// Crucially we do NOT reuse [`Self::first_suppressed_viewer`] here, unlike
    /// the document slot (`show_in_slot`/`find_or_show`). That suppressed-viewer
    /// pool belongs to the single content slot's swap-in-place rotation (one
    /// pane visible in `slot_pane`, the rest parked off-screen). An agent pane
    /// is its OWN independent tile, not the slot; surfacing a pool viewer into
    /// it pulls that viewer out of the rotation, leaving `slot_pane` and the
    /// pool desynced from Zellij's real layout - which scrambled the whole
    /// cockpit when a sub-agent was disposed over MCP (#207 regression). A
    /// dedicated empty viewer per disposal costs one process but keeps the slot
    /// pool untouched.
    fn suppress_pane(&mut self, target: PaneId) {
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
        let replacement = open_command_pane_in_place_of_pane_id(
            target,
            CommandToRun {
                path: PathBuf::from(&self.panopt_bin),
                args,
                cwd: Some(ws),
            },
            false,
            BTreeMap::new(),
        );
        // An agent/command tile is never the document slot, but guard anyway:
        // if `target` somehow was the slot, hand the slot to its replacement so
        // `slot_pane` keeps pointing at on-screen content.
        if self.slot_pane == Some(target) {
            self.slot_pane = replacement;
        }
        if let Some(plugin) = self.plugin_pane {
            focus_pane_with_id(plugin, false, false);
        }
    }

    /// Rebuild the per-type status matchers from `.panopt/agent-types.md` when
    /// its text changes (todo #142). A cheap no-op when unchanged - the patterns
    /// are static for the daemon's lifetime and regex compilation is not free.
    fn reload_status_matchers(&mut self) {
        let body = fs::read_to_string("/host/.panopt/agent-types.md").unwrap_or_default();
        if body == self.agent_types_src {
            return;
        }
        self.status_matchers = parse_agent_type_matchers(&body);
        self.agent_types_src = body;
    }

    /// Classify each running agent's pane output and report `agent_state`
    /// changes to the daemon (todo #142). The status half of the lifecycle: the
    /// plugin is the only code that can read a pane's buffer, so it captures the
    /// viewport, runs that type's compiled patterns locally, and reports just
    /// the derived one-word state (never the output) - and only on a *change*,
    /// so a steady agent costs no subprocess. Gated to the Todos gatekeeper so
    /// the five plugin instances don't each observe the same panes.
    fn observe_agent_states(&mut self) {
        if self.mode != Mode::Todos || !self.permitted {
            return;
        }
        // A running agent backed by a config whose type we have patterns for is
        // observable; snapshot `(id, tool_type, agent_id)` so the borrow of
        // `self.processes` is released before the per-process round-trips below.
        // `agent_id` (the row's stable name) is the key that binds the row to a
        // pane below.
        let observable: Vec<(u64, String, Option<String>)> = self
            .processes
            .iter()
            .filter(|r| r.kind == "agent" && r.status.as_deref() == Some("running"))
            .filter_map(|r| r.tool_type.clone().map(|t| (r.id, t, r.agent_id.clone())))
            .collect();
        let live: Vec<u64> = observable.iter().map(|(id, _, _)| *id).collect();
        self.reported_states.retain(|id, _| live.contains(id));

        let Some(cwd) = self.launch_cwd() else {
            return;
        };
        for (id, tool_type, agent_id) in observable {
            let Some(pane) = self.agent_pane(id, agent_id.as_deref()) else {
                continue;
            };
            let Ok(contents) = get_pane_scrollback(pane, false) else {
                continue;
            };
            // Tee the viewport to the capture file so the daemon's
            // process_output / search_output tools can serve it (todo #190 line).
            // Independent of status classification, so it runs even for types
            // without status patterns.
            self.write_output_capture(id, &contents.viewport);
            let Some(matcher) = self.status_matchers.get(&tool_type) else {
                continue;
            };
            // Classify only the live footer/prompt region, not the whole
            // viewport: an agent scrolls its transcript through the visible
            // area, so matching all of it makes the state sticky and flappy
            // (bug #163). See `viewport_live_region`.
            let state = matcher
                .classify(&viewport_live_region(&contents.viewport))
                .as_str();
            let prev = self.reported_states.get(&id).map(String::as_str);
            if prev == Some(state) {
                continue;
            }
            // Ring on a genuine busy -> idle transition (todo #175): the agent
            // finished a work cycle and wants attention. `prev == None` is the
            // boot case - a freshly spawned agent's first observed state is idle
            // (sitting at its prompt), which must NOT ring (the #237 boot-settle
            // caveat). An agent that was thinking/working and settles does.
            if self.idle_bell && state == "idle" && matches!(prev, Some(p) if p != "idle") {
                self.pending_bell = true;
            }
            let id_str = id.to_string();
            self.run_panopt(
                &[
                    "_process-report",
                    "--port",
                    self.port.as_str(),
                    "--id",
                    id_str.as_str(),
                    "--agent-state",
                    state,
                ],
                cwd.clone(),
            );
            self.reported_states.insert(id, state.to_string());
        }
    }

    /// Deliver queued input into agent panes (todo #160): the cockpit half of
    /// `send_input` and of an ad-hoc spawn's opening prompt. Reads the daemon's
    /// `inputs.jsonl`, writes each `content` into the addressed instance's pane
    /// via the Zellij host API, and acks the `seq` so the daemon drops it.
    ///
    /// Gated to the Todos gatekeeper (one of the five sidebar instances) and to
    /// `running` instances - typing into a `starting` pane before the agent has
    /// booted would be lost. A `delivered_inputs` set guards the window between
    /// our write and the projection catching up, so an input is typed once.
    /// Tee a running agent's recent viewport rows to the cockpit capture file
    /// `.panopt/.cockpit/output-<id>.txt` - the bounded, ~1s-lagged raw channel
    /// the daemon's `process_output`/`search_output` tools read (todo #190 line).
    /// Best-effort: a failed write just means the orchestrator sees no/stale
    /// output for that tick. The window is capped so the file stays small.
    fn write_output_capture(&self, id: u64, viewport: &[String]) {
        const MAX_ROWS: usize = 300;
        let start = viewport.len().saturating_sub(MAX_ROWS);
        let body = viewport[start..].join("\n");
        let dir = "/host/.panopt/.cockpit";
        let _ = fs::create_dir_all(dir);
        let _ = fs::write(format!("{dir}/output-{id}.txt"), body);
    }

    fn deliver_pending_inputs(&mut self) {
        if self.mode != Mode::Todos || !self.permitted {
            return;
        }
        // Flush Enter keystrokes deferred from the previous tick first: the body
        // was typed last tick, so this carriage return now arrives as a separate
        // input event (Claude Code has finished absorbing the paste) and submits
        // the prompt instead of inserting a newline. Drained before new inputs so
        // this tick's writes get their own Enter on the *next* tick, never now.
        for pane in std::mem::take(&mut self.pending_submit) {
            write_chars_to_pane_id("\r", pane);
        }
        let Ok(body) = fs::read_to_string(INPUTS_PATH) else {
            return;
        };
        // Snapshot (seq, pane, content) for delivered-now rows so the borrow of
        // `self.processes` is released before we ack via a subprocess. Only
        // running instances whose pane we can resolve are eligible.
        let mut to_write: Vec<(i64, PaneId, String)> = Vec::new();
        for row in body.lines().filter_map(parse_input_line) {
            if self.delivered_inputs.contains(&row.seq) {
                continue;
            }
            // Deliver only once the agent is *ready*, not merely `running`: the
            // edge reports its pid (flipping the row to running) just before it
            // execs the agent, so `running` precedes the TUI by seconds. Typing
            // then is lost, and the input is acked with no retry. `agent_state`
            // is set only after the status observer has classified the pane - a
            // good proxy for "the prompt is up and accepting input" (#190 line:
            // a spawned agent's opening prompt was delivered into a not-yet-ready
            // pane and dropped).
            let Some(proc) = self.processes.iter().find(|r| {
                if r.id != row.process_id || r.status.as_deref() != Some("running") {
                    return false;
                }
                // Ready = the status observer has classified the pane at least
                // once (agent_state set), our proxy for "the TUI is up". A type
                // with no status patterns can never be classified, so fall back to
                // delivering on `running` for it rather than waiting forever.
                let observable = r
                    .tool_type
                    .as_deref()
                    .is_some_and(|t| self.status_matchers.contains_key(t));
                r.agent_state.is_some() || !observable
            }) else {
                continue;
            };
            let Some(pane) = self.agent_pane(row.process_id, proc.agent_id.as_deref()) else {
                continue;
            };
            to_write.push((row.seq, pane, row.content));
        }
        let Some(cwd) = self.launch_cwd() else {
            return;
        };
        for (seq, pane, content) in to_write {
            // Claude Code's TUI inserts a written "\n" into its multiline prompt
            // rather than running it - submission is the Enter *key*, a carriage
            // return. So the send_input contract ("a trailing newline submits")
            // is honored by translation: type the body now, and defer the "\r" to
            // the next tick (queued on `pending_submit`). The defer matters - a
            // body+CR written together is read by Claude Code as one paste burst,
            // where the CR becomes a literal newline; a CR arriving ~1s later,
            // after the paste settles, is a real Enter that runs the prompt (bug:
            // a spawned agent's prompt was typed but never submitted).
            let submit = content.ends_with('\n');
            let body = content.trim_end_matches(['\r', '\n']);
            write_chars_to_pane_id(body, pane);
            if submit {
                self.pending_submit.push(pane);
            }
            self.delivered_inputs.insert(seq);
            let seq_str = seq.to_string();
            self.run_panopt(
                &[
                    "_input-ack",
                    "--port",
                    self.port.as_str(),
                    "--seq",
                    seq_str.as_str(),
                ],
                cwd.clone(),
            );
        }
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
        // A stale `slot_pane` pointing at a suppressed pane must not count as
        // the live slot: the reconcile in `ingest_panes` clears it, but a
        // keypress can arrive before the next PaneUpdate. Require the slot to
        // be visible so we always take the replace/show branch (which swaps
        // `pane` into the real content slot) rather than focusing a hidden
        // pane in place - the latter splits the focused sidebar pane.
        let is_slot = self.slot_pane == Some(pane) && self.pane_is_visible(pane);
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
        self.spawn_blank_viewer();
    }

    /// Spawn a fresh empty `_viewer` pane with its own routing slot, no gate.
    /// Split out of [`Self::spawn_blank_pane`] so the content floor can summon
    /// a replacement pane even when focus has fallen to the sidebar (where the
    /// `spawn_blank_pane` gate refuses) - the floor only reaches here when the
    /// session has no suppressed pane to resurface, so a fresh one is the only
    /// way to stop the sidebar expanding to full width.
    fn spawn_blank_viewer(&mut self) {
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
        // Mint a stable id when none was given so the plugin owns it and can
        // call `_agent-leave` for the pane on death. Without this, an
        // unnamed cockpit agent would generate its own random id inside the
        // pane and the plugin would have no way to identify it later.
        let (final_id, label) = match id {
            Some(given) => (given.to_string(), given.to_string()),
            None => {
                self.next_agent += 1;
                let n = self.next_agent;
                (format!("cockpit-agent-{n}"), format!("Agent {n}"))
            }
        };
        let args = vec!["_agent".to_string(), "--id".to_string(), final_id.clone()];
        let Some(PaneId::Terminal(tid)) = self.spawn_in_slot(args, true) else {
            return;
        };
        self.agent_labels.insert(tid, label);
        self.agent_pane_ids.insert(tid, final_id);
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
        // Closing the last content pane leaves no slot and no obvious way back
        // (focus falls to the sidebar, where pane creation is refused). Refuse
        // the explicit keybind close cleanly here, same shape as the sidebar
        // refusal above and ahead of the active-work dialog whose confirm path
        // would otherwise bypass this. `ensure_content_slot` is the safety net
        // for the close paths a keybind gate cannot see (Ctrl-c, exit, crash).
        if self.content_pane_count() <= 1 {
            self.refuse_gate("cannot close the last content pane");
            return;
        }
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

    /// Count live content panes across the whole session. `self.panes` already
    /// excludes the five plugin panes (skipped at ingest), so this counts only
    /// right-side content. Suppressed-but-running panes are included (they are
    /// swappable content, just hidden); exited/held panes and floating overlays
    /// (search popup, gate dialogs) are excluded. Drives
    /// [`PanoptPane::gate_close_focus`] (refuse the last `x`/`Ctrl-q` close) and
    /// [`PanoptPane::publish_content_count`] (so a viewer can refuse the last
    /// Ctrl-c/`q` close, which never routes through the plugin).
    fn content_pane_count(&self) -> usize {
        self.panes
            .iter()
            .filter(|p| !p.floating && !p.exited)
            .count()
    }

    /// Count the *live* content panes drawn on the right: not floating (an
    /// overlay, not the content slot), not suppressed (running but hidden
    /// behind a swapped-in pane), and not exited. The exited exclusion is the
    /// subtle one - a `panopt _agent`/`_viewer` is a Zellij command pane, so
    /// when its command exits (e.g. `/q` in an agent) the pane does not close,
    /// it lingers drawn showing the exit status. That husk holds its tile, so
    /// the sidebar does not expand, but it is dead content: not a slot the user
    /// can do anything with. When this count hits zero the right side has no
    /// usable pane - either nothing at all (full-width sidebar) or only a husk -
    /// and [`Self::ensure_content_slot`] reclaims it. Distinct from
    /// `content_pane_count`, which counts suppressed panes (a hidden-but-
    /// swappable pane is still a reason to refuse a *deliberate* close) but is
    /// only used to gate that close.
    fn live_content_pane_count(&self) -> usize {
        self.panes
            .iter()
            .filter(|p| !p.floating && !p.suppressed && !p.exited)
            .count()
    }

    /// Keep exactly one live, usable content pane on the right. Covers the
    /// close paths neither the keybind gate nor the viewer's in-pane gate can
    /// see: an agent `/q`/exit leaving a dead husk, a crash, or a swap that
    /// suppressed the boot viewer behind a pane the user then closed - and the
    /// outright-empty case (#151) where Zellij would re-tile the sidebar to
    /// full width with no obvious way back.
    ///
    /// When no live content is drawn, hand off to [`Self::clear_slot`], which
    /// routes a viewer to `empty` and swaps it into the slot *in place of*
    /// whatever sits there - replacing a dead husk by suppressing it (never
    /// closing it: the plugin-never-closes-panes invariant holds), reusing the
    /// suppressed boot viewer when one exists, or spawning a fresh empty viewer
    /// only as a last resort. The result is always one clean empty pane, never
    /// a second pane beside the husk.
    ///
    /// Reads only this instance's own live manifest, so it is correct even if a
    /// stale `content-count` file from another cockpit session on the same
    /// project is misleading. Gatekeeper-only, like the other session-wide
    /// projections, so the five instances don't all react to the same manifest.
    fn ensure_content_slot(&mut self) {
        if self.mode != Mode::Todos || !self.permitted {
            return;
        }
        if self.live_content_pane_count() > 0 {
            self.has_had_content = true;
            return;
        }
        // Zero before we have ever seen content is the cockpit still booting
        // the layout's `--slot main` viewer, not a drop - acting here would
        // race a duplicate pane in front of the real one. Wait for it.
        if !self.has_had_content {
            return;
        }
        self.clear_slot();
    }

    /// Publish [`Self::content_pane_count`] to [`CONTENT_COUNT_PATH`] so each
    /// `_viewer` can tell, at close time, whether it is the only right-side
    /// pane left. Only the Todos gatekeeper writes (every instance sees the
    /// same manifest, so the count is identical), and only when it changes, to
    /// avoid rewriting the file on every manifest tick.
    fn publish_content_count(&mut self) {
        if self.mode != Mode::Todos {
            return;
        }
        let count = self.content_pane_count();
        if self.last_content_count == Some(count) {
            return;
        }
        self.last_content_count = Some(count);
        write_content_count(count);
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

fn is_transient_pipe_pane(p: &PaneInfo) -> bool {
    p.terminal_command.as_deref().is_some_and(|c| {
        c.contains("zellij") && c.contains("action") && c.contains("pipe") && c.contains("panopt:")
    })
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

/// Atomically publish the live right-side content-pane count to
/// [`CONTENT_COUNT_PATH`] (temp + rename), for viewers to read at close time.
fn write_content_count(count: usize) {
    let dir = "/host/.panopt/.cockpit";
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = format!("{dir}/.content-count.tmp");
    if fs::write(&tmp, count.to_string()).is_ok() {
        let _ = fs::rename(&tmp, CONTENT_COUNT_PATH);
    }
}

/// Per-mode shared view file, keyed by [`Mode::letter`] so each mode's
/// presentation is independent (Todos cursor never moves the Notes cursor) and
/// only same-mode instances across clients share one file.
fn view_state_path(mode: Mode) -> String {
    format!("/host/.panopt/.cockpit/view-{}.json", mode.letter())
}

/// Atomically publish a [`ViewState`] (temp + rename), like the other
/// `.cockpit/` writers.
fn write_view_state(mode: Mode, view: &ViewState) {
    let dir = "/host/.panopt/.cockpit";
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let body = format!(
        "{{\"cursor\":{},\"scroll\":{},\"filter\":{},\"sort1\":{},\"sort2\":{},\"help\":{},\"seq\":{}}}",
        view.cursor,
        view.scroll,
        view.filter.to_wire(),
        view.sort_1.to_wire(),
        view.sort_2.to_wire(),
        view.show_help as u8,
        view.seq,
    );
    let letter = mode.letter();
    let tmp = format!("{dir}/.view-{letter}.tmp");
    if fs::write(&tmp, body).is_ok() {
        let _ = fs::rename(&tmp, view_state_path(mode));
    }
}

/// Read the per-mode shared view, tolerant of a missing/partial file (returns
/// `None` when there is no `seq` to compare). Unknown filter/sort codes from a
/// newer writer fall back to defaults rather than dropping the whole read.
fn read_view_state(mode: Mode) -> Option<ViewState> {
    let body = fs::read_to_string(view_state_path(mode)).ok()?;
    let seq = view_field(&body, "seq")?;
    Some(ViewState {
        cursor: view_field(&body, "cursor").unwrap_or(0) as usize,
        scroll: view_field(&body, "scroll").unwrap_or(0) as usize,
        filter: view_field(&body, "filter")
            .and_then(|c| TodoFilter::from_wire(c as u8))
            .unwrap_or_default(),
        sort_1: view_field(&body, "sort1")
            .and_then(|c| TodoSort::from_wire(c as u8))
            .unwrap_or_default(),
        sort_2: view_field(&body, "sort2")
            .and_then(|c| TodoSort::from_wire(c as u8))
            .unwrap_or(TodoSort::CreatedAsc),
        show_help: view_field(&body, "help").unwrap_or(0) != 0,
        seq,
    })
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

fn read_configs(path: &str) -> Vec<ConfigRow> {
    match fs::read_to_string(path) {
        Ok(body) => body.lines().filter_map(parse_config_line).collect(),
        Err(_) => Vec::new(),
    }
}

/// The ANSI styling a printed row carries.
#[derive(Clone, Copy)]
enum Style {
    Normal,
    Dim,
}

/// Truncate `content` to `cols` and wrap it in the SGR codes for `style`,
/// with the focused row reversed and an optional 256-colour foreground `fg`
/// (todo #236). The codes are added after truncation so they never count toward
/// the width. `fg` rides alongside the reverse/dim codes; on the focused row the
/// reverse (`7`) swaps fg/bg, so the colour shows as the row background - still
/// a distinct per-state cue.
fn paint(content: &str, cols: usize, style: Style, fg: Option<u8>, focused: bool) -> String {
    let truncated: String = content.chars().take(cols).collect();
    let mut codes: Vec<String> = Vec::new();
    if focused {
        codes.push("7".to_string());
    }
    match style {
        Style::Dim => codes.push("2".to_string()),
        Style::Normal => {}
    }
    if let Some(c) = fg {
        codes.push(format!("38;5;{c}"));
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
    fn disposed_reconciled_panes_selects_only_gone_or_terminal_rows() {
        let mut pane = pane_with(Mode::Todos);
        let row = |id: u64, status: &str| ProcessRow {
            id,
            status: Some(status.to_string()),
            ..Default::default()
        };
        // #1 running (live), #2 stopped, #3 exited; #4 has no row (deleted).
        pane.processes = vec![row(1, "running"), row(2, "stopped"), row(3, "exited")];
        pane.reconciled_panes = [(1u64, 11u32), (2, 12), (3, 13), (4, 14)]
            .into_iter()
            .collect();

        let mut disposed = pane.disposed_reconciled_panes();
        disposed.sort();
        // Only the terminal (#2/#3) and deleted (#4) rows are disposed; the
        // running instance keeps its pane.
        assert_eq!(disposed, vec![(2, 12), (3, 13), (4, 14)]);
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
    fn ingest_panes_drops_a_suppressed_slot_and_readopts_the_viewer() {
        use std::collections::HashMap;
        // The Agents instance holds an agent pane as its slot. Another
        // instance then swaps a viewer into the content area, suppressing the
        // agent. On the next manifest the agent (id 4) is suppressed and a
        // visible viewer (id 7) holds the slot. The stale agent slot must be
        // dropped and the visible viewer re-adopted - otherwise Enter on the
        // agent would `focus_pane_with_id` a suppressed pane and split the
        // focused sidebar pane.
        let mut sidebar = pane_with(Mode::Agents);
        sidebar.slot_pane = Some(PaneId::Terminal(4));
        let mut panes = HashMap::new();
        panes.insert(
            0usize,
            vec![
                PaneInfo {
                    id: 4,
                    is_selectable: true,
                    is_suppressed: true,
                    terminal_command: Some("/bin/panopt _agent --id a".to_string()),
                    ..Default::default()
                },
                PaneInfo {
                    id: 7,
                    is_selectable: true,
                    terminal_command: Some(
                        "/bin/panopt _viewer --slot vt1 --port 7600".to_string(),
                    ),
                    ..Default::default()
                },
            ],
        );
        sidebar.ingest_panes(PaneManifest { panes });
        assert_eq!(sidebar.slot_pane, Some(PaneId::Terminal(7)));
    }

    #[test]
    fn live_content_count_excludes_suppressed_panes() {
        use std::collections::HashMap;
        // A visible agent in front of the suppressed boot viewer. The whole-
        // session count (which gates the deliberate close) sees both; the live
        // count (which the reclaim watches) sees only the agent - a hidden pane
        // does nothing to stop the sidebar expanding.
        let mut panes = HashMap::new();
        panes.insert(
            0usize,
            vec![
                PaneInfo {
                    id: 3,
                    is_selectable: true,
                    is_suppressed: true,
                    terminal_command: Some(
                        "/bin/panopt _viewer --slot main --port 7600".to_string(),
                    ),
                    ..Default::default()
                },
                PaneInfo {
                    id: 9,
                    is_selectable: true,
                    terminal_command: Some("/bin/panopt _agent --id a".to_string()),
                    ..Default::default()
                },
            ],
        );
        let mut sidebar = pane_with(Mode::Todos);
        sidebar.ingest_panes(PaneManifest { panes });
        assert_eq!(sidebar.content_pane_count(), 2);
        assert_eq!(sidebar.live_content_pane_count(), 1);
    }

    #[test]
    fn exited_husk_is_not_live_content() {
        use std::collections::HashMap;
        // `/q` in an agent pane exits its command, but the `_agent` command
        // pane does not close - it lingers drawn, showing the exit status. It
        // holds its tile (so the sidebar does not expand) but it is dead: not a
        // usable slot. The live count must therefore read zero so the reclaim
        // fires and swaps a clean empty viewer in over the husk - rather than a
        // stale husk being mistaken for usable content and left on screen.
        let mut panes = HashMap::new();
        panes.insert(
            0usize,
            vec![
                PaneInfo {
                    id: 9,
                    is_selectable: true,
                    exited: true,
                    terminal_command: Some("/bin/panopt _agent --id a".to_string()),
                    ..Default::default()
                },
                PaneInfo {
                    id: 3,
                    is_selectable: true,
                    is_suppressed: true,
                    terminal_command: Some(
                        "/bin/panopt _viewer --slot main --port 7600".to_string(),
                    ),
                    ..Default::default()
                },
            ],
        );
        let mut sidebar = pane_with(Mode::Todos);
        sidebar.ingest_panes(PaneManifest { panes });
        // The husk is excluded from the deliberate-close count (it is gone as
        // far as swappable content goes)...
        assert_eq!(sidebar.content_pane_count(), 1);
        // ...and it is not live content either, so the reclaim treats the right
        // side as having no usable pane and steps in.
        assert_eq!(sidebar.live_content_pane_count(), 0);
        // The boot viewer is still around (suppressed) for `clear_slot` to swap
        // back over the husk.
        assert_eq!(sidebar.first_suppressed_viewer(), Some(PaneId::Terminal(3)));
    }

    #[test]
    fn floor_waits_for_the_boot_viewer_before_arming() {
        use std::collections::HashMap;
        // A PaneUpdate can arrive with zero content before the layout's
        // `--slot main` viewer is registered. The floor must not arm then
        // (spawning would duplicate the boot viewer); `has_had_content` stays
        // false until a visible content pane is actually seen, and flips once
        // one is.
        let mut sidebar = pane_with(Mode::Todos);
        sidebar.ingest_panes(PaneManifest {
            panes: HashMap::new(),
        });
        assert!(!sidebar.has_had_content);

        let mut panes = HashMap::new();
        panes.insert(
            0usize,
            vec![PaneInfo {
                id: 3,
                is_selectable: true,
                terminal_command: Some("/bin/panopt _viewer --slot main --port 7600".to_string()),
                ..Default::default()
            }],
        );
        sidebar.ingest_panes(PaneManifest { panes });
        assert!(sidebar.has_had_content);
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
            ("notes", Mode::Notes),
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
            Mode::Notes,
        ]
        .into_iter()
        .map(|m| m.letter())
        .collect();
        assert_eq!(letters.len(), 5, "every mode needs its own slot prefix");
    }

    #[test]
    fn viewer_slot_carries_mode_letter() {
        let mut todos = pane_with(Mode::Todos);
        let mut notes = pane_with(Mode::Notes);
        let a = todos.allocate_viewer_slot();
        let b = notes.allocate_viewer_slot();
        // Two panes both allocating their first slot - the mode letter is
        // what stops them from colliding on the same `v1`.
        assert_ne!(a, b);
        assert!(a.contains('t'));
        assert!(b.contains('s'));
    }
}
