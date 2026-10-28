//! Terminal primitives: styled cells, colour encoding, cbreak mode, and an
//! alternate-screen writer that re-sends only the cells that changed.

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

pub type Rgb = [u8; 3];

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Style {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub bold: bool,
}

pub const PLAIN: Style = Style { fg: None, bg: None, bold: false };

impl Style {
    pub const fn fg(c: Rgb) -> Self {
        Style { fg: Some(c), bg: None, bold: false }
    }
    pub const fn bold(self) -> Self {
        Style { bold: true, ..self }
    }
}

/// One terminal cell. A double-width character occupies two: the glyph, then a
/// `WIDE_TAIL` placeholder, so cell index == screen column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub st: Style,
}

pub const WIDE_TAIL: char = '\0';
pub const BLANK: Cell = Cell { ch: ' ', st: PLAIN };

// ── colours ──────────────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Colors {
    None,
    C256,
    True,
}

pub fn detect_colors() -> Colors {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    if !env("NO_COLOR").is_empty() || env("TERM") == "dumb" {
        return Colors::None;
    }
    if matches!(env("COLORTERM").as_str(), "truecolor" | "24bit") {
        return Colors::True;
    }
    // ponytail: no 16-colour fallback; every terminal still in use does 256
    Colors::C256
}

/// Nearest xterm-256 index (6×6×6 cube or 24-step grey ramp), as tmux does it.
fn ansi256([r, g, b]: Rgb) -> u8 {
    let q = |v: u8| {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            (v as usize - 35) / 40
        }
    };
    const LV: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let (qr, qg, qb) = (q(r), q(g), q(b));
    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let gi = if avg > 238 { 23 } else { (avg - 3).max(0) / 10 };
    let gv = 8 + 10 * gi;
    let d = |x: i32, y: i32, z: i32| (r as i32 - x).pow(2) + (g as i32 - y).pow(2) + (b as i32 - z).pow(2);
    if d(gv, gv, gv) < d(LV[qr], LV[qg], LV[qb]) { 232 + gi as u8 } else { (16 + 36 * qr + 6 * qg + qb) as u8 }
}

fn color(buf: &mut String, c: Rgb, fg: bool, cs: Colors) {
    let base = if fg { 38 } else { 48 };
    let _ = match cs {
        Colors::True => write!(buf, "{base};2;{};{};{}", c[0], c[1], c[2]),
        _ => write!(buf, "{base};5;{}", ansi256(c)),
    };
}

/// Emit only the attribute *changes* from `cur` to `want`; per-cell gradient
/// bars would otherwise cost a full reset per cell.
fn sgr(buf: &mut String, cur: &mut Style, want: Style, cs: Colors) {
    if *cur == want || cs == Colors::None {
        return;
    }
    buf.push_str("\x1b[");
    let start = buf.len();
    let sep = |buf: &mut String| {
        if buf.len() > start {
            buf.push(';');
        }
    };
    if (cur.bold && !want.bold) || (cur.fg.is_some() && want.fg.is_none()) || (cur.bg.is_some() && want.bg.is_none()) {
        buf.push('0');
        *cur = PLAIN;
    }
    if want.fg != cur.fg
        && let Some(c) = want.fg
    {
        sep(buf);
        color(buf, c, true, cs);
    }
    if want.bg != cur.bg
        && let Some(c) = want.bg
    {
        sep(buf);
        color(buf, c, false, cs);
    }
    if want.bold && !cur.bold {
        sep(buf);
        buf.push('1');
    }
    buf.push('m');
    *cur = want;
}

/// A row as a standalone string (snapshot output), trailing blanks trimmed.
pub fn encode(cells: &[Cell], cs: Colors) -> String {
    let end = cells.iter().rposition(|c| *c != BLANK).map_or(0, |i| i + 1);
    let mut buf = String::new();
    let mut cur = PLAIN;
    for c in &cells[..end] {
        if c.ch != WIDE_TAIL {
            sgr(&mut buf, &mut cur, c.st, cs);
            buf.push(c.ch);
        }
    }
    if cur != PLAIN {
        buf.push_str("\x1b[0m");
    }
    buf
}

// ── tty state ────────────────────────────────────────────────────────────────
static SAVED: OnceLock<libc::termios> = OnceLock::new();
static ON_ALT: AtomicBool = AtomicBool::new(false);
const LEAVE: &str = "\x1b[?1006l\x1b[?1000l\x1b[0m\x1b[?7h\x1b[?25h\x1b[?1049l";

/// cbreak: no echo, unbuffered keys, but Ctrl-C / Ctrl-Z still raise signals.
pub fn cbreak() {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) != 0 {
            return;
        }
        SAVED.get_or_init(|| t);
        t.c_lflag &= !(libc::ICANON | libc::ECHO);
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(0, libc::TCSANOW, &t);
    }
}

pub fn restore() {
    if let Some(t) = SAVED.get() {
        unsafe { libc::tcsetattr(0, libc::TCSADRAIN, t) };
    }
}

