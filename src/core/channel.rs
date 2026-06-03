//! L13 structured side-channel: a private-OSC JSON-RPC transport hosting the
//! adjacent protocols (MCP, plus LSP/ACP negotiation).
//!
//! **Wire format:** `OSC <CODE> ; <protocol> ; <json-rpc-message> ST` — one
//! JSON-RPC 2.0 message per OSC. It is full-duplex: the child emits these on its
//! stdout (we parse them out of the byte stream) and we reply by queuing OSCs on
//! the response channel, which the driver writes to the PTY master — i.e. the
//! child's stdin. serde_json escapes control bytes, so a JSON payload never
//! contains the `ST` terminator.
//!
//! **Graceful degradation:** a terminal without channel support just ignores the
//! private OSC, so an unaware child gets no reply and falls back to plain text —
//! the dual-channel contract from the spec-tree synthesis.
//!
//! **Routing:** the `<protocol>` tag sub-addresses a handler, each with its own
//! method/id space:
//! - `channel` — capability negotiation (`initialize` advertises what we speak),
//! - `mcp` — a Model Context Protocol server exposing the terminal to agents
//!   (the complete exemplar: `tools/list` + `tools/call`),
//! - `lsp` / `acp` — negotiable endpoints whose `initialize` handshakes are
//!   implemented; deeper methods return `method not found` until a backend is
//!   registered (a terminal has no language/agent backend of its own).
//!
//! The JSON-RPC 2.0 message model and the LSP types are reused from `rusty_lsp`.

use super::cell::{Cell, WIDE_TRAILER};
use super::grid::Grid;
use rusty_lsp::error::{ResponseError, codes};
use rusty_lsp::jsonrpc::{Message, Request, Response};
use serde_json::{Value, json};

/// Private OSC code for the structured channel. Distinctive and unassigned by
/// the common terminals.
pub(crate) const OSC_CODE: &str = "5379";
/// The `OSC <CODE> ;` byte prefix the parser matches to route a payload here.
pub(crate) const OSC_PREFIX: &[u8] = b"5379;";

const CHANNEL_VERSION: u32 = 1;
const TERMINAL_NAME: &str = "rusty_term";
const TERMINAL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Handle one channel OSC payload — the bytes after the `OSC <CODE> ;` prefix,
/// i.e. `<protocol> ; <json>`. Routes the JSON-RPC message and queues any reply
/// onto `responses` (child-bound).
pub(crate) fn handle(payload: &[u8], g: &Grid, responses: &mut Vec<u8>) {
    let Ok(text) = std::str::from_utf8(payload) else {
        return; // non-UTF-8 payload: ignore
    };
    let Some((protocol, body)) = text.split_once(';') else {
        return; // missing protocol tag
    };
    let Ok(message) = serde_json::from_str::<Message>(body) else {
        return; // malformed JSON-RPC — no id to address a reply to, so drop it
    };
    match message {
        Message::Request(req) => {
            let response = match dispatch(protocol, &req, g) {
                Ok(result) => Response::success(req.id, result),
                Err(error) => Response::error(Some(req.id), error),
            };
            send(protocol, &Message::Response(response), responses);
        }
        // Notifications (e.g. MCP `notifications/initialized`) need no reply, and
        // we issue no requests, so any response from the peer is ignored.
        Message::Notification(_) | Message::Response(_) => {}
    }
}

/// Route a request to its protocol handler.
fn dispatch(protocol: &str, req: &Request, g: &Grid) -> Result<Value, ResponseError> {
    match protocol {
        "channel" => channel_request(req),
        "mcp" => mcp_request(req, g),
        "lsp" => lsp_request(req),
        "acp" => acp_request(req),
        other => Err(error(
            codes::METHOD_NOT_FOUND,
            format!("unknown channel protocol: {other}"),
        )),
    }
}

/// The `channel` meta-protocol: capability negotiation.
fn channel_request(req: &Request) -> Result<Value, ResponseError> {
    match req.method.as_str() {
        "initialize" => Ok(json!({
            "version": CHANNEL_VERSION,
            "protocols": ["channel", "mcp", "lsp", "acp"],
            "terminalInfo": { "name": TERMINAL_NAME, "version": TERMINAL_VERSION },
        })),
        other => Err(error(
            codes::METHOD_NOT_FOUND,
            format!("channel: unknown method {other}"),
        )),
    }
}

/// The MCP server: exposes the terminal's state to agents as tools.
fn mcp_request(req: &Request, g: &Grid) -> Result<Value, ResponseError> {
    match req.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": TERMINAL_NAME, "version": TERMINAL_VERSION },
        })),
        "tools/list" => Ok(json!({ "tools": mcp_tools() })),
        "tools/call" => mcp_call(req.params.as_ref(), g),
        other => Err(error(
            codes::METHOD_NOT_FOUND,
            format!("mcp: unknown method {other}"),
        )),
    }
}

