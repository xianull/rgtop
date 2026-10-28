//! gpustatus: btop-style live GPU/CPU dashboard for remote servers over SSH.

mod app;
mod collect;
mod draw;
mod settings;
mod term;
mod theme;

use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use app::{App, Msg};
use collect::{Worker, ssh_command, supervise};
use draw::{FPS, Renderer};
use settings::{Settings, THEMES, choice, config_dir, load_settings, save_settings};
use theme::{Theme, detect_theme};

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Args {
    hosts: Vec<String>,
    once: bool,
    interval: Option<f64>,
    timeout: u64,
    theme: Option<&'static str>,
    reset: bool,
}

fn usage() -> String {
    let cfg = config_dir().display().to_string();
    format!(
        "\
gpustatus {VERSION}: btop-style live GPU/CPU dashboard for remote servers over SSH.
Press ? inside the dashboard for keys.

usage: gpustatus [options] [HOST ...]

  HOST              ssh host aliases (default: $GPUSTATUS_HOSTS, else {cfg}/hosts)
  -1, --once        print one snapshot and exit (implied when stdout is not a terminal)
  -n, --interval S  refresh interval in seconds; adjustable live with -/+ (default: saved, else 2)
  -t, --timeout S   SSH connect timeout in seconds (default: 8)
      --theme T     auto | dark | light (default: saved, else $GPUSTATUS_THEME, else auto)
      --reset       start from default settings (overwrites {cfg}/settings.json on exit)
  -V, --version     print the version
  -h, --help        print this help
"
    )
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { hosts: Vec::new(), once: false, interval: None, timeout: 8, theme: None, reset: false };
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
            _ if !arg.starts_with("--") && arg.len() > 2 && matches!(&arg[..2], "-n" | "-t") => {
                (arg[..2].to_string(), Some(arg[2..].to_string()))
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
                a.interval = Some(v.parse().map_err(|_| format!("bad interval: {v}"))?);
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
            "-V" | "--version" => {
                println!("gpustatus {VERSION}");
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

fn load_hosts(cli: Vec<String>) -> Vec<String> {
    let mut hosts = cli;
    if hosts.is_empty() {
        hosts = std::env::var("GPUSTATUS_HOSTS")
            .unwrap_or_default()
            .replace(',', " ")
            .split_whitespace()
            .map(String::from)
            .collect();
    }
    if hosts.is_empty()
        && let Ok(text) = std::fs::read_to_string(config_dir().join("hosts"))
    {
        hosts =
            text.lines().flat_map(|l| l.split('#').next().unwrap_or("").split_whitespace()).map(String::from).collect();
    }
    let mut seen = std::collections::HashSet::new();
    hosts.retain(|h| seen.insert(h.clone()));
    hosts
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
            let fr = renderer.frame(&snaps(workers), st, w, Some(h.saturating_sub(1)), now, Some(dt), false, false);
            let mut out = if shown > 1 { format!("\r\x1b[{}A\x1b[J", shown - 1) } else { "\r\x1b[J".into() };
            let lines: Vec<String> = std::iter::once(fr.header.clone())
                .chain(fr.rows(w))
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
    let fr = renderer.frame(&hosts, st, w, None, Instant::now(), None, false, false);
    let mut out: Vec<String> =
        std::iter::once(fr.header.clone()).chain(fr.rows(w)).map(|l| term::encode(&l.cells, cs)).collect();
    out.push(String::new());
    term::write_out(&out.join("\n"));
    if hosts.iter().any(|h| h.sample.is_some()) { 0 } else { 1 }
}

fn run() -> i32 {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("gpustatus: {e}\n\n{}", usage());
            return 2;
        }
    };
    let hosts = load_hosts(args.hosts);
    if hosts.is_empty() {
        eprintln!(
            "gpustatus: no hosts. Pass ssh host aliases as arguments, set $GPUSTATUS_HOSTS, \
                   or list them in {}",
            config_dir().join("hosts").display()
        );
        return 2;
    }
    let path = config_dir().join("settings.json");
    let mut st = if args.reset { Settings::default() } else { load_settings(&path) };
    if !path.exists()
        && args.theme.is_none()
        && let Some(t) = choice(&std::env::var("GPUSTATUS_THEME").unwrap_or_default(), &THEMES)
    {
        st.theme = t;
    }
    if let Some(t) = args.theme {
        st.theme = t;
    }
    if let Some(iv) = args.interval {
        st.interval_ms = (iv.clamp(0.25, 600.0) * 1000.0) as u64;
    }

    let tty = std::io::stdout().is_terminal();
    let live = tty && !args.once;
    let auto = if tty { detect_theme() } else { "dark" };
    let th = Theme::new(if st.theme == "auto" { auto } else { st.theme });
    let home = std::env::var("HOME").unwrap_or_default();
    let cfg = path.display().to_string();
    let cfg = if !home.is_empty() && cfg.starts_with(&home) { cfg.replacen(&home, "~", 1) } else { cfg };
    let mut renderer = Renderer::new(th, live, cfg);

    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let want_procs = Arc::new(AtomicBool::new(st.show_proc || st.show_user));
    let workers: Vec<Arc<Worker>> = hosts
        .iter()
        .map(|h| {
            Arc::new(Worker::new(h, ssh_command(h, args.timeout), !live, tx.clone(), stop.clone(), want_procs.clone()))
        })
        .collect();
    let hard = args.timeout as f64 + 7.0;

    use libc::{SIGHUP, SIGINT, SIGTERM, SIGTSTP, SIGWINCH};
    let sigs: &[i32] = if live { &[SIGWINCH, SIGTSTP, SIGINT, SIGTERM, SIGHUP] } else { &[SIGINT, SIGTERM, SIGHUP] };
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
                use std::io::Read;
                let mut buf = [0u8; 1024];
                loop {
                    match std::io::stdin().read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let _ = tx.send(Msg::Input(buf[..n].to_vec()));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                let _ = tx.send(Msg::InputEof);
            });
        }
        let mut app = App::new(st, workers.clone(), renderer, rx, want_procs, auto, hard);
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
