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
//! - `channel` — version negotiation + schema discovery (`initialize` agrees a
//!   version and advertises per-protocol capabilities; `describe` returns the
//!   full machine-readable schema),
//! - `mcp` — a Model Context Protocol server exposing the terminal to agents as
//!   both tools (`tools/list` + `tools/call`) and resources (`resources/list` +
//!   `resources/read`),
//! - `lsp` / `acp` — negotiable endpoints whose `initialize` handshakes are
//!   implemented; deeper methods return `method not found` until a backend is
//!   registered (a terminal has no language/agent backend of its own).
//!
//! The JSON-RPC 2.0 message model and the LSP types are reused from `rusty_lsp`.

use super::grid::{Grid, row_text};
use rusty_lsp::error::{ResponseError, codes};
use rusty_lsp::jsonrpc::{Message, Notification, Request, Response};
use serde_json::{Value, json};

/// Private OSC code for the structured channel. Distinctive and unassigned by
/// the common terminals.
pub(crate) const OSC_CODE: &str = "5379";
/// The `OSC <CODE> ;` byte prefix the parser matches to route a payload here.
pub(crate) const OSC_PREFIX: &[u8] = b"5379;";

const CHANNEL_VERSION_MIN: u32 = 1;
const CHANNEL_VERSION_MAX: u32 = 1;
/// MCP wire-protocol revision we implement (the dated MCP spec version).
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const TERMINAL_NAME: &str = "rusty_term";
const TERMINAL_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Sub-protocols this channel speaks, in advertised order. `channel` is the
/// meta-protocol every client starts with; the rest are sub-addressed by tag.
const SUPPORTED_PROTOCOLS: &[&str] = &["channel", "mcp", "lsp", "acp", "render"];

/// Resources that emit `notifications/resources/updated` when their underlying
/// state changes. Only low-frequency, discrete signals are pushable; high-churn
/// resources (screen/scrollback/cursor) are polled via `resources/read`.
pub(crate) const RES_CWD: &str = "terminal://cwd";
pub(crate) const RES_TITLE: &str = "terminal://title";
pub(crate) const RES_EXIT: &str = "terminal://exit";
pub(crate) const RES_COMMAND: &str = "terminal://command";
pub(crate) const RES_DIMENSIONS: &str = "terminal://dimensions";
const NOTIFIABLE: &[&str] = &[RES_CWD, RES_TITLE, RES_EXIT, RES_COMMAND, RES_DIMENSIONS];

/// Per-session channel state held on the [`Grid`]: the set of resource URIs a
/// client has subscribed to for change notifications. Empty until the client
/// calls MCP `resources/subscribe`.
#[derive(Default)]
pub(crate) struct ChannelState {
    subscriptions: Vec<&'static str>,
}

impl ChannelState {
    fn subscribe(&mut self, uri: &'static str) {
        if !self.subscriptions.contains(&uri) {
            self.subscriptions.push(uri);
        }
    }

    fn unsubscribe(&mut self, uri: &str) {
        self.subscriptions.retain(|s| *s != uri);
    }

    fn is_subscribed(&self, uri: &'static str) -> bool {
        self.subscriptions.contains(&uri)
    }
}

