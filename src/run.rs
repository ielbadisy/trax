//! `trax run <cmd>`: run an agent CLI inside a pseudo-terminal owned by trax.
//!
//! The wrapper is transparent: your keystrokes go to the agent and its output
//! comes back to your terminal unchanged. On the side it keeps a copy of the
//! screen and listens on a control socket, so a trax dashboard can mirror the
//! window and type into it as if you were sitting in front of it.

use crate::ipc;
use anyhow::{Context, Result, bail};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::num::NonZeroU16;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;
type Screen = Arc<Mutex<vt100::Parser>>;

fn nz(x: u16) -> NonZeroU16 {
    NonZeroU16::new(x.max(1)).unwrap()
}

/// `headless`: no terminal of its own (started by trax in the background);
/// the session is only reachable through the dashboard or `trax attach`.
pub fn run(cmd: Vec<String>, headless: bool) -> Result<i32> {
    if cmd.is_empty() {
        bail!("usage: trax run <command> [args...]   e.g. trax run claude");
    }
    if std::env::var_os("TRAX_WRAPPED").is_some() {
        // Already inside a trax wrapper: just exec the command directly.
        let st = std::process::Command::new(&cmd[0]).args(&cmd[1..]).status()?;
        return Ok(st.code().unwrap_or(1));
    }
    let (cols, rows) = if headless {
        (120, 40)
    } else {
        crossterm::terminal::size().ok().filter(|&(c, r)| c > 0 && r > 0).unwrap_or((120, 40))
    };
    let pair = native_pty_system()
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .context("open pseudo-terminal")?;
    let cwd = std::env::current_dir()?;
    let mut cb = CommandBuilder::new(&cmd[0]);
    cb.args(&cmd[1..]);
    cb.cwd(&cwd);
    cb.env("TRAX_WRAPPED", "1");
    let mut child = pair
        .slave
        .spawn_command(cb)
        .with_context(|| format!("start {}", cmd[0]))?;
    drop(pair.slave);
    let master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));
    let mut reader = master.lock().unwrap().try_clone_reader()?;
    let writer: Writer = Arc::new(Mutex::new(master.lock().unwrap().take_writer()?));
    let screen: Screen = Arc::new(Mutex::new(vt100::Parser::new(nz(rows), nz(cols), 0)));

    // Register the control socket.
    let dir = ipc::dir();
    std::fs::create_dir_all(&dir)?;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    let me = std::process::id();
    let sock = dir.join(format!("{me}.sock"));
    let info = dir.join(format!("{me}.json"));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).context("bind control socket")?;
    let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
    let started = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    std::fs::write(
        &info,
        serde_json::to_string(&json!({
            "wrapper": me,
            "child": child.process_id(),
            "cmd": cmd,
            "cwd": cwd,
            "started": started,
            "headless": headless,
        }))?,
    )?;

    if !headless {
        crossterm::terminal::enable_raw_mode()?;
    }

    // Agent output -> real terminal + screen copy.
    let scr = screen.clone();
    let out = thread::spawn(move || {
        let mut stdout = std::io::stdout();
        let mut buf = [0u8; 16384];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if !headless {
                        let _ = stdout.write_all(&buf[..n]);
                        let _ = stdout.flush();
                    }
                    scr.lock().unwrap().process(&buf[..n]);
                }
            }
        }
    });

    if !headless {
        local_terminal(writer.clone(), master.clone(), screen.clone(), (cols, rows));
    }

    // Control socket.
    let (w, scr, m) = (writer.clone(), screen.clone(), master.clone());
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (w, scr, m) = (w.clone(), scr.clone(), m.clone());
            thread::spawn(move || {
                let _ = serve(stream, &w, &scr, &m);
            });
        }
    });

    let status = child.wait();
    // Let the last output drain, but do not hang if a grandchild keeps the pty open.
    for _ in 0..20 {
        if out.is_finished() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    if !headless {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&info);
    Ok(status.map(|s| s.exit_code() as i32).unwrap_or(1))
}

type Master = Arc<Mutex<Box<dyn MasterPty + Send>>>;

/// Wire the wrapper's own terminal: keyboard in, window resizes followed.
fn local_terminal(writer: Writer, master: Master, screen: Screen, (cols, rows): (u16, u16)) {
    // Real keyboard -> agent.
    let w = writer.clone();
    thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut w = w.lock().unwrap();
                    let _ = w.write_all(&buf[..n]);
                    let _ = w.flush();
                }
            }
        }
    });

    // Follow window resizes.
    let (m, scr) = (master.clone(), screen.clone());
    thread::spawn(move || {
        let mut last = (cols, rows);
        loop {
            thread::sleep(Duration::from_millis(200));
            if let Ok(now) = crossterm::terminal::size()
                && now != last && now.0 > 0 && now.1 > 0 {
                    last = now;
                    let _ = m.lock().unwrap().resize(PtySize {
                        rows: now.1,
                        cols: now.0,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                    scr.lock().unwrap().set_size(nz(now.1), nz(now.0));
                }
        }
    });

}

fn serve(stream: UnixStream, w: &Writer, scr: &Screen, m: &Master) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let reply = match serde_json::from_str::<Value>(&line) {
        Ok(req) => handle(&req, w, scr, m).unwrap_or_else(|e| json!({"ok": false, "error": e.to_string()})),
        Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}),
    };
    let mut s = stream;
    let mut out = serde_json::to_string(&reply)?;
    out.push('\n');
    s.write_all(out.as_bytes())?;
    Ok(())
}

fn type_bytes(w: &Writer, bytes: &[u8]) -> Result<()> {
    let mut w = w.lock().unwrap();
    w.write_all(bytes)?;
    w.flush()?;
    Ok(())
}

fn handle(req: &Value, w: &Writer, scr: &Screen, m: &Master) -> Result<Value> {
    match req["op"].as_str().unwrap_or("") {
        "send" => {
            let text = req["text"].as_str().unwrap_or("");
            if text.contains('\n') {
                // Bracketed paste keeps newlines from submitting early.
                type_bytes(w, format!("\x1b[200~{text}\x1b[201~").as_bytes())?;
            } else {
                type_bytes(w, text.as_bytes())?;
            }
            if req["submit"].as_bool().unwrap_or(true) {
                // A short pause so the TUI does not read Enter as part of a paste.
                thread::sleep(Duration::from_millis(120));
                type_bytes(w, b"\r")?;
            }
            Ok(json!({"ok": true}))
        }
        "keys" => {
            type_bytes(w, req["data"].as_str().unwrap_or("").as_bytes())?;
            Ok(json!({"ok": true}))
        }
        "screen" => {
            let p = scr.lock().unwrap();
            let s = p.screen();
            let (rows, cols) = s.size();
            let (cr, cc) = s.cursor_position();
            Ok(json!({
                "ok": true,
                "rows": rows.get(),
                "cols": cols.get(),
                "cursor_row": cr,
                "cursor_col": cc,
                "data": s.contents_formatted(),
            }))
        }
        "resize" => {
            let rows = req["rows"].as_u64().unwrap_or(40).clamp(5, 500) as u16;
            let cols = req["cols"].as_u64().unwrap_or(120).clamp(20, 1000) as u16;
            m.lock().unwrap().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
            scr.lock().unwrap().set_size(nz(rows), nz(cols));
            Ok(json!({"ok": true}))
        }
        op => bail!("unknown op {op:?}"),
    }
}
