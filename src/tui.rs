//! The trax dashboard: a wall of live sessions, a focus reader, a compose bar
//! that types into sessions, and attach mode that takes over a window.

use crate::discover::Scanner;
use crate::{host, ipc};
use crate::model::{Agent, Session, State};
use crate::parse::Tail;
use crate::render::{self, Mirror, CLAUDE, DIM, FAINT, USER};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Wall,
    Focus,
    Attach,
}

enum Input {
    None,
    Compose(String),
    Filter(String),
    /// "<command> @ <folder>" for a new hosted session.
    New(String),
}

pub struct Options {
    pub view: View,
    pub hours: u64,
    pub show_ended: bool,
    /// Open this session (by control socket) attached, and quit on detach.
    pub select: Option<PathBuf>,
}

struct App {
    scanner: Scanner,
    all: Vec<Session>,
    sessions: Vec<Session>,
    tails: HashMap<PathBuf, Tail>,
    mirrors: HashMap<PathBuf, (Instant, Mirror)>,
    sel: usize,
    view: View,
    back_view: View,
    input: Input,
    filter: String,
    detail: u8,
    hours: u64,
    show_ended: bool,
    mirror_on: bool,
    marked: HashSet<String>,
    done: HashSet<String>,
    prev: HashMap<String, State>,
    back: usize,
    cols: usize,
    help: bool,
    flash: Option<(Instant, String)>,
    last_scan: Instant,
    focus_cache: Option<(PathBuf, u64, usize, u8, Vec<Line<'static>>)>,
    quit: bool,
    /// Started as `trax attach`: leaving the session leaves trax.
    attach_only: bool,
    /// Last size requested for a hosted session's screen.
    sized: Option<(PathBuf, u16, u16)>,
    /// Session being opened: (key, open now even if it is mid-turn).
    opening: Option<(String, bool)>,
}

pub fn run(opts: Options) -> Result<()> {
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste, crossterm::event::EnableMouseCapture);
    let mut app = App::new(opts);
    let res = app.event_loop(&mut terminal);
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste, crossterm::event::DisableMouseCapture);
    ratatui::restore();
    res
}

fn key_bytes(k: &KeyEvent) -> Option<String> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let s: String = match k.code {
        KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
            c @ 'a'..='z' => ((c as u8 - b'a' + 1) as char).to_string(),
            ' ' | '@' | '2' => "\0".into(),
            '[' | '3' => "\x1b".into(),
            '\\' | '4' => "\x1c".into(),
            '6' | '^' => "\x1e".into(),
            '7' | '_' | '/' => "\x1f".into(),
            _ => return None,
        },
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "\r".into(),
        KeyCode::Tab => "\t".into(),
        KeyCode::BackTab => "\x1b[Z".into(),
        KeyCode::Backspace => "\x7f".into(),
        KeyCode::Esc => "\x1b".into(),
        KeyCode::Left => "\x1b[D".into(),
        KeyCode::Right => "\x1b[C".into(),
        KeyCode::Up => "\x1b[A".into(),
        KeyCode::Down => "\x1b[B".into(),
        KeyCode::Home => "\x1b[H".into(),
        KeyCode::End => "\x1b[F".into(),
        KeyCode::PageUp => "\x1b[5~".into(),
        KeyCode::PageDown => "\x1b[6~".into(),
        KeyCode::Delete => "\x1b[3~".into(),
        KeyCode::Insert => "\x1b[2~".into(),
        KeyCode::F(n) => match n {
            1 => "\x1bOP",
            2 => "\x1bOQ",
            3 => "\x1bOR",
            4 => "\x1bOS",
            5 => "\x1b[15~",
            6 => "\x1b[17~",
            7 => "\x1b[18~",
            8 => "\x1b[19~",
            9 => "\x1b[20~",
            10 => "\x1b[21~",
            11 => "\x1b[23~",
            12 => "\x1b[24~",
            _ => return None,
        }
        .into(),
        _ => return None,
    };
    Some(if alt { format!("\x1b{s}") } else { s })
}

fn is_detach(k: &KeyEvent) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('q') | KeyCode::Char('Q'))
}

/// Rows of a mirrored screen that fit in `h`, ending at the cursor or the last text.
fn crop(m: &Mirror, h: usize) -> (usize, Vec<Line<'static>>) {
    let last_text = m
        .lines
        .iter()
        .rposition(|l| l.spans.iter().any(|s| !s.content.trim().is_empty()))
        .unwrap_or(0);
    let end = last_text.max(m.cursor.0 as usize) + 1;
    let start = end.saturating_sub(h);
    (start, m.lines[start..end.min(m.lines.len())].to_vec())
}

