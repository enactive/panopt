//! The editable todo form, hosted both standalone by `panopt todo edit` and
//! in-pane by the cockpit's `_viewer`.
//!
//! The form owns its scalar fields (title, status, priority, assignee, tags,
//! body), the comment thread and blocker list it loaded with the todo, and the
//! input rows that append a new comment or blocker. Its `draw` takes a `Rect`
//! so a host with its own surrounding chrome can place it where it likes;
//! `handle_key` returns a [`TodoFormAction`] so the host decides whether to
//! debounce a save or to close.
//!
//! Saves go through the MCP client: scalar field edits via `todo_update`,
//! comment ops via `todo_comment_add` / `update` / `delete`, blocker ops via
//! `todo_set_blockers`, lock acquisition via `todo_lock` / `todo_unlock`. The
//! form is otherwise transport-agnostic - it never reads or writes the
//! `.panopt/` projection itself.

use std::time::Instant;

use anyhow::{anyhow, Result};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use serde_json::{json, Value};
use tui_textarea::{CursorMove, TextArea};

use crate::mcpclient::Client;

/// The cyclable status values, in cycle order.
pub(crate) const STATUSES: [&str; 6] = [
    "open",
    "in_progress",
    "backlog",
    "draft",
    "completed",
    "not_done",
];
/// The cyclable priority values, in cycle order.
pub(crate) const PRIORITIES: [&str; 3] = ["high", "medium", "low"];

/// The form's fields, in Tab order. Comments and blockers participate in the
/// cycle so the user can reach them with the same key the scalar fields use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Field {
    Title,
    Status,
    Priority,
    Assignee,
    Tags,
    Body,
    Comments,
    Blockers,
}

const FIELDS: [Field; 8] = [
    Field::Title,
    Field::Status,
    Field::Priority,
    Field::Assignee,
    Field::Tags,
    Field::Body,
    Field::Comments,
    Field::Blockers,
];

/// One comment, in the shape the form needs to render and edit it.
#[derive(Clone)]
struct CommentEntry {
    id: u64,
    author: String,
    created_at: String,
    body: String,
}

/// One blocker, with the blocked todo's id and its current title (resolved at
/// load time so the user sees `#3 set up auth` instead of just `#3`).
#[derive(Clone)]
struct BlockerEntry {
    id: u64,
    title: String,
}

/// Snapshot of the scalar fields the daemon last reported, captured at
/// load time and after each successful save.
///
/// `flush` diffs the current field values against this baseline and sends only
/// the fields that actually changed. Without the diff, every autosave would
/// write back the form's stale view of fields the user never touched — and
/// would clobber any concurrent change another client made to the same todo
/// (a CLI `todo_complete`, an agent's MCP edit, the user editing the same
/// todo in a second pane). With the diff, an idle field is omitted from
/// `todo_update`, which the daemon treats as "leave unchanged."
#[derive(Clone, Default)]
struct Baseline {
    title: String,
    body: String,
    /// One of the tokens in [`STATUSES`].
    status: String,
    /// One of the tokens in [`PRIORITIES`].
    priority: String,
    assignee: String,
    tags: Vec<String>,
}

/// What [`TodoForm::handle_key`] is telling the host to do next.
///
/// The CLI shell uses `Dirty` only to redraw; it relies on `Ctrl-S` for saves.
/// The viewer uses it to start a debounce window and flush a short time later.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TodoFormAction {
    /// Nothing changed that the host needs to act on.
    Idle,
    /// A field changed: the host should consider this a pending save.
    Dirty,
    /// The user asked to close the form (Ctrl-C or, in the standalone CLI, Esc).
    Close,
}

/// The editable state of the form.
pub struct TodoForm {
    /// The daemon MCP URL with `?ws=...&observer=1`.
    pub(crate) url: String,
    /// The todo's id, or `None` until a new todo is first saved.
    pub(crate) id: Option<u64>,

    title: TextArea<'static>,
    assignee: TextArea<'static>,
    tags: TextArea<'static>,
    body: TextArea<'static>,
    /// Index into [`STATUSES`].
    status: usize,
    /// Index into [`PRIORITIES`].
    priority: usize,
    /// Index into [`FIELDS`].
    focus: usize,

    /// The todo's comment thread, top to bottom, as last seen from the daemon.
    comments: Vec<CommentEntry>,
    /// Selected row in the comments section.
    comment_cursor: usize,
    /// If set, the comment at this index is being edited in place; the
    /// `TextArea` holds the in-progress body.
    editing_comment: Option<(usize, TextArea<'static>)>,
    /// Input row that appends a new comment via `todo_comment_add` on Enter.
    new_comment: TextArea<'static>,

    /// The blockers the daemon last reported, with their titles resolved.
    blockers: Vec<BlockerEntry>,
    /// Position of the focus within the single-line Blockers row. Indices
    /// `0..blockers.len()` point at a chip; `blockers.len()` points at the
    /// trailing `+ id:` input.
    blocker_cursor: usize,
    /// Character offset of the first visible column of the chip strip. Kept
    /// aligned to a chip boundary so a partial chip never anchors the left
    /// edge; advanced when the focused chip would otherwise scroll off-screen.
    blocker_scroll: usize,
    /// Input field that adds a new blocker via `todo_add_blocker` on Enter.
    new_blocker: TextArea<'static>,

    /// Last-seen `created_at` / `updated_at` from the daemon, for the context line.
    pub(crate) created: String,
    pub(crate) updated: String,
    /// Display name of whoever holds `todo:<id>` advisory-locked. `None` when
    /// either the lock is unheld or this form's host has not loaded it yet.
    pub(crate) locked_by: Option<String>,

    /// True when there are unsaved scalar-field edits.
    pub(crate) dirty: bool,
    /// When the first unsaved edit landed; used by the viewer to debounce.
    /// Cleared on a successful flush.
    pub(crate) dirty_since: Option<Instant>,
    /// True when a single Esc may quit - there is nothing unsaved to lose, or
    /// the user has already been warned once. Only the CLI shell consults this;
    /// the in-pane host never quits on Esc.
    pub(crate) can_quit: bool,
    /// Bottom-line feedback shown next to the help string.
    pub(crate) message: String,

    /// First visible visual row of the soft-wrapped Body field. Drives the
    /// `draw_body` scroll so the cursor stays on screen as the user edits or
    /// pastes past the bottom of the field.
    body_scroll: usize,
    /// Visible row count of the Body field as of the most recent `draw_body`.
    /// Drives the half-page step for Ctrl-U / Ctrl-D in [`body_input`], so
    /// paging matches what the user can actually see. Starts at zero and is
    /// refreshed on every render; a Ctrl-U press before the first draw falls
    /// back to a minimum step of one line via [`body_input`]'s `.max(1)`.
    body_view_height: usize,
    /// Screen rectangle the Body field's content occupies (the area *inside*
    /// the bordered block). Captured each draw so click-to-position-cursor
    /// in [`TodoForm::handle_mouse`] can map a click to a logical cursor
    /// without re-deriving the layout. `None` until the first render lands.
    body_area: Option<Rect>,
    /// In-progress or completed mouse selection in the Body field:
    /// `(anchor, tip)` in logical `(row, col)` coordinates. `anchor` is set
    /// on mouse-down, `tip` follows the drag, and a non-empty selection on
    /// mouse-up pushes the text to the system clipboard via
    /// [`crate::clip::copy_to_clipboard`] (external command first, OSC 52
    /// fallback). Cleared on the next key press (any keystroke is treated
    /// as "user moved on") and on a bare click without drag.
    selection: Option<((usize, usize), (usize, usize))>,
    /// Screen rectangles each non-body field occupies, captured every draw
    /// so a left-click outside the Body field can still focus the right
    /// field (Title, Tags, Assignee, Status, Priority, Comments, Blockers).
    /// Rebuilt from scratch on every render so resize / focus changes do
    /// not stale-cache. Body is intentionally not in this list - it has its
    /// own click handling for cursor positioning via `body_area`.
    field_areas: Vec<(Field, Rect)>,
    /// Inner text-content rectangles for the single-line textarea fields
    /// (Title, Assignee, Tags). Drives click-to-position and drag-to-select
    /// inside those fields - separate from `field_areas` because the
    /// clickable hitbox for "focus this field" includes labels / borders
    /// that aren't actual text cells. Body has its own `body_area`.
    text_field_areas: Vec<(Field, Rect)>,

    /// Last-known daemon-side values of the scalar fields, used by `flush` to
    /// send only the fields the user actually changed since load (or since
    /// the previous save). Updated after every successful save.
    baseline: Baseline,
}

impl TodoForm {
    /// A blank form for a not-yet-created todo.
    pub fn blank(url: &str) -> TodoForm {
        TodoForm {
            url: url.to_string(),
            id: None,
            title: text_area(""),
            assignee: text_area(""),
            tags: text_area(""),
            body: text_area(""),
            status: 0,
            priority: index_of(&PRIORITIES, "medium"),
            focus: 0,
            comments: Vec::new(),
            comment_cursor: 0,
            editing_comment: None,
            new_comment: text_area(""),
            blockers: Vec::new(),
            blocker_cursor: 0,
            blocker_scroll: 0,
            new_blocker: text_area(""),
            created: String::new(),
            updated: String::new(),
            locked_by: None,
            dirty: false,
            dirty_since: None,
            can_quit: true,
            message: "new todo - type to begin".to_string(),
            body_scroll: 0,
            body_view_height: 0,
            body_area: None,
            selection: None,
            field_areas: Vec::new(),
            text_field_areas: Vec::new(),
            // A new todo's baseline matches the daemon's defaults for
            // `todo_create`: empty title/body/assignee/tags, status `open`,
            // priority `medium`. After the first save populates `id`, the
            // baseline is refreshed to the values just sent.
            baseline: Baseline {
                title: String::new(),
                body: String::new(),
                status: "open".to_string(),
                priority: "medium".to_string(),
                assignee: String::new(),
                tags: Vec::new(),
            },
        }
    }

