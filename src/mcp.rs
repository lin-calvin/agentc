//! MCP over HTTP (JSON-RPC) plus the human control panel.
//!
//! Design: there is exactly one API surface. Both the agent and the human panel are MCP
//! clients; they differ only by the `X-Agentc-Actor: human` header. Human-only tools
//! (pause/resume/status) are filtered out for the agent
//! only in the human's tools/list; the agent never sees them. Once paused, every
//! action tool returns PAUSED.
//!
//! JSON-RPC and HTTP are hand-written on purpose, with no tokio and no MCP SDK: the
//! compositor is a single-threaded calloop loop and we do not want an async runtime in
//! it, while the MCP surface here is tiny.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use serde_json::{json, Value};
use smithay::reexports::calloop::channel as cl_channel;

use crate::Msg;

const PROTOCOL_VERSION: &str = "2025-06-18";

// ------------------------------------------------------------------ HTTP

/// Bind a port, trying a few in sequence if `preferred` is taken (avoids clashing with
/// another instance).
pub fn bind(preferred: &str) -> Option<(String, TcpListener)> {
    let (host, base_port) = match preferred.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().ok()?),
        None => return None,
    };
    for off in 0..24u16 {
        let addr = format!("{host}:{}", base_port + off);
        if let Ok(l) = TcpListener::bind(&addr) {
            return Some((addr, l));
        }
    }
    None
}

pub fn spawn_http(listener: TcpListener, tx: cl_channel::Sender<Msg>) {
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let _ = handle_http(stream, tx);
            });
        }
    });
}

/// Read byte by byte until \r\n\r\n. Deliberately not a BufReader: after a WebSocket
/// upgrade we take over the raw stream, so anything over-read would be lost.
fn read_head(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut b = [0u8; 1];
    loop {
        let n = stream.read(&mut b)?;
        if n == 0 {
            break;
        }
        buf.push(b[0]);
        if buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n" {
            break;
        }
        if buf.len() > 64 * 1024 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn handle_http(mut stream: TcpStream, tx: cl_channel::Sender<Msg>) -> std::io::Result<()> {
    let head = read_head(&mut stream)?;
    let mut lines = head.lines();
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts
        .next()
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_string();

    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let hdr = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };

    // live channel
    if path == "/ws" && hdr("upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket")) {
        return serve_ws(stream, &head, tx);
    }

    let len: usize = hdr("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut body)?;
    }

    match (method.as_str(), path.as_str()) {
        ("GET", "/") => respond(&mut stream, "200 OK", "text/html; charset=utf-8", PANEL.as_bytes()),
        ("GET", "/health") => respond(&mut stream, "200 OK", "text/plain", b"ok"),
        ("POST", "/shutdown") => {
            let _ = respond(&mut stream, "200 OK", "text/plain", b"bye");
            let _ = std::fs::remove_file(crate::shim::url_file());
            std::thread::sleep(Duration::from_millis(80));
            std::process::exit(0);
        }
        ("POST", "/mcp") => {
            let incoming: Value = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(e) => {
                    let err = json!({"jsonrpc":"2.0","id":null,
                        "error":{"code":-32700,"message":format!("parse error: {e}")}});
                    return respond(
                        &mut stream,
                        "400 Bad Request",
                        "application/json",
                        err.to_string().as_bytes(),
                    );
                }
            };

            let actor = hdr("x-agentc-actor").unwrap_or("agent").to_string();
            let batch = incoming.is_array();
            let items: Vec<Value> = if batch {
                incoming.as_array().cloned().unwrap_or_default()
            } else {
                vec![incoming.clone()]
            };

            let mut out = Vec::new();
            for item in items {
                if let Some(resp) = dispatch_rpc(&item, &actor, &tx) {
                    out.push(resp);
                }
            }

            if out.is_empty() {
                return respond(&mut stream, "202 Accepted", "application/json", b"");
            }
            let body = if batch {
                Value::Array(out)
            } else {
                out.remove(0)
            };

            let accept = hdr("accept").unwrap_or("*/*");
            if accept.contains("text/event-stream") && !accept.contains("application/json") {
                let sse = format!("event: message\ndata: {}\n\n", body);
                respond(&mut stream, "200 OK", "text/event-stream", sse.as_bytes())
            } else {
                respond(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    body.to_string().as_bytes(),
                )
            }
        }
        ("GET", "/mcp") => respond(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain",
            b"server-initiated streams not supported",
        ),
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

