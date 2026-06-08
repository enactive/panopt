//! Transport-free logic for the PANopt Zellij sidebar.
//!
//! These are the pure pieces of the plugin - projection-line parsers, the
//! mode/filter/sort enums, the viewer-title resolver, and the shared
//! view-state codec - lifted out of `panopt-zellij` so they build and test on
//! the host. The plugin crate links the Zellij host ABI and so can never run
//! its own `cargo test`; this crate carries the unit tests that used to be
//! unreachable there. The plugin re-exports everything here via `use
//! panopt_zellij_logic::*` and adds the Zellij-bound view/controller on top.

use regex::Regex;
use std::collections::BTreeMap;

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
/// `Active` is the default working-set filter: the todos you can actually act
/// on right now - open-and-unblocked, plus whatever is already in progress.
/// The sidebar reads only the projection (no MCP), and the index doesn't
/// carry blocker info; in this pane the open-unblocked half of `Active`
/// degrades to plain "open" until the projection learns to record blockers
/// per row. The viewer pane on the right uses MCP and applies the full
/// blocker-aware filter, so the precise unblocked view is available there.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TodoFilter {
    All,
    Open,
    #[default]
    Active,
    InProgress,
    Backlog,
    Draft,
    Completed,
    NotDone,
}

pub const ALL_TODO_FILTERS: [TodoFilter; 8] = [
    TodoFilter::All,
    TodoFilter::Open,
    TodoFilter::Active,
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
            TodoFilter::Active => "active",
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
    /// Without blocker info, the open-unblocked half of `Active` is
    /// approximated as plain "open".
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
            TodoFilter::Open => status == "open",
            TodoFilter::Active => status == "open" || status == "in_progress",
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
/// `Active → open|in_progress` degradation in [`TodoFilter::includes_label`].
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
/// works; any trailing `(from #N)` is dropped from `label` and any trailing
/// ` · <status>` (todo #141) is lifted into `status`.
#[derive(Debug, Default, Clone)]
pub struct ProcessRow {
    pub kind: String,
    pub id: u64,
    pub label: String,
    /// Lifecycle status (`starting`/`running`/`stopped`/...), or `None` for a
    /// row that predates lifecycle ownership. Drives the cockpit's reconcile of
    /// a `starting` row into a pane.
    pub status: Option<String>,
    /// The agent type key (`type:<tool_type>` segment, todo #142), present only
    /// for agent rows backed by a config. Tells the status reconciler which
    /// profile's patterns to classify this instance's output with.
    pub tool_type: Option<String>,
    /// The last classified activity (`state:<agent_state>` segment, todo #142):
    /// `thinking`/`idle`/`waiting`/`done`. `None` until first observed.
    pub agent_state: Option<String>,
    /// The instance's stable agent id (`agent:<name>` segment, todo #142) - the
    /// process row's `name`, which is also the `--id` every agent stamps onto
    /// its `panopt _mcp-proxy`/`_agent` invocation. This is the join key the
    /// status observer uses to bind a pane to its row: the pane command Zellij
    /// surfaces is unreliable (it often reports the agent's `_mcp-proxy` child,
    /// not the `_process-run` shim), but that child still carries `--id <name>`,
    /// so matching it against this field finds the pane regardless. `None` for
    /// rows with no name (command/terminal kinds, migrated rows).
    pub agent_id: Option<String>,
    /// The backing agent config id, lifted from the ` (from #N)` suffix
    /// `render_processes_md` writes for instances spawned from a config. This is
    /// the join key the Agents pane uses to bind a config to a live instance. The
    /// daemon is a 1:N factory (a config can have several live instances, todo
    /// #190); the config-centric pane binds to a representative one (the first).
    /// `None` for command/terminal rows and any instance with no backing config.
    pub agent_tool_id: Option<u64>,
    /// The "sitting idle for N" age (`idle:<age>` segment, bug #163), present
    /// only while the agent is in the `idle` state - the daemon computes it as
    /// `now - state_since` and projects the formatted age. The Agents pane
    /// appends it to the row's status so a parked agent shows how long it has
    /// been waiting (#143), consistent with the roster's `(idle X)`. `None`
    /// whenever the agent is not idle.
    pub idle: Option<String>,
}

/// A parsed `.panopt/agent_tools.md` line: one durable agent config (the
/// config layer of the two-layer model). The Agents pane is config-centric -
/// each config *is* an agent - so this is the row it renders, joined to a
/// representative live [`ProcessRow`] by `id == agent_tool_id` (the daemon is a
/// 1:N factory, todo #190; surfacing every instance is a follow-up).
/// Line format (see `render_agent_tools_md`): `- #<id> <label> [<flag>]<cmd>`.
pub struct ConfigRow {
    pub id: u64,
    pub label: String,
    /// Whether the config is offered for spawning (`[enabled]` vs `[disabled]`).
    pub enabled: bool,
}

/// Parse one `.panopt/agent_tools.md` config line into a [`ConfigRow`], or
/// `None` for the header / `_(no agent tools)_` placeholder / any malformed
/// line. The label runs up to the ` [<flag>]` segment; a trailing command (when
/// the config carries one) is ignored - the pane shows the agent, not its argv.
pub fn parse_config_line(line: &str) -> Option<ConfigRow> {
    let rest = line.trim().strip_prefix("- #")?;
    let space = rest.find(' ')?;
    let id: u64 = rest[..space].parse().ok()?;
    let after = &rest[space + 1..];
    let bracket = after.find(" [")?;
    let label = after[..bracket].trim().to_string();
    let flag_rest = &after[bracket + 2..];
    let close = flag_rest.find(']')?;
    let enabled = &flag_rest[..close] == "enabled";
    Some(ConfigRow { id, label, enabled })
}

/// The Agents-pane row label for a config: `#<id> <name>`, plus a ` · <status>`
/// suffix drawn from its live instance when one exists - the agent's classified
/// activity (`thinking`/`waiting`/...) if observed, else the lifecycle status
/// (`starting`/`running`). A config with no live instance shows just `#<id>
/// <name>`. The leading `#<id>` matches the `#N <label>` shape the Todos and
/// Notes panes use, so every sidebar row is addressable by the same `#N` an
/// operator (or another agent) quotes elsewhere.
///
/// While the agent sits `idle`, the daemon's "idle for N" age rides along as
/// ` · idle <age>` (#143) - the same presence cue the roster shows as
/// `(idle X)` (#83), so a parked agent reads how long it has been waiting, not
/// merely that it is waiting. The age only rides the `idle` state because that
/// is the only state the daemon projects it for.
pub fn agent_config_label(config: &ConfigRow, inst: Option<&ProcessRow>) -> String {
    let base = format!("#{} {}", config.id, config.label);
    let Some(inst) = inst else {
        return base;
    };
    let Some(state) = inst
        .agent_state
        .as_deref()
        .or(inst.status.as_deref())
        .filter(|s| !s.is_empty())
    else {
        return base;
    };
    match inst
        .idle
        .as_deref()
        .filter(|_| inst.agent_state.as_deref() == Some("idle"))
    {
        Some(age) => format!("{base} · {state} {age}"),
        None => format!("{base} · {state}"),
    }
}

/// One parsed line of `.panopt/.cockpit/inputs.jsonl` (todo #160): a queued
/// input bound for a running instance's pane. The plugin writes `content` into
/// process `process_id`'s pane and acks `seq`.
pub struct PendingInputRow {
    pub seq: i64,
    pub process_id: u64,
    pub content: String,
}

/// Parse one JSON line of the input-queue projection, or `None` for a blank or
/// malformed line. The daemon writes `{"seq":N,"process_id":N,"content":"..."}`
/// with `serde_json`, so it is parsed the same way - hand-splitting would
/// mishandle a `content` that itself contains commas, quotes, or newlines.
pub fn parse_input_line(line: &str) -> Option<PendingInputRow> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    Some(PendingInputRow {
        seq: v.get("seq")?.as_i64()?,
        process_id: v.get("process_id")?.as_u64()?,
        content: v.get("content")?.as_str()?.to_string(),
    })
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
    } else if let Some(id) = instance_id_from_command(cmd) {
        // A first-class agent launched directly with its rendered per-instance
        // config - e.g. `claude --mcp-config .../instances/158/mcp_config` -
        // never passes through the `_process-run` reconcile shim, so the
        // `instances/<id>/` directory the spawn-spec interpreter materializes
        // its files into is the only back-reference from the pane to its
        // process id. Tying the pane to `Process(id)` here is what lets the
        // status observer (#142) find it via `process_pane`; without it such
        // agents fall through to `Shell` and are never observed. See
        // `panopt::paths::instance_dir`.
        PaneRole::Process(id)
    } else if cmd.contains("_agent") {
        PaneRole::Agent
    } else {
        PaneRole::Shell
    }
}

