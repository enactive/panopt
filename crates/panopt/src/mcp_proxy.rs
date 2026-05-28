//! `panopt _mcp-proxy` - a long-lived stdio MCP server that fronts panoptd.
//!
//! Claude Code spawns this at session start via `--mcp-config` type=stdio.
//! The proxy speaks MCP-over-stdio to Claude Code and MCP-over-HTTP to
//! panoptd, owning the panoptd session-id and reconnecting transparently
//! across daemon restarts. From Claude Code's point of view, panoptd never
//! goes away: a `just refresh` only stalls the next tool call for the time
//! it takes the new daemon to come up.
//!
//! Two transports, two MCP sessions:
//!
//! - The stdio side is a tiny MCP server. The proxy answers `initialize` and
//!   `notifications/initialized` itself (so Claude Code sees a stable
//!   server), and forwards everything else - `tools/list`, `tools/call`,
//!   future methods - to panoptd verbatim. The JSON-RPC `id` is preserved
//!   so the response correlates on Claude Code's side without rewrite.
//! - The HTTP side is the panoptd Streamable HTTP endpoint, scoped to this
//!   agent by `?ws=&agent=&name=&token=` baked into the URL. The proxy
//!   maintains its own `mcp-session-id` here. When a forwarded request
//!   fails (the daemon was killed, returned 4xx for an unknown session,
//!   refused the TCP connection), the proxy drops the session-id, re-runs
//!   `initialize` + `notifications/initialized`, and replays the original
//!   request - all invisible to Claude Code.
//!
//! Outage policy: hybrid. Each forwarded call retries against panoptd with
//! exponential backoff for up to [`RECONNECT_BUDGET`]. Short outages (a
//! `just refresh`, a brief daemon hiccup) look like a slow tool call to
//! Claude Code, no error. Longer outages fail with a JSON-RPC error so the
//! Claude Code conversation surface gets a visible failure rather than
//! hanging indefinitely.
//!
//! `tools/list` is answered locally from [`panopt_tool_surface::TOOL_SURFACE`],
//! the same table panoptd registers its routes from. That makes a cold start
//! with a dead daemon survivable: Claude Code learns the tool surface
//! immediately and the first `tools/call` is the first thing that has to wait
//! on a real panoptd connection. Without this local answer, an unreachable
//! daemon at startup would force Claude's `tools/list` request to error out
//! (the MCP spec doesn't mandate a retry) and the panopt MCP server would
//! appear toolless for the rest of the session.

use std::io::{stdin, stdout, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Value};

/// The MCP protocol version the proxy advertises to Claude Code, and the
/// version it uses in its own `initialize` against panoptd. Kept aligned
/// with `mcpclient.rs` so the two sides speak the same protocol revision.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// How long a single forwarded request keeps retrying against panoptd
/// before giving up and returning a JSON-RPC error to Claude Code. Long
/// enough to cover a full `cargo build && cargo run` rebuild cycle, short
/// enough that a genuinely-dead daemon does not hang the conversation.
const RECONNECT_BUDGET: Duration = Duration::from_secs(30);

/// Initial delay between reconnect attempts. Doubles up to
/// [`RECONNECT_BACKOFF_MAX`] so we hammer the daemon during the brief
/// shutdown-to-startup window without busy-spinning if it stays down.
const RECONNECT_BACKOFF_INITIAL: Duration = Duration::from_millis(200);

/// Upper bound on the reconnect interval.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(2);

