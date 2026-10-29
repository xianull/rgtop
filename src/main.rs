//! rgtop: btop-style live GPU/CPU dashboard for remote servers over SSH.

mod app;
mod collect;
mod draw;
mod settings;
mod term;
mod theme;
mod update;

use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::SeqCst};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use app::{App, INPUT_PAUSED, Msg, want_level};
use collect::{Worker, supervise};
use draw::{FPS, Renderer};
use settings::{Settings, THEMES, choice, config_dir, config_file, load_settings, save_settings};
use theme::{Theme, detect_theme};

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Args {
    hosts: Vec<String>,
    once: bool,
    interval: Option<f64>,
    timeout: u64,
    theme: Option<&'static str>,
    reset: bool,
    update: bool,
}

fn usage() -> String {
    let cfg = config_dir().display().to_string();
    format!(
        "\
rgtop {VERSION}: btop-style live GPU/CPU dashboard for remote servers over SSH.
Press ? inside the dashboard for keys.

usage: rgtop [options] [HOST ...]

  HOST              ssh host aliases (default: $RGTOP_HOSTS, else {cfg}/hosts)
  -1, --once        print one snapshot and exit (implied when stdout is not a terminal)
  -n, --interval S  refresh interval in seconds, from 0.1, or auto (as fast as hosts answer);
                    -/+ adjust it live by 100 ms
                    (default: saved, else 2)
  -t, --timeout S   SSH connect timeout in seconds (default: 8)
      --theme T     auto | dark | light (default: saved, else $RGTOP_THEME, else auto)
      --reset       start from default settings (overwrites {cfg}/settings.json on exit)
      --update      install the latest release over this binary (runs install.sh)
  -V, --version     print the version
  -h, --help        print this help
"
    )
}

fn parse_args() -> Result<Args, String> {
    let mut a =
        Args { hosts: Vec::new(), once: false, interval: None, timeout: 8, theme: None, reset: false, update: false };
    let mut it = std::env::args().skip(1);
    let mut only_hosts = false;
    while let Some(arg) = it.next() {
        if only_hosts || !arg.starts_with('-') || arg == "-" {
            a.hosts.push(arg);
            continue;
        }
        // --opt=value, -nVALUE, or -n VALUE
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ if !arg.starts_with("--") && arg.len() > 2 && matches!(arg.get(..2), Some("-n" | "-t")) => {
                (arg[..2].to_string(), Some(arg[2..].strip_prefix('=').unwrap_or(&arg[2..]).to_string()))
            }
            _ => (arg.clone(), None),
        };
        let mut value =
            |name: &str| inline.clone().or_else(|| it.next()).ok_or_else(|| format!("{name} needs a value"));
        match flag.as_str() {
            "--" => only_hosts = true,
            "-1" | "--once" => a.once = true,
            "-n" | "--interval" => {
                let v = value(&flag)?;
                let iv = if v == "auto" { Some(0.0) } else { v.parse().ok().filter(|x: &f64| x.is_finite()) };
                a.interval = Some(iv.ok_or(format!("bad interval: {v}"))?);
            }
            "-t" | "--timeout" => {
                let v = value(&flag)?;
                a.timeout = v.parse::<u64>().map_err(|_| format!("bad timeout: {v}"))?.max(1);
            }
            "--theme" => {
                let v = value(&flag)?;
                a.theme =
                    Some(choice(&v, &THEMES).ok_or_else(|| format!("theme must be one of {}", THEMES.join(", ")))?);
            }
            "--reset" => a.reset = true,
            "--update" => a.update = true,
            "-V" | "--version" => {
                println!("rgtop {VERSION}");
                std::process::exit(0);
            }
            "-h" | "--help" => {
                print!("{}", usage());
                std::process::exit(0);
            }
            _ => return Err(format!("unknown option: {arg}")),
        }
    }
    Ok(a)
}

/// $RGTOP_<name>, else the variable from before the rename ($GPUSTATUS_<name>).
fn env(name: &str) -> Option<String> {
    std::env::var(format!("RGTOP_{name}")).or_else(|_| std::env::var(format!("GPUSTATUS_{name}"))).ok()
}

