//! `panopt search` - the cockpit's popup search dialog.
//!
//! A floating-pane TUI launched by the sidebar plugin when the user presses
//! the global search keybind. Lets the user type a query, fires the daemon's
//! `todo_search` and `scratchpad_search` MCP tools (debounced ~150ms per
//! keystroke), and renders matching rows across both kinds. On Enter, it
//! pipes the selection back to the plugin via
//! `zellij action pipe --name panopt:show-result`; the plugin then routes
//! the chosen item into the cockpit's viewer the same way an Enter on a
//! sidebar row does. On Esc/Ctrl-C, the dialog exits without piping.
//!
//! Patterned on `delete_gate.rs` and `close_gate.rs`: ratatui-based floating
//! TUI, key=value pipe payload, the dialog closes its own Zellij pane on
//! exit so the user is never left with a spent command pane.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use serde_json::{json, Value};

use crate::daemon;
use crate::mcpclient::Client;
use crate::todo::observer_url;

/// How long to wait after the last keystroke before firing the MCP search.
/// Keeps us off the daemon for a burst of typing without making the UI feel
/// laggy. The two tool calls combined are sub-millisecond on localhost, so
/// the only reason to debounce at all is to avoid wasting cycles.
const DEBOUNCE: Duration = Duration::from_millis(150);

/// How long `event::poll` blocks each tick. Bounded so the debounce timer
/// can fire even when the user has stopped typing.
const POLL_TICK: Duration = Duration::from_millis(50);

/// Which surface a search row came from. Drives the row prefix and the wire
/// token used in the show-result pipe.
#[derive(Clone, Copy)]
enum Kind {
    Todo,
    Scratchpad,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::Todo => "T",
            Kind::Scratchpad => "S",
        }
    }

    fn wire(self) -> &'static str {
        match self {
            Kind::Todo => "todo",
            Kind::Scratchpad => "scratchpad",
        }
    }
}

#[derive(Clone)]
struct Row {
    kind: Kind,
    id: u64,
    title: String,
    /// Todo status string (e.g. "open", "in_progress"); `None` for scratchpads.
    status: Option<String>,
    /// Todo priority string (e.g. "high"); `None` for scratchpads.
    priority: Option<String>,
}

enum Outcome {
    Quit,
    Select(Row),
}

struct State {
    query: String,
    rows: Vec<Row>,
    cursor: usize,
    /// `Some(t)` when the query has changed since the last search call and the
    /// debounce window opened at `t`; cleared once the search fires.
    pending_since: Option<Instant>,
    /// Last search error to surface in the status line; cleared on success.
    last_error: Option<String>,
}

pub fn run(ws: Option<PathBuf>, port: u16) -> Result<()> {
    daemon::ensure(None, port)?;
    let url = observer_url(ws, port)?;
    let client = Client::connect(&url).context("connecting to the panopt daemon")?;

    let mut terminal = ratatui::init();
    let outcome = event_loop(&mut terminal, &client);
    ratatui::restore();
    client.close();

    // Both exit paths delegate pane closure to the sidebar plugin (over the
    // `panopt:show-result` / `panopt:close-search` pipes), rather than
    // calling `zellij action close-pane` here. That avoided a race where
    // the plugin's `open_document` shifted focus to the viewer first, so
    // our own close-pane either landed on the wrong pane or lost to
    // Zellij's `EXIT CODE: 0 / <ENTER> re-run` redraw on this CLI's exit.
    match &outcome {
        Ok(Outcome::Select(row)) => send_selection(row)?,
        Ok(Outcome::Quit) => send_close()?,
        Err(_) => send_close()?,
    }

    outcome.map(|_| ())
}

fn event_loop(terminal: &mut DefaultTerminal, client: &Client) -> Result<Outcome> {
    let mut state = State {
        query: String::new(),
        rows: Vec::new(),
        cursor: 0,
        pending_since: None,
        last_error: None,
    };

    loop {
        terminal.draw(|frame| draw(frame, &state))?;

        if let Some(ts) = state.pending_since {
            if ts.elapsed() >= DEBOUNCE {
                state.pending_since = None;
                if state.query.is_empty() {
                    state.rows.clear();
                    state.cursor = 0;
                    state.last_error = None;
                } else {
                    match run_search(client, &state.query) {
                        Ok(rows) => {
                            state.rows = rows;
                            state.cursor = 0;
                            state.last_error = None;
                        }
                        Err(e) => {
                            state.last_error = Some(format!("{e:#}"));
                        }
                    }
                }
                continue;
            }
        }

        if !event::poll(POLL_TICK)? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(o) = handle_key(key, &mut state) {
                return Ok(o);
            }
        }
    }
}

