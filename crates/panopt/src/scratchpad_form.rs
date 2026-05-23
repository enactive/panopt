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

use std::time::Instant;

use anyhow::{anyhow, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use serde_json::json;
use tui_textarea::TextArea;

use crate::mcpclient::Client;
use crate::todo_form::{single_line_input, text_area};

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
    /// Reserved for the eventual scratchpad-lock surface; today the viewer is
    /// observer-only and leaves this as `None`.
    pub(crate) locked_by: Option<String>,

    /// True when there are unsaved edits.
    pub(crate) dirty: bool,
    /// When the first unsaved edit landed; used by the viewer to debounce.
    /// Cleared on a successful flush.
    pub(crate) dirty_since: Option<Instant>,

    /// Bottom-line feedback shown next to the help string.
    pub(crate) message: String,
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
            message: "new scratchpad - type to begin".to_string(),
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
            message: format!("scratchpad #{id}"),
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
            Field::Body => self.body.input(key),
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
    pub fn flush(&mut self) -> Result<()> {
        let title = self.title.lines().join(" ").trim().to_string();
        if title.is_empty() {
            // Nothing to save against - silently no-op so an autosave on an
            // empty new form does not spam errors.
            return Ok(());
        }
        let body = self.body.lines().join("\n");

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
                    id
                }
            };
            client.call(
                "scratchpad_update",
                json!({
                    "scratchpad_id": id,
                    "title": title,
                    "body": body,
                }),
            )?;
            Ok(())
        })();
        client.close();
        outcome?;
        self.dirty = false;
        self.dirty_since = None;
        self.message = format!("saved scratchpad #{}", self.id.unwrap_or(0));
        Ok(())
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
        self.style_field(Field::Body, "Body");
        frame.render_widget(&self.body, rows[2]);

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

    /// Set a text field's border and cursor styling for the current focus.
    /// Mirrors [`crate::todo_form`]'s helper of the same name.
    fn style_field(&mut self, field: Field, label: &'static str) {
        let focused = self.focus == field;
        let area = match field {
            Field::Title => &mut self.title,
            Field::Body => &mut self.body,
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
}
