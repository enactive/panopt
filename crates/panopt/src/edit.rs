//! `panopt todo edit` - the cockpit's todo form.
//!
//! A `ratatui` TUI that loads a todo from the daemon, presents it as an
//! editable form, and writes changes back through the MCP client. The sidebar
//! plugin launches it in a floating Zellij pane; it also runs standalone from a
//! shell. The form edits the scalar fields - title, body, status, priority,
//! assignee, tags; comments and blockers are shown for context and managed with
//! the `panopt todo comment` / `panopt todo block` subcommands.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use serde_json::{json, Value};
use tui_textarea::TextArea;

use crate::daemon;
use crate::mcpclient::Client;
use crate::todo::observer_url;

/// The cyclable status values, in cycle order.
const STATUSES: [&str; 4] = ["open", "in_progress", "backlog", "completed"];
/// The cyclable priority values, in cycle order.
const PRIORITIES: [&str; 3] = ["high", "medium", "low"];

/// The form's fields, in Tab order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Title,
    Status,
    Priority,
    Assignee,
    Tags,
    Body,
}

const FIELDS: [Field; 6] = [
    Field::Title,
    Field::Status,
    Field::Priority,
    Field::Assignee,
    Field::Tags,
    Field::Body,
];

/// Run the todo form. Exactly one of `id` (edit that todo) or `new` (create a
/// fresh one) must be set.
pub fn run(ws: Option<PathBuf>, id: Option<u64>, new: bool, port: u16) -> Result<()> {
    if new == id.is_some() {
        return Err(anyhow!("pass a todo id to edit, or --new to create one"));
    }
    daemon::ensure(port)?;
    let url = observer_url(ws, port)?;

    let mut form = match id {
        Some(id) => {
            let todo = load(&url, id).with_context(|| format!("loading todo #{id}"))?;
            Form::from_todo(&url, &todo)?
        }
        None => Form::blank(&url),
    };

    let mut terminal = ratatui::init();
    let outcome = event_loop(&mut terminal, &mut form);
    ratatui::restore();

    // When launched as a floating cockpit pane, close that pane on the way out
    // so the form does not linger as a spent command pane. This must run
    // synchronously: the process has to stay alive - and stay the focused pane
    // - until Zellij has the request, or it would close whatever pane gains
    // focus next, or be killed before the request lands. The call typically
    // never returns: closing the pane ends this process.
    if std::env::var_os("ZELLIJ").is_some() {
        let _ = Command::new("zellij").args(["action", "close-pane"]).status();
    }
    outcome
}

/// Fetch one todo in full from the daemon.
fn load(url: &str, id: u64) -> Result<Value> {
    let client = Client::connect(url)?;
    let todo = client.call("todo_get", json!({ "todo_id": id }));
    client.close();
    todo
}

/// Draw, read a key, repeat, until [`Form::handle_key`] asks to quit.
fn event_loop(terminal: &mut DefaultTerminal, form: &mut Form) -> Result<()> {
    loop {
        terminal.draw(|frame| form.draw(frame))?;
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press && form.handle_key(key) {
                return Ok(());
            }
        }
    }
}

/// The editable state of the form.
struct Form {
    /// The daemon MCP URL with `?ws=...&observer=1`.
    url: String,
    /// The todo's id, or `None` until a new todo is first saved.
    id: Option<u64>,
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
    blockers: Vec<u64>,
    comment_count: usize,
    created: String,
    updated: String,
    /// True when there are unsaved edits.
    dirty: bool,
    /// True when a single Esc may quit - there is nothing unsaved to lose, or
    /// the user has already been warned once.
    can_quit: bool,
    /// Bottom-line feedback.
    message: String,
}

impl Form {
    /// A blank form for a not-yet-created todo.
    fn blank(url: &str) -> Form {
        Form {
            url: url.to_string(),
            id: None,
            title: text_area(""),
            assignee: text_area(""),
            tags: text_area(""),
            body: text_area(""),
            status: 0,
            priority: index_of(&PRIORITIES, "medium"),
            focus: 0,
            blockers: Vec::new(),
            comment_count: 0,
            created: String::new(),
            updated: String::new(),
            dirty: false,
            can_quit: true,
            message: "new todo - Ctrl-S to create it".to_string(),
        }
    }

