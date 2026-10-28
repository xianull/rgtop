//! Colour palettes (gruvbox-material, matching the btop theme) and detection.

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use crate::term::{Rgb, Style};

const fn rgb(h: u32) -> Rgb {
    [(h >> 16) as u8, (h >> 8) as u8, h as u8]
}

pub fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let f = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
    [f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2])]
}

fn gradient(stops: &[Rgb]) -> Vec<Rgb> {
    let seg = (stops.len() - 1) as f64;
    (0..=100)
        .map(|i| {
            let x = i as f64 / 100.0 * seg;
            let k = (x as usize).min(stops.len() - 2);
            mix(stops[k], stops[k + 1], x - k as f64)
        })
        .collect()
}

struct Palette {
    fg: Rgb,
    dim: Rgb,
    off: Rgb,
    faint: Rgb,
    border: Rgb,
    title: Rgb,
    accent: Rgb,
    green: Rgb,
    yellow: Rgb,
    red: Rgb,
    aqua: Rgb,
}

const DARK: Palette = Palette {
    fg: rgb(0xd4be98),
    dim: rgb(0x928374),
    off: rgb(0x665c54),
    faint: rgb(0x3c3836),
    border: rgb(0x5a524c),
    title: rgb(0xddc7a1),
    accent: rgb(0x7daea3),
    green: rgb(0xa9b665),
    yellow: rgb(0xd8a657),
    red: rgb(0xea6962),
    aqua: rgb(0x89b482),
};

const LIGHT: Palette = Palette {
    fg: rgb(0x654735),
    dim: rgb(0x7c6f64),
    off: rgb(0xa89984),
    faint: rgb(0xe5d5ad),
    border: rgb(0xbdae93),
    title: rgb(0x4f3829),
    accent: rgb(0x45707a),
    green: rgb(0x6c782e),
    yellow: rgb(0xb47109),
    red: rgb(0xc14a4a),
    aqua: rgb(0x4c7a5d),
};

pub struct Theme {
    pub fg: Style,
    pub dim: Style,
    pub border: Style,
    pub title: Style,
    pub accent: Style,
    pub key: Style, // hotkey letters / clickable arrows
    pub on: Style,  // active toggle
    pub off: Style, // inactive toggle
    pub me: Style,  // your own user name
    pub baseline: Style,
    pub track: Style,
    pub warn: Style,
    pub warn_on: Style,
    pub err: Style,
    pub state: [Style; 3],
    title_rgb: Rgb,
    faint: Rgb,
    ramp: Vec<Rgb>,
    heat: Vec<Rgb>,
}

fn idx(v: Option<f64>) -> usize {
    v.map_or(0, |v| v.clamp(0.0, 100.0) as usize)
}

impl Theme {
    pub fn new(name: &str) -> Theme {
        let p = if name == "light" { &LIGHT } else { &DARK };
        Theme {
            fg: Style::fg(p.fg),
            dim: Style::fg(p.dim),
            border: Style::fg(p.border),
            title: Style::fg(p.title).bold(),
            accent: Style::fg(p.accent).bold(),
            key: Style::fg(p.accent).bold(),
            on: Style::fg(p.title).bold(),
            off: Style::fg(p.off),
            me: Style::fg(p.accent).bold(),
            baseline: Style::fg(p.faint),
            track: Style { fg: None, bg: Some(p.faint), bold: false },
            warn: Style::fg(p.yellow),
            warn_on: Style::fg(p.yellow).bold(),
            err: Style::fg(p.red).bold(),
            state: [p.green, p.yellow, p.red].map(|c| Style::fg(c).bold()),
            title_rgb: p.title,
            faint: p.faint,
            ramp: gradient(&[p.green, p.yellow, p.red]),
            heat: gradient(&[p.aqua, p.yellow, p.red]),
        }
    }

    /// green → yellow → red by percentage
    pub fn level(&self, pct: Option<f64>) -> Style {
        Style::fg(self.ramp[idx(pct)])
    }

