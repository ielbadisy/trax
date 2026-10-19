//! Turning messages and mirrored screens into styled terminal lines.

use crate::model::{Agent, Msg, Role, State};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::num::NonZeroU16;
use std::time::SystemTime;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const CLAUDE: Color = Color::Rgb(217, 119, 87);
pub const CODEX: Color = Color::Rgb(110, 160, 255);
pub const OTHER: Color = Color::Rgb(190, 140, 255);
pub const USER: Color = Color::Rgb(90, 210, 200);
pub const TOOL: Color = Color::Rgb(230, 190, 90);
pub const DIM: Color = Color::Rgb(120, 120, 130);
pub const FAINT: Color = Color::Rgb(80, 80, 90);

pub fn agent_color(a: Agent) -> Color {
    match a {
        Agent::Claude => CLAUDE,
        Agent::Codex => CODEX,
        Agent::Other => OTHER,
    }
}

pub fn state_style(s: State) -> (char, Color) {
    match s {
        State::Busy => ('◉', Color::Rgb(120, 220, 120)),
        State::Idle => ('○', Color::Rgb(230, 200, 90)),
        State::Live => ('●', Color::Rgb(120, 180, 230)),
        State::Ended => ('·', DIM),
    }
}

/// Greedy word wrap by display width.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(4);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let start = out.len();
        let mut cur = String::new();
        let mut cw = 0;
        let mut tokens = Vec::new();
        let mut tok = String::new();
        let mut space = None;
        for ch in raw.chars() {
            let is_sp = ch == ' ';
            if space.is_some_and(|s| s != is_sp) {
                tokens.push(std::mem::take(&mut tok));
            }
            space = Some(is_sp);
            tok.push(ch);
        }
        if !tok.is_empty() {
            tokens.push(tok);
        }
        for t in tokens {
            let tw = t.width();
            if cw + tw <= width {
                cur.push_str(&t);
                cw += tw;
            } else if t.starts_with(' ') {
                out.push(std::mem::take(&mut cur));
                cw = 0;
            } else {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur).trim_end().to_string());
                    cw = 0;
                }
                if tw <= width {
                    cur.push_str(&t);
                    cw = tw;
                } else {
                    for ch in t.chars() {
                        let w = ch.width().unwrap_or(0);
                        if cw + w > width {
                            out.push(std::mem::take(&mut cur));
                            cw = 0;
                        }
                        cur.push(ch);
                        cw += w;
                    }
                }
            }
        }
        if !cur.is_empty() || out.len() == start {
            out.push(cur);
        }
    }
    out
}

pub fn truncate(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// How much detail to show: 0 conversation only, 1 + tool calls, 2 + tool output.
pub fn shown(m: &Msg, detail: u8) -> bool {
    match m.role {
        Role::User | Role::Assistant | Role::Info => true,
        Role::Tool => detail >= 1,
        Role::Output => detail >= 2,
    }
}

pub fn msg_lines(m: &Msg, agent: Agent, label: &str, width: usize, detail: u8, compact: bool) -> Vec<Line<'static>> {
    let mut v = Vec::new();
    if !shown(m, detail) {
        return v;
    }
    let time = m.time.clone().unwrap_or_default();
    let body_w = width.saturating_sub(2);
    match m.role {
        Role::User | Role::Assistant => {
            let (who, color) = if m.role == Role::User { ("you".to_string(), USER) } else { (label.to_string(), agent_color(agent)) };
            let mark = if m.role == Role::User { "▶ " } else { "◆ " };
            if !compact {
                v.push(Line::default());
            }
            v.push(Line::from(vec![
                Span::styled(mark, Style::new().fg(color)),
                Span::styled(who, Style::new().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {time}"), Style::new().fg(FAINT)),
            ]));
            let style = if m.role == Role::User { Style::new().fg(Color::White).add_modifier(Modifier::BOLD) } else { Style::new() };
            let lines = wrap(&m.text, body_w);
            let limit = if compact { 12 } else { usize::MAX };
            let n = lines.len();
            let mut md = Md::default();
            for l in lines.into_iter().take(limit) {
                let mut spans = vec![Span::raw("  ")];
                if m.role == Role::Assistant {
                    spans.extend(md.line(&l));
                } else {
                    spans.push(Span::styled(l, style));
                }
                v.push(Line::from(spans));
            }
            if n > limit {
                v.push(Line::styled(format!("  … {} more lines", n - limit), Style::new().fg(FAINT)));
            }
        }
        Role::Tool => {
            let name = m.name.clone().unwrap_or_default();
            let head = format!("  ⚙ {name} ");
            let rest = truncate(&m.text, width.saturating_sub(head.width()));
            v.push(Line::from(vec![
                Span::styled(head, Style::new().fg(TOOL)),
                Span::styled(rest, Style::new().fg(DIM)),
            ]));
        }
        Role::Output => {
            let lines = wrap(&m.text, width.saturating_sub(4));
            let limit = if compact { 3 } else { 8 };
            let n = lines.len();
            for l in lines.into_iter().take(limit) {
                v.push(Line::from(vec![Span::styled("  │ ", Style::new().fg(FAINT)), Span::styled(l, Style::new().fg(DIM))]));
            }
            if n > limit {
                v.push(Line::styled(format!("  │ … +{} lines", n - limit), Style::new().fg(FAINT)));
            }
        }
        Role::Info => {
            v.push(Line::styled(
                format!("  ─ {} {time}", m.text),
                Style::new().fg(DIM).add_modifier(Modifier::ITALIC),
            ));
        }
    }
    v
}

/// The last `height` lines of a transcript, built from the end (cheap for tiles).
pub fn tail_lines(msgs: &[Msg], agent: Agent, label: &str, width: usize, height: usize, detail: u8) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for m in msgs.iter().rev() {
        let mut l = msg_lines(m, agent, label, width, detail, true);
        if l.is_empty() {
            continue;
        }
        l.append(&mut out);
        out = l;
        if out.len() >= height {
            break;
        }
    }
    let skip = out.len().saturating_sub(height);
    out.split_off(skip)
}

