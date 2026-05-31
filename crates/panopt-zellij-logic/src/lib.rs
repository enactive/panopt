//! Transport-free logic for the PANopt Zellij sidebar.
//!
//! These are the pure pieces of the plugin - projection-line parsers, the
//! mode/filter/sort enums, the viewer-title resolver, and the shared
//! view-state codec - lifted out of `panopt-zellij` so they build and test on
//! the host. The plugin crate links the Zellij host ABI and so can never run
//! its own `cargo test`; this crate carries the unit tests that used to be
//! unreachable there. The plugin re-exports everything here via `use
//! panopt_zellij_logic::*` and adds the Zellij-bound view/controller on top.

/// Which kind of resource one plugin pane renders. Five plugin instances run
/// in parallel, one per `Mode`, each configured by the `mode "<kind>"` value
/// in the layout's plugin block. Zellij keys plugin identity on
/// `(URL, configuration)`, so the five panes are five distinct instances and
/// can be addressed individually by `zellij action pipe --plugin-configuration
/// "mode=<kind>"`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    #[default]
    Todos,
    Agents,
    Terminals,
    Commands,
    Notes,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "todos" => Some(Mode::Todos),
            "agents" => Some(Mode::Agents),
            "terminals" => Some(Mode::Terminals),
            "commands" => Some(Mode::Commands),
            "notes" => Some(Mode::Notes),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Todos => "Todos",
            Mode::Agents => "Agents",
            Mode::Terminals => "Terminals",
            Mode::Commands => "Commands",
            Mode::Notes => "Notes",
        }
    }

    /// One-letter slug that prefixes spawned viewer slot names so the five
    /// plugin instances cannot collide on the same `v<N>` suffix.
    pub fn letter(self) -> char {
        match self {
            Mode::Todos => 't',
            Mode::Agents => 'a',
            Mode::Terminals => 'r',
            Mode::Commands => 'c',
            Mode::Notes => 'n',
        }
    }

    /// Wire slug - the inverse of [`Mode::parse`]. Used to build the
    /// `--plugin-configuration mode=<slug>` narrowing on the `panopt:focus-pane`
    /// pipe so a focus request reaches exactly the target instance.
    pub fn slug(self) -> &'static str {
        match self {
            Mode::Todos => "todos",
            Mode::Agents => "agents",
            Mode::Terminals => "terminals",
            Mode::Commands => "commands",
            Mode::Notes => "notes",
        }
    }

    /// The `Alt-<n>` hotkey that focuses this pane, lazygit-style. Surfaced in
    /// the frame title (see [`PanoptPane::frame_title`]) and the `?` help so the
    /// gesture is discoverable from the pane itself.
    pub fn hotkey_hint(self) -> &'static str {
        match self {
            Mode::Todos => "alt+1",
            Mode::Agents => "alt+2",
            Mode::Terminals => "alt+3",
            Mode::Commands => "alt+4",
            Mode::Notes => "alt+5",
        }
    }

    /// Map an `Alt-<digit>` keypress to the sidebar pane it focuses. The
    /// inverse of [`Mode::hotkey_hint`]'s numbering.
    pub fn from_hotkey(c: char) -> Option<Mode> {
        match c {
            '1' => Some(Mode::Todos),
            '2' => Some(Mode::Agents),
            '3' => Some(Mode::Terminals),
            '4' => Some(Mode::Commands),
            '5' => Some(Mode::Notes),
            _ => None,
        }
    }
}

/// Status filter applied to the Todos pane. Each variant matches a wire
/// token from the projection's `- <status>, <priority>` suffix.
///
/// `OpenUnblocked` is the default working-set filter, but the sidebar reads
/// only the projection (no MCP), and the index doesn't carry blocker info;
/// in this pane `OpenUnblocked` degrades to "open" until the projection
/// learns to record blockers per row. The viewer pane on the right uses MCP
/// and applies the full blocker-aware filter, so the precise unblocked
/// view is available there.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TodoFilter {
    All,
    Open,
    #[default]
    OpenUnblocked,
    InProgress,
    Backlog,
    Draft,
    Completed,
    NotDone,
}

pub const ALL_TODO_FILTERS: [TodoFilter; 8] = [
    TodoFilter::All,
    TodoFilter::Open,
    TodoFilter::OpenUnblocked,
    TodoFilter::InProgress,
    TodoFilter::Backlog,
    TodoFilter::Draft,
    TodoFilter::Completed,
    TodoFilter::NotDone,
];