    /// A form populated from a `todo_get` result.
    fn from_todo(url: &str, todo: &Value) -> Result<Form> {
        let id = todo["id"].as_u64().ok_or_else(|| anyhow!("todo response has no id"))?;
        let tags = string_list(&todo["tags"]).join(", ");
        let blockers = todo["blockers"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();
        Ok(Form {
            url: url.to_string(),
            id: Some(id),
            title: text_area(todo["title"].as_str().unwrap_or("")),
            assignee: text_area(todo["assignee"].as_str().unwrap_or("")),
            tags: text_area(&tags),
            body: text_area(todo["body"].as_str().unwrap_or("")),
            status: index_of(&STATUSES, todo["status"].as_str().unwrap_or("open")),
            priority: index_of(&PRIORITIES, todo["priority"].as_str().unwrap_or("medium")),
            focus: 0,
            blockers,
            comment_count: todo["comments"].as_array().map_or(0, Vec::len),
            created: todo["created_at"].as_str().unwrap_or("").to_string(),
            updated: todo["updated_at"].as_str().unwrap_or("").to_string(),
            dirty: false,
            can_quit: true,
            message: format!("editing todo #{id}"),
        })
    }

    /// Handle one key press; return `true` to quit the form.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('s') if ctrl => {
                self.save();
                false
            }
            KeyCode::Char('c') if ctrl => true,
            KeyCode::Esc => {
                if self.can_quit {
                    true
                } else {
                    self.can_quit = true;
                    self.message =
                        "unsaved changes - Esc again to discard, Ctrl-S to save".to_string();
                    false
                }
            }
            KeyCode::Tab => {
                self.focus = (self.focus + 1) % FIELDS.len();
                false
            }
            KeyCode::BackTab => {
                self.focus = (self.focus + FIELDS.len() - 1) % FIELDS.len();
                false
            }
            _ => {
                self.field_key(key);
                false
            }
        }
    }

    /// Route a key to whatever field currently has focus.
    fn field_key(&mut self, key: KeyEvent) {
        match FIELDS[self.focus] {
            Field::Status => {
                if let Some(dir) = cycle_dir(key.code) {
                    self.status = wrap(self.status, dir, STATUSES.len());
                    self.mark_dirty();
                }
            }
            Field::Priority => {
                if let Some(dir) = cycle_dir(key.code) {
                    self.priority = wrap(self.priority, dir, PRIORITIES.len());
                    self.mark_dirty();
                }
            }
            Field::Title => {
                if single_line_input(&mut self.title, key) {
                    self.mark_dirty();
                }
            }
            Field::Assignee => {
                if single_line_input(&mut self.assignee, key) {
                    self.mark_dirty();
                }
            }
            Field::Tags => {
                if single_line_input(&mut self.tags, key) {
                    self.mark_dirty();
                }
            }
            Field::Body => {
                if self.body.input(key) {
                    self.mark_dirty();
                }
            }
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.can_quit = false;
    }

    /// Write the form back to the daemon, creating the todo first if it is new.
    fn save(&mut self) {
        let title = self.title.lines().join(" ").trim().to_string();
        if title.is_empty() {
            self.message = "title cannot be empty".to_string();
            return;
        }
        match self.write_back(&title) {
            Ok(()) => {
                self.dirty = false;
                self.can_quit = true;
                self.message = format!("saved todo #{}", self.id.unwrap_or(0));
            }
            // `{e:#}` so the daemon's actual message shows, not just the
            // outermost "tool ... failed" context.
            Err(e) => self.message = format!("save failed: {e:#}"),
        }
    }

    fn write_back(&mut self, title: &str) -> Result<()> {
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
        outcome
    }

    fn draw(&mut self, frame: &mut Frame) {
        let rows = Layout::vertical([
            Constraint::Length(1), // header
            Constraint::Length(3), // title
            Constraint::Length(1), // status + priority
            Constraint::Length(3), // assignee
            Constraint::Length(3), // tags
            Constraint::Min(4),    // body
            Constraint::Length(2), // context
            Constraint::Length(1), // message + help
        ])
        .split(frame.area());

        let header = match self.id {
            Some(id) => format!(" Edit todo #{id}"),
            None => " New todo".to_string(),
        };
        frame.render_widget(
            Paragraph::new(header).style(Style::default().add_modifier(Modifier::BOLD)),
            rows[0],
        );

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

        let mut context = Vec::new();
        if !self.blockers.is_empty() {
            let ids: Vec<String> = self.blockers.iter().map(|b| format!("#{b}")).collect();
            context.push(format!("blocked by {}", ids.join(", ")));
        }
        if self.comment_count > 0 {
            context.push(format!(
                "{} comment(s) - use `panopt todo comment`",
                self.comment_count
            ));
        }
        if !self.created.is_empty() {
            context.push(format!("created {}   updated {}", self.created, self.updated));
        }
        frame.render_widget(
            Paragraph::new(context.join("    ")).style(Style::default().fg(Color::DarkGray)),
            rows[6],
        );

        let help = "Tab field   Left/Right cycle   Ctrl-S save   Esc quit";
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

    /// Set a text field's border and cursor styling for the current focus.
    fn style_field(&mut self, field: Field, label: &'static str) {
        let focused = FIELDS[self.focus] == field;
        let area = match field {
            Field::Title => &mut self.title,
            Field::Assignee => &mut self.assignee,
            Field::Tags => &mut self.tags,
            Field::Body => &mut self.body,
            Field::Status | Field::Priority => return,
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

/// A text area carrying `initial`, with the cursor-line highlight disabled so
/// it reads as a plain field.
fn text_area(initial: &str) -> TextArea<'static> {
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
fn single_line_input(area: &mut TextArea, key: KeyEvent) -> bool {
    if key.code == KeyCode::Enter {
        return false;
    }
    area.input(key)
}

/// The cycle direction a key implies for an enum field, if any.
fn cycle_dir(code: KeyCode) -> Option<i32> {
    match code {
        KeyCode::Left => Some(-1),
        KeyCode::Right => Some(1),
        _ => None,
    }
}

/// Step index `i` by `dir`, wrapping within `len`.
fn wrap(i: usize, dir: i32, len: usize) -> usize {
    (i as i32 + dir).rem_euclid(len as i32) as usize
}

/// The position of `value` in `options`, or 0 when it is not present.
fn index_of(options: &[&str], value: &str) -> usize {
    options.iter().position(|o| *o == value).unwrap_or(0)
}

/// The non-empty strings of a JSON array value.
fn string_list(v: &Value) -> Vec<String> {
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
}
