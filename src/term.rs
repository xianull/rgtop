//! Terminal primitives: styled cells, colour encoding, cbreak mode, and an
//! alternate-screen writer that re-sends only the cells that changed.

use std::io::Write as _;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

pub type Rgb = [u8; 3];

/// Foreground, background and bold packed into one integer: the screen diff
/// compares every cell of every frame, and this makes each a single compare.
/// Bits 0-23 fg RGB, 24 fg set, 25-48 bg RGB, 49 bg set, 50 bold.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Style(u64);

pub const PLAIN: Style = Style(0);

impl Style {
    const FG: u64 = 1 << 24;
    const BG: u64 = 1 << 49;
    const BOLD: u64 = 1 << 50;

    const fn pack(c: Rgb) -> u64 {
        (c[0] as u64) << 16 | (c[1] as u64) << 8 | c[2] as u64
    }

    const fn unpack(v: u64) -> Rgb {
        [(v >> 16) as u8, (v >> 8) as u8, v as u8]
    }

    pub const fn fg(c: Rgb) -> Self {
        Style(Self::pack(c) | Self::FG)
    }

    pub const fn bg(c: Rgb) -> Self {
        Style(Self::pack(c) << 25 | Self::BG)
    }

    /// this style on background `c`
    pub const fn on(self, c: Rgb) -> Self {
        Style(self.0 & !(0x1FF_FFFF << 25) | Self::bg(c).0)
    }

    pub const fn bold(self) -> Self {
        Style(self.0 | Self::BOLD)
    }

    fn fg_rgb(self) -> Option<Rgb> {
        (self.0 & Self::FG != 0).then(|| Self::unpack(self.0))
    }

    fn bg_rgb(self) -> Option<Rgb> {
        (self.0 & Self::BG != 0).then(|| Self::unpack(self.0 >> 25))
    }

