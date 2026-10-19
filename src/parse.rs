//! Incremental readers for agent transcripts (JSONL files written by the agents).

use crate::model::{Agent, Msg, Role};
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

const MAX_MSGS: usize = 50_000;
const MAX_TEXT: usize = 20_000;
/// On first open, only the end of very large transcripts is read.
const FIRST_READ: u64 = 256 << 20;

pub struct Tail {
    pub path: PathBuf,
    pub agent: Agent,
    offset: u64,
    buf: Vec<u8>,
    pub msgs: Vec<Msg>,
    /// Count of messages ever pushed (survives trimming), for followers.
    pub total: u64,
    /// Bumped whenever `msgs` changes.
    pub version: u64,
    pub title: Option<String>,
    /// Codex only: a turn is in progress.
    pub busy: Option<bool>,
}

impl Tail {
    pub fn new(path: PathBuf, agent: Agent) -> Self {
        Tail { path, agent, offset: 0, buf: Vec::new(), msgs: Vec::new(), total: 0, version: 0, title: None, busy: None }
    }

    /// Read whatever was appended since last time. Returns true on change.
    pub fn poll(&mut self) -> bool {
        let Ok(meta) = std::fs::metadata(&self.path) else { return false };
        let len = meta.len();
        if len < self.offset {
            // Truncated or replaced: start over.
            self.offset = 0;
            self.buf.clear();
            self.msgs.clear();
        }
        if len == self.offset {
            return false;
        }
        let Ok(mut f) = File::open(&self.path) else { return false };
        let mut skip_partial = false;
        if self.offset == 0 && len > FIRST_READ {
            self.offset = len - FIRST_READ;
            skip_partial = true;
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }
        let mut chunk = Vec::new();
        let Ok(n) = f.take(len - self.offset).read_to_end(&mut chunk) else { return false };
        self.offset += n as u64;
        if skip_partial
            && let Some(i) = chunk.iter().position(|&b| b == b'\n') {
                chunk.drain(..=i);
            }
        self.buf.extend_from_slice(&chunk);
        let Some(last_nl) = self.buf.iter().rposition(|&b| b == b'\n') else { return false };
        let rest = self.buf.split_off(last_nl + 1);
        let complete = std::mem::replace(&mut self.buf, rest);
        let before = self.total;
        for line in complete.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_slice::<Value>(line) {
                match self.agent {
                    Agent::Claude => self.claude(&v),
                    Agent::Codex => self.codex(&v),
                    Agent::Other => {}
                }
            }
        }
        if self.msgs.len() > MAX_MSGS {
            let cut = self.msgs.len() - MAX_MSGS;
            self.msgs.drain(..cut);
        }
        let changed = self.total != before;
        if changed {
            self.version += 1;
        }
        changed || n > 0
    }

    fn push(&mut self, role: Role, name: Option<String>, text: String, time: Option<String>) {
        let mut text = sanitize(&text);
        if text.trim().is_empty() && role != Role::Tool {
            return;
        }
        if text.len() > MAX_TEXT {
            let mut cut = MAX_TEXT;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str(" …");
        }
        self.msgs.push(Msg { role, name, text: text.trim_end().to_string(), time });
        self.total += 1;
    }

    // ---- Claude Code: ~/.claude/projects/<dir>/<session>.jsonl ----

    fn claude(&mut self, v: &Value) {
        let time = clock(&v["timestamp"]);
        match v["type"].as_str().unwrap_or("") {
            "ai-title" => {
                if let Some(t) = v["aiTitle"].as_str() {
                    self.title = Some(t.to_string());
                }
            }
            "user" => {
                if v["isSidechain"].as_bool() == Some(true) || v["isMeta"].as_bool() == Some(true) {
                    return;
                }
                if v["isCompactSummary"].as_bool() == Some(true) {
                    self.push(Role::Info, None, "conversation compacted".into(), time);
                    return;
                }
                match &v["message"]["content"] {
                    Value::String(s) => self.claude_user_text(s, time),
                    Value::Array(items) => {
                        for it in items {
                            match it["type"].as_str().unwrap_or("") {
                                "text" => self.claude_user_text(it["text"].as_str().unwrap_or(""), time.clone()),
                                "image" => self.push(Role::User, None, "[image]".into(), time.clone()),
                                "tool_result" => {
                                    let mut t = flatten(&it["content"]);
                                    if it["is_error"].as_bool() == Some(true) {
                                        t = format!("error: {t}");
                                    }
                                    self.push(Role::Output, None, t, time.clone());
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => {
                if v["isSidechain"].as_bool() == Some(true) {
                    return;
                }
                let Some(items) = v["message"]["content"].as_array() else { return };
                for it in items {
                    match it["type"].as_str().unwrap_or("") {
                        "text" => self.push(Role::Assistant, None, it["text"].as_str().unwrap_or("").into(), time.clone()),
                        "tool_use" => {
                            let name = it["name"].as_str().unwrap_or("tool").to_string();
                            self.push(Role::Tool, Some(name), summarize(&it["input"]), time.clone());
                        }
                        _ => {}
                    }
                }
            }
            "system" => {
                if v["subtype"].as_str() == Some("compact_boundary") {
                    self.push(Role::Info, None, "context compacted".into(), time);
                } else if matches!(v["level"].as_str(), Some("error") | Some("warning")) {
                    self.push(Role::Info, None, v["content"].as_str().unwrap_or("").into(), time);
                }
            }
            _ => {}
        }
    }

    fn claude_user_text(&mut self, s: &str, time: Option<String>) {
        let t = s.trim_start();
        if t.starts_with("<command-name>") || t.starts_with("<command-message>") {
            let name = tag(t, "command-name").unwrap_or_default();
            let args = tag(t, "command-args").unwrap_or_default();
            self.push(Role::User, None, format!("{name} {args}").trim().to_string(), time);
        } else if t.starts_with("<local-command-stdout>") {
            let out = tag(t, "local-command-stdout").unwrap_or_default();
            self.push(Role::Output, None, out, time);
        } else if t.starts_with("<local-command-caveat>") || t.starts_with("<system-reminder>") {
        } else {
            self.push(Role::User, None, s.to_string(), time);
        }
    }

    // ---- Codex: ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl ----

    fn codex(&mut self, v: &Value) {
        let time = clock(&v["timestamp"]);
        let (kind, p) = match v["type"].as_str().unwrap_or("") {
            "response_item" => ("item", &v["payload"]),
            "event_msg" => ("event", &v["payload"]),
            // Old rollouts wrote items at top level.
            "message" | "function_call" | "function_call_output" | "custom_tool_call"
            | "custom_tool_call_output" => ("item", v),
            "compacted" => {
                self.push(Role::Info, None, "context compacted".into(), time);
                return;
            }
            _ => return,
        };
        if kind == "event" {
            match p["type"].as_str().unwrap_or("") {
                "task_started" => self.busy = Some(true),
                "task_complete" => self.busy = Some(false),
                "turn_aborted" => {
                    self.busy = Some(false);
                    self.push(Role::Info, None, "turn interrupted".into(), time);
                }
                _ => {}
            }
            return;
        }
        match p["type"].as_str().unwrap_or("") {
            "message" => {
                let role = p["role"].as_str().unwrap_or("");
                let text = p["content"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|c| c["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                match role {
                    "assistant" => self.push(Role::Assistant, None, text, time),
                    "user" => {
                        let t = text.trim_start();
                        // Injected context (AGENTS.md, environment, ...) is not something the user typed.
                        if !(t.starts_with('<') || t.starts_with("# AGENTS.md")) {
                            self.push(Role::User, None, text, time);
                        }
                    }
                    _ => {}
                }
            }
            "function_call" => {
                let name = p["name"].as_str().unwrap_or("tool").to_string();
                let args = p["arguments"]
                    .as_str()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .unwrap_or(Value::Null);
                self.push(Role::Tool, Some(name), summarize(&args), time);
            }
            "custom_tool_call" => {
                let name = p["name"].as_str().unwrap_or("tool").to_string();
                let input = p["input"].as_str().unwrap_or("");
                let (name, line) = custom_call(name, input);
                self.push(Role::Tool, Some(name), line, time);
            }
            "function_call_output" | "custom_tool_call_output" => {
                let out = match &p["output"] {
                    // Older rollouts store {"output": ..., "metadata": ...} as a JSON string.
                    Value::String(s) => match serde_json::from_str::<Value>(s) {
                        Ok(Value::Object(o)) if o.contains_key("output") => flatten(&o["output"]),
                        _ => s.clone(),
                    },
                    o => flatten(&o["output"]).to_string() + &flatten(&o["content"]),
                };
                // Codex prefixes shell output with a small header; keep the output part.
                let body = out.split_once("\nOutput:\n").map(|(_, b)| b.to_string()).unwrap_or(out);
                self.push(Role::Output, None, body, time);
            }
            "web_search_call" => {
                let q = p["action"]["query"].as_str().unwrap_or("").to_string();
                self.push(Role::Tool, Some("web_search".into()), q, time);
            }
            _ => {}
        }
    }
}

fn tag(s: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let a = s.find(&open)? + open.len();
    let b = s[a..].find(&close).map(|i| a + i).unwrap_or(s.len());
    Some(s[a..b].trim().to_string())
}

fn flatten(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|x| match x {
                Value::String(s) => s.clone(),
                _ => x["text"].as_str().map(String::from).unwrap_or_else(|| {
                    if x["type"].as_str() == Some("image") { "[image]".into() } else { String::new() }
                }),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// One-line description of a tool call from its input object.
pub fn summarize(input: &Value) -> String {
    const KEYS: &[&str] = &[
        "command", "cmd", "file_path", "path", "pattern", "url", "query", "skill", "description", "prompt",
    ];
    for k in KEYS {
        match &input[*k] {
            Value::String(s) if !s.is_empty() => return one_line(s),
            Value::Array(a) if !a.is_empty() => {
                let parts: Vec<&str> = a.iter().filter_map(|x| x.as_str()).collect();
                // ["bash", "-lc", "<script>"] -> the script
                let s = if parts.len() == 3 && parts[1] == "-lc" { parts[2].to_string() } else { parts.join(" ") };
                return one_line(&s);
            }
            _ => {}
        }
    }
    if let Some(obj) = input.as_object() {
        if let Some(s) = obj.values().find_map(|v| v.as_str()) {
            return one_line(s);
        }
        return one_line(&input.to_string());
    }
    String::new()
}

fn one_line(s: &str) -> String {
    let mut lines = s.trim().lines();
    let first = lines.next().unwrap_or("");
    let mut out: String = first.chars().take(240).collect();
    if lines.next().is_some() || first.chars().count() > 240 {
        out.push_str(" …");
    }
    out
}

fn clock(v: &Value) -> Option<String> {
    let s = v.as_str()?;
    let t = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    Some(t.with_timezone(&chrono::Local).format("%H:%M").to_string())
}

/// Drop ANSI escapes and control characters so text renders cleanly.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => match it.next() {
                Some('[') => {
                    while let Some(&n) = it.peek() {
                        it.next();
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC: until BEL or ST
                    while let Some(n) = it.next() {
                        if n == '\x07' || (n == '\x1b' && it.peek() == Some(&'\\')) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// One readable line for a Codex custom tool call. Newer Codex wraps tools in
/// JavaScript (`tools.exec_command({"cmd": ...})`), so dig the command or the
/// patched file out of it instead of showing the code.
fn custom_call(name: String, input: &str) -> (String, String) {
    if let Some(i) = input.find("\"cmd\":") {
        let rest = input[i + 6..].trim_start();
        if let Some(Ok(cmd)) = serde_json::Deserializer::from_str(rest).into_iter::<String>().next() {
            return ("shell".into(), cmd);
        }
    }
    for verb in ["Add File: ", "Update File: ", "Delete File: "] {
        if let Some(i) = input.find(&format!("*** {verb}")) {
            let path = &input[i + 4 + verb.len()..];
            let end = path.find(['\n', '\\', '"']).unwrap_or(path.len());
            let file = path[..end].rsplit('/').next().unwrap_or("");
            return ("patch".into(), format!("{verb}{file}"));
        }
    }
    (name, input.lines().next().unwrap_or("").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    fn read(name: &str, agent: Agent) -> Tail {
        let mut t = Tail::new(fixture(name), agent);
        assert!(t.poll());
        t
    }

    fn show(t: &Tail) -> Vec<(Role, String)> {
        t.msgs
            .iter()
            .map(|m| (m.role, match &m.name {
                Some(n) => format!("{n}: {}", m.text),
                None => m.text.clone(),
            }))
            .collect()
    }

    #[test]
    fn claude_transcript() {
        let t = read("claude.jsonl", Agent::Claude);
        assert_eq!(
            show(&t),
            vec![
                (Role::User, "Fix the broken link in README.md.".into()),
                (Role::Assistant, "I'll look at README.md first.".into()),
                (Role::Tool, "Bash: cat -n README.md …".into()),
                (Role::Output, "6  [Configuration](docs/configuraton.md)".into()),
                (Role::User, "/model opus".into()),
                (Role::Assistant, "Fixed the link on line 6.".into()),
                (Role::Info, "context compacted".into()),
            ]
        );
        assert_eq!(t.title.as_deref(), Some("README.md broken link"));
        assert_eq!(t.busy, None);
    }

    #[test]
    fn codex_wrapped_tool_calls() {
        let t = read("codex.jsonl", Agent::Codex);
        assert_eq!(
            show(&t),
            vec![
                (Role::User, "Add a docstring to clean_rows().".into()),
                (Role::Assistant, "I'll add a short docstring.".into()),
                (Role::Tool, "shell: sed -n '1,40p' clean.py".into()),
                (Role::Output, "def clean_rows(rows):".into()),
                (Role::Tool, "patch: Update File: clean.py".into()),
                (Role::Tool, "patch: Add File: new.md".into()),
                (Role::Assistant, "Added a docstring to clean_rows().".into()),
            ]
        );
        assert_eq!(t.busy, Some(false));
    }

    #[test]
    fn codex_legacy_rollout() {
        let t = read("codex-legacy.jsonl", Agent::Codex);
        assert_eq!(
            show(&t),
            vec![
                (Role::User, "List the files.".into()),
                (Role::Tool, "shell: ls -la".into()),
                (Role::Output, "a.py\nb.py".into()),
                (Role::Assistant, "Two files: a.py and b.py.".into()),
            ]
        );
    }

    #[test]
    fn reads_appended_lines_and_waits_for_partial_ones() {
        let dir = std::env::temp_dir().join(format!("trax-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let line = |text: &str| {
            format!(r#"{{"type":"user","message":{{"role":"user","content":"{text}"}}}}"#)
        };
        std::fs::write(&path, line("one") + "\n").unwrap();
        let mut t = Tail::new(path.clone(), Agent::Claude);
        assert!(t.poll());
        assert_eq!(t.msgs.len(), 1);
        assert!(!t.poll());

        // Half a line is kept until the rest arrives.
        let two = line("two");
        let (a, b) = two.split_at(10);
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(a.as_bytes()).unwrap();
        t.poll();
        assert_eq!(t.msgs.len(), 1);
        f.write_all(format!("{b}\n").as_bytes()).unwrap();
        assert!(t.poll());
        assert_eq!(t.msgs[1].text, "two");

        // A truncated file is read again from the start.
        std::fs::write(&path, line("fresh") + "\n").unwrap();
        assert!(t.poll());
        assert_eq!(show(&t), vec![(Role::User, "fresh".into())]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sanitize_drops_escapes_and_controls() {
        assert_eq!(sanitize("\x1b[1;31mred\x1b[0m\tx\x07"), "red    x");
        assert_eq!(sanitize("\x1b]0;title\x07after"), "after");
    }

    #[test]
    fn summarize_picks_the_useful_field() {
        assert_eq!(summarize(&serde_json::json!({"file_path": "src/a.rs", "content": "x"})), "src/a.rs");
        assert_eq!(summarize(&serde_json::json!({"command": ["bash", "-lc", "make test"]})), "make test");
        let long = "x".repeat(300);
        assert!(summarize(&serde_json::json!({ "command": long })).ends_with(" …"));
    }
}