/// The MCP tool catalogue: terminal-introspection tools an agent can call.
fn mcp_tools() -> Value {
    let no_args = || json!({ "type": "object", "properties": {}, "additionalProperties": false });
    json!([
        { "name": "get_screen",
          "description": "The terminal's current visible screen, as text.",
          "inputSchema": no_args() },
        { "name": "get_scrollback",
          "description": "Lines that scrolled off the top of the screen, oldest first.",
          "inputSchema": {
              "type": "object",
              "properties": { "lines": { "type": "integer", "description": "Max lines to return (default 100)." } },
              "additionalProperties": false } },
        { "name": "get_cwd",
          "description": "The child's working directory as reported via OSC 7, if any.",
          "inputSchema": no_args() },
        { "name": "get_title",
          "description": "The window title set by the child via OSC 0/2.",
          "inputSchema": no_args() },
        { "name": "get_dimensions",
          "description": "The terminal size in character cells, as \"COLSxROWS\".",
          "inputSchema": no_args() },
    ])
}

/// Execute an MCP `tools/call`, returning the standard `{ content: [...] }`.
fn mcp_call(params: Option<&Value>, g: &Grid) -> Result<Value, ResponseError> {
    let params = params.ok_or_else(|| error(codes::INVALID_PARAMS, "missing params"))?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| error(codes::INVALID_PARAMS, "missing tool name"))?;
    let args = params.get("arguments");

    let text = match name {
        "get_screen" => screen_text(g),
        "get_scrollback" => {
            let lines = args
                .and_then(|a| a.get("lines"))
                .and_then(Value::as_u64)
                .unwrap_or(100) as usize;
            scrollback_text(g, lines)
        }
        "get_cwd" => g.cwd.clone(),
        "get_title" => g.title.clone(),
        "get_dimensions" => format!("{}x{}", g.cols, g.rows),
        other => return Err(error(codes::INVALID_PARAMS, format!("unknown tool: {other}"))),
    };
    Ok(json!({ "content": [ { "type": "text", "text": text } ] }))
}

/// LSP endpoint — negotiates via `rusty_lsp`'s LSP types. A terminal has no
/// language backend, so it advertises empty capabilities; a future bridge would
/// register real ones here and could host them on `rusty_lsp`'s `Server`.
fn lsp_request(req: &Request) -> Result<Value, ResponseError> {
    match req.method.as_str() {
        "initialize" => {
            let result = rusty_lsp::lsp::InitializeResult {
                capabilities: rusty_lsp::lsp::ServerCapabilities::default(),
                server_info: Some(rusty_lsp::lsp::ServerInfo {
                    name: TERMINAL_NAME.into(),
                    version: Some(TERMINAL_VERSION.into()),
                }),
            };
            serde_json::to_value(result).map_err(|e| error(codes::INTERNAL_ERROR, e.to_string()))
        }
        other => Err(error(
            codes::METHOD_NOT_FOUND,
            format!("lsp: {other} (no language backend registered)"),
        )),
    }
}

/// ACP endpoint — implements the Agent Client Protocol `initialize` handshake
/// (schema per ACP v1). A terminal is not an LLM agent, so it advertises a
/// baseline agent with no extra prompt capabilities; session methods report
/// `method not found` until an agent backend is registered.
fn acp_request(req: &Request) -> Result<Value, ResponseError> {
    match req.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": false,
                "promptCapabilities": { "image": false, "audio": false, "embeddedContext": false },
            },
            "agentInfo": { "name": TERMINAL_NAME, "version": TERMINAL_VERSION },
            "authMethods": [],
        })),
        other => Err(error(
            codes::METHOD_NOT_FOUND,
            format!("acp: {other} (no agent backend registered)"),
        )),
    }
}

/// Frame `msg` as `OSC <CODE> ; <protocol> ; <json> ST` and queue it for the child.
fn send(protocol: &str, msg: &Message, responses: &mut Vec<u8>) {
    let Ok(json) = serde_json::to_string(msg) else {
        return;
    };
    responses.extend_from_slice(b"\x1b]");
    responses.extend_from_slice(OSC_CODE.as_bytes());
    responses.push(b';');
    responses.extend_from_slice(protocol.as_bytes());
    responses.push(b';');
    responses.extend_from_slice(json.as_bytes());
    responses.extend_from_slice(b"\x1b\\");
}

/// The visible screen as text: one line per row, trailing blank rows dropped.
fn screen_text(g: &Grid) -> String {
    let mut lines: Vec<String> = g
        .cells
        .chunks(g.cols)
        .map(|row| row_text(row, &g.clusters))
        .collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

/// The most recent `max` scrollback lines as text (oldest first).
fn scrollback_text(g: &Grid, max: usize) -> String {
    let skip = g.scrollback.len().saturating_sub(max);
    g.scrollback
        .iter()
        .skip(skip)
        .map(|row| row_text(row, &g.clusters))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Reconstruct a row of cells as text: base glyph plus any interned grapheme
/// continuation, skipping wide-glyph trailers, with trailing blanks trimmed.
fn row_text(cells: &[Cell], clusters: &[String]) -> String {
    let mut s = String::new();
    for cell in cells {
        if cell.flags & WIDE_TRAILER != 0 {
            continue;
        }
        s.push(cell.ch);
        if cell.cluster != 0
            && let Some(suffix) = clusters.get((cell.cluster - 1) as usize)
        {
            s.push_str(suffix);
        }
    }
    s.trim_end().to_string()
}

fn error(code: i64, message: impl Into<String>) -> ResponseError {
    ResponseError { code, message: message.into(), data: None }
}