fn vt_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

/// Rebuild a mirrored screen as styled lines.
pub struct Mirror {
    pub lines: Vec<Line<'static>>,
    pub cursor: (u16, u16),
    pub rows: u16,
    pub cols: u16,
}

pub fn mirror(dump: &crate::ipc::ScreenDump) -> Mirror {
    let nz = |x: u16| NonZeroU16::new(x.max(1)).unwrap();
    let mut p = vt100::Parser::new(nz(dump.rows), nz(dump.cols), 0);
    p.process(dump.data.as_bytes());
    let s = p.screen();
    let mut lines = Vec::with_capacity(dump.rows as usize);
    for r in 0..dump.rows {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut cur = String::new();
        let mut cur_style = Style::new();
        for c in 0..dump.cols {
            let Some(cell) = s.cell(r, c) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut st = Style::new();
            let (mut fg, mut bg) = (vt_color(cell.fgcolor()), vt_color(cell.bgcolor()));
            if cell.inverse() {
                std::mem::swap(&mut fg, &mut bg);
                fg = fg.or(Some(Color::Black));
                bg = bg.or(Some(Color::White));
            }
            if let Some(f) = fg {
                st = st.fg(f);
            }
            if let Some(b) = bg {
                st = st.bg(b);
            }
            if cell.bold() {
                st = st.add_modifier(Modifier::BOLD);
            }
            if cell.dim() {
                st = st.add_modifier(Modifier::DIM);
            }
            if cell.italic() {
                st = st.add_modifier(Modifier::ITALIC);
            }
            if cell.underline() {
                st = st.add_modifier(Modifier::UNDERLINED);
            }
            if st != cur_style && !cur.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut cur), cur_style));
            }
            cur_style = st;
            let t = cell.contents();
            if t.is_empty() { cur.push(' ') } else { cur.push_str(t) }
        }
        if !cur.is_empty() {
            spans.push(Span::styled(cur.trim_end().to_string(), cur_style));
        }
        lines.push(Line::from(spans));
    }
    Mirror { lines, cursor: dump.cursor, rows: dump.rows, cols: dump.cols }
}

pub fn age(t: Option<SystemTime>) -> String {
    let Some(t) = t else { return String::new() };
    let secs = SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    match secs {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

pub fn short_path(p: &std::path::Path, max: usize) -> String {
    let home = crate::discover::home();
    let s = match p.strip_prefix(&home) {
        Ok(r) if r.as_os_str().is_empty() => "~".to_string(),
        Ok(r) => format!("~/{}", r.display()),
        Err(_) => p.display().to_string(),
    };
    if s.width() <= max {
        return s;
    }
    let parts: Vec<&str> = s.split('/').collect();
    let mut tail = String::new();
    for part in parts.iter().rev() {
        let cand = if tail.is_empty() { part.to_string() } else { format!("{part}/{tail}") };
        if cand.width() + 2 > max {
            break;
        }
        tail = cand;
    }
    if tail.is_empty() { truncate(&s, max) } else { format!("…/{tail}") }
}

/// Light markdown for assistant text: headings, **bold**, `code`, fenced blocks.
/// State carries across wrapped lines so a bold run split by wrapping stays bold.
#[derive(Default)]
struct Md {
    bold: bool,
    code: bool,
    fence: bool,
}

impl Md {
    fn line(&mut self, l: &str) -> Vec<Span<'static>> {
        let code_style = Style::new().fg(TOOL);
        let t = l.trim_start();
        if t.starts_with("```") {
            self.fence = !self.fence;
            return vec![Span::styled(l.to_string(), Style::new().fg(FAINT))];
        }
        if self.fence {
            return vec![Span::styled(l.to_string(), code_style)];
        }
        if t.starts_with('#') {
            let h = t.trim_start_matches('#').trim_start();
            self.bold = false;
            return vec![Span::styled(h.to_string(), Style::new().fg(Color::White).add_modifier(Modifier::BOLD | Modifier::UNDERLINED))];
        }
        let mut out = Vec::new();
        let mut cur = String::new();
        let style = |md: &Md| {
            if md.code {
                code_style
            } else if md.bold {
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            }
        };
        let cs: Vec<char> = l.chars().collect();
        let mut i = 0;
        while i < cs.len() {
            let c = cs[i];
            if c == '`' {
                out.push(Span::styled(std::mem::take(&mut cur), style(self)));
                self.code = !self.code;
                i += 1;
            } else if !self.code && c == '*' && cs.get(i + 1) == Some(&'*') {
                out.push(Span::styled(std::mem::take(&mut cur), style(self)));
                self.bold = !self.bold;
                i += 2;
            } else {
                cur.push(c);
                i += 1;
            }
        }
        out.push(Span::styled(cur, style(self)));
        // Inline code never spans lines in practice; reset to avoid a stray backtick tinting the rest.
        self.code = false;
        out.retain(|s| !s.content.is_empty());
        out
    }
}