    /// A form populated from a `todo_get` result.
    ///
    /// Blocker titles need a `todo_list` lookup to resolve, so callers that
    /// have one already can pass it as `blocker_titles`; otherwise the labels
    /// fall back to just the id.
    pub fn from_todo(
        url: &str,
        todo: &Value,
        blocker_titles: &dyn Fn(u64) -> Option<String>,
    ) -> Result<TodoForm> {
        let id = todo["id"]
            .as_u64()
            .ok_or_else(|| anyhow!("todo response has no id"))?;
        let tags = string_list(&todo["tags"]).join(", ");
        let blocker_ids: Vec<u64> = todo["blockers"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();
        let blockers = blocker_ids
            .into_iter()
            .map(|bid| BlockerEntry {
                id: bid,
                title: blocker_titles(bid).unwrap_or_default(),
            })
            .collect();
        let comments = todo["comments"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|c| {
                        Some(CommentEntry {
                            id: c["id"].as_u64()?,
                            author: c["author"].as_str().unwrap_or("").to_string(),
                            created_at: c["created_at"].as_str().unwrap_or("").to_string(),
                            body: c["body"].as_str().unwrap_or("").to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let title = todo["title"].as_str().unwrap_or("").to_string();
        let body = todo["body"].as_str().unwrap_or("").to_string();
        let status = todo["status"].as_str().unwrap_or("open").to_string();
        let priority = todo["priority"].as_str().unwrap_or("medium").to_string();
        let assignee = todo["assignee"].as_str().unwrap_or("").to_string();
        let tag_list = string_list(&todo["tags"]);
        let baseline = Baseline {
            title: title.clone(),
            body: body.clone(),
            status: status.clone(),
            priority: priority.clone(),
            assignee: assignee.clone(),
            tags: tag_list.clone(),
        };
        Ok(TodoForm {
            url: url.to_string(),
            id: Some(id),
            title: text_area(&title),
            assignee: text_area(&assignee),
            tags: text_area(&tags),
            body: text_area(&body),
            status: index_of(&STATUSES, &status),
            priority: index_of(&PRIORITIES, &priority),
            focus: 0,
            comments,
            comment_cursor: 0,
            editing_comment: None,
            new_comment: text_area(""),
            blockers,
            blocker_cursor: 0,
            blocker_scroll: 0,
            new_blocker: text_area(""),
            created: todo["created_at"].as_str().unwrap_or("").to_string(),
            updated: todo["updated_at"].as_str().unwrap_or("").to_string(),
            locked_by: todo["locked_by"].as_str().map(str::to_string),
            dirty: false,
            dirty_since: None,
            can_quit: true,
            message: format!("editing todo #{id}"),
            body_scroll: 0,
            body_view_height: 0,
            body_area: None,
            selection: None,
            field_areas: Vec::new(),
            text_field_areas: Vec::new(),
            baseline,
        })
    }

    /// Handle one key press. The returned [`TodoFormAction`] tells the host whether
    /// to mark the form dirty (and start its autosave debounce) or to close.
    pub fn handle_key(&mut self, key: KeyEvent) -> TodoFormAction {
        if key.kind != KeyEventKind::Press {
            return TodoFormAction::Idle;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // Ctrl-Shift-C: copy the current selection to the system clipboard.
        // Body uses our wrap-aware `self.selection`; single-line textareas
        // (Title / Assignee / Tags) keep their own `selection_range()` after
        // a mouse drag. Handled before the Ctrl-C close arm below so the
        // shift modifier disambiguates the two.
        //
        // On Mac, ⌘C is intercepted by WezTerm (its built-in Copy) and
        // never reaches the app. Users who want ⌘C to behave the same way
        // can map it to Ctrl-Shift-C in their WezTerm config:
        //
        //   { key = 'c', mods = 'CMD',
        //     action = wezterm.action.SendKey { key = 'c', mods = 'CTRL|SHIFT' } }
        if ctrl && shift && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
            let focused_field = FIELDS[self.focus];
            let copied = match (focused_field, self.selection) {
                (Field::Body, Some((anchor, tip))) if anchor != tip => {
                    let text = selected_text(self.body.lines(), anchor, tip);
                    if !text.is_empty() {
                        let _ = crate::clip::copy_to_clipboard(&text);
                    }
                    true
                }
                _ => false,
            };
            if !copied {
                if let Some(area) = self.single_line_textarea_mut(focused_field) {
                    if let Some((anchor, tip)) = area.selection_range() {
                        if anchor != tip {
                            let text = selected_text(area.lines(), anchor, tip);
                            if !text.is_empty() {
                                let _ = crate::clip::copy_to_clipboard(&text);
                            }
                        }
                    }
                }
            }
            return TodoFormAction::Idle;
        }

        // Any other keystroke moves the user on, so the visible selection
        // is stale - clear it before dispatching.
        self.selection = None;

        // While editing a comment in place, every key but the commit / cancel
        // pair flows into the editor's TextArea.
        if self.editing_comment.is_some() {
            return self.handle_comment_edit_key(key);
        }

        match key.code {
            KeyCode::Char('c') if ctrl => TodoFormAction::Close,
            KeyCode::Tab => {
                self.focus = (self.focus + 1) % FIELDS.len();
                TodoFormAction::Idle
            }
            KeyCode::BackTab => {
                self.focus = (self.focus + FIELDS.len() - 1) % FIELDS.len();
                TodoFormAction::Idle
            }
            _ => self.field_key(key),
        }
    }

    /// Handle one mouse event in the Body field.
    ///
    /// - Left-click (`Down`): clear any prior selection, move the textarea
    ///   cursor to the clicked character, plant the selection anchor there.
    /// - Drag with left button held: extend the selection tip to the current
    ///   character; the cursor follows the tip so the user sees where they
    ///   are.
    /// - Release: if the selection is non-empty, emit it to the system
    ///   clipboard via OSC 52 (`\x1b]52;c;<base64>\x07`); a bare click
    ///   without drag clears the zero-length selection.
    /// - Scroll wheel: walk the textarea cursor up / down by one row; the
    ///   existing viewport-follow logic in `draw_body` then catches up.
    ///
    /// Drag and Up events whose coordinates leave the body rect are still
    /// processed for the bookkeeping (extend / finalize) but use the last
    /// in-rect logical position for the geometry; this is how every native
    /// text widget handles "drag off the bottom of the field."
    pub fn handle_mouse(&mut self, m: MouseEvent) -> TodoFormAction {
        let body_area = self.body_area;
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Body click: focus + position the cursor + start a selection.
                if let Some(area) = body_area {
                    if let Some(pos) = self.body_logical_pos_at(area, m.row, m.column) {
                        self.focus = FIELDS
                            .iter()
                            .position(|f| *f == Field::Body)
                            .unwrap_or(self.focus);
                        self.body
                            .move_cursor(CursorMove::Jump(pos.0 as u16, pos.1 as u16));
                        self.selection = Some((pos, pos));
                        return TodoFormAction::Idle;
                    }
                }
                // Single-line textarea field: focus, position cursor, plant
                // a selection anchor for the impending drag.
                if let Some((field, inner)) = self.text_field_at(m.row, m.column) {
                    self.focus = FIELDS
                        .iter()
                        .position(|f| *f == field)
                        .unwrap_or(self.focus);
                    if let Some(area) = self.single_line_textarea_mut(field) {
                        let col = m.column.saturating_sub(inner.x) as usize;
                        area.cancel_selection();
                        area.move_cursor(CursorMove::Jump(0, col as u16));
                        area.start_selection();
                    }
                    self.selection = None;
                    return TodoFormAction::Idle;
                }
                // Click missed every textarea; focus the surrounding field
                // (label / border / Status / Priority / Comments / Blockers)
                // without disturbing any cursor.
                if let Some(field) = self.field_at(m.row, m.column) {
                    if let Some(idx) = FIELDS.iter().position(|f| *f == field) {
                        self.focus = idx;
                        self.selection = None;
                    }
                }
                return TodoFormAction::Idle;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // Body drag extends the body's wrap-aware selection.
                if let Some(area) = body_area {
                    if let Some(pos) = self.body_logical_pos_at(area, m.row, m.column) {
                        if let Some((anchor, _)) = self.selection {
                            self.selection = Some((anchor, pos));
                            self.body
                                .move_cursor(CursorMove::Jump(pos.0 as u16, pos.1 as u16));
                            return TodoFormAction::Idle;
                        }
                    }
                }
                // Single-line textarea drag: extend the textarea's own
                // selection by feeding shift-Right / shift-Left chords until
                // the cursor reaches the target column. The textarea's
                // private `move_cursor_with_shift` isn't pub, but `input` is
                // the same code path and shift-arrow is its public face.
                let focused_field = FIELDS[self.focus];
                if let Some(inner) = self.text_field_inner(focused_field) {
                    let target = m.column.saturating_sub(inner.x) as usize;
                    if let Some(area) = self.single_line_textarea_mut(focused_field) {
                        select_to_column(area, target);
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                // Drag-release auto-copies the selection to the system
                // clipboard via OSC 52 (see `crate::clip`). The selection
                // stays painted so the user can see what got copied;
                // a zero-length selection (bare click, no drag) clears
                // instead. Body uses the wrap-aware `self.selection`;
                // single-line textareas (Title / Assignee / Tags) keep
                // their own `selection_range()`.
                if let Some((anchor, tip)) = self.selection {
                    if anchor == tip {
                        self.selection = None;
                    } else {
                        let text = selected_text(self.body.lines(), anchor, tip);
                        if !text.is_empty() {
                            let _ = crate::clip::copy_to_clipboard(&text);
                        }
                    }
                    return TodoFormAction::Idle;
                }
                let focused_field = FIELDS[self.focus];
                if let Some(area) = self.single_line_textarea_mut(focused_field) {
                    if let Some((anchor, tip)) = area.selection_range() {
                        if anchor != tip {
                            let text = selected_text(area.lines(), anchor, tip);
                            if !text.is_empty() {
                                let _ = crate::clip::copy_to_clipboard(&text);
                            }
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                self.body.move_cursor(CursorMove::Up);
            }
            MouseEventKind::ScrollDown => {
                self.body.move_cursor(CursorMove::Down);
            }
            _ => {}
        }
        TodoFormAction::Idle
    }

    /// Field whose recorded text-content rectangle contains the click, plus
    /// that rectangle. Returns `None` if the click missed every single-line
    /// textarea (body has its own click handling).
    fn text_field_at(&self, row: u16, col: u16) -> Option<(Field, Rect)> {
        self.text_field_areas.iter().find_map(|(field, rect)| {
            if row >= rect.y
                && row < rect.y + rect.height
                && col >= rect.x
                && col < rect.x + rect.width
            {
                Some((*field, *rect))
            } else {
                None
            }
        })
    }

    /// The text-content rect recorded for `field`, if it has one.
    fn text_field_inner(&self, field: Field) -> Option<Rect> {
        self.text_field_areas
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, r)| *r)
    }

    /// Mutable access to the single-line textarea backing `field`, if any.
    /// Body is intentionally not routed here - it uses its own selection
    /// model in `handle_mouse`'s body arm.
    fn single_line_textarea_mut(&mut self, field: Field) -> Option<&mut TextArea<'static>> {
        match field {
            Field::Title => Some(&mut self.title),
            Field::Assignee => Some(&mut self.assignee),
            Field::Tags => Some(&mut self.tags),
            _ => None,
        }
    }

    /// Field whose recorded rectangle contains terminal cell `(row, col)`,
    /// or `None` if the click missed every field. First match wins; rectangles
    /// for inline fields (Status next to Priority, Assignee next to Tags) do
    /// not overlap so order does not matter in practice.
    fn field_at(&self, row: u16, col: u16) -> Option<Field> {
        self.field_areas.iter().find_map(|(field, rect)| {
            if row >= rect.y
                && row < rect.y + rect.height
                && col >= rect.x
                && col < rect.x + rect.width
            {
                Some(*field)
            } else {
                None
            }
        })
    }

    /// Translate a terminal-cell position `(row, col)` inside `area` to a
    /// logical `(row, col)` in the body's source buffer, via the soft-wrap.
    /// Returns `None` when the click is outside the body content area; the
    /// border is treated as "outside".
    fn body_logical_pos_at(&self, area: Rect, row: u16, col: u16) -> Option<(usize, usize)> {
        if row < area.y || row >= area.y + area.height {
            return None;
        }
        if col < area.x || col >= area.x + area.width {
            return None;
        }
        let width = area.width as usize;
        let visual_row = (row - area.y) as usize + self.body_scroll;
        let visual_col = (col - area.x) as usize;
        let wrapped = crate::wrap::wrap_for_display(self.body.lines(), self.body.cursor(), width);
        Some(wrapped.visual_to_logical(visual_row, visual_col))
    }

    /// Insert a bracketed-paste payload into whichever field currently has
    /// focus. Forwards to the focused field's [`paste_into`] /
    /// [`paste_into_single_line`] / [`text_area`] as appropriate. Enum fields
    /// (status, priority) silently drop pastes; section fields (comments,
    /// blockers) route the paste into their input row.
    pub fn handle_paste(&mut self, s: &str) -> TodoFormAction {
        if s.is_empty() {
            return TodoFormAction::Idle;
        }
        // Scalar-field pastes count as a dirty edit (autosave should flush
        // them); section-row pastes only mutate the input draft, which the
        // user still has to commit with Enter or Ctrl-S.
        let (changed, scalar) = match FIELDS[self.focus] {
            Field::Title => (paste_into_single_line(&mut self.title, s), true),
            Field::Assignee => (paste_into_single_line(&mut self.assignee, s), true),
            Field::Tags => (paste_into_single_line(&mut self.tags, s), true),
            Field::Body => (paste_into(&mut self.body, s), true),
            Field::Status | Field::Priority => (false, false),
            Field::Comments => {
                // While an existing comment is being edited, the paste goes
                // there; otherwise it lands in the new-comment input row.
                let c = if let Some((_, area)) = self.editing_comment.as_mut() {
                    paste_into(area, s)
                } else {
                    self.comment_cursor = self.comments.len();
                    paste_into(&mut self.new_comment, s)
                };
                (c, false)
            }
            Field::Blockers => {
                self.blocker_cursor = self.blockers.len();
                (paste_into_single_line(&mut self.new_blocker, s), false)
            }
        };
        if changed && scalar {
            self.mark_dirty();
            TodoFormAction::Dirty
        } else {
            TodoFormAction::Idle
        }
    }

    /// Route a key to whatever field currently has focus.
    fn field_key(&mut self, key: KeyEvent) -> TodoFormAction {
        match FIELDS[self.focus] {
            Field::Status => {
                if let Some(dir) = cycle_dir(key.code) {
                    self.status = wrap(self.status, dir, STATUSES.len());
                    self.mark_dirty();
                    return TodoFormAction::Dirty;
                }
                TodoFormAction::Idle
            }
            Field::Priority => {
                if let Some(dir) = cycle_dir(key.code) {
                    self.priority = wrap(self.priority, dir, PRIORITIES.len());
                    self.mark_dirty();
                    return TodoFormAction::Dirty;
                }
                TodoFormAction::Idle
            }
            Field::Title => self.scalar_input_key(key, ScalarField::Title),
            Field::Assignee => self.scalar_input_key(key, ScalarField::Assignee),
            Field::Tags => self.scalar_input_key(key, ScalarField::Tags),
            Field::Body => self.scalar_input_key(key, ScalarField::Body),
            Field::Comments => self.comments_section_key(key),
            Field::Blockers => self.blockers_section_key(key),
        }
    }

    fn scalar_input_key(&mut self, key: KeyEvent, which: ScalarField) -> TodoFormAction {
        let changed = match which {
            ScalarField::Title => single_line_input(&mut self.title, key),
            ScalarField::Assignee => single_line_input(&mut self.assignee, key),
            ScalarField::Tags => single_line_input(&mut self.tags, key),
            ScalarField::Body => body_input(&mut self.body, key, self.body_view_height),
        };
        if changed {
            self.mark_dirty();
            TodoFormAction::Dirty
        } else {
            TodoFormAction::Idle
        }
    }

    /// Keys handled when the comments section has focus.
    ///
    /// The cursor row decides the bindings: while it's on an existing comment
    /// (read mode), `j`/`k`/Up/Down navigate, `Enter` starts in-place edit,
    /// and `d` deletes. While it's on the trailing input row (where the user
    /// is typing a new comment), every char including `j`/`k`/`d` goes into
    /// the textarea - the vim-ish navigation bindings only make sense over
    /// the read rows. `Up` from the input row still navigates out, since the
    /// input row is single-line; `Down` is a no-op. `Enter` submits the new
    /// comment.
    fn comments_section_key(&mut self, key: KeyEvent) -> TodoFormAction {
        // Rows in the comment section, top to bottom: each existing comment is
        // a row; the trailing input row is `comments.len()`.
        let total_rows = self.comments.len() + 1;
        let on_input_row = self.comment_cursor == self.comments.len();

        if on_input_row {
            return self.comments_input_row_key(key);
        }

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.comment_cursor > 0 {
                    self.comment_cursor -= 1;
                }
                TodoFormAction::Idle
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.comment_cursor + 1 < total_rows {
                    self.comment_cursor += 1;
                }
                TodoFormAction::Idle
            }
            KeyCode::Enter => {
                // Begin in-place edit of the highlighted comment.
                let existing = self.comments[self.comment_cursor].body.clone();
                self.editing_comment = Some((self.comment_cursor, text_area(&existing)));
                TodoFormAction::Idle
            }
            KeyCode::Char('d') => {
                let cid = self.comments[self.comment_cursor].id;
                match self.delete_comment(cid) {
                    Ok(()) => self.message = format!("deleted comment #{cid}"),
                    Err(e) => self.message = format!("delete failed: {e:#}"),
                }
                TodoFormAction::Idle
            }
            _ => TodoFormAction::Idle,
        }
    }

    /// Keys for the new-comment input row. All printable chars go into the
    /// textarea; `Enter` commits; `Up` navigates back into the read rows.
    fn comments_input_row_key(&mut self, key: KeyEvent) -> TodoFormAction {
        match key.code {
            KeyCode::Up => {
                if self.comment_cursor > 0 {
                    self.comment_cursor -= 1;
                }
                TodoFormAction::Idle
            }
            // `Down` from the input row is a no-op - there is nowhere further.
            KeyCode::Down => TodoFormAction::Idle,
            KeyCode::Enter => {
                let body = self.new_comment.lines().join("\n");
                if body.trim().is_empty() {
                    return TodoFormAction::Idle;
                }
                match self.append_comment(&body) {
                    Ok(()) => {
                        self.new_comment = text_area("");
                        self.message = "comment added".to_string();
                        self.comment_cursor = self.comments.len();
                    }
                    Err(e) => self.message = format!("comment failed: {e:#}"),
                }
                TodoFormAction::Idle
            }
            _ => {
                let _ = text_input(&mut self.new_comment, key);
                TodoFormAction::Idle
            }
        }
    }

    /// While editing a comment: Ctrl-S commits, Esc cancels, everything else
    /// flows into the in-progress TextArea.
    fn handle_comment_edit_key(&mut self, key: KeyEvent) -> TodoFormAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('s') if ctrl => {
                if let Some((idx, area)) = self.editing_comment.take() {
                    let body = area.lines().join("\n");
                    let cid = self.comments[idx].id;
                    match self.update_comment(cid, &body) {
                        Ok(()) => self.message = format!("updated comment #{cid}"),
                        Err(e) => {
                            // Put the editor back so the user does not lose work.
                            self.editing_comment = Some((idx, text_area(&body)));
                            self.message = format!("update failed: {e:#}");
                        }
                    }
                }
                TodoFormAction::Idle
            }
            KeyCode::Esc => {
                self.editing_comment = None;
                self.message = "edit canceled".to_string();
                TodoFormAction::Idle
            }
            _ => {
                if let Some((_, area)) = self.editing_comment.as_mut() {
                    text_input(area, key);
                }
                TodoFormAction::Idle
            }
        }
    }

    /// Keys handled when the Blockers row has focus. The row is a horizontal
    /// strip of chips followed by a `+ id:` input; navigation cycles left and
    /// right between them. On a chip, `d` removes that blocker. The input row
    /// routes printable keys to the textarea so a number like `12` can be
    /// typed without the chip-navigation bindings hijacking the keystrokes.
    fn blockers_section_key(&mut self, key: KeyEvent) -> TodoFormAction {
        let on_input_row = self.blocker_cursor == self.blockers.len();

        if on_input_row {
            return self.blockers_input_row_key(key);
        }

        match key.code {
            KeyCode::Left => {
                if self.blocker_cursor > 0 {
                    self.blocker_cursor -= 1;
                }
                TodoFormAction::Idle
            }
            KeyCode::Right => {
                // Right past the last chip lands on the input row.
                if self.blocker_cursor < self.blockers.len() {
                    self.blocker_cursor += 1;
                }
                TodoFormAction::Idle
            }
            KeyCode::Char('d') => {
                let bid = self.blockers[self.blocker_cursor].id;
                match self.remove_blocker(bid) {
                    Ok(()) => self.message = format!("removed blocker #{bid}"),
                    Err(e) => self.message = format!("remove failed: {e:#}"),
                }
                TodoFormAction::Idle
            }
            _ => TodoFormAction::Idle,
        }
    }

    /// Keys for the new-blocker input. `Enter` parses the typed id and adds
    /// the blocker; `Backspace` on an empty input removes the rightmost chip
    /// (the Gmail-recipients pattern); `Left` walks back into the chip strip;
    /// every other key types into the textarea.
    fn blockers_input_row_key(&mut self, key: KeyEvent) -> TodoFormAction {
        let input_empty = self.new_blocker.lines().join("").is_empty();
        match key.code {
            KeyCode::Left => {
                if self.blocker_cursor > 0 {
                    self.blocker_cursor -= 1;
                }
                TodoFormAction::Idle
            }
            KeyCode::Right => TodoFormAction::Idle,
            KeyCode::Backspace if input_empty && !self.blockers.is_empty() => {
                // Pop the rightmost chip. `remove_blocker` clamps the cursor
                // back into range when the list shrinks, so we land on the
                // (new) input row again ready for the next id.
                let bid = self.blockers[self.blockers.len() - 1].id;
                match self.remove_blocker(bid) {
                    Ok(()) => self.message = format!("removed blocker #{bid}"),
                    Err(e) => self.message = format!("remove failed: {e:#}"),
                }
                TodoFormAction::Idle
            }
            KeyCode::Enter => {
                let typed = self.new_blocker.lines().join("");
                let trimmed = typed.trim_start_matches('#').trim();
                let Some(id) = trimmed.parse::<u64>().ok() else {
                    self.message = format!("blocker id '{trimmed}' is not a number");
                    return TodoFormAction::Idle;
                };
                match self.add_blocker(id) {
                    Ok(()) => {
                        self.new_blocker = text_area("");
                        self.message = format!("added blocker #{id}");
                        self.blocker_cursor = self.blockers.len();
                    }
                    Err(e) => self.message = format!("add blocker failed: {e:#}"),
                }
                TodoFormAction::Idle
            }
            _ => {
                let _ = text_input(&mut self.new_blocker, key);
                TodoFormAction::Idle
            }
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        if self.dirty_since.is_none() {
            self.dirty_since = Some(Instant::now());
        }
        self.can_quit = false;
    }

    /// Whether the title is empty; saving is suppressed while it is.
    pub fn title_is_empty(&self) -> bool {
        self.title.lines().join(" ").trim().is_empty()
    }

    /// Push the scalar fields the user actually changed back to the daemon.
    /// Used by both the CLI's Ctrl-S handler and the viewer's debounced
    /// autosave.
    ///
    /// Creates the todo first when this is a new form (no id yet) and the
    /// title is non-empty. Only fields that differ from [`Self::baseline`]
    /// are sent in the follow-up `todo_update`; untouched fields are
    /// omitted, which the daemon treats as "leave unchanged." This is the
    /// guard against an idle form clobbering a concurrent write (a CLI
    /// `todo_complete`, an agent's MCP edit, the same todo open in a
    /// second pane).
    pub fn flush(&mut self) -> Result<()> {
        let title = self.title.lines().join(" ").trim().to_string();
        // Suppress only the new-form case: `todo_create` needs a non-empty
        // title, and an autosave on a blank-from-the-start form would surface
        // that as a spurious error in the message line. Once the todo exists,
        // the user is allowed to clear its title - the update path accepts an
        // empty string. Without this distinction the last non-empty mid-
        // delete state ("a" while removing "abc") would be the value the
        // daemon and the sidebar see, and the next refresh would replay that
        // orphan character back into the title field. Mirrors the
        // NoteForm fix for todo #75.
        if self.id.is_none() && title.is_empty() {
            return Ok(());
        }
        let body = self.body.lines().join("\n");
        let assignee = self.assignee.lines().join(" ").trim().to_string();
        let tags: Vec<String> = self
            .tags
            .lines()
            .join(",")
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(String::from)
            .collect();
        let status = STATUSES[self.status].to_string();
        let priority = PRIORITIES[self.priority].to_string();

        let client = Client::connect(&self.url)?;
        let outcome = (|| -> Result<()> {
            let id = match self.id {
                Some(id) => id,
                None => {
                    let created = client.call("todo_create", json!({ "title": title }))?;
                    let id = created
                        .as_u64()
                        .ok_or_else(|| anyhow!("daemon returned no todo id"))?;
                    self.id = Some(id);
                    // `todo_create` accepts only `title`; the daemon initialises
                    // every other scalar to its default. Record that in the
                    // baseline so the diff below sends whichever non-defaults
                    // the user typed in the new-todo form.
                    self.baseline.title = title.clone();
                    id
                }
            };
            let current = Baseline {
                title: title.clone(),
                body: body.clone(),
                status: status.clone(),
                priority: priority.clone(),
                assignee: assignee.clone(),
                tags: tags.clone(),
            };
            let payload = build_update_payload(id, &self.baseline, &current);
            // Skip the round-trip when nothing changed - this is the common
            // shape of a debounced autosave fired by an unrelated edit
            // (e.g. typing in the comment-input row).
            if payload.len() > 1 {
                client.call("todo_update", Value::Object(payload))?;
            }
            Ok(())
        })();
        client.close();
        outcome?;
        // Refresh the baseline so future flushes diff against what the daemon
        // now holds, not what was loaded an edit ago.
        self.baseline = Baseline {
            title,
            body,
            status,
            priority,
            assignee,
            tags,
        };
        self.dirty = false;
        self.dirty_since = None;
        self.can_quit = true;
        self.message = format!("saved todo #{}", self.id.unwrap_or(0));
        Ok(())
    }

    /// Pull the daemon's current snapshot of this todo and replay it onto the
    /// form. Per #40: untouched scalar fields are replaced with the remote
    /// value; locally edited fields keep their text and the message line
    /// flags the conflict so the user knows their save will win. The
    /// [`Baseline`] is always advanced to the remote value, which is what
    /// makes [`Self::flush`]'s diff send only the user's pending changes
    /// after a refresh.
    ///
    /// Returns `Ok(true)` when anything visible changed and the host should
    /// redraw. New-todo forms (no id yet) return `Ok(false)` immediately -
    /// there is no daemon row to refresh against.
    pub fn refresh_from_daemon(&mut self) -> Result<bool> {
        let Some(id) = self.id else {
            return Ok(false);
        };
        let client = Client::connect(&self.url)?;
        let outcome: Result<(Value, Vec<BlockerEntry>)> = (|| {
            let todo = client.call("todo_get", json!({ "todo_id": id }))?;
            // Resolve blocker titles inline; we hold one MCP connection
            // across the lookups so an N+1 fan-out still costs one session.
            let blocker_ids: Vec<u64> = todo["blockers"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default();
            let mut entries = Vec::with_capacity(blocker_ids.len());
            for bid in blocker_ids {
                let title = client
                    .call("todo_get", json!({ "todo_id": bid }))
                    .ok()
                    .and_then(|v| v["title"].as_str().map(str::to_string))
                    .unwrap_or_default();
                entries.push(BlockerEntry { id: bid, title });
            }
            Ok((todo, entries))
        })();
        client.close();
        let (todo, remote_blockers) = outcome?;

        let remote = Baseline {
            title: todo["title"].as_str().unwrap_or("").to_string(),
            body: todo["body"].as_str().unwrap_or("").to_string(),
            status: todo["status"].as_str().unwrap_or("open").to_string(),
            priority: todo["priority"].as_str().unwrap_or("medium").to_string(),
            assignee: todo["assignee"].as_str().unwrap_or("").to_string(),
            tags: string_list(&todo["tags"]),
        };
        let remote_comments: Vec<CommentEntry> = todo["comments"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|c| {
                        Some(CommentEntry {
                            id: c["id"].as_u64()?,
                            author: c["author"].as_str().unwrap_or("").to_string(),
                            created_at: c["created_at"].as_str().unwrap_or("").to_string(),
                            body: c["body"].as_str().unwrap_or("").to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let remote_updated = todo["updated_at"].as_str().unwrap_or("").to_string();
        Ok(self.replay_remote(remote, remote_comments, remote_blockers, remote_updated))
    }

    /// Apply a daemon snapshot to the form. Pure of MCP - takes the already-
    /// loaded values directly - so the replay rules can be unit-tested. See
    /// [`Self::refresh_from_daemon`] for the surrounding wire-up.
    fn replay_remote(
        &mut self,
        remote: Baseline,
        remote_comments: Vec<CommentEntry>,
        remote_blockers: Vec<BlockerEntry>,
        remote_updated: String,
    ) -> bool {
        let mut changed = false;
        let mut conflicts: Vec<&'static str> = Vec::new();

        if remote.title != self.baseline.title {
            if self.current_title() != self.baseline.title {
                conflicts.push("title");
            } else {
                self.title = text_area(&remote.title);
                changed = true;
            }
        }
        if remote.body != self.baseline.body {
            if self.current_body() != self.baseline.body {
                conflicts.push("body");
            } else {
                self.body = text_area(&remote.body);
                changed = true;
            }
        }
        if remote.status != self.baseline.status {
            if STATUSES[self.status] != self.baseline.status {
                conflicts.push("status");
            } else {
                self.status = index_of(&STATUSES, &remote.status);
                changed = true;
            }
        }
        if remote.priority != self.baseline.priority {
            if PRIORITIES[self.priority] != self.baseline.priority {
                conflicts.push("priority");
            } else {
                self.priority = index_of(&PRIORITIES, &remote.priority);
                changed = true;
            }
        }
        if remote.assignee != self.baseline.assignee {
            if self.current_assignee() != self.baseline.assignee {
                conflicts.push("assignee");
            } else {
                self.assignee = text_area(&remote.assignee);
                changed = true;
            }
        }
        if remote.tags != self.baseline.tags {
            if self.current_tags() != self.baseline.tags {
                conflicts.push("tags");
            } else {
                self.tags = text_area(&remote.tags.join(", "));
                changed = true;
            }
        }

        // Comments: the daemon owns the list; the form's only local state
        // is whichever comment is being edited in place. While an edit is
        // open we skip the blanket replace (re-indexing under the editor
        // would yank the edit out from under the user).
        if self.editing_comment.is_none() && !comments_match(&self.comments, &remote_comments) {
            self.comments = remote_comments;
            if self.comment_cursor > self.comments.len() {
                self.comment_cursor = self.comments.len();
            }
            changed = true;
        }

        if !blockers_match(&self.blockers, &remote_blockers) {
            self.blockers = remote_blockers;
            if self.blocker_cursor > self.blockers.len() {
                self.blocker_cursor = self.blockers.len();
            }
            changed = true;
        }

        self.updated = remote_updated;
        // Advance the baseline unconditionally so a subsequent flush only
        // sends fields the user is still mid-edit on (the "replay" half of
        // #40's plan).
        self.baseline = remote;

        if !conflicts.is_empty() {
            self.message = format!(
                "remote changed {} - your save will overwrite",
                conflicts.join(", ")
            );
            changed = true;
        }

        changed
    }

    fn current_title(&self) -> String {
        self.title.lines().join(" ").trim().to_string()
    }

    fn current_body(&self) -> String {
        self.body.lines().join("\n")
    }

    fn current_assignee(&self) -> String {
        self.assignee.lines().join(" ").trim().to_string()
    }

    fn current_tags(&self) -> Vec<String> {
        self.tags
            .lines()
            .join(",")
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(String::from)
            .collect()
    }

    fn append_comment(&mut self, body: &str) -> Result<()> {
        let id = self.id.ok_or_else(|| anyhow!("save the todo first"))?;
        let client = Client::connect(&self.url)?;
        let outcome = client.call(
            "todo_comment_add",
            json!({ "todo_id": id, "body": body, "author": "cockpit" }),
        );
        client.close();
        let res = outcome?;
        let cid = res.as_u64().unwrap_or(0);
        self.comments.push(CommentEntry {
            id: cid,
            author: "cockpit".to_string(),
            created_at: String::new(),
            body: body.to_string(),
        });
        Ok(())
    }

    fn update_comment(&mut self, comment_id: u64, body: &str) -> Result<()> {
        let todo_id = self.id.ok_or_else(|| anyhow!("save the todo first"))?;
        let client = Client::connect(&self.url)?;
        let outcome = client.call(
            "todo_comment_update",
            json!({ "todo_id": todo_id, "comment_id": comment_id, "body": body }),
        );
        client.close();
        outcome?;
        if let Some(c) = self.comments.iter_mut().find(|c| c.id == comment_id) {
            c.body = body.to_string();
        }
        Ok(())
    }

    fn delete_comment(&mut self, comment_id: u64) -> Result<()> {
        let todo_id = self.id.ok_or_else(|| anyhow!("save the todo first"))?;
        let client = Client::connect(&self.url)?;
        let outcome = client.call(
            "todo_comment_delete",
            json!({ "todo_id": todo_id, "comment_id": comment_id }),
        );
        client.close();
        outcome?;
        self.comments.retain(|c| c.id != comment_id);
        if self.comment_cursor >= self.comments.len() {
            self.comment_cursor = self.comments.len();
        }
        Ok(())
    }

    fn add_blocker(&mut self, blocker_id: u64) -> Result<()> {
        let todo_id = self.id.ok_or_else(|| anyhow!("save the todo first"))?;
        if blocker_id == todo_id {
            return Err(anyhow!("a todo cannot block itself"));
        }
        let client = Client::connect(&self.url)?;
        let outcome = client.call(
            "todo_add_blocker",
            json!({ "todo_id": todo_id, "blocker_id": blocker_id }),
        );
        // Resolve the blocker's title for display while the client is still open.
        let title = match client.call("todo_get", json!({ "todo_id": blocker_id })) {
            Ok(v) => v["title"].as_str().unwrap_or("").to_string(),
            Err(_) => String::new(),
        };
        client.close();
        outcome?;
        if !self.blockers.iter().any(|b| b.id == blocker_id) {
            self.blockers.push(BlockerEntry {
                id: blocker_id,
                title,
            });
        }
        Ok(())
    }

    fn remove_blocker(&mut self, blocker_id: u64) -> Result<()> {
        let todo_id = self.id.ok_or_else(|| anyhow!("save the todo first"))?;
        let client = Client::connect(&self.url)?;
        let outcome = client.call(
            "todo_remove_blocker",
            json!({ "todo_id": todo_id, "blocker_id": blocker_id }),
        );
        client.close();
        outcome?;
        self.blockers.retain(|b| b.id != blocker_id);
        if self.blocker_cursor >= self.blockers.len() {
            self.blocker_cursor = self.blockers.len();
        }
        Ok(())
    }

    /// The standalone CLI's explicit-save key. Equivalent to a `flush`, but
    /// also surfaces the empty-title rejection as a user message rather than
    /// a silent no-op (the autosave path treats empty title as not yet saved).
    pub fn save_explicit(&mut self) {
        if self.title_is_empty() {
            self.message = "title cannot be empty".to_string();
            return;
        }
        if let Err(e) = self.flush() {
            self.message = format!("save failed: {e:#}");
        }
    }

    /// Render the form into `area`. The host is responsible for clearing the
    /// rect first if it needs to.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        // Rows: title / status+priority / assignee+tags / body / comments /
        // blockers / context / message. Body is 3x the size of comments.
        // Blockers is a single-line chip strip; Status, Priority, Assignee,
        // and Tags are also single-line inline fields - together they free
        // six rows of vertical space for the body and comments blocks. The
        // pane title (set by the Zellij sidebar) already names the todo, so
        // no in-form header row is needed; locked-by, when set, surfaces in
        // the message line at the bottom.
        let rows = Layout::vertical([
            Constraint::Length(3),   // title
            Constraint::Length(1),   // status + priority
            Constraint::Length(1),   // assignee + tags
            Constraint::Ratio(3, 4), // body (3/4 of flexible space)
            Constraint::Ratio(1, 4), // comments (1/4 of flexible space)
            Constraint::Length(1),   // blockers chip strip
            Constraint::Length(1),   // context (created/updated)
            Constraint::Length(1),   // message + help
        ])
        .split(area);

        self.field_areas.clear();
        self.text_field_areas.clear();

        self.style_field(Field::Title, "Title");
        frame.render_widget(&self.title, rows[0]);
        self.field_areas.push((Field::Title, rows[0]));
        // Title is bordered (set by `style_field`); its text content sits
        // inside that border.
        self.text_field_areas
            .push((Field::Title, Block::bordered().inner(rows[0])));

        let focus = FIELDS[self.focus];
        let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[1]);
        frame.render_widget(
            enum_line("Status", STATUSES[self.status], focus == Field::Status),
            cols[0],
        );
        self.field_areas.push((Field::Status, cols[0]));
        frame.render_widget(
            enum_line(
                "Priority",
                PRIORITIES[self.priority],
                focus == Field::Priority,
            ),
            cols[1],
        );
        self.field_areas.push((Field::Priority, cols[1]));

        self.style_inline_field(Field::Assignee);
        self.style_inline_field(Field::Tags);
        let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[2]);
        let assignee_cols =
            Layout::horizontal([Constraint::Length(11), Constraint::Min(1)]).split(cols[0]);
        frame.render_widget(
            Paragraph::new(" Assignee: ").style(label_style(focus == Field::Assignee)),
            assignee_cols[0],
        );
        frame.render_widget(&self.assignee, assignee_cols[1]);
        // The clickable hitbox for Assignee includes the label so a click
        // anywhere on the row picks the field; same for Tags. The
        // text-content rect is the narrower textarea cell, used for click
        // positioning and drag-to-select.
        self.field_areas.push((Field::Assignee, cols[0]));
        self.text_field_areas
            .push((Field::Assignee, assignee_cols[1]));
        let tags_cols =
            Layout::horizontal([Constraint::Length(7), Constraint::Min(1)]).split(cols[1]);
        frame.render_widget(
            Paragraph::new(" Tags: ").style(label_style(focus == Field::Tags)),
            tags_cols[0],
        );
        frame.render_widget(&self.tags, tags_cols[1]);
        self.field_areas.push((Field::Tags, cols[1]));
        self.text_field_areas.push((Field::Tags, tags_cols[1]));

        self.draw_body(frame, rows[3]);

        self.draw_comments(frame, rows[4]);
        self.field_areas.push((Field::Comments, rows[4]));
        self.draw_blockers(frame, rows[5]);
        self.field_areas.push((Field::Blockers, rows[5]));

        let context = if !self.created.is_empty() {
            format!(" created {}   updated {}", self.created, self.updated)
        } else {
            String::new()
        };
        frame.render_widget(
            Paragraph::new(context).style(Style::default().fg(Color::DarkGray)),
            rows[6],
        );

        let help = "Tab field  Left/Right cycle  Enter add/edit  d delete  Ctrl-C close";
        // When focus is on a blocker chip, prefer showing that blocker's title
        // over `self.message` - chips render as ID-only to save horizontal
        // space, so the title needs a surface somewhere the user can always
        // see it.
        let displayed_message =
            if focus == Field::Blockers && self.blocker_cursor < self.blockers.len() {
                let b = &self.blockers[self.blocker_cursor];
                if b.title.is_empty() {
                    format!("#{}", b.id)
                } else {
                    format!("#{} {}", b.id, b.title)
                }
            } else {
                self.message.clone()
            };
        // The removed in-form header used to carry the locked-by banner; with
        // the pane title now naming the todo, fold any lock indicator into
        // the message line so a held lock still has a visible surface.
        let lock_prefix = self
            .locked_by
            .as_deref()
            .map(|h| format!("[locked by {h}]   "))
            .unwrap_or_default();
        let line = if displayed_message.is_empty() {
            format!(" {lock_prefix}{help}")
        } else {
            format!(" {lock_prefix}{displayed_message}   |   {help}")
        };
        let line_style = if self.locked_by.is_some() {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Yellow)
        };
        frame.render_widget(Paragraph::new(line).style(line_style), rows[7]);
    }

    /// Render the Body field as a soft-wrapped paragraph plus an overlay
    /// cursor. We bypass [`tui_textarea::TextArea`]'s own draw because that
    /// widget has no soft-wrap - it scrolls horizontally instead, so a pasted
    /// line longer than the field reads as "my text vanished off the right
    /// edge." The buffer and edit logic still live in the textarea; only the
    /// visual representation is ours.
    fn draw_body(&mut self, frame: &mut Frame, area: Rect) {
        let focused = FIELDS[self.focus] == Field::Body;
        let border = field_border_color(focused);
        let block = Block::bordered()
            .title("Body")
            .border_style(Style::default().fg(border));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let width = inner.width as usize;
        let height = inner.height as usize;
        // Remember the height so Ctrl-U / Ctrl-D in `body_input` can page by
        // half-screen instead of by a fixed step. Stored every draw so resize
        // is picked up automatically on the next key.
        self.body_view_height = height;
        // Remember the body rectangle so `handle_mouse` can decide whether a
        // click hit the body field and, if so, where in it.
        self.body_area = Some(inner);
        if width == 0 || height == 0 {
            return;
        }

        let cursor = self.body.cursor();
        let wrapped = crate::wrap::wrap_for_display(self.body.lines(), cursor, width);
        let (cvr, cvc) = wrapped.cursor;

        // Keep the cursor in view by adjusting the scroll offset.
        if cvr < self.body_scroll {
            self.body_scroll = cvr;
        } else if cvr >= self.body_scroll + height {
            self.body_scroll = cvr + 1 - height;
        }
        // Don't scroll past the last visual row.
        let max_scroll = wrapped.lines.len().saturating_sub(height);
        if self.body_scroll > max_scroll {
            self.body_scroll = max_scroll;
        }

        let selection_ranges: Vec<(usize, usize, usize)> = match self.selection {
            Some((a, t)) => wrapped.visual_selection_ranges(a, t),
            None => Vec::new(),
        };
        let visible: Vec<Line> = wrapped
            .lines
            .iter()
            .enumerate()
            .skip(self.body_scroll)
            .take(height)
            .map(|(vrow, l)| {
                if let Some(&(_, from, to)) = selection_ranges.iter().find(|(r, _, _)| *r == vrow) {
                    highlight_line(l, from, to)
                } else {
                    Line::from(l.clone())
                }
            })
            .collect();
        frame.render_widget(Paragraph::new(visible), inner);

        if focused && cvr >= self.body_scroll && cvr < self.body_scroll + height && cvc < width {
            let cy = inner.y + (cvr - self.body_scroll) as u16;
            let cx = inner.x + cvc as u16;
            frame.set_cursor_position((cx, cy));
        }
    }

    fn draw_comments(&mut self, frame: &mut Frame, area: Rect) {
        let focused = FIELDS[self.focus] == Field::Comments;
        let border = field_border_color(focused);
        let block = Block::bordered()
            .title(format!("Comments ({})", self.comments.len()))
            .border_style(Style::default().fg(border));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Split the inner area: existing comments (Min(1)) + new-comment row (Length(1)).
        let inner_rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

        if let Some((idx, area)) = self.editing_comment.as_ref() {
            let mut lines: Vec<Line> = Vec::new();
            for (i, c) in self.comments.iter().enumerate() {
                if i == *idx {
                    lines.push(Line::styled(
                        format!(
                            " #{} {} - editing (Ctrl-S save, Esc cancel)",
                            c.id, c.author
                        ),
                        Style::default().fg(Color::Yellow),
                    ));
                } else {
                    lines.push(Line::from(format!(
                        " #{} {} {} - {}",
                        c.id, c.author, c.created_at, c.body
                    )));
                }
            }
            frame.render_widget(Paragraph::new(lines), inner_rows[0]);
            // The textarea itself sits on the input row while editing.
            frame.render_widget(area, inner_rows[1]);
            return;
        }

        // Read-mode comments list, with the cursor row reversed.
        let lines: Vec<Line> = self
            .comments
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let label = format!(" #{} {} {} - {}", c.id, c.author, c.created_at, c.body);
                if focused && i == self.comment_cursor {
                    Line::styled(label, Style::default().add_modifier(Modifier::REVERSED))
                } else {
                    Line::from(label)
                }
            })
            .collect();
        let body = if lines.is_empty() {
            Paragraph::new(" (no comments)").style(Style::default().fg(Color::DarkGray))
        } else {
            Paragraph::new(lines)
        };
        frame.render_widget(body, inner_rows[0]);

        // The add-row: a `+ ` prefix plus the textarea so it reads as an input.
        let add_focused = focused && self.comment_cursor == self.comments.len();
        let prefix_style = if add_focused {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let prefix_cols =
            Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).split(inner_rows[1]);
        frame.render_widget(Paragraph::new("+ ").style(prefix_style), prefix_cols[0]);
        frame.render_widget(&self.new_comment, prefix_cols[1]);
    }

    /// Render the Blockers row as `Blockers: #3 #7 #12   + id: ___` on one
    /// line. The focused chip's title rides in the form's bottom message line
    /// (computed in [`Self::draw`]), so chips here stay ID-only to keep the
    /// strip narrow. Long strips scroll horizontally; the scroll always lands
    /// on a chip boundary so the leading chip never appears half-clipped.
    fn draw_blockers(&mut self, frame: &mut Frame, area: Rect) {
        let focused = FIELDS[self.focus] == Field::Blockers;
        let on_input_row = self.blocker_cursor == self.blockers.len();

        // Three columns: label / chip strip / input. The input width is fixed
        // so the chip strip always knows how much room it has to scroll into.
        let cols = Layout::horizontal([
            Constraint::Length(11),
            Constraint::Min(0),
            Constraint::Length(16),
        ])
        .split(area);

        frame.render_widget(
            Paragraph::new(" Blockers: ").style(label_style(focused)),
            cols[0],
        );

        self.draw_blocker_chips(frame, cols[1], focused);

        let input_focused = focused && on_input_row;
        let prefix_style = if input_focused {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let input_cols =
            Layout::horizontal([Constraint::Length(7), Constraint::Min(1)]).split(cols[2]);
        frame.render_widget(Paragraph::new(" + id: ").style(prefix_style), input_cols[0]);
        self.new_blocker.set_cursor_style(if input_focused {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        });
        frame.render_widget(&self.new_blocker, input_cols[1]);
    }

    /// Render the chip strip into `area`, scrolling so the focused chip stays
    /// fully visible. Chips are separated by a single space; the scroll offset
    /// is held in `self.blocker_scroll` and aligned to a chip's leading edge.
    fn draw_blocker_chips(&mut self, frame: &mut Frame, area: Rect, section_focused: bool) {
        let width = area.width as usize;
        if width == 0 || self.blockers.is_empty() {
            // Reset scroll when there is nothing to display - otherwise a
            // stale offset survives across deletions and the next render
            // starts mid-chip.
            self.blocker_scroll = 0;
            return;
        }

        // Compute per-chip widths (incl. the leading separator space for all
        // but the first chip) and the cumulative offsets used both to choose
        // the scroll position and to drive `Paragraph::scroll`.
        let mut chip_widths: Vec<usize> = Vec::with_capacity(self.blockers.len());
        for (i, b) in self.blockers.iter().enumerate() {
            let sep = usize::from(i > 0);
            chip_widths.push(sep + format!("#{}", b.id).len());
        }
        let mut offsets: Vec<usize> = Vec::with_capacity(self.blockers.len() + 1);
        offsets.push(0);
        for w in &chip_widths {
            offsets.push(offsets.last().unwrap() + w);
        }

        // Slide the strip so the focused chip lands inside the visible window
        // by anchoring it to the leftmost column - same offset whether the
        // focus came from the right (chip would be cut off on the right) or
        // from the left (chip would be cut off on the left), which keeps the
        // leading chip from ever appearing half-clipped. When the input row
        // is focused we leave scroll where it last sat so returning to a
        // chip does not jump the view.
        if section_focused && self.blocker_cursor < self.blockers.len() {
            let start = offsets[self.blocker_cursor];
            let end = offsets[self.blocker_cursor + 1];
            if start < self.blocker_scroll || end > self.blocker_scroll + width {
                self.blocker_scroll = start;
            }
        }
        // Don't scroll past the end of the strip - if the user just removed
        // the rightmost blocker, the previous scroll value may be too large.
        let total = *offsets.last().unwrap();
        let max_scroll = total.saturating_sub(width);
        if self.blocker_scroll > max_scroll {
            self.blocker_scroll = max_scroll;
        }

        let mut spans: Vec<Span<'static>> = Vec::new();
        for (i, b) in self.blockers.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" "));
            }
            let style = if section_focused && i == self.blocker_cursor {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else if section_focused {
                Style::default().fg(Color::Gray)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            spans.push(Span::styled(format!("#{}", b.id), style));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).scroll((0, self.blocker_scroll as u16)),
            area,
        );
    }

