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
