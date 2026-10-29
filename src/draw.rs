//! Rendering: styled lines with click zones, meters, host cards and panels.
//!
//! Every frame is rebuilt from scratch (it takes microseconds); `Screen` in
//! term.rs diffs cells, so only what visibly changed reaches the terminal.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::Instant;

use unicode_width::UnicodeWidthChar;

use crate::app::Act;
use crate::collect::{Gpu, HISTORY, Host, Proc, Sample, Status, UNKNOWN};
use crate::settings::{AUTO, AUTO_TICK_MS, SORTS, Settings, fmt_interval};
use crate::term::{BLANK, Cell, PLAIN, Style, WIDE_TAIL};
use crate::theme::Theme;

pub const FPS: f64 = 30.0; // while something is animating; every frame is a terminal redraw
const TWEEN_TAU: f64 = 0.07; // seconds; easing time constant: settled ~0.3 s after a sample
const PULSE_SECS: f64 = 0.5; // status dot glows after each fresh sample (intervals of 1 s and up)
const CARD_MIN_WIDTH: usize = 80; // narrower terminals get fewer card columns
const PROC_MAX: usize = 8; // process rows per host card

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const BRAILLE_L: [u32; 5] = [0x00, 0x40, 0x44, 0x46, 0x47]; // left column, filled bottom-up
const BRAILLE_R: [u32; 5] = [0x00, 0x80, 0xA0, 0xB0, 0xB8]; // right column, filled bottom-up

const PCT_W: usize = 5;
const MEM_W: usize = 11;
const USER_W: usize = 10;
const TEMP_W: usize = 4;
const PWR_W: usize = 5;
const GAP: usize = 2;

// tween slots per host besides gpu*4 + {0 util, 1 vram, 2 dot}
const K_CPU: u32 = u32::MAX;
const K_RAM: u32 = u32::MAX - 1;
const K_DOT: u32 = u32::MAX - 2;
const K_CACHE: u32 = u32::MAX - 3;

// ── lines with click zones ───────────────────────────────────────────────────
fn char_w(c: char) -> usize {
    if c.is_control() { 1 } else { c.width().unwrap_or(1) }
}

pub fn str_w(s: &str) -> usize {
    if s.is_ascii() { s.len() } else { s.chars().map(char_w).sum() }
}

/// At most `width` cells of `s`, no ellipsis.
fn cut(s: &str, width: usize) -> String {
    let mut w = 0;
    s.chars()
        .take_while(|&c| {
            w += char_w(c);
            w <= width
        })
        .collect()
}

fn crop(s: &str, width: usize) -> String {
    if str_w(s) <= width {
        return s.into();
    }
    if width == 0 {
        return String::new();
    }
    let out = cut(s, width - 1);
    let pad = width - 1 - str_w(&out);
    format!("{out}{}…", " ".repeat(pad))
}

fn ljust(s: &str, w: usize) -> String {
    format!("{s}{}", " ".repeat(w.saturating_sub(str_w(s))))
}

fn rjust(s: &str, w: usize) -> String {
    format!("{}{s}", " ".repeat(w.saturating_sub(str_w(s))))
}

/// One terminal row: cells, plus click zones [(x0, x1, action)].
#[derive(Clone, Default)]
pub struct Line {
    pub cells: Vec<Cell>,
    pub zones: Vec<(usize, usize, Act)>,
    pub optional: bool, // a column header: dropped when the cards don't fit the screen
}

fn line(s: &str, st: Style) -> Line {
    let mut l = Line::new();
    l.add(s, st);
    l
}

impl Line {
    /// Room for a full screen row up front: rows grow cell by cell (and card
    /// rows get the next column appended), so growing from empty would
    /// reallocate and copy ~6 times per row.
    pub fn new() -> Self {
        Self::with_capacity(256)
    }

    pub fn with_capacity(n: usize) -> Self {
        Line { cells: Vec::with_capacity(n), zones: Vec::new(), optional: false }
    }

    pub fn w(&self) -> usize {
        self.cells.len()
    }

    pub fn add(&mut self, s: &str, st: Style) -> &mut Self {
        if s.is_ascii() {
            // fast path for the bulk of the text: one cell per byte, no width table
            self.cells.extend(s.bytes().map(|b| Cell { ch: if b < 0x20 || b == 0x7f { '?' } else { b as char }, st }));
            return self;
        }
        for ch in s.chars() {
            // control characters from remote data must never reach the terminal
            let (ch, w) = if ch.is_control() { ('?', 1) } else { (ch, ch.width().unwrap_or(1)) };
            if w == 0 {
                continue; // combining marks: dropped
            }
            self.cells.push(Cell { ch, st });
            if w == 2 {
                self.cells.push(Cell { ch: WIDE_TAIL, st });
            }
        }
        self
    }

    pub fn act(&mut self, s: &str, st: Style, a: Act) -> &mut Self {
        let x = self.w();
        self.add(s, st);
        if self.w() > x {
            self.zones.push((x, self.w(), a));
        }
        self
    }

    pub fn fill(&mut self, n: usize, ch: char, st: Style) -> &mut Self {
        self.cells.extend(std::iter::repeat_n(Cell { ch, st }, n));
        self
    }

    pub fn pad(&mut self, n: usize) -> &mut Self {
        self.cells.extend(std::iter::repeat_n(BLANK, n));
        self
    }

    pub fn extend(&mut self, o: &Line) -> &mut Self {
        let off = self.w();
        self.zones.extend(o.zones.iter().map(|&(a, b, act)| (a + off, b + off, act)));
        self.cells.extend_from_slice(&o.cells);
        self
    }

    /// Pad, or crop with an ellipsis, to exactly `width` cells.
    pub fn fit(&mut self, width: usize) -> &mut Self {
        let n = self.w();
        if n <= width {
            return self.pad(width - n);
        }
        self.zones.retain_mut(|z| {
            z.1 = z.1.min(width);
            z.0 < width
        });
        if width == 0 {
            self.cells.clear();
            return self;
        }
        let split_wide = self.cells[width - 1].ch == WIDE_TAIL;
        let st = self.cells[width - 1].st;
        self.cells.truncate(width - 1);
        if split_wide {
            self.cells[width - 2].ch = ' ';
        }
        self.cells.push(Cell { ch: '…', st });
        self
    }
}

// ── easing ───────────────────────────────────────────────────────────────────
/// Exponential easing of displayed values toward their targets. A value
/// advances at most once per frame, however often it is read.
#[derive(Default)]
pub struct Tweener {
    vals: HashMap<(usize, u32), (f64, u64)>,
    alpha: f64,
    moving: bool,
    frame: u64,
}

impl Tweener {
    fn begin(&mut self, dt: Option<f64>, tau: f64) {
        self.alpha = dt.map_or(1.0, |d| 1.0 - (-d.max(0.0) / tau).exp());
        self.moving = false;
        self.frame += 1;
    }

    /// `start` is where a value seen for the first time begins (bars grow in from 0).
    fn ease(&mut self, key: (usize, u32), target: f64, start: f64) -> f64 {
        let prev = self.vals.get(&key).copied();
        if let Some((v, f)) = prev
            && f == self.frame
        {
            self.moving |= v != target;
            return v;
        }
        let mut cur = prev.map_or(start, |p| p.0);
        if self.alpha < 1.0 {
            cur += (target - cur) * self.alpha;
            if (target - cur).abs() < 0.4 {
                cur = target;
            } else {
                self.moving = true;
            }
        } else {
            cur = target;
        }
        self.vals.insert(key, (cur, self.frame));
        cur
    }
}

// ── small formatters and glyph helpers ───────────────────────────────────────
/// f64 with no decimals. `{:.0}` / `{:.1}` on floats take std's exact
/// (dragon4, bignum) path for many values; via integers it is a few ns.
/// Width / alignment flags work as usual.
struct D0(f64);
/// f64 with one decimal.
struct D1(f64);

impl std::fmt::Display for D0 {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Display::fmt(&(self.0.round() as i64), f) // also: -0.0 prints as 0
    }
}

impl std::fmt::Display for D1 {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let t = (self.0 * 10.0).round() as i64;
        let (mut b, mut i, mut n) = ([0u8; 24], 24, t.unsigned_abs());
        i -= 2;
        (b[i], b[i + 1]) = (b'.', b'0' + (n % 10) as u8);
        n /= 10;
        loop {
            i -= 1;
            b[i] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        if t < 0 {
            i -= 1;
            b[i] = b'-';
        }
        f.pad(std::str::from_utf8(&b[i..]).unwrap_or("?"))
    }
}
struct Ctx<'a> {
    th: &'a Theme,
    st: &'a Settings,
    now: Instant,
    secs: f64, // monotonic seconds, for the spinner phase
    live: bool,
    paused: bool,
    stale_after: f64,
    iv: f64,   // refresh interval, seconds
    ng: usize, // most GPUs on any host: folded cards pad their strips to it
}

fn since(now: Instant, t: Instant) -> f64 {
    now.saturating_duration_since(t).as_secs_f64()
}

fn spin(secs: f64) -> char {
    SPINNER[(secs * 12.0) as usize % SPINNER.len()]
}