    /// Set a bordered text field's border and cursor styling for the current
    /// focus. Used for the Title field; Body has its own wrapped render in
    /// [`Self::draw_body`]; Assignee and Tags are inline single-line fields
    /// styled by [`Self::style_inline_field`].
    fn style_field(&mut self, field: Field, label: &'static str) {
        let focused = FIELDS[self.focus] == field;
        let area = match field {
            Field::Title => &mut self.title,
            Field::Status
            | Field::Priority
            | Field::Assignee
            | Field::Tags
            | Field::Body
            | Field::Comments
            | Field::Blockers => return,
        };
        let border = field_border_color(focused);
        area.set_block(
            Block::bordered()
                .title(label)
                .border_style(Style::default().fg(border)),
        );
        area.set_cursor_style(if focused {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        });
    }

    /// Cursor styling for an inline single-line field (Assignee, Tags). These
    /// render without a block; their label sits to the left and the cursor
    /// itself indicates focus.
    fn style_inline_field(&mut self, field: Field) {
        let focused = FIELDS[self.focus] == field;
        let area = match field {
            Field::Assignee => &mut self.assignee,
            Field::Tags => &mut self.tags,
            _ => return,
        };
        area.set_cursor_style(if focused {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        });
    }
}

/// Internal tag for routing single-line vs multiline scalar input.
enum ScalarField {
    Title,
    Assignee,
    Tags,
    Body,
}