/// Handle one channel OSC payload — the bytes after the `OSC <CODE> ;` prefix,
/// i.e. `<protocol> ; <json>`. Routes the JSON-RPC message and queues any reply
/// onto `responses` (child-bound).
pub(crate) fn handle(payload: &[u8], g: &mut Grid, responses: &mut Vec<u8>) {
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
fn dispatch(protocol: &str, req: &Request, g: &mut Grid) -> Result<Value, ResponseError> {
    match protocol {
        "channel" => channel_request(req),
        "mcp" => mcp_request(req, g),
        "lsp" => lsp_request(req),
        "acp" => acp_request(req),
        "render" => render_request(req, g),
        other => Err(error(
            codes::METHOD_NOT_FOUND,
            format!("unknown channel protocol: {other}"),
        )),
    }
}

/// The `channel` meta-protocol: version negotiation and schema discovery. A
/// client opens every session here before using a sub-protocol.
fn channel_request(req: &Request) -> Result<Value, ResponseError> {
    match req.method.as_str() {
        "initialize" => channel_initialize(req.params.as_ref()),
        "describe" => Ok(channel_describe()),
        other => Err(error(
            codes::METHOD_NOT_FOUND,
            format!("channel: unknown method {other}"),
        )),
    }
}

/// Negotiate the channel version and protocol set with a connecting client.
///
/// The client advertises the highest channel version it speaks (`params.version`,
/// defaulting to ours when absent) and, optionally, the sub-protocols it cares
/// about (`params.protocols`). We agree on `min(client, ours)`, erroring with the
/// supported range if that would drop below our floor, and reply with the agreed
/// version, the intersected protocol list, and a per-protocol capability map.
fn channel_initialize(params: Option<&Value>) -> Result<Value, ResponseError> {
    let client_version = params
        .and_then(|p| p.get("version"))
        .and_then(Value::as_u64)
        .map(|v| v as u32)
        .unwrap_or(CHANNEL_VERSION_MAX);
    let agreed = client_version.min(CHANNEL_VERSION_MAX);
    if agreed < CHANNEL_VERSION_MIN {
        return Err(ResponseError {
            code: codes::INVALID_PARAMS,
            message: format!(
                "unsupported channel version {client_version}; this terminal speaks {CHANNEL_VERSION_MIN}..={CHANNEL_VERSION_MAX}"
            ),
            data: Some(json!({ "supported": { "min": CHANNEL_VERSION_MIN, "max": CHANNEL_VERSION_MAX } })),
        });
    }
    // Intersect the client's protocol wishlist with what we offer, preserving our
    // advertised order; absent a wishlist, offer everything.
    let protocols: Vec<&str> = match params
        .and_then(|p| p.get("protocols"))
        .and_then(Value::as_array)
    {
        Some(wanted) => {
            let wanted: Vec<&str> = wanted.iter().filter_map(Value::as_str).collect();
            SUPPORTED_PROTOCOLS
                .iter()
                .copied()
                .filter(|p| wanted.contains(p))
                .collect()
        }
        None => SUPPORTED_PROTOCOLS.to_vec(),
    };
    Ok(json!({
        "version": agreed,
        "protocols": protocols,
        "capabilities": channel_capabilities(),
        "terminalInfo": { "name": TERMINAL_NAME, "version": TERMINAL_VERSION },
    }))
}

/// Per-protocol capability summary returned by `initialize`, so a client knows
/// what each sub-protocol supports without a round-trip per protocol.
fn channel_capabilities() -> Value {
    json!({
        "channel": { "methods": ["initialize", "describe"] },
        "mcp": { "tools": true, "resources": true, "subscribe": true },
        "lsp": { "initialize": true },
        "acp": { "initialize": true },
        "render": { "set_status": true, "clear_status": true },
    })
}

/// Machine-readable schema of the whole channel: the supported version range
/// and, per sub-protocol, its version and method list. Lets a client discover
/// the exact contract programmatically instead of hard-coding it — the versioned
/// schema that anchors the dual-channel protocol.
fn channel_describe() -> Value {
    json!({
        "version": { "min": CHANNEL_VERSION_MIN, "max": CHANNEL_VERSION_MAX },
        "terminalInfo": { "name": TERMINAL_NAME, "version": TERMINAL_VERSION },
        "protocols": {
            "channel": { "methods": ["initialize", "describe"] },
            "mcp": {
                "version": MCP_PROTOCOL_VERSION,
                "methods": ["initialize", "tools/list", "tools/call", "resources/list", "resources/read", "resources/subscribe", "resources/unsubscribe"],
                "notifications": ["notifications/resources/updated", "notifications/command_finished"],
            },
            "lsp": { "methods": ["initialize"] },
            "acp": { "version": 1, "methods": ["initialize"] },
            "render": { "methods": ["set_status", "clear_status"] },
        },
    })
}

/// The MCP server: exposes the terminal's state to agents as tools and resources.
fn mcp_request(req: &Request, g: &mut Grid) -> Result<Value, ResponseError> {
    match req.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": { "tools": {}, "resources": { "subscribe": true } },
            "serverInfo": { "name": TERMINAL_NAME, "version": TERMINAL_VERSION },
        })),
        "tools/list" => Ok(json!({ "tools": mcp_tools() })),
        "tools/call" => mcp_call(req.params.as_ref(), g),
        "resources/list" => Ok(json!({ "resources": mcp_resources() })),
        "resources/read" => mcp_resource_read(req.params.as_ref(), g),
        "resources/subscribe" => mcp_subscribe(req.params.as_ref(), g),
        "resources/unsubscribe" => mcp_unsubscribe(req.params.as_ref(), g),
        other => Err(error(
            codes::METHOD_NOT_FOUND,
            format!("mcp: unknown method {other}"),
        )),
    }
}

