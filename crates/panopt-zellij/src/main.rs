//! PANopt coordination sidebar - a Zellij plugin.
//!
//! Renders a sidebar with five collapsible sections - todos, agents, terminals,
//! commands, scratchpads - read from PANopt's `.panopt/` projection and from
//! Zellij's live pane state. A caret toggles each section; up/down move a cursor
//! through the rows; left/right collapse and expand; the mouse clicks any of it.
//!
//! The cockpit is this sidebar plus one content pane on the right. Selecting an
//! item swaps its pane into that one slot and suppresses whatever was there - a
//! suppressed pane keeps running, just hidden, no stack and no title bar.
//! Documents (todos, scratchpads, lists) all share one re-pointable
//! `panopt _viewer` pane; agents, commands, and terminals are each their own
//! pane. Moving the cursor previews the selected item in the slot - or clears
//! the slot when the row has nothing to show - always without taking focus off
//! the sidebar. A click does the same; Enter additionally focuses the pane.
//!
//! If the user splits the content pane, a selection swaps into whichever pane
//! was focused last before the sidebar took focus - the designated slot.
//!
//! The plugin never closes a pane: agents, terminals, and viewers alike are the
//! user's to keep or close.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use zellij_tile::prelude::*;

/// How many items the todos and scratchpads sections show before a "more" row.
const ITEM_LIMIT: usize = 7;

/// Routing slot prefix for viewer panes the plugin spawns ad hoc. The layout
/// boots one viewer with `--slot main`; further viewers spawned by
/// [`ensure_viewer_in_slot`](PanoptSidebar::ensure_viewer_in_slot) get unique
/// names `v1`, `v2`, ... so each pane has its own
/// `.panopt/.cockpit/viewer-<slot>.json` routing file. Per-pane routing keeps
/// sidebar navigation single-pane: only the slot's viewer re-points on a
/// preview, leaving any other split's viewer on whatever it was last showing
/// (the user's "kept doc" pattern).
const SPAWNED_VIEWER_SLOT_PREFIX: &str = "v";

/// The five sidebar sections, in fixed top-to-bottom order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SectionKind {
    Todos,
    Agents,
    Terminals,
    Commands,
    Scratchpads,
}

impl SectionKind {
    const ORDER: [SectionKind; 5] = [
        SectionKind::Todos,
        SectionKind::Agents,
        SectionKind::Terminals,
        SectionKind::Commands,
        SectionKind::Scratchpads,
    ];

    fn label(self) -> &'static str {
        match self {
            SectionKind::Todos => "Todos",
            SectionKind::Agents => "Agents",
            SectionKind::Terminals => "Terminals",
            SectionKind::Commands => "Commands",
            SectionKind::Scratchpads => "Scratchpads",
        }
    }

    /// Todos and scratchpads are long; they show a capped list. The roster and
    /// terminal sections are short and shown whole.
    fn limited(self) -> bool {
        matches!(self, SectionKind::Todos | SectionKind::Scratchpads)
    }
}

#[derive(Default)]
struct PanoptSidebar {
    /// Absolute project root, from the layout's plugin config. The cwd for
    /// spawned panes.
    ws: Option<String>,
    /// Absolute path to the `panopt` binary, from the layout's plugin config.
    panopt_bin: String,
    /// The daemon port, from the layout's plugin config.
    port: String,
    /// Whether Zellij has granted the requested permissions.
    permitted: bool,

    /// Todos parsed from `.panopt/todos.md`: `(id, label)`.
    todos: Vec<(u64, String)>,
    /// Scratchpads parsed from `.panopt/scratchpads.md`: `(id, label)`.
    scratchpads: Vec<(u64, String)>,
    /// Roster entries parsed from `.panopt/roster.md`.
    roster: Vec<RosterRow>,
    /// Live (and suppressed) content panes flattened from Zellij's manifest.
    panes: Vec<PaneRow>,

    /// The five sections, rebuilt from the data above on every change.
    sections: Vec<Section>,
    /// Per-section collapsed flag, kept across rebuilds. Indexed like
    /// [`SectionKind::ORDER`].
    collapsed: [bool; 5],
    /// The row the keyboard cursor is on.
    focus: Focus,
    /// One printed-row -> meaning entry, rebuilt every render for click routing.
    row_map: Vec<RowKind>,

    /// This plugin's own pane id, learned at load - used to return focus to the
    /// sidebar after a swap.
    plugin_pane: Option<PaneId>,
    /// The pane occupying the designated content slot: the pane a selection
    /// swaps against. It is the last non-plugin pane focused before the sidebar
    /// took focus, updated in place whenever the plugin swaps the slot itself.
    slot_pane: Option<PaneId>,
    /// How many ad-hoc agents have been numbered, for the next "Agent N" label.
    next_agent: u32,
    /// Counter for allocating unique routing slot names for viewer panes the
    /// plugin spawns. The boot viewer keeps its `--slot main` from the layout;
    /// every new viewer takes `v<n>` here, never recycled.
    next_viewer_slot: u32,
    /// Per-agent sidebar label, keyed by terminal pane id. The label is a
    /// mutable *presentation* string - today a stable "Agent N", later meant to
    /// be refreshed from the agent's own published activity. Ordering must
    /// never derive from it: agent rows are ordered by pane id (creation
    /// order), so the label is free to change without reshuffling the sidebar.
    agent_labels: BTreeMap<u32, String>,
}