fn load_hosts(cli: Vec<String>) -> Vec<String> {
    let mut hosts = cli;
    if hosts.is_empty() {
        hosts = env("HOSTS").unwrap_or_default().replace(',', " ").split_whitespace().map(String::from).collect();
    }
    if hosts.is_empty()
        && let Ok(text) = std::fs::read_to_string(config_file("hosts"))
    {
        hosts =
            text.lines().flat_map(|l| l.split('#').next().unwrap_or("").split_whitespace()).map(String::from).collect();
    }
    let mut seen = std::collections::HashSet::new();
    hosts.retain(|h| seen.insert(h.clone()));
    hosts
}

/// `Host` aliases in an ssh config (patterns skipped), for the "no hosts" hint.
fn ssh_aliases(config: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for l in config.lines() {
        let mut w = l.split(|c: char| c.is_whitespace() || c == '=').filter(|s| !s.is_empty());
        if w.next().is_some_and(|k| k.eq_ignore_ascii_case("host")) {
            out.extend(w.take_while(|h| !h.starts_with('#')).filter(|h| !h.contains(['*', '?', '!'])));
        }
    }
    out
}

fn snaps(workers: &[Arc<Worker>]) -> Vec<collect::Host> {
    workers.iter().map(|w| w.host.lock().unwrap().clone()).collect()
}

/// Wait for every host to answer once (or fail), then print a snapshot.
/// On a terminal the cards fill in live while hosts report in.
fn run_once(workers: &[Arc<Worker>], renderer: &mut Renderer, st: &Settings, rx: &Receiver<Msg>, hard: f64) -> i32 {
    let tty = std::io::stdout().is_terminal();
    let cs = if tty { term::detect_colors() } else { term::Colors::None };
    let alive = || workers.iter().any(|w| !w.done.load(SeqCst));
    let interrupted = |m: Result<Msg, RecvTimeoutError>| matches!(m, Ok(Msg::Signal(_)));
    let mut shown = 0;
    let mut last = Instant::now();
    loop {
        let now = Instant::now();
        supervise(workers, now, hard);
        if tty {
            let (w, h) = term::size();
            let dt = now.duration_since(last).as_secs_f64().min(1.5 / FPS);
            last = now;
            let mut fr = renderer.frame(&snaps(workers), st, w, Some(h.saturating_sub(1)), now, Some(dt), false, false);
            let mut out = if shown > 1 { format!("\r\x1b[{}A\x1b[J", shown - 1) } else { "\r\x1b[J".into() };
            let lines: Vec<String> = std::iter::once(fr.header.clone())
                .chain(fr.take_rows(w))
                .take(h.saturating_sub(1).max(1))
                .map(|l| term::encode(&l.cells, cs))
                .collect();
            out += &lines.join("\n");
            shown = lines.len();
            term::write_out(&out);
            if !alive() && !renderer.animating {
                term::write_out(&if shown > 1 { format!("\r\x1b[{}A\x1b[J", shown - 1) } else { "\r\x1b[J".into() });
                break;
            }
        } else if !alive() {
            break;
        }
        if interrupted(rx.recv_timeout(Duration::from_secs_f64(if tty { 1.0 / FPS } else { 0.25 }))) {
            return 130;
        }
    }
    let w = if tty { term::size().0 } else { std::env::var("COLUMNS").ok().and_then(|v| v.parse().ok()).unwrap_or(80) };
    let hosts = snaps(workers);
    let mut fr = renderer.frame(&hosts, st, w, None, Instant::now(), None, false, false);
    let mut out: Vec<String> =
        std::iter::once(fr.header.clone()).chain(fr.take_rows(w)).map(|l| term::encode(&l.cells, cs)).collect();
    out.push(String::new());
    term::write_out(&out.join("\n"));
    if hosts.iter().any(|h| h.sample.is_some()) { 0 } else { 1 }
}