impl TodoFilter {
    pub fn label(self) -> &'static str {
        match self {
            TodoFilter::All => "all",
            TodoFilter::Open => "open",
            TodoFilter::OpenUnblocked => "open-unblocked",
            TodoFilter::InProgress => "in_progress",
            TodoFilter::Backlog => "backlog",
            TodoFilter::Draft => "draft",
            TodoFilter::Completed => "completed",
            TodoFilter::NotDone => "not_done",
        }
    }

    pub fn next(self) -> TodoFilter {
        let i = ALL_TODO_FILTERS
            .iter()
            .position(|f| *f == self)
            .unwrap_or(0);
        ALL_TODO_FILTERS[(i + 1) % ALL_TODO_FILTERS.len()]
    }

    pub fn prev(self) -> TodoFilter {
        let i = ALL_TODO_FILTERS
            .iter()
            .position(|f| *f == self)
            .unwrap_or(0);
        ALL_TODO_FILTERS[(i + ALL_TODO_FILTERS.len() - 1) % ALL_TODO_FILTERS.len()]
    }

    /// Whether the projection-index label passes this filter. The label is
    /// the trailing text after the link, e.g. `"the title - open, high"`.
    /// Without blocker info, `OpenUnblocked` is approximated as `Open`.
    pub fn includes_label(self, label: &str) -> bool {
        if matches!(self, TodoFilter::All) {
            return true;
        }
        let Some(status) = parse_status_suffix(label) else {
            // Missing / unparsable status: leave the entry visible so a
            // stray projection format never silently hides a real todo.
            return true;
        };
        match self {
            TodoFilter::All => true,
            TodoFilter::Open | TodoFilter::OpenUnblocked => status == "open",
            TodoFilter::InProgress => status == "in_progress",
            TodoFilter::Backlog => status == "backlog",
            TodoFilter::Draft => status == "draft",
            TodoFilter::Completed => status == "completed",
            TodoFilter::NotDone => status == "not_done",
        }
    }

    /// Stable integer code for the shared view-state file (todo #116). Indexes
    /// into [`ALL_TODO_FILTERS`], whose order is append-only, so old files keep
    /// decoding even as variants are added.
    pub fn to_wire(self) -> u8 {
        ALL_TODO_FILTERS
            .iter()
            .position(|f| *f == self)
            .unwrap_or(0) as u8
    }

    pub fn from_wire(code: u8) -> Option<TodoFilter> {
        ALL_TODO_FILTERS.get(code as usize).copied()
    }
}

/// Find where the trailing " - <suffix>" segment of a projection label
/// begins. Two shapes need to round-trip cleanly:
/// - "wire up auth - open, high" - normal case, look for the last " - ".
/// - "- open, high" - empty-title case (the projection's `{title}` slot was
///   empty, the parser already stripped one of the surrounding spaces), strip
///   the leading "- ".
///
/// Returns the byte index of the suffix payload's first character, or `None`
/// when the label has no recognized suffix marker.
pub fn suffix_start(label: &str) -> Option<usize> {
    if let Some(pos) = label.rfind(" - ") {
        Some(pos + 3)
    } else if label.starts_with("- ") {
        Some(2)
    } else {
        None
    }
}

/// Extract the wire status token from a projection-index label suffix like
/// `wire up auth - open, high`. Returns `None` for labels without a known
/// suffix; callers treat that as "do not hide."
pub fn parse_status_suffix(label: &str) -> Option<&str> {
    let start = suffix_start(label)?;
    let rest = &label[start..];
    let comma = rest.find(',').unwrap_or(rest.len());
    let token = rest[..comma].trim();
    matches!(
        token,
        "open" | "in_progress" | "backlog" | "draft" | "completed" | "not_done"
    )
    .then_some(token)
}

/// Extract the wire priority token from a projection-index label suffix
/// like `wire up auth - open, high, updated 2026-05-23 18:05:21`. Returns
/// `None` for labels without a known suffix. The suffix now carries a
/// third comma-separated token (`updated <ts>`), so we explicitly slice
/// the *second* token rather than "everything after the first comma".
pub fn parse_priority_suffix(label: &str) -> Option<&str> {
    let start = suffix_start(label)?;
    let rest = &label[start..];
    let first = rest.find(',')?;
    let after_first = &rest[first + 1..];
    let end = after_first.find(',').unwrap_or(after_first.len());
    let token = after_first[..end].trim();
    matches!(token, "high" | "medium" | "low").then_some(token)
}

/// Extract the `updated_at` timestamp from a projection-index label suffix
/// like `wire up auth - open, high, updated 2026-05-23 18:05:21`. Returns
/// `None` when the row has no recognizable `updated <ts>` token (older
/// projections that predate the timestamp suffix), so callers can degrade
/// gracefully on a stale on-disk file rather than panicking mid-sort.
pub fn parse_updated_suffix(label: &str) -> Option<&str> {
    let start = suffix_start(label)?;
    label[start..]
        .split(',')
        .map(str::trim)
        .find_map(|token| token.strip_prefix("updated "))
        .map(str::trim)
}

