//! Interactive full-screen UI: keyboard + mouse input, sampling scheduler,
//! frame pacing, scrolling.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering::SeqCst};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::collect::{DATA_GEN, UI_BUSY, WANT_HOST, WANT_PROCS, WANT_SYS, Worker, supervise};
use crate::draw::{FPS, Line, Renderer, footer_line};
use crate::settings::{AUTO, AUTO_TICK_MS, SORTS, Settings, THEMES, VIEWS, step_interval};
use crate::term::{self, Screen};
use crate::theme::Theme;

/// Everything that can wake the UI loop.
pub enum Msg {
    Data, // a worker has news
    Input(Vec<u8>),
    InputEof,
    Signal(i32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Act {
    Quit,
    Help,
    Esc,
    SortPrev,
    SortNext,
    Rev,
    IntUp,
    IntDown,
    Pause,
    Refresh,
    Free,
    Cpu,
    Cores,
    Hist,
    User,
    Proc,
    View,
    Theme,
    Mouse,
    FoldAll,
    Fold(usize),  // host index
    FoldN(usize), // N-th host on screen
    Goto(usize),  // host index
    Scroll(isize),
    Page(isize),
}

// ── input ────────────────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PgUp,
    PgDn,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ev {
    Key(Key),
    Mouse(u32, usize, usize), // button, x, y (0-based); presses only
}

fn csi_key(params: &[u8], fin: u8) -> Option<Key> {
    let first = params.split(|&b| b == b';').next().unwrap_or(&[]);
    Some(match (fin, first) {
        (b'A', _) => Key::Up,
        (b'B', _) => Key::Down,
        (b'C', _) => Key::Right,
        (b'D', _) => Key::Left,
        (b'H', _) | (b'~', b"1" | b"7") => Key::Home,
        (b'F', _) | (b'~', b"4" | b"8") => Key::End,
        (b'~', b"5") => Key::PgUp,
        (b'~', b"6") => Key::PgDn,
        _ => return None,
    })
}

fn num_until(buf: &[u8], stop: u8) -> Option<(usize, usize)> {
    let n = buf.iter().take_while(|b| b.is_ascii_digit()).count();
    if n == 0 || buf.get(n) != Some(&stop) {
        return None;
    }
    Some((std::str::from_utf8(&buf[..n]).ok()?.parse().ok()?, n + 1))
}

/// SGR mouse: ESC [ < b ; x ; y (M|m) -> (button, x, y, pressed, length)
fn mouse(buf: &[u8]) -> Option<(u32, usize, usize, bool, usize)> {
    let rest = buf.strip_prefix(b"\x1b[<")?;
    let (b, i) = num_until(rest, b';')?;
    let (x, j) = num_until(&rest[i..], b';')?;
    let tail = &rest[i + j..];
    let n = tail.iter().take_while(|b| b.is_ascii_digit()).count();
    let y: usize = std::str::from_utf8(&tail[..n]).ok()?.parse().ok()?;
    let fin = *tail.get(n)?;
    if fin != b'M' && fin != b'm' {
        return None;
    }
    Some((b as u32, x.max(1) - 1, y.max(1) - 1, fin == b'M', 3 + i + j + n + 1))
}

/// -> (events, unparsed tail)
pub fn parse_input(buf: &[u8]) -> (Vec<Ev>, Vec<u8>) {
    let mut ev = Vec::new();
    let (mut i, n) = (0, buf.len());
    while i < n {
        if buf[i] != 0x1b {
            let mut j = i + 1;
            while j < n && buf[j] & 0xC0 == 0x80 {
                j += 1; // rest of a UTF-8 character
            }
            let ch = String::from_utf8_lossy(&buf[i..j]).chars().next().unwrap_or('?');
            ev.push(Ev::Key(match ch {
                '\r' | '\n' => Key::Enter,
                '\x7f' => Key::Backspace,
                c => Key::Char(c),
            }));
            i = j;
            continue;
        }
        if i + 1 >= n {
            return (ev, buf[i..].to_vec()); // lone ESC: maybe more is coming
        }
        let rest = &buf[i..];
        if let Some((b, x, y, press, len)) = mouse(rest) {
            if press {
                ev.push(Ev::Mouse(b, x, y));
            }
            i += len;
            continue;
        }
        match rest[1] {
            b'[' => {
                let p = rest[2..].iter().take_while(|b| b.is_ascii_digit() || **b == b';' || **b == b'?').count();
                if let Some(&fin) = rest.get(2 + p).filter(|b| b.is_ascii_alphabetic() || **b == b'~') {
                    ev.extend(csi_key(&rest[2..2 + p], fin).map(Ev::Key));
                    i += 3 + p;
                    continue;
                }
            }
            b'O' if rest.len() > 2 && rest[2].is_ascii_alphabetic() => {
                ev.extend(csi_key(b"", rest[2]).map(Ev::Key));
                i += 3;
                continue;
            }
            b']' => {
                // OSC (e.g. a late reply to the background-colour query): skip to BEL / ST
                let end = rest
                    .iter()
                    .position(|&b| b == 0x07)
                    .or_else(|| rest.windows(2).position(|w| w == b"\x1b\\").map(|k| k + 1));
                match end {
                    Some(k) => {
                        i += k + 1;
                        continue;
                    }
                    None if rest.len() < 64 => return (ev, rest.to_vec()),
                    None => {}
                }
            }
            _ => {}
        }
        if matches!(rest[1], b'[' | b'O') && rest.len() < 16 {
            return (ev, rest.to_vec()); // incomplete sequence
        }
        ev.push(Ev::Key(Key::Esc));
        i += 1;
    }
    (ev, Vec::new())
}

fn keymap(k: Key) -> Option<Act> {
    use Key::*;
    Some(match k {
        Char('q' | 'Q') => Act::Quit,
        Char('?') => Act::Help,
        Esc => Act::Esc,
        Left | Char('<') => Act::SortPrev,
        Right | Char('>') => Act::SortNext,
        Char('r') => Act::Rev,
        Char('+' | '=') => Act::IntUp,
        Char('-' | '_') => Act::IntDown,
        Char(' ') => Act::Pause,
        Enter => Act::Refresh,
        Char('f') => Act::Free,
        Char('c') => Act::Cpu,
        Char('o') => Act::Cores,
        Char('h') => Act::Hist,
        Char('u') => Act::User,
        Char('p') => Act::Proc,
        Char('l' | 'v') => Act::View,
        Char('t') => Act::Theme,
        Char('m') => Act::Mouse,
        Char('a') => Act::FoldAll,
        Char(d @ '1'..='9') => Act::FoldN(d as usize - '1' as usize),
        Up | Char('k') => Act::Scroll(-1),
        Down | Char('j') => Act::Scroll(1),
        PgUp => Act::Page(-1),
        PgDn => Act::Page(1),
        Home | Char('g') => Act::Scroll(-1_000_000_000),
        End | Char('G') => Act::Scroll(1_000_000_000),
        _ => return None,
    })
}

fn next(of: &[&'static str], cur: &str, by: isize) -> &'static str {
    let i = of.iter().position(|c| *c == cur).unwrap_or(0) as isize;
    of[(i + by).rem_euclid(of.len() as isize) as usize]
}

/// What to ask the agents for: the process list also shows the host's top
/// CPU users; the USER column needs only the GPU processes.
pub fn want_level(st: &Settings) -> u8 {
    if st.show_proc {
        WANT_HOST
    } else if st.show_user {
        WANT_PROCS
    } else {
        WANT_SYS
    }
}

// ── app ──────────────────────────────────────────────────────────────────────
pub struct App {
    pub st: Settings,
    workers: Vec<Arc<Worker>>,
    names: Vec<String>,
    renderer: Renderer,
    rx: Receiver<Msg>,
    want: Arc<AtomicU8>,
    auto_theme: &'static str,
    hard: f64,
    screen: Screen,
    paused: bool,
    help_on: bool,
    scroll: isize,
    goto: Option<usize>,
    zones: Vec<Vec<(usize, usize, Act)>>,
    order: Vec<usize>,
    area: usize,
    next_tick: Instant,
    quit: bool,
    size: (usize, usize), // cached: re-read on SIGWINCH, not every frame (ioctl)
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        st: Settings,
        workers: Vec<Arc<Worker>>,
        renderer: Renderer,
        rx: Receiver<Msg>,
        want: Arc<AtomicU8>,
        auto_theme: &'static str,
        hard: f64,
    ) -> Self {
        let names = workers.iter().map(|w| w.host.lock().unwrap().name.clone()).collect();
        App {
            st,
            workers,
            names,
            renderer,
            rx,
            want,
            auto_theme,
            hard,
            screen: Screen::new(term::detect_colors()),
            paused: false,
            help_on: false,
            scroll: 0,
            goto: None,
            zones: Vec::new(),
            order: Vec::new(),
            area: 1,
            next_tick: Instant::now(),
            quit: false,
            size: term::size(),
        }
    }

    /// In auto mode every host is asked again as soon as it answered (a request
    /// is a no-op while one is in flight), but at most every AUTO_TICK_MS.
    fn interval(&self) -> Duration {
        Duration::from_millis(if self.st.interval_ms == AUTO { AUTO_TICK_MS } else { self.st.interval_ms })
    }

    // sampling ----------------------------------------------------------------
    fn sample_now(&mut self) {
        for w in &self.workers {
            w.request();
        }
        self.next_tick = Instant::now() + self.interval();
    }

    fn schedule(&mut self, now: Instant) {
        for w in &self.workers {
            w.keepalive(); // paused or not
        }
        if self.paused || now < self.next_tick {
            return;
        }
        for w in &self.workers {
            w.request();
        }
        self.next_tick += self.interval();
        if self.next_tick <= now {
            self.next_tick = now + self.interval();
        }
    }

    // actions -----------------------------------------------------------------
    fn toggle_fold(&mut self, name: &str) {
        match self.st.collapsed.iter().position(|c| c == name) {
            Some(k) => {
                self.st.collapsed.remove(k);
            }
            None => self.st.collapsed.push(name.into()),
        }
    }

    fn act(&mut self, a: Act) {
        let st = &mut self.st;
        match a {
            Act::Fold(i) => self.toggle_fold(&self.names[i].clone()),
            Act::FoldN(k) => {
                if let Some(&i) = self.order.get(k) {
                    self.toggle_fold(&self.names[i].clone());
                }
            }
            Act::Goto(i) => {
                st.view = "cards";
                st.collapsed.retain(|c| *c != self.names[i]);
                self.goto = Some(i);
            }
            Act::Scroll(n) => self.scroll = self.scroll.saturating_add(n),
            Act::Page(n) => self.scroll += n * (self.area as isize - 2).max(1),
            Act::Quit => self.quit = true,
            Act::Help => {
                self.help_on = !self.help_on;
                self.scroll = 0;
            }
            Act::Esc => self.help_on = false,
            Act::SortNext => st.sort = next(&SORTS, st.sort, 1),
            Act::SortPrev => st.sort = next(&SORTS, st.sort, -1),
            Act::Rev => st.reverse = !st.reverse,
            Act::IntUp | Act::IntDown => {
                let old = st.interval_ms;
                st.interval_ms = step_interval(old, if a == Act::IntUp { 1 } else { -1 });
                // keep the phase, new period
                let new = st.interval_ms;
                self.next_tick = if new > old {
                    self.next_tick + Duration::from_millis(new - old)
                } else {
                    self.next_tick.checked_sub(Duration::from_millis(old - new)).unwrap_or(self.next_tick)
                };
            }
            Act::Pause => {
                self.paused = !self.paused;
                if !self.paused {
                    self.sample_now();
                }
            }
            Act::Refresh => self.sample_now(),
            Act::Free => st.free_only = !st.free_only,
            Act::Cpu => st.show_cpu = !st.show_cpu,
            Act::Cores => st.show_cores = !st.show_cores,
            Act::Hist => st.show_hist = !st.show_hist,
            Act::User | Act::Proc => {
                let on = if a == Act::User { &mut st.show_user } else { &mut st.show_proc };
                *on = !*on;
                let on = *on;
                self.want.store(want_level(st), SeqCst);
                if on {
                    self.sample_now(); // fetch process info right away
                }
            }
            Act::View => {
                st.view = next(&VIEWS, st.view, 1);
                self.scroll = 0;
            }
            Act::FoldAll => {
                let all = self.order.iter().all(|&i| st.collapsed.contains(&self.names[i]));
                st.collapsed =
                    if all { Vec::new() } else { self.order.iter().map(|&i| self.names[i].clone()).collect() };
            }
            Act::Theme => {
                st.theme = next(&THEMES, st.theme, 1);
                self.renderer.th = Theme::new(if st.theme == "auto" { self.auto_theme } else { st.theme });
                self.screen.invalidate();
            }
            Act::Mouse => {
                st.mouse = !st.mouse;
                self.screen.set_mouse(st.mouse);
            }
        }
    }

    fn on_event(&mut self, ev: Ev) {
        match ev {
            Ev::Key(k) => {
                if let Some(a) = keymap(k) {
                    self.act(a);
                }
            }
            Ev::Mouse(b, x, y) => {
                if b & 64 != 0 {
                    // wheel
                    self.act(Act::Scroll(if b & 3 == 0 { -3 } else { 3 }));
                } else if b & 3 == 0 && b & 32 == 0 {
                    // left button press
                    let hit = self.zones.get(y).and_then(|z| z.iter().find(|z| z.0 <= x && x < z.1));
                    if let Some(&(_, _, a)) = hit {
                        self.act(a);
                    }
                }
            }
        }
    }

    // main loop ---------------------------------------------------------------
    fn build(&mut self, now: Instant, dt: Option<f64>) {
        let (w, h) = self.size;
        let area = h.saturating_sub(2).max(1);
        self.area = area;
        let snaps: Vec<_> = self.workers.iter().map(|w| w.host.lock().unwrap().clone()).collect();
        let mut fr = self.renderer.frame(&snaps, &self.st, w, Some(area), now, dt, self.paused, self.help_on);
        self.order.clone_from(&fr.order);
        if let Some(y) = self.goto.and_then(|g| fr.anchors.get(g).copied().flatten()) {
            self.scroll = y as isize;
            self.goto = None;
        }
        let mut rows = fr.take_rows(w);
        let total = rows.len();
        self.scroll = self.scroll.clamp(0, total.saturating_sub(area) as isize);
        let s0 = self.scroll as usize;
        let scroll_txt =
            if total > area { format!("↕ {}-{}/{total}", s0 + 1, (s0 + area).min(total)) } else { String::new() };
        let mut view: Vec<Line> = rows.drain(s0..(s0 + area).min(total)).collect();
        view.resize_with(area, || {
            let mut l = Line::default();
            l.pad(w);
            l
        });
        let footer = footer_line(&self.st, w, &self.renderer.th, self.paused, &scroll_txt);
        let lines = std::iter::once(fr.header).chain(view).chain(std::iter::once(footer));
        let (cells, zones): (Vec<_>, Vec<_>) = lines.map(|l| (l.cells, l.zones)).unzip();
        self.zones = zones;
        self.screen.draw(cells, w, h);
    }

    /// Stop on Ctrl-Z: hand the terminal back, resume cleanly on `fg`.
    fn suspend(&mut self) {
        self.screen.leave();
        term::restore();
        let _ = signal_hook::low_level::emulate_default_handler(libc::SIGTSTP);
        term::cbreak();
        self.size = term::size(); // may have changed while we were stopped
        self.screen.enter(self.st.mouse);
    }

    pub fn run(&mut self) {
        term::cbreak();
        self.screen.enter(self.st.mouse);
        self.next_tick = Instant::now() + self.interval();
        let mut last = Instant::now();
        let (mut pending, mut pending_t) = (Vec::<u8>::new(), Instant::now());
        let frame = Duration::from_secs_f64(1.0 / FPS);
        while !self.quit {
            let now = Instant::now();
            supervise(&self.workers, now, self.hard);
            self.schedule(now);
            let seen = DATA_GEN.load(SeqCst);
            self.build(now, Some(now.duration_since(last).as_secs_f64().min(1.5 / FPS)));
            last = now;
            UI_BUSY.store(self.renderer.animating, SeqCst);
            if !self.renderer.animating && DATA_GEN.load(SeqCst) != seen {
                continue; // a host changed while we built and didn't wake us: draw it now
            }

            // the next frame of its own: at FPS while animating, else at the next
            // spinner glyph or the clock's next second
            let redraw_at = if self.renderer.animating {
                now + frame
            } else {
                let to_second =
                    1.0 - SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64().fract() + 0.01;
                let secs = if self.renderer.spinning { self.renderer.spinner_in(now) } else { to_second };
                now + Duration::from_secs_f64(secs.min(to_second))
            };
            let esc_at = pending_t + Duration::from_millis(50);
            let mut msgs = Vec::new();
            loop {
                let mut until = redraw_at;
                if !self.paused {
                    until = until.min(self.next_tick);
                }
                if !pending.is_empty() {
                    until = until.min(esc_at);
                }
                match self.rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                    Ok(m) => {
                        msgs.push(m);
                        msgs.extend(self.rx.try_iter());
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
                let t = Instant::now();
                self.schedule(t); // sample requests go out on time without forcing a redraw
                // keys and signals: now. New data: now if idle, else with the next
                // animation frame (5 hosts at 100 ms would otherwise add 50 frames/s)
                let urgent =
                    msgs.iter().any(|m| !matches!(m, Msg::Data)) || (!msgs.is_empty() && !self.renderer.animating);
                if urgent || t >= redraw_at || (!pending.is_empty() && t >= esc_at) {
                    break;
                }
            }
            let mut got_input = false;
            for m in msgs {
                match m {
                    Msg::Data => {}
                    Msg::Input(b) => {
                        pending.extend(b);
                        (pending_t, got_input) = (Instant::now(), true);
                    }
                    Msg::InputEof => return,
                    Msg::Signal(libc::SIGWINCH) => self.size = term::size(),
                    Msg::Signal(libc::SIGTSTP) => self.suspend(),
                    Msg::Signal(_) => return, // INT / TERM / HUP: quit, settings still saved
                }
            }
            if !got_input && !pending.is_empty() && pending_t.elapsed() > Duration::from_millis(50) {
                if pending == b"\x1b" {
                    self.on_event(Ev::Key(Key::Esc)); // a real Esc press, not a sequence prefix
                }
                pending.clear();
            }
            if !pending.is_empty() {
                let (events, rest) = parse_input(&pending);
                pending = rest;
                for ev in events {
                    self.on_event(ev);
                    if self.quit {
                        break;
                    }
                }
            }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.screen.leave();
        term::restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_sequences() {
        let (ev, rest) = parse_input(b"q\x1b[A\x1b[6~\x1b[<0;12;3M\x1b[<0;12;3m\x1bOD");
        assert_eq!(
            ev,
            [Ev::Key(Key::Char('q')), Ev::Key(Key::Up), Ev::Key(Key::PgDn), Ev::Mouse(0, 11, 2), Ev::Key(Key::Left)]
        );
        assert!(rest.is_empty());
        let (ev, rest) = parse_input(b"\x1b[<0;1"); // split mid-sequence: wait for more
        assert!(ev.is_empty() && rest == b"\x1b[<0;1");
        let (ev, rest) = parse_input(b"\x1b]11;rgb:0/0/0\x07r"); // late OSC reply is swallowed
        assert_eq!((ev, rest.len()), (vec![Ev::Key(Key::Char('r'))], 0));
        assert_eq!(parse_input(b"\x1b").1, b"\x1b");
    }
}
