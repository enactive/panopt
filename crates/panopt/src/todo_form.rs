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
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use serde_json::{json, Value};
use tui_textarea::TextArea;

use crate::mcpclient::Client;

/// The cyclable status values, in cycle order.
pub(crate) const STATUSES: [&str; 5] = ["open", "in_progress", "backlog", "completed", "not_done"];
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
            ScalarField::Body => text_input(&mut self.body, key),
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
        if title.is_empty() {
            // Nothing to save against - silently no-op so an autosave on an
            // empty new form does not spam errors.
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
        // Rows: header / title / status+priority / assignee+tags / body /
        // comments / blockers / context / message.
        // Body is 3x the size of comments. Blockers is a single-line chip
        // strip; Status, Priority, Assignee, and Tags are also single-line
        // inline fields - together they free six rows of vertical space for
        // the body and comments blocks.
        let rows = Layout::vertical([
            Constraint::Length(1),   // header (incl. locked-by banner)
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

        let header_text = match (&self.id, &self.locked_by) {
            (Some(id), Some(holder)) => format!(" Edit todo #{id}   [locked by {holder}]"),
            (Some(id), None) => format!(" Edit todo #{id}"),
            (None, _) => " New todo".to_string(),
        };
        let header_style = if self.locked_by.is_some() {
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Red)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        frame.render_widget(Paragraph::new(header_text).style(header_style), rows[0]);

        self.style_field(Field::Title, "Title");
        frame.render_widget(&self.title, rows[1]);

        let focus = FIELDS[self.focus];
        let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[2]);
        frame.render_widget(
            enum_line("Status", STATUSES[self.status], focus == Field::Status),
            cols[0],
        );
        frame.render_widget(
            enum_line(
                "Priority",
                PRIORITIES[self.priority],
                focus == Field::Priority,
            ),
            cols[1],
        );

        self.style_inline_field(Field::Assignee);
        self.style_inline_field(Field::Tags);
        let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[3]);
        let assignee_cols =
            Layout::horizontal([Constraint::Length(11), Constraint::Min(1)]).split(cols[0]);
        frame.render_widget(
            Paragraph::new(" Assignee: ").style(label_style(focus == Field::Assignee)),
            assignee_cols[0],
        );
        frame.render_widget(&self.assignee, assignee_cols[1]);
        let tags_cols =
            Layout::horizontal([Constraint::Length(7), Constraint::Min(1)]).split(cols[1]);
        frame.render_widget(
            Paragraph::new(" Tags: ").style(label_style(focus == Field::Tags)),
            tags_cols[0],
        );
        frame.render_widget(&self.tags, tags_cols[1]);

        self.draw_body(frame, rows[4]);

        self.draw_comments(frame, rows[5]);
        self.draw_blockers(frame, rows[6]);

        let context = if !self.created.is_empty() {
            format!(" created {}   updated {}", self.created, self.updated)
        } else {
            String::new()
        };
        frame.render_widget(
            Paragraph::new(context).style(Style::default().fg(Color::DarkGray)),
            rows[7],
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
        let line = if displayed_message.is_empty() {
            format!(" {help}")
        } else {
            format!(" {displayed_message}   |   {help}")
        };
        frame.render_widget(
            Paragraph::new(line).style(Style::default().fg(Color::Yellow)),
            rows[8],
        );
    }

    /// Render the Body field as a soft-wrapped paragraph plus an overlay
    /// cursor. We bypass [`tui_textarea::TextArea`]'s own draw because that
    /// widget has no soft-wrap - it scrolls horizontally instead, so a pasted
    /// line longer than the field reads as "my text vanished off the right
    /// edge." The buffer and edit logic still live in the textarea; only the
    /// visual representation is ours.
    fn draw_body(&mut self, frame: &mut Frame, area: Rect) {
        let focused = FIELDS[self.focus] == Field::Body;
        let border = if focused {
            Color::Yellow
        } else {
            Color::DarkGray
        };
        let block = Block::bordered()
            .title("Body")
            .border_style(Style::default().fg(border));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let width = inner.width as usize;
        let height = inner.height as usize;
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

        let visible: Vec<Line> = wrapped
            .lines
            .iter()
            .skip(self.body_scroll)
            .take(height)
            .map(|l| Line::from(l.clone()))
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
        let border = if focused {
            Color::Yellow
        } else {
            Color::DarkGray
        };
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
        let border = if focused {
            Color::Yellow
        } else {
            Color::DarkGray
        };
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
        assert_eq!(index_of(&STATUSES, "completed"), 3);
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