    /// One cell of a meter: ramp colour by position, on the tinted track.
    pub fn bar(&self, pct: usize) -> Style {
        Style { fg: Some(self.ramp[pct.min(100)]), bg: Some(self.faint), bold: false }
    }

    pub fn temp(&self, c: f64) -> Style {
        Style::fg(self.heat[idx(Some((c - 30.0) * 100.0 / 55.0))]) // 30°C cool … 85°C hot
    }

    /// Status dot for a (possibly eased, fractional) state 0 idle … 2 busy,
    /// blended toward the title colour by `glow` (1 right after a fresh sample).
    pub fn dot(&self, state: f64, glow: f64) -> Style {
        Style::fg(mix(self.ramp[idx(Some(state * 50.0))], self.title_rgb, glow)).bold()
    }
}

/// Ask the terminal for its background colour (OSC 11). A DA1 query is sent
/// right after it, so terminals that ignore OSC 11 don't cost the full timeout.
fn query_background(timeout: Duration) -> Option<[f64; 3]> {
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return None;
    }
    let mut buf = Vec::new();
    unsafe {
        let mut old: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut old) != 0 {
            return None;
        }
        let mut raw = old;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG); // Ctrl-C here would skip the restore
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(0, libc::TCSANOW, &raw);
        let q = b"\x1b]11;?\x1b\\\x1b[c";
        libc::write(1, q.as_ptr().cast(), q.len());
        let end = Instant::now() + timeout;
        while !has_da1(&buf) {
            let left = end.saturating_duration_since(Instant::now()).as_millis() as i32;
            let mut pfd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
            if left <= 0 || libc::poll(&mut pfd, 1, left) <= 0 {
                break;
            }
            let mut tmp = [0u8; 256];
            let n = libc::read(0, tmp.as_mut_ptr().cast(), tmp.len());
            if n <= 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n as usize]);
        }
        libc::tcsetattr(0, libc::TCSAFLUSH, &old);
    }
    parse_rgb(&buf)
}

fn has_da1(buf: &[u8]) -> bool {
    // ESC [ ? digits/semicolons c
    buf.windows(3).enumerate().any(|(i, w)| {
        w == b"\x1b[?" && {
            let rest = &buf[i + 3..];
            let n = rest.iter().take_while(|b| b.is_ascii_digit() || **b == b';').count();
            rest.get(n) == Some(&b'c')
        }
    })
}

/// "rgb:RRRR/GGGG/BBBB" (1–4 hex digits per channel) -> 0..1 floats
fn parse_rgb(buf: &[u8]) -> Option<[f64; 3]> {
    let s = String::from_utf8_lossy(buf);
    let rest = &s[s.find("rgb:")? + 4..];
    let mut out = [0.0; 3];
    let mut parts = rest.splitn(3, '/');
    for v in out.iter_mut() {
        let p = parts.next()?;
        let hex: String = p.chars().take_while(char::is_ascii_hexdigit).take(4).collect();
        if hex.is_empty() {
            return None;
        }
        *v = u32::from_str_radix(&hex, 16).ok()? as f64 / ((1u32 << (4 * hex.len())) - 1) as f64;
    }
    Some(out)
}

pub fn detect_theme() -> &'static str {
    if let Some([r, g, b]) = query_background(Duration::from_millis(300)) {
        return if 0.2126 * r + 0.7152 * g + 0.0722 * b > 0.5 { "light" } else { "dark" };
    }
    let bg = std::env::var("COLORFGBG").unwrap_or_default();
    match bg.rsplit(';').next().and_then(|v| v.parse::<u32>().ok()) {
        Some(7 | 15) => "light",
        _ => "dark",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc11_reply_parses() {
        let reply = b"\x1b]11;rgb:ffff/ffff/0000\x1b\\\x1b[?62;22c";
        assert!(has_da1(reply));
        assert_eq!(parse_rgb(reply), Some([1.0, 1.0, 0.0]));
        assert!(!has_da1(b"\x1b]11;rgb:ff/ff/ff\x07"));
    }
}