/// A parsed `.panopt/roster.md` line.
struct RosterRow {
    kind: String,
    id: u64,
    label: String,
}

/// A content pane flattened from Zellij's manifest.
struct PaneRow {
    id: PaneId,
    title: String,
    focused: bool,
    /// A suppressed pane is hidden but still running - swapped out of the slot
    /// by an earlier selection. Used by
    /// [`route_pane_to_slot`](PanoptSidebar::route_pane_to_slot) and
    /// [`ensure_viewer_in_slot`](PanoptSidebar::ensure_viewer_in_slot) to tell
    /// whether a target pane is already on screen.
    suppressed: bool,
    exited: bool,
    role: PaneRole,
    /// For [`PaneRole::Viewer`] panes only: the `--slot X` token from the
    /// launch command, used as the routing file name
    /// `.panopt/.cockpit/viewer-<slot>.json`. `None` on any other role.
    viewer_slot: Option<String>,
}

/// What a content pane is, derived from the command it was launched with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PaneRole {
    /// The shared `panopt _viewer` document pane.
    Viewer,
    /// An ad-hoc `panopt _agent` pane, started with `a`.
    Agent,
    /// A `panopt _roster-run <id>` pane, by roster id.
    Roster(u64),
    /// A plain terminal the user opened.
    Shell,
}

/// One built section: its kind and its items.
struct Section {
    kind: SectionKind,
    items: Vec<Item>,
}

/// One item within a section.
struct Item {
    label: String,
    target: ItemTarget,
    /// A live marker: a running roster entry, or the Zellij-focused pane.
    live: bool,
}

/// What selecting an item does.
#[derive(Clone)]
enum ItemTarget {
    Todo(u64),
    Scratchpad(u64),
    /// A roster agent or command, by roster id.
    Roster(u64),
    /// An existing pane: an ad-hoc agent or a plain terminal.
    Pane(PaneId),
}

/// The keyboard cursor: a section, and a row within it (`None` = its header).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
struct Focus {
    section: usize,
    item: Option<usize>,
}

/// What a printed row means, for resolving a mouse click.
#[derive(Clone, Copy)]
enum RowKind {
    /// Not interactive: the title, a placeholder, the help line.
    Inert,
    /// A section header. A click in columns 0-1 hits the caret.
    Header(usize),
    /// An item: section index, item index.
    Item(usize, usize),
    /// The "+N more" row of a capped section.
    More(usize),
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
        self.plugin_pane = Some(PaneId::Plugin(get_plugin_ids().plugin_id));
        // Clear any routing files left by a previous cockpit session.
        let _ = fs::remove_dir_all("/host/.panopt/.cockpit");
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
        self.rebuild_sections();
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
                self.rebuild_sections();
                true
            }
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Timer(_) => {
                self.reload_data();
                self.rebuild_sections();
                set_timeout(1.0);
                true
            }
            _ => false,
        }
    }

    /// `panopt:spawn-agent` opens a new agent pane; `panopt:spawn-blank-pane`
    /// opens a fresh empty viewer in a brand-new tiled pane - Alt-N's cockpit
    /// behaviour, dispatched from the keybind via `MessagePlugin`.
    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        match pipe_message.name.as_str() {
            "panopt:spawn-agent" => {
                self.spawn_agent_pane(pipe_message.payload.as_deref());
                true
            }
            "panopt:spawn-blank-pane" => {
                self.spawn_blank_pane();
                true
            }
            _ => false,
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        // The plugin's stdout becomes the pane content. Each emitted line must
        // carry non-whitespace - Zellij's parser drops a blank line - so the
        // row map and the printed lines stay in lockstep.
        let mut lines: Vec<(String, RowKind, Style)> = Vec::new();

        let title = if self.permitted {
            "PANopt  [a]gent [c]todo".to_string()
        } else {
            "PANopt - grant permissions in the Zellij prompt".to_string()
        };
        lines.push((title, RowKind::Inert, Style::HEADER));

        for (si, section) in self.sections.iter().enumerate() {
            let collapsed = self.collapsed[si];
            let caret = if collapsed { '>' } else { 'v' };
            let header = format!("{caret} {} ({})", section.kind.label(), section.items.len());
            lines.push((header, RowKind::Header(si), Style::HEADER));
            if collapsed {
                continue;
            }
            if section.items.is_empty() {
                lines.push(("  (none)".to_string(), RowKind::Inert, Style::DIM));
                continue;
            }
            let limit = if section.kind.limited() {
                ITEM_LIMIT
            } else {
                usize::MAX
            };
            for (ii, item) in section.items.iter().enumerate().take(limit) {
                let marker = if item.live { '*' } else { ' ' };
                lines.push((
                    format!(" {marker}{}", item.label),
                    RowKind::Item(si, ii),
                    Style::NORMAL,
                ));
            }
            if section.items.len() > limit {
                let more = section.items.len() - limit;
                lines.push((
                    format!("  +{more} more"),
                    RowKind::More(si),
                    Style::DIM,
                ));
            }
        }

        self.row_map = lines.iter().map(|(_, kind, _)| *kind).take(rows).collect();
        for (line, kind, style) in lines.into_iter().take(rows) {
            let focused = self.row_is_focused(kind);
            print!("{}\r\n", paint(&line, cols, style, focused));
        }
    }
}

