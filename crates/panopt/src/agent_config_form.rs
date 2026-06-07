//! The editable agent-config form, hosted in-pane by the cockpit's `_viewer`.
//!
//! Sibling of [`crate::todo_form`] / [`crate::note_form`], shaped to the
//! durable agent-config (slot) model (todo #140): the single-line `name`,
//! `display_name`, `command`, and `cwd` fields, a `tool_type` chosen from the
//! loaded agent-type profiles, a toggleable `enabled` flag, and a multi-line
//! `system_prompt` body. The config is the *what to launch* half of the
//! two-layer process model (#27); editing it never touches a running instance.
//!
//! Saves go through the MCP client: `agent_tool_create` on first save (once the
//! name is non-empty), then `agent_tool_update` for every later flush, diffing
//! against the [`Baseline`] so an autosave only sends fields that actually
//! changed. The viewer polls [`Self::refresh_from_daemon`] so a concurrent edit
//! (another agent's `agent_tool_update`, a CLI `agent-tool set`) reconciles into
//! the open form, exactly as the todo/note forms do (todo #40 / #65).
//!
//! The `tool_type` dropdown is populated from the profile registry the viewer
//! loads via [`panopt_core::agent_profiles::ProfileSet`]; cycling it can only
//! ever land on a known profile key, so an invalid type can never be submitted.

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
    body_input, cycle_dir, enum_line, field_border_color, highlight_line, index_of, paste_into,
    paste_into_single_line, select_to_column, selected_text, single_line_input, text_area, wrap,
};

/// What [`AgentConfigForm::handle_key`] is telling the host to do next. Same
/// contract as the todo/note form actions: `Dirty` opens a debounce window,
/// `Close` is the user's Ctrl-C, `Idle` is a no-op for the host.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentConfigFormAction {
    Idle,
    Dirty,
    Close,
}

/// Which config field currently has focus, in Tab order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    Name,
    Display,
    Type,
    Enabled,
    Command,
    Cwd,
    Prompt,
}

/// Tab order for [`Field`]; `Tab` walks it forward, `BackTab` in reverse.
const FIELDS: [Field; 7] = [
    Field::Name,
    Field::Display,
    Field::Type,
    Field::Enabled,
    Field::Command,
    Field::Cwd,
    Field::Prompt,
];

/// Snapshot of the editable fields the daemon last reported, captured at load
/// time and after each successful save/refresh. `flush` diffs the current
/// values against this so an autosave only echoes the fields the user touched.
/// Mirrors `note_form::Baseline`.
#[derive(Clone, Default)]
struct Baseline {
    name: String,
    display_name: String,
    command: String,
    cwd: String,
    tool_type: String,
    system_prompt: String,
    enabled: bool,
}

/// The editable state of the agent-config form.
pub struct AgentConfigForm {
    /// The daemon MCP URL with `?ws=...&observer=1`.
    pub(crate) url: String,
    /// The config's id, or `None` until a new config is first saved.
    pub(crate) id: Option<u64>,

    name: TextArea<'static>,
    display: TextArea<'static>,
    command: TextArea<'static>,
    cwd: TextArea<'static>,
    /// Multi-line system prompt, edited with the same soft-wrap body widget the
    /// note form uses.
    prompt: TextArea<'static>,

    /// The known `tool_type` keys, sorted, populated from the profile registry.
    /// Cycling `type_idx` over this is the only way to set the type, so the
    /// value is always a known profile.
    type_options: Vec<String>,
    type_idx: usize,
    enabled: bool,
    focus: Field,

    /// `created_at` from the daemon, for the context line. Empty on a
    /// not-yet-saved form. Configs carry no `updated_at`.
    created: String,

    pub(crate) dirty: bool,
    pub(crate) dirty_since: Option<Instant>,
    pub(crate) message: String,

    /// Body-field render state for the system-prompt field, identical in role to
    /// the note form's body machinery.
    body_scroll: usize,
    body_view_height: usize,
    body_area: Option<Rect>,
    selection: Option<((usize, usize), (usize, usize))>,
    field_areas: Vec<(Field, Rect)>,
    text_field_areas: Vec<(Field, Rect)>,

    baseline: Baseline,
}