fn fmt_pct(v: Option<f64>) -> String {
    v.map_or("    -".into(), |v| format!("{:>4}%", D0(v)))
}

fn fmt_age(sec: Option<u64>) -> String {
    match sec {
        None => "-".into(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        Some(s) if s < 86400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        Some(s) => format!("{}d{:02}h", s / 86400, s % 86400 / 3600),
    }
}

/// [0, 1, 2, 3, 6] -> "0-3,6"
fn fmt_gpus(ix: &[u32]) -> String {
    if ix.is_empty() {
        return "?".into();
    }
    let mut out: Vec<String> = Vec::new();
    let (mut start, mut prev) = (ix[0], ix[0]);
    for &i in &ix[1..] {
        if i == prev + 1 {
            prev = i;
            continue;
        }
        out.push(if start == prev { start.to_string() } else { format!("{start}-{prev}") });
        (start, prev) = (i, i);
    }
    out.push(if start == prev { start.to_string() } else { format!("{start}-{prev}") });
    out.join(",")
}

fn thousands(v: f64) -> String {
    let s = D0(v).to_string();
    let (sign, d) = s.strip_prefix('-').map_or(("", s.as_str()), |d| ("-", d));
    let mut out = String::from(sign);
    for (k, c) in d.chars().enumerate() {
        if k > 0 && (d.len() - k) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn short_model(name: &str) -> &str {
    let mut n = name;
    for pre in ["NVIDIA ", "GeForce ", "Tesla "] {
        n = n.strip_prefix(pre).unwrap_or(n);
    }
    n
}

/// "/opt/conda/bin/python train.py" -> "python train.py"
fn short_cmd(cmd: &str) -> String {
    let (head, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
    let head = if head.starts_with('/') { head.rsplit('/').next().unwrap_or(head) } else { head };
    if rest.is_empty() { head.into() } else { format!("{head} {rest}") }
}

/// GiB, one decimal below 10.
fn fmt_gib(kb: u64) -> String {
    let g = kb as f64 / 1048576.0;
    if g < 10.0 { format!("{}G", D1(g)) } else { format!("{}G", D0(g)) }
}

/// process RSS
fn fmt_kb(kb: u64) -> String {
    match kb {
        1_048_576_000.. => format!("{}T", D1(kb as f64 / 1_073_741_824.0)),
        1_048_576.. => format!("{}G", D1(kb as f64 / 1_048_576.0)),
        _ => format!("{}M", kb / 1024),
    }
}

/// %CPU, where 100 = one core
fn fmt_cpu(c: f64) -> String {
    if c >= 99.95 { D0(c).to_string() } else { D1(c).to_string() }
}

fn fmt_uptime(secs: f64) -> String {
    let s = secs as u64;
    match s {
        86400.. => format!("up {}d", s / 86400),
        3600.. => format!("up {}h", s / 3600),
        _ => format!("up {}m", s / 60),
    }
}

/// "INTEL(R) XEON(R) PLATINUM 8558" -> "XEON PLATINUM 8558",
/// "AMD EPYC 9654 96-Core Processor" -> "EPYC 9654"
fn short_cpu(model: &str) -> String {
    let m = model.split('@').next().unwrap_or(model);
    let m = m.replace("(R)", "").replace("(r)", "").replace("(TM)", "").replace("(tm)", "");
    m.split_whitespace()
        .filter(|w| {
            let l = w.to_ascii_lowercase();
            !matches!(l.as_str(), "intel" | "amd" | "cpu" | "processor") && !l.ends_with("-core")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Filled length of a meter in 1/8 cells (what is actually visible).
fn bar_units(pct: f64, width: usize) -> usize {
    (pct.clamp(0.0, 100.0) * width as f64 * 8.0 / 100.0).round() as usize
}

/// Gradient meter with 1/8-cell resolution on a tinted track.
fn put_bar(ln: &mut Line, pct: f64, width: usize, th: &Theme) {
    if width == 0 {
        return;
    }
    let (full, rem) = (bar_units(pct, width) / 8, bar_units(pct, width) % 8);
    let span = (width - 1).max(1);
    for i in 0..full {
        ln.cells.push(Cell { ch: '█', st: th.bar(i * 100 / span) });
    }
    let mut used = full;
    if rem > 0 {
        ln.cells.push(Cell { ch: EIGHTHS[rem], st: th.bar(full * 100 / span) });
        used += 1;
    }
    ln.fill(width - used, ' ', th.track);
}

/// RAM meter: used memory on the gradient, page cache after it in a muted tone.
fn put_mem_bar(ln: &mut Line, used: f64, cache: f64, width: usize, th: &Theme) {
    let uu = bar_units(used, width);
    let cu = bar_units(used + cache, width).max(uu);
    let span = width.saturating_sub(1).max(1);
    for i in 0..width {
        let (lo, pct) = (i * 8, i * 100 / span);
        let (ch, st) = if lo + 8 <= uu {
            ('█', th.bar(pct))
        } else if lo < uu {
            (EIGHTHS[uu - lo], th.bar_on(pct, cu >= lo + 8))
        } else if lo + 4 <= cu {
            ('█', th.cache_cell())
        } else {
            (' ', th.track)
        };
        ln.cells.push(Cell { ch, st });
    }
}

fn spark_lvl(v: Option<f64>) -> usize {
    match v {
        Some(v) if v >= 1.0 => ((v / 25.0).ceil() as usize).min(4),
        _ => 0,
    }
}

/// Braille sparkline, two samples per cell, newest on the right.
fn put_spark(ln: &mut Line, hist: Option<&VecDeque<f32>>, width: usize, th: &Theme) {
    let need = width * 2;
    let empty = VecDeque::new();
    let hist = hist.unwrap_or(&empty);
    let skip = hist.len().saturating_sub(need);
    let vals: Vec<Option<f64>> = std::iter::repeat_n(None, need - (hist.len() - skip))
        .chain(hist.iter().skip(skip).map(|&v| Some(v as f64)))
        .collect();
    for p in vals.chunks(2) {
        let (a, b) = (p[0], p[1]);
        if a.is_none() && b.is_none() {
            ln.pad(1);
            continue;
        }
        let (la, lb) = (spark_lvl(a), spark_lvl(b));
        if la == 0 && lb == 0 {
            ln.add("⣀", th.baseline);
            continue;
        }
        let ch = char::from_u32(0x2800 | BRAILLE_L[la] | BRAILLE_R[lb]).unwrap_or('?');
        ln.cells.push(Cell { ch, st: th.level(Some(a.unwrap_or(0.0).max(b.unwrap_or(0.0)))) });
    }
}

/// One cell per GPU: block height and colour by value.
fn put_strip(ln: &mut Line, vals: impl Iterator<Item = Option<f64>>, th: &Theme) {
    for v in vals {
        let (ch, st) = match v {
            None => ('·', th.dim),
            Some(v) if v < 1.0 => ('▁', th.baseline),
            Some(v) => (BLOCKS[((v * 8.0 / 100.01) as usize).min(7)], th.level(Some(v))),
        };
        ln.cells.push(Cell { ch, st });
    }
}

fn ram_pct(s: &Sample) -> Option<f64> {
    let t = s.mem_total_kb as f64;
    (t > 0.0).then(|| 100.0 * (t - s.mem_avail_kb as f64) / t)
}

fn cache_pct(s: &Sample) -> f64 {
    let t = s.mem_total_kb as f64;
    if t > 0.0 { (100.0 * s.mem_cache_kb as f64 / t).min(100.0 - ram_pct(s).unwrap_or(0.0)).max(0.0) } else { 0.0 }
}

/// (1-minute load, cores)
fn load_of(s: &Sample) -> Option<(f64, u32)> {
    match (s.load, s.cores) {
        (Some(l), Some(n)) if n > 0 => Some((l[0], n)),
        _ => None,
    }
}

fn free_gib(gpus: &[Gpu]) -> f64 {
    gpus.iter().map(|g| g.mem_total - g.mem_used).sum::<f64>() / 1024.0
}

fn host_state(gpus: &[Gpu]) -> usize {
    gpus.iter().map(Gpu::state).min().unwrap_or(2)
}

/// 1 right after a fresh sample, fading out (ease-out) over PULSE_SECS, in
/// 8 steps: a continuous fade would redraw the terminal on every frame.
fn glow(age: f64) -> f64 {
    ((1.0 - age / PULSE_SECS).max(0.0).powi(2) * 8.0).round() / 8.0
}

fn status_dot(cx: &Ctx, tw: &mut Tweener, i: usize, h: &Host) -> (char, Style) {
    let th = cx.th;
    if h.status == Status::Connecting {
        return (spin(cx.secs), th.accent);
    }
    let s = match &h.sample {
        Some(s) if h.status != Status::Error => s,
        _ => return ('×', th.err),
    };
    if s.gpus.is_empty() {
        return ('●', th.warn);
    }
    let age = since(cx.now, h.updated);
    if cx.live && !cx.paused && age > cx.stale_after {
        return ('●', th.warn);
    }
    let hs = host_state(&s.gpus);
    if !cx.live {
        return ('●', th.state[hs]);
    }
    let v = tw.ease((i, K_DOT), hs as f64 * 50.0, hs as f64 * 50.0);
    ('●', th.dot(v / 50.0, if cx.iv >= 1.0 { glow(age) } else { 0.0 })) // faster: it would never fade
}

// ── card content ─────────────────────────────────────────────────────────────
struct Cols {
    prefix: usize,
    idx_w: usize,
    name_w: usize,
    bar_u: usize,
    bar_m: usize,
    user: usize,
    spark: usize,
}

fn plan_cols(cw: usize, gpus: &[Gpu], hist: bool, user: bool) -> Cols {
    let idx_w = gpus.iter().map(|g| g.idx).max().unwrap_or(0).to_string().len();
    let name_w = if gpus.iter().map(|g| &g.name).collect::<BTreeSet<_>>().len() > 1 { 11 } else { 0 };
    let prefix = 2 + idx_w + 1 + name_w;
    let mut flex = cw as isize - (prefix + PCT_W + GAP + MEM_W + GAP + TEMP_W + 1 + PWR_W) as isize;
    let mut uw = 0;
    if user && flex - (USER_W + GAP) as isize >= 16 {
        uw = USER_W;
        flex -= (USER_W + GAP) as isize;
    }
    let mut spark = 0;
    if hist && flex >= 30 {
        spark = ((flex as usize - GAP) * 3 / 10).min(HISTORY / 2);
        flex -= (spark + GAP) as isize;
    }
    let flex = flex.max(0) as usize;
    Cols { prefix, idx_w, name_w, bar_u: flex / 2, bar_m: flex - flex / 2, user: uw, spark }
}

/// Two host rows, column-aligned with the GPU rows below:
///   CPU meter | RAM meter (used + cache) | swap | CPU history | CPU temperature
///   per-core strip (busiest core of each cell)            load 1 / 5 / 15 min
/// Numbers ease together with their meters.
#[allow(clippy::too_many_arguments)]
fn sys_rows(
    s: &Sample,
    hist: &VecDeque<f32>,
    c: &Cols,
    cw: usize,
    th: &Theme,
    cpu: f64,
    used: f64,
    cache: f64,
    cores_row: bool,
) -> Vec<Line> {
    let mut ln = line(&ljust("CPU", c.prefix), th.dim);
    put_bar(&mut ln, cpu, c.bar_u, th);
    let cpu = s.cpu_pct.map(|_| cpu);
    ln.add(&fmt_pct(cpu), th.level(cpu)).add("  ", PLAIN).add("RAM ", th.dim);
    put_mem_bar(&mut ln, used, cache, c.bar_m.saturating_sub(4), th);
    let tot = s.mem_total_kb as f64 / 1048576.0;
    let txt = if tot > 0.0 { format!("{}/{}G", D0(used / 100.0 * tot), D0(tot)) } else { "-".into() };
    ln.add(&rjust(&txt, MEM_W), PLAIN);
    let n = s.cores.unwrap_or(1).max(1);
    let load_st = |l: f64| th.level(Some(l / n as f64 / 1.2 * 100.0));
    if c.user > 0 {
        ln.add("  ", PLAIN);
        // 1-minute load / cores, e.g. "40.9/192c"
        let mut cell = Line::new();
        if let Some(l) = s.load {
            cell.add(&D1(l[0]).to_string(), load_st(l[0])).add(&format!("/{n}c"), th.dim);
        }
        ln.extend(cell.fit(c.user));
    }
    if c.spark > 0 {
        ln.add("  ", PLAIN);
        put_spark(&mut ln, Some(hist), c.spark, th);
    }
    ln.add("  ", PLAIN);
    match s.cpu_temp {
        Some(t) => ln.add(&format!("{:>3}°", D0(t)), th.temp(t)),
        None => ln.add(&rjust("-", TEMP_W), th.dim),
    };
    if !cores_row {
        return vec![ln];
    }

    let swap_used = s.swap_total_kb.saturating_sub(s.swap_free_kb);
    let label = s.cores.map_or(String::new(), |n| format!("{n}c"));
    let mut r2 = line(&ljust(&label, c.prefix.max(label.len() + 1)), th.dim);
    let mut right = Line::default();
    if let Some(l) = s.load {
        right.add("load ", th.dim).add(&D1(l[0]).to_string(), load_st(l[0]));
        right.add(&format!(" {} {}", D1(l[1]), D1(l[2])), th.dim);
    }
    if s.swap_total_kb > 0 {
        let st = if swap_used > s.swap_total_kb / 4 { th.warn } else { th.dim }; // a quarter used: worth a look
        right.add("  swap ", th.dim).add(&fmt_gib(swap_used), st);
    }
    let room = cw.saturating_sub(r2.w() + right.w() + 2);
    if !s.core_pct.is_empty() && room >= 8 {
        let g = s.core_pct.len().div_ceil(room);
        put_strip(&mut r2, s.core_pct.chunks(g).map(|ch| Some(ch.iter().fold(0.0f32, |a, &b| a.max(b)) as f64)), th);
    }
    r2.pad(cw.saturating_sub(r2.w() + right.w())).extend(&right);
    vec![ln, r2]
}

fn gpu_header(c: &Cols, th: &Theme) -> Line {
    let mut s =
        format!("  {:>w$} {}", "#", if c.name_w > 0 { ljust("MODEL", c.name_w) } else { String::new() }, w = c.idx_w);
    s += &ljust("UTIL", c.bar_u + PCT_W);
    s += "  ";
    s += &ljust("VRAM", c.bar_m + MEM_W);
    if c.user > 0 {
        s += "  ";
        s += &ljust("USER", c.user);
    }
    if c.spark > 0 {
        s += "  ";
        s += &cut(&ljust("HISTORY", c.spark), c.spark);
    }
    s += &format!("  {:>TEMP_W$} {:>PWR_W$}", "TEMP", "POWER");
    line(&s, th.dim)
}

fn user_cell(ln: &mut Line, users: &[String], me: &str, known: bool, width: usize, th: &Theme) {
    if !known {
        ln.pad(width);
        return;
    }
    let Some(first) = users.first() else {
        ln.add(&ljust("-", width), th.dim);
        return;
    };
    let more = if users.len() > 1 { format!("+{}", users.len() - 1) } else { String::new() };
    let name = crop(first, width.saturating_sub(more.len()));
    let st = if first == me {
        th.me
    } else if first == UNKNOWN {
        th.dim
    } else {
        th.fg
    };
    ln.add(&name, st);
    ln.add(&more, if users[1..].iter().any(|u| u == me) { th.me } else { th.dim });
    ln.pad(width.saturating_sub(str_w(&name) + more.len()));
}

#[allow(clippy::too_many_arguments)]
fn gpu_row(
    g: &Gpu,
    hist: Option<&VecDeque<f32>>,
    c: &Cols,
    th: &Theme,
    u: f64,
    m: f64,
    dot: Style,
    me: &str,
    known: bool,
) -> Line {
    let mut ln = line("● ", dot);
    ln.add(&format!("{:>w$} ", g.idx, w = c.idx_w), PLAIN);
    if c.name_w > 0 {
        ln.add(&ljust(&cut(short_model(&g.name), c.name_w - 1), c.name_w), th.dim);
    }
    put_bar(&mut ln, u, c.bar_u, th);
    let ut = g.util.map(|_| u);
    ln.add(&fmt_pct(ut), th.level(ut)).add("  ", PLAIN);
    put_bar(&mut ln, m, c.bar_m, th);
    let used = m / 100.0 * g.mem_total / 1024.0;
    ln.add(&rjust(&format!("{}/{}G", D1(used), D0(g.mem_total / 1024.0)), MEM_W), PLAIN);
    if c.user > 0 {
        ln.add("  ", PLAIN);
        user_cell(&mut ln, &g.users, me, known, c.user, th);
    }
    if c.spark > 0 {
        ln.add("  ", PLAIN);
        put_spark(&mut ln, hist, c.spark, th);
    }
    ln.add("  ", PLAIN);
    match g.temp {
        None => ln.add(&rjust("-", TEMP_W), th.dim),
        Some(t) => ln.add(&format!("{:>3}°", D0(t)), th.temp(t)),
    };
    ln.add(" ", PLAIN);
    match g.power {
        None => ln.add(&rjust("-", PWR_W), th.dim),
        Some(p) => {
            let ratio = g.power_limit.filter(|&l| l > 0.0).map_or(0.0, |l| 100.0 * p / l);
            ln.add(&format!("{:>4}W", D0(p)), th.level(Some(ratio)))
        }
    };
    ln
}

fn fmt_vram(mem: f64) -> String {
    if mem != 0.0 { format!("{:>5}G", D1(mem / 1024.0)) } else { "     -".into() }
}

fn proc_section(s: &Sample, cw: usize, th: &Theme) -> Vec<Line> {
    let mut rule = Line::default();
    rule.fill(cw, '─', th.border);
    let mut out = vec![rule];
    let Some(procs) = &s.procs else {
        out.push(line("processes: waiting for the next sample…", th.dim));
        return out;
    };
    let Some(last) = procs.last() else {
        out.push(line("no GPU processes", th.dim));
        return out;
    };
    let mut shown: Vec<&Proc> = procs.iter().take(PROC_MAX).collect();
    if last.pid == 0 && procs.len() > PROC_MAX {
        shown.pop(); // keep the hidden-process summary visible
        shown.push(last);
    }
    let gl: Vec<String> = shown.iter().map(|p| fmt_gpus(&p.gpus)).collect();
    let pid_w = shown.iter().map(|p| p.pid.to_string().len()).max().unwrap_or(0).max(3);
    let user_w = shown.iter().map(|p| str_w(&p.user)).max().unwrap_or(0).clamp(4, 12);
    let gpu_w = gl.iter().map(String::len).max().unwrap_or(0).clamp(3, 11);
    let host = shown.iter().any(|p| p.rss_kb.is_some()); // agent.py also reports host CPU / RAM
    let mut h = format!("{:>pid_w$}  {:<user_w$}  {:<gpu_w$}  {:>6}  ", "PID", "USER", "GPU", "VRAM");
    if host {
        h += &format!("{:>5}  {:>6}  ", "%CPU", "RSS");
    }
    let mut head = line(&(h + &format!("{:>6}  COMMAND", "TIME")), th.dim);
    head.optional = true;
    out.push(head);
    for (p, g) in shown.iter().zip(&gl) {
        let mut ln = Line::default();
        let gpus = ljust(&crop(g, gpu_w), gpu_w) + "  ";
        if p.pid == 0 {
            ln.add(&format!("{:>pid_w$}  {:<user_w$}  ", "-", UNKNOWN), th.dim).add(&gpus, th.dim);
            ln.add(&fmt_vram(p.mem), th.dim);
            if host {
                ln.add(&format!("  {:>5}  {:>6}", "-", "-"), th.dim);
            }
            ln.add(&format!("  {:>6}  {}", "-", p.cmd), th.dim);
        } else {
            ln.add(&format!("{:>pid_w$}  ", p.pid), th.dim);
            let u = crop(&p.user, user_w);
            ln.add(&u, if p.user == s.me { th.me } else { th.fg });
            ln.pad(user_w - str_w(&u) + 2).add(&gpus, PLAIN).add(&fmt_vram(p.mem), PLAIN);
            if host {
                ln.add("  ", PLAIN);
                match p.cpu {
                    Some(c) => ln.add(&rjust(&fmt_cpu(c), 5), th.level(Some(c.min(100.0)))),
                    None => ln.add(&rjust("-", 5), th.dim),
                };
                ln.add("  ", PLAIN).add(&rjust(&p.rss_kb.map_or("-".into(), fmt_kb), 6), PLAIN);
            }
            ln.add(&format!("  {:>6}  ", fmt_age(p.etime)), th.dim);
            ln.add(&if p.cmd.is_empty() { "?".into() } else { short_cmd(&p.cmd) }, PLAIN);
        }
        out.push(ln);
    }
    if procs.len() > shown.len() {
        out.push(line(&format!("… {} more, sorted by VRAM", procs.len() - shown.len()), th.dim));
    }
    if !s.host_users.is_empty() {
        // who is busy on the CPUs right now, GPU jobs or not (100% = one core)
        let mut ln = line("cpu users  ", th.dim);
        for (k, (u, c, rss)) in s.host_users.iter().enumerate() {
            ln.add(if k > 0 { " · " } else { "" }, th.dim).add(u, if *u == s.me { th.me } else { th.fg });
            ln.add(&format!(" {}% {}", D0(*c), fmt_kb(*rss)), th.dim);
        }
        out.push(ln);
    }
    out
}

/// One-line host summary for folded cards. Every field has a fixed width and
/// the strips are padded to the largest GPU count, so folded cards line up.
fn fold_line(s: &Sample, th: &Theme, ng: usize) -> Line {
    let mut ln = line("CPU ", th.dim);
    ln.add(&fmt_pct(s.cpu_pct)[1..], th.level(s.cpu_pct)); // "  7%" … "100%"
    let rp = ram_pct(s);
    ln.add("  RAM ", th.dim).add(&fmt_pct(rp)[1..], th.level(rp)).add("  load ", th.dim);
    match load_of(s) {
        Some((l, n)) => {
            ln.add(&rjust(&D1(l).to_string(), 5), th.level(Some(l / n as f64 / 1.2 * 100.0)));
            ln.add(&ljust(&format!("/{n}c"), 6), th.dim)
        }
        None => ln.add(&ljust(&rjust("-", 5), 11), th.dim),
    };
    if !s.gpus.is_empty() {
        ln.add(" UTIL ", th.dim);
        put_strip(&mut ln, s.gpus.iter().map(|g| g.util), th);
        ln.pad(ng.saturating_sub(s.gpus.len())).add("  VRAM ", th.dim);
        put_strip(&mut ln, s.gpus.iter().map(|g| Some(g.mem_pct())), th);
    }
    ln
}

/// -> (title left, title right, bottom-right label)
fn card_title(cx: &Ctx, tw: &mut Tweener, i: usize, h: &Host, folded: bool) -> (Line, Line, Option<Line>) {
    let th = cx.th;
    let a = Act::Fold(i);
    let (mut left, mut right) = (Line::default(), Line::default());
    let (dot, dot_st) = status_dot(cx, tw, i, h);
    left.act(if folded { "▸ " } else { "▾ " }, th.dim, a).act(&dot.to_string(), dot_st, a).add(" ", PLAIN);
    left.act(&h.name, th.title, a);
    let Some(s) = &h.sample else { return (left, right, None) };
    let models: BTreeSet<&str> = s.gpus.iter().map(|g| short_model(&g.name)).collect();
    if models.len() == 1 {
        left.add(&format!("  {}× {}", s.gpus.len(), models.first().unwrap()), th.dim);
    }
    if !s.cpu_model.is_empty() {
        let n = if s.sockets > 1 { format!("{}× ", s.sockets) } else { String::new() };
        left.add(&format!(" · {n}{}", short_cpu(&s.cpu_model)), th.dim);
    }
    let age = since(cx.now, h.updated);
    if cx.live && !cx.paused && h.status == Status::Ok && age > cx.stale_after {
        right.add(&format!("stale {}s", D0(age)), th.warn).add(" · ", th.dim);
    }
    if !s.gpus.is_empty() {
        let idle = s.gpus.iter().filter(|g| g.state() == 0).count();
        right.add(&idle.to_string(), if idle > 0 { th.state[0] } else { th.dim });
        right.add(&format!("/{} idle", s.gpus.len()), th.dim).add(" · ", th.dim);
        right.add(&format!("{}G", D0(free_gib(&s.gpus))), PLAIN).add(" free", th.dim);
    }
    let mut bottom = Line::default();
    if let Some(u) = s.uptime {
        bottom.add(&fmt_uptime(u), th.dim);
    }
    if let Some(l) = h.latency.filter(|_| cx.live) {
        bottom.add(if bottom.w() > 0 { " · " } else { "" }, th.dim).add(&format!("↻ {}ms", D0(l * 1000.0)), th.dim);
    }
    (left, right, (bottom.w() > 0).then_some(bottom))
}

/// Word-wrap by display width; words longer than a line are hard-broken.
fn wrap(s: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let mut word = word.to_string();
        while !word.is_empty() {
            let need = if cur.is_empty() { str_w(&word) } else { str_w(&cur) + 1 + str_w(&word) };
            if need <= width {
                if !cur.is_empty() {
                    cur.push(' ');
                }
                cur.push_str(&word);
                break;
            }
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            let head = cut(&word, width.max(2));
            word = word[head.len()..].to_string();
            out.push(head);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn card_body(cx: &Ctx, tw: &mut Tweener, i: usize, h: &Host, width: usize, folded: bool) -> Vec<Line> {
    let (th, st) = (cx.th, cx.st);
    let cw = width.saturating_sub(4);
    let mut body = Vec::new();
    let retry = h
        .retry_at
        .map(|t| format!("retry in {}s", t.saturating_duration_since(cx.now).as_secs_f64().ceil()))
        .unwrap_or_default();

    let Some(s) = &h.sample else {
        if h.status == Status::Connecting {
            body.push(line(&format!("connecting… {}s", D0(since(cx.now, h.since))), th.dim));
        } else {
            let err = h.error.as_deref().unwrap_or("unknown error");
            for part in wrap(err, cw.max(10)).into_iter().take(if folded { 1 } else { 2 }) {
                body.push(line(&part, th.err));
            }
            if h.needs_login {
                body.push(line("press L to log in (ssh asks for the password)", th.warn));
            } else if !retry.is_empty() && !folded {
                body.push(line(&retry, th.dim));
            }
        }
        return body;
    };
    if folded {
        body.push(fold_line(s, th, cx.ng));
        return body;
    }

    let known = s.procs.is_some();
    let c = plan_cols(cw, &s.gpus, cx.live && st.show_hist, st.show_user);
    if st.show_cpu {
        let cpu = tw.ease((i, K_CPU), s.cpu_pct.unwrap_or(0.0), 0.0);
        let used = tw.ease((i, K_RAM), ram_pct(s).unwrap_or(0.0), 0.0);
        let cache = tw.ease((i, K_CACHE), cache_pct(s), 0.0);
        body.extend(sys_rows(s, &h.history.cpu, &c, cw, th, cpu, used, cache, st.show_cores));
    }
    let vis: Vec<&Gpu> = s.gpus.iter().filter(|g| !st.free_only || g.state() < 2).collect();
    if !vis.is_empty() {
        let mut head = gpu_header(&c, th);
        head.optional = true;
        body.push(head);
        for g in vis {
            let u = tw.ease((i, g.idx * 4), g.util.unwrap_or(0.0), 0.0);
            let m = tw.ease((i, g.idx * 4 + 1), g.mem_pct(), 0.0);
            let dot = if cx.live {
                // the dot cross-fades green ↔ yellow ↔ red instead of jumping
                let sv = g.state() as f64 * 50.0;
                th.dot(tw.ease((i, g.idx * 4 + 2), sv, sv) / 50.0, 0.0)
            } else {
                th.state[g.state()]
            };
            body.push(gpu_row(g, h.history.gpu.get(&g.idx), &c, th, u, m, dot, &s.me, known));
        }
    } else if !s.gpus.is_empty() {
        body.push(line(&format!("all {} GPUs busy", s.gpus.len()), th.dim));
    }
    if st.show_proc && !s.gpus.is_empty() {
        body.extend(proc_section(s, cw, th));
    }
    if let Some(e) = &s.gpu_error {
        body.push(line(&format!("! {e}"), th.warn));
    }
    match h.status {
        Status::Error => {
            let retry = if h.needs_login { "press L to log in".into() } else { retry };
            let sep = if retry.is_empty() { "" } else { " · " };
            body.push(line(&format!("! {}{sep}{retry}", h.error.as_deref().unwrap_or("")), th.warn));
        }
        Status::Connecting => body.push(line(&format!("{} reconnecting…", spin(cx.secs)), th.dim)),
        Status::Ok => {}
    }
    body
}

fn top_border(left: Line, right: Line, width: usize, th: &Theme) -> Line {
    // ╭─ left ─────── right ─╮
    let (mut left, mut right) = (left, right);
    let mut rw = if right.w() > 0 { right.w() + 2 } else { 0 };
    if (width as isize) - 7 - (rw as isize) < left.w().min(16) as isize {
        (right, rw) = (Line::default(), 0);
    }
    let maxl = width.saturating_sub(7 + rw);
    if left.w() > maxl {
        left.fit(maxl);
    }
    let mut ln = line("╭─ ", th.border);
    ln.extend(&left).add(" ", PLAIN).fill(width.saturating_sub(6 + left.w() + rw), '─', th.border);
    if rw > 0 {
        ln.add(" ", PLAIN).extend(&right).add(" ", PLAIN);
    }
    ln.add("─╮", th.border);
    ln
}

fn bottom_border(bottom: Option<Line>, width: usize, th: &Theme) -> Line {
    let mut ln = line("╰", th.border);
    match bottom {
        Some(b) if width >= 6 + b.w() => {
            ln.fill(width - 5 - b.w(), '─', th.border).add(" ", PLAIN).extend(&b).add(" ─╯", th.border);
        }
        _ => {
            ln.fill(width.saturating_sub(2), '─', th.border).add("╯", th.border);
        }
    }
    ln
}

fn frame_card(left: Line, right: Line, body: Vec<Line>, width: usize, th: &Theme, bottom: Option<Line>) -> Vec<Line> {
    let cw = width.saturating_sub(4);
    let mut out = Vec::with_capacity(body.len() + 2);
    out.push(top_border(left, right, width, th));
    for mut b in body {
        // framed in place: no new row, no copy of its cells
        b.fit(cw);
        b.cells.splice(0..0, [Cell { ch: '│', st: th.border }, Cell { ch: ' ', st: th.border }]);
        b.zones.iter_mut().for_each(|z| (z.0, z.1) = (z.0 + 2, z.1 + 2));
        b.add(" │", th.border);
        out.push(b);
    }
    out.push(bottom_border(bottom, width, th));
    out
}

// ── list view ────────────────────────────────────────────────────────────────
/// Users on this host, by total VRAM, largest first.
fn host_users(s: &Sample) -> Vec<&str> {
    let mut tot: Vec<(&str, f64)> = Vec::new();
    for p in s.procs.iter().flatten().filter(|p| p.user != UNKNOWN) {
        match tot.iter_mut().find(|e| e.0 == p.user) {
            Some(e) => e.1 += p.mem,
            None => tot.push((&p.user, p.mem)),
        }
    }
    tot.sort_by(|a, b| b.1.total_cmp(&a.1));
    tot.into_iter().map(|e| e.0).collect()
}

fn list_panel(cx: &Ctx, tw: &mut Tweener, snaps: &[Host], order: &[usize], w: usize) -> Vec<Line> {
    let (th, st) = (cx.th, cx.st);
    let nw = snaps.iter().map(|h| str_w(&h.name)).max().unwrap_or(0).clamp(4, 24);
    let ng = snaps.iter().filter_map(|h| h.sample.as_ref()).map(|s| s.gpus.len()).max().unwrap_or(0).max(4);
    let mut head = format!(
        "  {:<nw$}  {:>5}  {:>6}  {:<ng$}  {:<ng$}   CPU   RAM  {:>11}",
        "HOST", "IDLE", "FREE", "UTIL", "VRAM", "LOAD"
    );
    if st.show_user {
        head += "  USERS";
    }
    let mut body = vec![line(&head, th.dim)];
    for &i in order {
        let h = &snaps[i];
        let (dot, dot_st) = status_dot(cx, tw, i, h);
        let mut ln = line(&format!("{dot} "), dot_st);
        ln.add(&ljust(&crop(&h.name, nw), nw), th.title).add("  ", PLAIN);
        if let Some(s) = &h.sample {
            let gpus = &s.gpus;
            let idle = gpus.iter().filter(|g| g.state() == 0).count();
            ln.add(&rjust(&format!("{idle}/{}", gpus.len()), 5), if idle > 0 { th.state[0] } else { th.dim });
            ln.add("  ", PLAIN).add(&rjust(&format!("{}G", D0(free_gib(gpus))), 6), PLAIN).add("  ", PLAIN);
            put_strip(&mut ln, gpus.iter().map(|g| g.util), th);
            ln.pad(ng - gpus.len().min(ng) + 2);
            put_strip(&mut ln, gpus.iter().map(|g| Some(g.mem_pct())), th);
            ln.pad(ng - gpus.len().min(ng) + 2);
            let rp = ram_pct(s);
            ln.add(&fmt_pct(s.cpu_pct)[1..], th.level(s.cpu_pct)).add("  ", PLAIN);
            ln.add(&fmt_pct(rp)[1..], th.level(rp)).add("  ", PLAIN);
            match load_of(s) {
                Some((l, n)) => {
                    let (v, r) = (D1(l).to_string(), format!("/{n}c"));
                    ln.pad(11usize.saturating_sub(v.len() + r.len()));
                    ln.add(&v, th.level(Some(l / n as f64 / 1.2 * 100.0))).add(&r, th.dim);
                }
                None => {
                    ln.add(&rjust("-", 11), th.dim);
                }
            }
            if st.show_user {
                ln.add("  ", PLAIN);
                if s.procs.is_none() {
                    ln.add("…", th.dim);
                }
                for (k, u) in host_users(s).into_iter().take(4).enumerate() {
                    ln.add(if k > 0 { " " } else { "" }, PLAIN).add(u, if u == s.me { th.me } else { th.fg });
                }
            }
            if let Some(e) = &s.gpu_error {
                ln.add(&format!("  ! {e}"), th.warn);
            }
        } else if h.status == Status::Connecting {
            ln.add(&format!("connecting… {}s", D0(since(cx.now, h.since))), th.dim);
        } else {
            ln.add(h.error.as_deref().unwrap_or("error"), th.err);
        }
        ln.zones = vec![(0, ln.w(), Act::Goto(i))]; // the whole row is clickable
        body.push(ln);
    }
    frame_card(line("hosts", th.title), line("click a host to open it", th.dim), body, w, th, None)
}

// ── header / footer / help ───────────────────────────────────────────────────
fn clock() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

fn header_line(cx: &Ctx, snaps: &[Host], w: usize) -> Line {
    let th = cx.th;
    let gpus: Vec<&Gpu> = snaps.iter().filter_map(|h| h.sample.as_ref()).flat_map(|s| &s.gpus).collect();
    let idle = gpus.iter().filter(|g| g.state() == 0).count();
    let free = gpus.iter().map(|g| g.mem_total - g.mem_used).sum::<f64>() / 1024.0;
    let online = snaps.iter().filter(|h| h.sample.is_some() && h.status == Status::Ok).count();
    let known: Vec<&Sample> = snaps.iter().filter_map(|h| h.sample.as_deref()).filter(|s| s.procs.is_some()).collect();
    let mine: usize =
        known.iter().map(|s| s.gpus.iter().filter(|g| !s.me.is_empty() && g.users.contains(&s.me)).count()).sum();

    let summary = |short: bool| {
        let sep = if short { " · " } else { "  ·  " };
        let mut ln = line(if short { " ◆" } else { " ◆ rgtop" }, th.accent);
        ln.add(if short { " " } else { "   " }, PLAIN);
        ln.add(&idle.to_string(), if idle > 0 { th.state[0] } else { th.err });
        ln.add(&format!("/{} {}", gpus.len(), if short { "idle" } else { "GPUs idle" }), th.dim);
        ln.add(sep, th.dim).add(&(thousands(free) + if short { "G" } else { " GiB" }), PLAIN);
        ln.add(if short { " free" } else { " VRAM free" }, th.dim).add(sep, th.dim);
        ln.add(&format!("{online}/{}", snaps.len()), if online == snaps.len() { th.state[0] } else { th.warn });
        ln.add(if short { " up" } else { " hosts" }, th.dim);
        if !known.is_empty() && !short {
            ln.add(sep, th.dim).add("you on ", th.dim).add(&mine.to_string(), if mine > 0 { th.me } else { th.dim });
            ln.add(if mine == 1 { " GPU" } else { " GPUs" }, th.dim);
        }
        ln
    };
    let mut left = summary(false);
    if left.w() + 1 > w {
        left = summary(true);
    }
    let mut legend = line("● ", th.state[0]);
    legend.add("idle  ", th.dim).add("● ", th.state[1]).add("light  ", th.dim);
    legend.add("● ", th.state[2]).add("busy", th.dim).add("    ", PLAIN);
    let mut clk = Line::default();
    if cx.paused {
        clk.act("‖ paused  ", th.warn_on, Act::Pause);
    }
    clk.add(&clock(), th.title).add(" ", PLAIN);
    let mut right = Line::default();
    if left.w() + legend.w() + clk.w() < w {
        right.extend(&legend);
    }
    if left.w() + right.w() + clk.w() < w {
        right.extend(&clk);
    }
    left.pad(w.saturating_sub(left.w() + right.w())).extend(&right).fit(w);
    left
}

/// btop-style button: hotkey highlighted inside the label (or prefixed).
fn button(label: &str, hot: &str, a: Act, th: &Theme, on: Option<bool>) -> Line {
    let style = match on {
        None => th.dim,
        Some(true) => th.on,
        Some(false) => th.off,
    };
    let mut ln = Line::default();
    match label.find(hot).filter(|_| hot.chars().count() == 1) {
        Some(i) => {
            ln.act(&label[..i], style, a).act(hot, th.key, a).act(&label[i + hot.len()..], style, a);
        }
        None => {
            ln.act(&format!("{hot} "), th.key, a).act(label, style, a);
        }
    }
    ln
}

pub fn footer_line(st: &Settings, w: usize, th: &Theme, paused: bool, scroll_txt: &str, login: bool) -> Line {
    let mut sort = Line::default();
    sort.act("← ", th.key, Act::SortPrev).act(st.sort, th.on, Act::SortNext).act(" →", th.key, Act::SortNext);
    let mut interval = Line::default();
    interval.act("− ", th.key, Act::IntDown).act(&fmt_interval(st.interval_ms), th.on, Act::IntUp);
    interval.act(" +", th.key, Act::IntUp);
    let left = [
        ("sort", sort),
        ("rev", button("reverse", "r", Act::Rev, th, Some(st.reverse))),
        ("free", button("free", "f", Act::Free, th, Some(st.free_only))),
        ("cpu", button("cpu", "c", Act::Cpu, th, Some(st.show_cpu))),
        ("cores", button("cores", "o", Act::Cores, th, Some(st.show_cores))),
        ("hist", button("hist", "h", Act::Hist, th, Some(st.show_hist))),
        ("user", button("user", "u", Act::User, th, Some(st.show_user))),
        ("proc", button("proc", "p", Act::Proc, th, Some(st.show_proc))),
        ("list", button("list", "l", Act::View, th, Some(st.view == "list"))),
        ("pause", button("pause", "␣", Act::Pause, th, Some(paused))),
    ];
    let mut login_btn = Line::default();
    login_btn.act("L ", th.key, Act::Login).act("login", th.warn_on, Act::Login);
    let newer = crate::update::NEWER.get().map(|v| {
        let mut l = Line::default();
        l.act(&format!("↑ {v}"), th.accent, Act::Help); // the help page says how to update
        l
    });
    let right = [
        ("login", login.then_some(login_btn)), // a host wants a password
        ("update", newer), // never dropped, like login: a notice only wide screens show is no notice
        ("scroll", (!scroll_txt.is_empty()).then(|| line(scroll_txt, th.dim))),
        ("int", Some(interval)),
        ("theme", Some(button(&format!("theme {}", st.theme), "t", Act::Theme, th, None))),
        ("help", Some(button("help", "?", Act::Help, th, None))),
        ("quit", Some(button("quit", "q", Act::Quit, th, None))),
    ];
    let drop = ["theme", "scroll", "cores", "hist", "cpu", "rev", "help", "user", "free", "pause", "proc", "list"];
    let build = |skip: &[&str]| {
        let (mut l, mut r) = (line(" ", PLAIN), Line::default());
        for (name, seg) in &left {
            if !skip.contains(name) {
                if l.w() > 1 {
                    l.add("  ", PLAIN);
                }
                l.extend(seg);
            }
        }
        for (name, seg) in &right {
            if let Some(seg) = seg.as_ref().filter(|_| !skip.contains(name)) {
                if r.w() > 0 {
                    r.add("  ", PLAIN);
                }
                r.extend(seg);
            }
        }
        r.add(" ", PLAIN);
        (l, r)
    };
    let mut skip = Vec::new();
    let (mut l, mut r) = build(&skip);
    for name in drop {
        if l.w() + r.w() + 2 <= w {
            break;
        }
        skip.push(name);
        (l, r) = build(&skip);
    }
    l.pad(w.saturating_sub(l.w() + r.w())).extend(&r).fit(w);
    l
}

const HELP: &[(&str, &str)] = &[
    ("Keys", ""),
    ("← →   < >", "sort hosts: host · idle · vram · util"),
    ("r", "reverse sort"),
    ("−  +", "refresh interval ±100 ms; below 100 ms: auto, as fast as hosts answer"),
    ("space   enter", "pause / resume sampling · sample now"),
    ("f", "free only: hide busy GPUs"),
    ("c o h u p", "toggle CPU rows · per-core strip · util history · GPU user · processes"),
    ("l", "list view: one row per host"),
    ("1-9   a", "fold host N · fold / unfold all"),
    ("↑ ↓ PgUp PgDn", "scroll (also j / k, g / G, mouse wheel)"),
    ("t", "theme: auto → dark → light"),
    ("m", "mouse capture on / off (off = select text with the mouse)"),
    ("L", "log in to hosts that want a password; kept until 8 h unused"),
    ("q", "quit            esc  ?   close this help"),
    ("Mouse", ""),
    ("click", "bottom-bar buttons · host title folds it · list row opens it"),
    ("Colours", ""),
];

/// ANSI Shadow, like btop's logo (docs/logo.py draws the same for the README).
const LOGO: [&str; 6] = [
    "██████╗  ██████╗ ████████╗ ██████╗ ██████╗ ",
    "██╔══██╗██╔════╝ ╚══██╔══╝██╔═══██╗██╔══██╗",
    "██████╔╝██║  ███╗   ██║   ██║   ██║██████╔╝",
    "██╔══██╗██║   ██║   ██║   ██║   ██║██╔═══╝ ",
    "██║  ██║╚██████╔╝   ██║   ╚██████╔╝██║     ",
    "╚═╝  ╚═╝ ╚═════╝    ╚═╝    ╚═════╝ ╚═╝     ",
];

fn help_panel(w: usize, th: &Theme, cfg: &str) -> (Vec<Line>, usize) {
    debug_assert_eq!(SORTS.join(" · "), "host · idle · vram · util");
    let hw = w.min(84);
    let mut body = Vec::new();
    let lw = LOGO[0].chars().count();
    if let Some(pad) = hw.checked_sub(4 + lw).map(|room| room / 2) {
        // centred, the meter gradient left to right (a bar filling up), version under its right end
        for row in LOGO {
            let mut ln = Line::default();
            ln.pad(pad);
            ln.cells
                .extend(row.chars().enumerate().map(|(i, ch)| Cell { ch, st: th.logo(i * 100 / (lw - 1), ch != '█') }));
            body.push(ln);
        }
        let mut tail = Line::default();
        if let Some(n) = crate::update::NEWER.get() {
            tail.add(&format!("{n} is out: "), th.accent).add("rgtop --update", th.key).add(" · ", th.dim);
        }
        tail.add(&format!("v{}", env!("CARGO_PKG_VERSION")), th.dim);
        let mut ln = Line::default();
        ln.pad((pad + lw - 1).saturating_sub(tail.w())).extend(&tail);
        body.push(ln);
    }
    for &(k, d) in HELP {
        if d.is_empty() {
            if !body.is_empty() {
                body.push(Line::default());
            }
            body.push(line(k, th.title));
        } else {
            let mut ln = line(&format!("  {}", ljust(k, 15)), th.key);
            ln.add(d, th.fg);
            body.push(ln);
        }
    }
    let mut ln = line("  ● ", th.state[0]);
    ln.add("idle  util < 5% and VRAM < 5%    ", th.fg).add("● ", th.state[1]).add("light  < 40%    ", th.fg);
    ln.add("● ", th.state[2]).add("busy", th.fg);
    body.push(ln);
    let mut ln = line("  your user name is highlighted like ", th.fg);
    ln.add("this", th.me);
    body.push(ln);
    body.push(Line::default());
    body.push(line(&format!("Settings are saved to {cfg} on exit."), th.dim));
    let card = frame_card(line("help", th.title), button("close", "esc", Act::Help, th, None), body, hw, th, None);
    let pad = (w - hw) / 2;
    let card = card
        .into_iter()
        .map(|l| {
            let mut p = Line::default();
            p.pad(pad).extend(&l);
            p
        })
        .collect();
    (card, pad + hw)
}

// ── frame assembly ───────────────────────────────────────────────────────────
pub struct Frame {
    pub header: Line,
    cols: Vec<(Vec<Line>, usize)>,   // columns of stacked cards, with their widths
    pub anchors: Vec<Option<usize>>, // host -> first content row of its card
    pub order: Vec<usize>,           // hosts in display order
}

impl Frame {
    /// Content rows with the columns placed side by side, each exactly `w` wide.
    /// Takes the columns: the first one's lines become the rows (no copy).
    pub fn take_rows(&mut self, w: usize) -> Vec<Line> {
        let total = self.cols.iter().map(|c| c.0.len()).max().unwrap_or(0);
        let mut cols: Vec<_> = std::mem::take(&mut self.cols).into_iter().map(|(c, cw)| (c.into_iter(), cw)).collect();
        (0..total)
            .map(|_| {
                let mut row: Option<Line> = None;
                for (it, cw) in cols.iter_mut() {
                    let r = match (&mut row, it.next()) {
                        (Some(r), Some(l)) => r.extend(&l),
                        (Some(r), None) => r.pad(*cw),
                        (None, l) => row.insert(l.unwrap_or_else(|| Line::with_capacity(w))),
                    };
                    if r.w() == 0 {
                        r.pad(*cw); // this column has no line here
                    }
                }
                let mut row = row.unwrap_or_default();
                row.fit(w);
                row
            })
            .collect()
    }
}

/// Hosts in display order. Sorting uses each host's value as last held, which
/// only follows the live value once that moved by more than a margin (1 GiB, 10%
/// util): two hosts with near-equal values would otherwise trade places on
/// every sample. `held` belongs to the sort it was made for.
fn order_hosts(snaps: &[Host], st: &Settings, held: &mut (&'static str, Vec<Option<f64>>)) -> Vec<usize> {
    if held.0 != st.sort {
        *held = (st.sort, Vec::new());
    }
    let mut idx: Vec<usize> = (0..snaps.len()).collect();
    if st.sort == "host" {
        if st.reverse {
            idx.reverse();
        }
        return idx;
    }
    let gpus = |i: usize| snaps[i].sample.as_ref().map(|s| s.gpus.as_slice()).unwrap_or(&[]);
    let (mut have, rest): (Vec<usize>, Vec<usize>) = idx.into_iter().partition(|&i| !gpus(i).is_empty());
    let (margin, key): (f64, fn(&[Gpu]) -> f64) = match st.sort {
        // most idle GPUs first, then most free VRAM (≤ 1e6 GiB per host)
        "idle" => (1.0, |g| -(g.iter().filter(|g| g.state() == 0).count() as f64 * 1e6 + free_gib(g))),
        "vram" => (1.0, |g| -free_gib(g)),
        _ => (10.0, |g| g.iter().map(|g| g.util.unwrap_or(0.0)).sum::<f64>() / g.len() as f64), // least busy first
    };
    held.1.resize(snaps.len(), None);
    for &i in &have {
        let k = key(gpus(i));
        if held.1[i].is_none_or(|h| (k - h).abs() > margin) {
            held.1[i] = Some(k);
        }
    }
    let val = |i: usize| held.1[i].unwrap_or(0.0);
    // stable either way, like Python's sorted(reverse=True)
    have.sort_by(|&a, &b| {
        let o = val(a).total_cmp(&val(b));
        if st.reverse { o.reverse() } else { o }
    });
    have.extend(rest);
    have
}

/// Masonry: a column for each card (heights in rows), each going to the then
/// shortest column, and the tallest column's height. `keep` (the last frame's
/// columns) wins unless it is more than SLACK rows taller, or runs past the
/// screen (`room` rows) where the greedy layout fits: a card that grows by a row
/// (a process started, an error line) must not send the cards after it hopping
/// to the other column.
fn place(heights: &[usize], ncols: usize, keep: Option<&[usize]>, room: Option<usize>) -> (Vec<usize>, usize) {
    const SLACK: usize = 3;
    let tallest = |at: &[usize]| {
        let mut h = vec![0; ncols];
        at.iter().zip(heights).for_each(|(&c, &n)| h[c] += n);
        h.into_iter().max().unwrap_or(0)
    };
    let mut h = vec![0; ncols];
    let greedy: Vec<usize> = heights
        .iter()
        .map(|&n| {
            let c = (0..ncols).min_by_key(|&c| h[c]).unwrap_or(0);
            h[c] += n;
            c
        })
        .collect();
    let tall = tallest(&greedy);
    match keep.filter(|k| k.len() == heights.len() && k.iter().all(|&c| c < ncols)).map(|k| (k, tallest(k))) {
        Some((k, kt)) if kt <= tall + SLACK && room.is_none_or(|r| kt <= r.max(tall)) => (k.to_vec(), kt),
        _ => (greedy, tall),
    }
}

/// Card columns, placed by `place`, so a folded or short card never leaves a
/// hole next to a tall one. Too tall for the screen -> compact: the
/// column-header rows are dropped (each card is still built only once).
/// `placed`: (order, columns, column of each card) of the last frame.
#[allow(clippy::too_many_arguments)]
fn layout_cards(
    cx: &Ctx,
    tw: &mut Tweener,
    snaps: &[Host],
    order: &[usize],
    anchors: &mut [Option<usize>],
    area: Option<usize>,
    w: usize,
    placed: &mut Option<(Vec<usize>, usize, Vec<usize>)>,
) -> Vec<(Vec<Line>, usize)> {
    let n = order.len().max(1);
    let max_cols = (w / CARD_MIN_WIDTH).clamp(1, n);
    let ncols = n.div_ceil(n.div_ceil(max_cols));
    let card_w = (w / ncols).max(24);
    let cards: Vec<(usize, Vec<Line>)> = order
        .iter()
        .map(|&i| {
            let h = &snaps[i];
            let folded = cx.st.collapsed.contains(&h.name);
            let (l, r, b) = card_title(cx, tw, i, h, folded);
            (i, frame_card(l, r, card_body(cx, tw, i, h, card_w, folded), card_w, cx.th, b))
        })
        .collect();
    let heights = |compact: bool| -> Vec<usize> {
        cards.iter().map(|(_, lines)| lines.iter().filter(|l| !(compact && l.optional)).count()).collect()
    };
    let keep = placed.as_ref().filter(|p| p.0 == order && p.1 == ncols).map(|p| p.2.as_slice());
    let (mut at, tall) = place(&heights(false), ncols, keep, area);
    let compact = area.is_some_and(|a| tall > a);
    if compact {
        at = place(&heights(true), ncols, keep, area).0;
    }
    let mut cols: Vec<Vec<Line>> = vec![Vec::new(); ncols];
    for ((i, lines), &c) in cards.into_iter().zip(&at) {
        anchors[i] = Some(cols[c].len());
        cols[c].extend(lines.into_iter().filter(|l| !(compact && l.optional)));
    }
    *placed = Some((order.to_vec(), ncols, at));
    cols.into_iter().map(|c| (c, card_w)).collect()
}

pub struct Renderer {
    pub th: Theme,
    live: bool,
    cfg: String,
    tw: Tweener,
    t0: Instant,
    pub animating: bool,                             // bars easing or a dot glowing -> render at FPS
    pub spinning: bool,                              // a spinner is visible -> render at the spinner rate
    held: (&'static str, Vec<Option<f64>>),          // sort values, see order_hosts
    placed: Option<(Vec<usize>, usize, Vec<usize>)>, // card columns, see layout_cards
}

impl Renderer {
    pub fn new(th: Theme, live: bool, cfg: String) -> Self {
        Renderer {
            th,
            live,
            cfg,
            tw: Tweener::default(),
            t0: Instant::now(),
            animating: false,
            spinning: false,
            held: ("", Vec::new()),
            placed: None,
        }
    }

    /// Seconds until the spinner shows its next glyph.
    pub fn spinner_in(&self, now: Instant) -> f64 {
        let t = since(now, self.t0) * 12.0;
        (t.floor() + 1.0 - t) / 12.0
    }

    #[allow(clippy::too_many_arguments)]
    pub fn frame(
        &mut self,
        snaps: &[Host],
        st: &Settings,
        w: usize,
        area: Option<usize>,
        now: Instant,
        dt: Option<f64>,
        paused: bool,
        help_on: bool,
    ) -> Frame {
        let iv = if st.interval_ms == AUTO { AUTO_TICK_MS } else { st.interval_ms } as f64 / 1000.0;
        self.tw.begin(dt, TWEEN_TAU.min(iv / 3.0)); // settle well before the next sample
        let cx = Ctx {
            th: &self.th,
            st,
            now,
            secs: since(now, self.t0),
            live: self.live,
            paused,
            stale_after: (3.0 * iv).max(iv + 8.0),
            iv,
            ng: snaps.iter().filter_map(|h| h.sample.as_ref()).map(|s| s.gpus.len()).max().unwrap_or(0),
        };
        let tw = &mut self.tw;
        let order = order_hosts(snaps, st, &mut self.held);
        let header = header_line(&cx, snaps, w);
        let mut anchors = vec![None; snaps.len()];

        let cols = if help_on {
            vec![help_panel(w, cx.th, &self.cfg)]
        } else if st.view == "list" {
            vec![(list_panel(&cx, tw, snaps, &order, w), w)]
        } else {
            layout_cards(&cx, tw, snaps, &order, &mut anchors, area, w, &mut self.placed)
        };

        self.spinning = snaps.iter().any(|h| h.status == Status::Connecting);
        let pulsing = self.live
            && iv >= 1.0
            && snaps.iter().any(|h| {
                let age = since(now, h.updated);
                h.sample.is_some() && h.status == Status::Ok && age < PULSE_SECS
            });
        self.animating = pulsing || self.tw.moving;
        Frame { header, cols, anchors, order }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        let has_logo = |w| {
            help_panel(w, &Theme::new("dark"), "")
                .0
                .iter()
                .any(|l| l.cells.iter().map(|c| c.ch).collect::<String>().contains("██████╗  ██████╗ ████████╗"))
        };
        assert!(has_logo(100) && has_logo(47) && !has_logo(46)); // shown only where it fits whole
        assert_eq!(thousands(std::iter::empty::<f64>().sum()), "0");
        assert_eq!(thousands(1576.6), "1,577");
        assert_eq!(thousands(1234567.0), "1,234,567");
        assert_eq!(fmt_gpus(&[0, 1, 2, 3, 6]), "0-3,6");
        let mut l = line("a中b", PLAIN);
        l.fit(3); // the wide char can't be split: blank + ellipsis
        assert_eq!(l.cells.iter().map(|c| c.ch).collect::<String>(), "a …");
        assert_eq!(short_cpu("AMD EPYC 9654 96-Core Processor"), "EPYC 9654");
        assert_eq!(short_cpu("Intel(R) Xeon(R) Gold 6338 CPU @ 2.00GHz"), "Xeon Gold 6338");
    }

    #[test]
    fn update_notice_stays_in_a_narrow_bottom_bar() {
        let _ = crate::update::NEWER.set("v9.9.9".into());
        let bar = footer_line(&Settings::default(), 60, &Theme::new("dark"), false, "", false);
        assert!(bar.cells.iter().map(|c| c.ch).collect::<String>().contains("↑ v9.9.9"));
    }

    #[test]
    fn cards_and_hosts_stay_put() {
        let (a, _) = place(&[10, 10, 8], 2, None, None);
        assert_eq!(a, [0, 1, 0]);
        assert_eq!(place(&[11, 10, 8], 2, None, None).0, [0, 1, 1]); // alone, the third card would hop
        assert_eq!(place(&[11, 10, 8], 2, Some(&a), None).0, a);
        assert_eq!(place(&[30, 10, 8], 2, Some(&a), None).0, [0, 1, 1]); // far out of balance: re-placed
        assert_eq!(place(&[11, 10, 8], 2, Some(&a), Some(18)).0, [0, 1, 1]); // kept: 19 rows, room for 18

        let host = |u: f64| {
            let mut h = Host::new("h");
            let g = Gpu {
                idx: 0,
                uuid: String::new(),
                name: String::new(),
                util: Some(u),
                mem_used: 0.0,
                mem_total: 1.0,
                temp: None,
                power: None,
                power_limit: None,
                users: Vec::new(),
            };
            h.sample = Some(std::sync::Arc::new(Sample { gpus: vec![g], ..Sample::default() }));
            h
        };
        let st = Settings { sort: "util", ..Settings::default() };
        let mut held = ("", Vec::new());
        assert_eq!(order_hosts(&[host(50.0), host(45.0)], &st, &mut held), [1, 0]);
        assert_eq!(order_hosts(&[host(44.0), host(47.0)], &st, &mut held), [1, 0]); // jitter: no swap
        let by_host = Settings { sort: "host", ..Settings::default() };
        order_hosts(&[host(44.0), host(47.0)], &by_host, &mut held);
        assert_eq!(order_hosts(&[host(44.0), host(47.0)], &st, &mut held), [0, 1]); // back from host: sorted afresh
        assert_eq!(order_hosts(&[host(30.0), host(47.0)], &st, &mut held), [0, 1]); // a real change
    }

    /// Worst case: 5 hosts x 8 GPUs, 192 cores, every panel on, data changing
    /// every 6th frame (100 ms at 60 fps) so bars never stop easing.
    /// cargo test --release -- --ignored bench_frame --nocapture
    #[test]
    #[ignore]
    fn bench_frame() {
        use crate::collect::{History, Proc};
        use crate::term::{Colors, Screen};
        let st =
            Settings { show_user: true, show_hist: true, show_proc: true, interval_ms: 100, ..Settings::default() };
        let sample = |k: usize| {
            let gpus: Vec<Gpu> = (0..8)
                .map(|i| Gpu {
                    idx: i,
                    uuid: format!("GPU-{i}"),
                    name: "NVIDIA H100 80GB HBM3".into(),
                    util: Some(((i as usize * 37 + k * 13) % 101) as f64),
                    mem_used: ((i as usize * 7919 + k * 977) % 81559) as f64,
                    mem_total: 81559.0,
                    temp: Some(40.0 + i as f64),
                    power: Some(300.0 + (k % 50) as f64),
                    power_limit: Some(700.0),
                    users: vec!["alice".into(), "bob".into()],
                })
                .collect();
            let procs = (0..10)
                .map(|p| Proc {
                    pid: 100_000 + p,
                    user: if p % 2 == 0 { "alice" } else { "bob" }.into(),
                    etime: Some(3600 * p as u64),
                    cmd: "/opt/conda/bin/python -m torch.distributed.run --nproc_per_node 8 train.py".into(),
                    gpus: vec![p % 8],
                    mem: 20000.0,
                    cpu: Some(99.0),
                    rss_kb: Some(12 << 20),
                })
                .collect();
            Sample {
                gpus,
                cpu_pct: Some((k % 100) as f64),
                core_pct: (0..192).map(|c| ((c * 7 + k) % 100) as f32).collect(),
                cpu_temp: Some(45.0),
                cpu_model: "INTEL(R) XEON(R) PLATINUM 8558".into(),
                sockets: 2,
                load: Some([30.0, 28.0, 27.0]),
                cores: Some(192),
                mem_total_kb: 2 << 31,
                mem_avail_kb: 1 << 31,
                mem_cache_kb: 1 << 30,
                swap_total_kb: 16 << 20,
                swap_free_kb: 15 << 20,
                uptime: Some(4e6),
                procs: Some(procs),
                host_users: vec![("alice".into(), 1840.0, 40 << 20), ("bob".into(), 900.0, 20 << 20)],
                me: "alice".into(),
                ..Sample::default()
            }
        };
        let mut hist = History::default();
        hist.cpu = (0..240).map(|v| (v % 100) as f32).collect();
        hist.gpu = (0..8).map(|i| (i, (0..240).map(|v| ((v + i) % 100) as f32).collect())).collect();
        let hist = std::sync::Arc::new(hist);
        let mut hosts: Vec<Host> = (0..5)
            .map(|h| {
                let mut x = Host::new(&format!("gpu-node-{h}"));
                (x.status, x.history, x.latency) = (Status::Ok, hist.clone(), Some(0.045));
                x
            })
            .collect();
        let (w, ht) = (200, 60);
        let mut r = Renderer::new(Theme::new("dark"), true, String::new());
        let mut screen = Screen::new(Colors::True);
        let t0 = Instant::now();
        let frames = std::env::var("RGTOP_BENCH_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(1200);
        let mut bytes = 0;
        for f in 0..frames {
            if f % 6 == 0 {
                let s = std::sync::Arc::new(sample(f / 6));
                for x in &mut hosts {
                    (x.sample, x.updated) = (Some(s.clone()), Instant::now());
                }
            }
            let now = Instant::now();
            let mut fr = r.frame(&hosts, &st, w, Some(ht - 2), now, Some(1.0 / FPS), false, false);
            let rows: Vec<Vec<Cell>> =
                std::iter::once(fr.header.clone()).chain(fr.take_rows(w)).take(ht).map(|l| l.cells).collect();
            bytes += screen.diff(rows, w, ht).len();
        }
        let per = t0.elapsed().as_secs_f64() / frames as f64;
        println!(
            "bench_frame: {:.1} us/frame ({:.2}% of a core at {FPS} fps), {} bytes/frame",
            per * 1e6,
            per * FPS * 100.0,
            bytes / frames
        );
    }
}
