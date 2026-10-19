//! Find agent sessions: live processes (from /proc) and recent transcripts.

use crate::ipc;
use crate::model::{Agent, Session, State};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// CLIs without a transcript trax can read; they are listed (and can be
/// driven when started with `trax run`).
const OTHERS: &[&str] = &[
    "gemini", "aider", "opencode", "goose", "amp", "cursor-agent", "qwen", "crush", "copilot", "kimi", "droid",
];

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}
fn claude_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home().join(".claude"))
}
fn codex_dir() -> PathBuf {
    std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".codex"))
}

struct Proc {
    pid: u32,
    ppid: u32,
    agent: Agent,
    label: String,
    cwd: PathBuf,
    tty: Option<String>,
    start: Option<SystemTime>,
}

#[derive(Clone, Default)]
struct FileMeta {
    cwd: Option<String>,
    id: Option<String>,
    title: Option<String>,
}

#[derive(Default)]
pub struct Scanner {
    meta: HashMap<PathBuf, (SystemTime, FileMeta)>,
}

fn stat(pid: u32) -> Option<(u32, u64)> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some((f.get(1)?.parse().ok()?, f.get(19)?.parse().ok()?))
}

fn boot_time() -> Option<u64> {
    fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("btime ").and_then(|x| x.trim().parse().ok()))
}

fn base(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

fn procs() -> Vec<Proc> {
    let btime = boot_time();
    let me = std::process::id();
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        if pid == me {
            continue;
        }
        let dir = e.path();
        let comm = fs::read_to_string(dir.join("comm")).unwrap_or_default();
        let comm = comm.trim();
        let (agent, label) = match comm {
            "claude" => (Agent::Claude, "claude".to_string()),
            "codex" => (Agent::Codex, "codex".to_string()),
            _ => {
                let cmd = fs::read(dir.join("cmdline")).unwrap_or_default();
                let args: Vec<String> = cmd
                    .split(|&b| b == 0)
                    .take(2)
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect();
                match args.iter().map(|a| base(a)).find(|a| OTHERS.contains(a)) {
                    Some(name) => (Agent::Other, name.to_string()),
                    None => continue,
                }
            }
        };
        // Newer Codex keeps a background `codex app-server` daemon; it is not a session.
        if agent == Agent::Codex && fs::read(dir.join("cmdline")).is_ok_and(|c| c.split(|&b| b == 0).any(|a| a == b"app-server")) {
            continue;
        }
        let Ok(cwd) = fs::read_link(dir.join("cwd")) else { continue };
        let (ppid, ticks) = stat(pid).unwrap_or((0, 0));
        let start = btime.map(|b| UNIX_EPOCH + Duration::from_secs(b) + Duration::from_millis(ticks * 10));
        let tty = fs::read_link(dir.join("fd/0"))
            .ok()
            .and_then(|p| p.to_str().and_then(|s| s.strip_prefix("/dev/").map(String::from)));
        out.push(Proc { pid, ppid, agent, label, cwd, tty, start });
    }
    // A wrapper process (e.g. node running codex) and its child are one session.
    let ids: HashMap<u32, Agent> = out.iter().map(|p| (p.pid, p.agent)).collect();
    out.retain(|p| ids.get(&p.ppid) != Some(&p.agent));
    out
}

fn ancestors(pid: u32, depth: usize) -> Vec<u32> {
    let mut v = Vec::new();
    let mut cur = pid;
    for _ in 0..depth {
        match stat(cur) {
            Some((pp, _)) if pp > 1 => {
                v.push(pp);
                cur = pp;
            }
            _ => break,
        }
    }
    v
}

fn read_head_lines(path: &Path, max_bytes: u64) -> Vec<String> {
    let Ok(f) = fs::File::open(path) else { return vec![] };
    let mut r = BufReader::new(f.take(max_bytes));
    let mut lines = Vec::new();
    let mut s = String::new();
    while r.read_line(&mut s).unwrap_or(0) > 0 {
        if s.ends_with('\n') {
            lines.push(std::mem::take(&mut s));
        } else {
            break;
        }
    }
    lines
}

fn read_tail_lines(path: &Path, max_bytes: u64) -> Vec<String> {
    let Ok(mut f) = fs::File::open(path) else { return vec![] };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return vec![];
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    lines
}

fn short(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= n { s } else { s.chars().take(n).collect::<String>() + "…" }
}