impl AgentConfigForm {
    /// A blank form for a not-yet-created config. `type_options` is the loaded
    /// profile-key list; the type defaults to `default_type` when present, else
    /// the first option.
    pub fn blank(url: &str, type_options: Vec<String>, default_type: &str) -> AgentConfigForm {
        let type_idx = type_options
            .iter()
            .position(|t| t == default_type)
            .unwrap_or(0);
        let chosen_type = type_options.get(type_idx).cloned().unwrap_or_default();
        AgentConfigForm {
            url: url.to_string(),
            id: None,
            name: text_area(""),
            display: text_area(""),
            command: text_area(""),
            cwd: text_area(""),
            prompt: text_area(""),
            type_options,
            type_idx,
            enabled: true,
            focus: Field::Name,
            created: String::new(),
            dirty: false,
            dirty_since: None,
            message: "new agent config - name it to begin".to_string(),
            body_scroll: 0,
            body_view_height: 0,
            body_area: None,
            selection: None,
            field_areas: Vec::new(),
            text_field_areas: Vec::new(),
            baseline: Baseline {
                tool_type: chosen_type,
                enabled: true,
                ..Baseline::default()
            },
        }
    }

    /// A form preloaded from an existing config. `type_options` is the loaded
    /// profile-key list; if `tool_type` is not among them (a stale or unknown
    /// type) it is appended so the field still displays and round-trips it.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        url: &str,
        id: u64,
        name: &str,
        display_name: &str,
        command: &str,
        cwd: &str,
        tool_type: &str,
        system_prompt: &str,
        enabled: bool,
        created_at: &str,
        mut type_options: Vec<String>,
    ) -> AgentConfigForm {
        if !type_options.iter().any(|t| t == tool_type) {
            type_options.push(tool_type.to_string());
        }
        let type_idx = type_options
            .iter()
            .position(|t| t == tool_type)
            .unwrap_or(0);
        AgentConfigForm {
            url: url.to_string(),
            id: Some(id),
            name: text_area(name),
            display: text_area(display_name),
            command: text_area(command),
            cwd: text_area(cwd),
            prompt: text_area(system_prompt),
            type_options,
            type_idx,
            enabled,
            focus: Field::Name,
            created: created_at.to_string(),
            dirty: false,
            dirty_since: None,
            message: format!("agent config #{id}"),
            body_scroll: 0,
            body_view_height: 0,
            body_area: None,
            selection: None,
            field_areas: Vec::new(),
            text_field_areas: Vec::new(),
            baseline: Baseline {
                name: name.to_string(),
                display_name: display_name.to_string(),
                command: command.to_string(),
                cwd: cwd.to_string(),
                tool_type: tool_type.to_string(),
                system_prompt: system_prompt.to_string(),
                enabled,
            },
        }
    }

    /// Handle one key press. The returned [`AgentConfigFormAction`] tells the
    /// host whether to debounce a save, close, or do nothing.
    pub fn handle_key(&mut self, key: KeyEvent) -> AgentConfigFormAction {
        if key.kind != KeyEventKind::Press {
            return AgentConfigFormAction::Idle;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // Ctrl-Shift-C copies the current selection (the prompt body's
        // wrap-aware selection, else a single-line field's own range). Handled
        // ahead of the Ctrl-C close arm so the shift modifier disambiguates.
        if ctrl && shift && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
            self.copy_selection();
            return AgentConfigFormAction::Idle;
        }

        // Any other key: the painted selection is stale; clear it.
        self.selection = None;

        match key.code {
            KeyCode::Char('c') if ctrl => AgentConfigFormAction::Close,
            KeyCode::Tab => {
                self.focus = next_field(self.focus, true);
                AgentConfigFormAction::Idle
            }
            KeyCode::BackTab => {
                self.focus = next_field(self.focus, false);
                AgentConfigFormAction::Idle
            }
            _ => self.field_key(key),
        }
    }

    fn field_key(&mut self, key: KeyEvent) -> AgentConfigFormAction {
        let changed = match self.focus {
            Field::Name => single_line_input(&mut self.name, key),
            Field::Display => single_line_input(&mut self.display, key),
            Field::Command => single_line_input(&mut self.command, key),
            Field::Cwd => single_line_input(&mut self.cwd, key),
            Field::Prompt => body_input(&mut self.prompt, key, self.body_view_height),
            Field::Type => {
                if let Some(dir) = cycle_dir(key.code) {
                    self.type_idx = wrap(self.type_idx, dir, self.type_options.len().max(1));
                    true
                } else {
                    false
                }
            }
            // Enabled toggles on Left/Right (cycle) or Space, matching the
            // enum fields' Left/Right idiom while keeping a discoverable Space.
            Field::Enabled => {
                if cycle_dir(key.code).is_some() || matches!(key.code, KeyCode::Char(' ')) {
                    self.enabled = !self.enabled;
                    true
                } else {
                    false
                }
            }
        };
        if changed {
            self.mark_dirty();
            AgentConfigFormAction::Dirty
        } else {
            AgentConfigFormAction::Idle
        }
    }

    /// Copy the current selection to the system clipboard via OSC 52. The
    /// prompt body uses the wrap-aware `self.selection`; single-line fields keep
    /// their own `selection_range()`.
    fn copy_selection(&mut self) {
        if self.focus == Field::Prompt {
            if let Some((anchor, tip)) = self.selection {
                if anchor != tip {
                    let text = selected_text(self.prompt.lines(), anchor, tip);
                    if !text.is_empty() {
                        let _ = crate::clip::copy_to_clipboard(&text);
                    }
                    return;
                }
            }
        }
        if let Some(area) = self.single_line_textarea_mut(self.focus) {
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

    /// Handle one mouse event. The prompt body uses the note form's wrap-aware
    /// selection path; the single-line fields use click-to-position and
    /// drag-to-select; the enum/bool rows just take focus on click.
    pub fn handle_mouse(&mut self, m: MouseEvent) -> AgentConfigFormAction {
        let body_area = self.body_area;
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(area) = body_area {
                    if let Some(pos) = self.body_logical_pos_at(area, m.row, m.column) {
                        self.focus = Field::Prompt;
                        self.prompt
                            .move_cursor(CursorMove::Jump(pos.0 as u16, pos.1 as u16));
                        self.selection = Some((pos, pos));
                        return AgentConfigFormAction::Idle;
                    }
                }
                if let Some((field, inner)) = self.text_field_at(m.row, m.column) {
                    self.focus = field;
                    if let Some(area) = self.single_line_textarea_mut(field) {
                        let col = m.column.saturating_sub(inner.x) as usize;
                        area.cancel_selection();
                        area.move_cursor(CursorMove::Jump(0, col as u16));
                        area.start_selection();
                    }
                    self.selection = None;
                    return AgentConfigFormAction::Idle;
                }
                if let Some(field) = self.field_at(m.row, m.column) {
                    self.focus = field;
                    self.selection = None;
                }
                AgentConfigFormAction::Idle
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(area) = body_area {
                    if let Some(pos) = self.body_logical_pos_at(area, m.row, m.column) {
                        if let Some((anchor, _)) = self.selection {
                            self.selection = Some((anchor, pos));
                            self.prompt
                                .move_cursor(CursorMove::Jump(pos.0 as u16, pos.1 as u16));
                            return AgentConfigFormAction::Idle;
                        }
                    }
                }
                let focused = self.focus;
                if let Some(inner) = self.text_field_inner(focused) {
                    let target = m.column.saturating_sub(inner.x) as usize;
                    if let Some(area) = self.single_line_textarea_mut(focused) {
                        select_to_column(area, target);
                    }
                }
                AgentConfigFormAction::Idle
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some((anchor, tip)) = self.selection {
                    if anchor == tip {
                        self.selection = None;
                    } else {
                        let text = selected_text(self.prompt.lines(), anchor, tip);
                        if !text.is_empty() {
                            let _ = crate::clip::copy_to_clipboard(&text);
                        }
                    }
                    return AgentConfigFormAction::Idle;
                }
                let focused = self.focus;
                if let Some(area) = self.single_line_textarea_mut(focused) {
                    if let Some((anchor, tip)) = area.selection_range() {
                        if anchor != tip {
                            let text = selected_text(area.lines(), anchor, tip);
                            if !text.is_empty() {
                                let _ = crate::clip::copy_to_clipboard(&text);
                            }
                        }
                    }
                }
                AgentConfigFormAction::Idle
            }
            MouseEventKind::ScrollUp => {
                self.prompt.move_cursor(CursorMove::Up);
                AgentConfigFormAction::Idle
            }
            MouseEventKind::ScrollDown => {
                self.prompt.move_cursor(CursorMove::Down);
                AgentConfigFormAction::Idle
            }
            _ => AgentConfigFormAction::Idle,
        }
    }

    fn text_field_at(&self, row: u16, col: u16) -> Option<(Field, Rect)> {
        self.text_field_areas
            .iter()
            .find(|(_, rect)| within(*rect, row, col))
            .map(|(f, r)| (*f, *r))
    }

    fn text_field_inner(&self, field: Field) -> Option<Rect> {
        self.text_field_areas
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, r)| *r)
    }

    fn single_line_textarea_mut(&mut self, field: Field) -> Option<&mut TextArea<'static>> {
        match field {
            Field::Name => Some(&mut self.name),
            Field::Display => Some(&mut self.display),
            Field::Command => Some(&mut self.command),
            Field::Cwd => Some(&mut self.cwd),
            _ => None,
        }
    }

    fn field_at(&self, row: u16, col: u16) -> Option<Field> {
        self.field_areas
            .iter()
            .find(|(_, rect)| within(*rect, row, col))
            .map(|(f, _)| *f)
    }

    fn body_logical_pos_at(&self, area: Rect, row: u16, col: u16) -> Option<(usize, usize)> {
        if !within(area, row, col) {
            return None;
        }
        let width = area.width as usize;
        let visual_row = (row - area.y) as usize + self.body_scroll;
        let visual_col = (col - area.x) as usize;
        let wrapped =
            crate::wrap::wrap_for_display(self.prompt.lines(), self.prompt.cursor(), width);
        Some(wrapped.visual_to_logical(visual_row, visual_col))
    }

    /// Insert a bracketed-paste payload into the focused field. Multi-line
    /// pastes into a single-line field are flattened to spaces; the enum/bool
    /// fields ignore pastes.
    pub fn handle_paste(&mut self, s: &str) -> AgentConfigFormAction {
        if s.is_empty() {
            return AgentConfigFormAction::Idle;
        }
        let changed = match self.focus {
            Field::Name => paste_into_single_line(&mut self.name, s),
            Field::Display => paste_into_single_line(&mut self.display, s),
            Field::Command => paste_into_single_line(&mut self.command, s),
            Field::Cwd => paste_into_single_line(&mut self.cwd, s),
            Field::Prompt => paste_into(&mut self.prompt, s),
            Field::Type | Field::Enabled => false,
        };
        if changed {
            self.mark_dirty();
            AgentConfigFormAction::Dirty
        } else {
            AgentConfigFormAction::Idle
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        if self.dirty_since.is_none() {
            self.dirty_since = Some(Instant::now());
        }
    }

    /// Whether the name is empty; creation is suppressed while it is.
    #[allow(dead_code)]
    pub fn name_is_empty(&self) -> bool {
        self.current_name().is_empty()
    }

    /// Push the current fields back to the daemon. Creates the config first when
    /// this is a new form (no id yet) with a non-empty name; otherwise updates
    /// the existing one in place, diffing against [`Self::baseline`] so an idle
    /// field is omitted from `agent_tool_update`.
    pub fn flush(&mut self) -> Result<()> {
        let name = self.current_name();
        // Suppress the new-form case while the name is blank: `agent_tool_create`
        // needs a name, and an autosave on a still-empty form would surface a
        // spurious error. Once the config exists the name can be edited freely.
        if self.id.is_none() && name.is_empty() {
            return Ok(());
        }
        let snapshot = self.snapshot();

        let client = Client::connect(&self.url)?;
        let outcome = (|| -> Result<()> {
            match self.id {
                None => {
                    // `agent_tool_create` accepts every field, so send the whole
                    // snapshot in one shot and adopt it as the baseline.
                    let created = client.call(
                        "agent_tool_create",
                        json!({
                            "name": snapshot.name,
                            "display_name": snapshot.display_name,
                            "command": snapshot.command,
                            "cwd": snapshot.cwd,
                            "tool_type": snapshot.tool_type,
                            "system_prompt": snapshot.system_prompt,
                            "enabled": snapshot.enabled,
                        }),
                    )?;
                    let id = created
                        .as_u64()
                        .ok_or_else(|| anyhow!("daemon returned no agent tool id"))?;
                    self.id = Some(id);
                }
                Some(id) => {
                    let mut payload = serde_json::Map::new();
                    payload.insert("agent_tool_id".into(), json!(id));
                    if snapshot.name != self.baseline.name {
                        payload.insert("name".into(), json!(snapshot.name));
                    }
                    if snapshot.display_name != self.baseline.display_name {
                        payload.insert("display_name".into(), json!(snapshot.display_name));
                    }
                    if snapshot.command != self.baseline.command {
                        payload.insert("command".into(), json!(snapshot.command));
                    }
                    if snapshot.cwd != self.baseline.cwd {
                        payload.insert("cwd".into(), json!(snapshot.cwd));
                    }
                    if snapshot.tool_type != self.baseline.tool_type {
                        payload.insert("tool_type".into(), json!(snapshot.tool_type));
                    }
                    if snapshot.system_prompt != self.baseline.system_prompt {
                        payload.insert("system_prompt".into(), json!(snapshot.system_prompt));
                    }
                    if snapshot.enabled != self.baseline.enabled {
                        payload.insert("enabled".into(), json!(snapshot.enabled));
                    }
                    // Skip the round-trip when nothing diverged - the common
                    // shape of a debounced autosave fired by an unrelated event.
                    if payload.len() > 1 {
                        client.call("agent_tool_update", Value::Object(payload))?;
                    }
                }
            }
            Ok(())
        })();
        client.close();
        outcome?;
        self.baseline = snapshot;
        self.dirty = false;
        self.dirty_since = None;
        self.message = format!("saved agent config #{}", self.id.unwrap_or(0));
        Ok(())
    }

    /// Pull the daemon's current snapshot and replay it onto the form. Untouched
    /// fields adopt the remote value; fields the user is mid-edit on keep their
    /// local text and the message line flags the conflict. The [`Baseline`] is
    /// always advanced. Not-yet-saved forms (no id) return `Ok(false)`.
    pub fn refresh_from_daemon(&mut self) -> Result<bool> {
        let Some(id) = self.id else {
            return Ok(false);
        };
        let client = Client::connect(&self.url)?;
        let outcome = client.call("agent_tool_get", json!({ "agent_tool_id": id }));
        client.close();
        let tool = outcome?;

        let remote = Baseline {
            name: tool["name"].as_str().unwrap_or("").to_string(),
            display_name: tool["display_name"].as_str().unwrap_or("").to_string(),
            command: tool["command"].as_str().unwrap_or("").to_string(),
            cwd: tool["cwd"].as_str().unwrap_or("").to_string(),
            tool_type: tool["tool_type"].as_str().unwrap_or("").to_string(),
            system_prompt: tool["system_prompt"].as_str().unwrap_or("").to_string(),
            enabled: tool["enabled"].as_bool().unwrap_or(true),
        };
        Ok(self.replay_remote(remote))
    }

    /// Apply a daemon snapshot. Pure of MCP so the replay rules are unit-testable;
    /// see [`Self::refresh_from_daemon`] for the wire-up. Returns whether
    /// anything visible changed.
    fn replay_remote(&mut self, remote: Baseline) -> bool {
        let mut changed = false;
        let mut conflicts: Vec<&'static str> = Vec::new();

        if remote.name != self.baseline.name {
            if self.current_name() != self.baseline.name {
                conflicts.push("name");
            } else {
                self.name = text_area(&remote.name);
                changed = true;
            }
        }
        if remote.display_name != self.baseline.display_name {
            if self.current_display() != self.baseline.display_name {
                conflicts.push("display");
            } else {
                self.display = text_area(&remote.display_name);
                changed = true;
            }
        }
        if remote.command != self.baseline.command {
            if self.current_command() != self.baseline.command {
                conflicts.push("command");
            } else {
                self.command = text_area(&remote.command);
                changed = true;
            }
        }
        if remote.cwd != self.baseline.cwd {
            if self.current_cwd() != self.baseline.cwd {
                conflicts.push("cwd");
            } else {
                self.cwd = text_area(&remote.cwd);
                changed = true;
            }
        }
        if remote.tool_type != self.baseline.tool_type {
            if self.current_type() != self.baseline.tool_type {
                conflicts.push("type");
            } else {
                self.set_type(&remote.tool_type);
                changed = true;
            }
        }
        if remote.system_prompt != self.baseline.system_prompt {
            if self.current_prompt() != self.baseline.system_prompt {
                conflicts.push("prompt");
            } else {
                self.prompt = text_area(&remote.system_prompt);
                changed = true;
            }
        }
        if remote.enabled != self.baseline.enabled {
            if self.enabled != self.baseline.enabled {
                conflicts.push("enabled");
            } else {
                self.enabled = remote.enabled;
                changed = true;
            }
        }

        // Advance the baseline unconditionally so a subsequent flush only sends
        // fields the user is still mid-edit on.
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

    /// Point `type_idx` at `tool_type`, appending it to the options if it is not
    /// already a known key so a remote retype to an unknown profile still shows.
    fn set_type(&mut self, tool_type: &str) {
        if !self.type_options.iter().any(|t| t == tool_type) {
            self.type_options.push(tool_type.to_string());
        }
        self.type_idx = index_of(
            &self
                .type_options
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            tool_type,
        );
    }

    fn snapshot(&self) -> Baseline {
        Baseline {
            name: self.current_name(),
            display_name: self.current_display(),
            command: self.current_command(),
            cwd: self.current_cwd(),
            tool_type: self.current_type(),
            system_prompt: self.current_prompt(),
            enabled: self.enabled,
        }
    }

    fn current_name(&self) -> String {
        self.name.lines().join(" ").trim().to_string()
    }
    fn current_display(&self) -> String {
        self.display.lines().join(" ").trim().to_string()
    }
    fn current_command(&self) -> String {
        self.command.lines().join(" ").trim().to_string()
    }
    fn current_cwd(&self) -> String {
        self.cwd.lines().join(" ").trim().to_string()
    }
    fn current_prompt(&self) -> String {
        self.prompt.lines().join("\n")
    }
    fn current_type(&self) -> String {
        self.type_options
            .get(self.type_idx)
            .cloned()
            .unwrap_or_default()
    }

    /// Render the form into `area`.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let rows = Layout::vertical([
            Constraint::Length(3), // name
            Constraint::Length(3), // display
            Constraint::Length(1), // type | enabled
            Constraint::Length(3), // command
            Constraint::Length(3), // cwd
            Constraint::Min(3),    // system prompt (body)
            Constraint::Length(1), // context (created)
            Constraint::Length(1), // message + help
        ])
        .split(area);

        self.field_areas.clear();
        self.text_field_areas.clear();

        self.style_field(Field::Name, "Name");
        frame.render_widget(&self.name, rows[0]);
        self.field_areas.push((Field::Name, rows[0]));
        self.text_field_areas
            .push((Field::Name, Block::bordered().inner(rows[0])));

        self.style_field(Field::Display, "Display name");
        frame.render_widget(&self.display, rows[1]);
        self.field_areas.push((Field::Display, rows[1]));
        self.text_field_areas
            .push((Field::Display, Block::bordered().inner(rows[1])));

        let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[2]);
        frame.render_widget(
            enum_line("Type", &self.current_type(), self.focus == Field::Type),
            cols[0],
        );
        self.field_areas.push((Field::Type, cols[0]));
        frame.render_widget(
            enum_line(
                "Enabled",
                if self.enabled { "yes" } else { "no" },
                self.focus == Field::Enabled,
            ),
            cols[1],
        );
        self.field_areas.push((Field::Enabled, cols[1]));

        self.style_field(Field::Command, "Command");
        frame.render_widget(&self.command, rows[3]);
        self.field_areas.push((Field::Command, rows[3]));
        self.text_field_areas
            .push((Field::Command, Block::bordered().inner(rows[3])));

        self.style_field(Field::Cwd, "Working dir");
        frame.render_widget(&self.cwd, rows[4]);
        self.field_areas.push((Field::Cwd, rows[4]));
        self.text_field_areas
            .push((Field::Cwd, Block::bordered().inner(rows[4])));

        self.draw_body(frame, rows[5]);

        let context = if self.created.is_empty() {
            String::new()
        } else {
            format!(" created {}   type {}", self.created, self.current_type())
        };
        frame.render_widget(
            Paragraph::new(context).style(Style::default().fg(Color::DarkGray)),
            rows[6],
        );

        let help = "Tab field  ←/→ type/enabled  Ctrl-C close";
        let line = if self.message.is_empty() {
            format!(" {help}")
        } else {
            format!(" {}   |   {help}", self.message)
        };
        frame.render_widget(
            Paragraph::new(line).style(Style::default().fg(Color::Yellow)),
            rows[7],
        );
    }

    /// Render the system-prompt body field with the soft-wrap renderer. Same
    /// shape as [`crate::note_form`]'s `draw_body`.
    fn draw_body(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Field::Prompt;
        let border = field_border_color(focused);
        let block = Block::bordered()
            .title("System prompt")
            .border_style(Style::default().fg(border));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let width = inner.width as usize;
        let height = inner.height as usize;
        self.body_view_height = height;
        self.body_area = Some(inner);
        if width == 0 || height == 0 {
            return;
        }

        let cursor = self.prompt.cursor();
        let wrapped = crate::wrap::wrap_for_display(self.prompt.lines(), cursor, width);
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

    /// Set a single-line field's border and cursor styling for the current
    /// focus. Mirrors the note form's `style_field`.
    fn style_field(&mut self, field: Field, label: &'static str) {
        let focused = self.focus == field;
        let area = match self.single_line_textarea_mut(field) {
            Some(a) => a,
            None => return,
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
}

/// Whether terminal cell `(row, col)` lands inside `rect`.
fn within(rect: Rect, row: u16, col: u16) -> bool {
    row >= rect.y && row < rect.y + rect.height && col >= rect.x && col < rect.x + rect.width
}

/// The next field in Tab order, forward or backward, wrapping at the ends.
fn next_field(field: Field, forward: bool) -> Field {
    let i = FIELDS.iter().position(|f| *f == field).unwrap_or(0);
    let n = FIELDS.len();
    let j = if forward {
        (i + 1) % n
    } else {
        (i + n - 1) % n
    };
    FIELDS[j]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Vec<String> {
        vec!["claude-code".to_string(), "codex".to_string()]
    }

    #[test]
    fn blank_form_defaults_type_and_starts_on_name() {
        let form = AgentConfigForm::blank("http://x/?ws=/x", opts(), "codex");
        assert_eq!(form.id, None);
        assert_eq!(form.focus, Field::Name);
        assert_eq!(form.current_type(), "codex");
        assert!(form.enabled);
        assert!(form.name_is_empty());
        assert!(!form.dirty);
    }

    #[test]
    fn blank_form_falls_back_to_first_type_when_default_unknown() {
        let form = AgentConfigForm::blank("http://x/?ws=/x", opts(), "no-such");
        assert_eq!(form.current_type(), "claude-code");
    }

    #[test]
    fn from_parts_appends_an_unknown_type_so_it_round_trips() {
        let form = AgentConfigForm::from_parts(
            "http://x/?ws=/x",
            5,
            "claude",
            "Mediator",
            "claude --model sonnet",
            "/work",
            "legacy-type",
            "be terse",
            false,
            "2026-06-07 00:00:00",
            opts(),
        );
        assert_eq!(form.id, Some(5));
        assert_eq!(form.current_type(), "legacy-type");
        assert!(!form.enabled);
        assert_eq!(form.current_prompt(), "be terse");
    }

    #[test]
    fn tab_walks_every_field_and_wraps() {
        let mut form = AgentConfigForm::blank("http://x/?ws=/x", opts(), "claude-code");
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::empty());
        let order = [
            Field::Display,
            Field::Type,
            Field::Enabled,
            Field::Command,
            Field::Cwd,
            Field::Prompt,
            Field::Name,
        ];
        for expected in order {
            form.handle_key(tab);
            assert_eq!(form.focus, expected);
        }
    }

    #[test]
    fn ctrl_c_closes() {
        let mut form = AgentConfigForm::blank("http://x/?ws=/x", opts(), "claude-code");
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(form.handle_key(ctrl_c), AgentConfigFormAction::Close);
    }

    #[test]
    fn typing_the_name_marks_dirty() {
        let mut form = AgentConfigForm::blank("http://x/?ws=/x", opts(), "claude-code");
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty());
        assert_eq!(form.handle_key(key), AgentConfigFormAction::Dirty);
        assert!(form.dirty);
        assert!(!form.name_is_empty());
    }

    #[test]
    fn cycling_type_with_arrows_changes_the_value() {
        let mut form = AgentConfigForm::blank("http://x/?ws=/x", opts(), "claude-code");
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::empty());
        form.handle_key(tab); // Display
        form.handle_key(tab); // Type
        assert_eq!(form.focus, Field::Type);
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::empty());
        assert_eq!(form.handle_key(right), AgentConfigFormAction::Dirty);
        assert_eq!(form.current_type(), "codex");
        // Cycling wraps back around.
        assert_eq!(form.handle_key(right), AgentConfigFormAction::Dirty);
        assert_eq!(form.current_type(), "claude-code");
    }

    #[test]
    fn space_toggles_enabled() {
        let mut form = AgentConfigForm::blank("http://x/?ws=/x", opts(), "claude-code");
        form.focus = Field::Enabled;
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty());
        assert_eq!(form.handle_key(space), AgentConfigFormAction::Dirty);
        assert!(!form.enabled);
        assert_eq!(form.handle_key(space), AgentConfigFormAction::Dirty);
        assert!(form.enabled);
    }

    #[test]
    fn refresh_replays_remote_into_untouched_fields() {
        let mut form = AgentConfigForm::from_parts(
            "http://x/?ws=/x",
            1,
            "old",
            "",
            "",
            "",
            "claude-code",
            "",
            true,
            "2026-06-07 00:00:00",
            opts(),
        );
        let changed = form.replay_remote(Baseline {
            name: "new".into(),
            display_name: "New".into(),
            command: "claude".into(),
            cwd: "/w".into(),
            tool_type: "codex".into(),
            system_prompt: "terse".into(),
            enabled: false,
        });
        assert!(changed);
        assert_eq!(form.current_name(), "new");
        assert_eq!(form.current_display(), "New");
        assert_eq!(form.current_command(), "claude");
        assert_eq!(form.current_cwd(), "/w");
        assert_eq!(form.current_type(), "codex");
        assert_eq!(form.current_prompt(), "terse");
        assert!(!form.enabled);
        assert!(!form.message.contains("overwrite"));
    }

    #[test]
    fn refresh_keeps_local_edit_and_flags_conflict() {
        let mut form = AgentConfigForm::from_parts(
            "http://x/?ws=/x",
            1,
            "old",
            "",
            "",
            "",
            "claude-code",
            "",
            true,
            "2026-06-07 00:00:00",
            opts(),
        );
        // Local edit to the name.
        let key = KeyEvent::new(KeyCode::Char('!'), KeyModifiers::empty());
        form.handle_key(key);
        let changed = form.replay_remote(Baseline {
            name: "remote".into(),
            display_name: "".into(),
            command: "claude".into(),
            cwd: "".into(),
            tool_type: "claude-code".into(),
            system_prompt: "".into(),
            enabled: true,
        });
        assert!(changed);
        // Name was being edited - local wins; command was untouched - adopted.
        assert!(form.current_name().contains("old"));
        assert_ne!(form.current_name(), "old");
        assert_eq!(form.current_command(), "claude");
        assert!(form.message.contains("name"));
        assert!(form.message.contains("overwrite"));
        // Baseline advances to the remote view regardless.
        assert_eq!(form.baseline.name, "remote");
    }

    #[test]
    fn refresh_with_no_drift_is_a_noop() {
        let mut form = AgentConfigForm::from_parts(
            "http://x/?ws=/x",
            1,
            "n",
            "",
            "",
            "",
            "claude-code",
            "",
            true,
            "2026-06-07 00:00:00",
            opts(),
        );
        let changed = form.replay_remote(Baseline {
            name: "n".into(),
            display_name: "".into(),
            command: "".into(),
            cwd: "".into(),
            tool_type: "claude-code".into(),
            system_prompt: "".into(),
            enabled: true,
        });
        assert!(!changed);
    }
}