// ------------------------------------------------------------------ live channel

/// One WebSocket = one frame-pushing thread plus this thread receiving input.
/// Frames are pushed only when the picture actually changes; a static screen costs
/// almost nothing.
fn serve_ws(stream: TcpStream, head: &str, tx: cl_channel::Sender<Msg>) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::Arc;

    let mut conn = crate::ws::WsConn::accept(stream, head)?;
    let mut out = conn.try_clone()?;

    let win = Arc::new(AtomicU32::new(0));
    let epoch = Arc::new(AtomicU64::new(0));

    let (w, e, txc) = (win.clone(), epoch.clone(), tx.clone());
    std::thread::spawn(move || {
        let mut since = 0u64;
        let mut last_epoch = u64::MAX; // the first message must be a full frame
        loop {
            let ep = e.load(Ordering::Relaxed);
            let mut full = false;
            if ep != last_epoch {
                since = 0; // window changed, the hash baseline is void
                last_epoch = ep;
                full = true; // the browser canvas still shows another window, so force a full frame
            }
            let id = w.load(Ordering::Relaxed);
            let started = std::time::Instant::now();
            let cmd = format!(
                "{{\"cmd\":\"stream\",\"id\":{id},\"since\":{since},\"timeout_ms\":150,\"full\":{full}}}"
            );
            let Ok(reply) = rpc_call(&cmd, &txc) else {
                break;
            };
            let Ok(v) = serde_json::from_str::<Value>(&reply) else {
                break;
            };
            let changed = v.get("changed").and_then(|x| x.as_bool()) == Some(true);
            if changed {
                since = v.get("hash").and_then(|x| x.as_u64()).unwrap_or(0);
            }
            // On change we send a frame; on timeout a lightweight status. Both as text frames.
            if out.send_text(&reply).is_err() {
                break;
            }
            // cap at ~20fps so animation cannot saturate the CPU
            let spent = started.elapsed();
            if spent < Duration::from_millis(33) {
                std::thread::sleep(Duration::from_millis(33) - spent);
            }
        }
        out.close();
    });

    loop {
        match conn.read() {
            Ok(crate::ws::Msg::Text(t)) => handle_input(&t, &tx, &win, &epoch),
            Ok(crate::ws::Msg::Ping(p)) => {
                let _ = conn.pong(&p);
            }
            Ok(crate::ws::Msg::Close) | Err(_) => break,
            _ => {}
        }
    }
    Ok(())
}