/// A text area carrying `initial`, with the cursor-line highlight disabled so
/// it reads as a plain field.
pub(crate) fn text_area(initial: &str) -> TextArea<'static> {
    let mut area = if initial.is_empty() {
        TextArea::default()
    } else {
        TextArea::new(initial.split('\n').map(String::from).collect())
    };
    area.set_cursor_line_style(Style::default());
    area
}

/// Render a cyclable enum field as a one-line `Label: < value >`.
fn enum_line(label: &str, value: &str, focused: bool) -> Paragraph<'static> {
    Paragraph::new(format!(" {label}: < {value} >")).style(label_style(focused))
}

/// Border color for a bordered form field, keyed on whether it has focus.
/// Shared by both the todo and note forms (Title, Body, Comments, Tags).
/// The focused field takes the accent; every unfocused border drops to a dim
/// `#444444` so the focused field stands alone and the rest recede. An absolute
/// RGB, rather than `Color::DarkGray`, keeps the de-emphasis from being undone
/// by a terminal palette that renders the gray ANSI slot bright.
pub(crate) fn field_border_color(focused: bool) -> Color {
    if focused {
        Color::Yellow
    } else {
        Color::Rgb(0x44, 0x44, 0x44)
    }
}

/// Style for an inline field's leading label. Matches `enum_line`'s palette so
/// Status, Priority, Assignee, and Tags read as one band of inline fields.
fn label_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    }
}