impl App {
    fn new(o: Options) -> Self {
        let mut a = App {
            scanner: Scanner::default(),
            all: vec![],
            sessions: vec![],
            tails: HashMap::new(),
            mirrors: HashMap::new(),
            sel: 0,
            view: if o.view == View::Attach { View::Wall } else { o.view },
            back_view: View::Wall,
            input: Input::None,
            filter: String::new(),
            detail: 1,
            hours: o.hours,
            show_ended: o.show_ended,
            mirror_on: false,
            marked: HashSet::new(),
            done: HashSet::new(),
            prev: HashMap::new(),
            back: 0,
            cols: 1,
            help: false,
            flash: None,
            last_scan: Instant::now() - Duration::from_secs(60),
            focus_cache: None,
            quit: false,
            attach_only: false,
            sized: None,
            opening: None,
        };
        a.rescan();
        if let Some(sock) = o.select {
            a.attach_only = true;
            a.select_sock(&sock);
            a.view = View::Attach;
        }
        a
    }

    fn select_sock(&mut self, sock: &PathBuf) -> bool {
        self.filter.clear();
        self.apply_filter();
        match self.sessions.iter().position(|s| s.sock.as_ref() == Some(sock)) {
            Some(i) => {
                self.sel = i;
                self.back = 0;
                true
            }
            None => false,
        }
    }

    fn leave_attach(&mut self) {
        if self.attach_only {
            self.quit = true;
        }
        self.view = self.back_view;
    }

    fn current(&self) -> Option<&Session> {
        self.sessions.get(self.sel)
    }

    fn say(&mut self, s: impl Into<String>) {
        self.flash = Some((Instant::now(), s.into()));
    }

    fn rescan(&mut self) {
        let key = self.current().map(|s| s.key());
        self.all = self.scanner.scan(if self.show_ended { self.hours } else { 0 });
        self.apply_filter();
        if let Some(k) = key
            && let Some(i) = self.sessions.iter().position(|s| s.key() == k) {
                self.sel = i;
            }
        self.sel = self.sel.min(self.sessions.len().saturating_sub(1));
        let keep: HashSet<PathBuf> = self.all.iter().filter_map(|s| s.log.clone()).collect();
        self.tails.retain(|p, _| keep.contains(p));
        self.last_scan = Instant::now();
    }

    fn apply_filter(&mut self) {
        let f = self.filter.to_lowercase();
        self.sessions = self
            .all
            .iter()
            .filter(|s| {
                f.is_empty()
                    || [&s.name, &s.label, &s.cwd.display().to_string(), s.title.as_deref().unwrap_or("")]
                        .iter()
                        .any(|x| x.to_lowercase().contains(&f))
            })
            .cloned()
            .collect();
    }

    /// Poll transcripts of live sessions (so busy/idle stays right) and the selected one.
    fn poll(&mut self) {
        let sel = self.current().map(|s| s.key());
        for s in self.all.iter_mut() {
            let Some(log) = s.log.clone() else { continue };
            if !s.state.alive() && Some(s.key()) != sel {
                continue;
            }
            let t = self.tails.entry(log.clone()).or_insert_with(|| Tail::new(log, s.agent));
            t.poll();
            if s.title.is_none() {
                s.title = t.title.clone();
            }
            if s.agent == Agent::Codex && s.state.alive()
                && let Some(b) = t.busy {
                    s.state = if b { State::Busy } else { State::Idle };
                }
        }
        // "done" = went from busy to waiting since you last looked.
        for s in &self.all {
            let k = s.key();
            let was = self.prev.insert(k.clone(), s.state);
            if was == Some(State::Busy) && matches!(s.state, State::Idle | State::Ended) {
                self.done.insert(k);
            } else if s.state == State::Busy {
                self.done.remove(&k);
            }
        }
        let states: HashMap<String, (State, Option<String>)> =
            self.all.iter().map(|s| (s.key(), (s.state, s.title.clone()))).collect();
        for s in self.sessions.iter_mut() {
            if let Some((st, t)) = states.get(&s.key()) {
                s.state = *st;
                s.title = t.clone();
            }
        }
    }

