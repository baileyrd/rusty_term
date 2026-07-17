//! L13 structured side-channel: a private-OSC JSON-RPC transport hosting the
//! adjacent protocols (MCP, plus LSP/ACP negotiation).
//!
//! **Wire format:** `OSC <CODE> ; <protocol> ; <json-rpc-message> ST` — one
//! JSON-RPC 2.0 message per OSC. It is full-duplex: the child emits these on its
//! stdout (the host parses them out of the byte stream) and replies by queuing
//! OSCs on the response channel, which the driver writes to the PTY master — i.e.
//! the child's stdin. serde_json escapes control bytes, so a JSON payload never
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
//! **Crate boundary:** this crate has no dependency on rusty_term's `Grid` (or
//! any other rusty_term type). Everything it needs from the host terminal is
//! expressed as the [`TerminalState`] trait — the whole point of splitting it
//! out of `rusty_term`'s `core` module: the side-channel is independently
//! buildable, testable, and reusable against any terminal implementation that
//! can implement thirteen narrow methods, rather than being welded to one
//! specific `Grid` struct.
//!
//! The JSON-RPC 2.0 message model and the LSP types are reused from `rusty_lsp`.

use rusty_lsp::error::{ResponseError, codes};
use rusty_lsp::jsonrpc::{Message, Notification, Request, Response};
use serde_json::{Value, json};

/// Private OSC code for the structured channel. Distinctive and unassigned by
/// the common terminals.
pub const OSC_CODE: &str = "5379";
/// The `OSC <CODE> ;` byte prefix the parser matches to route a payload here.
pub const OSC_PREFIX: &[u8] = b"5379;";

const CHANNEL_VERSION_MIN: u32 = 1;
const CHANNEL_VERSION_MAX: u32 = 1;
/// MCP wire-protocol revision this crate implements (the dated MCP spec version).
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const TERMINAL_NAME: &str = "rusty_term";
const TERMINAL_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Sub-protocols this channel speaks, in advertised order. `channel` is the
/// meta-protocol every client starts with; the rest are sub-addressed by tag.
const SUPPORTED_PROTOCOLS: &[&str] = &["channel", "mcp", "lsp", "acp", "render"];

/// Resources that emit `notifications/resources/updated` when their underlying
/// state changes. Only low-frequency, discrete signals are pushable; high-churn
/// resources (screen/scrollback/cursor) are polled via `resources/read`.
pub const RES_CWD: &str = "terminal://cwd";
pub const RES_TITLE: &str = "terminal://title";
pub const RES_EXIT: &str = "terminal://exit";
pub const RES_COMMAND: &str = "terminal://command";
pub const RES_DIMENSIONS: &str = "terminal://dimensions";
const NOTIFIABLE: &[&str] = &[RES_CWD, RES_TITLE, RES_EXIT, RES_COMMAND, RES_DIMENSIONS];

