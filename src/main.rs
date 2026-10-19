mod discover;
mod host;
mod ipc;
mod model;
mod parse;
mod render;
mod run;
mod tui;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use model::{Role, Session};
use std::io::IsTerminal;

#[derive(Parser)]
#[command(
    name = "trax",
    version,
    about = "Watch and drive every Claude Code, Codex and other agent CLI session from one terminal",
    after_help = "Run `trax` for the dashboard. Start agents with `trax run claude` (or codex, gemini, ...) to also type into them from trax."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Clone, Copy, ValueEnum)]
enum ViewArg {
    Wall,
    Focus,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the dashboard (default)
    Watch {
        /// Also show sessions that ended recently
        #[arg(short, long)]
        all: bool,
        /// How far back "recently" goes, in hours
        #[arg(long, default_value_t = 24)]
        hours: u64,
        #[arg(long, value_enum, default_value = "wall")]
        view: ViewArg,
    },
    /// List sessions
    Ls {
        /// Include sessions that ended in the last --hours
        #[arg(short, long)]
        all: bool,
        #[arg(long, default_value_t = 24)]
        hours: u64,
        #[arg(long)]
        json: bool,
    },
    /// Print a session's conversation
    Show {
        /// pid, tty (pts/3), session id prefix, or part of the name/folder/title
        session: String,
        /// Keep printing new messages
        #[arg(short, long)]
        follow: bool,
        /// Number of messages to print first
        #[arg(short = 'n', long, default_value_t = 20)]
        last: usize,
        /// 0 conversation only, 1 + tool calls, 2 + tool output
        #[arg(long, default_value_t = 1)]
        detail: u8,
    },
    /// Type a message into a session started with `trax run`
    Send {
        session: String,
        /// The message (joined with spaces)
        #[arg(required = true, trailing_var_arg = true)]
        text: Vec<String>,
        /// Type it without pressing Enter
        #[arg(long)]
        no_enter: bool,
    },
    /// Print the current screen of a session started with `trax run`
    Screen { session: String },
    /// Run an agent CLI so trax can mirror and drive it, e.g. `trax run claude`
    Run {
        /// Run in the background with no window (what `trax new` uses)
        #[arg(long)]
        headless: bool,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Start a new agent session hosted by trax, e.g. `trax new codex -C ~/proj`
    New {
        /// Folder to start in (default: here)
        #[arg(short = 'C', long)]
        dir: Option<std::path::PathBuf>,
        /// Do not open it full screen afterwards
        #[arg(short, long)]
        detach: bool,
        /// Command to run (default: claude)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Move a session that runs in another window into trax, keeping its conversation
    Take { session: String },
    /// Open a session full screen (ctrl-q to leave)
    Attach { session: String },
}

fn find(sessions: &[Session], q: &str) -> Result<Session> {
    let ql = q.to_lowercase();
    let hit = |s: &&Session| -> bool {
        s.pid.map(|p| p.to_string()) == Some(q.to_string())
            || s.tty.as_deref() == Some(q)
            || s.id.as_deref().is_some_and(|id| id.starts_with(q))
            || s.name.to_lowercase().contains(&ql)
            || s.cwd.display().to_string().to_lowercase().contains(&ql)
            || s.title.as_deref().is_some_and(|t| t.to_lowercase().contains(&ql))
    };
    let hits: Vec<&Session> = sessions.iter().filter(hit).collect();
    match hits.as_slice() {
        [] => bail!("no session matches {q:?} (see `trax ls -a`)"),
        [one] => Ok((*one).clone()),
        many => {
            // Prefer a single live match.
            let live: Vec<&&Session> = many.iter().filter(|s| s.state.alive()).collect();
            if live.len() == 1 {
                return Ok((**live[0]).clone());
            }
            let names: Vec<String> = many.iter().take(6).map(|s| format!("{} [{}]", s.name, s.pid.map(|p| p.to_string()).unwrap_or_else(|| "ended".into()))).collect();
            bail!("{q:?} matches several sessions: {} — use a pid or tty", names.join(", "))
        }
    }
}

fn ls(all: bool, hours: u64, json: bool) -> Result<()> {
    let sessions = discover::Scanner::default().scan(if all { hours } else { 0 });
    if json {
        let v: Vec<_> = sessions
            .iter()
            .map(|s| {
                serde_json::json!({
                    "agent": s.label, "state": s.state.word(), "pid": s.pid, "tty": s.tty,
                    "name": s.name, "title": s.title, "cwd": s.cwd, "session_id": s.id,
                    "log": s.log, "drivable": s.sock.is_some(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    if sessions.is_empty() {
        println!("no agent sessions running (trax ls -a shows recent ones)");
        return Ok(());
    }
    println!("{:<6} {:<7} {:>7} {:<7} {:<22} {:<34} TITLE", "STATE", "AGENT", "PID", "TTY", "NAME", "FOLDER");
    for s in &sessions {
        println!(
            "{:<6} {:<7} {:>7} {:<7} {:<22} {:<34} {}",
            s.state.word(),
            s.label,
            s.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
            s.tty.clone().unwrap_or_else(|| "-".into()),
            render::truncate(&s.name, 22),
            render::short_path(&s.cwd, 34),
            render::truncate(s.title.as_deref().unwrap_or(""), 60),
        );
    }
    Ok(())
}

fn print_msg(m: &model::Msg, label: &str, color: bool) {
    let (c, r) = if color { ("\x1b[", "\x1b[0m") } else { ("", "") };
    let paint = |code: &str, s: &str| if color { format!("{c}{code}m{s}{r}") } else { s.to_string() };
    let time = m.time.clone().unwrap_or_default();
    match m.role {
        Role::User => println!("\n{} {}\n{}", paint("1;36", "▶ you"), paint("2", &time), m.text),
        Role::Assistant => println!("\n{} {}\n{}", paint("1;33", &format!("◆ {label}")), paint("2", &time), m.text),
        Role::Tool => println!("  {} {}", paint("33", &format!("⚙ {}", m.name.clone().unwrap_or_default())), paint("2", &m.text)),
        Role::Output => {
            let lines: Vec<&str> = m.text.lines().collect();
            for l in lines.iter().take(8) {
                println!("  {}", paint("2", &format!("│ {l}")));
            }
            if lines.len() > 8 {
                println!("  {}", paint("2", &format!("│ … +{} lines", lines.len() - 8)));
            }
        }
        Role::Info => println!("  {}", paint("2;3", &format!("─ {} {time}", m.text))),
    }
}

fn show(q: &str, follow: bool, last: usize, detail: u8) -> Result<()> {
    let sessions = discover::Scanner::default().scan(24 * 7);
    let s = find(&sessions, q)?;
    let Some(log) = s.log.clone() else {
        bail!("{} has no transcript trax can read", s.name);
    };
    let color = std::io::stdout().is_terminal();
    let mut t = parse::Tail::new(log, s.agent);
    t.poll();
    let visible: Vec<&model::Msg> = t.msgs.iter().filter(|m| render::shown(m, detail)).collect();
    for m in visible.iter().skip(visible.len().saturating_sub(last)) {
        print_msg(m, &s.label, color);
    }
    if !follow {
        return Ok(());
    }
    let mut seen = t.total;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if t.poll() && t.total > seen {
            let new = (t.total - seen) as usize;
            for m in t.msgs.iter().skip(t.msgs.len().saturating_sub(new)) {
                if render::shown(m, detail) {
                    print_msg(m, &s.label, color);
                }
            }
            seen = t.total;
        }
    }
}

fn send(q: &str, text: &str, enter: bool) -> Result<()> {
    let sessions = discover::Scanner::default().scan(0);
    let s = find(&sessions, q)?;
    let Some(sock) = s.sock else {
        bail!("{} runs in a window trax does not own. Move it into trax first: trax take {}", s.name, s.pid.map(|p| p.to_string()).unwrap_or(s.name.clone()));
    };
    ipc::send(&sock, text, enter)?;
    println!("sent to {} ({})", s.name, s.label);
    Ok(())
}

fn screen(q: &str) -> Result<()> {
    let sessions = discover::Scanner::default().scan(0);
    let s = find(&sessions, q)?;
    let Some(sock) = s.sock else {
        bail!("{} is not running under `trax run`, so its screen is not available", s.name);
    };
    let m = render::mirror(&ipc::screen(&sock)?);
    for l in &m.lines {
        let t: String = l.spans.iter().map(|x| x.content.as_ref()).collect();
        println!("{}", t.trim_end());
    }
    Ok(())
}

fn attach_sock(sock: &std::path::Path) -> Result<()> {
    tui::run(tui::Options { view: tui::View::Attach, hours: 24, show_ended: false, select: Some(sock.to_path_buf()) })
}

fn main() -> Result<()> {
    // Behave like a normal Unix tool when piped into `head`.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Watch { all: false, hours: 24, view: ViewArg::Wall }) {
        Cmd::Watch { all, hours, view } => tui::run(tui::Options {
            view: match view {
                ViewArg::Wall => tui::View::Wall,
                ViewArg::Focus => tui::View::Focus,
            },
            hours,
            show_ended: all,
            select: None,
        }),
        Cmd::Ls { all, hours, json } => ls(all, hours, json),
        Cmd::Show { session, follow, last, detail } => show(&session, follow, last, detail),
        Cmd::Send { session, text, no_enter } => send(&session, &text.join(" "), !no_enter),
        Cmd::Screen { session } => screen(&session),
        Cmd::Run { command, headless } => {
            let code = run::run(command, headless)?;
            std::process::exit(code);
        }
        Cmd::New { dir, detach, command } => {
            let cmd = if command.is_empty() { vec!["claude".to_string()] } else { command };
            let cwd = match dir {
                Some(d) => d,
                None => std::env::current_dir()?,
            };
            let sock = host::new(&cmd, &cwd)?;
            println!("started {} in {}", cmd.join(" "), cwd.display());
            if detach { Ok(()) } else { attach_sock(&sock) }
        }
        Cmd::Take { session } => {
            let sessions = discover::Scanner::default().scan(24 * 7);
            let s = find(&sessions, &session)?;
            if s.sock.is_some() {
                println!("{} is already in trax", s.name);
                return Ok(());
            }
            host::take(&s)?;
            println!("{} now runs inside trax: trax attach {}, trax send {} ..., or the dashboard", s.name, s.name, s.name);
            Ok(())
        }
        Cmd::Attach { session } => {
            let sessions = discover::Scanner::default().scan(0);
            let s = find(&sessions, &session)?;
            let Some(sock) = s.sock else {
                bail!("{} runs in a window trax does not own. Move it into trax first: trax take {}", s.name, s.pid.map(|p| p.to_string()).unwrap_or(s.name.clone()));
            };
            attach_sock(&sock)
        }
    }
}