/// Put the terminal back even when we die from a panic. Uses raw write(2):
/// the stdout lock may be held by the code that panicked.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if ON_ALT.swap(false, SeqCst) {
            unsafe { libc::write(1, LEAVE.as_ptr().cast(), LEAVE.len()) };
        }
        restore();
        default(info);
    }));
}

pub fn size() -> (usize, usize) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ as _, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            return (ws.ws_col as usize, ws.ws_row as usize);
        }
    }
    let env = |k: &str, d| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    (env("COLUMNS", 80), env("LINES", 24))
}

pub fn write_out(s: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

// ── screen ───────────────────────────────────────────────────────────────────
pub struct Screen {
    prev: Vec<Vec<Cell>>,
    size: Option<(usize, usize)>,
    pub cs: Colors,
    pub mouse: bool,
}

impl Screen {
    pub fn new(cs: Colors) -> Self {
        Screen { prev: Vec::new(), size: None, cs, mouse: false }
    }

    pub fn enter(&mut self, mouse: bool) {
        // alt screen, hide cursor, no autowrap (writing the last column is safe)
        write_out("\x1b[?1049h\x1b[?25l\x1b[?7l");
        ON_ALT.store(true, SeqCst);
        self.size = None;
        self.set_mouse(mouse);
    }

    pub fn set_mouse(&mut self, on: bool) {
        // 1000 = button press/release incl. wheel, 1006 = SGR coordinates
        write_out(if on { "\x1b[?1000h\x1b[?1006h" } else { "\x1b[?1006l\x1b[?1000l" });
        self.mouse = on;
    }

    pub fn leave(&mut self) {
        if ON_ALT.swap(false, SeqCst) {
            write_out(LEAVE);
        }
    }

    /// Force a full repaint on the next draw.
    pub fn invalidate(&mut self) {
        self.size = None;
    }

    /// Diff against the previous frame per cell: only the changed span of each
    /// row is sent, so an easing bar costs a few cells, not a row.
    pub fn draw(&mut self, rows: Vec<Vec<Cell>>, w: usize, h: usize) {
        let mut buf = String::with_capacity(4096);
        let mut cur = PLAIN;
        if self.size != Some((w, h)) {
            self.size = Some((w, h));
            self.prev.clear();
            buf.push_str("\x1b[2J");
        }
        let mut rows = rows;
        rows.truncate(h);
        for (y, row) in rows.iter().enumerate() {
            let old = self.prev.get(y).filter(|o| o.len() == row.len());
            let (a, b) = match old {
                Some(o) => match diff_span(o, row) {
                    Some(span) => span,
                    None => continue,
                },
                None => (0, row.len()),
            };
            let _ = write!(buf, "\x1b[{};{}H", y + 1, a + 1);
            for c in &row[a..b] {
                if c.ch != WIDE_TAIL {
                    sgr(&mut buf, &mut cur, c.st, self.cs);
                    buf.push(c.ch);
                }
            }
            if old.is_none() && row.len() < w {
                buf.push_str("\x1b[0m\x1b[K");
                cur = PLAIN;
            }
        }
        if self.prev.len() > rows.len() {
            buf.push_str("\x1b[0m");
            for y in rows.len()..self.prev.len() {
                let _ = write!(buf, "\x1b[{};1H\x1b[2K", y + 1);
            }
        }
        self.prev = rows;
        if !buf.is_empty() {
            // synchronized update (DEC 2026): the terminal shows the frame atomically
            write_out(&format!("\x1b[?2026h\x1b[0m{buf}\x1b[0m\x1b[?2026l"));
        }
    }
}

/// Changed cells of a row as a half-open range, widened so it never starts on
/// the right half of a double-width character.
pub fn diff_span(old: &[Cell], new: &[Cell]) -> Option<(usize, usize)> {
    let a = old.iter().zip(new).position(|(x, y)| x != y)?;
    let b = old.iter().zip(new).rposition(|(x, y)| x != y)? + 1;
    let a = if new[a].ch == WIDE_TAIL || old[a].ch == WIDE_TAIL { a.saturating_sub(1) } else { a };
    Some((a, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(s: &str) -> Vec<Cell> {
        s.chars().map(|ch| Cell { ch, st: PLAIN }).collect()
    }

    #[test]
    fn span_covers_only_changes() {
        assert_eq!(diff_span(&row("abcdef"), &row("abXdeY")), Some((2, 6)));
        assert_eq!(diff_span(&row("abc"), &row("abc")), None);
        assert_eq!(diff_span(&row("a中\0b"), &row("a国\0b")), Some((1, 2)));
    }

    #[test]
    fn ansi256_maps_extremes() {
        assert_eq!(ansi256([0, 0, 0]), 16);
        assert_eq!(ansi256([255, 255, 255]), 231);
        assert_eq!(ansi256([255, 0, 0]), 196);
    }
}