fn handle_input(
    text: &str,
    tx: &cl_channel::Sender<Msg>,
    win: &std::sync::Arc<std::sync::atomic::AtomicU32>,
    epoch: &std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    use std::sync::atomic::Ordering;
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let t = v.get("t").and_then(|x| x.as_str()).unwrap_or("");
    if t == "select" {
        let id = v.get("window").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        win.store(id, Ordering::Relaxed);
        epoch.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let id = win.load(Ordering::Relaxed);
    let btn = match v.get("b").and_then(|x| x.as_u64()).unwrap_or(0) {
        1 => "middle",
        2 => "right",
        _ => "left",
    };
    let num = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);

    let cmd = match t {
        "move" => format!(
            "{{\"cmd\":\"move\",\"x\":{},\"y\":{},\"id\":{id},\"actor\":\"human\"}}",
            num("x"),
            num("y")
        ),
        "down" => format!(
            "{{\"cmd\":\"press\",\"x\":{},\"y\":{},\"button\":\"{btn}\",\"id\":{id},\"actor\":\"human\"}}",
            num("x"),
            num("y")
        ),
        "up" => format!(
            "{{\"cmd\":\"release\",\"x\":{},\"y\":{},\"button\":\"{btn}\",\"id\":{id},\"actor\":\"human\"}}",
            num("x"),
            num("y")
        ),
        "wheel" => format!(
            "{{\"cmd\":\"scroll\",\"x\":{},\"y\":{},\"dy\":{},\"id\":{id},\"actor\":\"human\"}}",
            num("x"),
            num("y"),
            num("dy")
        ),
        "keydown" | "keyup" => format!(
            "{{\"cmd\":\"{t}\",\"code\":{},\"id\":{id},\"actor\":\"human\"}}",
            v.get("code").and_then(|x| x.as_u64()).unwrap_or(0)
        ),
        "text" => format!(
            "{{\"cmd\":\"type\",\"text\":{},\"id\":{id},\"actor\":\"human\"}}",
            serde_json::to_string(v.get("text").and_then(|x| x.as_str()).unwrap_or(""))
                .unwrap_or_else(|_| "\"\"".into())
        ),
        "control" => format!(
            "{{\"cmd\":\"{}\",\"actor\":\"human\"}}",
            match v.get("action").and_then(|x| x.as_str()).unwrap_or("") {
                "pause" => "pause",
                "resume" => "resume",
                _ => return,
            }
        ),
        _ => return,
    };
    let _ = rpc_call(&cmd, tx);
}

// ------------------------------------------------------------------ JSON-RPC

fn dispatch_rpc(msg: &Value, actor: &str, tx: &cl_channel::Sender<Msg>) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

    // notifications carry no id and get no reply
    let id = match id {
        Some(v) => v,
        None => {
            if method.starts_with("notifications/") {
                return None;
            }
            Value::Null
        }
    };

    let result: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "agentc", "version": env!("CARGO_PKG_VERSION") },
            "instructions": instructions(),
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_schemas(actor) })),
        "tools/call" => {
            let name = msg
                .pointer("/params/name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = msg
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));
            call_tool(&name, &args, actor, tx).map_err(|e| (-32000, e))
        }
        other => Err((-32601, format!("method not found: {other}"))),
    };

    Some(match result {
        Ok(r) => json!({"jsonrpc":"2.0","id":id,"result":r}),
        Err((code, message)) => json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}),
    })
}

fn instructions() -> &'static str {
    "agentc is a single-window virtual desktop. Every window is its own 1280x720 screen; \
     coordinates are relative to that window's top-left corner.\n\
     Start with `windows` to see what is open, then `screenshot` (returns PNG), then \
     click/type. After an action prefer `wait_for_idle` over blind sleeps: it returns when \
     the screen settles, which is both more reliable and cheaper.\n\
     Windows are maximized by default. Use set_mode/fullscreen if you really want no chrome, \
     but note that browser fullscreen hides the address bar and tab strip.\n\
     After `set_size` all previous coordinates are invalid, so take a fresh screenshot.\n\
     If a tool returns PAUSED, a human took over: stop and wait, do not retry."
}

// ------------------------------------------------------------------ tools

