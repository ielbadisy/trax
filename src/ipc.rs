//! Control sockets of sessions started with `trax run`.
//!
//! Each wrapper writes `<dir>/<wrapper-pid>.json` and listens on
//! `<dir>/<wrapper-pid>.sock`. Requests and replies are one JSON line each.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("TRAX_RUNTIME_DIR") {
        return PathBuf::from(d);
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) => PathBuf::from(d).join("trax"),
        None => {
            let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
            std::env::temp_dir().join(format!("trax-{user}"))
        }
    }
}

#[derive(Clone, Debug)]
pub struct Wrapped {
    pub wrapper: u32,
    pub child: Option<u32>,
    pub cmd: Vec<String>,
    pub cwd: PathBuf,
    pub sock: PathBuf,
    pub started: Option<u64>,
    /// Hosted by trax in the background (no terminal window of its own).
    pub headless: bool,
}

/// Live wrappers. Entries whose wrapper process is gone are removed.
pub fn list() -> Vec<Wrapped> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir()) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(v) = std::fs::read_to_string(&p)
            .map_err(anyhow::Error::from)
            .and_then(|s| serde_json::from_str::<Value>(&s).map_err(Into::into))
        else {
            continue;
        };
        let Some(wrapper) = v["wrapper"].as_u64().map(|x| x as u32) else { continue };
        if !Path::new(&format!("/proc/{wrapper}")).exists() {
            let _ = std::fs::remove_file(&p);
            let _ = std::fs::remove_file(p.with_extension("sock"));
            continue;
        }
        out.push(Wrapped {
            wrapper,
            child: v["child"].as_u64().map(|x| x as u32),
            cmd: v["cmd"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            cwd: PathBuf::from(v["cwd"].as_str().unwrap_or("")),
            sock: p.with_extension("sock"),
            started: v["started"].as_u64(),
            headless: v["headless"].as_bool().unwrap_or(false),
        });
    }
    out
}

pub fn request(sock: &Path, req: &Value) -> Result<Value> {
    let mut s = UnixStream::connect(sock).with_context(|| format!("connect {}", sock.display()))?;
    s.set_read_timeout(Some(Duration::from_millis(1500)))?;
    s.set_write_timeout(Some(Duration::from_millis(1500)))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    s.write_all(line.as_bytes())?;
    let mut reply = String::new();
    BufReader::new(s).read_line(&mut reply)?;
    let v: Value = serde_json::from_str(&reply).context("bad reply from session")?;
    if v["ok"].as_bool() != Some(true) {
        bail!("{}", v["error"].as_str().unwrap_or("session refused the request"));
    }
    Ok(v)
}

/// Type `text` into the session; `submit` presses Enter afterwards.
pub fn send(sock: &Path, text: &str, submit: bool) -> Result<()> {
    request(sock, &json!({"op": "send", "text": text, "submit": submit})).map(|_| ())
}

/// Forward raw key bytes (escape sequences included).
pub fn keys(sock: &Path, data: &str) -> Result<()> {
    request(sock, &json!({"op": "keys", "data": data})).map(|_| ())
}

/// Resize a background session's screen (used when attaching to it).
pub fn resize(sock: &Path, rows: u16, cols: u16) -> Result<()> {
    request(sock, &json!({"op": "resize", "rows": rows, "cols": cols})).map(|_| ())
}

/// Start `cmd` in `cwd` as a background session hosted by trax.
/// Returns its control socket once it is up.
pub fn spawn(cmd: &[String], cwd: &Path) -> Result<PathBuf> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    if cmd.is_empty() {
        bail!("nothing to start");
    }
    if !cwd.is_dir() {
        bail!("folder {} does not exist", cwd.display());
    }
    let exe = std::env::current_exe().context("find the trax executable")?;
    let mut c = Command::new(exe);
    c.arg("run").arg("--headless").arg("--").args(cmd).current_dir(cwd);
    // Markers a parent agent sets for its own children. A hosted session is a
    // top-level session, so it must not inherit them (Claude Code turns off
    // transcript saving for child sessions, for example).
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy().into_owned();
        let child_marker = matches!(k.as_str(), "CLAUDECODE" | "CLAUDE_PID" | "CLAUDE_EFFORT" | "TRAX_WRAPPED")
            || (k.starts_with("CLAUDE_CODE_")
                && ["CHILD_SESSION", "SESSION_", "MESSAGING_", "ENTRYPOINT", "EXECPATH", "PLUGIN_DIRS", "SSE_PORT"]
                    .iter()
                    .any(|m| k["CLAUDE_CODE_".len()..].starts_with(m)))
            || k.starts_with("CODEX_THREAD")
            || k.starts_with("CODEX_SANDBOX");
        if child_marker {
            c.env_remove(&k);
        }
    }
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // Own session: survives the dashboard and the terminal that started it.
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = c.spawn().context("start background session")?;
    let sock = dir().join(format!("{}.sock", child.id()));
    for _ in 0..60 {
        if sock.exists() {
            return Ok(sock);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("background session did not come up")
}

pub struct ScreenDump {
    pub rows: u16,
    pub cols: u16,
    pub data: String,
    pub cursor: (u16, u16),
}

pub fn screen(sock: &Path) -> Result<ScreenDump> {
    let v = request(sock, &json!({"op": "screen"}))?;
    Ok(ScreenDump {
        rows: v["rows"].as_u64().unwrap_or(24) as u16,
        cols: v["cols"].as_u64().unwrap_or(80) as u16,
        data: v["data"].as_str().unwrap_or("").to_string(),
        cursor: (
            v["cursor_row"].as_u64().unwrap_or(0) as u16,
            v["cursor_col"].as_u64().unwrap_or(0) as u16,
        ),
    })
}
