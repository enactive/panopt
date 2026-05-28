//! The editable scratchpad form, hosted in-pane by the cockpit's `_viewer`.
//!
//! Sibling of [`crate::todo_form`] but trimmed to fit the scratchpad model:
//! one single-line title field and one multi-line body field, no enums, no
//! comments, no blockers. The `draw` takes a `Rect` so the viewer can place
//! it directly; `handle_key` returns a [`ScratchpadFormAction`] so the host
//! decides whether to debounce a save or to close.
//!
//! Saves go through the MCP client: a creation round-trip with
//! `scratchpad_create` on first save, then `scratchpad_update` for every
//! subsequent flush. The form never reads or writes the `.panopt/scratchpad/`
//! projection itself.
//!
//! The viewer polls [`Self::refresh_from_daemon`] on its `REFRESH` cadence so
//! a concurrent edit (another agent's `scratchpad_update`, a CLI write)
//! reconciles into the open form: untouched fields are replayed from the
//! remote value; fields the user is mid-edit on hold their local text and the
//! message line flags the conflict. The [`Baseline`] is always advanced to
//! the remote value so the next [`Self::flush`] sends only the user's still-
//! pending changes. Mirrors `todo_form.rs`'s refresh wiring (todo #40).

use std::time::Instant;

use anyhow::{anyhow, Result};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use serde_json::{json, Value};
use tui_textarea::{CursorMove, TextArea};

use crate::mcpclient::Client;
use crate::todo_form::{
    body_input, highlight_line, paste_into, paste_into_single_line, selected_text,
    single_line_input, text_area,
};

/// What [`ScratchpadForm::handle_key`] is telling the host to do next.
///
/// The viewer uses `Dirty` to start a debounce window and flush a short time
/// later; `Close` is the user's request to leave the form (Ctrl-C).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScratchpadFormAction {
    /// Nothing changed that the host needs to act on.
    Idle,
    /// A field changed: the host should consider this a pending save.
    Dirty,
    /// The user asked to close the form.
    Close,
}

/// Which scratchpad field currently has focus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    Title,
    Tags,
    Body,
}

/// Snapshot of the editable fields the daemon last reported, captured at load
/// time and after each successful save/refresh.
///
/// `flush` diffs the current field values against this baseline and sends
/// only the fields that actually changed - so an autosave that fires while
/// the user has only touched the title doesn't echo back the stale body and
/// clobber a concurrent edit. Mirrors `todo_form::Baseline`.
#[derive(Clone, Default)]
struct Baseline {
    title: String,
    body: String,
    tags: Vec<String>,
}

/// The editable state of the scratchpad form.
pub struct ScratchpadForm {
    /// The daemon MCP URL with `?ws=...&observer=1`.
    pub(crate) url: String,
    /// The scratchpad's id, or `None` until a new scratchpad is first saved.
    pub(crate) id: Option<u64>,

    title: TextArea<'static>,
    body: TextArea<'static>,
    /// Comma-separated, single-line tag input. Shares the project-wide tag
    /// vocabulary with todos (todo #61). Parsed the same way as the todo
    /// form's tag input: comma-split, trim, drop empties.
    tags: TextArea<'static>,
    focus: Field,

    /// Last-seen `created_at` / `updated_at` from the daemon, for the context
    /// line. Empty on a not-yet-saved form.
    created: String,
    updated: String,

    /// Display name of whoever holds an advisory lock on this scratchpad.
    /// Today `scratchpad_get` doesn't yet surface this; the field is kept in
    /// sync from the response anyway so the eventual scratchpad-lock surface
    /// (todo #55) has live data the moment the daemon starts emitting it.
    pub(crate) locked_by: Option<String>,

    /// True when there are unsaved edits.
    pub(crate) dirty: bool,
    /// When the first unsaved edit landed; used by the viewer to debounce.
    /// Cleared on a successful flush.
    pub(crate) dirty_since: Option<Instant>,