/// Record a `resources/subscribe`: the client wants `notifications/resources/
/// updated` when this resource changes. Only [`NOTIFIABLE`] resources push
/// updates, so subscribing to anything else is an error rather than a silent
/// no-op the client would wait on forever.
fn mcp_subscribe(params: Option<&Value>, g: &mut Grid) -> Result<Value, ResponseError> {
    let uri = params
        .and_then(|p| p.get("uri"))
        .and_then(Value::as_str)
        .ok_or_else(|| error(codes::INVALID_PARAMS, "missing resource uri"))?;
    let canonical = NOTIFIABLE
        .iter()
        .copied()
        .find(|&u| u == uri)
        .ok_or_else(|| error(codes::INVALID_PARAMS, format!("resource not subscribable: {uri}")))?;
    g.channel.subscribe(canonical);
    Ok(json!({}))
}

/// Drop a `resources/subscribe`. An unknown or never-subscribed URI is a no-op.
fn mcp_unsubscribe(params: Option<&Value>, g: &mut Grid) -> Result<Value, ResponseError> {
    let uri = params
        .and_then(|p| p.get("uri"))
        .and_then(Value::as_str)
        .ok_or_else(|| error(codes::INVALID_PARAMS, "missing resource uri"))?;
    g.channel.unsubscribe(uri);
    Ok(json!({}))
}

/// Push a `notifications/resources/updated` to the child for `uri` when a client
/// is subscribed — the structured channel's event half. Called from the state
/// mutators (OSC 7 cwd, OSC 0/2 title) right after the change lands, so the
/// notification rides the same child-bound `responses` egress as every reply.
pub(crate) fn notify_resource_changed(g: &Grid, uri: &'static str, responses: &mut Vec<u8>) {
    if !g.channel.is_subscribed(uri) {
        return;
    }
    let note = Notification {
        method: "notifications/resources/updated".into(),
        params: Some(json!({ "uri": uri })),
    };
    send("mcp", &Message::Notification(note), responses);
}

/// Push a typed `notifications/command_finished { exit }` when a command ends
/// (OSC 133;D) and a client is subscribed to `terminal://exit`. Unlike a generic
/// `resources/updated`, this carries the exit code (or `null` when the shell
/// omitted it) in the push itself, so the client needs no follow-up read.
pub(crate) fn notify_command_finished(g: &Grid, exit: Option<i32>, responses: &mut Vec<u8>) {
    if !g.channel.is_subscribed(RES_EXIT) {
        return;
    }
    let note = Notification {
        method: "notifications/command_finished".into(),
        params: Some(json!({ "exit": exit })),
    };
    send("mcp", &Message::Notification(note), responses);
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
        { "name": "get_cursor",
          "description": "The cursor position in character cells, as \"COL,ROW\" (both zero-based).",
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
        "get_cursor" => cursor_text(g),
        other => return Err(error(codes::INVALID_PARAMS, format!("unknown tool: {other}"))),
    };
    Ok(json!({ "content": [ { "type": "text", "text": text } ] }))
}

