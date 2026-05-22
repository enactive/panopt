//! A minimal synchronous MCP client over Streamable HTTP.
//!
//! Just enough of the protocol for `panopt todo` to call the daemon's tools:
//! one `initialize` handshake, the `initialized` notification, then any number
//! of `tools/call`s, over plain localhost HTTP. There is no async runtime -
//! `ureq` blocks, which is exactly right for a short-lived CLI.
//!
//! Streamable HTTP lets the daemon answer a POST with either a bare JSON object
//! or a `text/event-stream` body; [`extract_json`] copes with both.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

/// An open, initialized MCP session against the panopt daemon.
pub struct Client {
    /// The daemon's `/mcp` endpoint, query string and all.
    url: String,
    /// The session id the daemon assigned at `initialize`.
    session: String,
}

impl Client {
    /// Open a session against `url` (the `/mcp` endpoint with its
    /// `?ws=...&observer=1` query already appended) and run the MCP lifecycle
    /// handshake, leaving the session ready for [`Client::call`].
    pub fn connect(url: &str) -> Result<Client> {
        let init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "panopt-cli", "version": env!("CARGO_PKG_VERSION") }
            }
        });
        let resp = match ureq::post(url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .send_string(&init.to_string())
        {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => bail!(
                "daemon rejected initialize (HTTP {code}): {}",
                r.into_string().unwrap_or_default()
            ),
            Err(e) => return Err(e).context("connecting to the panopt daemon"),
        };

        let session = resp
            .header("mcp-session-id")
            .ok_or_else(|| anyhow!("daemon returned no MCP session id"))?
            .to_string();
        let body = resp.into_string().context("reading the initialize response")?;
        parse_rpc(&body).context("initialize failed")?;

        let client = Client { url: url.to_string(), session };
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        client
            .post(&note.to_string())
            .context("sending the initialized notification")?;
        Ok(client)
    }

    /// Call tool `name` with `args`. The panopt tools return either a bare
    /// string ("ok", a numeric id) or a JSON document; this hands back the
    /// parsed JSON when it parses and a JSON string otherwise, so every caller
    /// gets one [`Value`].
    pub fn call(&self, name: &str, args: Value) -> Result<Value> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": name, "arguments": args }
        });
        let body = self
            .post(&req.to_string())
            .with_context(|| format!("calling tool `{name}`"))?;
        let result = parse_rpc(&body).with_context(|| format!("tool `{name}` failed"))?;
        tool_text(&result)
    }

    /// Best-effort: ask the daemon to drop this session.
    pub fn close(self) {
        let _ = ureq::delete(&self.url).set("mcp-session-id", &self.session).call();
    }

    /// POST `body` to the endpoint with the session header; return the response
    /// body (empty for an accepted notification).
    fn post(&self, body: &str) -> Result<String> {
        match ureq::post(&self.url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set("mcp-session-id", &self.session)
            .send_string(body)
        {
            Ok(r) => Ok(r.into_string().unwrap_or_default()),
            Err(ureq::Error::Status(code, r)) => bail!(
                "daemon returned HTTP {code}: {}",
                r.into_string().unwrap_or_default()
            ),
            Err(e) => Err(e).context("posting to the panopt daemon"),
        }
    }
}

/// Extract the JSON-RPC response from a Streamable HTTP body and return its
/// `result`, mapping a JSON-RPC `error` onto an `Err`.
fn parse_rpc(body: &str) -> Result<Value> {
    let json = extract_json(body)?;
    if let Some(err) = json.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        bail!("{msg}");
    }
    json.get("result")
        .cloned()
        .ok_or_else(|| anyhow!("response carried neither result nor error"))
}

/// Pull the JSON-RPC object out of a response body: a body that is already
/// JSON is parsed directly; a `text/event-stream` body has its `data:` lines
/// concatenated and parsed.
fn extract_json(body: &str) -> Result<Value> {
    let trimmed = body.trim_start();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed).context("parsing the JSON response");
    }
    let mut data = String::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data.is_empty() {
        bail!("empty response from the daemon");
    }
    serde_json::from_str(&data).context("parsing the SSE data payload")
}

/// Extract a tool result's first text content, failing when the tool reported
/// an error.
fn tool_text(result: &Value) -> Result<Value> {
    let text = first_text(result);
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        bail!("{}", text.unwrap_or_else(|| "tool reported an error".into()));
    }
    let text = text.ok_or_else(|| anyhow!("tool returned no text content"))?;
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

/// The first `text` field across a tool result's `content` array.
fn first_text(result: &Value) -> Option<String> {
    result
        .get("content")?
        .as_array()?
        .iter()
        .find_map(|c| c.get("text").and_then(Value::as_str))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_reads_a_plain_body() {
        let v = extract_json(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#).unwrap();
        assert_eq!(v["result"]["ok"], json!(true));
    }

    #[test]
    fn extract_json_reads_an_sse_body() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":42}\n\n";
        assert_eq!(extract_json(body).unwrap()["result"], json!(42));
    }

    #[test]
    fn parse_rpc_surfaces_a_jsonrpc_error() {
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"bad id"}}"#;
        let err = parse_rpc(body).unwrap_err();
        assert!(err.to_string().contains("bad id"), "{err}");
    }

    #[test]
    fn tool_text_parses_json_content_and_falls_back_to_a_string() {
        let json_result = json!({ "content": [ { "type": "text", "text": "[1,2,3]" } ] });
        assert_eq!(tool_text(&json_result).unwrap(), json!([1, 2, 3]));

        let bare = json!({ "content": [ { "type": "text", "text": "ok" } ] });
        assert_eq!(tool_text(&bare).unwrap(), json!("ok"));
    }

    #[test]
    fn tool_text_fails_on_an_error_result() {
        let result = json!({ "isError": true, "content": [ { "text": "todo 9 not found" } ] });
        let err = tool_text(&result).unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }
}
