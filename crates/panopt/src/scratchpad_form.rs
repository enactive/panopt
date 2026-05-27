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
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use serde_json::{json, Value};
use tui_textarea::TextArea;

use crate::mcpclient::Client;
use crate::todo_form::{
    paste_into, paste_into_single_line, single_line_input, text_area, text_input,
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

/// Which of the two scratchpad fields currently has focus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    Title,
    Body,
}

/// Snapshot of the title and body the daemon last reported, captured at load
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
}

/// The editable state of the scratchpad form.
pub struct ScratchpadForm {
    /// The daemon MCP URL with `?ws=...&observer=1`.
    pub(crate) url: String,
    /// The scratchpad's id, or `None` until a new scratchpad is first saved.
    pub(crate) id: Option<u64>,

    title: TextArea<'static>,
    body: TextArea<'static>,
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
            focus: Field::Title,
            created: String::new(),
            updated: String::new(),
            locked_by: None,
            dirty: false,
            dirty_since: None,
            body_scroll: 0,
            message: "new scratchpad - type to begin".to_string(),
            baseline: Baseline::default(),
        }
    }

    /// A form preloaded from an existing scratchpad's id, title, body, and
    /// timestamps.
    pub fn from_parts(
        url: &str,
        id: u64,
        title: &str,
        body: &str,
        created_at: &str,
        updated_at: &str,
    ) -> ScratchpadForm {
        ScratchpadForm {
            url: url.to_string(),
            id: Some(id),
            title: text_area(title),
            body: text_area(body),
            focus: Field::Title,
            created: created_at.to_string(),
            updated: updated_at.to_string(),
            locked_by: None,
            dirty: false,
            dirty_since: None,
            body_scroll: 0,
            message: format!("scratchpad #{id}"),
            baseline: Baseline {
                title: title.to_string(),
                body: body.to_string(),
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
        match key.code {
            // Ctrl-C closes the form; q/x must remain typeable in the body.
            KeyCode::Char('c') if ctrl => ScratchpadFormAction::Close,
            // Tab cycles focus between title and body.
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Field::Title => Field::Body,
                    Field::Body => Field::Title,
                };
                ScratchpadFormAction::Idle
            }
            _ => self.field_key(key),
        }
    }

    fn field_key(&mut self, key: KeyEvent) -> ScratchpadFormAction {
        let changed = match self.focus {
            Field::Title => single_line_input(&mut self.title, key),
            Field::Body => text_input(&mut self.body, key),
        };
        if changed {
            self.mark_dirty();
            ScratchpadFormAction::Dirty
        } else {
            ScratchpadFormAction::Idle
        }
    }

    /// Insert a bracketed-paste payload into the focused field. Multi-line
    /// pastes into Title are flattened to spaces.
    pub fn handle_paste(&mut self, s: &str) -> ScratchpadFormAction {
        if s.is_empty() {
            return ScratchpadFormAction::Idle;
        }
        let changed = match self.focus {
            Field::Title => paste_into_single_line(&mut self.title, s),
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
        if title.is_empty() {
            // Nothing to save against - silently no-op so an autosave on an
            // empty new form does not spam errors.
            return Ok(());
        }
        let body = self.current_body();

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
                    // initialises the body to "". Record that in the baseline
                    // so the diff below sends whichever body the user typed
                    // in the new-scratchpad form.
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
        self.baseline = Baseline { title, body };
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

    /// Render the form into `area`.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        // Rows: header / title / body / context / message.
        let rows = Layout::vertical([
            Constraint::Length(1), // header (incl. locked-by banner)
            Constraint::Length(3), // title
            Constraint::Min(3),    // body
            Constraint::Length(1), // context (created/updated)
            Constraint::Length(1), // message + help
        ])
        .split(area);

        let header_text = match (&self.id, &self.locked_by) {
            (Some(id), Some(holder)) => format!(" Edit scratchpad #{id}   [locked by {holder}]"),
            (Some(id), None) => format!(" Edit scratchpad #{id}"),
            (None, _) => " New scratchpad".to_string(),
        };
        let header_style = if self.locked_by.is_some() {
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Red)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        frame.render_widget(Paragraph::new(header_text).style(header_style), rows[0]);

        self.style_field(Field::Title, "Title");
        frame.render_widget(&self.title, rows[1]);
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
        let line = if self.message.is_empty() {
            format!(" {help}")
        } else {
            format!(" {}   |   {help}", self.message)
        };
        frame.render_widget(
            Paragraph::new(line).style(Style::default().fg(Color::Yellow)),
            rows[4],
        );
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

        let visible: Vec<ratatui::text::Line> = wrapped
            .lines
            .iter()
            .skip(self.body_scroll)
            .take(height)
            .map(|l| ratatui::text::Line::from(l.clone()))
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
    fn from_parts_preloads_id_title_and_body() {
        let form = ScratchpadForm::from_parts(
            "http://localhost/?ws=/x",
            7,
            "notes",
            "first\nsecond",
            "2026-05-23 04:36:21",
            "2026-05-23 04:36:21",
        );
        assert_eq!(form.id, Some(7));
        assert_eq!(form.title.lines().join(" "), "notes");
        assert_eq!(form.body.lines().join("\n"), "first\nsecond");
        assert!(!form.dirty);
    }

    #[test]
    fn tab_cycles_focus_between_title_and_body() {
        let mut form = ScratchpadForm::blank("http://localhost/?ws=/x");
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::empty());
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
        // Tab to Body.
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::empty());
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
            "2026-05-27 00:00:00",
            "2026-05-27 00:00:00",
        );
        let changed = form.replay_remote(
            Baseline {
                title: "new title".to_string(),
                body: "new body".to_string(),
            },
            "2026-05-27 00:00:05".to_string(),
            None,
        );
        assert!(changed);
        assert_eq!(form.current_title(), "new title");
        assert_eq!(form.current_body(), "new body");
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
            "2026-05-27 00:00:00",
            "2026-05-27 00:00:00",
        );
        let changed = form.replay_remote(
            Baseline {
                title: "t".to_string(),
                body: "b".to_string(),
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
            "2026-05-27 00:00:00",
            "2026-05-27 00:00:00",
        );
        let changed = form.replay_remote(
            Baseline {
                title: "t".to_string(),
                body: "b".to_string(),
            },
            "2026-05-27 00:00:00".to_string(),
            Some("alice".to_string()),
        );
        assert!(changed);
        assert_eq!(form.locked_by.as_deref(), Some("alice"));
    }
}