fn tool_schemas(actor: &str) -> Vec<Value> {
    let mut tools = vec![
        tool(
            "screenshot",
            "Capture one window as PNG. Coordinates are relative to that window's top-left (0,0).",
            json!({
                "type": "object",
                "properties": {
                    "window": {"type": "integer", "description": "window id; omit to use the active window"}
                }
            }),
        ),
        tool(
            "windows",
            "List all windows (id / app_id / title).",
            json!({"type": "object", "properties": {}}),
        ),
        tool(
            "focus",
            "Give keyboard focus to a window.",
            json!({
                "type": "object",
                "properties": {"window": {"type": "integer"}},
                "required": ["window"]
            }),
        ),
        tool(
            "click",
            "Click once at window-local coordinate (x,y).",
            json!({
                "type": "object",
                "properties": {
                    "x": {"type": "number"}, "y": {"type": "number"},
                    "window": {"type": "integer"},
                    "button": {"type": "string", "enum": ["left", "right", "middle"]},
                    "duration_ms": {"type": "integer", "description": "how long to hold the button; default 60, range 10-800"}
                },
                "required": ["x", "y"]
            }),
        ),
        tool(
            "double_click",
            "Double click. The gap between the two presses is adjustable (default 160ms).",
            json!({
                "type": "object",
                "properties": {
                    "x": {"type": "number"}, "y": {"type": "number"},
                    "window": {"type": "integer"},
                    "duration_ms": {"type": "integer"},
                    "interval_ms": {"type": "integer"}
                },
                "required": ["x", "y"]
            }),
        ),
        tool(
            "move",
            "Move the pointer to (x,y) without pressing (hover).",
            json!({
                "type": "object",
                "properties": {"x": {"type": "number"}, "y": {"type": "number"}, "window": {"type": "integer"}},
                "required": ["x", "y"]
            }),
        ),
        tool(
            "drag",
            "Drag from (x1,y1) to (x2,y2).",
            json!({
                "type": "object",
                "properties": {
                    "x1": {"type": "number"}, "y1": {"type": "number"},
                    "x2": {"type": "number"}, "y2": {"type": "number"},
                    "window": {"type": "integer"},
                    "hold_ms": {"type": "integer", "description": "ms to hold before moving; default 60"},
                    "steps": {"type": "integer", "description": "number of interpolation steps; default 10"}
                },
                "required": ["x1", "y1", "x2", "y2"]
            }),
        ),
        tool(
            "scroll",
            "Scroll at (x,y); positive dy scrolls down.",
            json!({
                "type": "object",
                "properties": {
                    "x": {"type": "number"}, "y": {"type": "number"},
                    "dy": {"type": "number"},
                    "window": {"type": "integer"}
                },
                "required": ["x", "y", "dy"]
            }),
        ),
        tool(
            "type",
            "Type text. Any Unicode works (CJK/emoji go through clipboard paste). A trailing \n is sent as Enter.",
            json!({
                "type": "object",
                "properties": {"text": {"type": "string"}, "window": {"type": "integer"}},
                "required": ["text"]
            }),
        ),
        tool(
            "key",
            "Press a key combination, e.g. CTRL+C, ENTER, ALT+TAB.",
            json!({
                "type": "object",
                "properties": {"combo": {"type": "string"}, "window": {"type": "integer"}},
                "required": ["combo"]
            }),
        ),
        tool(
            "wait_for_change",
            "Block until the screen actually changes (or times out), returning a fresh screenshot and the changed rects. Far more reliable than sleep.",
            json!({
                "type": "object",
                "properties": {
                    "window": {"type": "integer"},
                    "timeout_ms": {"type": "integer", "description": "default 3000"},
                    "min_area": {"type": "number", "description": "minimum changed-area fraction, default 0.002; filters out cursor blink"},
                    "image": {"type": "boolean", "description": "include a screenshot; default true"}
                }
            }),
        ),
        tool(
            "wait_for_idle",
            "Block until the screen settles (no change for quiet_ms), then return a screenshot. Use instead of blind sleeps.",
            json!({
                "type": "object",
                "properties": {
                    "window": {"type": "integer"},
                    "quiet_ms": {"type": "integer", "description": "how long without changes counts as settled; default 120"},
                    "grace_ms": {"type": "integer", "description": "grace period before settling may be declared; default 200"},
                    "timeout_ms": {"type": "integer", "description": "default 3000"},
                    "min_area": {"type": "number", "description": "default 0.002"},
                    "image": {"type": "boolean", "description": "default true"}
                }
            }),
        ),
        tool(
            "set_size",
            "Resize a window's virtual screen (reflows the app; old coordinates become invalid). Returns the new revision.",
            json!({
                "type": "object",
                "properties": {
                    "window": {"type": "integer"},
                    "width": {"type": "integer"},
                    "height": {"type": "integer"}
                },
                "required": ["width", "height"]
            }),
        ),
        tool(
            "set_mode",
            "Set window mode: windowed / maximized (default: fills the screen but keeps app chrome) / fullscreen (no chrome).",
            json!({
                "type": "object",
                "properties": {
                    "window": {"type": "integer"},
                    "mode": {"type": "string", "enum": ["windowed", "maximized", "fullscreen"]}
                },
                "required": ["mode"]
            }),
        ),
        tool(
            "wait",
            "Wait N ms. Prefer wait_for_idle.",
            json!({
                "type": "object",
                "properties": {"ms": {"type": "integer"}},
                "required": ["ms"]
            }),
        ),
        tool(
            "launch",
            "Launch a program and wait for its window (default up to 3s). Returns the new window id.",
            json!({
                "type": "object",
                "properties": {
                    "argv": {"type": "array", "items": {"type": "string"}},
                    "wait_ms": {"type": "integer", "description": "timeout for waiting on the window; 0 means do not wait"}
                },
                "required": ["argv"]
            }),
        ),
        tool(
            "wait_for_window",
            "Wait for a new window to appear (e.g. a dialog after clicking a button). Returns its info.",
            json!({
                "type": "object",
                "properties": {
                    "timeout_ms": {"type": "integer"},
                    "after_id": {"type": "integer", "description": "only consider windows with a greater id"},
                    "app_id": {"type": "string"}
                }
            }),
        ),
    ];

    if actor == "human" {
        tools.push(tool(
            "status",
            "(human only) Current state: paused flag, window list, recent action log.",
            json!({"type": "object", "properties": {}}),
        ));
        tools.push(tool(
            "pause",
            "(human only) Pause the agent: all of its actions will be rejected afterwards.",
            json!({"type": "object", "properties": {"reason": {"type": "string"}}}),
        ));
        tools.push(tool(
            "resume",
            "(human only) Resume the agent.",
            json!({"type": "object", "properties": {}}),
        ));
    }
    tools
}