fn handle_key(key: KeyEvent, state: &mut State) -> Option<Outcome> {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, _) => Some(Outcome::Quit),
        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => Some(Outcome::Quit),
        (KeyCode::Enter, _) => state.rows.get(state.cursor).cloned().map(Outcome::Select),
        (KeyCode::Up, _) => {
            if state.cursor > 0 {
                state.cursor -= 1;
            }
            None
        }
        (KeyCode::Down, _) => {
            if state.cursor + 1 < state.rows.len() {
                state.cursor += 1;
            }
            None
        }
        (KeyCode::PageUp, _) => {
            state.cursor = state.cursor.saturating_sub(10);
            None
        }
        (KeyCode::PageDown, _) => {
            state.cursor = (state.cursor + 10).min(state.rows.len().saturating_sub(1));
            None
        }
        (KeyCode::Home, _) => {
            state.cursor = 0;
            None
        }
        (KeyCode::End, _) => {
            state.cursor = state.rows.len().saturating_sub(1);
            None
        }
        (KeyCode::Backspace, _) => {
            if state.query.pop().is_some() {
                state.pending_since = Some(Instant::now());
            }
            None
        }
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            state.query.push(c);
            state.pending_since = Some(Instant::now());
            None
        }
        _ => None,
    }
}

fn run_search(client: &Client, query: &str) -> Result<Vec<Row>> {
    let mut rows = Vec::new();

    let todos = client
        .call("todo_search", json!({ "query": query }))
        .context("todo_search")?;
    if let Some(arr) = todos.as_array() {
        for t in arr {
            rows.push(Row {
                kind: Kind::Todo,
                id: t.get("id").and_then(Value::as_u64).unwrap_or(0),
                title: t
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                status: t.get("status").and_then(Value::as_str).map(str::to_string),
                priority: t
                    .get("priority")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
    }

    let pads = client
        .call("scratchpad_search", json!({ "query": query }))
        .context("scratchpad_search")?;
    if let Some(arr) = pads.as_array() {
        for s in arr {
            rows.push(Row {
                kind: Kind::Scratchpad,
                id: s.get("id").and_then(Value::as_u64).unwrap_or(0),
                title: s
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                status: None,
                priority: None,
            });
        }
    }

    Ok(rows)
}

fn draw(frame: &mut Frame, state: &State) {
    let area = frame.area();
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " panopt search ",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let layout = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);

    let query_line = Line::from(vec![
        Span::styled("> ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(state.query.as_str()),
        Span::styled("\u{2588}", Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(query_line), layout[0]);

    let visible_max = layout[1].height as usize;
    let total = state.rows.len();
    let scroll_start = if total > visible_max {
        state
            .cursor
            .saturating_sub(visible_max / 2)
            .min(total - visible_max)
    } else {
        0
    };
    let range_end = (scroll_start + visible_max).min(total);

    let mut lines: Vec<Line> = Vec::new();
    for (offset, row) in state.rows[scroll_start..range_end].iter().enumerate() {
        let real_idx = scroll_start + offset;
        let focused = real_idx == state.cursor;
        let marker = if focused { " \u{25b6} " } else { "   " };
        let meta = match (&row.status, &row.priority) {
            (Some(s), Some(p)) => format!("  {s}  {p}"),
            _ => String::new(),
        };
        let line = format!(
            "{marker}{}  #{:<5} {}{}",
            row.kind.prefix(),
            row.id,
            row.title,
            meta,
        );
        let style = if focused {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::styled(line, style));
    }
    if lines.is_empty() {
        let msg = if state.query.is_empty() {
            "(type to search todos and scratchpads)"
        } else if state.pending_since.is_some() {
            "(searching...)"
        } else {
            "(no matches)"
        };
        lines.push(Line::styled(msg, Style::default().fg(Color::DarkGray)));
    }
    frame.render_widget(Paragraph::new(lines), layout[1]);

    let status = if let Some(err) = &state.last_error {
        Line::styled(format!("error: {err}"), Style::default().fg(Color::Red))
    } else {
        let hint = Line::from(vec![
            Span::styled(
                "\u{2191}\u{2193}",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(" select  "),
            Span::styled("\u{23ce}", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" open  "),
            Span::styled("esc", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" close   "),
            Span::raw(format!(
                "{} hit{}",
                total,
                if total == 1 { "" } else { "s" }
            )),
        ]);
        hint
    };
    frame.render_widget(Paragraph::new(status).alignment(Alignment::Left), layout[2]);
}

fn send_selection(row: &Row) -> Result<()> {
    if std::env::var_os("ZELLIJ").is_none() {
        println!("{}:{}", row.kind.wire(), row.id);
        return Ok(());
    }
    let payload = format!("kind={};id={}", row.kind.wire(), row.id);
    pipe_to_plugin("panopt:show-result", Some(&payload))
}

/// Tell the sidebar plugin to close the search popup pane without dispatching
/// a result. Used when the user quits (Esc / Ctrl-C) and on any error path -
/// in both cases the plugin owns the close, so the pane disappears cleanly
/// rather than parking on Zellij's exit-code prompt.
fn send_close() -> Result<()> {
    if std::env::var_os("ZELLIJ").is_none() {
        return Ok(());
    }
    pipe_to_plugin("panopt:close-search", None)
}

/// Pipe a message to the sidebar plugin.
///
/// Deliberately omits `--plugin-configuration mode=todos` so the message
/// broadcasts to every sidebar plugin instance. `Alt-/` on a non-Todos
/// sidebar pane spawns the popup locally on that instance, so the holder of
/// `search_pane` is not necessarily Todos; broadcasting lets the actual
/// holder receive close-search / show-result regardless. Non-holders no-op
/// because their `search_pane` is `None`. The locked-content-pane keybind
/// in `up::SEARCH_BIND_TO` does the opposite for `panopt:open-search` -
/// filters to `mode=todos` so only Todos spawns from there.
fn pipe_to_plugin(name: &str, payload: Option<&str>) -> Result<()> {
    let mut cmd = Command::new("zellij");
    cmd.args(["action", "pipe", "--name", name]);
    if let Some(p) = payload {
        cmd.arg("--").arg(p);
    }
    let status = cmd
        .status()
        .with_context(|| format!("running `zellij action pipe` for `{name}`"))?;
    if !status.success() {
        return Err(anyhow!(
            "`zellij action pipe` for `{name}` exited with a failure"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_state() -> State {
        State {
            query: String::new(),
            rows: Vec::new(),
            cursor: 0,
            pending_since: None,
            last_error: None,
        }
    }

    #[test]
    fn typing_a_char_buffers_into_the_query_and_opens_the_debounce_window() {
        let mut state = fresh_state();
        let outcome = handle_key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty()),
            &mut state,
        );
        assert!(outcome.is_none());
        assert_eq!(state.query, "a");
        assert!(state.pending_since.is_some());
    }

    #[test]
    fn backspace_pops_a_char_and_no_op_on_empty_query_does_not_open_debounce() {
        let mut state = fresh_state();
        state.query = "x".into();
        handle_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()),
            &mut state,
        );
        assert!(state.query.is_empty());
        assert!(state.pending_since.is_some());

        state.pending_since = None;
        handle_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()),
            &mut state,
        );
        assert!(state.pending_since.is_none());
    }

    #[test]
    fn esc_quits_immediately() {
        let mut state = fresh_state();
        let outcome = handle_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
            &mut state,
        );
        assert!(matches!(outcome, Some(Outcome::Quit)));
    }

    #[test]
    fn ctrl_c_quits_but_lowercase_c_just_types() {
        let mut state = fresh_state();
        assert!(matches!(
            handle_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &mut state
            ),
            Some(Outcome::Quit)
        ));

        let mut state = fresh_state();
        handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::empty()),
            &mut state,
        );
        assert_eq!(state.query, "c");
    }

    #[test]
    fn enter_with_no_rows_does_nothing_and_with_rows_selects_under_cursor() {
        let mut state = fresh_state();
        assert!(handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &mut state,
        )
        .is_none());

        state.rows.push(Row {
            kind: Kind::Todo,
            id: 7,
            title: "x".into(),
            status: None,
            priority: None,
        });
        state.rows.push(Row {
            kind: Kind::Scratchpad,
            id: 9,
            title: "y".into(),
            status: None,
            priority: None,
        });
        state.cursor = 1;
        let out = handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &mut state,
        );
        match out {
            Some(Outcome::Select(row)) => {
                assert!(matches!(row.kind, Kind::Scratchpad));
                assert_eq!(row.id, 9);
            }
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn down_clamps_at_end_up_clamps_at_zero() {
        let mut state = fresh_state();
        state.rows.push(Row {
            kind: Kind::Todo,
            id: 1,
            title: "a".into(),
            status: None,
            priority: None,
        });
        state.rows.push(Row {
            kind: Kind::Todo,
            id: 2,
            title: "b".into(),
            status: None,
            priority: None,
        });
        for _ in 0..5 {
            handle_key(
                KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
                &mut state,
            );
        }
        assert_eq!(state.cursor, 1);
        for _ in 0..5 {
            handle_key(
                KeyEvent::new(KeyCode::Up, KeyModifiers::empty()),
                &mut state,
            );
        }
        assert_eq!(state.cursor, 0);
    }
}