/// The MCP resource catalogue: the same terminal state as addressable
/// `terminal://` resources, the idiomatic MCP way to expose readable context.
fn mcp_resources() -> Value {
    let res = |uri: &str, name: &str, description: &str| {
        json!({ "uri": uri, "name": name, "description": description, "mimeType": "text/plain" })
    };
    json!([
        res("terminal://screen", "Visible screen", "The current visible screen, as text."),
        res("terminal://scrollback", "Scrollback", "Lines that scrolled off the top, oldest first."),
        res("terminal://cwd", "Working directory", "The child's CWD as reported via OSC 7."),
        res("terminal://title", "Window title", "The title set by the child via OSC 0/2."),
        res("terminal://dimensions", "Dimensions", "The terminal size in cells, as \"COLSxROWS\"."),
        res("terminal://cursor", "Cursor", "The cursor position in cells, as \"COL,ROW\"."),
        res("terminal://exit", "Last exit status", "Exit code of the last finished command (OSC 133;D), or empty."),
        res("terminal://command", "Last command output", "Output text of the last finished command (between OSC 133;C and ;D)."),
    ])
}

/// Read one MCP resource by `params.uri`, returning the standard
/// `{ contents: [...] }`. Mirrors the `get_*` tools over the resource URIs.
fn mcp_resource_read(params: Option<&Value>, g: &Grid) -> Result<Value, ResponseError> {
    let uri = params
        .and_then(|p| p.get("uri"))
        .and_then(Value::as_str)
        .ok_or_else(|| error(codes::INVALID_PARAMS, "missing resource uri"))?;
    let text = match uri {
        "terminal://screen" => screen_text(g),
        "terminal://scrollback" => scrollback_text(g, 1000),
        "terminal://cwd" => g.cwd.clone(),
        "terminal://title" => g.title.clone(),
        "terminal://dimensions" => format!("{}x{}", g.cols, g.rows),
        "terminal://cursor" => cursor_text(g),
        "terminal://exit" => g.last_command_exit().map(|c| c.to_string()).unwrap_or_default(),
        "terminal://command" => g.last_command_output().unwrap_or_default().to_string(),
        other => return Err(error(codes::INVALID_PARAMS, format!("unknown resource: {other}"))),
    };
    Ok(json!({ "contents": [ { "uri": uri, "mimeType": "text/plain", "text": text } ] }))
}

/// The cursor position as `"COL,ROW"` (both zero-based) — the format shared by
/// the `get_cursor` tool and the `terminal://cursor` resource.
fn cursor_text(g: &Grid) -> String {
    format!("{},{}", g.cursor.0, g.cursor.1)
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

/// The `render` protocol: a client declares terminal-owned UI the renderer
/// composites independent of the child's text stream — currently a status-line
/// overlay across the bottom row. An unaware terminal ignores the OSC, so the
/// child simply gets no overlay (graceful degradation).
fn render_request(req: &Request, g: &mut Grid) -> Result<Value, ResponseError> {
    match req.method.as_str() {
        "set_status" => render_set_status(req.params.as_ref(), g),
        "clear_status" => {
            g.clear_status_line();
            Ok(json!({}))
        }
        other => Err(error(
            codes::METHOD_NOT_FOUND,
            format!("render: unknown method {other}"),
        )),
    }
}

/// Set the bottom-row status overlay. `text` is required; `fg`/`bg` are optional
/// `0xRRGGBB` integers defaulting to the grid's current default colors.
fn render_set_status(params: Option<&Value>, g: &mut Grid) -> Result<Value, ResponseError> {
    let params = params.ok_or_else(|| error(codes::INVALID_PARAMS, "missing params"))?;
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| error(codes::INVALID_PARAMS, "missing status text"))?;
    let fg = params.get("fg").and_then(Value::as_u64).map(|v| v as u32);
    let bg = params.get("bg").and_then(Value::as_u64).map(|v| v as u32);
    g.set_status_line(text.to_string(), fg, bg);
    Ok(json!({}))
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
        .map(|line| row_text(&line.cells, &g.clusters))
        .collect::<Vec<_>>()
        .join("\n")
}

fn error(code: i64, message: impl Into<String>) -> ResponseError {
    ResponseError { code, message: message.into(), data: None }
}