/// Recover the process id from the `.../instances/<id>/...` path the spawn-spec
/// interpreter bakes into a directly-launched agent's command. The id is the
/// path segment immediately after `instances`; returns `None` when no such
/// segment is present (the common case for shells, viewers, and `_agent`
/// panes). Kept tolerant of a non-numeric follow-on segment so an unrelated
/// `instances` path never panics or misclassifies.
fn instance_id_from_command(cmd: &str) -> Option<u64> {
    let mut segments = cmd.split('/');
    while let Some(seg) = segments.next() {
        if seg == "instances" {
            return segments.next()?.parse::<u64>().ok();
        }
    }
    None
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
        // The agent-config form carries the config's name in its own fields, so
        // the pane title stays id-only (the config index isn't in scope here).
        (Some("agent-config"), Some(id)) => format!("Agent config #{id}"),
        (Some("new-agent-config"), _) => "New agent config".to_string(),
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

/// Drop a trailing instance id from a process label. The daemon names
/// instances by appending their own id (`good-224`, `verify-submit #220`), so
/// the id is already baked into `label`; we strip it before re-adding an
/// explicit `#<id>` to avoid the doubled `good-224` / `#224` read. Only a
/// *separated* trailing id is removed (`-224`, ` #220`, ` 193`) or an id that
/// is the whole label - bare digits that merely end a name (`abc224`) are left
/// alone, since they are part of the name, not a disambiguator.
pub fn strip_trailing_id(label: &str, id: u64) -> String {
    let label = label.trim();
    if let Some(prefix) = label.strip_suffix(&id.to_string()) {
        let trimmed = prefix.trim_end_matches(['#', '-', ' ']);
        if trimmed.len() < prefix.len() || prefix.is_empty() {
            return trimmed.to_string();
        }
    }
    label.to_string()
}

/// Title for a process (agent/command/terminal) pane: `<Kind> #<id> - <name>`,
/// matching the `Todo #N - <title>` / `Note #N - <title>` shape
/// [`viewer_title_for`] gives document panes. Surfacing the `#<id>` lets an
/// operator map a process id quoted on the coordination plane (`dispose #224`)
/// to the pane on screen. The daemon-appended id is stripped from `name` first
/// (see [`strip_trailing_id`]) so it shows exactly once; a name that was *only*
/// the id collapses to the bare `<Kind> #<id>`.
pub fn process_pane_title_for(kind: &str, id: u64, name: &str) -> String {
    let bare = strip_trailing_id(name, id);
    if bare.is_empty() {
        format!("{kind} #{id}")
    } else {
        format!("{kind} #{id} - {bare}")
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

/// Parse one `- [kind] #id label [(from #N)] [· status]` line from
/// `processes.md`. The trailing ` · <status>` (when present) is the lifecycle
/// status and is lifted into [`ProcessRow::status`]; the trailing `(from #N)`
/// names the source agent tool and is dropped from `label`. Both suffixes are
/// peeled from the end in render order (status last, then the tool ref).
pub fn parse_process_line(line: &str) -> Option<ProcessRow> {
    let rest = line.trim().strip_prefix("- [")?;
    let close = rest.find(']')?;
    let kind = rest[..close].to_string();
    let after = rest[close + 1..].trim_start().strip_prefix('#')?;
    let space = after.find(' ')?;
    let id: u64 = after[..space].parse().ok()?;
    let body = after[space + 1..].trim();
    // The label is followed by zero or more ` · `-separated attribute segments
    // (see `render_processes_md`): a bare `status`, then keyed `type:`/`state:`/
    // `idle:` segments. Split them off the front chunk (the label) so the
    // middle-dot separator can't be mistaken for label text.
    let mut segments = body.split(" · ");
    let mut label = segments.next().unwrap_or("").trim().to_string();
    let mut status = None;
    let mut tool_type = None;
    let mut agent_state = None;
    let mut agent_id = None;
    let mut idle = None;
    for seg in segments {
        let seg = seg.trim();
        if let Some(v) = seg.strip_prefix("type:") {
            tool_type = Some(v.trim().to_string());
        } else if let Some(v) = seg.strip_prefix("state:") {
            agent_state = Some(v.trim().to_string());
        } else if let Some(v) = seg.strip_prefix("agent:") {
            agent_id = Some(v.trim().to_string());
        } else if let Some(v) = seg.strip_prefix("idle:") {
            // The "sitting idle for N" age (#143): kept so the Agents pane can
            // show how long a parked agent has been waiting. Only present while
            // the agent is `idle` (see `render_processes_md`).
            idle = Some(v.trim().to_string());
        } else if !seg.is_empty() {
            // The lone bare segment is the lifecycle status.
            status = Some(seg.to_string());
        }
    }
    let mut agent_tool_id = None;
    if let Some(from_at) = label.rfind(" (from #") {
        if label.ends_with(')') {
            let inner = &label[from_at + " (from #".len()..label.len() - 1];
            agent_tool_id = inner.trim().parse::<u64>().ok();
            label.truncate(from_at);
        }
    }
    Some(ProcessRow {
        kind,
        id,
        label,
        status,
        tool_type,
        agent_state,
        agent_id,
        agent_tool_id,
        idle,
    })
}

/// Recover the stable agent id from a pane's launch command - the value after
/// `--id` on a `panopt _mcp-proxy` or `panopt _agent` invocation (todo #142).
///
/// This is the linchpin of pane↔instance binding for the status observer. A
/// first-class agent's pane runs (after `_process-run` execs into it) the agent
/// binary, which spawns `panopt _mcp-proxy --id <agent_id> …` as its stdio MCP
/// server; Zellij's command detection frequently surfaces *that* child for the
/// pane rather than the `_process-run` shim or the agent binary. The one
/// constant across every surfacing is the `--id <agent_id>` flag, and that id
/// equals the instance row's `name` (see [`ProcessRow::agent_id`]), so matching
/// it binds the pane to its row no matter which command Zellij reports.
///
/// Restricted to `_mcp-proxy`/`_agent` commands so an unrelated tool that
/// happens to take a `--id` flag never masquerades as an agent pane. Returns
/// `None` when the command is not an agent invocation or carries no `--id`.
pub fn agent_id_from_command(command: Option<&str>) -> Option<String> {
    let cmd = command?;
    if !cmd.contains("_mcp-proxy") && !cmd.contains("_agent") {
        return None;
    }
    let mut tokens = cmd.split_whitespace();
    while let Some(t) = tokens.next() {
        if t == "--id" {
            return tokens.next().map(str::to_string);
        }
    }
    None
}

/// The activity a status pattern classifies an agent into (todo #142). Mirrors
/// `panopt_core::agent_profiles::AgentState`, re-declared here because the wasm
/// plugin cannot link `panopt-core` (rusqlite has no wasm target). The string
/// forms are the contract between the two halves: they must match the core
/// enum's `serde(rename_all = "lowercase")` names, since core projects them
/// into `agent-types.md` and the plugin reports them back via
/// `_process-report --agent-state`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum AgentState {
    Thinking,
    Idle,
    Waiting,
    Done,
}

impl AgentState {
    /// The wire/projection name. Must stay in lockstep with core's enum.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Thinking => "thinking",
            AgentState::Idle => "idle",
            AgentState::Waiting => "waiting",
            AgentState::Done => "done",
        }
    }

    /// Inverse of [`AgentState::as_str`]; `None` for an unknown token.
    pub fn parse(s: &str) -> Option<AgentState> {
        match s {
            "thinking" => Some(AgentState::Thinking),
            "idle" => Some(AgentState::Idle),
            "waiting" => Some(AgentState::Waiting),
            "done" => Some(AgentState::Done),
            _ => None,
        }
    }
}

