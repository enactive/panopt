//! `panopt _close-gate` - the cockpit's close confirmation dialog.
//!
//! A minimal `ratatui` floating-pane TUI the sidebar plugin spawns when one of
//! its three gates (CloseFocus, CloseTab, Quit) would close an active item.
//! The dialog lists the items that would be lost and offers a `[y] close
//! anyway` override or `[n] cancel`. On `y`, it pipes a decision back to the
//! sidebar plugin via `zellij action pipe --name panopt:close-gate-decision`;
//! the plugin then invokes the matching zellij-tile API call (which bypasses
//! the rewritten keybind so the gate is not re-triggered). On `n`/Esc, the
//! dialog exits without piping anything.
//!
//! Like the todo form, the dialog closes its own Zellij pane on the way out -
//! `zellij action close-pane` from inside the pane - so the user is never
//! left with a spent command pane.

use std::process::Command;

use anyhow::{anyhow, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

/// Which destructive action the dialog is gating. Determines the title text
/// and what the plugin will call if the user confirms.
#[derive(Clone, Copy)]
enum Scope {
    Focus,
    Tab,
    Quit,
}

impl Scope {
    fn parse(s: &str) -> Result<Scope> {
        match s {
            "focus" => Ok(Scope::Focus),
            "tab" => Ok(Scope::Tab),
            "quit" => Ok(Scope::Quit),
            other => Err(anyhow!("unknown scope `{other}` - expected focus/tab/quit")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Scope::Focus => "focus",
            Scope::Tab => "tab",
            Scope::Quit => "quit",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Scope::Focus => "Close this pane?",
            Scope::Tab => "Close this tab?",
            Scope::Quit => "Quit the cockpit?",
        }
    }
}

/// One active item the dialog will list. Carries the kind label (agent /
/// command / terminal) and the display name; both come from the plugin via
/// the `--items` CLI argument.
struct Item {
    kind: String,
    label: String,
}

/// Run the close-gate dialog. Returns when the user has answered yes or no.
/// `items` is the encoded list from the plugin: `kind:label;kind:label;...`,
/// with `;` and `:` in labels replaced upstream so the split is unambiguous.
/// `target_pane` is the terminal pane id the plugin wants closed when scope
/// is `focus`; passed back verbatim in the decision payload.
pub fn run(scope: &str, items: &str, target_pane: Option<u32>, port: u16) -> Result<()> {
    let scope = Scope::parse(scope)?;
    let items = parse_items(items);

    let mut terminal = ratatui::init();
    let outcome = event_loop(&mut terminal, scope, &items);
    ratatui::restore();

    // The user's choice goes back to the plugin BEFORE we close our own
    // pane: the close call below tears down this process, so anything past
    // it will not run.
    if let Ok(Confirmation::Close) = &outcome {
        send_decision(scope, target_pane, port)?;
    }

    // Close the floating pane on the way out, same as the todo form. The
    // `zellij action close-pane` CLI fires CloseFocus directly (not through
    // a keybind), so it bypasses the cockpit's gate and just closes us.
    if std::env::var_os("ZELLIJ").is_some() {
        let _ = Command::new("zellij").args(["action", "close-pane"]).status();
    }

    outcome.map(|_| ())
}

/// The two terminal states of the dialog.
enum Confirmation {
    Close,
    Cancel,
}

/// Parse the `--items` argument into a list of [`Item`]. Each entry is
/// `kind:label`; entries are joined by `;`. Empty entries are skipped so a
/// trailing `;` or an empty `--items` value is harmless.
fn parse_items(encoded: &str) -> Vec<Item> {
    encoded
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|entry| match entry.split_once(':') {
            Some((kind, label)) => Item {
                kind: kind.to_string(),
                label: label.to_string(),
            },
            None => Item {
                kind: "item".to_string(),
                label: entry.to_string(),
            },
        })
        .collect()
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    scope: Scope,
    items: &[Item],
) -> Result<Confirmation> {
    loop {
        terminal.draw(|frame| draw(frame, scope, items))?;
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match handle_key(key) {
                Some(Confirmation::Close) => return Ok(Confirmation::Close),
                Some(Confirmation::Cancel) => return Ok(Confirmation::Cancel),
                None => continue,
            }
        }
    }
}