    fn is_bold(self) -> bool {
        self.0 & Self::BOLD != 0
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

/// Decimal digits without the fmt machinery: a frame can carry thousands of
/// these (gradient meters change colour every cell).
fn push_num(buf: &mut String, n: usize) {
    if n >= 10 {
        push_num(buf, n / 10);
    }
    buf.push((b'0' + (n % 10) as u8) as char);
}

fn color(buf: &mut String, c: Rgb, fg: bool, cs: Colors) {
    buf.push_str(if fg { "38;" } else { "48;" });
    if cs == Colors::True {
        buf.push_str("2;");
        for (k, v) in c.into_iter().enumerate() {
            if k > 0 {
                buf.push(';');
            }
            push_num(buf, v as usize);
        }
    } else {
        buf.push_str("5;");
        push_num(buf, ansi256(c) as usize);
    }
}

fn move_to(buf: &mut String, y: usize, x: usize) {
    buf.push_str("\x1b[");
    push_num(buf, y + 1);
    buf.push(';');
    push_num(buf, x + 1);
    buf.push('H');
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
    let (fg, bg) = (want.fg_rgb(), want.bg_rgb());
    if (cur.is_bold() && !want.is_bold())
        || (cur.fg_rgb().is_some() && fg.is_none())
        || (cur.bg_rgb().is_some() && bg.is_none())
    {
        buf.push('0');
        *cur = PLAIN;
    }
    if fg != cur.fg_rgb()
        && let Some(c) = fg
    {
        sep(buf);
        color(buf, c, true, cs);
    }
    if bg != cur.bg_rgb()
        && let Some(c) = bg
    {
        sep(buf);
        color(buf, c, false, cs);
    }
    if want.is_bold() && !cur.is_bold() {
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

/// One key, unechoed: y / Y? Ctrl-C is just a key here (no).
pub fn ask_yes() -> bool {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) != 0 {
            return false;
        }
        let old = t;
        t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(0, libc::TCSAFLUSH, &t); // keys typed before the question don't answer it
        let mut b = 0u8;
        let n = loop {
            let n = libc::read(0, (&raw mut b).cast(), 1);
            if n >= 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                break n;
            }
        };
        libc::tcsetattr(0, libc::TCSANOW, &old);
        n == 1 && b.eq_ignore_ascii_case(&b'y')
    }
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

    pub fn draw(&mut self, rows: Vec<Vec<Cell>>, w: usize, h: usize) {
        let out = self.diff(rows, w, h);
        if !out.is_empty() {
            write_out(&out);
        }
    }

    /// Diff against the previous frame per cell: only the changed runs of each
    /// row are sent, so an easing bar costs a few cells, not a row. "" = nothing changed.
    pub fn diff(&mut self, rows: Vec<Vec<Cell>>, w: usize, h: usize) -> String {
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
            let runs = match old {
                Some(o) => diff_runs(o, row),
                None => vec![(0, row.len())],
            };
            for (a, b) in runs {
                move_to(&mut buf, y, a);
                for c in &row[a..b] {
                    if c.ch != WIDE_TAIL {
                        sgr(&mut buf, &mut cur, c.st, self.cs);
                        buf.push(c.ch);
                    }
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
                move_to(&mut buf, y, 0);
                buf.push_str("\x1b[2K");
            }
        }
        self.prev = rows;
        if buf.is_empty() {
            return buf;
        }
        // synchronized update (DEC 2026): the terminal shows the frame atomically
        format!("\x1b[?2026h\x1b[0m{buf}\x1b[0m\x1b[?2026l")
    }
}

/// Unchanged stretches of at least this many cells between two changes (e.g.
/// the text between two easing meters) are jumped over with a cursor move,
/// shorter ones are resent. 3 sends the fewest bytes in bench_frame: resent
/// cells often carry a colour code.
const SKIP: usize = 3;

/// Changed cells of a row as half-open runs, each widened so it never starts
/// on the right half of a double-width character.
pub fn diff_runs(old: &[Cell], new: &[Cell]) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (i, (x, y)) in old.iter().zip(new).enumerate() {
        if x == y {
            continue;
        }
        match runs.last_mut() {
            Some(r) if i - r.1 < SKIP => r.1 = i + 1,
            _ => {
                let a = if x.ch == WIDE_TAIL || y.ch == WIDE_TAIL { i.saturating_sub(1) } else { i };
                runs.push((a, i + 1));
            }
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthChar;

    fn row(s: &str) -> Vec<Cell> {
        s.chars().map(|ch| Cell { ch, st: PLAIN }).collect()
    }

    #[test]
    fn runs_cover_only_changes() {
        assert_eq!(diff_runs(&row("abcdef"), &row("abXdeY")), [(2, 6)]); // short gap: one run
        assert_eq!(diff_runs(&row("abc"), &row("abc")), []);
        assert_eq!(diff_runs(&row("a中\0b"), &row("a国\0b")), [(1, 2)]);
        assert_eq!(diff_runs(&row("a.........b"), &row("X.........Y")), [(0, 1), (10, 11)]); // long gap: skipped
        let mut tail_only = row("a中\0");
        tail_only[2].st = Style::fg([9, 9, 9]);
        assert_eq!(diff_runs(&row("a中\0"), &tail_only), [(1, 3)]); // never start on a wide char's right half
    }

    /// Replays `diff` output on a minimal terminal (CUP, SGR, ED/EL, text) and
    /// checks it ends up showing exactly the frame, over frames that change a
    /// little, a lot, in wide characters and in colour only.
    #[test]
    fn diff_replays_to_the_frame() {
        let (w, h) = (24, 3);
        let mut grid = vec![vec![BLANK; w]; h];
        let mut screen = Screen::new(Colors::True);
        let mut seed = 7u32;
        let mut rnd = |n: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) % n
        };
        let tok = |rnd: &mut dyn FnMut(u32) -> u32| {
            let st = Style::fg([rnd(2) as u8 * 200, 0, 0]).on([0, rnd(2) as u8 * 90, 0]);
            (['.', 'x', '中'][rnd(3) as usize], st)
        };
        let mut rows: Vec<Vec<(char, Style)>> = vec![Vec::new(); h];
        for f in 0..2000 {
            // a few tokens change per frame, sometimes all of them
            for r in rows.iter_mut() {
                if f % 50 == 0 {
                    *r = (0..w).map(|_| tok(&mut rnd)).collect();
                }
                for _ in 0..rnd(4) {
                    let k = rnd(w as u32) as usize;
                    r[k] = tok(&mut rnd);
                }
            }
            let frame: Vec<Vec<Cell>> = rows
                .iter()
                .map(|toks| {
                    let mut r = Vec::new();
                    for &(ch, st) in toks {
                        match ch {
                            '中' if r.len() + 2 <= w => r.extend([Cell { ch, st }, Cell { ch: WIDE_TAIL, st }]),
                            '中' => r.push(Cell { ch: '.', st }),
                            _ => r.push(Cell { ch, st }),
                        }
                    }
                    r.truncate(w);
                    if r.last().is_some_and(|c| UnicodeWidthChar::width(c.ch) == Some(2)) {
                        r[w - 1].ch = '.'; // a wide char cut in half at the edge
                    }
                    r
                })
                .collect();
            let out = screen.diff(frame.clone(), w, h);
            let (mut y, mut x, mut st, mut s) = (0, 0, PLAIN, out.as_str());
            while let Some(c) = s.chars().next() {
                if c != '\x1b' {
                    grid[y][x] = Cell { ch: c, st };
                    if UnicodeWidthChar::width(c) == Some(2) {
                        grid[y][x + 1] = Cell { ch: WIDE_TAIL, st };
                        x += 1;
                    }
                    x += 1;
                    s = &s[c.len_utf8()..];
                    continue;
                }
                let end = s.find(|c: char| c.is_ascii_alphabetic()).unwrap();
                let (params, fin) = (&s[2..end], &s[end..=end]);
                let n: Vec<u32> = params.split(';').filter_map(|p| p.parse().ok()).collect();
                match fin {
                    "H" => (y, x) = (n[0] as usize - 1, n[1] as usize - 1),
                    "J" => grid.iter_mut().for_each(|r| r.fill(BLANK)),
                    "m" => {
                        let mut k = 0;
                        while k < n.len() {
                            match n[k] {
                                0 => st = PLAIN,
                                1 => st = st.bold(),
                                38 => {
                                    (st, k) = (
                                        Style(
                                            st.0 & !0x1FF_FFFF
                                                | Style::fg([n[k + 2], n[k + 3], n[k + 4]].map(|v| v as u8)).0,
                                        ),
                                        k + 4,
                                    )
                                }
                                48 => (st, k) = (st.on([n[k + 2], n[k + 3], n[k + 4]].map(|v| v as u8)), k + 4),
                                _ => panic!("unexpected SGR {}", n[k]),
                            }
                            k += 1;
                        }
                    }
                    _ => {} // ?2026h/l, K: no effect on a full-width frame
                }
                s = &s[end + 1..];
            }
            assert_eq!(grid, frame);
        }
    }

    #[test]
    fn style_packs_and_encodes() {
        let st = Style::fg([1, 2, 3]).on([250, 251, 252]).bold();
        assert_eq!((st.fg_rgb(), st.bg_rgb(), st.is_bold()), (Some([1, 2, 3]), Some([250, 251, 252]), true));
        assert_eq!(Style::fg([9, 9, 9]).on([1, 1, 1]).on([2, 2, 2]).bg_rgb(), Some([2, 2, 2]));
        let row = [Cell { ch: 'x', st }];
        assert_eq!(encode(&row, Colors::True), "\x1b[38;2;1;2;3;48;2;250;251;252;1mx\x1b[0m");
    }

    #[test]
    fn ansi256_maps_extremes() {
        assert_eq!(ansi256([0, 0, 0]), 16);
        assert_eq!(ansi256([255, 255, 255]), 231);
        assert_eq!(ansi256([255, 0, 0]), 196);
    }
}