/// `panopt _mcp-proxy` entry point. Blocks until stdin closes (Claude Code
/// has gone away).
pub fn run(
    host: String,
    port: u16,
    ws: PathBuf,
    id: String,
    name: String,
    token: String,
) -> Result<()> {
    let url = build_url(&host, port, &ws, &id, &name, &token);
    let mut backend = Backend::new(url, id, name);

    // Eagerly bring up the panoptd session so the agent registers in the
    // roster before its first tool call. Failure here is non-fatal: the
    // next forwarded request will retry the same connection sequence, so a
    // daemon that boots a little later still picks the agent up.
    if let Err(e) = backend.connect() {
        eprintln!("panopt-proxy: initial panoptd connect failed (will retry on first call): {e:#}");
    }

    let stdin = stdin();
    let mut stdout = stdout().lock();
    let mut reader = BufReader::new(stdin.lock());
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).context("reading stdin")?;
        if n == 0 {
            break; // Claude Code closed stdin - session over.
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("panopt-proxy: malformed JSON-RPC frame ignored: {e}");
                continue;
            }
        };
        if let Some(resp) = dispatch(&mut backend, &req) {
            let serialized = serde_json::to_string(&resp).expect("response must serialize as JSON");
            writeln!(stdout, "{serialized}").context("writing to stdout")?;
            stdout.flush().context("flushing stdout")?;
        }
    }

    backend.farewell();
    Ok(())
}

fn build_url(host: &str, port: u16, ws: &Path, id: &str, name: &str, token: &str) -> String {
    let encode = |s: &str| utf8_percent_encode(s, NON_ALPHANUMERIC).to_string();
    format!(
        "http://{host}:{port}/mcp?ws={}&agent={}&name={}&token={}",
        encode(&ws.to_string_lossy()),
        encode(id),
        encode(name),
        encode(token),
    )
}

/// Route a JSON-RPC request: answer the lifecycle methods and `tools/list`
/// locally, forward everything else to panoptd. Returns `Some(response)` for
/// requests (those carrying an `id`) and `None` for notifications.
fn dispatch(backend: &mut Backend, req: &Value) -> Option<Value> {
    let method = req.get("method").and_then(Value::as_str)?;
    let id = req.get("id").cloned();
    match method {
        "initialize" => Some(handle_initialize(backend, id)),
        "notifications/initialized" => None,
        "tools/list" => Some(handle_tools_list(id)),
        _ => Some(forward_or_error(backend, req, id)),
    }
}

/// Respond to `tools/list` from the shared [`TOOL_SURFACE`] table.
///
/// Same data panoptd uses to register its routes, so the proxy's published
/// surface and the daemon's served surface cannot drift apart at build time.
/// The schema for each tool is materialized by calling that entry's
/// `schema_fn` - identical bytes to what panoptd emits, because both sides
/// route through `panopt_tool_surface::schema_for::<T>`.
fn handle_tools_list(id: Option<Value>) -> Value {
    let tools: Vec<Value> = panopt_tool_surface::TOOL_SURFACE
        .iter()
        .map(|def| {
            json!({
                "name": def.name,
                "description": def.description,
                "inputSchema": (def.schema_fn)(),
            })
        })
        .collect();
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "tools": tools },
    })
}

/// Respond to Claude Code's `initialize` without forwarding. The proxy is
/// its own MCP server from Claude Code's perspective; the panoptd session
/// is independent and was (or will be) initialized via [`Backend::connect`].
fn handle_initialize(_backend: &mut Backend, id: Option<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "panopt-proxy",
                "version": env!("CARGO_PKG_VERSION"),
            },
        },
    })
}

/// Forward `req` to panoptd. On unrecoverable failure (the
/// [`RECONNECT_BUDGET`] elapsed), return a JSON-RPC error response so
/// Claude Code surfaces the outage instead of hanging.
fn forward_or_error(backend: &mut Backend, req: &Value, id: Option<Value>) -> Value {
    let body = req.to_string();
    match backend.forward(&body) {
        Ok(v) => v,
        Err(e) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32603,
                "message": format!("panoptd unreachable: {e:#}"),
            },
        }),
    }
}