/// One axis of the two-level todo sort. The sidebar carries two of these
/// (level 1 / level 2) and applies them as a stable two-pass sort, so equal
/// keys on level 1 are broken by level 2.
///
/// The sidebar reads only the projection index, which carries status,
/// priority, and `updated_at` per row. The `Modified` axes compare on
/// `updated_at` directly (the daemon writes `datetime('now')` text, which
/// is lexicographically orderable). The `Created` axes still degrade to
/// **id order**, which is correct given per-project ids are monotonic and
/// never reused: `id asc ≡ creation order asc`. This mirrors the existing
/// `OpenUnblocked → Open` degradation in [`TodoFilter::includes_label`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TodoSort {
    #[default]
    PriorityDesc,
    CreatedAsc,
    CreatedDesc,
    ModifiedAsc,
    ModifiedDesc,
}

pub const ALL_TODO_SORTS: [TodoSort; 5] = [
    TodoSort::PriorityDesc,
    TodoSort::CreatedAsc,
    TodoSort::CreatedDesc,
    TodoSort::ModifiedAsc,
    TodoSort::ModifiedDesc,
];

impl TodoSort {
    /// Display label. `/` indicates ascending (low → high), `\` indicates
    /// descending (high → low); priority is single-direction (high → low)
    /// so it carries no suffix.
    pub fn label(self) -> &'static str {
        match self {
            TodoSort::PriorityDesc => "priority",
            TodoSort::CreatedAsc => "created-/",
            TodoSort::CreatedDesc => "created-\\",
            TodoSort::ModifiedAsc => "modified-/",
            TodoSort::ModifiedDesc => "modified-\\",
        }
    }

    pub fn next(self) -> TodoSort {
        let i = ALL_TODO_SORTS.iter().position(|x| *x == self).unwrap_or(0);
        ALL_TODO_SORTS[(i + 1) % ALL_TODO_SORTS.len()]
    }

    pub fn prev(self) -> TodoSort {
        let i = ALL_TODO_SORTS.iter().position(|x| *x == self).unwrap_or(0);
        ALL_TODO_SORTS[(i + ALL_TODO_SORTS.len() - 1) % ALL_TODO_SORTS.len()]
    }

    /// Stable integer code for the shared view-state file (todo #116), indexing
    /// into the append-only [`ALL_TODO_SORTS`].
    pub fn to_wire(self) -> u8 {
        ALL_TODO_SORTS.iter().position(|x| *x == self).unwrap_or(0) as u8
    }

    pub fn from_wire(code: u8) -> Option<TodoSort> {
        ALL_TODO_SORTS.get(code as usize).copied()
    }

    /// Compare two projection rows on this axis. The sidebar's row shape is
    /// `(id, label)`; priority comes from [`parse_priority_suffix`], the
    /// `updated_at` timestamp from [`parse_updated_suffix`], and `Created`
    /// degrades to id comparison (see the type doc). Rows missing the
    /// `updated <ts>` token (stale projection) fall back to id so a
    /// half-rewritten index doesn't panic.
    pub fn cmp_rows(self, a: &(u64, String), b: &(u64, String)) -> std::cmp::Ordering {
        match self {
            TodoSort::PriorityDesc => {
                let rank = |label: &str| match parse_priority_suffix(label) {
                    Some("high") => 3,
                    Some("medium") => 2,
                    Some("low") => 1,
                    _ => 0,
                };
                rank(&b.1).cmp(&rank(&a.1))
            }
            TodoSort::CreatedAsc => a.0.cmp(&b.0),
            TodoSort::CreatedDesc => b.0.cmp(&a.0),
            TodoSort::ModifiedAsc => match (parse_updated_suffix(&a.1), parse_updated_suffix(&b.1))
            {
                (Some(ua), Some(ub)) => ua.cmp(ub),
                _ => a.0.cmp(&b.0),
            },
            TodoSort::ModifiedDesc => {
                match (parse_updated_suffix(&a.1), parse_updated_suffix(&b.1)) {
                    (Some(ua), Some(ub)) => ub.cmp(ua),
                    _ => b.0.cmp(&a.0),
                }
            }
        }
    }
}

/// A parsed `.panopt/processes.md` line. The line format is preserved from
/// the pre-V6 `roster.md` so the existing `[kind] #id label` parser still
/// works; any trailing `(from #N)` is dropped from `label`.
pub struct ProcessRow {
    pub kind: String,
    pub id: u64,
    pub label: String,
}