fn tool(name: &str, desc: &str, schema: Value) -> Value {
    json!({"name": name, "description": desc, "inputSchema": schema})
}

/// Translate an MCP tool call into an internal control command, then wrap the reply back
/// into MCP content blocks.
fn call_tool(
    name: &str,
    args: &Value,
    actor: &str,
    tx: &cl_channel::Sender<Msg>,
) -> Result<Value, String> {
    let win = args.get("window").and_then(|v| v.as_i64());
    let win_part = win.map(|w| format!(",\"id\":{w}")).unwrap_or_default();

    let cmd = match name {
        "screenshot" => format!("{{\"cmd\":\"screenshot\",\"b64\":true{win_part}}}"),
        "windows" => "{\"cmd\":\"windows\"}".to_string(),
        "focus" => format!("{{\"cmd\":\"focus\"{}}}", win_part),
        "click" => format!(
            "{{\"cmd\":\"click\",\"x\":{},\"y\":{},\"button\":\"{}\",\"duration_ms\":{}{win_part}}}",
            num(args, "x"),
            num(args, "y"),
            args.get("button").and_then(|v| v.as_str()).unwrap_or("left"),
            args.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(60)
        ),
        "double_click" => format!(
            "{{\"cmd\":\"dblclick\",\"x\":{},\"y\":{},\"duration_ms\":{},\"interval_ms\":{}{win_part}}}",
            num(args, "x"),
            num(args, "y"),
            args.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(60),
            args.get("interval_ms").and_then(|v| v.as_i64()).unwrap_or(160)
        ),
        "move" => format!(
            "{{\"cmd\":\"move\",\"x\":{},\"y\":{}{win_part}}}",
            num(args, "x"),
            num(args, "y")
        ),
        "drag" => format!(
            "{{\"cmd\":\"drag\",\"x1\":{},\"y1\":{},\"x2\":{},\"y2\":{},\"hold_ms\":{},\"steps\":{}{win_part}}}",
            num(args, "x1"),
            num(args, "y1"),
            num(args, "x2"),
            num(args, "y2"),
            args.get("hold_ms").and_then(|v| v.as_i64()).unwrap_or(60),
            args.get("steps").and_then(|v| v.as_i64()).unwrap_or(10)
        ),
        "scroll" => format!(
            "{{\"cmd\":\"scroll\",\"x\":{},\"y\":{},\"dy\":{}{win_part}}}",
            num(args, "x"),
            num(args, "y"),
            num(args, "dy")
        ),
        "type" => format!(
            "{{\"cmd\":\"type\",\"text\":{}{win_part}}}",
            serde_json::to_string(args.get("text").and_then(|v| v.as_str()).unwrap_or(""))
                .unwrap_or_else(|_| "\"\"".into())
        ),
        "key" => format!(
            "{{\"cmd\":\"key\",\"combo\":{}{win_part}}}",
            serde_json::to_string(args.get("combo").and_then(|v| v.as_str()).unwrap_or(""))
                .unwrap_or_else(|_| "\"\"".into())
        ),
        "wait_for_change" | "wait_for_idle" => {
            let timeout = args.get("timeout_ms").and_then(|v| v.as_i64()).unwrap_or(3000);
            let min_area = args.get("min_area").and_then(|v| v.as_f64()).unwrap_or(0.002);
            let image = args.get("image").and_then(|v| v.as_bool()).unwrap_or(true);
            let quiet = args.get("quiet_ms").and_then(|v| v.as_i64()).unwrap_or(120);
            let grace = args.get("grace_ms").and_then(|v| v.as_i64()).unwrap_or(200);
            format!(
                "{{\"cmd\":\"{name}\",\"timeout_ms\":{timeout},\"min_area\":{min_area},\"image\":{image},\"quiet_ms\":{quiet},\"grace_ms\":{grace}{win_part}}}"
            )
        }
        "set_size" | "resize" => format!(
            "{{\"cmd\":\"resize\",\"width\":{},\"height\":{}{win_part}}}",
            args.get("width").and_then(|v| v.as_i64()).unwrap_or(0),
            args.get("height").and_then(|v| v.as_i64()).unwrap_or(0)
        ),
        "set_mode" => format!(
            "{{\"cmd\":\"set_mode\",\"mode\":{}{win_part}}}",
            serde_json::to_string(args.get("mode").and_then(|v| v.as_str()).unwrap_or("maximized"))
                .unwrap_or_else(|_| "\"maximized\"".into())
        ),
        "wait" => format!(
            "{{\"cmd\":\"wait\",\"ms\":{}}}",
            args.get("ms").and_then(|v| v.as_i64()).unwrap_or(0)
        ),
        "launch" => format!(
            "{{\"cmd\":\"launch\",\"argv\":{},\"wait_ms\":{}}}",
            serde_json::to_string(args.get("argv").unwrap_or(&json!([]))).unwrap_or_default(),
            args.get("wait_ms").and_then(|v| v.as_i64()).unwrap_or(3000)
        ),
        "wait_for_window" => format!(
            "{{\"cmd\":\"wait_for_window\",\"timeout_ms\":{},\"after_id\":{},\"app_id\":{}}}",
            args.get("timeout_ms").and_then(|v| v.as_i64()).unwrap_or(3000),
            args.get("after_id").and_then(|v| v.as_i64()).unwrap_or(0),
            match args.get("app_id").and_then(|v| v.as_str()) {
                Some(a) => serde_json::to_string(a).unwrap_or_default(),
                None => "null".to_string(),
            }
        ),
        "status" => "{\"cmd\":\"status\"}".to_string(),
        "pause" => "{\"cmd\":\"pause\"}".to_string(),
        "resume" => "{\"cmd\":\"resume\"}".to_string(),
        other => return Err(format!("unknown tool: {other}")),
    };

    // human-only tools are not for the agent
    if matches!(name, "status" | "pause" | "resume") && actor != "human" {
        return Err(format!("tool {name} is human-only"));
    }

    // The actor must be in the command body: if it only lives in the HTTP header the
    // daemon treats panel requests as agent ones. Consequences: the panel's status poll
    // would drain the new-window queue, and panel clicks would be blocked by the pause gate.
    let cmd = if actor == "human" && cmd.starts_with('{') {
        format!("{{\"actor\":\"human\",{}", &cmd[1..])
    } else {
        cmd
    };

    let reply = rpc_call(&cmd, tx)?;
    let v: Value = serde_json::from_str(&reply).unwrap_or_else(|_| json!({"ok":false,"error":"bad reply"}));

    // The daemon attaches windows created since the last reply; turn them into a readable line.
    let extra = v
        .get("new_windows")
        .and_then(|x| x.as_array())
        .filter(|a| !a.is_empty())
        .map(|a| {
            let items: Vec<String> = a
                .iter()
                .map(|w| {
                    let id = w.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
                    if w.get("dialog").and_then(|x| x.as_bool()) == Some(true) {
                        format!("#{id}(dialog)")
                    } else {
                        format!("#{id}")
                    }
                })
                .collect();
            format!(
                "⚠ New window appeared: {} — inspect it with screenshot/windows",
                items.join(", ")
            )
        });

    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        let msg = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown error")
            .to_string();
        return Ok(json!({
            "content": [{"type": "text", "text": msg}],
            "isError": true
        }));
    }

    // Any reply carrying a png becomes a text summary plus an image block
    if let Some(png) = v.get("png").and_then(|p| p.as_str()) {
        let mut content = vec![json!({"type": "text", "text": describe(name, &v)})];
        content.push(json!({"type": "image", "data": png, "mimeType": "image/png"}));
        if let Some(e) = extra {
            content.push(json!({"type": "text", "text": e}));
        }
        return Ok(json!({"content": content, "isError": false}));
    }

    let mut content = vec![json!({
        "type": "text",
        "text": if name == "status" {
            serde_json::to_string_pretty(&v).unwrap_or_default()
        } else if name == "windows" {
            v.to_string()
        } else {
            describe(name, &v)
        }
    })];
    if let Some(e) = extra {
        content.push(json!({"type": "text", "text": e}));
    }
    Ok(json!({"content": content, "isError": false}))
}

