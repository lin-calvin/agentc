//! `agentc mcp` — a stdio <-> HTTP MCP relay.
//!
//! Why this layer exists:
//! - MCP clients (Claude Code, opencode, ...) usually only spawn a process and talk
//!   over stdio; not every client supports connecting to HTTP directly.
//! - The relay knows nothing about tool definitions. It just moves JSON-RPC across,
//!   so the tool surface has exactly one implementation (inside the daemon).
//! - It also wakes the daemon: if none is running it starts one detached and waits
//!   until it is healthy.
//!
//! The daemon runs in its own session (setsid), so when the MCP client exits and the
//! agent session ends, the desktop and its applications are still there for next time.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

pub fn runtime_dir() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
}

pub fn url_file() -> PathBuf {
    runtime_dir().join("agentc.url")
}

pub fn log_file() -> PathBuf {
    runtime_dir().join("agentc.log")
}

pub fn read_url() -> Option<String> {
    std::fs::read_to_string(url_file())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ------------------------------------------------------------------ minimal HTTP client

fn http_post(url: &str, body: &str, accept: &str) -> std::io::Result<String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "need http://"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let mut stream = TcpStream::connect(authority)?;
    stream.set_read_timeout(Some(Duration::from_secs(40)))?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nAccept: {accept}\r\nX-Agentc-Actor: agent\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes())?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).to_string();
    Ok(text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default())
}

fn healthy(url: &str) -> bool {
    let rest = match url.strip_prefix("http://") {
        Some(r) => r,
        None => return false,
    };
    let (authority, _) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let Ok(mut s) = TcpStream::connect(authority) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(600)));
    let req = format!("GET /health HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    buf.contains("200") && buf.trim_end().ends_with("ok")
}

// ------------------------------------------------------------------ waking the daemon

fn spawn_daemon() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let log = log_file();
    let logf = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    // setsid detaches the daemon from the MCP client's process group so it survives
    std::process::Command::new("setsid")
        .arg("-f")
        .arg(&exe)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(logf.try_clone()?)
        .stderr(logf)
        .spawn()?;
    Ok(())
}

pub fn ensure_daemon(explicit: Option<String>) -> String {
    if let Some(url) = explicit {
        if healthy(&url) {
            return url;
        }
        // An explicit URL was given but nothing answers there: still wake one, using the default port
        let _ = spawn_daemon();
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(120));
            if healthy(&url) {
                return url;
            }
            if let Some(u) = read_url() {
                if healthy(&u) {
                    return u;
                }
            }
        }
        eprintln!("[agentc mcp] daemon did not come up, see {}", log_file().display());
        std::process::exit(1);
    }

    if let Some(url) = read_url() {
        if healthy(&url) {
            return url;
        }
    }

    eprintln!("[agentc mcp] no daemon running, starting one...");
    if let Err(e) = spawn_daemon() {
        eprintln!("[agentc mcp] failed to start: {e}");
        std::process::exit(1);
    }

    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(120));
        if let Some(url) = read_url() {
            if healthy(&url) {
                eprintln!("[agentc mcp] daemon ready at {url}");
                return url;
            }
        }
    }
    eprintln!(
        "[agentc mcp] timed out waiting for the daemon, see {}",
        log_file().display()
    );
    std::process::exit(1);
}

// ------------------------------------------------------------------ stdio main loop

pub fn run_stdio(explicit_url: Option<String>) -> ! {
    use std::sync::{Arc, Mutex};

    let base = ensure_daemon(explicit_url);
    let endpoint = format!("{}/mcp", base.trim_end_matches('/'));
    let out = Arc::new(Mutex::new(std::io::stdout()));

    // ---- proactive push: poll window events and emit MCP notifications
    //
    // Tool replies carry them too, but that only reaches the model on its next call.
    // Dialogs often pop up on their own (crash recovery, update prompts) while the model
    // is not calling anything, so there has to be a channel independent of request/response.
    {
        let out = out.clone();
        std::thread::spawn(move || {
            // Align the cursor at startup so we do not replay history
            let mut since = 0u64;
            if let Ok(r) = post_cmd("", "{\"cmd\":\"events\",\"since\":0}") {
                if let Some(v) = serde_json::from_str::<serde_json::Value>(&r).ok() {
                    since = v.get("seq").and_then(|x| x.as_u64()).unwrap_or(0);
                }
            }
            loop {
                std::thread::sleep(Duration::from_millis(250));
                let cmd = format!("{{\"cmd\":\"events\",\"since\":{since}}}");
                let Ok(raw) = post_cmd("", &cmd) else { continue };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { continue };
                since = v.get("seq").and_then(|x| x.as_u64()).unwrap_or(since);
                let Some(events) = v.get("events").and_then(|x| x.as_array()) else { continue };
                for e in events {
                    let w = e.get("window").cloned().unwrap_or_default();
                    let id = w.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
                    let dialog = w.get("dialog").and_then(|x| x.as_bool()).unwrap_or(false);
                    let msg = if dialog {
                        format!("new dialog window #{id} appeared; inspect it now with screenshot({{window:{id}}})")
                    } else {
                        format!("new window #{id} appeared; inspect it with screenshot({{window:{id}}})")
                    };
                    let note = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/message",
                        "params": {"level": "warning", "logger": "agentc", "data": msg}
                    });
                    if let Ok(mut o) = out.lock() {
                        let _ = writeln!(o, "{note}");
                        let _ = o.flush();
                    }
                }
            }
        });
    }

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match http_post(&endpoint, &line, "application/json") {
            Ok(resp) if !resp.trim().is_empty() => {
                let resp = unwrap_sse(&resp);
                let mut o = match out.lock() {
                    Ok(o) => o,
                    Err(_) => break,
                };
                if o.write_all(resp.as_bytes()).is_err() {
                    break;
                }
                if o.write_all(b"\n").is_err() || o.flush().is_err() {
                    break;
                }
            }
            // notifications: upstream replies 202 with an empty body, which is normal
            Ok(_) => {}
            Err(e) => {
                let id = extract_id(&line);
                let msg = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32000, "message": format!("agentc daemon unreachable: {e}")}
                });
                if let Ok(mut o) = out.lock() {
                    let _ = writeln!(o, "{msg}");
                    let _ = o.flush();
                }
            }
        }
    }
    std::process::exit(0)
}

/// Internal commands go over the daemon's control socket (same implementation as /mcp,
/// but without the JSON-RPC wrapping).
fn post_cmd(_endpoint: &str, cmd: &str) -> std::io::Result<String> {
    let sock = runtime_dir().join("agentc.sock");
    let mut s = std::os::unix::net::UnixStream::connect(sock)?;
    s.write_all(format!("{cmd}\n").as_bytes())?;
    let mut buf = String::new();
    std::io::BufReader::new(s).read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

/// In case upstream answered with a single SSE event, strip the `data:` line.
fn unwrap_sse(body: &str) -> String {
    let t = body.trim();
    if t.starts_with('{') {
        return t.to_string();
    }
    for line in t.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            return rest.trim().to_string();
        }
    }
    t.to_string()
}

fn extract_id(line: &str) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("id").cloned())
        .unwrap_or(serde_json::Value::Null)
}

/// Keeps the trait imports used by the loops above from being reported as unused.
#[allow(dead_code)]
fn _assert_read<R: Read>(_: R) {}
#[allow(dead_code)]
fn _assert_reader<R: BufRead>(_: R) {}
#[allow(dead_code)]
fn _touch(b: BufReader<TcpStream>) {
    drop(b);
}