/// What a content pane is, derived from the command it was launched with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaneRole {
    /// The shared `panopt _viewer` document pane.
    Viewer,
    /// An ad-hoc `panopt _agent` pane, started with `a`.
    Agent,
    /// A `panopt _process-run <id>` pane, by process id.
    Process(u64),
    /// A plain terminal the user opened.
    Shell,
}

pub fn is_user_shell(basename: &str) -> bool {
    matches!(
        basename,
        "zsh" | "bash" | "fish" | "sh" | "dash" | "ksh" | "tcsh" | "nu" | "ash" | "elvish"
    )
}

pub fn classify_pane(command: Option<&str>) -> PaneRole {
    let Some(cmd) = command else {
        return PaneRole::Shell;
    };
    if cmd.contains("_viewer") {
        PaneRole::Viewer
    } else if cmd.contains("_process-run") {
        match cmd
            .split_whitespace()
            .filter_map(|t| t.parse::<u64>().ok())
            .next_back()
        {
            Some(id) => PaneRole::Process(id),
            None => PaneRole::Shell,
        }
    } else if cmd.contains("_agent") {
        PaneRole::Agent
    } else {
        PaneRole::Shell
    }
}

/// The presentation snapshot shared across every client attached to the
/// session, persisted per mode so all instances render the same sidebar (todo
/// #116). `mirror_session` mirrors focus and terminal panes, but each client's
/// sidebar is its own plugin instance with its own selection/filter/scroll,
/// which Zellij does not mirror - this is how those stay in sync. All fields
/// are plain integers/bools, so a hand-rolled flat-object serializer matches
/// the rest of `.cockpit/` without a JSON dep. `seq` is a monotonic
/// last-writer-wins counter.
pub struct ViewState {
    pub cursor: usize,
    pub scroll: usize,
    pub filter: TodoFilter,
    pub sort_1: TodoSort,
    pub sort_2: TodoSort,
    pub show_help: bool,
    pub seq: u64,
}

/// Scan a single `"key":<digits>` field out of the flat view-state object.
/// Returns `None` for an absent key so callers can default it.
pub fn view_field(body: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse::<u64>().ok()
}

/// Parse the agent-label projection back into `(tid, label)` pairs. Tolerant
/// of an empty/malformed file: returns an empty iterator on any parse error.
pub fn parse_agent_labels(body: &str) -> Vec<(u32, String)> {
    let body = body.trim();
    let Some(inner) = body.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
        return Vec::new();
    };
    if inner.trim().is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for entry in split_top_level(inner, ',') {
        let entry = entry.trim();
        let Some(colon) = entry.find(':') else {
            continue;
        };
        let key = entry[..colon].trim();
        let value = entry[colon + 1..].trim();
        let Some(tid) = key
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(label) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
            continue;
        };
        let unescaped = label.replace("\\\"", "\"").replace("\\\\", "\\");
        out.push((tid, unescaped));
    }
    out
}

/// Split `body` on `sep`, respecting `"..."` strings so a separator inside a
/// label does not split the entry. The projection writer escapes `"` and `\`
/// in labels, so the only thing this needs to dodge is unescaped `,` inside
/// a string.
pub fn split_top_level(body: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut escape = false;
    for c in body.chars() {
        if escape {
            current.push(c);
            escape = false;
            continue;
        }
        if c == '\\' && in_string {
            current.push(c);
            escape = true;
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            current.push(c);
            continue;
        }
        if c == sep && !in_string {
            out.push(std::mem::take(&mut current));
            continue;
        }
        current.push(c);
    }
    out.push(current);
    out
}

pub fn parse_viewer_slot(command: Option<&str>) -> Option<String> {
    let cmd = command?;
    let mut tokens = cmd.split_whitespace();
    while let Some(t) = tokens.next() {
        if t == "--slot" {
            return tokens.next().map(|s| s.to_string());
        }
    }
    None
}

/// Parse a viewer routing file body - the JSON-ish payload written by
/// [`write_routing`] - back into `(kind, id)`. Tolerant of an empty or
/// malformed body: returns `(None, None)` so callers fall back to the
/// generic `Viewer` title.
pub fn parse_viewer_routing(body: &str) -> (Option<String>, Option<u64>) {
    let trimmed = body.trim();
    let Some(inner) = trimmed.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
        return (None, None);
    };
    let mut kind: Option<String> = None;
    let mut id: Option<u64> = None;
    for entry in inner.split(',') {
        let entry = entry.trim();
        let Some(colon) = entry.find(':') else {
            continue;
        };
        let key = entry[..colon].trim();
        let value = entry[colon + 1..].trim();
        let key = key.strip_prefix('"').and_then(|s| s.strip_suffix('"'));
        match key {
            Some("kind") => {
                kind = value
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .map(|s| s.to_string());
            }
            Some("id") => {
                id = value.parse::<u64>().ok();
            }
            _ => {}
        }
    }
    (kind, id)
}

