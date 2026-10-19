//! Sessions hosted by trax itself: start new ones, and take over sessions
//! that were started in an ordinary terminal window.
//!
//! Taking over closes the agent in its original window (SIGTERM, which both
//! Claude Code and Codex treat as a normal exit; the conversation is already
//! on disk) and resumes the same conversation in a background session that
//! trax owns. From then on trax can read, type, attach and interrupt.

use crate::ipc;
use crate::model::Session;
use anyhow::{Result, anyhow, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Make `s` drivable. Returns the control socket of the hosted session.
pub fn take(s: &Session) -> Result<PathBuf> {
    if let Some(sock) = &s.sock {
        return Ok(sock.clone());
    }
    let mut args = s
        .resume_args()
        .ok_or_else(|| anyhow!("trax can only take over claude and codex conversations"))?;
    // Nothing said yet means nothing to resume: start it fresh instead.
    let empty = s.log.as_ref().is_none_or(|p| std::fs::metadata(p).map(|m| m.len() == 0).unwrap_or(true));
    if empty {
        args.truncate(1);
    }
    if let Some(pid) = s.pid {
        close(pid)?;
    }
    ipc::spawn(&args, &s.cwd)
}

/// Start a new hosted session.
pub fn new(cmd: &[String], cwd: &Path) -> Result<PathBuf> {
    ipc::spawn(cmd, cwd)
}

fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // Field 3 is the state; Z means it exited and waits for its parent.
        Ok(st) => st.rsplit(')').next().and_then(|r| r.split_whitespace().next()) != Some("Z"),
        Err(_) => false,
    }
}

fn close(pid: u32) -> Result<()> {
    if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
        bail!("could not close pid {pid}: {}", std::io::Error::last_os_error());
    }
    for _ in 0..160 {
        if !alive(pid) {
            // Give the agent a moment to release its files.
            std::thread::sleep(Duration::from_millis(200));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("pid {pid} is still running; close that window yourself, then take it over again")
}