/// Owns the panoptd MCP session. Reconnects transparently when the
/// daemon's session table forgets us (a restart, an idle reap).
struct Backend {
    url: String,
    /// Stable agent id used to call `agent_leave` at shutdown. Matches the
    /// `?agent=` parameter baked into [`Self::url`]; carried as a field so
    /// the proxy doesn't have to re-parse its own URL.
    agent_id: String,
    /// Friendly display name, baked into [`Self::url`] for the daemon's
    /// implicit identify on first sight. Held for symmetry with `agent_id`;
    /// not used after the URL is built.
    #[allow(dead_code)]
    name: String,
    /// Panoptd's `mcp-session-id` header value. `None` before the first
    /// successful `initialize`, or after a forwarded call discovered the
    /// session was forgotten.
    session_id: Option<String>,
}

impl Backend {
    fn new(url: String, agent_id: String, name: String) -> Self {
        Backend {
            url,
            agent_id,
            name,
            session_id: None,
        }
    }

    /// Initialize a fresh panoptd session: `initialize` request, capture
    /// the assigned `mcp-session-id`, send the `notifications/initialized`
    /// follow-up. Idempotent: a successful call replaces any prior session.
    fn connect(&mut self) -> Result<()> {
        let init_body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "panopt-proxy",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            },
        });
        let resp = ureq::post(&self.url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .send_string(&init_body.to_string())
            .map_err(|e| anyhow!("initialize against panoptd: {e}"))?;
        let sid = resp
            .header("mcp-session-id")
            .map(str::to_string)
            .ok_or_else(|| anyhow!("panoptd returned no mcp-session-id on initialize"))?;
        let _ = resp.into_string(); // discard the JSON-RPC body; we only needed the header

        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        ureq::post(&self.url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set("mcp-session-id", &sid)
            .send_string(&note.to_string())
            .map_err(|e| anyhow!("initialized notification to panoptd: {e}"))?;

        self.session_id = Some(sid);
        Ok(())
    }

    /// POST `body` (a complete JSON-RPC frame) to panoptd with the current
    /// session id and return the parsed response. Re-establishes the
    /// session on failure and retries up to [`RECONNECT_BUDGET`].
    fn forward(&mut self, body: &str) -> Result<Value> {
        let deadline = Instant::now() + RECONNECT_BUDGET;
        let mut backoff = RECONNECT_BACKOFF_INITIAL;
        let mut last_err: Option<anyhow::Error> = None;

        loop {
            if Instant::now() >= deadline {
                return Err(last_err.unwrap_or_else(|| anyhow!("reconnect budget exhausted")));
            }
            if self.session_id.is_none() {
                if let Err(e) = self.connect() {
                    last_err = Some(e);
                    thread::sleep(backoff);
                    backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
                    continue;
                }
            }
            match self.try_forward(body) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    last_err = Some(e);
                    // The session id is either invalid (daemon restarted)
                    // or the daemon is unreachable - drop it and let the
                    // next iteration re-initialize. Sleeping keeps us from
                    // hammering a daemon that is mid-restart.
                    self.session_id = None;
                    thread::sleep(backoff);
                    backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
                }
            }
        }
    }

    fn try_forward(&self, body: &str) -> Result<Value> {
        let sid = self
            .session_id
            .as_ref()
            .ok_or_else(|| anyhow!("no panoptd session"))?;
        let resp = ureq::post(&self.url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set("mcp-session-id", sid)
            .send_string(body)
            .map_err(|e| anyhow!("forwarding to panoptd: {e}"))?;
        let body = resp
            .into_string()
            .context("reading panoptd response body")?;
        parse_response(&body)
    }

    /// On shutdown, best-effort: tell the daemon we are leaving so the
    /// registry entry and any held locks clear immediately, then close the
    /// HTTP session. Errors are swallowed - the agent is exiting anyway,
    /// and the next sweep will eventually catch the entry if this call
    /// failed to land.
    fn farewell(&mut self) {
        if self.session_id.is_none() {
            return;
        }
        let req = json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/call",
            "params": { "name": "agent_leave", "arguments": {} },
        });
        let _ = self.try_forward(&req.to_string());
        if let Some(sid) = self.session_id.take() {
            let _ = ureq::delete(&self.url).set("mcp-session-id", &sid).call();
        }
        let _ = &self.agent_id; // silence unused-field warning if logging is added later
    }
}