fn describe(name: &str, v: &Value) -> String {
    let id = v.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
    let w = v.get("w").and_then(|x| x.as_i64()).unwrap_or(0);
    let h = v.get("h").and_then(|x| x.as_i64()).unwrap_or(0);
    let rev = v.get("revision").and_then(|x| x.as_i64()).unwrap_or(0);
    if name == "screenshot" {
        return format!("window {id}, {w}x{h}, revision {rev}, origin at top-left");
    }
    if name == "launch" || name == "wait_for_window" {
        return match v.get("window") {
            Some(Value::Null) | None => "no new window (timed_out)".to_string(),
            Some(w) => format!(
                "new window #{} \"{}\" {}x{}",
                w.get("id").and_then(|x| x.as_i64()).unwrap_or(0),
                w.get("title").and_then(|x| x.as_str()).unwrap_or(""),
                w.get("w").and_then(|x| x.as_i64()).unwrap_or(0),
                w.get("h").and_then(|x| x.as_i64()).unwrap_or(0)
            ),
        };
    }
    if v.get("changed").is_none() && v.get("settled").is_none() {
        return "ok".to_string();
    }
    let changed = v.get("changed").and_then(|x| x.as_bool()).unwrap_or(false);
    let settled = v.get("settled").and_then(|x| x.as_bool()).unwrap_or(false);
    let timed = v.get("timed_out").and_then(|x| x.as_bool()).unwrap_or(false);
    let rects = v
        .get("rects")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    format!(
        "window {id}, {w}x{h}, revision {rev}, changed={changed} settled={settled} timed_out={timed} rects=[{rects}]"
    )
}

fn num(args: &Value, key: &str) -> f64 {
    args.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0)
}

/// Send one control command into the compositor event loop and wait for the reply.
fn rpc_call(cmd: &str, tx: &cl_channel::Sender<Msg>) -> Result<String, String> {
    let (rtx, rrx) = std_mpsc::channel();
    tx.send(Msg::Cmd(cmd.to_string(), rtx))
        .map_err(|e| format!("compositor gone: {e}"))?;
    rrx.recv_timeout(Duration::from_secs(30))
        .map_err(|_| "compositor timeout".to_string())
}

const PANEL: &str = include_str!("panel.html");