    /// Bottom-line feedback shown next to the help string.
    pub(crate) message: String,

    /// First visible visual row of the soft-wrapped Body field. Drives the
    /// `draw_body` scroll so the cursor stays on screen.
    body_scroll: usize,
    /// Visible row count of the Body field as of the most recent `draw_body`.
    /// Drives the half-page step for Ctrl-U / Ctrl-D in
    /// [`crate::todo_form::body_input`], same rationale as the todo form.
    body_view_height: usize,
    /// Screen rectangle the Body field occupies (inside its border). Captured
    /// each draw so click-to-position-cursor in [`ScratchpadForm::handle_mouse`]
    /// can map a click back to a logical cursor without re-deriving the
    /// layout. `None` until the first render lands.
    body_area: Option<Rect>,
    /// In-progress or completed mouse selection in the Body field:
    /// `(anchor, tip)` in logical `(row, col)` coordinates. Mirrors
    /// `TodoForm::selection`; see that field's doc for the lifecycle.
    selection: Option<((usize, usize), (usize, usize))>,

    /// The daemon's last-observed view of the editable fields. See
    /// [`Baseline`].
    baseline: Baseline,
}

impl ScratchpadForm {
    /// A blank form for a not-yet-created scratchpad.
    pub fn blank(url: &str) -> ScratchpadForm {
        ScratchpadForm {
            url: url.to_string(),
            id: None,
            title: text_area(""),
            body: text_area(""),
            tags: text_area(""),
            focus: Field::Title,
            created: String::new(),
            updated: String::new(),
            locked_by: None,
            dirty: false,
            dirty_since: None,
            body_scroll: 0,
            body_view_height: 0,
            body_area: None,
            selection: None,
            message: "new scratchpad - type to begin".to_string(),
            baseline: Baseline::default(),
        }
    }

    /// A form preloaded from an existing scratchpad's id, title, body, tags,
    /// and timestamps.
    pub fn from_parts(
        url: &str,
        id: u64,
        title: &str,
        body: &str,
        tags: &[String],
        created_at: &str,
        updated_at: &str,
    ) -> ScratchpadForm {
        ScratchpadForm {
            url: url.to_string(),
            id: Some(id),
            title: text_area(title),
            body: text_area(body),
            tags: text_area(&tags.join(", ")),
            focus: Field::Title,
            created: created_at.to_string(),
            updated: updated_at.to_string(),
            locked_by: None,
            dirty: false,
            dirty_since: None,
            body_scroll: 0,
            body_view_height: 0,
            body_area: None,
            selection: None,
            message: format!("scratchpad #{id}"),
            baseline: Baseline {
                title: title.to_string(),
                body: body.to_string(),
                tags: tags.to_vec(),
            },
        }
    }

