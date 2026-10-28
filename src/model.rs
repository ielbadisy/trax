use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Agent {
    Claude,
    Codex,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// Agent is working on a turn.
    Busy,
    /// Agent is waiting for the user.
    Idle,
    /// Process is alive but trax cannot tell busy from idle.
    Live,
    /// No process any more; only the transcript remains.
    Ended,
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::Busy => "busy",
            State::Idle => "idle",
            State::Live => "live",
            State::Ended => "ended",
        }
    }
    pub fn alive(self) -> bool {
        self != State::Ended
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub agent: Agent,
    /// Program name shown to the user: claude, codex, gemini, ...
    pub label: String,
    pub pid: Option<u32>,
    pub tty: Option<String>,
    pub id: Option<String>,
    pub name: String,
    pub title: Option<String>,
    pub cwd: PathBuf,
    pub state: State,
    pub log: Option<PathBuf>,
    pub modified: Option<SystemTime>,
    pub started: Option<SystemTime>,
    /// Control socket when the session runs under `trax run`.
    pub sock: Option<PathBuf>,
    /// Runs in the background inside trax (no window of its own).
    pub hosted: bool,
}

impl Session {
    /// Command line that reopens this conversation, if the agent supports it.
    pub fn resume_args(&self) -> Option<Vec<String>> {
        let id = self.id.clone()?;
        match self.agent {
            Agent::Claude => Some(vec!["claude".into(), "--resume".into(), id]),
            Agent::Codex => Some(vec!["codex".into(), "resume".into(), id]),
            Agent::Other => None,
        }
    }

    pub fn key(&self) -> String {
        match (&self.log, self.pid) {
            (_, Some(pid)) => format!("pid:{pid}"),
            (Some(p), None) => p.display().to_string(),
            _ => self.name.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    User,
    Assistant,
    Tool,
    Output,
    Info,
}

#[derive(Clone, Debug)]
pub struct Msg {
    pub role: Role,
    /// Tool name for Tool messages.
    pub name: Option<String>,
    pub text: String,
    pub time: Option<String>,
}

/// Token use read from a transcript.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    /// Tokens processed over the whole session (input incl. cache, plus output).
    pub tokens: u64,
    /// Size of the context at the last turn.
    pub context: Option<u64>,
    /// Context window of the model, when the transcript says it.
    pub window: Option<u64>,
}

impl Usage {
    /// Share of the context window in use, 0-100. Claude transcripts do not
    /// record the window: assume 200k, or 1M once the context went past 200k.
    pub fn context_pct(&self) -> Option<u64> {
        let c = self.context?;
        let w = self.window.unwrap_or(if c > 200_000 { 1_000_000 } else { 200_000 });
        Some((c * 100 / w.max(1)).min(100))
    }
}

/// One plan rate-limit window (Codex records these after every turn).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    pub used_pct: f64,
    pub minutes: u64,
    /// Unix seconds.
    pub resets_at: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Limits {
    /// Short window (5 hours).
    pub primary: Option<Window>,
    /// Long window (weekly).
    pub secondary: Option<Window>,
    /// Unix seconds of the event that reported them.
    pub at: i64,
}