/// The order non-idle states are tested in; first match wins, idle is the
/// fallthrough. Identical to the core interpreter's precedence (#138): waiting
/// (the agent needs a human) beats thinking (actively working) beats done (a
/// completion marker), so an ambiguous snapshot resolves deterministically.
const STATUS_PRECEDENCE: [AgentState; 3] =
    [AgentState::Waiting, AgentState::Thinking, AgentState::Done];

/// A compiled set of status patterns for one agent type: `(state, regexes)` in
/// [`STATUS_PRECEDENCE`] order. Built once when `agent-types.md` changes and
/// reused across polls. The wasm-side twin of
/// `panopt_core::agent_profiles::StatusMatcher`.
#[derive(Debug)]
pub struct StatusMatcher {
    rules: Vec<(AgentState, Vec<Regex>)>,
}

impl StatusMatcher {
    /// Classify an output snapshot: the highest-precedence state whose any
    /// pattern matches, or [`AgentState::Idle`] if none do.
    pub fn classify(&self, output: &str) -> AgentState {
        for (state, regexes) in &self.rules {
            if regexes.iter().any(|r| r.is_match(output)) {
                return *state;
            }
        }
        AgentState::Idle
    }
}

/// How many lines up from the bottom of a pane's viewport count as its "live
/// region" - the footer/spinner/prompt area an agent TUI repaints in place
/// (todo #142, bug #163). Classification reads only this tail, never the whole
/// viewport: an agent scrolls its entire transcript through the visible region,
/// so matching all of it makes the state sticky and flappy - a finished turn's
/// `esc to interrupt` spinner lingers in the scrollback, and any reply that
/// merely *mentions* a pattern word (an agent discussing its own status rules,
/// say) pins the state forever. Wide enough to catch a multi-line permission
/// prompt, tight enough to leave the bulk of transcript above it.
pub const LIVE_REGION_LINES: usize = 12;

