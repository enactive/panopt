//! `panopt todo edit` - the standalone CLI shell over the shared todo form.
//!
//! Loads a todo through the daemon, hands the form (in [`crate::todo_form`]) a
//! `ratatui` event loop, and writes back through the MCP client. The same
//! form is hosted in-pane by the cockpit's `_viewer`; this binary is the
//! shell that lets a user run it outside the cockpit.
//!
//! The CLI shell preserves the legacy keybinds the cockpit no longer uses:
//! Ctrl-S to save explicitly and Esc to quit (with a dirty-flag warning),
//! because there is no host driving debounced autosave for it.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{DefaultTerminal, Frame};
use serde_json::{json, Value};

use crate::daemon;
use crate::todo_form::{TodoForm, TodoFormAction};
use crate::mcpclient::Client;
use crate::todo::observer_url;

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
            // The standalone CLI does not bother resolving blocker titles -
            // the in-pane host that needs them passes its own resolver.
            TodoForm::from_todo(&url, &todo, &|_| None)?
        }
        None => TodoForm::blank(&url),
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

/// Draw, read a key, repeat, until the user asks to close.
fn event_loop(terminal: &mut DefaultTerminal, form: &mut TodoForm) -> Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, form))?;
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            // CLI-specific shortcuts: Ctrl-S forces a save; Esc quits with a
            // dirty-flag warning. Both are intercepted before the form widget
            // sees them so the standalone behavior matches the legacy form.
            match key.code {
                KeyCode::Char('s') if ctrl => {
                    form.save_explicit();
                    continue;
                }
                KeyCode::Esc => {
                    if form.can_quit {
                        return Ok(());
                    }
                    form.can_quit = true;
                    form.message =
                        "unsaved changes - Esc again to discard, Ctrl-S to save".to_string();
                    continue;
                }
                _ => {}
            }
            match form.handle_key(key) {
                TodoFormAction::Close => return Ok(()),
                TodoFormAction::Dirty | TodoFormAction::Idle => {}
            }
        }
    }
}

/// Render the form into the full frame area.
fn draw(frame: &mut Frame, form: &mut TodoForm) {
    let area = frame.area();
    form.draw(frame, area);
}