/// Feed a key to a multi-line text field. We forward almost everything
/// straight to [`tui_textarea::TextArea::input`] so its full set of
/// emacs-style shortcuts works (Ctrl-A start of line, Ctrl-E end, Ctrl-K kill
/// to end, Ctrl-W delete word, Ctrl-U undo, Ctrl-R redo, Ctrl-D delete next,
/// arrow keys, etc.). Two narrow overrides:
///
/// - `Ctrl-J` and `Ctrl-M` insert a newline. In raw mode crossterm parses the
///   bare bytes `\n` and `\r` as these chords, and `tui_textarea` would
///   otherwise read `Ctrl-J` as `delete_line_by_head`. The keystrokes are not
///   useful on their own, so treating them as Enter loses nothing and gives
///   us a defensive fallback if a paste ever bypasses bracketed-paste mode.
/// - `Ctrl-Z` runs undo. `tui_textarea` already binds undo to `Ctrl-U`, but
///   `Ctrl-Z` is the muscle memory most users have for it.
///
/// Returns whether the field's content changed.
pub(crate) fn text_input(area: &mut TextArea, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('j') | KeyCode::Char('m') if ctrl => {
            area.insert_newline();
            true
        }
        KeyCode::Char('z') if ctrl => area.undo(),
        _ => area.input(key),
    }
}