/// The live-region slice of a pane viewport: the last [`LIVE_REGION_LINES`]
/// non-empty lines, joined with newlines, for [`StatusMatcher::classify`] to
/// scan (bug #163). Trailing blank lines (the TUI pads the screen below its
/// content) are dropped first so the window lands on real footer/prompt text
/// rather than empty padding. An all-blank or empty viewport yields `""`,
/// which classifies as [`AgentState::Idle`].
pub fn viewport_live_region(viewport: &[String]) -> String {
    let end = viewport
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map(|i| i + 1)
        .unwrap_or(0);
    let start = end.saturating_sub(LIVE_REGION_LINES);
    viewport[start..end].join("\n")
}

/// Parse the `.panopt/agent-types.md` projection (todo #142) into a compiled
/// [`StatusMatcher`] per `tool_type`. The file is a list of `## <tool_type>`
/// headers, each followed by `<state> = <regex>` lines (one pattern per line,
/// the same state repeated for several patterns). The patterns are
/// single-sourced from the daemon's loaded profile set; this only reads what
/// the daemon projected. Tolerant by design: an unparsable line is skipped and
/// an invalid regex drops just that one pattern, so a bad user override can
/// never crash the sidebar - it only loses classification fidelity.
pub fn parse_agent_type_matchers(body: &str) -> BTreeMap<String, StatusMatcher> {
    let mut raw: BTreeMap<String, BTreeMap<AgentState, Vec<Regex>>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in body.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("## ") {
            current = Some(name.trim().to_string());
            continue;
        }
        let Some(tool_type) = current.as_ref() else {
            continue;
        };
        let Some(eq) = line.find(" = ") else {
            continue;
        };
        let Some(state) = AgentState::parse(line[..eq].trim()) else {
            continue;
        };
        let Ok(re) = Regex::new(line[eq + " = ".len()..].trim()) else {
            continue;
        };
        raw.entry(tool_type.clone())
            .or_default()
            .entry(state)
            .or_default()
            .push(re);
    }
    raw.into_iter()
        .map(|(tool_type, by_state)| {
            let rules = STATUS_PRECEDENCE
                .into_iter()
                .filter_map(|s| by_state.get(&s).cloned().map(|res| (s, res)))
                .collect();
            (tool_type, StatusMatcher { rules })
        })
        .collect()
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
        assert_eq!(row.status, None);
        assert_eq!(row.agent_tool_id, Some(3));
    }

    #[test]
    fn parses_config_lines_and_ignores_non_config() {
        let row = parse_config_line("- #144 good [enabled]").unwrap();
        assert_eq!(row.id, 144);
        assert_eq!(row.label, "good");
        assert!(row.enabled);

        // A label with spaces, a disabled flag, and a trailing command.
        let row = parse_config_line("- #7 My Mediator [disabled] claude --foo").unwrap();
        assert_eq!(row.id, 7);
        assert_eq!(row.label, "My Mediator");
        assert!(!row.enabled);

        assert!(parse_config_line("# Agent tools").is_none());
        assert!(parse_config_line("_(no agent tools)_").is_none());
    }

    #[test]
    fn parses_a_process_line_with_status_and_from_suffix() {
        let row = parse_process_line("- [agent] #4 NASTL-Mediator (from #3) · starting").unwrap();
        assert_eq!(row.kind, "agent");
        assert_eq!(row.id, 4);
        assert_eq!(row.label, "NASTL-Mediator");
        assert_eq!(row.status.as_deref(), Some("starting"));
    }

    #[test]
    fn parses_a_process_line_with_status_and_no_from_suffix() {
        let row = parse_process_line("- [command] #2 Build · running").unwrap();
        assert_eq!(row.kind, "command");
        assert_eq!(row.id, 2);
        assert_eq!(row.label, "Build");
        assert_eq!(row.status.as_deref(), Some("running"));
    }

    #[test]
    fn ignores_non_process_lines() {
        assert!(parse_process_line("# Processes").is_none());
        assert!(parse_process_line("_(no processes)_").is_none());
    }

    #[test]
    fn parses_type_state_agent_and_idle_segments() {
        let row = parse_process_line(
            "- [agent] #5 Mediator (from #3) · running · type:claude-code · agent:mediator-1a · state:idle · idle:2m",
        )
        .unwrap();
        assert_eq!(row.label, "Mediator");
        assert_eq!(row.status.as_deref(), Some("running"));
        assert_eq!(row.tool_type.as_deref(), Some("claude-code"));
        assert_eq!(row.agent_id.as_deref(), Some("mediator-1a"));
        assert_eq!(row.agent_state.as_deref(), Some("idle"));
        assert_eq!(row.idle.as_deref(), Some("2m"));
    }

    fn config(label: &str) -> ConfigRow {
        ConfigRow {
            id: 7,
            label: label.to_string(),
            enabled: true,
        }
    }

    #[test]
    fn label_for_config_with_no_instance_is_id_and_name() {
        // The `#<id>` prefix mirrors the Todos/Notes panes' `#N <label>` shape
        // so every Agents row is addressable by the same id quoted elsewhere.
        assert_eq!(agent_config_label(&config("Mediator"), None), "#7 Mediator");
    }

    #[test]
    fn label_shows_classified_state_when_not_idle() {
        let inst = parse_process_line("- [agent] #5 Mediator (from #7) · running · state:thinking")
            .unwrap();
        assert_eq!(
            agent_config_label(&config("Mediator"), Some(&inst)),
            "#7 Mediator · thinking"
        );
    }

    #[test]
    fn label_appends_idle_age_while_idle() {
        let inst =
            parse_process_line("- [agent] #5 Mediator (from #7) · running · state:idle · idle:3m")
                .unwrap();
        assert_eq!(
            agent_config_label(&config("Mediator"), Some(&inst)),
            "#7 Mediator · idle 3m"
        );
    }

    #[test]
    fn label_falls_back_to_lifecycle_status_before_first_observation() {
        // A freshly-started instance has a lifecycle status but no classified
        // `state:` yet, so the row shows `starting` and carries no idle age.
        let inst = parse_process_line("- [agent] #5 Mediator (from #7) · starting").unwrap();
        assert_eq!(
            agent_config_label(&config("Mediator"), Some(&inst)),
            "#7 Mediator · starting"
        );
    }

    #[test]
    fn strip_trailing_id_removes_only_a_separated_or_whole_id() {
        // Daemon-appended forms: `name-<id>` and `name #<id>`.
        assert_eq!(strip_trailing_id("good-224", 224), "good");
        assert_eq!(
            strip_trailing_id("verify-submit #220", 220),
            "verify-submit"
        );
        assert_eq!(
            strip_trailing_id("weather-portland #193", 193),
            "weather-portland"
        );
        // The id IS the whole label -> collapses to empty.
        assert_eq!(strip_trailing_id("224", 224), "");
        // No trailing id, or digits that are part of the name -> left alone.
        assert_eq!(strip_trailing_id("good", 224), "good");
        assert_eq!(strip_trailing_id("abc224", 224), "abc224");
        // A different trailing number is not this row's id -> left alone.
        assert_eq!(strip_trailing_id("good-225", 224), "good-225");
    }

    #[test]
    fn process_pane_title_matches_viewer_pane_shape() {
        // `Agent #<id> - <name>`, parallel to `Todo #N - <title>`, with the
        // daemon-baked id stripped from the name so it appears exactly once.
        assert_eq!(
            process_pane_title_for("Agent", 224, "good-224"),
            "Agent #224 - good"
        );
        assert_eq!(
            process_pane_title_for("Agent", 220, "verify-submit #220"),
            "Agent #220 - verify-submit"
        );
        assert_eq!(
            process_pane_title_for("Command", 30, "deploy"),
            "Command #30 - deploy"
        );
        // Name that was only the id -> bare `<Kind> #<id>`, no dangling dash.
        assert_eq!(process_pane_title_for("Agent", 9, "9"), "Agent #9");
    }

    #[test]
    fn parse_input_line_reads_seq_process_and_unescaped_content() {
        let row = parse_input_line(
            r#"{"seq":3,"process_id":5,"content":"check the weather\nin Portland\n"}"#,
        )
        .unwrap();
        assert_eq!(row.seq, 3);
        assert_eq!(row.process_id, 5);
        // The embedded escape is unescaped to a real newline - what a hand-rolled
        // splitter would get wrong.
        assert_eq!(row.content, "check the weather\nin Portland\n");
    }

    #[test]
    fn parse_input_line_skips_blank_and_malformed() {
        assert!(parse_input_line("").is_none());
        assert!(parse_input_line("   ").is_none());
        assert!(parse_input_line("not json").is_none());
        assert!(parse_input_line(r#"{"seq":1}"#).is_none());
    }

    #[test]
    fn agent_id_parses_from_mcp_proxy_and_agent_commands_only() {
        // The `_mcp-proxy` child Zellij usually surfaces for an agent pane.
        assert_eq!(
            agent_id_from_command(Some(
                "/bin/panopt --port 7600 _mcp-proxy --host 127.0.0.1 --id good --name good"
            )),
            Some("good".to_string())
        );
        // The pre-exec `_agent` shim.
        assert_eq!(
            agent_id_from_command(Some("/bin/panopt _agent --id mediator-1a2b")),
            Some("mediator-1a2b".to_string())
        );
        // A non-agent command carrying `--id` must not masquerade as an agent.
        assert_eq!(agent_id_from_command(Some("some-tool --id 42")), None);
        // Agent command with no `--id` (anonymous) yields nothing to bind on.
        assert_eq!(agent_id_from_command(Some("/bin/panopt _agent")), None);
        assert_eq!(agent_id_from_command(None), None);
    }

    #[test]
    fn agent_state_roundtrips_through_its_wire_name() {
        for s in [
            AgentState::Thinking,
            AgentState::Idle,
            AgentState::Waiting,
            AgentState::Done,
        ] {
            assert_eq!(AgentState::parse(s.as_str()), Some(s));
        }
        assert_eq!(AgentState::parse("bogus"), None);
    }

    #[test]
    fn matcher_classifies_with_waiting_over_thinking_precedence() {
        let body =
            "# Agent types\n\n## claude-code\nthinking = esc to interrupt\nwaiting = Do you want\n";
        let matchers = parse_agent_type_matchers(body);
        let m = matchers.get("claude-code").expect("claude-code matcher");
        assert_eq!(m.classify("... esc to interrupt ..."), AgentState::Thinking);
        assert_eq!(m.classify("quiet output"), AgentState::Idle);
        // Both patterns present: waiting wins by precedence.
        assert_eq!(
            m.classify("esc to interrupt\nDo you want to proceed?"),
            AgentState::Waiting
        );
    }

    #[test]
    fn live_region_windows_to_the_footer_and_ignores_transcript() {
        let body =
            "# Agent types\n\n## claude-code\nthinking = esc to interrupt\nwaiting = Do you want\n";
        let m = parse_agent_type_matchers(body)
            .remove("claude-code")
            .expect("claude-code matcher");

        // A finished turn that mentioned the spinner far up in the transcript,
        // now sitting idle at the prompt. The whole viewport would (wrongly)
        // classify as thinking; the live region sees only the idle footer.
        let mut viewport: Vec<String> = vec!["assistant: running esc to interrupt".into()];
        for _ in 0..40 {
            viewport.push("transcript line".into());
        }
        viewport.push("> ".into());
        viewport.push(String::new());
        viewport.push(String::new());
        assert!(m.classify(&viewport.join("\n")) == AgentState::Thinking);
        assert_eq!(
            m.classify(&viewport_live_region(&viewport)),
            AgentState::Idle
        );

        // The live spinner on the last non-blank line is inside the window.
        let working = vec![
            "working...".into(),
            "esc to interrupt".into(),
            String::new(),
        ];
        assert_eq!(
            m.classify(&viewport_live_region(&working)),
            AgentState::Thinking
        );

        // An all-blank or empty viewport is idle, never a panic.
        assert_eq!(viewport_live_region(&[]), "");
        assert_eq!(viewport_live_region(&["".into(), "  ".into()]), "");
    }

    #[test]
    fn matcher_parse_skips_bad_lines_and_invalid_regex() {
        let body = "## t\nthinking = (unclosed\nbogus = x\nwaiting = ok\n";
        let m = parse_agent_type_matchers(body);
        let t = m.get("t").expect("type t");
        // The invalid-regex `thinking` and unknown-state `bogus` lines drop;
        // only the valid `waiting` pattern survives.
        assert_eq!(t.classify("ok"), AgentState::Waiting);
        assert_eq!(t.classify("nope"), AgentState::Idle);
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
        // A first-class agent launched directly with its rendered per-instance
        // config ties back to its process id via the `instances/<id>/` segment,
        // even though it never went through the `_process-run` shim (#142).
        assert_eq!(
            classify_pane(Some(
                "claude --mcp-config /home/u/.local/share/panopt/instances/158/mcp_config"
            )),
            PaneRole::Process(158)
        );
        // A bare `instances` path with no numeric id must not misclassify.
        assert_eq!(
            classify_pane(Some("vim /home/u/code/instances/notes.md")),
            PaneRole::Shell
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