/// Everything this crate needs to read or mutate on the host terminal. A host
/// implements this once (rusty_term's `Grid` does) and every protocol handler
/// here is written against the trait, never against a concrete terminal type.
pub trait TerminalState {
    /// The visible screen as text: one line per row, trailing blank rows dropped.
    fn screen_text(&self) -> String;
    /// The most recent `max` scrollback lines as text (oldest first).
    fn scrollback_text(&self, max: usize) -> String;
    /// The child's working directory as last reported via OSC 7 (empty if none).
    fn cwd(&self) -> &str;
    /// The window title as last reported via OSC 0/2 (empty if none).
    fn title(&self) -> &str;
    /// Terminal size in cells, as `(cols, rows)`.
    fn dimensions(&self) -> (usize, usize);
    /// Cursor position in cells, as `(col, row)` (both zero-based).
    fn cursor(&self) -> (usize, usize);
    /// Exit code of the last finished command (OSC 133;D), if any finished yet.
    fn last_command_exit(&self) -> Option<i32>;
    /// Output text of the last finished command (OSC 133;C..D), if captured.
    fn last_command_output(&self) -> Option<&str>;
    /// Set the bottom-row status overlay (the `render` protocol).
    fn set_status_line(&mut self, text: String, fg: Option<u32>, bg: Option<u32>);
    /// Clear the status overlay set by [`Self::set_status_line`].
    fn clear_status_line(&mut self);
    /// Whether a client has subscribed to change notifications for `uri`.
    fn is_subscribed(&self, uri: &'static str) -> bool;
    /// Record a client's subscription to `uri` (idempotent).
    fn subscribe(&mut self, uri: &'static str);
    /// Drop a client's subscription to `uri` (a no-op if never subscribed).
    fn unsubscribe(&mut self, uri: &str);
}

/// Per-session channel state a host holds (typically as a `Grid` field) and
/// delegates [`TerminalState`]'s subscribe/unsubscribe/is_subscribed to: the
/// set of resource URIs a client has subscribed to for change notifications.
/// Empty until the client calls MCP `resources/subscribe`.
#[derive(Default)]
pub struct ChannelState {
    subscriptions: Vec<&'static str>,
}

impl ChannelState {
    pub fn subscribe(&mut self, uri: &'static str) {
        if !self.subscriptions.contains(&uri) {
            self.subscriptions.push(uri);
        }
    }

    pub fn unsubscribe(&mut self, uri: &str) {
        self.subscriptions.retain(|s| *s != uri);
    }

    pub fn is_subscribed(&self, uri: &'static str) -> bool {
        self.subscriptions.contains(&uri)
    }
}

/// Handle one channel OSC payload — the bytes after the `OSC <CODE> ;` prefix,
/// i.e. `<protocol> ; <json>`. Routes the JSON-RPC message and queues any reply
/// onto `responses` (child-bound).
pub fn handle(payload: &[u8], state: &mut impl TerminalState, responses: &mut Vec<u8>) {
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
            let response = match dispatch(protocol, &req, state) {
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
fn dispatch(
    protocol: &str,
    req: &Request,
    state: &mut impl TerminalState,
) -> Result<Value, ResponseError> {
    match protocol {
        "channel" => channel_request(req),
        "mcp" => mcp_request(req, state),
        "lsp" => lsp_request(req),
        "acp" => acp_request(req),
        "render" => render_request(req, state),
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
            data: Some(
                json!({ "supported": { "min": CHANNEL_VERSION_MIN, "max": CHANNEL_VERSION_MAX } }),
            ),
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
fn mcp_request(req: &Request, state: &mut impl TerminalState) -> Result<Value, ResponseError> {
    match req.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": { "tools": {}, "resources": { "subscribe": true } },
            "serverInfo": { "name": TERMINAL_NAME, "version": TERMINAL_VERSION },
        })),
        "tools/list" => Ok(json!({ "tools": mcp_tools() })),
        "tools/call" => mcp_call(req.params.as_ref(), state),
        "resources/list" => Ok(json!({ "resources": mcp_resources() })),
        "resources/read" => mcp_resource_read(req.params.as_ref(), state),
        "resources/subscribe" => mcp_subscribe(req.params.as_ref(), state),
        "resources/unsubscribe" => mcp_unsubscribe(req.params.as_ref(), state),
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
fn mcp_subscribe(
    params: Option<&Value>,
    state: &mut impl TerminalState,
) -> Result<Value, ResponseError> {
    let uri = params
        .and_then(|p| p.get("uri"))
        .and_then(Value::as_str)
        .ok_or_else(|| error(codes::INVALID_PARAMS, "missing resource uri"))?;
    let canonical = NOTIFIABLE
        .iter()
        .copied()
        .find(|&u| u == uri)
        .ok_or_else(|| {
            error(
                codes::INVALID_PARAMS,
                format!("resource not subscribable: {uri}"),
            )
        })?;
    state.subscribe(canonical);
    Ok(json!({}))
}

/// Drop a `resources/subscribe`. An unknown or never-subscribed URI is a no-op.
fn mcp_unsubscribe(
    params: Option<&Value>,
    state: &mut impl TerminalState,
) -> Result<Value, ResponseError> {
    let uri = params
        .and_then(|p| p.get("uri"))
        .and_then(Value::as_str)
        .ok_or_else(|| error(codes::INVALID_PARAMS, "missing resource uri"))?;
    state.unsubscribe(uri);
    Ok(json!({}))
}

/// Push a `notifications/resources/updated` to the child for `uri` when a client
/// is subscribed — the structured channel's event half. Called from the state
/// mutators (OSC 7 cwd, OSC 0/2 title) right after the change lands, so the
/// notification rides the same child-bound `responses` egress as every reply.
pub fn notify_resource_changed(
    state: &impl TerminalState,
    uri: &'static str,
    responses: &mut Vec<u8>,
) {
    if !state.is_subscribed(uri) {
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
pub fn notify_command_finished(
    state: &impl TerminalState,
    exit: Option<i32>,
    responses: &mut Vec<u8>,
) {
    if !state.is_subscribed(RES_EXIT) {
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
fn mcp_call(params: Option<&Value>, state: &impl TerminalState) -> Result<Value, ResponseError> {
    let params = params.ok_or_else(|| error(codes::INVALID_PARAMS, "missing params"))?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| error(codes::INVALID_PARAMS, "missing tool name"))?;
    let args = params.get("arguments");

    let text = match name {
        "get_screen" => state.screen_text(),
        "get_scrollback" => {
            let lines = args
                .and_then(|a| a.get("lines"))
                .and_then(Value::as_u64)
                .unwrap_or(100) as usize;
            state.scrollback_text(lines)
        }
        "get_cwd" => state.cwd().to_string(),
        "get_title" => state.title().to_string(),
        "get_dimensions" => {
            let (cols, rows) = state.dimensions();
            format!("{cols}x{rows}")
        }
        "get_cursor" => cursor_text(state),
        other => {
            return Err(error(
                codes::INVALID_PARAMS,
                format!("unknown tool: {other}"),
            ));
        }
    };
    Ok(json!({ "content": [ { "type": "text", "text": text } ] }))
}

/// The MCP resource catalogue: the same terminal state as addressable
/// `terminal://` resources, the idiomatic MCP way to expose readable context.
fn mcp_resources() -> Value {
    let res = |uri: &str, name: &str, description: &str| json!({ "uri": uri, "name": name, "description": description, "mimeType": "text/plain" });
    json!([
        res(
            "terminal://screen",
            "Visible screen",
            "The current visible screen, as text."
        ),
        res(
            "terminal://scrollback",
            "Scrollback",
            "Lines that scrolled off the top, oldest first."
        ),
        res(
            "terminal://cwd",
            "Working directory",
            "The child's CWD as reported via OSC 7."
        ),
        res(
            "terminal://title",
            "Window title",
            "The title set by the child via OSC 0/2."
        ),
        res(
            "terminal://dimensions",
            "Dimensions",
            "The terminal size in cells, as \"COLSxROWS\"."
        ),
        res(
            "terminal://cursor",
            "Cursor",
            "The cursor position in cells, as \"COL,ROW\"."
        ),
        res(
            "terminal://exit",
            "Last exit status",
            "Exit code of the last finished command (OSC 133;D), or empty."
        ),
        res(
            "terminal://command",
            "Last command output",
            "Output text of the last finished command (between OSC 133;C and ;D)."
        ),
    ])
}

/// Read one MCP resource by `params.uri`, returning the standard
/// `{ contents: [...] }`. Mirrors the `get_*` tools over the resource URIs.
fn mcp_resource_read(
    params: Option<&Value>,
    state: &impl TerminalState,
) -> Result<Value, ResponseError> {
    let uri = params
        .and_then(|p| p.get("uri"))
        .and_then(Value::as_str)
        .ok_or_else(|| error(codes::INVALID_PARAMS, "missing resource uri"))?;
    let text = match uri {
        "terminal://screen" => state.screen_text(),
        "terminal://scrollback" => state.scrollback_text(1000),
        "terminal://cwd" => state.cwd().to_string(),
        "terminal://title" => state.title().to_string(),
        "terminal://dimensions" => {
            let (cols, rows) = state.dimensions();
            format!("{cols}x{rows}")
        }
        "terminal://cursor" => cursor_text(state),
        "terminal://exit" => state
            .last_command_exit()
            .map(|c| c.to_string())
            .unwrap_or_default(),
        "terminal://command" => state.last_command_output().unwrap_or_default().to_string(),
        other => {
            return Err(error(
                codes::INVALID_PARAMS,
                format!("unknown resource: {other}"),
            ));
        }
    };
    Ok(json!({ "contents": [ { "uri": uri, "mimeType": "text/plain", "text": text } ] }))
}

/// The cursor position as `"COL,ROW"` (both zero-based) — the format shared by
/// the `get_cursor` tool and the `terminal://cursor` resource.
fn cursor_text(state: &impl TerminalState) -> String {
    let (col, row) = state.cursor();
    format!("{col},{row}")
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
fn render_request(req: &Request, state: &mut impl TerminalState) -> Result<Value, ResponseError> {
    match req.method.as_str() {
        "set_status" => render_set_status(req.params.as_ref(), state),
        "clear_status" => {
            state.clear_status_line();
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
fn render_set_status(
    params: Option<&Value>,
    state: &mut impl TerminalState,
) -> Result<Value, ResponseError> {
    let params = params.ok_or_else(|| error(codes::INVALID_PARAMS, "missing params"))?;
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| error(codes::INVALID_PARAMS, "missing status text"))?;
    let fg = params.get("fg").and_then(Value::as_u64).map(|v| v as u32);
    let bg = params.get("bg").and_then(Value::as_u64).map(|v| v as u32);
    state.set_status_line(text.to_string(), fg, bg);
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

fn error(code: i64, message: impl Into<String>) -> ResponseError {
    ResponseError {
        code,
        message: message.into(),
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal in-memory [`TerminalState`] for exercising the protocol
    /// handlers without any of rusty_term's `Grid`/parser machinery — the
    /// payoff of the trait boundary: this crate's tests need no rusty_term
    /// dependency at all.
    #[derive(Default)]
    struct FakeTerminal {
        screen: String,
        scrollback: Vec<String>,
        cwd: String,
        title: String,
        dims: (usize, usize),
        cursor: (usize, usize),
        exit: Option<i32>,
        command_output: Option<String>,
        status: Option<(String, Option<u32>, Option<u32>)>,
        channel: ChannelState,
    }

    impl TerminalState for FakeTerminal {
        fn screen_text(&self) -> String {
            self.screen.clone()
        }
        fn scrollback_text(&self, max: usize) -> String {
            let skip = self.scrollback.len().saturating_sub(max);
            self.scrollback[skip..].join("\n")
        }
        fn cwd(&self) -> &str {
            &self.cwd
        }
        fn title(&self) -> &str {
            &self.title
        }
        fn dimensions(&self) -> (usize, usize) {
            self.dims
        }
        fn cursor(&self) -> (usize, usize) {
            self.cursor
        }
        fn last_command_exit(&self) -> Option<i32> {
            self.exit
        }
        fn last_command_output(&self) -> Option<&str> {
            self.command_output.as_deref()
        }
        fn set_status_line(&mut self, text: String, fg: Option<u32>, bg: Option<u32>) {
            self.status = Some((text, fg, bg));
        }
        fn clear_status_line(&mut self) {
            self.status = None;
        }
        fn is_subscribed(&self, uri: &'static str) -> bool {
            self.channel.is_subscribed(uri)
        }
        fn subscribe(&mut self, uri: &'static str) {
            self.channel.subscribe(uri);
        }
        fn unsubscribe(&mut self, uri: &str) {
            self.channel.unsubscribe(uri);
        }
    }

    fn call(state: &mut FakeTerminal, protocol: &str, method: &str, params: Value) -> Value {
        let payload = format!(
            "{protocol};{}",
            json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
        );
        let mut responses = Vec::new();
        handle(payload.as_bytes(), state, &mut responses);
        extract_result(&responses)
    }

    /// Pull the JSON payload out of one queued `OSC 5379;<protocol>;<json> ST`
    /// response frame.
    fn extract_result(responses: &[u8]) -> Value {
        let text = std::str::from_utf8(responses).unwrap();
        let body = text
            .strip_prefix("\x1b]5379;")
            .unwrap()
            .strip_suffix("\x1b\\")
            .unwrap();
        let (_protocol, json) = body.split_once(';').unwrap();
        let msg: Value = serde_json::from_str(json).unwrap();
        msg["result"].clone()
    }

    #[test]
    fn channel_initialize_agrees_version_and_protocols() {
        let mut state = FakeTerminal::default();
        let result = call(&mut state, "channel", "initialize", json!({}));
        assert_eq!(result["version"], 1);
        assert_eq!(result["terminalInfo"]["name"], "rusty_term");
        assert_eq!(
            result["protocols"].as_array().unwrap().len(),
            SUPPORTED_PROTOCOLS.len()
        );
    }

    #[test]
    fn channel_initialize_rejects_unsupported_version() {
        let mut state = FakeTerminal::default();
        let payload = format!(
            "channel;{}",
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "version": 0 } })
        );
        let mut responses = Vec::new();
        handle(payload.as_bytes(), &mut state, &mut responses);
        let text = std::str::from_utf8(&responses).unwrap();
        let body = text
            .strip_prefix("\x1b]5379;")
            .unwrap()
            .strip_suffix("\x1b\\")
            .unwrap();
        let (_protocol, json) = body.split_once(';').unwrap();
        let msg: Value = serde_json::from_str(json).unwrap();
        assert_eq!(msg["error"]["code"], codes::INVALID_PARAMS);
    }

    #[test]
    fn mcp_get_cwd_and_title_tools_read_through_the_trait() {
        let mut state = FakeTerminal {
            cwd: "/home/user".into(),
            title: "session".into(),
            ..Default::default()
        };
        let cwd = call(
            &mut state,
            "mcp",
            "tools/call",
            json!({ "name": "get_cwd" }),
        );
        assert_eq!(cwd["content"][0]["text"], "/home/user");
        let title = call(
            &mut state,
            "mcp",
            "tools/call",
            json!({ "name": "get_title" }),
        );
        assert_eq!(title["content"][0]["text"], "session");
    }

    #[test]
    fn mcp_get_dimensions_and_cursor_format_correctly() {
        let mut state = FakeTerminal {
            dims: (80, 24),
            cursor: (5, 3),
            ..Default::default()
        };
        let dims = call(
            &mut state,
            "mcp",
            "tools/call",
            json!({ "name": "get_dimensions" }),
        );
        assert_eq!(dims["content"][0]["text"], "80x24");
        let cursor = call(
            &mut state,
            "mcp",
            "tools/call",
            json!({ "name": "get_cursor" }),
        );
        assert_eq!(cursor["content"][0]["text"], "5,3");
    }

    #[test]
    fn mcp_resource_read_mirrors_tools() {
        let mut state = FakeTerminal {
            cwd: "/tmp".into(),
            ..Default::default()
        };
        let result = call(
            &mut state,
            "mcp",
            "resources/read",
            json!({ "uri": "terminal://cwd" }),
        );
        assert_eq!(result["contents"][0]["text"], "/tmp");
    }

    #[test]
    fn mcp_subscribe_then_notify_pushes_update() {
        let mut state = FakeTerminal::default();
        call(
            &mut state,
            "mcp",
            "resources/subscribe",
            json!({ "uri": RES_CWD }),
        );
        assert!(state.is_subscribed(RES_CWD));

        let mut responses = Vec::new();
        notify_resource_changed(&state, RES_CWD, &mut responses);
        let result = extract_result_notification(&responses);
        assert_eq!(result["method"], "notifications/resources/updated");
        assert_eq!(result["params"]["uri"], RES_CWD);
    }

    #[test]
    fn unsubscribed_resource_gets_no_notification() {
        let state = FakeTerminal::default();
        let mut responses = Vec::new();
        notify_resource_changed(&state, RES_CWD, &mut responses);
        assert!(responses.is_empty());
    }

    #[test]
    fn subscribing_to_a_non_notifiable_resource_errors() {
        let mut state = FakeTerminal::default();
        let payload = format!(
            "mcp;{}",
            json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/subscribe", "params": { "uri": "terminal://screen" } })
        );
        let mut responses = Vec::new();
        handle(payload.as_bytes(), &mut state, &mut responses);
        let text = std::str::from_utf8(&responses).unwrap();
        let body = text
            .strip_prefix("\x1b]5379;")
            .unwrap()
            .strip_suffix("\x1b\\")
            .unwrap();
        let (_protocol, json) = body.split_once(';').unwrap();
        let msg: Value = serde_json::from_str(json).unwrap();
        assert_eq!(msg["error"]["code"], codes::INVALID_PARAMS);
    }

    #[test]
    fn render_set_and_clear_status_round_trip_through_the_trait() {
        let mut state = FakeTerminal::default();
        call(
            &mut state,
            "render",
            "set_status",
            json!({ "text": "building..." }),
        );
        assert_eq!(state.status.as_ref().unwrap().0, "building...");
        call(&mut state, "render", "clear_status", json!({}));
        assert!(state.status.is_none());
    }

    #[test]
    fn lsp_and_acp_initialize_advertise_empty_backends() {
        let mut state = FakeTerminal::default();
        let lsp = call(&mut state, "lsp", "initialize", json!({}));
        assert_eq!(lsp["serverInfo"]["name"], "rusty_term");
        let acp = call(&mut state, "acp", "initialize", json!({}));
        assert_eq!(acp["agentInfo"]["name"], "rusty_term");
    }

    #[test]
    fn malformed_json_is_dropped_silently() {
        let mut state = FakeTerminal::default();
        let mut responses = Vec::new();
        handle(b"mcp;not json", &mut state, &mut responses);
        assert!(responses.is_empty());
    }

    #[test]
    fn non_utf8_payload_is_ignored() {
        let mut state = FakeTerminal::default();
        let mut responses = Vec::new();
        handle(&[0xff, 0xfe, 0xfd], &mut state, &mut responses);
        assert!(responses.is_empty());
    }

    fn extract_result_notification(responses: &[u8]) -> Value {
        let text = std::str::from_utf8(responses).unwrap();
        let body = text
            .strip_prefix("\x1b]5379;")
            .unwrap()
            .strip_suffix("\x1b\\")
            .unwrap();
        let (_protocol, json) = body.split_once(';').unwrap();
        serde_json::from_str(json).unwrap()
    }
}
