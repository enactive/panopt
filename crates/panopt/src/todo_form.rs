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
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use serde_json::{json, Value};
use tui_textarea::TextArea;

use crate::mcpclient::Client;

/// The cyclable status values, in cycle order.
pub(crate) const STATUSES: [&str; 4] = ["open", "in_progress", "backlog", "completed"];
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
    /// Selected row in the blockers section.
    blocker_cursor: usize,
    /// Input row that adds a new blocker via `todo_add_blocker` on Enter.
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
            new_blocker: text_area(""),
            created: String::new(),
            updated: String::new(),
            locked_by: None,
            dirty: false,
            dirty_since: None,
            can_quit: true,
            message: "new todo - type to begin".to_string(),
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
        let id = todo["id"].as_u64().ok_or_else(|| anyhow!("todo response has no id"))?;
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
        Ok(TodoForm {
            url: url.to_string(),
            id: Some(id),
            title: text_area(todo["title"].as_str().unwrap_or("")),
            assignee: text_area(todo["assignee"].as_str().unwrap_or("")),
            tags: text_area(&tags),
            body: text_area(todo["body"].as_str().unwrap_or("")),
            status: index_of(&STATUSES, todo["status"].as_str().unwrap_or("open")),
            priority: index_of(&PRIORITIES, todo["priority"].as_str().unwrap_or("medium")),
            focus: 0,
            comments,
            comment_cursor: 0,
            editing_comment: None,
            new_comment: text_area(""),
            blockers,
            blocker_cursor: 0,
            new_blocker: text_area(""),
            created: todo["created_at"].as_str().unwrap_or("").to_string(),
            updated: todo["updated_at"].as_str().unwrap_or("").to_string(),
            locked_by: todo["locked_by"].as_str().map(str::to_string),
            dirty: false,
            dirty_since: None,
            can_quit: true,
            message: format!("editing todo #{id}"),
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
            KeyCode::Char('c') if ctrl => return TodoFormAction::Close,
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
            ScalarField::Body => self.body.input(key),
        };
        if changed {
            self.mark_dirty();
            TodoFormAction::Dirty
        } else {
            TodoFormAction::Idle
        }
    }

    /// Keys handled when the comments section has focus. The cursor moves over
    /// existing comments (j/k); `Enter` either commits typed input as a new
    /// comment or, if the focus is on an existing comment, starts editing it
    /// in place; `d` deletes the highlighted comment. Any other printable key
    /// types into the bottom input row.
    fn comments_section_key(&mut self, key: KeyEvent) -> TodoFormAction {
        // Rows in the comment section, top to bottom: each existing comment is
        // a row; the trailing input row is `comments.len()`.
        let total_rows = self.comments.len() + 1;
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
                if self.comment_cursor == self.comments.len() {
                    // Append: ship the new comment, clear the input.
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
                } else {
                    // Begin in-place edit of the highlighted comment.
                    let existing = self.comments[self.comment_cursor].body.clone();
                    self.editing_comment = Some((self.comment_cursor, text_area(&existing)));
                    TodoFormAction::Idle
                }
            }
            KeyCode::Char('d') => {
                if self.comment_cursor < self.comments.len() {
                    let cid = self.comments[self.comment_cursor].id;
                    match self.delete_comment(cid) {
                        Ok(()) => self.message = format!("deleted comment #{cid}"),
                        Err(e) => self.message = format!("delete failed: {e:#}"),
                    }
                }
                TodoFormAction::Idle
            }
            _ => {
                // Anything else types into the new-comment input row.
                if self.comment_cursor == self.comments.len() {
                    let _ = self.new_comment.input(key);
                }
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
                    area.input(key);
                }
                TodoFormAction::Idle
            }
        }
    }

    /// Keys handled when the blockers section has focus. `Enter` on the input
    /// row adds a blocker by id; `d` removes the highlighted one.
    fn blockers_section_key(&mut self, key: KeyEvent) -> TodoFormAction {
        let total_rows = self.blockers.len() + 1;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.blocker_cursor > 0 {
                    self.blocker_cursor -= 1;
                }
                TodoFormAction::Idle
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.blocker_cursor + 1 < total_rows {
                    self.blocker_cursor += 1;
                }
                TodoFormAction::Idle
            }
            KeyCode::Enter => {
                if self.blocker_cursor == self.blockers.len() {
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
                }
                TodoFormAction::Idle
            }
            KeyCode::Char('d') => {
                if self.blocker_cursor < self.blockers.len() {
                    let bid = self.blockers[self.blocker_cursor].id;
                    match self.remove_blocker(bid) {
                        Ok(()) => self.message = format!("removed blocker #{bid}"),
                        Err(e) => self.message = format!("remove failed: {e:#}"),
                    }
                }
                TodoFormAction::Idle
            }
            _ => {
                if self.blocker_cursor == self.blockers.len() {
                    let _ = self.new_blocker.input(key);
                }
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

    /// Push every scalar field back to the daemon. Used by both the CLI's
    /// Ctrl-S handler and the viewer's debounced autosave.
    ///
    /// Creates the todo first when this is a new form (no id yet) and the
    /// title is non-empty; otherwise saves only the existing scalar fields.
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
                    id
                }
            };
            client.call(
                "todo_update",
                json!({
                    "todo_id": id,
                    "title": title,
                    "body": body,
                    "status": STATUSES[self.status],
                    "priority": PRIORITIES[self.priority],
                    "assignee": assignee,
                    "tags": tags,
                }),
            )?;
            Ok(())
        })();
        client.close();
        outcome?;
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
            self.blockers.push(BlockerEntry { id: blocker_id, title });
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
        // Rows: header / title / status+priority / assignee / tags / body /
        // comments / blockers / context / message.
        let rows = Layout::vertical([
            Constraint::Length(1), // header (incl. locked-by banner)
            Constraint::Length(3), // title
            Constraint::Length(1), // status + priority
            Constraint::Length(3), // assignee
            Constraint::Length(3), // tags
            Constraint::Min(3),    // body
            Constraint::Min(3),    // comments
            Constraint::Min(2),    // blockers
            Constraint::Length(1), // context (created/updated)
            Constraint::Length(1), // message + help
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

        let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[2]);
        let focus = FIELDS[self.focus];
        frame.render_widget(
            enum_line("Status", STATUSES[self.status], focus == Field::Status),
            cols[0],
        );
        frame.render_widget(
            enum_line("Priority", PRIORITIES[self.priority], focus == Field::Priority),
            cols[1],
        );

        self.style_field(Field::Assignee, "Assignee");
        frame.render_widget(&self.assignee, rows[3]);
        self.style_field(Field::Tags, "Tags (comma-separated)");
        frame.render_widget(&self.tags, rows[4]);
        self.style_field(Field::Body, "Body");
        frame.render_widget(&self.body, rows[5]);

        self.draw_comments(frame, rows[6]);
        self.draw_blockers(frame, rows[7]);

        let context = if !self.created.is_empty() {
            format!(" created {}   updated {}", self.created, self.updated)
        } else {
            String::new()
        };
        frame.render_widget(
            Paragraph::new(context).style(Style::default().fg(Color::DarkGray)),
            rows[8],
        );

        let help = "Tab field  Left/Right cycle  Enter add/edit  d delete  Ctrl-C close";
        let line = if self.message.is_empty() {
            format!(" {help}")
        } else {
            format!(" {}   |   {help}", self.message)
        };
        frame.render_widget(
            Paragraph::new(line).style(Style::default().fg(Color::Yellow)),
            rows[9],
        );
    }

    fn draw_comments(&mut self, frame: &mut Frame, area: Rect) {
        let focused = FIELDS[self.focus] == Field::Comments;
        let border = if focused { Color::Yellow } else { Color::DarkGray };
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
                        format!(" #{} {} - editing (Ctrl-S save, Esc cancel)", c.id, c.author),
                        Style::default().fg(Color::Yellow),
                    ));
                } else {
                    lines.push(Line::from(format!(" #{} {} {} - {}", c.id, c.author, c.created_at, c.body)));
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
            Paragraph::new(" (no comments)")
                .style(Style::default().fg(Color::DarkGray))
        } else {
            Paragraph::new(lines)
        };
        frame.render_widget(body, inner_rows[0]);

        // The add-row: a `+ ` prefix plus the textarea so it reads as an input.
        let add_focused = focused && self.comment_cursor == self.comments.len();
        let prefix_style = if add_focused {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let prefix_cols = Layout::horizontal([Constraint::Length(2), Constraint::Min(1)]).split(inner_rows[1]);
        frame.render_widget(Paragraph::new("+ ").style(prefix_style), prefix_cols[0]);
        frame.render_widget(&self.new_comment, prefix_cols[1]);
    }

    fn draw_blockers(&mut self, frame: &mut Frame, area: Rect) {
        let focused = FIELDS[self.focus] == Field::Blockers;
        let border = if focused { Color::Yellow } else { Color::DarkGray };
        let block = Block::bordered()
            .title(format!("Blockers ({})", self.blockers.len()))
            .border_style(Style::default().fg(border));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let inner_rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

        let lines: Vec<Line> = self
            .blockers
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let label = if b.title.is_empty() {
                    format!(" #{}", b.id)
                } else {
                    format!(" #{} {}", b.id, b.title)
                };
                if focused && i == self.blocker_cursor {
                    Line::styled(label, Style::default().add_modifier(Modifier::REVERSED))
                } else {
                    Line::from(label)
                }
            })
            .collect();
        let body = if lines.is_empty() {
            Paragraph::new(" (no blockers)")
                .style(Style::default().fg(Color::DarkGray))
        } else {
            Paragraph::new(lines)
        };
        frame.render_widget(body, inner_rows[0]);

        let add_focused = focused && self.blocker_cursor == self.blockers.len();
        let prefix_style = if add_focused {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let prefix_cols = Layout::horizontal([Constraint::Length(7), Constraint::Min(1)]).split(inner_rows[1]);
        frame.render_widget(Paragraph::new("+ id: ").style(prefix_style), prefix_cols[0]);
        frame.render_widget(&self.new_blocker, prefix_cols[1]);
    }

    /// Set a text field's border and cursor styling for the current focus.
    fn style_field(&mut self, field: Field, label: &'static str) {
        let focused = FIELDS[self.focus] == field;
        let area = match field {
            Field::Title => &mut self.title,
            Field::Assignee => &mut self.assignee,
            Field::Tags => &mut self.tags,
            Field::Body => &mut self.body,
            Field::Status
            | Field::Priority
            | Field::Comments
            | Field::Blockers => return,
        };
        let border = if focused { Color::Yellow } else { Color::DarkGray };
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
    let style = if focused {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    Paragraph::new(format!(" {label}: < {value} >")).style(style)
}

/// Feed a key to a single-line field, swallowing Enter so it stays one line.
/// Returns whether the field's content changed.
pub(crate) fn single_line_input(area: &mut TextArea, key: KeyEvent) -> bool {
    if key.code == KeyCode::Enter {
        return false;
    }
    area.input(key)
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

/// The non-empty strings of a JSON array value.
pub(crate) fn string_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(str::to_string).collect())
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
}