/// Compose the viewer-pane title for a `(kind, id)` routing pair. The
/// projection indexes are looked up so the title carries the resource's
/// own name (e.g. `Todo #30 - fixup pane titles`) rather than just its id.
pub fn viewer_title_for(
    kind: Option<&str>,
    id: Option<u64>,
    todos: &[(u64, String)],
    notes: &[(u64, String)],
) -> String {
    match (kind, id) {
        (None, _) | (Some("empty"), _) => "Viewer".to_string(),
        (Some("todo"), Some(id)) => match lookup_title(todos, id) {
            Some(t) => format!("Todo #{id} - {t}"),
            None => format!("Todo #{id}"),
        },
        (Some("note"), Some(id)) => match lookup_title(notes, id) {
            Some(t) => format!("Note #{id} - {t}"),
            None => format!("Note #{id}"),
        },
        (Some("todo-list"), _) => "Todos".to_string(),
        (Some("note-list"), _) => "Notes".to_string(),
        (Some("new-todo"), _) => "New todo".to_string(),
        (Some("new-note"), _) => "New note".to_string(),
        _ => "Viewer".to_string(),
    }
}

/// Look up an index entry's label by id, stripping the trailing
/// `" - status, priority"` (todos) or `" - updated ..."` (notes)
/// suffix that the projection format appends. The result is the bare title
/// the user typed.
pub fn lookup_title(index: &[(u64, String)], id: u64) -> Option<String> {
    let label = index.iter().find(|(i, _)| *i == id).map(|(_, l)| l)?;
    // The title is everything BEFORE the suffix marker. " - " (with leading
    // space) covers the normal case; a label that starts directly with "- "
    // is the empty-title case, so the title is "".
    let trimmed = if let Some(dash) = label.rfind(" - ") {
        label[..dash].trim()
    } else if label.starts_with("- ") {
        ""
    } else {
        label.trim()
    };
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Prefix `label` with `kind: ` unless `label` already begins with that
/// kind (case-insensitive). Avoids the silly `Agent: Agent 1` for the
/// default ad-hoc-agent label while still tagging user-named ones like
/// `Agent: panopt-bot`.
pub fn kind_prefixed_title(kind: &str, label: &str) -> String {
    let label = label.trim();
    if label.is_empty() {
        return kind.to_string();
    }
    let lk = label.to_lowercase();
    if lk == kind.to_lowercase() || lk.starts_with(&format!("{} ", kind.to_lowercase())) {
        label.to_string()
    } else {
        format!("{kind}: {label}")
    }
}

pub fn parse_index_line(line: &str) -> Option<(u64, String)> {
    let line = line.trim();
    if !line.starts_with("- [") {
        return None;
    }
    let hash = line.find("[#")? + 2;
    let close = line[hash..].find(']')? + hash;
    let id: u64 = line[hash..close].parse().ok()?;
    let label_at = line[close..].find(") ")? + close + 2;
    // The note/todo projections render as `- [#N](path) {title} - <sfx>`,
    // so an empty title leaves the raw chunk starting with a space (one of the
    // two literal spaces around `{title}`). `.trim()` collapses both the
    // empty-title case and the non-empty case onto the same shape - a label
    // like "- open, medium" (no title) or "wire it - open, medium" (with
    // title); the suffix parsers below detect "no title" via the leading "- ".
    let label = line.get(label_at..).unwrap_or("").trim().to_string();
    Some((id, label))
}

/// Parse one `- [kind] #id label [(from #N)]` line from `processes.md`. The
/// trailing `(from #N)` (when present) names the source agent tool and is
/// dropped from `label`.
pub fn parse_process_line(line: &str) -> Option<ProcessRow> {
    let rest = line.trim().strip_prefix("- [")?;
    let close = rest.find(']')?;
    let kind = rest[..close].to_string();
    let after = rest[close + 1..].trim_start().strip_prefix('#')?;
    let space = after.find(' ')?;
    let id: u64 = after[..space].parse().ok()?;
    let mut label = after[space + 1..].trim().to_string();
    if let Some(from_at) = label.rfind(" (from #") {
        if label.ends_with(')') {
            label.truncate(from_at);
        }
    }
    Some(ProcessRow { kind, id, label })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_todo_index_line() {
        let (id, label) = parse_index_line(
            "- [ ] [#3](todos/3.md) wire the form - open, high, updated 2026-05-23 18:05:21",
        )
        .unwrap();
        assert_eq!(id, 3);
        assert_eq!(
            label,
            "wire the form - open, high, updated 2026-05-23 18:05:21"
        );
    }

    #[test]
    fn parse_priority_suffix_skips_the_trailing_updated_token() {
        // The third token (`updated <ts>`) was added so the sidebar can sort
        // by modified; the priority parser must keep returning the middle
        // token rather than "high, updated 2026-...".
        let label = "wire the form - open, high, updated 2026-05-23 18:05:21";
        assert_eq!(parse_priority_suffix(label), Some("high"));
        assert_eq!(parse_status_suffix(label), Some("open"));
    }

    #[test]
    fn parse_updated_suffix_extracts_the_timestamp() {
        let label = "wire the form - open, high, updated 2026-05-23 18:05:21";
        assert_eq!(parse_updated_suffix(label), Some("2026-05-23 18:05:21"));
    }

    #[test]
    fn parse_updated_suffix_returns_none_when_absent() {
        // Stale on-disk projection (predates the timestamp) - the parser
        // returns None so cmp_rows degrades to id ordering instead of
        // panicking.
        assert_eq!(parse_updated_suffix("wire the form - open, high"), None);
        assert_eq!(parse_updated_suffix("plain title"), None);
    }

    #[test]
    fn modified_desc_sorts_by_timestamp_not_id() {
        // The lower-id row has the newer timestamp; ModifiedDesc must put it
        // first. If the cmp falls back to id, this returns Greater (b before
        // a) instead of Less, and the assertion fails.
        let a = (
            1u64,
            "wire the form - open, high, updated 2026-05-29 09:00:00".to_string(),
        );
        let b = (
            2u64,
            "write readme - open, medium, updated 2026-05-21 10:00:00".to_string(),
        );
        assert_eq!(
            TodoSort::ModifiedDesc.cmp_rows(&a, &b),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            TodoSort::ModifiedAsc.cmp_rows(&a, &b),
            std::cmp::Ordering::Greater
        );
        // Sanity check that Created still uses id, unchanged.
        assert_eq!(
            TodoSort::CreatedAsc.cmp_rows(&a, &b),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn parses_a_note_index_line() {
        let (id, label) = parse_index_line("- [#7](note/7.md) design notes").unwrap();
        assert_eq!(id, 7);
        assert_eq!(label, "design notes");
    }

    #[test]
    fn parses_a_note_index_line_with_updated_timestamp() {
        let (id, label) =
            parse_index_line("- [#1](note/1.md) Sample Notes - updated 2026-05-23 18:05:21")
                .unwrap();
        assert_eq!(id, 1);
        assert_eq!(label, "Sample Notes - updated 2026-05-23 18:05:21");
    }

    #[test]
    fn ignores_non_index_lines() {
        assert!(parse_index_line("# Todos").is_none());
        assert!(parse_index_line("_(no todos)_").is_none());
        assert!(parse_index_line("").is_none());
    }

    #[test]
    fn empty_title_note_line_keeps_the_suffix_in_the_label() {
        // After a user clears the title field, the projection renders
        // `- [#N](note/N.md)  - updated <ts>` (double space). The
        // parser strips one of the spaces, leaving the label as
        // "- updated <ts>" - suffix-only, no title.
        let (id, label) =
            parse_index_line("- [#73](note/73.md)  - updated 2026-05-27 07:00:00").unwrap();
        assert_eq!(id, 73);
        assert_eq!(label, "- updated 2026-05-27 07:00:00");
        // lookup_title recognises the leading "- " as the empty-title shape
        // and returns None so the pane title falls back to "Note #N".
        let pads = vec![(73u64, label)];
        assert!(lookup_title(&pads, 73).is_none());
    }

    #[test]
    fn empty_title_todo_line_still_parses_status_and_priority() {
        // Empty title for a todo: "- [ ] [#75](todos/75.md)  - open, medium".
        // The sort path needs the priority and the filter path needs the
        // status; without `suffix_start`'s empty-title branch the label
        // "- open, medium" would parse as no-suffix and the todo would sort
        // to the bottom (rank 0) regardless of its real priority.
        let (id, label) = parse_index_line("- [ ] [#75](todos/75.md)  - open, medium").unwrap();
        assert_eq!(id, 75);
        assert_eq!(label, "- open, medium");
        assert_eq!(parse_status_suffix(&label), Some("open"));
        assert_eq!(parse_priority_suffix(&label), Some("medium"));
        let todos = vec![(75u64, label)];
        assert!(lookup_title(&todos, 75).is_none());
    }

    #[test]
    fn suffix_start_picks_the_right_marker_for_each_shape() {
        // Normal case: title + suffix, separator is " - " (3 chars).
        assert_eq!(suffix_start("wire up auth - open, high"), Some(15));
        // Empty-title case: label is the suffix only, "- " consumed (2 chars).
        assert_eq!(suffix_start("- open, high"), Some(2));
        // Title with embedded " - " plus a trailing suffix: rfind picks the
        // rightmost, which is the projection-level separator.
        let s = "a - b - open, high";
        assert_eq!(suffix_start(s), Some(s.rfind(" - ").unwrap() + 3));
        // No suffix at all.
        assert!(suffix_start("plain title").is_none());
    }

    #[test]
    fn parses_a_process_line() {
        let row = parse_process_line("- [agent] #1 NASTL-Mediator").unwrap();
        assert_eq!(row.kind, "agent");
        assert_eq!(row.id, 1);
        assert_eq!(row.label, "NASTL-Mediator");
    }

    #[test]
    fn parses_a_process_line_with_a_from_suffix() {
        let row = parse_process_line("- [agent] #4 NASTL-Mediator (from #3)").unwrap();
        assert_eq!(row.kind, "agent");
        assert_eq!(row.id, 4);
        assert_eq!(row.label, "NASTL-Mediator");
    }

    #[test]
    fn ignores_non_process_lines() {
        assert!(parse_process_line("# Processes").is_none());
        assert!(parse_process_line("_(no processes)_").is_none());
    }

    #[test]
    fn classify_pane_reads_the_launch_command() {
        assert_eq!(
            classify_pane(Some("/bin/panopt _viewer --slot main --port 7600")),
            PaneRole::Viewer
        );
        assert_eq!(
            classify_pane(Some("/bin/panopt _process-run --port 7600 5")),
            PaneRole::Process(5)
        );
        assert_eq!(
            classify_pane(Some("/bin/panopt _agent --id mediator-1a2b")),
            PaneRole::Agent
        );
        assert_eq!(classify_pane(Some("/bin/zsh -l")), PaneRole::Shell);
        assert_eq!(classify_pane(None), PaneRole::Shell);
    }

    #[test]
    fn parse_viewer_routing_reads_kind_and_id() {
        assert_eq!(
            parse_viewer_routing(r#"{"kind":"todo","id":30}"#),
            (Some("todo".to_string()), Some(30))
        );
        assert_eq!(
            parse_viewer_routing(r#"{"kind":"empty"}"#),
            (Some("empty".to_string()), None)
        );
        // Order is not constrained by the writer, but be tolerant anyway.
        assert_eq!(
            parse_viewer_routing(r#"{"id":7,"kind":"note"}"#),
            (Some("note".to_string()), Some(7))
        );
    }

    #[test]
    fn parse_viewer_routing_tolerates_garbage() {
        assert_eq!(parse_viewer_routing(""), (None, None));
        assert_eq!(parse_viewer_routing("not json"), (None, None));
        // Missing kind: returns just the id, the caller falls back to "Viewer".
        assert_eq!(parse_viewer_routing(r#"{"id":3}"#), (None, Some(3)));
    }

    #[test]
    fn viewer_title_for_each_known_kind() {
        let todos = vec![(30u64, "fixup pane titles - open, high".to_string())];
        let pads = vec![(5u64, "design notes - updated 2026-05-23".to_string())];
        assert_eq!(viewer_title_for(None, None, &todos, &pads), "Viewer");
        assert_eq!(
            viewer_title_for(Some("empty"), None, &todos, &pads),
            "Viewer"
        );
        assert_eq!(
            viewer_title_for(Some("todo"), Some(30), &todos, &pads),
            "Todo #30 - fixup pane titles"
        );
        // Unknown id: still useful, just no name.
        assert_eq!(
            viewer_title_for(Some("todo"), Some(99), &todos, &pads),
            "Todo #99"
        );
        assert_eq!(
            viewer_title_for(Some("note"), Some(5), &todos, &pads),
            "Note #5 - design notes"
        );
        assert_eq!(
            viewer_title_for(Some("todo-list"), None, &todos, &pads),
            "Todos"
        );
        assert_eq!(
            viewer_title_for(Some("note-list"), None, &todos, &pads),
            "Notes"
        );
        assert_eq!(
            viewer_title_for(Some("new-todo"), None, &todos, &pads),
            "New todo"
        );
        assert_eq!(
            viewer_title_for(Some("new-note"), None, &todos, &pads),
            "New note"
        );
        // Unknown kind: do not invent a name, just label generically.
        assert_eq!(
            viewer_title_for(Some("rumor"), None, &todos, &pads),
            "Viewer"
        );
    }

    #[test]
    fn kind_prefixed_title_avoids_doubling_the_kind_word() {
        // Default ad-hoc agent label already names itself "Agent N" - the
        // prefix would duplicate, so use the label verbatim.
        assert_eq!(kind_prefixed_title("Agent", "Agent 1"), "Agent 1");
        // User-named: the prefix carries the kind, so the user sees both.
        assert_eq!(
            kind_prefixed_title("Agent", "panopt-bot"),
            "Agent: panopt-bot"
        );
        // Process-row labels from `panopt process add` typically lack the
        // kind word, so the prefix is what makes the role legible.
        assert_eq!(
            kind_prefixed_title("Command", "just check"),
            "Command: just check"
        );
        // Case-insensitive match so user typing `agent foo` still gets
        // collapsed onto the canonical "Agent foo".
        assert_eq!(kind_prefixed_title("Agent", "agent foo"), "agent foo");
    }

    #[test]
    fn mode_hotkey_mapping_is_consistent() {
        // `from_hotkey` and `hotkey_hint` are inverses, and `slug` round-trips
        // through `parse` - the focus pipe relies on both to reach exactly the
        // target instance (todo #110).
        for (digit, mode) in [
            ('1', Mode::Todos),
            ('2', Mode::Agents),
            ('3', Mode::Terminals),
            ('4', Mode::Commands),
            ('5', Mode::Notes),
        ] {
            assert_eq!(Mode::from_hotkey(digit), Some(mode));
            assert_eq!(mode.hotkey_hint(), format!("alt+{digit}"));
            assert_eq!(Mode::parse(mode.slug()), Some(mode));
        }
        assert_eq!(Mode::from_hotkey('0'), None);
        assert_eq!(Mode::from_hotkey('6'), None);
        assert_eq!(Mode::from_hotkey('a'), None);
    }

    #[test]
    fn parse_viewer_slot_extracts_the_slot_token() {
        assert_eq!(
            parse_viewer_slot(Some("/bin/panopt _viewer --slot main --port 7600")),
            Some("main".to_string())
        );
        assert_eq!(
            parse_viewer_slot(Some("/bin/panopt _viewer --port 7600 --slot vt2")),
            Some("vt2".to_string())
        );
        assert_eq!(parse_viewer_slot(Some("/bin/zsh -l")), None);
        assert_eq!(parse_viewer_slot(None), None);
    }

    #[test]
    fn agent_labels_parser_tolerates_malformed_input() {
        assert!(parse_agent_labels("").is_empty());
        assert!(parse_agent_labels("not json").is_empty());
        assert!(parse_agent_labels("{}").is_empty());
    }

    #[test]
    fn todo_filter_wire_codes_round_trip() {
        for f in ALL_TODO_FILTERS {
            assert_eq!(TodoFilter::from_wire(f.to_wire()), Some(f));
        }
        // An out-of-range code from a newer writer decodes to None so the
        // reader can fall back to a default rather than panicking.
        assert_eq!(TodoFilter::from_wire(200), None);
    }

    #[test]
    fn todo_sort_wire_codes_round_trip() {
        for s in ALL_TODO_SORTS {
            assert_eq!(TodoSort::from_wire(s.to_wire()), Some(s));
        }
        assert_eq!(TodoSort::from_wire(200), None);
    }

    #[test]
    fn view_field_reads_each_key_and_tolerates_absence() {
        let body = r#"{"cursor":4,"scroll":2,"filter":3,"sort1":1,"sort2":0,"help":1,"seq":17}"#;
        assert_eq!(view_field(body, "cursor"), Some(4));
        assert_eq!(view_field(body, "scroll"), Some(2));
        assert_eq!(view_field(body, "filter"), Some(3));
        assert_eq!(view_field(body, "seq"), Some(17));
        assert_eq!(view_field(body, "help"), Some(1));
        // Absent key and total garbage both yield None.
        assert_eq!(view_field(body, "missing"), None);
        assert_eq!(view_field("not json", "seq"), None);
        assert_eq!(view_field("", "seq"), None);
    }

    #[test]
    fn agent_labels_roundtrip_through_the_projection_format() {
        use std::collections::BTreeMap;
        let mut input = BTreeMap::new();
        input.insert(4u32, "Mediator".to_string());
        input.insert(9u32, "Edge \"case\" with, commas".to_string());
        let mut body = String::from("{");
        for (i, (tid, label)) in input.iter().enumerate() {
            if i > 0 {
                body.push(',');
            }
            let safe = label.replace('\\', "\\\\").replace('"', "\\\"");
            body.push_str(&format!("\"{tid}\":\"{safe}\""));
        }
        body.push('}');
        let parsed: BTreeMap<u32, String> = parse_agent_labels(&body).into_iter().collect();
        assert_eq!(parsed, input);
    }
}