/// Extract the text under a logical selection `(anchor, tip)` from `lines`.
/// Each endpoint is `(row, char_col)`; the endpoints may arrive in either
/// order (a backward drag is just as valid as a forward one). Out-of-range
/// columns clamp to the line's char count - a click past the right edge of
/// a wrapped line still produces a sensible cut.
///
/// Multi-line selections join with `\n`, matching what a user would type to
/// recreate the buffer. This is what we hand to OSC 52 so paste-back
/// elsewhere preserves line breaks.
pub(crate) fn selected_text(
    lines: &[String],
    anchor: (usize, usize),
    tip: (usize, usize),
) -> String {
    let (start, end) = if anchor <= tip {
        (anchor, tip)
    } else {
        (tip, anchor)
    };
    if start.0 == end.0 {
        let line = lines.get(start.0).map(String::as_str).unwrap_or("");
        let chars: Vec<char> = line.chars().collect();
        let s = start.1.min(chars.len());
        let e = end.1.min(chars.len());
        return chars[s..e].iter().collect();
    }
    let mut out = String::new();
    if let Some(line) = lines.get(start.0) {
        let chars: Vec<char> = line.chars().collect();
        let s = start.1.min(chars.len());
        out.extend(chars[s..].iter());
    }
    for r in (start.0 + 1)..end.0 {
        out.push('\n');
        if let Some(line) = lines.get(r) {
            out.push_str(line);
        }
    }
    out.push('\n');
    if let Some(line) = lines.get(end.0) {
        let chars: Vec<char> = line.chars().collect();
        let e = end.1.min(chars.len());
        out.extend(chars[..e].iter());
    }
    out
}

/// Extend a single-line `TextArea`'s selection from its current cursor
/// column to `target_col` by feeding shift-Right / shift-Left keypresses.
/// The `tui_textarea` `move_cursor_with_shift` API is private; shift-arrow
/// chords are the public face of the same code path and reliably extend
/// (or anchor) the textarea's own selection on each press.
///
/// No-op when the cursor is already at the target.
pub(crate) fn select_to_column(area: &mut TextArea<'static>, target_col: usize) {
    let (_, current) = area.cursor();
    if target_col == current {
        return;
    }
    let (key, steps) = if target_col > current {
        (
            KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT),
            target_col - current,
        )
    } else {
        (
            KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT),
            current - target_col,
        )
    };
    for _ in 0..steps {
        area.input(key);
    }
}

/// Build a [`Line`] from a soft-wrapped visual row, with the char range
/// `[from, to)` rendered in reverse video (the standard "selected" cue
/// across terminal emulators). Out-of-range indices clamp to the segment
/// length so a slightly stale selection (drag-then-resize) renders safely.
pub(crate) fn highlight_line(seg: &str, from: usize, to: usize) -> Line<'static> {
    let chars: Vec<char> = seg.chars().collect();
    let lo = from.min(chars.len());
    let hi = to.min(chars.len()).max(lo);
    let pre: String = chars[..lo].iter().collect();
    let mid: String = chars[lo..hi].iter().collect();
    let post: String = chars[hi..].iter().collect();
    let style = Style::default().add_modifier(Modifier::REVERSED);
    Line::from(vec![
        Span::raw(pre),
        Span::styled(mid, style),
        Span::raw(post),
    ])
}

/// Body-specific wrapper around [`text_input`]: intercepts `Ctrl-U` and
/// `Ctrl-D` to page the cursor by half the visible Body height (vim/less
/// convention), then lets every other key fall through to the textarea.
///
/// Why: Phase 2 of note #91 globally bound `PageUp` / `PageDown` to
/// Zellij's `PageScrollUp` / `PageScrollDown`, which means a form pane's
/// PageUp does nothing (the form is on the terminal's alt-screen, so Zellij
/// scrollback is empty). The form needs its own paging gesture; `Ctrl-U` /
/// `Ctrl-D` is the convention every vim/less user already has in muscle
/// memory.
///
/// Page step is `(view_height / 2).max(1)`. A press before the first draw
/// (when `view_height` is still zero) moves one row, so the gesture is
/// never a no-op. The cursor moves, the existing `draw_body` viewport
/// follow-logic then catches up to keep it on screen.
///
/// `Ctrl-U` and `Ctrl-D` are not content edits, so they return `false`. The
/// viewer's `handle_key` redraws on every key regardless, so the visible
/// scroll position updates next frame. The textarea's default `Ctrl-U`
/// (undo) is shadowed; `Ctrl-Z` already covers undo here (see [`text_input`])
/// so users keep an undo binding. The textarea's default `Ctrl-D`
/// (delete-next-char) is shadowed too; `Delete` still forward-deletes.
pub(crate) fn body_input(area: &mut TextArea, key: KeyEvent, view_height: usize) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl {
        let step = (view_height / 2).max(1);
        match key.code {
            KeyCode::Char('u') => {
                for _ in 0..step {
                    area.move_cursor(CursorMove::Up);
                }
                return false;
            }
            KeyCode::Char('d') => {
                for _ in 0..step {
                    area.move_cursor(CursorMove::Down);
                }
                return false;
            }
            _ => {}
        }
    }
    text_input(area, key)
}

/// Feed a key to a single-line field, swallowing anything that would add a
/// line break (Enter, Ctrl-J, Ctrl-M) so it stays one line. All other
/// shortcuts go through [`text_input`].
pub(crate) fn single_line_input(area: &mut TextArea, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if key.code == KeyCode::Enter {
        return false;
    }
    if ctrl && matches!(key.code, KeyCode::Char('j') | KeyCode::Char('m')) {
        return false;
    }
    text_input(area, key)
}

/// Insert a pasted string into a textarea. Each `\n` becomes an `insert_newline`,
/// every other char goes through `insert_str`. Pastes always count as a change.
pub(crate) fn paste_into(area: &mut TextArea, s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut first = true;
    for line in s.split('\n') {
        if !first {
            area.insert_newline();
        }
        if !line.is_empty() {
            area.insert_str(line);
        }
        first = false;
    }
    true
}

/// Insert a pasted string into a single-line field: all line breaks are
/// flattened to spaces, since multi-line content has no place in a one-line
/// input row.
pub(crate) fn paste_into_single_line(area: &mut TextArea, s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let flat = s.replace(['\n', '\r'], " ");
    area.insert_str(flat);
    true
}

/// The cycle direction a key implies for an enum field, if any.
pub(crate) fn cycle_dir(code: KeyCode) -> Option<i32> {
    match code {
        KeyCode::Left => Some(-1),
        KeyCode::Right => Some(1),
        _ => None,
    }
}

/// Step index `i` by `dir`, wrapping within `len`.
pub(crate) fn wrap(i: usize, dir: i32, len: usize) -> usize {
    (i as i32 + dir).rem_euclid(len as i32) as usize
}

/// The position of `value` in `options`, or 0 when it is not present.
pub(crate) fn index_of(options: &[&str], value: &str) -> usize {
    options.iter().position(|o| *o == value).unwrap_or(0)
}

/// Whether two comment lists carry the same ids, authors, and bodies in
/// the same order. Used by [`TodoForm::refresh_from_daemon`] to decide
/// whether the daemon's snapshot is visibly different from what the form
/// holds; equality is exact rather than set-equality so a comment-edit
/// (same id, new body) still counts as a change.
fn comments_match(a: &[CommentEntry], b: &[CommentEntry]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.id == y.id && x.author == y.author && x.body == y.body)
}

/// Whether two blocker lists carry the same ids in the same order. Titles
/// are not compared - a renamed blocker still renders the same id-prefixed
/// chip, and the title is best-effort metadata anyway.
fn blockers_match(a: &[BlockerEntry], b: &[BlockerEntry]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.id == y.id)
}

/// Build the JSON payload for a `todo_update` call, including only the
/// scalar fields that differ between `baseline` and `current`. The result
/// always contains `todo_id`; a caller that finds it carries only
/// `todo_id` should skip the round-trip.
fn build_update_payload(
    id: u64,
    baseline: &Baseline,
    current: &Baseline,
) -> serde_json::Map<String, Value> {
    let mut payload = serde_json::Map::new();
    payload.insert("todo_id".into(), json!(id));
    if current.title != baseline.title {
        payload.insert("title".into(), json!(current.title));
    }
    if current.body != baseline.body {
        payload.insert("body".into(), json!(current.body));
    }
    if current.status != baseline.status {
        payload.insert("status".into(), json!(current.status));
    }
    if current.priority != baseline.priority {
        payload.insert("priority".into(), json!(current.priority));
    }
    if current.assignee != baseline.assignee {
        payload.insert("assignee".into(), json!(current.assignee));
    }
    if current.tags != baseline.tags {
        payload.insert("tags".into(), json!(current.tags));
    }
    payload
}