fn claude_meta(path: &Path) -> FileMeta {
    let mut m = FileMeta::default();
    for l in read_head_lines(path, 256 << 10) {
        if let Ok(v) = serde_json::from_str::<Value>(&l) {
            if m.cwd.is_none() {
                m.cwd = v["cwd"].as_str().map(String::from);
            }
            if m.id.is_none() {
                m.id = v["sessionId"].as_str().map(String::from);
            }
            if m.title.is_none() && v["type"].as_str() == Some("ai-title") {
                m.title = v["aiTitle"].as_str().map(String::from);
            }
        }
    }
    let mut prompt = None;
    for l in read_tail_lines(path, 256 << 10).iter().rev() {
        if !l.contains("\"ai-title\"") && !l.contains("\"last-prompt\"") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(l) {
            match v["type"].as_str() {
                Some("ai-title") => {
                    m.title = v["aiTitle"].as_str().map(String::from);
                    break;
                }
                Some("last-prompt") if prompt.is_none() => prompt = v["lastPrompt"].as_str().map(|s| short(s, 80)),
                _ => {}
            }
        }
    }
    if m.title.is_none() {
        m.title = prompt;
    }
    m
}

fn codex_meta(path: &Path, index: &HashMap<String, String>) -> FileMeta {
    let mut m = FileMeta::default();
    if let Some(l) = read_head_lines(path, 4 << 20).first()
        && let Ok(v) = serde_json::from_str::<Value>(l) {
            let p = if v["type"].as_str() == Some("session_meta") { &v["payload"] } else { &v };
            m.cwd = p["cwd"].as_str().map(String::from);
            m.id = p["id"].as_str().map(String::from);
        }
    m.title = m.id.as_ref().and_then(|id| index.get(id).cloned());
    if m.title.is_none() {
        for l in read_tail_lines(path, 256 << 10).iter().rev() {
            if !l.contains("\"role\":\"user\"") {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(l) {
                let t = v["payload"]["content"][0]["text"].as_str().unwrap_or("");
                if !t.is_empty() && !t.starts_with('<') && !t.starts_with("# AGENTS.md") {
                    m.title = Some(short(t, 80));
                    break;
                }
            }
        }
    }
    m
}

fn codex_index() -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(s) = fs::read_to_string(codex_dir().join("session_index.jsonl")) {
        for l in s.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(l)
                && let (Some(id), Some(n)) = (v["id"].as_str(), v["thread_name"].as_str()) {
                    map.insert(id.to_string(), n.to_string());
                }
        }
    }
    map
}

fn mtime(p: &Path) -> Option<SystemTime> {
    fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Codex rollout files from the last `days` day folders, newest first.
fn codex_rollouts(days: i64) -> Vec<PathBuf> {
    let mut v = Vec::new();
    let today = chrono::Local::now().date_naive();
    for d in 0..=days {
        let day = today - chrono::Duration::days(d);
        let dir = codex_dir().join("sessions").join(day.format("%Y/%m/%d").to_string());
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                    v.push(p);
                }
            }
        }
    }
    v.sort_by_key(|p| std::cmp::Reverse(mtime(p)));
    v
}