    fn mirror_of(&mut self, sock: &PathBuf, max_age: Duration) -> Option<&Mirror> {
        let stale = self.mirrors.get(sock).is_none_or(|(t, _)| t.elapsed() >= max_age);
        if stale {
            match ipc::screen(sock) {
                Ok(d) => {
                    self.mirrors.insert(sock.clone(), (Instant::now(), render::mirror(&d)));
                }
                Err(_) => {
                    self.mirrors.remove(sock);
                }
            }
        }
        self.mirrors.get(sock).map(|(_, m)| m)
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            if self.last_scan.elapsed() >= Duration::from_millis(1500) {
                self.rescan();
            }
            self.poll();
            if let Some(s) = self.ready_to_open() {
                self.say(format!("opening {} …", s.name));
                terminal.draw(|f| self.draw(f))?;
                self.do_open(&s);
            }
            terminal.draw(|f| self.draw(f))?;
            let wait = if self.view == View::Attach { 40 } else { 250 };
            if event::poll(Duration::from_millis(wait))? {
                // Drain everything queued so typing stays snappy.
                loop {
                    match event::read()? {
                        Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
                        Event::Paste(p) => self.on_paste(p),
                        Event::Mouse(m) => self.on_mouse(m),
                        _ => {}
                    }
                    if self.quit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    // ---------- input ----------

    fn on_mouse(&mut self, m: crossterm::event::MouseEvent) {
        use crossterm::event::MouseEventKind;
        match (self.view, m.kind) {
            (View::Attach, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) => {
                // Let the agent scroll its own view.
                let up = m.kind == MouseEventKind::ScrollUp;
                if let Some(sock) = self.current().and_then(|s| s.sock.clone()) {
                    let _ = ipc::keys(&sock, if up { "\x1b[5~" } else { "\x1b[6~" });
                    self.mirrors.remove(&sock);
                }
            }
            (View::Focus, MouseEventKind::ScrollUp) => self.back += 3,
            (View::Focus, MouseEventKind::ScrollDown) => self.back = self.back.saturating_sub(3),
            _ => {}
        }
    }

    fn on_paste(&mut self, p: String) {
        match &mut self.input {
            Input::Compose(buf) | Input::Filter(buf) | Input::New(buf) => buf.push_str(&p),
            Input::None if self.view == View::Attach => {
                if let Some(sock) = self.current().and_then(|s| s.sock.clone()) {
                    let _ = ipc::keys(&sock, &format!("\x1b[200~{p}\x1b[201~"));
                }
            }
            Input::None => {}
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if self.view == View::Attach {
            if is_detach(&k) {
                self.leave_attach();
                return;
            }
            let Some(sock) = self.current().and_then(|s| s.sock.clone()) else {
                self.leave_attach();
                return;
            };
            if let Some(b) = key_bytes(&k) {
                if ipc::keys(&sock, &b).is_err() {
                    self.leave_attach();
                    self.say("session closed");
                }
                self.mirrors.remove(&sock);
            }
            return;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match std::mem::replace(&mut self.input, Input::None) {
            Input::Compose(mut buf) => {
                match k.code {
                    KeyCode::Esc => return,
                    KeyCode::Enter if k.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) => buf.push('\n'),
                    KeyCode::Char('j') if ctrl => buf.push('\n'),
                    KeyCode::Enter => {
                        self.send(&buf);
                        return;
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char('u') if ctrl => buf.clear(),
                    KeyCode::Char('w') if ctrl => {
                        let t = buf.trim_end().len();
                        buf.truncate(t);
                        let cut = buf.rfind(' ').map(|i| i + 1).unwrap_or(0);
                        buf.truncate(cut);
                    }
                    KeyCode::Char(c) if !ctrl => buf.push(c),
                    _ => {}
                }
                self.input = Input::Compose(buf);
                return;
            }
            Input::New(mut buf) => {
                match k.code {
                    KeyCode::Esc => return,
                    KeyCode::Enter => {
                        self.start_new(&buf);
                        return;
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char('u') if ctrl => buf.clear(),
                    KeyCode::Char(c) if !ctrl => buf.push(c),
                    _ => {}
                }
                self.input = Input::New(buf);
                return;
            }
            Input::Filter(mut buf) => {
                match k.code {
                    KeyCode::Esc => {
                        self.filter.clear();
                        self.apply_filter();
                        return;
                    }
                    KeyCode::Enter => return,
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) if !ctrl => buf.push(c),
                    _ => {}
                }
                self.filter = buf.clone();
                self.apply_filter();
                self.sel = 0;
                self.input = Input::Filter(buf);
                return;
            }
            Input::None => {}
        }
        if self.help {
            self.help = false;
            return;
        }
        let n = self.sessions.len();
        let before = self.sel;
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Esc => {
                if self.opening.take().is_some() {
                    self.say("cancelled");
                } else if !self.marked.is_empty() {
                    self.marked.clear();
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.apply_filter();
                } else {
                    self.view = View::Wall;
                }
            }
            KeyCode::Enter | KeyCode::Char('o') | KeyCode::Char('a') | KeyCode::Char('T') => self.open(),
            KeyCode::Tab => self.view = if self.view == View::Wall { View::Focus } else { View::Wall },
            KeyCode::Char('i') | KeyCode::Char('>')
                if n > 0 => {
                    self.input = Input::Compose(String::new());
                }
            KeyCode::Char('/') => self.input = Input::Filter(self.filter.clone()),
            KeyCode::Char('n') => {
                let dir = self.current().map(|s| s.cwd.clone()).or_else(|| std::env::current_dir().ok()).unwrap_or_default();
                let agent = self.current().filter(|s| s.agent != Agent::Other).map(|s| s.label.clone()).unwrap_or_else(|| "claude".into());
                self.input = Input::New(format!("{agent} @ {}", tilde(&dir)));
            }
            KeyCode::Char('x') => self.interrupt(),
            KeyCode::Char(' ') => {
                if let Some(s) = self.current() {
                    let key = s.key();
                    if !self.marked.remove(&key) {
                        self.marked.insert(key);
                    }
                }
            }
            KeyCode::Char('m') => {
                self.mirror_on = !self.mirror_on;
                self.say(if self.mirror_on { "mirror: real screens of trax-run sessions" } else { "mirror off: transcripts" });
            }
            KeyCode::Char('t') => {
                self.detail = (self.detail + 1) % 3;
                self.say(["detail: conversation only", "detail: + tool calls", "detail: + tool output"][self.detail as usize]);
            }
            KeyCode::Char('e') => {
                self.show_ended = !self.show_ended;
                self.rescan();
                self.say(if self.show_ended { format!("showing sessions from the last {}h", self.hours) } else { "live sessions only".into() });
            }
            KeyCode::Char('r') => self.rescan(),
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if i < n {
                    self.sel = i;
                }
            }
            KeyCode::PageUp => self.back += 20,
            KeyCode::Char('u') if ctrl => self.back += 20,
            KeyCode::PageDown => self.back = self.back.saturating_sub(20),
            KeyCode::Char('d') if ctrl => self.back = self.back.saturating_sub(20),
            KeyCode::End | KeyCode::Char('G') => self.back = 0,
            KeyCode::Home => self.back = usize::MAX / 2,
            KeyCode::Left | KeyCode::Char('h') if n > 0 => self.sel = (self.sel + n - 1) % n,
            KeyCode::Right | KeyCode::Char('l') if n > 0 => self.sel = (self.sel + 1) % n,
            KeyCode::Up | KeyCode::Char('k') if n > 0 => {
                let step = if self.view == View::Wall { self.cols } else { 1 };
                self.sel = self.sel.saturating_sub(step);
            }
            KeyCode::Down | KeyCode::Char('j') if n > 0 => {
                let step = if self.view == View::Wall { self.cols } else { 1 };
                self.sel = (self.sel + step).min(n - 1);
            }
            _ => {}
        }
        if self.sel != before {
            self.back = 0;
        }
        if self.view == View::Focus
            && let Some(s) = self.current() {
                let k = s.key();
                self.done.remove(&k);
            }
    }

    fn targets(&self) -> Vec<Session> {
        if self.marked.is_empty() {
            self.current().cloned().into_iter().collect()
        } else {
            self.all.iter().filter(|s| self.marked.contains(&s.key())).cloned().collect()
        }
    }

    fn send(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let (mut ok, mut ro, mut err) = (0, 0, Vec::new());
        let mut hint = None;
        for s in self.targets() {
            match &s.sock {
                Some(sock) => match ipc::send(sock, text, true) {
                    Ok(()) => {
                        ok += 1;
                        self.done.remove(&s.key());
                    }
                    Err(e) => err.push(format!("{}: {e}", s.name)),
                },
                None => {
                    ro += 1;
                    hint.get_or_insert(s.name.clone());
                }
            }
        }
        let mut msg = format!("sent to {ok}");
        if ro > 0 {
            msg += &format!(" · {ro} not in trax ({}): press Enter to open it", hint.unwrap_or_default());
        }
        if !err.is_empty() {
            msg += &format!(" · failed: {}", err.join(", "));
        }
        self.say(msg);
    }

    fn interrupt(&mut self) {
        let mut n = 0;
        for s in self.targets() {
            if let Some(sock) = &s.sock
                && ipc::keys(sock, "\x1b").is_ok() {
                    n += 1;
                }
        }
        self.say(if n > 0 { "sent Esc (interrupt)".to_string() } else { "not in trax: press Enter to open it".to_string() });
    }

    /// Enter on a session: be inside it, as in its own window.
    /// Sessions from other windows are moved into trax first; one that is
    /// mid-turn is moved when the turn ends (Enter again: now).
    fn open(&mut self) {
        let Some(s) = self.current().cloned() else { return };
        if s.sock.is_some() {
            self.done.remove(&s.key());
            self.back_view = self.view;
            self.view = View::Attach;
            return;
        }
        if s.resume_args().is_none() {
            self.say(format!("{} can't be reopened inside trax; start it with `trax run {}`", s.label, s.label));
            return;
        }
        let key = s.key();
        let force = matches!(&self.opening, Some((k, _)) if *k == key);
        self.opening = Some((key, force));
    }

    /// The session to move into trax now, if one is waiting and ready.
    fn ready_to_open(&mut self) -> Option<Session> {
        let (key, force) = self.opening.clone()?;
        let Some(s) = self.all.iter().find(|s| s.key() == key).cloned() else {
            self.opening = None;
            self.say("that session is gone");
            return None;
        };
        if s.state == State::Busy && !force {
            self.say(format!("{} is working: opens when its turn ends · Enter now · Esc cancel", s.name));
            return None;
        }
        self.opening = None;
        Some(s)
    }

    fn do_open(&mut self, s: &Session) {
        match host::take(s) {
            Ok(sock) => {
                self.rescan();
                if self.select_sock(&sock) {
                    self.back_view = self.view;
                    self.view = View::Attach;
                    self.flash = None;
                } else {
                    self.say("moved into trax, but it did not show up yet");
                }
            }
            Err(e) => self.say(format!("could not open: {e}")),
        }
    }

    fn start_new(&mut self, spec: &str) {
        let (cmd, dir) = match spec.split_once(" @ ") {
            Some((c, d)) => (c.trim(), untilde(d.trim())),
            None => (spec.trim(), std::env::current_dir().unwrap_or_default()),
        };
        let cmd: Vec<String> = cmd.split_whitespace().map(String::from).collect();
        if cmd.is_empty() {
            return;
        }
        match host::new(&cmd, &dir) {
            Ok(sock) => {
                self.rescan();
                self.select_sock(&sock);
                self.back_view = if self.view == View::Attach { View::Focus } else { self.view };
                self.view = View::Attach;
            }
            Err(e) => self.say(format!("could not start: {e}")),
        }
    }

    // ---------- drawing ----------

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        if self.view == View::Attach {
            self.draw_attach(f, area);
            return;
        }
        let compose_h = match &self.input {
            Input::Compose(b) => (b.lines().count().max(1) + if b.ends_with('\n') { 1 } else { 0 }).min(6) as u16 + 2,
            Input::Filter(_) | Input::New(_) => 1,
            Input::None => 0,
        };
        let [head, body, input, foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(compose_h),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_header(f, head);
        if self.sessions.is_empty() {
            self.draw_empty(f, body);
        } else if self.view == View::Wall {
            self.draw_wall(f, body);
        } else {
            self.draw_focus(f, body);
        }
        self.draw_input(f, input);
        self.draw_footer(f, foot);
        if self.help {
            self.draw_help(f, area);
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let live: Vec<&Session> = self.all.iter().filter(|s| s.state.alive()).collect();
        let count = |st: State| live.iter().filter(|s| s.state == st).count();
        let mut spans = vec![
            Span::styled(" trax ", Style::new().fg(Color::Black).bg(USER).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
        ];
        let mut part = |sym: &str, n: usize, word: &str, c: Color| {
            if n > 0 {
                spans.push(Span::styled(format!("{sym} {n} {word}   "), Style::new().fg(c)));
            }
        };
        part("◉", count(State::Busy), "working", render::state_style(State::Busy).1);
        part("○", count(State::Idle), "waiting", render::state_style(State::Idle).1);
        part("●", count(State::Live), "live", render::state_style(State::Live).1);
        part("✦", self.done.len(), "finished since you looked", CLAUDE);
        if !self.marked.is_empty() {
            spans.push(Span::styled(format!("★ {} marked   ", self.marked.len()), Style::new().fg(Color::Yellow)));
        }
        if !self.filter.is_empty() {
            spans.push(Span::styled(format!("filter “{}”   ", self.filter), Style::new().fg(Color::Yellow)));
        }
        f.render_widget(Line::from(spans), area);
        let clock = chrono::Local::now().format("%H:%M:%S ").to_string();
        f.render_widget(Line::styled(clock, Style::new().fg(DIM)).right_aligned(), area);
    }

    fn draw_empty(&self, f: &mut Frame, area: Rect) {
        let lines = vec![
            Line::default(),
            Line::styled("  No agent sessions running.", Style::new().add_modifier(Modifier::BOLD)),
            Line::default(),
            Line::styled("  Press n to start one inside trax, or start claude / codex anywhere: it shows up here.", Style::new().fg(DIM)),
            Line::styled("  Select any session and press Enter to be inside it (ctrl-q comes back here).", Style::new().fg(DIM)),
            Line::styled("  Press e to browse sessions that ended recently.", Style::new().fg(DIM)),
        ];
        f.render_widget(Paragraph::new(lines), area);
    }

    fn tile_block(&self, s: &Session, selected: bool) -> Block<'static> {
        let (dot, dc) = render::state_style(s.state);
        let ac = render::agent_color(s.agent);
        let key = s.key();
        let mut title = vec![
            Span::styled(format!(" {dot} {} ", state_word(s.state)), Style::new().fg(dc)),
            Span::styled(s.name.clone(), Style::new().fg(if selected { Color::White } else { Color::Gray }).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {} ", s.label), Style::new().fg(ac)),
        ];
        if let Some(t) = &s.tty {
            title.push(Span::styled(format!("{t} "), Style::new().fg(FAINT)));
        }
        let mut badges = Vec::new();
        if self.done.contains(&key) {
            badges.push(Span::styled(" ✦ done ", Style::new().fg(Color::Black).bg(CLAUDE)));
        }
        if self.marked.contains(&key) {
            badges.push(Span::styled(" ★ ", Style::new().fg(Color::Yellow)));
        }
        if !s.state.alive() {
            badges.push(Span::styled(format!(" {} ago ", render::age(s.modified)), Style::new().fg(DIM)));
        }
        let border = if selected { ac } else { FAINT };
        Block::bordered()
            .border_type(if selected { BorderType::Thick } else { BorderType::Rounded })
            .border_style(Style::new().fg(border))
            .title(Line::from(title))
            .title(Line::from(badges).right_aligned())
    }

    fn body_lines(&mut self, idx: usize, inner: Rect, compact: bool) -> Vec<Line<'static>> {
        let s = self.sessions[idx].clone();
        let (w, h) = (inner.width as usize, inner.height as usize);
        let use_mirror = s.sock.is_some() && (self.mirror_on || s.log.is_none());
        if use_mirror {
            let sock = s.sock.clone().unwrap();
            let age = if compact { Duration::from_millis(400) } else { Duration::from_millis(150) };
            return match self.mirror_of(&sock, age) {
                Some(m) => crop(m, h).1,
                None => vec![Line::styled("  (session not answering)", Style::new().fg(DIM))],
            };
        }
        let Some(log) = &s.log else {
            return vec![
                Line::styled("  No transcript for this program.", Style::new().fg(DIM)),
                Line::styled("  Press m to see its screen when it runs inside trax.", Style::new().fg(DIM)),
            ];
        };
        let Some(t) = self.tails.get(log) else { return vec![] };
        if compact {
            let mut lines = render::tail_lines(&t.msgs, s.agent, &s.label, w, h, self.detail);
            if lines.is_empty() {
                lines.push(Line::styled("  (no messages yet)", Style::new().fg(DIM)));
            }
            return lines;
        }
        // Full transcript, cached until it changes.
        let fresh = matches!(&self.focus_cache, Some((p, g, cw, d, _)) if p == log && *g == t.version && *cw == w && *d == self.detail);
        if !fresh {
            let mut all = Vec::new();
            for m in &t.msgs {
                all.extend(render::msg_lines(m, s.agent, &s.label, w, self.detail, false));
            }
            self.focus_cache = Some((log.clone(), t.version, w, self.detail, all));
        }
        let lines = &self.focus_cache.as_ref().unwrap().4;
        let max_back = lines.len().saturating_sub(h);
        self.back = self.back.min(max_back);
        let end = lines.len() - self.back;
        lines[end.saturating_sub(h)..end].to_vec()
    }

    fn draw_wall(&mut self, f: &mut Frame, area: Rect) {
        let n = self.sessions.len();
        let max_cols = (area.width as usize / 46).max(1);
        let mut cols = ((n as f64).sqrt().ceil() as usize).clamp(1, max_cols);
        // Prefer wide layouts on wide screens with few sessions.
        if n <= max_cols && area.width as usize / n.max(1) >= 46 && area.height < 40 {
            cols = n.max(1);
        }
        let rows_fit = (area.height as usize / 8).max(1);
        let cap = cols * rows_fit;
        let page = self.sel / cap;
        let start = page * cap;
        let shown = (n - start).min(cap);
        let rows = shown.div_ceil(cols).max(1);
        self.cols = cols;
        let row_areas = Layout::vertical(vec![Constraint::Ratio(1, rows as u32); rows]).split(area);
        for r in 0..rows {
            let cells = Layout::horizontal(vec![Constraint::Ratio(1, cols as u32); cols]).split(row_areas[r]);
            for c in 0..cols {
                let i = start + r * cols + c;
                if i >= start + shown {
                    break;
                }
                let selected = i == self.sel;
                let s = self.sessions[i].clone();
                let mut block = self.tile_block(&s, selected);
                let sub = match &s.title {
                    Some(t) => format!(" {} · {} ", render::short_path(&s.cwd, 28), t),
                    None => format!(" {} ", render::short_path(&s.cwd, 40)),
                };
                block = block.title_bottom(Line::styled(
                    render::truncate(&sub, cells[c].width.saturating_sub(4) as usize),
                    Style::new().fg(if selected { DIM } else { FAINT }),
                ));
                let inner = block.inner(cells[c]);
                f.render_widget(block, cells[c]);
                let lines = self.body_lines(i, inner, true);
                f.render_widget(Paragraph::new(lines), inner);
            }
        }
        if n > cap {
            let pages = n.div_ceil(cap);
            let txt = format!(" page {}/{} ", page + 1, pages);
            let r = Rect { x: area.x + area.width.saturating_sub(txt.len() as u16 + 1), y: area.y + area.height - 1, width: txt.len() as u16, height: 1 };
            f.render_widget(Line::styled(txt, Style::new().fg(Color::Yellow)), r);
        }
    }

    fn draw_focus(&mut self, f: &mut Frame, area: Rect) {
        let rail_w = (area.width / 4).clamp(24, 36);
        let [rail, main] = Layout::horizontal([Constraint::Length(rail_w), Constraint::Min(20)]).areas(area);
        let items: Vec<ListItem> = self
            .sessions
            .iter()
            .map(|s| {
                let (dot, dc) = render::state_style(s.state);
                let key = s.key();
                let mut spans = vec![
                    Span::styled(format!("{dot} "), Style::new().fg(dc)),
                    Span::styled(
                        render::truncate(&s.name, rail_w.saturating_sub(12) as usize),
                        Style::new().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!(" {}", s.label), Style::new().fg(render::agent_color(s.agent))),
                ];
                if self.done.contains(&key) {
                    spans.push(Span::styled(" ✦", Style::new().fg(CLAUDE)));
                }
                if self.marked.contains(&key) {
                    spans.push(Span::styled(" ★", Style::new().fg(Color::Yellow)));
                }
                let sub = s.title.clone().unwrap_or_else(|| render::short_path(&s.cwd, 40));
                ListItem::new(vec![
                    Line::from(spans),
                    Line::styled(format!("  {}", render::truncate(&sub, rail_w.saturating_sub(4) as usize)), Style::new().fg(FAINT)),
                ])
            })
            .collect();
        let mut st = ListState::default().with_selected(Some(self.sel));
        f.render_stateful_widget(
            List::new(items)
                .block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(FAINT)))
                .highlight_style(Style::new().bg(Color::Rgb(38, 40, 52))),
            rail,
            &mut st,
        );

        let s = self.sessions[self.sel].clone();
        let mut block = self.tile_block(&s, true);
        let mut bottom = vec![Span::styled(
            format!(" {} ", render::short_path(&s.cwd, 60)),
            Style::new().fg(DIM),
        )];
        if let Some(p) = s.pid {
            bottom.push(Span::styled(format!("pid {p} "), Style::new().fg(FAINT)));
        }
        if self.back > 0 {
            bottom.push(Span::styled(format!(" ▲ scrolled {} lines · End to follow ", self.back), Style::new().fg(Color::Yellow)));
        }
        block = block.title_bottom(Line::from(bottom));
        if let Some(t) = &s.title {
            block = block.title(Line::styled(format!(" {} ", render::truncate(t, 50)), Style::new().fg(DIM)).centered());
        }
        let inner = block.inner(main);
        f.render_widget(block, main);
        let lines = self.body_lines(self.sel, inner, false);
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_attach(&mut self, f: &mut Frame, area: Rect) {
        let [head, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
        let Some(s) = self.current().cloned() else {
            self.leave_attach();
            return;
        };
        let ac = render::agent_color(s.agent);
        f.render_widget(
            Line::from({
                let (dot, dc) = render::state_style(s.state);
                let mut v = vec![
                    Span::styled(format!(" {} · {} ", s.name, s.label), Style::new().fg(Color::Black).bg(ac).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("  {dot} {}", state_word(s.state)), Style::new().fg(dc).add_modifier(Modifier::BOLD)),
                ];
                // Only when another session finished: time to go back.
                let done = self.all.iter().filter(|o| o.key() != s.key() && self.done.contains(&o.key())).count();
                if done > 0 {
                    let word = if done == 1 { "other session finished" } else { "other sessions finished" };
                    v.push(Span::styled(format!("    ✦ {done} {word}"), Style::new().fg(CLAUDE).add_modifier(Modifier::BOLD)));
                }
                v
            }),
            head,
        );
        f.render_widget(
            Line::from(vec![Span::styled(" ctrl-q ", Style::new().fg(Color::Black).bg(Color::White).add_modifier(Modifier::BOLD)), Span::styled(" back to trax ", Style::new().fg(DIM))]).right_aligned(),
            head,
        );
        let Some(sock) = s.sock.clone() else {
            self.leave_attach();
            return;
        };
        // A hosted session has no window of its own: give it ours.
        if s.hosted {
            let want = (sock.clone(), body.height, body.width);
            if self.sized.as_ref() != Some(&want) && ipc::resize(&sock, body.height, body.width).is_ok() {
                self.sized = Some(want);
                self.mirrors.remove(&sock);
            }
        }
        match self.mirror_of(&sock, Duration::from_millis(60)) {
            Some(m) => {
                let (start, lines) = crop(m, body.height as usize);
                let (cr, cc) = m.cursor;
                let narrower = m.cols > body.width || m.rows > body.height;
                f.render_widget(Paragraph::new(lines), body);
                let y = cr as i32 - start as i32;
                if y >= 0 && (y as u16) < body.height && cc < body.width {
                    f.set_cursor_position(Position { x: body.x + cc, y: body.y + y as u16 });
                }
                if narrower {
                    let txt = format!(" window is {}x{}, enlarge trax to see all of it ", m.cols, m.rows);
                    let w = txt.chars().count() as u16;
                    f.render_widget(
                        Line::styled(txt, Style::new().fg(Color::Black).bg(Color::Yellow)),
                        Rect { x: area.x + area.width.saturating_sub(w), y: area.y, width: w.min(area.width), height: 1 },
                    );
                }
            }
            None => {
                self.leave_attach();
                self.say("session closed");
            }
        }
    }

    fn draw_input(&self, f: &mut Frame, area: Rect) {
        match &self.input {
            Input::Compose(buf) => {
                let targets = self.targets();
                let to = if targets.len() == 1 {
                    format!("{} ({})", targets[0].name, targets[0].label)
                } else {
                    format!("{} marked sessions", targets.len())
                };
                let ro = targets.iter().filter(|s| s.sock.is_none()).count();
                let mut title = vec![Span::styled(format!(" message → {to} "), Style::new().fg(USER).add_modifier(Modifier::BOLD))];
                if ro > 0 {
                    title.push(Span::styled(format!(" {ro} read-only "), Style::new().fg(Color::Black).bg(Color::Yellow)));
                }
                let block = Block::bordered()
                    .border_type(BorderType::Thick)
                    .border_style(Style::new().fg(USER))
                    .title(Line::from(title))
                    .title_bottom(Line::styled(" Enter send · Alt-Enter / ctrl-j new line · Esc cancel ", Style::new().fg(DIM)).right_aligned());
                let inner = block.inner(area);
                f.render_widget(block, area);
                let mut text = buf.clone();
                text.push('▏');
                let lines: Vec<Line> = text.split('\n').map(|l| Line::raw(l.to_string())).collect();
                let skip = lines.len().saturating_sub(inner.height as usize);
                f.render_widget(Paragraph::new(lines[skip..].to_vec()).wrap(Wrap { trim: false }), inner);
            }
            Input::New(buf) => {
                f.render_widget(
                    Line::from(vec![
                        Span::styled(" new session ", Style::new().fg(Color::Black).bg(USER)),
                        Span::raw(format!(" {buf}▏")),
                        Span::styled("   command @ folder · Enter start · Esc cancel", Style::new().fg(DIM)),
                    ]),
                    area,
                );
            }
            Input::Filter(buf) => {
                f.render_widget(
                    Line::from(vec![Span::styled(" filter / ", Style::new().fg(Color::Black).bg(Color::Yellow)), Span::raw(format!(" {buf}▏"))]),
                    area,
                );
            }
            Input::None => {}
        }
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let keys: &[(&str, &str)] = match self.view {
            View::Wall => &[("←↑↓→", "select"), ("enter", "open session"), ("tab", "read"), ("n", "new"), ("i", "quick message"), ("space", "mark"), ("e", "ended"), ("/", "filter"), ("?", "help"), ("q", "quit")],
            _ => &[("↑↓", "select"), ("enter", "open session"), ("wheel/PgUp", "scroll"), ("tab", "wall"), ("n", "new"), ("i", "quick message"), ("x", "interrupt"), ("t", "detail"), ("?", "help"), ("q", "quit")],
        };
        let mut spans = Vec::new();
        for (k, d) in keys {
            spans.push(Span::styled(format!(" {k}"), Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD)));
            spans.push(Span::styled(format!(" {d} "), Style::new().fg(FAINT)));
        }
        f.render_widget(Line::from(spans), area);
        if let Some((t, msg)) = &self.flash
            && t.elapsed() < Duration::from_secs(5) {
                let w = (msg.chars().count() as u16 + 2).min(area.width);
                let r = Rect { x: area.x + area.width - w, y: area.y, width: w, height: 1 };
                f.render_widget(Clear, r);
                f.render_widget(Line::styled(format!(" {msg} "), Style::new().fg(Color::Black).bg(USER)), r);
            }
    }

    fn draw_help(&self, f: &mut Frame, area: Rect) {
        let rows = [
            ("", "Watch"),
            ("arrows / hjkl", "move between sessions (1-9 jumps)"),
            ("enter", "open the session: you are inside it (ctrl-q back)"),
            ("tab", "switch wall ↔ reader"),
            ("wheel PgUp PgDn Home", "scroll the whole conversation (End follows live)"),
            ("t", "detail: conversation → + tool calls → + tool output"),
            ("m", "mirror: show the real screen of sessions inside trax"),
            ("e", "include sessions that ended recently"),
            ("/", "filter by name, folder, title"),
            ("", "Drive"),
            ("n", "start a new session inside trax (command @ folder)"),
            ("i", "quick message without opening (to marked sessions too)"),
            ("space", "mark sessions; a message then goes to every marked one"),
            ("x", "send Esc to interrupt the current turn"),
            ("", "Signals"),
            ("◉ ○ ● ·", "working · waiting for you · running (state unknown) · ended"),
            ("✦ done", "finished a turn since you last looked"),
        ];
        let w = 74.min(area.width);
        let h = (rows.len() as u16 + 4).min(area.height);
        let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
        let lines: Vec<Line> = rows
            .iter()
            .map(|(k, d)| {
                if k.is_empty() {
                    Line::styled(format!(" {d}"), Style::new().fg(USER).add_modifier(Modifier::BOLD))
                } else {
                    Line::from(vec![Span::styled(format!("   {k:<20}"), Style::new().add_modifier(Modifier::BOLD)), Span::styled(d.to_string(), Style::new().fg(DIM))])
                }
            })
            .collect();
        f.render_widget(Clear, r);
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(USER)).title(" trax keys ")),
            r,
        );
    }
}

fn tilde(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s,
    }
}

fn untilde(s: &str) -> PathBuf {
    match (s.strip_prefix('~'), std::env::var("HOME")) {
        (Some(rest), Ok(h)) => PathBuf::from(format!("{h}{rest}")),
        _ => PathBuf::from(s),
    }
}

/// Plain words for a session's state, as shown next to its sign.
fn state_word(s: State) -> &'static str {
    match s {
        State::Busy => "working",
        State::Idle => "waiting for you",
        State::Live => "running",
        State::Ended => "ended",
    }
}