/// The non-empty strings of a JSON array value.
pub(crate) fn string_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_cycles_in_both_directions() {
        assert_eq!(wrap(0, -1, 4), 3);
        assert_eq!(wrap(3, 1, 4), 0);
        assert_eq!(wrap(1, 1, 4), 2);
    }

    #[test]
    fn index_of_finds_values_and_defaults_to_zero() {
        assert_eq!(index_of(&STATUSES, "completed"), 4);
        assert_eq!(index_of(&PRIORITIES, "medium"), 1);
        assert_eq!(index_of(&STATUSES, "bogus"), 0);
    }

    #[test]
    fn cycle_dir_maps_only_left_and_right() {
        assert_eq!(cycle_dir(KeyCode::Left), Some(-1));
        assert_eq!(cycle_dir(KeyCode::Right), Some(1));
        assert_eq!(cycle_dir(KeyCode::Char('x')), None);
    }

    #[test]
    fn blank_form_has_default_focus_and_no_id() {
        let form = TodoForm::blank("http://localhost/?ws=/x");
        assert_eq!(form.id, None);
        assert_eq!(form.focus, 0);
        assert!(form.title_is_empty());
        assert!(!form.dirty);
    }

    /// Regression: pasted multi-line text used to scrub itself across the body
    /// because the raw `\n` between lines parses as Ctrl-J in raw mode and
    /// `tui_textarea` reads Ctrl-J as `delete_line_by_head`. `text_input`
    /// remaps that chord to a literal newline; multi-line text now lands
    /// intact.
    #[test]
    fn ctrl_j_is_a_newline_in_text_input_not_a_delete() {
        let mut area = text_area("");
        for c in "hello".chars() {
            assert!(text_input(
                &mut area,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()),
            ));
        }
        // The byte `\n` arrives in raw mode as Ctrl-J.
        assert!(text_input(
            &mut area,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        ));
        for c in "world".chars() {
            text_input(
                &mut area,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()),
            );
        }
        assert_eq!(area.lines(), vec!["hello", "world"]);
    }

    /// `text_input` forwards the standard emacs-style editor chords to
    /// `tui_textarea`: Ctrl-A / Ctrl-E (home / end), Ctrl-K (delete to end),
    /// Ctrl-Z (undo, our override). The only Ctrl chord remapped here is
    /// Ctrl-J / Ctrl-M -> newline.
    #[test]
    fn ctrl_letter_chords_reach_the_textarea() {
        let mut area = text_area("hello world");
        // Ctrl-A goes to start of line.
        text_input(
            &mut area,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        assert_eq!(area.cursor(), (0, 0));
        // Ctrl-E goes to end of line.
        text_input(
            &mut area,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
        );
        assert_eq!(area.cursor(), (0, "hello world".len()));
        // Ctrl-A then Ctrl-K kills to end of line.
        text_input(
            &mut area,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        text_input(
            &mut area,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        assert_eq!(area.lines(), vec![""]);
    }

    /// `body_input`'s Ctrl-D / Ctrl-U page the cursor by half the visible
    /// body height, the vim/less convention. The text is unchanged so the
    /// helper returns `false`, but the textarea's cursor must have moved.
    #[test]
    fn body_input_ctrl_d_and_ctrl_u_page_by_half_view_height() {
        let mut area = text_area("");
        for _ in 0..40 {
            area.insert_newline();
        }
        // Cursor sits on the last (41st) line after the 40 inserts.
        let (start_row, _) = area.cursor();
        assert_eq!(start_row, 40);
        area.move_cursor(CursorMove::Top);
        assert_eq!(area.cursor().0, 0);

        // View height 20 -> step 10. Ctrl-D pages down ten rows.
        let changed = body_input(
            &mut area,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            20,
        );
        assert!(!changed, "Ctrl-D is a cursor move, not a content edit");
        assert_eq!(area.cursor().0, 10);

        // Ctrl-U pages back the same distance.
        body_input(
            &mut area,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            20,
        );
        assert_eq!(area.cursor().0, 0);
    }

    /// Before the first draw the form has not yet learned its visible body
    /// height. `body_input` must still respond to Ctrl-U / Ctrl-D - falling
    /// back to a one-row step - rather than silently doing nothing.
    #[test]
    fn body_input_pages_at_least_one_row_before_first_draw() {
        let mut area = text_area("");
        for _ in 0..5 {
            area.insert_newline();
        }
        area.move_cursor(CursorMove::Top);
        body_input(
            &mut area,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            0,
        );
        assert_eq!(area.cursor().0, 1);
    }

    #[test]
    fn selected_text_extracts_single_line_substring() {
        let lines = vec!["hello world".to_string()];
        assert_eq!(selected_text(&lines, (0, 6), (0, 11)), "world");
        // Reversed anchors produce the same text.
        assert_eq!(selected_text(&lines, (0, 11), (0, 6)), "world");
    }

    #[test]
    fn selected_text_joins_multi_line_with_newlines() {
        let lines = vec!["abc".to_string(), "def".to_string(), "ghi".to_string()];
        // From (0,1) to (2,2): "bc\ndef\ngh".
        assert_eq!(selected_text(&lines, (0, 1), (2, 2)), "bc\ndef\ngh");
    }

    #[test]
    fn selected_text_clamps_past_end_columns() {
        let lines = vec!["abc".to_string()];
        assert_eq!(selected_text(&lines, (0, 0), (0, 99)), "abc");
    }

    /// Mouse-drag-then-release with a non-empty selection emits OSC 52. We
    /// can't read the host stdout from a test, but the selection bookkeeping
    /// is the part that actually drives the copy - verify those transitions.
    #[test]
    fn drag_then_release_populates_selection_and_clears_on_bare_click() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;
        let mut form = TodoForm::blank("http://localhost/?ws=/x");
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::empty());
        for _ in 0..5 {
            form.handle_key(tab);
        }
        for c in "hello world".chars() {
            form.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()));
        }
        form.body_area = Some(Rect::new(10, 5, 20, 4));
        form.body_scroll = 0;
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 5,
            modifiers: KeyModifiers::empty(),
        };
        form.handle_mouse(down);
        assert_eq!(form.selection, Some(((0, 0), (0, 0))));
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 10 + 5,
            row: 5,
            modifiers: KeyModifiers::empty(),
        };
        form.handle_mouse(drag);
        assert_eq!(form.selection, Some(((0, 0), (0, 5))));
        // Releasing with a non-empty selection keeps it set (so the highlight
        // survives until the next interaction); the OSC 52 emit is the
        // side-effect we can't observe here.
        let up = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 10 + 5,
            row: 5,
            modifiers: KeyModifiers::empty(),
        };
        form.handle_mouse(up);
        assert_eq!(form.selection, Some(((0, 0), (0, 5))));
        // A fresh bare click (Down + Up with no drag) starts then clears.
        form.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10 + 2,
            row: 5,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(form.selection, Some(((0, 2), (0, 2))));
        form.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 10 + 2,
            row: 5,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(form.selection, None);
    }

    /// Any keystroke other than Ctrl-Shift-C clears the selection so the
    /// highlight doesn't linger over text the user is editing past.
    #[test]
    fn keystroke_clears_a_pending_selection() {
        let mut form = TodoForm::blank("http://localhost/?ws=/x");
        form.selection = Some(((0, 0), (0, 3)));
        form.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::empty()));
        assert_eq!(form.selection, None);
    }

    /// Ctrl-Shift-C does NOT clear the selection - it re-emits OSC 52 while
    /// leaving the highlight in place so the user can copy again.
    #[test]
    fn ctrl_shift_c_preserves_the_selection() {
        let mut form = TodoForm::blank("http://localhost/?ws=/x");
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::empty());
        for _ in 0..5 {
            form.handle_key(tab);
        }
        for c in "hi".chars() {
            form.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()));
        }
        form.selection = Some(((0, 0), (0, 2)));
        form.handle_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(form.selection, Some(((0, 0), (0, 2))));
    }

    /// Click-to-position-cursor: a left-click inside the recorded body
    /// rectangle focuses the Body field and moves the textarea cursor to the
    /// logical position under the click, mapped through the soft-wrap.
    #[test]
    fn left_click_in_body_area_positions_the_cursor() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;
        let mut form = TodoForm::blank("http://localhost/?ws=/x");
        // Pre-fill the body so there's something to click into.
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::empty());
        for _ in 0..5 {
            form.handle_key(tab);
        }
        assert_eq!(FIELDS[form.focus], Field::Body);
        for c in "hello world".chars() {
            form.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()));
        }
        // Pretend the body rendered into rows 5..=8 at columns 10..=29.
        form.body_area = Some(Rect::new(10, 5, 20, 4));
        form.body_scroll = 0;
        // Click at the start of the second word, "world": visual (0, 6).
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10 + 6,
            row: 5,
            modifiers: KeyModifiers::empty(),
        };
        let action = form.handle_mouse(click);
        assert_eq!(action, TodoFormAction::Idle);
        assert_eq!(form.body.cursor(), (0, 6));
        assert_eq!(FIELDS[form.focus], Field::Body);
    }

    /// Drag-to-select on the Title field plants an anchor on Down, extends
    /// the textarea's own selection on Drag, and on Up leaves a non-empty
    /// selection range covering the dragged span.
    #[test]
    fn drag_in_single_line_field_builds_a_selection_range() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;
        let mut form = TodoForm::blank("http://localhost/?ws=/x");
        // Title is the focus on a blank form; seed some content.
        for c in "hello world".chars() {
            form.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()));
        }
        // Pretend the title textarea drew with its content rect starting at
        // column 11 (a typical inner-of-border position).
        form.text_field_areas
            .push((Field::Title, Rect::new(11, 1, 30, 1)));
        // Down at col 11 -> textarea column 0, drag to col 16 -> textarea
        // column 5; the textarea should report a selection covering the
        // first five characters.
        form.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 11,
            row: 1,
            modifiers: KeyModifiers::empty(),
        });
        form.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 16,
            row: 1,
            modifiers: KeyModifiers::empty(),
        });
        let range = form.title.selection_range();
        assert_eq!(range, Some(((0, 0), (0, 5))));
    }

    /// A click outside the body rectangle is a no-op; the form's focus and
    /// the textarea's cursor must not move.
    #[test]
    fn left_click_outside_body_area_is_a_noop() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Rect;
        let mut form = TodoForm::blank("http://localhost/?ws=/x");
        let initial_focus = form.focus;
        form.body_area = Some(Rect::new(10, 5, 20, 4));
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        };
        let action = form.handle_mouse(click);
        assert_eq!(action, TodoFormAction::Idle);
        assert_eq!(form.focus, initial_focus);
    }

    /// Regression guard for the bracketed-paste path: `body_input` must
    /// forward ordinary typing to the textarea unchanged (only Ctrl-U /
    /// Ctrl-D are intercepted; the rest goes through `text_input`).
    #[test]
    fn body_input_forwards_typing_through_text_input() {
        let mut area = text_area("");
        for c in "hi".chars() {
            body_input(
                &mut area,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()),
                20,
            );
        }
        assert_eq!(area.lines(), vec!["hi"]);
    }

    /// Ctrl-Z is our convenience binding for undo (`tui_textarea`'s native
    /// one is Ctrl-U). After typing and then Ctrl-Z, the change is rolled back.
    #[test]
    fn ctrl_z_undoes_the_last_edit() {
        let mut area = text_area("");
        for c in "hi".chars() {
            text_input(
                &mut area,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()),
            );
        }
        assert_eq!(area.lines(), vec!["hi"]);
        text_input(
            &mut area,
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
        );
        assert_ne!(area.lines(), vec!["hi"]);
    }

    /// Regression: pasting a multi-line block into the Body field used to
    /// erase itself line by line. `handle_paste` inserts the payload
    /// atomically, so every line survives.
    #[test]
    fn handle_paste_into_body_preserves_lines() {
        let mut form = TodoForm::blank("http://localhost/?ws=/x");
        // Tab past Title -> Status -> Priority -> Assignee -> Tags -> Body.
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::empty());
        for _ in 0..5 {
            form.handle_key(tab);
        }
        assert_eq!(FIELDS[form.focus], Field::Body);
        let payload = "- one\n- two\n- three";
        let action = form.handle_paste(payload);
        assert_eq!(action, TodoFormAction::Dirty);
        assert_eq!(form.body.lines(), vec!["- one", "- two", "- three"]);
    }

    /// Regression: pasting into Title flattens line breaks instead of
    /// silently splitting the single-line field.
    #[test]
    fn handle_paste_into_title_flattens_newlines() {
        let mut form = TodoForm::blank("http://localhost/?ws=/x");
        assert_eq!(FIELDS[form.focus], Field::Title);
        let action = form.handle_paste("first line\nsecond line");
        assert_eq!(action, TodoFormAction::Dirty);
        assert_eq!(form.title.lines(), vec!["first line second line"]);
    }

    /// Regression: while the new-comment input row is focused, typing `d`,
    /// `j`, or `k` used to delete an existing comment or move the cursor
    /// instead of typing a character. After the fix, those keys land in the
    /// textarea like any other letter.
    /// Regression: an idle form whose user only edits one field must not push
    /// the other (stale) scalar fields back to the daemon. Before the fix,
    /// `flush` always wrote every scalar wholesale, so any local edit would
    /// clobber a concurrent `todo_complete` from the CLI or an MCP agent.
    #[test]
    fn flush_payload_includes_only_changed_scalars() {
        let baseline = Baseline {
            title: "wire up auth".to_string(),
            body: "details".to_string(),
            status: "open".to_string(),
            priority: "medium".to_string(),
            assignee: String::new(),
            tags: Vec::new(),
        };
        // The user only changed the body; every other scalar is unchanged.
        let current = Baseline {
            title: "wire up auth".to_string(),
            body: "details with a fresh line".to_string(),
            status: "open".to_string(),
            priority: "medium".to_string(),
            assignee: String::new(),
            tags: Vec::new(),
        };
        let payload = build_update_payload(7, &baseline, &current);
        assert_eq!(payload.get("todo_id"), Some(&json!(7)));
        assert_eq!(
            payload.get("body"),
            Some(&json!("details with a fresh line"))
        );
        // The clobber prevention: untouched fields are absent, so the daemon's
        // "omitted = leave unchanged" semantics protect a concurrent edit.
        assert!(!payload.contains_key("title"));
        assert!(!payload.contains_key("status"));
        assert!(!payload.contains_key("priority"));
        assert!(!payload.contains_key("assignee"));
        assert!(!payload.contains_key("tags"));
        assert_eq!(payload.len(), 2);
    }

    /// When the form has no diff against its baseline, the payload should
    /// carry only `todo_id`, signalling the caller to skip the round-trip.
    #[test]
    fn flush_payload_is_empty_when_form_matches_baseline() {
        let baseline = Baseline {
            title: "t".to_string(),
            body: "b".to_string(),
            status: "open".to_string(),
            priority: "high".to_string(),
            assignee: "alex".to_string(),
            tags: vec!["x".to_string()],
        };
        let current = baseline.clone();
        let payload = build_update_payload(3, &baseline, &current);
        assert_eq!(payload.len(), 1);
        assert_eq!(payload.get("todo_id"), Some(&json!(3)));
    }

    #[test]
    fn new_comment_row_accepts_d_j_k_as_typed_chars() {
        let mut form = TodoForm::blank("http://localhost/?ws=/x");
        // Tab past everything to reach Comments.
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::empty());
        for _ in 0..6 {
            form.handle_key(tab);
        }
        assert_eq!(FIELDS[form.focus], Field::Comments);
        // No existing comments -> cursor sits on the input row.
        assert_eq!(form.comment_cursor, 0);
        for c in ['d', 'j', 'k'] {
            form.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()));
        }
        assert_eq!(form.new_comment.lines(), vec!["djk"]);
    }

    /// Helper for the chip-strip tests: tabs to the Blockers field and seeds
    /// the local blocker list without going through the daemon.
    fn form_on_blockers_with(ids_and_titles: &[(u64, &str)]) -> TodoForm {
        let mut form = TodoForm::blank("http://localhost/?ws=/x");
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::empty());
        for _ in 0..7 {
            form.handle_key(tab);
        }
        assert_eq!(FIELDS[form.focus], Field::Blockers);
        for (id, title) in ids_and_titles {
            form.blockers.push(BlockerEntry {
                id: *id,
                title: (*title).to_string(),
            });
        }
        form.blocker_cursor = 0;
        form
    }

    /// Left/Right cycle through chips and then onto the trailing input row;
    /// Right past the input is a no-op, Left at the start is a no-op.
    #[test]
    fn blocker_chip_strip_cycles_with_left_and_right() {
        let mut form = form_on_blockers_with(&[(3, "wire up auth"), (7, "deploy")]);

        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::empty());
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::empty());

        form.handle_key(right);
        assert_eq!(form.blocker_cursor, 1);
        form.handle_key(right);
        assert_eq!(form.blocker_cursor, 2); // input row
        form.handle_key(right);
        assert_eq!(form.blocker_cursor, 2); // no further right
        form.handle_key(left);
        assert_eq!(form.blocker_cursor, 1);
        form.handle_key(left);
        assert_eq!(form.blocker_cursor, 0);
        form.handle_key(left);
        assert_eq!(form.blocker_cursor, 0); // no further left
    }

    /// Build a TodoForm by piping a synthetic todo through `from_todo`, so
    /// the baseline and every scalar field land in the same shape they do
    /// from a live `todo_get`. The blocker resolver returns no titles -
    /// tests that care about chips wire that themselves.
    fn form_with_todo(todo: Value) -> TodoForm {
        let resolver = |_: u64| -> Option<String> { None };
        TodoForm::from_todo("http://localhost/?ws=/x", &todo, &resolver).unwrap()
    }

    fn remote_baseline(
        title: &str,
        body: &str,
        status: &str,
        priority: &str,
        assignee: &str,
        tags: &[&str],
    ) -> Baseline {
        Baseline {
            title: title.to_string(),
            body: body.to_string(),
            status: status.to_string(),
            priority: priority.to_string(),
            assignee: assignee.to_string(),
            tags: tags.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    /// A field the user has not touched gets replayed with the daemon's
    /// current value, and the baseline advances to match - so a subsequent
    /// flush sees `current == baseline` and produces no payload.
    #[test]
    fn refresh_replays_remote_into_untouched_scalar_fields() {
        let mut form = form_with_todo(json!({
            "id": 1, "title": "A", "body": "b1",
            "status": "open", "priority": "medium",
            "assignee": "", "tags": [],
        }));
        let remote = remote_baseline("B", "b2", "completed", "high", "alex", &["x"]);
        let changed = form.replay_remote(remote, Vec::new(), Vec::new(), String::new());
        assert!(changed);
        assert_eq!(form.current_title(), "B");
        assert_eq!(form.current_body(), "b2");
        assert_eq!(STATUSES[form.status], "completed");
        assert_eq!(PRIORITIES[form.priority], "high");
        assert_eq!(form.current_assignee(), "alex");
        assert_eq!(form.current_tags(), vec!["x".to_string()]);
        assert_eq!(form.baseline.title, "B");
        assert!(form.message.is_empty() || !form.message.contains("remote changed"));
    }

    /// A field the user is mid-edit on keeps its local text; the baseline
    /// still advances to the daemon's value so the next `flush` diff sends
    /// only the user's pending edit. The conflict surfaces in the message.
    #[test]
    fn refresh_keeps_local_edit_and_flags_conflict() {
        let mut form = form_with_todo(json!({
            "id": 1, "title": "A", "body": "", "status": "open",
            "priority": "medium", "assignee": "", "tags": [],
        }));
        // User typed past the baseline - local edit pending.
        form.title = text_area("A draft");
        let remote = remote_baseline("B", "", "open", "medium", "", &[]);
        form.replay_remote(remote, Vec::new(), Vec::new(), String::new());
        assert_eq!(form.current_title(), "A draft");
        assert_eq!(form.baseline.title, "B");
        assert!(form.message.contains("title"));
        // The user's pending edit now diverges from the new baseline by
        // exactly one field, so flush would send only the title.
        let current = Baseline {
            title: form.current_title(),
            body: form.current_body(),
            status: STATUSES[form.status].to_string(),
            priority: PRIORITIES[form.priority].to_string(),
            assignee: form.current_assignee(),
            tags: form.current_tags(),
        };
        let payload = build_update_payload(1, &form.baseline, &current);
        assert_eq!(payload.len(), 2); // todo_id + title
        assert_eq!(payload.get("title"), Some(&json!("A draft")));
    }

    /// Status/priority follow the same rule via their string tokens: an
    /// untouched status replays, a locally-changed one is preserved.
    #[test]
    fn refresh_status_replays_when_untouched_and_holds_when_local() {
        // Untouched: replays.
        let mut form = form_with_todo(json!({
            "id": 1, "title": "t", "body": "", "status": "open",
            "priority": "medium", "assignee": "", "tags": [],
        }));
        let remote = remote_baseline("t", "", "completed", "medium", "", &[]);
        form.replay_remote(remote, Vec::new(), Vec::new(), String::new());
        assert_eq!(STATUSES[form.status], "completed");

        // Locally changed: preserved, baseline advances, conflict surfaces.
        let mut form = form_with_todo(json!({
            "id": 1, "title": "t", "body": "", "status": "open",
            "priority": "medium", "assignee": "", "tags": [],
        }));
        form.status = index_of(&STATUSES, "in_progress");
        let remote = remote_baseline("t", "", "completed", "medium", "", &[]);
        form.replay_remote(remote, Vec::new(), Vec::new(), String::new());
        assert_eq!(STATUSES[form.status], "in_progress");
        assert_eq!(form.baseline.status, "completed");
        assert!(form.message.contains("status"));
    }

    /// Comment list is owned by the daemon - a refresh blanket-replaces it,
    /// but only when no in-place edit is active. An active editor pins the
    /// list so the edit's row index does not shift under it.
    #[test]
    fn refresh_replaces_comments_unless_editing_one() {
        let mut form = form_with_todo(json!({
            "id": 1, "title": "t", "body": "",
            "status": "open", "priority": "medium",
            "assignee": "", "tags": [],
            "comments": [{ "id": 11, "author": "a", "created_at": "", "body": "first" }],
        }));
        let baseline = form.baseline.clone();
        let remote_comments = vec![
            CommentEntry {
                id: 11,
                author: "a".to_string(),
                created_at: String::new(),
                body: "first".to_string(),
            },
            CommentEntry {
                id: 12,
                author: "b".to_string(),
                created_at: String::new(),
                body: "second".to_string(),
            },
        ];
        form.replay_remote(
            baseline.clone(),
            remote_comments.clone(),
            Vec::new(),
            String::new(),
        );
        assert_eq!(form.comments.len(), 2);

        // Now open an in-place edit and re-run with a divergent list - the
        // editor's index must survive the refresh.
        form.editing_comment = Some((0, text_area("editing first")));
        let remote_comments_after = vec![CommentEntry {
            id: 12,
            author: "b".to_string(),
            created_at: String::new(),
            body: "only second".to_string(),
        }];
        form.replay_remote(baseline, remote_comments_after, Vec::new(), String::new());
        assert_eq!(form.comments.len(), 2);
        assert!(form.editing_comment.is_some());
    }

    /// On the input row, Backspace with non-empty content edits the textarea;
    /// only an empty input triggers the Gmail-recipients pattern (and would
    /// pop the rightmost chip, but here we just confirm the no-pop path).
    #[test]
    fn blocker_input_backspace_with_text_edits_the_textarea() {
        let mut form = form_on_blockers_with(&[(3, "wire up auth")]);
        // Move to the input row.
        form.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::empty()));
        assert_eq!(form.blocker_cursor, 1);
        // Type some characters, then Backspace - the textarea should lose
        // the last character and the chip list should stay intact.
        for c in ['1', '2'] {
            form.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()));
        }
        assert_eq!(form.new_blocker.lines(), vec!["12"]);
        form.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()));
        assert_eq!(form.new_blocker.lines(), vec!["1"]);
        assert_eq!(form.blockers.len(), 1);
    }
}