/// The dialog responds to a tiny keyboard surface: `y` confirms, `n`/Esc/`q`
/// cancels. Any other key is a no-op so a stray keystroke does not commit
/// the close.
fn handle_key(key: KeyEvent) -> Option<Confirmation> {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(Confirmation::Close),
        KeyCode::Char('n') | KeyCode::Char('N') => Some(Confirmation::Cancel),
        KeyCode::Char('q') | KeyCode::Esc => Some(Confirmation::Cancel),
        _ => None,
    }
}

fn draw(frame: &mut Frame, scope: Scope, items: &[Item]) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(
            format!(" {} ", scope.title()),
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

    let intro = match items.is_empty() {
        true => Paragraph::new("No active items - confirm to proceed.")
            .alignment(Alignment::Left),
        false => Paragraph::new("Active items would be lost:").alignment(Alignment::Left),
    };
    frame.render_widget(intro, layout[0]);

    let list_lines: Vec<Line> = items
        .iter()
        .map(|item| {
            Line::from(vec![
                Span::raw("  - "),
                Span::styled(
                    item.kind.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(": "),
                Span::raw(item.label.clone()),
            ])
        })
        .collect();
    let list = Paragraph::new(list_lines).wrap(Wrap { trim: false });
    frame.render_widget(list, layout[1]);

    let prompt = Paragraph::new(Line::from(vec![
        Span::styled("[y]", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" close anyway   "),
        Span::styled("[n]", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" cancel"),
    ]))
    .alignment(Alignment::Center);
    frame.render_widget(prompt, layout[3]);
}

/// Pipe the user's `close anyway` decision back to the sidebar plugin. The
/// payload is a tiny key=value;key=value string so the plugin can parse it
/// without pulling in a JSON dependency. Errors are surfaced because the
/// pipe is the only way the gate hears back; a silent failure here would
/// leave the user thinking the close worked when it did not.
fn send_decision(scope: Scope, target_pane: Option<u32>, port: u16) -> Result<()> {
    let _ = port; // reserved for future telemetry, kept in the signature
    let payload = match target_pane {
        Some(id) => format!("scope={};target_pane={};decision=close", scope.name(), id),
        None => format!("scope={};decision=close", scope.name()),
    };
    let status = Command::new("zellij")
        .args(["action", "pipe", "--name", "panopt:close-gate-decision", "--", &payload])
        .status()
        .context("running `zellij action pipe` to send the close decision")?;
    if !status.success() {
        return Err(anyhow!("`zellij action pipe` exited with a failure"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_items, Scope};

    #[test]
    fn parses_scope_strings() {
        assert!(matches!(Scope::parse("focus").unwrap(), Scope::Focus));
        assert!(matches!(Scope::parse("tab").unwrap(), Scope::Tab));
        assert!(matches!(Scope::parse("quit").unwrap(), Scope::Quit));
        assert!(Scope::parse("nope").is_err());
    }

    #[test]
    fn parses_an_encoded_items_list() {
        let items = parse_items("agent:NASTL;command:cargo build;terminal:my-shell");
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].kind, "agent");
        assert_eq!(items[0].label, "NASTL");
        assert_eq!(items[1].kind, "command");
        assert_eq!(items[1].label, "cargo build");
        assert_eq!(items[2].kind, "terminal");
        assert_eq!(items[2].label, "my-shell");
    }

    #[test]
    fn empty_items_is_no_items() {
        assert!(parse_items("").is_empty());
        // A trailing separator is harmless.
        assert_eq!(parse_items("agent:a;").len(), 1);
    }

    #[test]
    fn an_entry_without_a_colon_lands_under_a_generic_kind() {
        let items = parse_items("bare-label");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "item");
        assert_eq!(items[0].label, "bare-label");
    }
}