fn claude_project_dir(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

fn find_claude_log(sid: &str, cwd: &str) -> Option<PathBuf> {
    let projects = claude_dir().join("projects");
    let guess = projects.join(claude_project_dir(cwd)).join(format!("{sid}.jsonl"));
    if guess.exists() {
        return Some(guess);
    }
    fs::read_dir(&projects)
        .ok()?
        .flatten()
        .map(|e| e.path().join(format!("{sid}.jsonl")))
        .find(|p| p.exists())
}

impl Scanner {
    fn meta(&mut self, path: &Path, agent: Agent, index: &HashMap<String, String>) -> FileMeta {
        let mt = mtime(path).unwrap_or(UNIX_EPOCH);
        if let Some((t, m)) = self.meta.get(path)
            && *t == mt {
                return m.clone();
            }
        let m = match agent {
            Agent::Claude => claude_meta(path),
            Agent::Codex => codex_meta(path, index),
            Agent::Other => FileMeta::default(),
        };
        self.meta.insert(path.to_path_buf(), (mt, m.clone()));
        m
    }

    /// Live sessions, plus (when `hours > 0`) transcripts touched in the last `hours`.
    pub fn scan(&mut self, hours: u64) -> Vec<Session> {
        let index = codex_index();
        let wrapped = ipc::list();
        let by_wrapper: HashMap<u32, &ipc::Wrapped> = wrapped.iter().map(|w| (w.wrapper, w)).collect();
        let mut used_logs: HashSet<PathBuf> = HashSet::new();
        let mut used_wrappers: HashSet<u32> = HashSet::new();
        let mut out = Vec::new();
        let rollouts = codex_rollouts(2);

        for p in procs() {
            let anc = ancestors(p.pid, 4);
            let w = anc.iter().find_map(|a| by_wrapper.get(a)).copied();
            if let Some(w) = w {
                used_wrappers.insert(w.wrapper);
            }
            let mut s = Session {
                agent: p.agent,
                label: p.label.clone(),
                pid: Some(p.pid),
                tty: p.tty.clone(),
                id: None,
                name: p.cwd.file_name().map(|x| x.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into()),
                title: None,
                cwd: p.cwd.clone(),
                state: State::Live,
                log: None,
                modified: None,
                started: p.start,
                sock: w.map(|w| w.sock.clone()),
                hosted: w.is_some_and(|w| w.headless),
            };
            match p.agent {
                Agent::Claude => {
                    let f = claude_dir().join("sessions").join(format!("{}.json", p.pid));
                    if let Some(v) = fs::read_to_string(f).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
                        if let Some(n) = v["name"].as_str() {
                            s.name = n.to_string();
                        }
                        s.state = match v["status"].as_str() {
                            Some("busy") => State::Busy,
                            Some("idle") => State::Idle,
                            _ => State::Live,
                        };
                        if let Some(sid) = v["sessionId"].as_str() {
                            s.id = Some(sid.to_string());
                            s.log = find_claude_log(sid, v["cwd"].as_str().unwrap_or(""));
                        }
                    }
                }
                Agent::Codex => {
                    // The rollout file the process holds open, else the newest one for this folder.
                    let held = fs::read_dir(format!("/proc/{}/fd", p.pid)).ok().and_then(|rd| {
                        rd.flatten().filter_map(|e| fs::read_link(e.path()).ok()).find(|t| {
                            t.file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
                        })
                    });
                    s.log = held.or_else(|| {
                        let since = p.start.map(|t| t - Duration::from_secs(5)).unwrap_or(UNIX_EPOCH);
                        rollouts
                            .iter()
                            .filter(|r| !used_logs.contains(*r) && mtime(r).is_some_and(|m| m >= since))
                            .find(|r| self.meta(r, Agent::Codex, &index).cwd.as_deref() == p.cwd.to_str())
                            .cloned()
                    });
                }
                Agent::Other => {}
            }
            if let Some(log) = &s.log {
                used_logs.insert(log.clone());
                let m = self.meta(log, p.agent, &index);
                s.title = m.title;
                if s.id.is_none() {
                    s.id = m.id;
                }
                s.modified = mtime(log);
            }
            out.push(s);
        }

        // Wrapped commands trax does not otherwise recognise (any CLI works).
        for w in &wrapped {
            if used_wrappers.contains(&w.wrapper) {
                continue;
            }
            let label = w.cmd.first().map(|c| base(c).to_string()).unwrap_or_else(|| "cmd".into());
            out.push(Session {
                agent: Agent::Other,
                label,
                pid: w.child.or(Some(w.wrapper)),
                tty: None,
                id: None,
                name: w.cwd.file_name().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default(),
                title: Some(w.cmd.join(" ")),
                cwd: w.cwd.clone(),
                state: State::Live,
                log: None,
                modified: None,
                started: w.started.map(|s| UNIX_EPOCH + Duration::from_secs(s)),
                sock: Some(w.sock.clone()),
                hosted: w.headless,
            });
        }

        // Stable order for live sessions so tiles do not jump around.
        out.sort_by_key(|s| (s.started, s.pid));

        if hours > 0 {
            let cutoff = SystemTime::now() - Duration::from_secs(hours * 3600);
            let mut ended = Vec::new();
            if let Ok(rd) = fs::read_dir(claude_dir().join("projects")) {
                for d in rd.flatten() {
                    let Ok(files) = fs::read_dir(d.path()) else { continue };
                    for f in files.flatten() {
                        let p = f.path();
                        let name = f.file_name().to_string_lossy().into_owned();
                        if !name.ends_with(".jsonl") || name.starts_with("agent-") || used_logs.contains(&p) {
                            continue;
                        }
                        let Ok(md) = f.metadata() else { continue };
                        let Ok(mt) = md.modified() else { continue };
                        if mt < cutoff || md.len() < 2000 {
                            continue;
                        }
                        ended.push((Agent::Claude, p, mt));
                    }
                }
            }
            for p in codex_rollouts(hours.div_ceil(24) as i64) {
                if used_logs.contains(&p) {
                    continue;
                }
                if let Some(mt) = mtime(&p)
                    && mt >= cutoff {
                        ended.push((Agent::Codex, p, mt));
                    }
            }
            ended.sort_by_key(|e| std::cmp::Reverse(e.2));
            for (agent, p, mt) in ended {
                let m = self.meta(&p, agent, &index);
                let cwd = PathBuf::from(m.cwd.clone().unwrap_or_default());
                out.push(Session {
                    agent,
                    label: if agent == Agent::Claude { "claude".into() } else { "codex".into() },
                    pid: None,
                    tty: None,
                    id: m.id,
                    name: cwd.file_name().map(|x| x.to_string_lossy().into_owned()).unwrap_or_else(|| "?".into()),
                    title: m.title,
                    cwd,
                    state: State::Ended,
                    log: Some(p),
                    modified: Some(mt),
                    started: None,
                    sock: None,
                    hosted: false,
                });
            }
        }
        // TRAX_ROOT limits trax to sessions under one folder (demos, tests).
        if let Some(root) = std::env::var_os("TRAX_ROOT") {
            let root = PathBuf::from(root);
            out.retain(|s| s.cwd.starts_with(&root));
        }
        out
    }
}
