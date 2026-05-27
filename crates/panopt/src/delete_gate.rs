//! `panopt _delete-gate` - the cockpit's delete confirmation dialog.
//!
//! A floating-pane TUI the sidebar plugin spawns when the user presses `x` on
//! a deletable row. The dialog names what is about to be deleted and offers a
//! `[y] delete` / `[n] cancel` choice. On `y` it pipes a decision back to the
//! sidebar plugin via `zellij action pipe --name panopt:delete-gate-decision`;
//! the plugin then runs the actual `panopt <kind> rm <id>` so the sidebar -
//! not the dialog - owns the destructive call. On `n`/Esc/`q`, the dialog
//! exits without piping anything.
//!
//! Mirrors `close_gate.rs` (same floating-window UX, same pipe protocol) so a
//! reader who knows one knows the other.

use std::process::Command;

use anyhow::{anyhow, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

/// Which item type the user wants to delete. Drives the prompt text and is
/// echoed back to the plugin verbatim so it can dispatch the right delete.
#[derive(Clone, Copy)]
enum Kind {
    Todo,
    Scratchpad,
    AgentTool,
    Process,
}

impl Kind {
    fn parse(s: &str) -> Result<Kind> {
        match s {
            "todo" => Ok(Kind::Todo),
            "scratchpad" => Ok(Kind::Scratchpad),
            "agent-tool" => Ok(Kind::AgentTool),
            "process" => Ok(Kind::Process),
            other => Err(anyhow!(
                "unknown kind `{other}` - expected todo/scratchpad/agent-tool/process"
            )),
        }
    }

    fn wire(self) -> &'static str {
        match self {
            Kind::Todo => "todo",
            Kind::Scratchpad => "scratchpad",
            Kind::AgentTool => "agent-tool",
            Kind::Process => "process",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Kind::Todo => "todo",
            Kind::Scratchpad => "scratchpad",
            Kind::AgentTool => "agent tool",
            Kind::Process => "process",
        }
    }
}

/// Run the dialog. `id` is the numeric item id; `label` is the human title
/// the dialog shows next to it. The plugin uses the same `id` to dispatch
/// the delete after a confirm.
pub fn run(kind: &str, id: u64, label: &str, port: u16) -> Result<()> {
    let kind = Kind::parse(kind)?;

    let mut terminal = ratatui::init();
    let outcome = event_loop(&mut terminal, kind, id, label);
    ratatui::restore();

    if let Ok(Confirmation::Delete) = &outcome {
        send_decision(kind, id, port)?;
    }

    if std::env::var_os("ZELLIJ").is_some() {
        let _ = Command::new("zellij")
            .args(["action", "close-pane"])
            .status();
    }

    outcome.map(|_| ())
}

enum Confirmation {
    Delete,
    Cancel,
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    kind: Kind,
    id: u64,
    label: &str,
) -> Result<Confirmation> {
    loop {
        terminal.draw(|frame| draw(frame, kind, id, label))?;
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match handle_key(key) {
                Some(c) => return Ok(c),
                None => continue,
            }
        }
    }
}

fn handle_key(key: KeyEvent) -> Option<Confirmation> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(Confirmation::Delete),
        KeyCode::Char('n') | KeyCode::Char('N') => Some(Confirmation::Cancel),
        KeyCode::Char('q') | KeyCode::Esc => Some(Confirmation::Cancel),
        _ => None,
    }
}

fn draw(frame: &mut Frame, kind: Kind, id: u64, label: &str) {
    let area = frame.area();
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        format!(" Delete this {}? ", kind.label()),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let layout = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);

    let intro = Paragraph::new("This item would be soft-deleted:").alignment(Alignment::Left);
    frame.render_widget(intro, layout[0]);

    let row = Line::from(vec![
        Span::raw("  - "),
        Span::styled(
            format!("#{id}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::raw(label.to_string()),
    ]);
    let list = Paragraph::new(row).wrap(Wrap { trim: false });
    frame.render_widget(list, layout[1]);

    let prompt = Paragraph::new(Line::from(vec![
        Span::styled("[y]", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" delete   "),
        Span::styled("[n]", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" cancel"),
    ]))
    .alignment(Alignment::Center);
    frame.render_widget(prompt, layout[3]);
}

/// Pipe the confirm back to the Todos plugin pane, mirroring `close_gate`'s
/// shape: tiny key=value;key=value string, narrowed delivery via
/// `--plugin-configuration mode=todos`.
fn send_decision(kind: Kind, id: u64, port: u16) -> Result<()> {
    let _ = port;
    let payload = format!("kind={};id={};decision=delete", kind.wire(), id);
    let status = Command::new("zellij")
        .args([
            "action",
            "pipe",
            "--name",
            "panopt:delete-gate-decision",
            "--plugin-configuration",
            "mode=todos",
            "--",
            &payload,
        ])
        .status()
        .context("running `zellij action pipe` to send the delete decision")?;
    if !status.success() {
        return Err(anyhow!("`zellij action pipe` exited with a failure"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{handle_key, Kind};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn parses_known_kinds() {
        assert!(matches!(Kind::parse("todo").unwrap(), Kind::Todo));
        assert!(matches!(
            Kind::parse("scratchpad").unwrap(),
            Kind::Scratchpad
        ));
        assert!(matches!(
            Kind::parse("agent-tool").unwrap(),
            Kind::AgentTool
        ));
        assert!(matches!(Kind::parse("process").unwrap(), Kind::Process));
        assert!(Kind::parse("nope").is_err());
    }

    #[test]
    fn y_confirms_n_and_esc_cancel() {
        assert!(matches!(
            handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::empty())),
            Some(super::Confirmation::Delete)
        ));
        assert!(matches!(
            handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty())),
            Some(super::Confirmation::Cancel)
        ));
        assert!(matches!(
            handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
            Some(super::Confirmation::Cancel)
        ));
        // An unrelated key produces no decision; the dialog keeps prompting.
        assert!(handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty())).is_none());
    }
}