impl PanoptSidebar {
    // --- data ---

    /// Re-read the three projected index files from Zellij's `/host` mount.
    fn reload_data(&mut self) {
        self.todos = read_index("/host/.panopt/todos.md");
        self.scratchpads = read_index("/host/.panopt/scratchpads.md");
        self.roster = read_roster("/host/.panopt/roster.md");
    }

    /// Flatten the pane manifest into the content-pane list - suppressed panes
    /// included, since they are the hidden agents and terminals the sidebar
    /// still lists - and keep the designated slot pane pointing at a live pane.
    fn ingest_panes(&mut self, manifest: PaneManifest) {
        let mut tabs: Vec<&usize> = manifest.panes.keys().collect();
        tabs.sort();
        let mut rows = Vec::new();
        let mut focused_non_plugin: Option<PaneId> = None;
        for tab in tabs {
            for p in &manifest.panes[tab] {
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
                });
            }
        }
        // Order content panes by pane id - creation order. Zellij's manifest
        // does not keep a stable order as panes move in and out of the
        // suppressed set, and every slot swap suppresses one pane and reveals
        // another; without this sort the sidebar's rows reshuffle under the
        // keyboard cursor between one arrow press and the next.
        rows.sort_by_key(|p| p.id);
        self.panes = rows;
        self.sync_agent_labels();
        // Keep `slot_pane` on the pane focused before the sidebar took focus:
        // only overwrite it when a non-plugin pane is focused.
        if let Some(pane) = focused_non_plugin {
            self.slot_pane = Some(pane);
        }
        // Drop the slot pane if it has since closed.
        if let Some(slot) = self.slot_pane {
            if !self.panes.iter().any(|p| p.id == slot) {
                self.slot_pane = None;
            }
        }
        // Adopt a slot pane when there is none yet - at startup, the lone
        // viewer pane - so the first selection swaps in place. Only adopt a
        // visible pane: replacing against a suppressed slot wastes the swap
        // call and can leave the layout confused.
        if self.slot_pane.is_none() {
            self.slot_pane = self
                .panes
                .iter()
                .find(|p| !p.suppressed && p.role == PaneRole::Viewer)
                .or_else(|| self.panes.iter().find(|p| !p.suppressed))
                .map(|p| p.id);
        }
    }

    /// Whether `pane` is currently on screen - present in the manifest and not
    /// suppressed. A pane that is already visible should never be swapped into
    /// the slot: [`replace_pane_with_existing_pane`] would yank it out of its
    /// current position, collapsing whichever split it was filling.
    fn pane_is_visible(&self, pane: PaneId) -> bool {
        self.panes.iter().any(|p| p.id == pane && !p.suppressed)
    }

    /// The routing slot name of a viewer pane, if `pane` is a viewer with one.
    fn viewer_slot_of(&self, pane: PaneId) -> Option<String> {
        self.panes
            .iter()
            .find(|p| p.id == pane)
            .and_then(|p| p.viewer_slot.clone())
    }

    /// The first suppressed viewer pane, if one is being held offscreen by a
    /// previous slot swap. Cheaper than spawning a new viewer when the slot
    /// needs to switch back to a document.
    fn first_suppressed_viewer(&self) -> Option<PaneId> {
        self.panes
            .iter()
            .find(|p| p.role == PaneRole::Viewer && p.suppressed)
            .map(|p| p.id)
    }

    /// Allocate the next unique routing slot name for a viewer the plugin is
    /// about to spawn.
    fn allocate_viewer_slot(&mut self) -> String {
        self.next_viewer_slot += 1;
        format!("{SPAWNED_VIEWER_SLOT_PREFIX}{}", self.next_viewer_slot)
    }

    /// The running pane of roster entry `id`, if it is running.
    fn roster_pane(&self, id: u64) -> Option<PaneId> {
        self.panes
            .iter()
            .find(|p| p.role == PaneRole::Roster(id) && !p.exited)
            .map(|p| p.id)
    }

    /// Keep [`agent_labels`](Self::agent_labels) in step with the live agent
    /// panes: forget closed ones, and give any agent still unlabelled - one the
    /// plugin did not spawn itself, e.g. after a plugin reload - a stable
    /// "Agent N" fallback. New numbers only ever go up, so a label, once
    /// assigned, never changes for the life of its pane.
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
    }

    /// The sidebar label for an agent pane: its mutable display label, or the
    /// pane title as a fallback if it is somehow not yet labelled.
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

    /// Rebuild the five sections from the parsed data and live pane state.
    fn rebuild_sections(&mut self) {
        let mut sections = Vec::with_capacity(5);
        for kind in SectionKind::ORDER {
            let items = match kind {
                SectionKind::Todos => self
                    .todos
                    .iter()
                    .map(|(id, label)| Item {
                        label: format!("#{id} {label}"),
                        target: ItemTarget::Todo(*id),
                        live: false,
                    })
                    .collect(),
                SectionKind::Scratchpads => self
                    .scratchpads
                    .iter()
                    .map(|(id, label)| Item {
                        label: format!("#{id} {label}"),
                        target: ItemTarget::Scratchpad(*id),
                        live: false,
                    })
                    .collect(),
                SectionKind::Agents => {
                    // Roster agents, plus ad-hoc `a`-spawned agent panes that
                    // are not roster entries.
                    let mut items: Vec<Item> = self
                        .roster
                        .iter()
                        .filter(|r| r.kind == "agent")
                        .map(|r| Item {
                            label: r.label.clone(),
                            target: ItemTarget::Roster(r.id),
                            live: self.roster_pane(r.id).is_some(),
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
                SectionKind::Commands => self
                    .roster
                    .iter()
                    .filter(|r| r.kind == "command")
                    .map(|r| Item {
                        label: r.label.clone(),
                        target: ItemTarget::Roster(r.id),
                        live: self.roster_pane(r.id).is_some(),
                    })
                    .collect(),
                SectionKind::Terminals => self
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
            sections.push(Section { kind, items });
        }
        self.sections = sections;
        self.clamp_focus();
    }

    // --- focus ---

    /// Every keyboard-navigable row, in order: each header, then each item of
    /// an expanded section.
    fn nav_rows(&self) -> Vec<Focus> {
        let mut rows = Vec::new();
        for (si, section) in self.sections.iter().enumerate() {
            rows.push(Focus { section: si, item: None });
            if !self.collapsed[si] {
                let shown = visible_item_count(section);
                for ii in 0..shown {
                    rows.push(Focus { section: si, item: Some(ii) });
                }
            }
        }
        rows
    }

    /// Keep the focus on a row that still exists after a rebuild.
    fn clamp_focus(&mut self) {
        if self.focus.section >= self.sections.len() {
            self.focus = Focus::default();
            return;
        }
        if let Some(ii) = self.focus.item {
            let section = &self.sections[self.focus.section];
            let shown = visible_item_count(section);
            if self.collapsed[self.focus.section] || ii >= shown {
                self.focus.item = None;
            }
        }
    }

    fn row_is_focused(&self, kind: RowKind) -> bool {
        match kind {
            RowKind::Header(si) => self.focus.section == si && self.focus.item.is_none(),
            RowKind::Item(si, ii) => {
                self.focus.section == si && self.focus.item == Some(ii)
            }
            _ => false,
        }
    }

    /// Step the keyboard cursor by `delta` rows. Returns whether the cursor
    /// actually moved - `false` when it was already at the first or last row.
    fn move_focus(&mut self, delta: i64) -> bool {
        let nav = self.nav_rows();
        if nav.is_empty() {
            return false;
        }
        let here = nav
            .iter()
            .position(|f| f.section == self.focus.section && f.item == self.focus.item)
            .unwrap_or(0);
        let next = (here as i64 + delta).clamp(0, nav.len() as i64 - 1) as usize;
        let moved = nav[next] != self.focus;
        self.focus = nav[next];
        moved
    }

    /// The target of the currently focused item, or `None` when the cursor is
    /// on a section header.
    fn focused_target(&self) -> Option<ItemTarget> {
        let ii = self.focus.item?;
        self.sections
            .get(self.focus.section)
            .and_then(|s| s.items.get(ii))
            .map(|item| item.target.clone())
    }

    /// Preview the focused row in the slot, leaving focus on the sidebar. A
    /// document re-points every viewer; a running pane is routed into the slot
    /// or - when it is already visible in another split - the slot clears
    /// instead, since a TTY cannot be in two places at once. A row with
    /// nothing to show (a section header, a roster entry that is not running)
    /// also clears the slot. Preview never starts a process.
    fn preview_focus(&mut self) {
        match self.focused_target() {
            Some(ItemTarget::Todo(id)) => self.open_document("todo", Some(id), false),
            Some(ItemTarget::Scratchpad(id)) => {
                self.open_document("scratchpad", Some(id), false)
            }
            Some(ItemTarget::Roster(id)) => match self.roster_pane(id) {
                Some(pane) => self.route_pane_to_slot(pane, false),
                None => self.clear_slot(),
            },
            Some(ItemTarget::Pane(pane)) => self.route_pane_to_slot(pane, false),
            None => self.clear_slot(),
        }
    }

    // --- input ---

    fn handle_key(&mut self, key: KeyWithModifier) -> bool {
        match key.bare_key {
            BareKey::Up => {
                if self.move_focus(-1) {
                    self.preview_focus();
                }
            }
            BareKey::Down => {
                if self.move_focus(1) {
                    self.preview_focus();
                }
            }
            BareKey::Left => self.set_collapsed(self.focus.section, true),
            BareKey::Right => self.set_collapsed(self.focus.section, false),
            BareKey::Enter => self.activate_focus(),
            BareKey::Char('a') => self.spawn_agent_pane(None),
            BareKey::Char('c') => self.open_todo_form(None),
            BareKey::Char('e') => self.edit_focused_todo(),
            _ => return false,
        }
        true
    }

    fn handle_mouse(&mut self, mouse: Mouse) -> bool {
        match mouse {
            Mouse::LeftClick(line, col) => {
                if line < 0 {
                    return false;
                }
                let handled = match self.row_map.get(line as usize).copied() {
                    Some(RowKind::Header(si)) => {
                        self.focus = Focus { section: si, item: None };
                        if col < 2 {
                            self.toggle_collapsed(si);
                        } else {
                            self.open_section_list(si, false);
                        }
                        true
                    }
                    Some(RowKind::Item(si, ii)) => {
                        self.focus = Focus { section: si, item: Some(ii) };
                        self.activate_item(si, ii, false);
                        true
                    }
                    Some(RowKind::More(si)) => {
                        self.open_section_list(si, false);
                        true
                    }
                    _ => false,
                };
                if handled {
                    // A click anywhere in the sidebar keeps it focused - and,
                    // when the sidebar was not focused, this is the click that
                    // both focuses it and selects the row.
                    if let Some(plugin) = self.plugin_pane {
                        focus_pane_with_id(plugin, false, false);
                    }
                }
                handled
            }
            Mouse::ScrollUp(_) => {
                if self.move_focus(-1) {
                    self.preview_focus();
                }
                true
            }
            Mouse::ScrollDown(_) => {
                if self.move_focus(1) {
                    self.preview_focus();
                }
                true
            }
            _ => false,
        }
    }

    /// Act on the focused row from the keyboard (Enter): a header opens its
    /// list, an item opens itself, and focus moves onto the content pane.
    fn activate_focus(&mut self) {
        match self.focus.item {
            None => self.open_section_list(self.focus.section, true),
            Some(ii) => self.activate_item(self.focus.section, ii, true),
        }
    }

    /// Act on item `ii` of section `si`. `focus` moves keyboard focus onto the
    /// content pane (Enter); a click passes `false` to stay in the sidebar.
    fn activate_item(&mut self, si: usize, ii: usize, focus: bool) {
        let Some(target) = self
            .sections
            .get(si)
            .and_then(|s| s.items.get(ii))
            .map(|item| item.target.clone())
        else {
            return;
        };
        match target {
            ItemTarget::Todo(id) => self.open_document("todo", Some(id), focus),
            ItemTarget::Scratchpad(id) => self.open_document("scratchpad", Some(id), focus),
            ItemTarget::Roster(id) => self.activate_roster(id, focus),
            ItemTarget::Pane(pane) => self.route_pane_to_slot(pane, focus),
        }
    }

    /// Open a section's full list. Todos and scratchpads open a list in the
    /// viewer; the roster and terminal sections just toggle collapse. `focus`
    /// is threaded to the viewer the same way [`activate_item`] threads it.
    fn open_section_list(&mut self, si: usize, focus: bool) {
        match self.sections.get(si).map(|s| s.kind) {
            Some(SectionKind::Todos) => self.open_document("todo-list", None, focus),
            Some(SectionKind::Scratchpads) => {
                self.open_document("scratchpad-list", None, focus)
            }
            _ => self.toggle_collapsed(si),
        }
    }

    fn toggle_collapsed(&mut self, si: usize) {
        if si < self.collapsed.len() {
            self.set_collapsed(si, !self.collapsed[si]);
        }
    }

    fn set_collapsed(&mut self, si: usize, collapsed: bool) {
        if si < self.collapsed.len() {
            self.collapsed[si] = collapsed;
            self.clamp_focus();
        }
    }

    /// Open the floating todo form for the focused todo, if one is focused.
    fn edit_focused_todo(&mut self) {
        if let Some(ItemTarget::Todo(id)) = self.focused_target() {
            self.open_todo_form(Some(id));
        }
    }

    // --- slot routing ---

    /// Open a document (`todo`/`scratchpad`) or a section list
    /// (`todo-list`/`scratchpad-list`) in the slot's viewer. Per-pane routing
    /// means only the slot's viewer re-points; any other viewer pane the user
    /// has on screen (a split they made to keep a doc visible, say) stays on
    /// whatever it was showing. If the slot is not a viewer yet, a viewer is
    /// brought into it: revealing a suppressed one when available, spawning a
    /// fresh one otherwise.
    fn open_document(&mut self, kind: &str, id: Option<u64>, focus: bool) {
        if !self.permitted {
            return;
        }
        self.ensure_viewer_in_slot(kind, id, focus);
    }

    /// Clear the slot to the empty viewer - what the cockpit shows when the
    /// selection lands on a row with nothing useful to display (a section
    /// header, a roster entry that is not running, or a Pane target that is
    /// already visible in another split and cannot be duplicated). Per-pane
    /// routing means only the slot's viewer clears; any other split's viewer
    /// keeps whatever it was showing.
    fn clear_slot(&mut self) {
        self.ensure_viewer_in_slot("empty", None, false);
    }

    /// Bring roster entry `id` into the slot: swap in its pane when it is
    /// already running, otherwise start it there.
    fn activate_roster(&mut self, id: u64, focus: bool) {
        if let Some(pane) = self.roster_pane(id) {
            self.route_pane_to_slot(pane, focus);
            return;
        }
        let args = vec![
            "_roster-run".to_string(),
            "--port".to_string(),
            self.port.clone(),
            id.to_string(),
        ];
        self.spawn_in_slot(args, focus);
    }

    /// Route an existing terminal pane (agent, terminal, running roster
    /// command) into the slot. A pane that is already visible in another split
    /// cannot be duplicated: instead a preview clears the slot - so the arrow
    /// press has a visible effect - and an activate just focuses the existing
    /// pane, leaving the slot alone.
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

    /// Ensure the slot hosts a visible viewer pane and that the viewer is
    /// pointed at `kind`/`id`. Three cases:
    ///
    /// - The slot already holds a visible viewer: write its routing file and
    ///   focus it if asked. The other-viewer routing was handled by the caller
    ///   (`open_document`).
    /// - A suppressed viewer is available: bring it into the slot via
    ///   [`show_in_slot`] after writing routing to its slot name, so it
    ///   re-displays with the right item.
    /// - No viewer exists yet: allocate a fresh slot name, pre-write its
    ///   routing file, and spawn `panopt _viewer --slot <name>` into the slot.
    fn ensure_viewer_in_slot(&mut self, kind: &str, id: Option<u64>, focus: bool) {
        if let Some(slot) = self.slot_pane {
            if self.pane_is_visible(slot) {
                if let Some(slot_name) = self.viewer_slot_of(slot) {
                    write_routing(kind, id, &slot_name);
                    if focus {
                        focus_pane_with_id(slot, false, false);
                    }
                    return;
                }
            }
        }
        if let Some(viewer) = self.first_suppressed_viewer() {
            if let Some(slot_name) = self.viewer_slot_of(viewer) {
                write_routing(kind, id, &slot_name);
            }
            self.show_in_slot(viewer, focus);
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

    /// Swap an existing pane into the designated slot, suppressing whatever was
    /// there. `focus` moves keyboard focus onto it; otherwise focus returns to
    /// the sidebar so the user can keep navigating.
    ///
    /// This is the low-level primitive: it does not check whether `pane` is
    /// already visible in another split. Callers reaching this for an
    /// already-visible target route through [`route_pane_to_slot`] instead, to
    /// avoid yanking the pane out of its current position.
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
            // A swap can move focus onto the new pane; hand it back.
            if let Some(plugin) = self.plugin_pane {
                focus_pane_with_id(plugin, false, false);
            }
        }
    }

    /// Spawn a `panopt` subcommand as a new pane in the designated slot,
    /// suppressing the slot's current pane. Returns the new pane's id.
    fn spawn_in_slot(&mut self, args: Vec<String>, focus: bool) -> Option<PaneId> {
        let ws = self.launch_cwd()?;
        let command = CommandToRun {
            path: PathBuf::from(&self.panopt_bin),
            args,
            cwd: Some(ws),
        };
        let new = match self.slot_pane {
            // `open_command_pane_in_place_of_pane_id` suppresses the replaced
            // pane (false = do not close it) and does not change focus.
            Some(slot) => open_command_pane_in_place_of_pane_id(
                slot,
                command,
                false,
                BTreeMap::new(),
            ),
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

    /// Spawn a fresh empty viewer in a brand-new tiled pane - not in the slot:
    /// the user pressed Alt-N to *add* a pane, not replace the slot's. The
    /// viewer is given a unique routing slot name (`v<n>`) so the sidebar can
    /// re-point it independently of the boot viewer and every other Alt-N
    /// pane - that's what makes the new pane addressable.
    fn spawn_blank_pane(&mut self) {
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
            CommandToRun { path: PathBuf::from(&self.panopt_bin), args, cwd: Some(ws) },
            BTreeMap::new(),
        );
    }

    /// Spawn a new agent pane (`panopt _agent`) in the slot and focus it,
    /// giving it a stable sidebar label: the caller's name, or the next
    /// "Agent N" for an unnamed one.
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
    }

    /// Open the todo form in a floating pane: `panopt todo edit <id>`, or
    /// `--new` when `id` is `None`. Creating and quick-editing a todo use the
    /// floating form; browsing one uses the viewer.
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
            CommandToRun { path: PathBuf::from(&self.panopt_bin), args, cwd: Some(ws) },
            None,
            BTreeMap::new(),
        );
    }

    /// The cwd for a launched pane: the project root. `None` when permissions
    /// are not yet granted or no project is configured.
    fn launch_cwd(&self) -> Option<PathBuf> {
        if !self.permitted {
            return None;
        }
        self.ws.as_ref().map(PathBuf::from)
    }
}

/// Classify a content pane by the command it was launched with. A plain
/// terminal the user opened has no launch command and is a [`PaneRole::Shell`].
fn classify_pane(command: Option<&str>) -> PaneRole {
    let Some(cmd) = command else {
        return PaneRole::Shell;
    };
    if cmd.contains("_viewer") {
        PaneRole::Viewer
    } else if cmd.contains("_roster-run") {
        // The roster id is the last positional arg; `--port <n>` precedes it.
        match cmd
            .split_whitespace()
            .filter_map(|t| t.parse::<u64>().ok())
            .last()
        {
            Some(id) => PaneRole::Roster(id),
            None => PaneRole::Shell,
        }
    } else if cmd.contains("_agent") {
        PaneRole::Agent
    } else {
        PaneRole::Shell
    }
}

/// The sidebar label for a content pane: its title, marked when it has exited.
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

/// The visible (uncapped) item count of a section - what the keyboard cursor
/// can land on.
fn visible_item_count(section: &Section) -> usize {
    if section.kind.limited() {
        section.items.len().min(ITEM_LIMIT)
    } else {
        section.items.len()
    }
}

/// Write the viewer's routing file `.panopt/.cockpit/viewer-<slot>.json`. The
/// viewer polls this file and re-points itself at the named item. Each viewer
/// pane owns its own `slot` token, so writes can target one viewer (a slot
/// clear) or every viewer (a document preview), depending on the caller.
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

/// Pull the `--slot X` token out of a viewer pane's launch command. Returns
/// `None` for a command without that flag (defensive: every viewer the plugin
/// or the layout spawns passes it).
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

/// Read a `.panopt/` index file (`todos.md`, `scratchpads.md`) into
/// `(id, label)` pairs.
fn read_index(path: &str) -> Vec<(u64, String)> {
    match fs::read_to_string(path) {
        Ok(body) => body.lines().filter_map(parse_index_line).collect(),
        Err(_) => Vec::new(),
    }
}

/// Read `.panopt/roster.md` into roster rows.
fn read_roster(path: &str) -> Vec<RosterRow> {
    match fs::read_to_string(path) {
        Ok(body) => body.lines().filter_map(parse_roster_line).collect(),
        Err(_) => Vec::new(),
    }
}

/// Parse one index line - `- [ ] [#3](todos/3.md) the title ...` or
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

/// Parse one `.panopt/roster.md` line - `- [agent] #1 Mediator` - into a row.
fn parse_roster_line(line: &str) -> Option<RosterRow> {
    let rest = line.trim().strip_prefix("- [")?;
    let close = rest.find(']')?;
    let kind = rest[..close].to_string();
    let after = rest[close + 1..].trim_start().strip_prefix('#')?;
    let space = after.find(' ')?;
    let id: u64 = after[..space].parse().ok()?;
    let label = after[space + 1..].trim().to_string();
    Some(RosterRow { kind, id, label })
}

/// The ANSI styling a printed row carries.
#[derive(Clone, Copy)]
enum Style {
    Normal,
    Header,
    Dim,
}

impl Style {
    const NORMAL: Style = Style::Normal;
    const HEADER: Style = Style::Header;
    const DIM: Style = Style::Dim;
}

/// Truncate `content` to `cols` and wrap it in the SGR codes for `style`, with
/// the focused row reversed. The codes are added after truncation so they
/// never count toward the width.
fn paint(content: &str, cols: usize, style: Style, focused: bool) -> String {
    let truncated: String = content.chars().take(cols).collect();
    let mut codes: Vec<&str> = Vec::new();
    if focused {
        codes.push("7");
    }
    match style {
        Style::Header => codes.push("1"),
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

    /// A sidebar with the given todo items and every section expanded.
    fn sidebar_with_todos(n: usize) -> PanoptSidebar {
        let items = (0..n)
            .map(|i| Item {
                label: format!("todo {i}"),
                target: ItemTarget::Todo(i as u64),
                live: false,
            })
            .collect();
        PanoptSidebar {
            sections: vec![Section { kind: SectionKind::Todos, items }],
            ..PanoptSidebar::default()
        }
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
    fn ignores_non_index_lines() {
        assert!(parse_index_line("# Todos").is_none());
        assert!(parse_index_line("_(no todos)_").is_none());
        assert!(parse_index_line("").is_none());
    }

    #[test]
    fn parses_a_roster_line() {
        let row = parse_roster_line("- [agent] #1 NASTL-Mediator").unwrap();
        assert_eq!(row.kind, "agent");
        assert_eq!(row.id, 1);
        assert_eq!(row.label, "NASTL-Mediator");
    }

    #[test]
    fn ignores_non_roster_lines() {
        assert!(parse_roster_line("# Roster").is_none());
        assert!(parse_roster_line("_(no roster entries)_").is_none());
    }

    #[test]
    fn classify_pane_reads_the_launch_command() {
        assert_eq!(
            classify_pane(Some("/bin/panopt _viewer --slot main --port 7600")),
            PaneRole::Viewer
        );
        assert_eq!(
            classify_pane(Some("/bin/panopt _roster-run --port 7600 5")),
            PaneRole::Roster(5)
        );
        assert_eq!(
            classify_pane(Some("/bin/panopt _agent --id mediator-1a2b")),
            PaneRole::Agent
        );
        assert_eq!(classify_pane(Some("/bin/zsh -l")), PaneRole::Shell);
        assert_eq!(classify_pane(None), PaneRole::Shell);
    }

    #[test]
    fn move_focus_walks_the_header_and_each_item() {
        let mut sidebar = sidebar_with_todos(2);
        // The cursor starts on the section header.
        assert_eq!(sidebar.focus, Focus { section: 0, item: None });
        // Down stops on each item in turn.
        assert!(sidebar.move_focus(1));
        assert_eq!(sidebar.focus, Focus { section: 0, item: Some(0) });
        assert!(sidebar.move_focus(1));
        assert_eq!(sidebar.focus, Focus { section: 0, item: Some(1) });
        // Up walks back onto the header.
        assert!(sidebar.move_focus(-1));
        assert!(sidebar.move_focus(-1));
        assert_eq!(sidebar.focus, Focus { section: 0, item: None });
    }

    #[test]
    fn move_focus_reports_no_movement_at_the_ends() {
        let mut sidebar = sidebar_with_todos(1);
        // Already on the first row.
        assert!(!sidebar.move_focus(-1));
        // Move to the last row, then confirm Down there does not move.
        assert!(sidebar.move_focus(1));
        assert!(!sidebar.move_focus(1));
        assert_eq!(sidebar.focus, Focus { section: 0, item: Some(0) });
    }

    #[test]
    fn focused_target_is_none_on_a_header_and_set_on_an_item() {
        let mut sidebar = sidebar_with_todos(1);
        assert!(sidebar.focused_target().is_none());
        sidebar.move_focus(1);
        assert!(matches!(sidebar.focused_target(), Some(ItemTarget::Todo(0))));
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
        // Two agent panes delivered high-id-first, as a slot swap can leave
        // them in the manifest.
        let mut panes = HashMap::new();
        panes.insert(
            0usize,
            vec![
                pane(9, "/bin/panopt _agent --id b"),
                pane(4, "/bin/panopt _agent --id a"),
            ],
        );
        let mut sidebar = PanoptSidebar::default();
        sidebar.ingest_panes(PaneManifest { panes });
        sidebar.rebuild_sections();
        // The Agents section lists them low id first, so the keyboard cursor's
        // index keeps meaning the same agent across rebuilds.
        let agents = &sidebar.sections[1];
        assert!(matches!(agents.kind, SectionKind::Agents));
        assert!(matches!(
            agents.items[0].target,
            ItemTarget::Pane(PaneId::Terminal(4))
        ));
        assert!(matches!(
            agents.items[1].target,
            ItemTarget::Pane(PaneId::Terminal(9))
        ));
    }

    #[test]
    fn parse_viewer_slot_extracts_the_slot_token() {
        // The slot token is what keys the per-pane routing file - the cockpit
        // writes to `viewer-<slot>.json` for each viewer pane.
        assert_eq!(
            parse_viewer_slot(Some("/bin/panopt _viewer --slot main --port 7600")),
            Some("main".to_string())
        );
        assert_eq!(
            parse_viewer_slot(Some("/bin/panopt _viewer --port 7600 --slot v2")),
            Some("v2".to_string())
        );
        assert_eq!(parse_viewer_slot(Some("/bin/zsh -l")), None);
        assert_eq!(parse_viewer_slot(None), None);
    }

    #[test]
    fn ingest_panes_captures_each_viewer_slot_name() {
        // Per-pane routing depends on the plugin learning each viewer's
        // `--slot X` token from the launch command at ingest time.
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
                        "/bin/panopt _viewer --slot v1 --port 7600".to_string(),
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
        let mut sidebar = PanoptSidebar::default();
        sidebar.ingest_panes(PaneManifest { panes });
        // Each viewer's slot token is captured - that's what `write_routing`
        // keys off when re-pointing the slot's viewer at a new item.
        assert_eq!(
            sidebar.viewer_slot_of(PaneId::Terminal(3)),
            Some("main".to_string())
        );
        assert_eq!(
            sidebar.viewer_slot_of(PaneId::Terminal(7)),
            Some("v1".to_string())
        );
        // The suppressed viewer is the one `ensure_viewer_in_slot` reveals
        // into the slot before falling back to spawning a fresh one.
        assert_eq!(
            sidebar.first_suppressed_viewer(),
            Some(PaneId::Terminal(7))
        );
        // The agent pane carries no viewer slot name.
        assert!(sidebar.viewer_slot_of(PaneId::Terminal(9)).is_none());
    }

    #[test]
    fn pane_is_visible_tracks_the_suppressed_flag() {
        // Two agent panes: one on screen, one suppressed (the slot-swap
        // bookkeeping). `route_pane_to_slot` keys off this to clear the slot
        // rather than yank an already-visible pane out of its split - the fix
        // for the disappearing pane when the sidebar previews into a non-slot
        // split.
        use std::collections::HashMap;
        let pane = |id: u32, suppressed: bool| PaneInfo {
            id,
            is_selectable: true,
            is_suppressed: suppressed,
            terminal_command: Some("/bin/panopt _agent".to_string()),
            ..Default::default()
        };
        let mut panes = HashMap::new();
        panes.insert(0usize, vec![pane(4, false), pane(9, true)]);
        let mut sidebar = PanoptSidebar::default();
        sidebar.ingest_panes(PaneManifest { panes });
        assert!(sidebar.pane_is_visible(PaneId::Terminal(4)));
        assert!(!sidebar.pane_is_visible(PaneId::Terminal(9)));
        // A pane not in the manifest at all is also not visible.
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
        let mut sidebar = PanoptSidebar::default();
        sidebar.ingest_panes(manifest(&[4, 9]));
        sidebar.rebuild_sections();
        let agents = &sidebar.sections[1];
        assert_eq!(agents.items[0].label, "Agent 1"); // pane 4
        assert_eq!(agents.items[1].label, "Agent 2"); // pane 9
        // A later manifest delivers the panes in the other order and adds one.
        // Existing labels stay put; the new pane takes the next number; rows
        // stay ordered by pane id.
        sidebar.ingest_panes(manifest(&[12, 9, 4]));
        sidebar.rebuild_sections();
        let agents = &sidebar.sections[1];
        assert_eq!(agents.items[0].label, "Agent 1"); // pane 4, unchanged
        assert_eq!(agents.items[1].label, "Agent 2"); // pane 9, unchanged
        assert_eq!(agents.items[2].label, "Agent 3"); // pane 12, new
    }
}