fn run() -> i32 {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("rgtop: {e}\n\n{}", usage());
            return 2;
        }
    };
    if args.update {
        return update::update();
    }
    let hosts = load_hosts(args.hosts);
    if hosts.is_empty() {
        eprintln!(
            "rgtop: no hosts. Pass ssh host aliases as arguments, set $RGTOP_HOSTS, \
                   or list them in {}",
            config_dir().join("hosts").display()
        );
        let config = std::fs::read_to_string(format!("{}/.ssh/config", std::env::var("HOME").unwrap_or_default()));
        let known = ssh_aliases(config.as_deref().unwrap_or(""));
        if !known.is_empty() {
            eprintln!("Hosts in ~/.ssh/config: {}", known.join(" "));
        }
        return 2;
    }
    let path = config_dir().join("settings.json");
    let saved = config_file("settings.json");
    let mut st = if args.reset { Settings::default() } else { load_settings(&saved) };
    if !saved.exists()
        && args.theme.is_none()
        && let Some(t) = choice(&env("THEME").unwrap_or_default(), &THEMES)
    {
        st.theme = t;
    }
    if let Some(t) = args.theme {
        st.theme = t;
    }
    if let Some(iv) = args.interval {
        st.interval_ms = if iv <= 0.0 { settings::AUTO } else { (iv.clamp(0.1, 600.0) * 1000.0).round() as u64 };
    }

    let tty = std::io::stdout().is_terminal();
    let live = tty && !args.once;
    if live && env("NO_UPDATE_CHECK").is_none_or(|v| v.is_empty()) {
        update::check();
    }
    let auto = if tty { detect_theme() } else { "dark" };
    let th = Theme::new(if st.theme == "auto" { auto } else { st.theme });
    let home = std::env::var("HOME").unwrap_or_default();
    let cfg = path.display().to_string();
    let cfg = if !home.is_empty() && cfg.starts_with(&home) { cfg.replacen(&home, "~", 1) } else { cfg };
    let mut renderer = Renderer::new(th, live, cfg);

    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let want = Arc::new(AtomicU8::new(want_level(&st)));
    let workers: Vec<Arc<Worker>> = hosts
        .iter()
        .map(|h| Arc::new(Worker::new(h, args.timeout, !live, tx.clone(), stop.clone(), want.clone())))
        .collect();
    let hard = args.timeout as f64 + 7.0;

    use libc::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGTSTP, SIGWINCH};
    let sigs: &[i32] =
        if live { &[SIGWINCH, SIGTSTP, SIGINT, SIGQUIT, SIGTERM, SIGHUP] } else { &[SIGINT, SIGQUIT, SIGTERM, SIGHUP] };
    if let Ok(mut signals) = signal_hook::iterator::Signals::new(sigs) {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for s in signals.forever() {
                let _ = tx.send(Msg::Signal(s));
            }
        });
    }
    for w in &workers {
        let w = w.clone();
        std::thread::spawn(move || w.run());
    }

    let rc = if live {
        if std::io::stdin().is_terminal() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                // poll, then read: while INPUT_PAUSED (an ssh login owns the
                // terminal) nothing is read, so the password goes to ssh. No
                // timeout: an idle rgtop shouldn't wake up 10 times a second
                let mut buf = [0u8; 1024];
                loop {
                    let mut pfd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
                    if INPUT_PAUSED.load(SeqCst) {
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    if unsafe { libc::poll(&mut pfd, 1, -1) } <= 0 || INPUT_PAUSED.load(SeqCst) {
                        continue;
                    }
                    let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
                    if n > 0 {
                        let _ = tx.send(Msg::Input(buf[..n as usize].to_vec()));
                    } else if n == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                        break;
                    }
                }
                let _ = tx.send(Msg::InputEof);
            });
        }
        let mut app = App::new(st, workers.clone(), renderer, rx, want, auto, hard);
        app.run();
        save_settings(&path, &app.st);
        0
    } else {
        run_once(&workers, &mut renderer, &st, &rx, hard)
    };
    stop.store(true, SeqCst);
    for w in &workers {
        w.kill(None);
    }
    rc
}

fn main() {
    term::install_panic_hook();
    std::process::exit(run());
}

#[cfg(test)]
mod tests {
    #[test]
    fn ssh_aliases() {
        let cfg = "Host gpu-1 gpu-2 # lab\n  HostName 10.0.0.1\nhost=bastion\nHost *.lab !x gpu-?\nHostName y\n";
        assert_eq!(super::ssh_aliases(cfg), ["gpu-1", "gpu-2", "bastion"]);
    }
}