    /// Handle one key press. The returned [`ScratchpadFormAction`] tells the
    /// host whether the form is dirty (start the debounce), idle, or asking
    /// to close.
    pub fn handle_key(&mut self, key: KeyEvent) -> ScratchpadFormAction {
        if key.kind != KeyEventKind::Press {
            return ScratchpadFormAction::Idle;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // Ctrl-Shift-C: copy current Body selection to the system clipboard.
        // Handled ahead of the Ctrl-C close arm so the shift modifier
        // disambiguates the two.
        if ctrl && shift && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
            if let Some((anchor, tip)) = self.selection {
                if anchor != tip {
                    let text = selected_text(self.body.lines(), anchor, tip);
                    let _ = crate::clip::emit_osc52(&text);
                }
            }
            return ScratchpadFormAction::Idle;
        }

        // Any other key: the visible selection is stale; clear before
        // dispatching so the highlight goes away on the next render.
        self.selection = None;

        match key.code {
            // Ctrl-C closes the form; q/x must remain typeable in the body.
            KeyCode::Char('c') if ctrl => ScratchpadFormAction::Close,
            // Tab cycles focus: Title -> Tags -> Body -> Title. BackTab walks
            // the same cycle in reverse so Shift-Tab is the inverse of Tab.
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Field::Title => Field::Tags,
                    Field::Tags => Field::Body,
                    Field::Body => Field::Title,
                };
                ScratchpadFormAction::Idle
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Field::Title => Field::Body,
                    Field::Tags => Field::Title,
                    Field::Body => Field::Tags,
                };
                ScratchpadFormAction::Idle
            }
            _ => self.field_key(key),
        }
    }

    fn field_key(&mut self, key: KeyEvent) -> ScratchpadFormAction {
        let changed = match self.focus {
            Field::Title => single_line_input(&mut self.title, key),
            Field::Tags => single_line_input(&mut self.tags, key),
            Field::Body => body_input(&mut self.body, key, self.body_view_height),
        };
        if changed {
            self.mark_dirty();
            ScratchpadFormAction::Dirty
        } else {
            ScratchpadFormAction::Idle
        }
    }

    /// Handle one mouse event in the Body field. Mirrors
    /// [`crate::todo_form::TodoForm::handle_mouse`]; see that for the full
    /// lifecycle (Down anchors a selection, Drag extends it, Up emits OSC 52,
    /// scroll wheel walks the cursor).
    pub fn handle_mouse(&mut self, m: MouseEvent) -> ScratchpadFormAction {
        let Some(area) = self.body_area else {
            return ScratchpadFormAction::Idle;
        };
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(pos) = self.body_logical_pos_at(area, m.row, m.column) else {
                    return ScratchpadFormAction::Idle;
                };
                self.focus = Field::Body;
                self.body
                    .move_cursor(CursorMove::Jump(pos.0 as u16, pos.1 as u16));
                self.selection = Some((pos, pos));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(pos) = self.body_logical_pos_at(area, m.row, m.column) {
                    if let Some((anchor, _)) = self.selection {
                        self.selection = Some((anchor, pos));
                        self.body
                            .move_cursor(CursorMove::Jump(pos.0 as u16, pos.1 as u16));
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some((anchor, tip)) = self.selection {
                    if anchor == tip {
                        self.selection = None;
                    } else {
                        let text = selected_text(self.body.lines(), anchor, tip);
                        let _ = crate::clip::emit_osc52(&text);
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
        ScratchpadFormAction::Idle
    }

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

    /// Insert a bracketed-paste payload into the focused field. Multi-line
    /// pastes into Title are flattened to spaces.
    pub fn handle_paste(&mut self, s: &str) -> ScratchpadFormAction {
        if s.is_empty() {
            return ScratchpadFormAction::Idle;
        }
        let changed = match self.focus {
            Field::Title => paste_into_single_line(&mut self.title, s),
            Field::Tags => paste_into_single_line(&mut self.tags, s),
            Field::Body => paste_into(&mut self.body, s),
        };
        if changed {
            self.mark_dirty();
            ScratchpadFormAction::Dirty
        } else {
            ScratchpadFormAction::Idle
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        if self.dirty_since.is_none() {
            self.dirty_since = Some(Instant::now());
        }
    }

    /// Whether the title is empty; saving is suppressed while it is.
    #[allow(dead_code)]
    pub fn title_is_empty(&self) -> bool {
        self.title.lines().join(" ").trim().is_empty()
    }

    /// Push the current title and body back to the daemon. Used by the
    /// viewer's debounced autosave.
    ///
    /// Creates the scratchpad first when this is a new form (no id yet) and
    /// the title is non-empty; otherwise updates an existing one in place.
    /// Diffs against [`Self::baseline`] so an idle field is omitted from
    /// `scratchpad_update` rather than echoed back stale.
    pub fn flush(&mut self) -> Result<()> {
        let title = self.current_title();
        // Suppress only the new-form case: `scratchpad_create` needs a non-
        // empty title, and an autosave on a blank-from-the-start form would
        // surface that as a spurious error in the message line. Once the
        // scratchpad exists, the user is allowed to clear its title - the
        // update path accepts an empty string. Without this distinction the
        // last non-empty intermediate state ("a" while the user is mid-delete
        // of "abc") would be the value the daemon and the sidebar see, and
        // the next refresh would replay that orphan character back into the
        // title field.
        if self.id.is_none() && title.is_empty() {
            return Ok(());
        }
        let body = self.current_body();
        let tags = self.current_tags();

        let client = Client::connect(&self.url)?;
        let outcome = (|| -> Result<()> {
            let id = match self.id {
                Some(id) => id,
                None => {
                    let created = client.call("scratchpad_create", json!({ "title": title }))?;
                    let id = created
                        .as_u64()
                        .ok_or_else(|| anyhow!("daemon returned no scratchpad id"))?;
                    self.id = Some(id);
                    // `scratchpad_create` accepts only `title`; the daemon
                    // initialises the body to "" and tags to []. Record both
                    // in the baseline so the diff below sends whichever body
                    // or tags the user typed in the new-scratchpad form.
                    self.baseline.title = title.clone();
                    id
                }
            };
            let mut payload = serde_json::Map::new();
            payload.insert("scratchpad_id".into(), json!(id));
            if title != self.baseline.title {
                payload.insert("title".into(), json!(title));
            }
            if body != self.baseline.body {
                payload.insert("body".into(), json!(body));
            }
            if tags != self.baseline.tags {
                payload.insert("tags".into(), json!(tags));
            }
            // Skip the round-trip when nothing changed - this is the common
            // shape of a debounced autosave fired by an unrelated event.
            if payload.len() > 1 {
                client.call("scratchpad_update", Value::Object(payload))?;
            }
            Ok(())
        })();
        client.close();
        outcome?;
        // Refresh the baseline so future flushes diff against what the daemon
        // now holds, not what was loaded an edit ago.
        self.baseline = Baseline { title, body, tags };
        self.dirty = false;
        self.dirty_since = None;
        self.message = format!("saved scratchpad #{}", self.id.unwrap_or(0));
        Ok(())
    }

    /// Pull the daemon's current snapshot and replay it onto the form. Per
    /// todo #40 / #65: untouched fields are replaced with the remote value;
    /// fields the user is mid-edit on keep their local text and the message
    /// line flags the conflict. The [`Baseline`] is always advanced so the
    /// next [`Self::flush`] sends only fields still divergent from the
    /// remote.
    ///
    /// Returns `Ok(true)` when anything visible changed and the host should
    /// redraw. Not-yet-saved forms (no id) return `Ok(false)` immediately -
    /// there is no daemon row to refresh against.
    pub fn refresh_from_daemon(&mut self) -> Result<bool> {
        let Some(id) = self.id else {
            return Ok(false);
        };
        let client = Client::connect(&self.url)?;
        let outcome = client.call("scratchpad_get", json!({ "scratchpad_id": id }));
        client.close();
        let pad = outcome?;

        let remote = Baseline {
            title: pad["title"].as_str().unwrap_or("").to_string(),
            body: pad["body"].as_str().unwrap_or("").to_string(),
            tags: pad["tags"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        };
        let remote_updated = pad["updated_at"].as_str().unwrap_or("").to_string();
        let remote_locked_by = pad["locked_by"].as_str().map(str::to_string);
        Ok(self.replay_remote(remote, remote_updated, remote_locked_by))
    }

    /// Apply a daemon snapshot. Pure of MCP - takes the already-loaded values
    /// directly - so the replay rules can be unit-tested. See
    /// [`Self::refresh_from_daemon`] for the surrounding wire-up.
    fn replay_remote(
        &mut self,
        remote: Baseline,
        remote_updated: String,
        remote_locked_by: Option<String>,
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
        if remote.tags != self.baseline.tags {
            if self.current_tags() != self.baseline.tags {
                conflicts.push("tags");
            } else {
                self.tags = text_area(&remote.tags.join(", "));
                changed = true;
            }
        }

        if self.updated != remote_updated {
            self.updated = remote_updated;
            changed = true;
        }
        if self.locked_by != remote_locked_by {
            self.locked_by = remote_locked_by;
            changed = true;
        }

        // Advance the baseline unconditionally so a subsequent flush only
        // sends fields the user is still mid-edit on.
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

    /// The tag list parsed from the tags input: comma-split, trim, drop empty.
    /// A multi-line paste is normalised to commas first so the result is order-
    /// preserving and matches whatever the user sees on screen. Mirrors the
    /// todo form's tag parsing.
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

    /// Render the form into `area`.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        // Rows: title / tags / body / context / message. The pane title (set
        // by the Zellij sidebar) already names the scratchpad, so no in-form
        // header row is needed; locked-by, when set, surfaces in the message
        // line at the bottom. Mirrors the todo form's #53 cleanup.
        let rows = Layout::vertical([
            Constraint::Length(3), // title
            Constraint::Length(3), // tags
            Constraint::Min(3),    // body
            Constraint::Length(1), // context (created/updated)
            Constraint::Length(1), // message + help
        ])
        .split(area);

        self.style_field(Field::Title, "Title");
        frame.render_widget(&self.title, rows[0]);
        self.style_field(Field::Tags, "Tags");
        frame.render_widget(&self.tags, rows[1]);
        self.draw_body(frame, rows[2]);

        let context = if !self.created.is_empty() {
            format!(" created {}   updated {}", self.created, self.updated)
        } else {
            String::new()
        };
        frame.render_widget(
            Paragraph::new(context).style(Style::default().fg(Color::DarkGray)),
            rows[3],
        );

        let help = "Tab field  Ctrl-C close";
        let lock_prefix = self
            .locked_by
            .as_deref()
            .map(|h| format!("[locked by {h}]   "))
            .unwrap_or_default();
        let line = if self.message.is_empty() {
            format!(" {lock_prefix}{help}")
        } else {
            format!(" {lock_prefix}{}   |   {help}", self.message)
        };
        let line_style = if self.locked_by.is_some() {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Yellow)
        };
        frame.render_widget(Paragraph::new(line).style(line_style), rows[4]);
    }

    /// Render the Body field with our soft-wrap renderer. See
    /// [`crate::todo_form::TodoForm::draw_body`] for the rationale - the same
    /// `tui_textarea` limitation drove this widget here.
    fn draw_body(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Field::Body;
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
        // Same purpose as the matching line in todo_form: feeds the half-page
        // step for Ctrl-U / Ctrl-D paging in `body_input`.
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

        if cvr < self.body_scroll {
            self.body_scroll = cvr;
        } else if cvr >= self.body_scroll + height {
            self.body_scroll = cvr + 1 - height;
        }
        let max_scroll = wrapped.lines.len().saturating_sub(height);
        if self.body_scroll > max_scroll {
            self.body_scroll = max_scroll;
        }

        let selection_ranges: Vec<(usize, usize, usize)> = match self.selection {
            Some((a, t)) => wrapped.visual_selection_ranges(a, t),
            None => Vec::new(),
        };
        let visible: Vec<ratatui::text::Line> = wrapped
            .lines
            .iter()
            .enumerate()
            .skip(self.body_scroll)
            .take(height)
            .map(|(vrow, l)| {
                if let Some(&(_, from, to)) = selection_ranges.iter().find(|(r, _, _)| *r == vrow) {
                    highlight_line(l, from, to)
                } else {
                    ratatui::text::Line::from(l.clone())
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

    /// Set a text field's border and cursor styling for the current focus.
    /// Mirrors [`crate::todo_form`]'s helper of the same name. Body is
    /// excluded - it has its own wrapped render in [`Self::draw_body`].
    fn style_field(&mut self, field: Field, label: &'static str) {
        let focused = self.focus == field;
        let area = match field {
            Field::Title => &mut self.title,
            Field::Tags => &mut self.tags,
            Field::Body => return,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_form_starts_focused_on_title_with_no_id() {
        let form = ScratchpadForm::blank("http://localhost/?ws=/x");
        assert_eq!(form.id, None);
        assert_eq!(form.focus, Field::Title);
        assert!(form.title_is_empty());
        assert!(!form.dirty);
    }

    #[test]
    fn from_parts_preloads_id_title_body_and_tags() {
        let form = ScratchpadForm::from_parts(
            "http://localhost/?ws=/x",
            7,
            "notes",
            "first\nsecond",
            &["a".into(), "b".into()],
            "2026-05-23 04:36:21",
            "2026-05-23 04:36:21",
        );
        assert_eq!(form.id, Some(7));
        assert_eq!(form.title.lines().join(" "), "notes");
        assert_eq!(form.body.lines().join("\n"), "first\nsecond");
        assert_eq!(form.tags.lines().join(""), "a, b");
        assert_eq!(form.current_tags(), vec!["a".to_string(), "b".to_string()]);
        assert!(!form.dirty);
    }

    #[test]
    fn tab_cycles_focus_title_tags_body() {
        let mut form = ScratchpadForm::blank("http://localhost/?ws=/x");
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::empty());
        assert_eq!(form.handle_key(tab), ScratchpadFormAction::Idle);
        assert_eq!(form.focus, Field::Tags);
        assert_eq!(form.handle_key(tab), ScratchpadFormAction::Idle);
        assert_eq!(form.focus, Field::Body);
        assert_eq!(form.handle_key(tab), ScratchpadFormAction::Idle);
        assert_eq!(form.focus, Field::Title);
    }

    #[test]
    fn ctrl_c_returns_close() {
        let mut form = ScratchpadForm::blank("http://localhost/?ws=/x");
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(form.handle_key(ctrl_c), ScratchpadFormAction::Close);
    }

    #[test]
    fn typing_marks_dirty_and_starts_debounce_window() {
        let mut form = ScratchpadForm::blank("http://localhost/?ws=/x");
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty());
        assert_eq!(form.handle_key(key), ScratchpadFormAction::Dirty);
        assert!(form.dirty);
        assert!(form.dirty_since.is_some());
    }

    /// Regression: a multi-line paste into Body must land intact. Before the
    /// fix every `\n` between lines arrived as Ctrl-J and `tui_textarea`
    /// silently wiped the line under the cursor.
    #[test]
    fn handle_paste_into_body_preserves_lines() {
        let mut form = ScratchpadForm::blank("http://localhost/?ws=/x");
        // Tab past Tags to Body (cycle is Title -> Tags -> Body).
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::empty());
        form.handle_key(tab);
        form.handle_key(tab);
        assert_eq!(form.focus, Field::Body);
        let action = form.handle_paste("alpha\nbeta\ngamma");
        assert_eq!(action, ScratchpadFormAction::Dirty);
        assert_eq!(form.body.lines(), vec!["alpha", "beta", "gamma"]);
    }

    /// An untouched field picks up the remote value on refresh.
    #[test]
    fn refresh_replays_remote_into_untouched_fields() {
        let mut form = ScratchpadForm::from_parts(
            "http://localhost/?ws=/x",
            1,
            "old title",
            "old body",
            &[],
            "2026-05-27 00:00:00",
            "2026-05-27 00:00:00",
        );
        let changed = form.replay_remote(
            Baseline {
                title: "new title".to_string(),
                body: "new body".to_string(),
                tags: vec!["x".into()],
            },
            "2026-05-27 00:00:05".to_string(),
            None,
        );
        assert!(changed);
        assert_eq!(form.current_title(), "new title");
        assert_eq!(form.current_body(), "new body");
        assert_eq!(form.current_tags(), vec!["x".to_string()]);
        assert_eq!(form.updated, "2026-05-27 00:00:05");
        assert!(!form.message.contains("overwrite"));
    }

    /// A locally edited field is kept and the message line flags the
    /// conflict; the baseline still advances so the next flush only sends
    /// the still-pending change.
    #[test]
    fn refresh_keeps_local_edit_and_flags_conflict() {
        let mut form = ScratchpadForm::from_parts(
            "http://localhost/?ws=/x",
            1,
            "old title",
            "old body",
            &[],
            "2026-05-27 00:00:00",
            "2026-05-27 00:00:00",
        );
        // Local edit to the title.
        let key = KeyEvent::new(KeyCode::Char('!'), KeyModifiers::empty());
        form.handle_key(key);
        // Remote also changed title and body.
        let changed = form.replay_remote(
            Baseline {
                title: "remote title".to_string(),
                body: "remote body".to_string(),
                tags: vec![],
            },
            "2026-05-27 00:00:05".to_string(),
            None,
        );
        assert!(changed);
        // Title was being edited - local wins (cursor at the start, so the
        // typed char goes at column 0).
        assert!(form.current_title().contains("old title"));
        assert_ne!(form.current_title(), "old title");
        // Body wasn't edited - replayed from the remote.
        assert_eq!(form.current_body(), "remote body");
        assert!(form.message.contains("title"));
        assert!(form.message.contains("overwrite"));
        // Baseline advances to the remote view regardless.
        assert_eq!(form.baseline.title, "remote title");
        assert_eq!(form.baseline.body, "remote body");
    }

    /// A no-op refresh (remote matches baseline) reports no visible change.
    #[test]
    fn refresh_with_no_remote_drift_is_a_noop() {
        let mut form = ScratchpadForm::from_parts(
            "http://localhost/?ws=/x",
            1,
            "t",
            "b",
            &[],
            "2026-05-27 00:00:00",
            "2026-05-27 00:00:00",
        );
        let changed = form.replay_remote(
            Baseline {
                title: "t".to_string(),
                body: "b".to_string(),
                tags: vec![],
            },
            "2026-05-27 00:00:00".to_string(),
            None,
        );
        assert!(!changed);
    }

    /// The lock-holder field follows the remote value so the future
    /// scratchpad-lock surface (todo #55) has live data.
    #[test]
    fn refresh_updates_locked_by_from_remote() {
        let mut form = ScratchpadForm::from_parts(
            "http://localhost/?ws=/x",
            1,
            "t",
            "b",
            &[],
            "2026-05-27 00:00:00",
            "2026-05-27 00:00:00",
        );
        let changed = form.replay_remote(
            Baseline {
                title: "t".to_string(),
                body: "b".to_string(),
                tags: vec![],
            },
            "2026-05-27 00:00:00".to_string(),
            Some("alice".to_string()),
        );
        assert!(changed);
        assert_eq!(form.locked_by.as_deref(), Some("alice"));
    }

    /// A local tag edit conflicts with a remote tag change; an untouched tag
    /// field instead picks up the remote value.
    #[test]
    fn refresh_handles_tags_drift_with_conflict_detection() {
        let mut form = ScratchpadForm::from_parts(
            "http://localhost/?ws=/x",
            1,
            "t",
            "b",
            &["a".into()],
            "2026-05-27 00:00:00",
            "2026-05-27 00:00:00",
        );
        // No local edit: remote tags should replace ours and not conflict.
        let changed = form.replay_remote(
            Baseline {
                title: "t".to_string(),
                body: "b".to_string(),
                tags: vec!["a".into(), "b".into()],
            },
            "2026-05-27 00:00:00".to_string(),
            None,
        );
        assert!(changed);
        assert_eq!(form.current_tags(), vec!["a".to_string(), "b".to_string()]);
        assert!(!form.message.contains("overwrite"));
    }
}