/// Parse a Streamable HTTP response body: either a bare JSON object or a
/// `text/event-stream` body whose `data:` lines concatenate into the
/// JSON-RPC frame. Mirrors the same logic in `mcpclient.rs` so behavior is
/// identical across the CLI's one-shot calls and the proxy's forwards.
fn parse_response(body: &str) -> Result<Value> {
    let trimmed = body.trim_start();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed).context("parsing JSON response");
    }
    let mut data = String::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data.is_empty() {
        bail!("empty response from panoptd");
    }
    serde_json::from_str(&data).context("parsing SSE data payload")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_response_reads_a_plain_body() {
        let v = parse_response(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#).unwrap();
        assert_eq!(v["result"]["ok"], json!(true));
    }

    #[test]
    fn parse_response_reads_an_sse_body() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":42}\n\n";
        assert_eq!(parse_response(body).unwrap()["result"], json!(42));
    }

    #[test]
    fn build_url_percent_encodes_every_field() {
        let url = build_url(
            "127.0.0.1",
            7600,
            Path::new("/tmp/with spaces"),
            "user-host",
            "Display Name",
            "tok/en+with=stuff",
        );
        // Slashes, spaces, and special URL characters must be escaped so a
        // malformed query string cannot break the request.
        assert!(url.contains("ws=%2Ftmp%2Fwith%20spaces"), "{url}");
        assert!(url.contains("name=Display%20Name"), "{url}");
        assert!(url.contains("token=tok%2Fen%2Bwith%3Dstuff"), "{url}");
    }

    #[test]
    fn dispatch_answers_initialize_locally() {
        let mut backend = Backend::new(
            "http://127.0.0.1:0/mcp".into(),
            "alpha".into(),
            "alpha".into(),
        );
        let req = json!({"jsonrpc": "2.0", "id": 7, "method": "initialize"});
        let resp = dispatch(&mut backend, &req).unwrap();
        assert_eq!(resp["id"], json!(7));
        assert_eq!(resp["result"]["serverInfo"]["name"], "panopt-proxy");
        // We did not connect to panoptd, so the session must still be empty.
        assert!(backend.session_id.is_none());
    }

    #[test]
    fn dispatch_ignores_initialized_notification() {
        let mut backend = Backend::new(
            "http://127.0.0.1:0/mcp".into(),
            "alpha".into(),
            "alpha".into(),
        );
        let req = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(dispatch(&mut backend, &req).is_none());
    }

    #[test]
    fn dispatch_answers_tools_list_locally() {
        // No panoptd in this test; if the proxy were forwarding tools/list it
        // would fail. A success here means the local TOOL_SURFACE path served
        // the request - the whole reason step 6 of todo #88 exists.
        let mut backend = Backend::new(
            "http://127.0.0.1:0/mcp".into(),
            "alpha".into(),
            "alpha".into(),
        );
        let req = json!({"jsonrpc": "2.0", "id": 11, "method": "tools/list"});
        let resp = dispatch(&mut backend, &req).unwrap();
        assert_eq!(resp["id"], json!(11));

        let tools = resp["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), panopt_tool_surface::TOOL_SURFACE.len());

        // Spot-check shape on the first entry: every published tool must have
        // a non-empty name, a description, and an inputSchema that's an
        // object - the three fields the MCP spec requires on a Tool.
        for t in tools {
            assert!(t["name"].is_string());
            assert!(t["description"].is_string());
            assert!(t["inputSchema"].is_object(), "{t}");
        }

        // We never touched panoptd, so the proxy's backend session must be
        // unchanged. This is the load-bearing property: a cold start with a
        // dead daemon should still answer tools/list.
        assert!(backend.session_id.is_none());
    }
}
