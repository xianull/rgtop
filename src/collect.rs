//! Remote sampling over SSH.
//!
//! Each host gets ONE long-lived SSH session running a small agent (agent.py,
//! or agent.sh where there is no python3). Every request line written to its
//! stdin produces exactly one sample frame on stdout, so the local side decides
//! when to sample: the refresh interval can change instantly, pausing costs
//! nothing, and no sample ever pays for a new handshake. agent.py reads GPUs
//! through NVML and /proc in-process, so a sample forks nothing remotely.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU64, Ordering::SeqCst};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::app::Msg;

static SPAWN: Mutex<()> = Mutex::new(());

/// Bumped on every host change. While the UI is animating (UI_BUSY) it redraws
/// within a frame anyway, so workers skip the wake-up: with 5 hosts at 100 ms
/// that is 50 needless wake-ups a second. Before sleeping, the UI clears
/// UI_BUSY and re-checks DATA_GEN, so no change is missed.
pub static DATA_GEN: AtomicU64 = AtomicU64::new(0);
pub static UI_BUSY: AtomicBool = AtomicBool::new(false);

pub const HISTORY: usize = 240; // history points kept per GPU / CPU for the sparklines
// one history point per sample, but samples closer than this share one (max), so
// at 100 ms / auto a sparkline still spans a minute rather than seconds
const BUCKET: Duration = Duration::from_millis(240);
const IDLE_PCT: f64 = 5.0; // util AND mem below this -> idle  (green)
const LIGHT_PCT: f64 = 40.0; // util AND mem below this -> light (yellow), otherwise busy (red)
pub const UNKNOWN: &str = "?"; // owner of a GPU process whose PID isn't visible on the host

/// What each request asks the agent for.
pub const WANT_SYS: u8 = 0; // system + GPUs
pub const WANT_PROCS: u8 = 1; // + GPU processes (USER column)
pub const WANT_HOST: u8 = 2; // + top CPU users of the host (process list)

/// The agent exits after 60 s without input: a connection whose close never
/// reached the server (proxies, VPNs, sleep) would otherwise leave it running
/// for good. So every host hears from us at least this often, paused or not.
const KEEPALIVE: Duration = Duration::from_secs(15);

const AGENT_PY: &str = include_str!("agent.py");
const AGENT_SH: &str = include_str!("agent.sh");

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

/// Where `rgtop` keeps a host's login when the host wants a password: an
/// OpenSSH master connection (ControlMaster) that later sessions ride on.
/// One-to-one ("a/b" -> "a+2fb"), so two hosts never share a login: a key
/// installed over it would reach the wrong server.
pub fn mux_socket(host: &str) -> PathBuf {
    let safe: String = host
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"._-".contains(&b) { (b as char).to_string() } else { format!("+{b:02x}") }
        })
        .collect();
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".ssh").join(format!("rgtop-{safe}"))
}

/// Errors only an interactive login can fix: a password / one-time code, or a
/// host key to confirm. rgtop runs ssh with BatchMode, which never asks.
pub fn is_auth_error(msg: &str) -> bool {
    [
        "Permission denied",
        "Host key verification failed",
        "Too many authentication failures",
        "No more authentication methods",
    ]
    .iter()
    .any(|p| msg.contains(p))
}

/// Interactive login: ssh asks for the password itself (rgtop never sees it),
/// then stays in the background as the master connection: while used, and 8 h after.
pub fn login_command(host: &str, timeout: u64) -> Vec<String> {
    let mut v: Vec<String> = ["ssh", "-f", "-N"].map(String::from).to_vec();
    for o in [
        "ControlMaster=yes".to_string(),
        format!("ControlPath={}", mux_socket(host).display()),
        "ControlPersist=8h".into(),
        format!("ConnectTimeout={timeout}"),
        // the sessions riding on the master send no keepalives of their own: without
        // these a dead network (sleep, Wi-Fi change) would leave the host stale for
        // hours. 30 s, not 15 like the sessions: losing this costs a password.
        "ServerAliveInterval=5".into(),
        "ServerAliveCountMax=6".into(),
        "ClearAllForwardings=yes".into(),
        "RemoteCommand=none".into(),
        "PermitLocalCommand=no".into(),
        "LogLevel=ERROR".into(),
    ] {
        v.extend(["-o".into(), o]);
    }
    v.extend(["--".into(), host.into()]);
    v
}

pub fn ssh_command(host: &str, timeout: u64) -> Vec<String> {
    // agent.py if this python3 can run it (-I -S: no user site, PYTHONPATH or
    // sitecustomize, just the stdlib); it execs agent.sh itself if NVML is unusable
    const LAUNCHER: &str = "python3 -I -S -c 'import ctypes, pwd' >/dev/null 2>&1 && \
                            exec python3 -I -S -c \"$1\" \"$2\"\nexec sh -c \"$2\"\n";
    let remote = format!("sh -c {} rgtop {} {}", sh_quote(LAUNCHER), sh_quote(AGENT_PY), sh_quote(AGENT_SH));
    let sock = mux_socket(host);
    // logged in through rgtop: ride on that connection (no password needed)
    let master = sock.exists().then(|| sock.display().to_string());
    batch_command(host, timeout, master.as_deref(), &remote)
}

/// ssh running `remote`, never asking for anything. `master`: a ControlPath
/// to ride on, or "none" for a connection of its own whatever ~/.ssh/config says.
pub fn batch_command(host: &str, timeout: u64, master: Option<&str>, remote: &str) -> Vec<String> {
    let mut v: Vec<String> = ["ssh", "-T", "-x", "-a"].map(String::from).to_vec();
    for o in [
        "BatchMode=yes".to_string(),
        format!("ConnectTimeout={timeout}"),
        "ConnectionAttempts=1".into(),
        "ServerAliveInterval=5".into(),
        "ServerAliveCountMax=3".into(),
        "ClearAllForwardings=yes".into(), // skip RemoteForward/LocalForward from ~/.ssh/config
        "RemoteCommand=none".into(),
        "PermitLocalCommand=no".into(),
        "LogLevel=ERROR".into(),
    ] {
        v.push("-o".into());
        v.push(o);
    }
    if let Some(p) = master {
        v.extend(["-o".into(), "ControlMaster=no".into(), "-o".into(), format!("ControlPath={p}")]);
    }
    v.extend(["--".into(), host.into(), remote.into()]);
    v
}

/// Appends the public key `$1` to ~/.ssh/authorized_keys, like ssh-copy-id.
/// Already there (commented out, restricted, or a file sshd doesn't read): an
/// error, never a second copy that would undo an admin's edit.
const INSTALL_KEY: &str = "umask 077; : \"${HOME:?}\"; d=\"$HOME/.ssh\"; f=\"$d/authorized_keys\"; \
    mkdir -p \"$d\" && touch \"$f\" || exit 1; k=$(printf '%s\\n' \"$1\" | cut -d' ' -f1,2); \
    if grep -qF \"$k\" \"$f\"; then echo \"the key is in ~/.ssh/authorized_keys there already, \
    but not accepted (disabled, restricted, or not the file sshd reads)\" >&2; exit 3; fi; \
    { [ ! -s \"$f\" ] || [ -z \"$(tail -c1 \"$f\")\" ] || echo; printf '%s\\n' \"$1\"; } >> \"$f\" || exit 1; \
    command -v restorecon >/dev/null 2>&1 && restorecon -F \"$d\" \"$f\" >/dev/null 2>&1; exit 0";

/// Install `key` (a public key line) on `host` over its login's master connection.
pub fn install_key_command(host: &str, timeout: u64, key: &str) -> Vec<String> {
    let remote = format!("sh -c {} rgtop {}", sh_quote(INSTALL_KEY), sh_quote(key));
    batch_command(host, timeout, Some(&mux_socket(host).display().to_string()), &remote)
}

/// The first key file ssh tries for `host` (IdentityFile, `ssh -G`) that has a
/// valid .pub next to it. -> (that .pub, its key line)
/// ponytail: keys that live only in an agent (no .pub file) aren't offered.
pub fn public_key(host: &str) -> Option<(PathBuf, String)> {
    let out = Command::new("ssh").args(["-G", "--", host]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let home = std::env::var("HOME").unwrap_or_default();
    String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.strip_prefix("identityfile ")).find_map(|f| {
        let p = PathBuf::from(f.strip_prefix("~/").map_or(f.to_string(), |rest| format!("{home}/{rest}")) + ".pub");
        if !p.is_absolute() {
            return None; // "none", relative or unexpanded %-tokens: would read from the current directory
        }
        Some((p.clone(), pub_key_line(std::fs::read_to_string(&p).ok()?.lines().next()?)?))
    })
}

/// "type base64 [comment]" when `line` is a public key; a comment that isn't
/// plain text is dropped (the line passes through the remote login shell).
fn pub_key_line(line: &str) -> Option<String> {
    let mut w = line.split_whitespace();
    let (t, b) = (w.next()?, w.next()?);
    let key = ["ssh-", "ecdsa-", "sk-"].iter().any(|p| t.starts_with(p))
        && t.bytes().all(|c| c.is_ascii_alphanumeric() || b"-@.".contains(&c))
        && b.bytes().all(|c| c.is_ascii_alphanumeric() || b"+/=".contains(&c));
    let comment: Vec<&str> = w.collect();
    let plain = comment.iter().all(|c| c.bytes().all(|c| c.is_ascii_alphanumeric() || b"@._-".contains(&c)));
    key.then(
        || if plain && !comment.is_empty() { format!("{t} {b} {}", comment.join(" ")) } else { format!("{t} {b}") },
    )
}

// ── data model ───────────────────────────────────────────────────────────────
#[derive(Clone, Debug)]
pub struct Gpu {
    pub idx: u32,
    pub uuid: String,
    pub name: String,
    pub util: Option<f64>,
    pub mem_used: f64,  // MiB
    pub mem_total: f64, // MiB
    pub temp: Option<f64>,
    pub power: Option<f64>,
    pub power_limit: Option<f64>,
    pub users: Vec<String>, // by VRAM used, largest first (only with process data)
}

impl Gpu {
    pub fn mem_pct(&self) -> f64 {
        if self.mem_total > 0.0 { 100.0 * self.mem_used / self.mem_total } else { 0.0 }
    }

    /// 0 = idle, 1 = light, 2 = busy.
    pub fn state(&self) -> usize {
        let (u, m) = (self.util.unwrap_or(0.0), self.mem_pct());
        if u < IDLE_PCT && m < IDLE_PCT {
            0
        } else if u < LIGHT_PCT && m < LIGHT_PCT {
            1
        } else {
            2
        }
    }
}

#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: u32, // 0 = summary row for processes not visible on the host
    pub user: String,
    pub etime: Option<u64>, // seconds since start
    pub cmd: String,
    pub gpus: Vec<u32>,
    pub mem: f64,            // MiB, summed over its GPUs
    pub cpu: Option<f64>,    // current %CPU (100 = one core); agent.py only
    pub rss_kb: Option<u64>, // host memory; agent.py only
}

#[derive(Clone, Debug, Default)]
pub struct Sample {
    pub gpus: Vec<Gpu>,
    pub gpu_error: Option<String>,
    pub cpu_pct: Option<f64>,
    pub core_pct: Vec<f32>, // per logical core; empty from agent.sh
    pub cpu_temp: Option<f64>,
    pub cpu_model: String,
    pub sockets: u32,
    pub load: Option<[f64; 3]>,
    pub cores: Option<u32>,
    pub mem_total_kb: u64,
    pub mem_avail_kb: u64,
    pub mem_cache_kb: u64,
    pub swap_total_kb: u64,
    pub swap_free_kb: u64,
    pub uptime: Option<f64>,
    pub stat: Option<(u64, u64)>,            // (total, idle) jiffies at the end of the frame
    pub procs: Option<Vec<Proc>>,            // None = processes weren't requested
    pub host_users: Vec<(String, f64, u64)>, // top CPU users of the host: (user, %CPU, RSS kB)
    pub me: String,                          // remote login user
}

fn num(s: &str) -> Option<f64> {
    s.trim().parse().ok().filter(|v: &f64| v.is_finite()) // "[N/A]", "[Not Supported]", ... -> None
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn parse_stat(rest: &str) -> Option<(u64, u64)> {
    let p: Vec<&str> = rest.split_whitespace().collect();
    if p.len() < 5 || p[0] != "cpu" {
        return None;
    }
    let v: Vec<u64> = p[1..p.len().min(9)].iter().map(|x| x.parse().ok()).collect::<Option<_>>()?;
    Some((v.iter().sum(), v[3] + v.get(4).copied().unwrap_or(0))) // idle + iowait
}

fn busy(a: Option<(u64, u64)>, b: Option<(u64, u64)>) -> Option<f64> {
    let (a, b) = (a?, b?);
    let dt = b.0 as f64 - a.0 as f64;
    let di = b.1 as f64 - a.1 as f64;
    (dt > 0.0).then(|| (100.0 * (dt - di) / dt).clamp(0.0, 100.0))
}

/// Up to `n` whitespace-separated fields, then the untouched remainder.
fn fields(s: &str, n: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s.trim_start();
    while out.len() < n && !rest.is_empty() {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        out.push(&rest[..end]);
        rest = rest[end..].trim_start();
    }
    if !rest.is_empty() {
        out.push(rest.trim_end());
    }
    out
}

fn u64s<const N: usize>(rest: &str) -> Option<[u64; N]> {
    let v: Vec<u64> = rest.split_whitespace().take(N).map_while(|x| x.parse().ok()).collect();
    v.try_into().ok()
}

pub fn parse_frame(lines: &[String], prev_stat: Option<(u64, u64)>) -> Sample {
    let mut s = Sample { sockets: 1, ..Sample::default() };
    let mut junk: Vec<&str> = Vec::new();
    let mut stats: Vec<Option<(u64, u64)>> = Vec::new();
    let mut apps: Vec<(&str, u32, Option<f64>)> = Vec::new();
    let mut pinfo: HashMap<u32, (&str, Option<u64>, &str)> = HashMap::new();
    let mut qinfo: HashMap<u32, (Option<f64>, Option<u64>)> = HashMap::new();
    let (mut rc, mut want) = (0i32, false);
    for line in lines {
        if let Some(tagged) = line.strip_prefix("@@") {
            let (tag, rest) = tagged.split_once(' ').unwrap_or((tagged, ""));
            match tag {
                "S" => stats.push(parse_stat(rest)),
                "G" => rc = rest.trim().parse().unwrap_or(rc),
                "L" => {
                    let t: Vec<f64> = rest.split_whitespace().take(3).map_while(num).collect();
                    s.load = t.try_into().ok();
                }
                "M" => {
                    if let Some([t, a]) = u64s(rest) {
                        (s.mem_total_kb, s.mem_avail_kb) = (t, a);
                    }
                }
                "W" => {
                    if let Some([c, st, sf]) = u64s(rest) {
                        (s.mem_cache_kb, s.swap_total_kb, s.swap_free_kb) = (c, st, sf);
                    }
                }
                "N" => s.cores = rest.trim().parse().ok(),
                "U" => s.me = rest.trim().into(),
                "C" => {
                    s.core_pct =
                        rest.trim().chars().map(|c| c.to_digit(36).unwrap_or(0) as f32 * 100.0 / 35.0).collect()
                }
                "T" => s.cpu_temp = num(rest),
                "O" => s.uptime = num(rest),
                "I" => {
                    let (n, model) = rest.split_once(' ').unwrap_or((rest, ""));
                    s.sockets = n.parse().unwrap_or(1).max(1);
                    s.cpu_model = model.trim().into();
                }
                "X" => want = true,
                "A" => {
                    let p: Vec<&str> = rest.split(',').map(str::trim).collect();
                    if p.len() >= 3
                        && digits(p[1])
                        && let Ok(pid) = p[1].parse()
                    {
                        apps.push((p[0], pid, num(p[2])));
                    }
                }
                "P" => {
                    let f = fields(rest, 3);
                    if f.len() >= 3
                        && digits(f[0])
                        && let Ok(pid) = f[0].parse()
                    {
                        pinfo.insert(pid, (f[1], f[2].parse().ok(), f.get(3).copied().unwrap_or("")));
                    }
                }
                "Q" => {
                    if let [pid, cpu, rss] = fields(rest, 3)[..]
                        && let Ok(pid) = pid.parse()
                    {
                        qinfo.insert(pid, (num(cpu), rss.parse().ok()));
                    }
                }
                "H" => {
                    if let [user, cpu, rss] = fields(rest, 3)[..] {
                        s.host_users.push((user.into(), num(cpu).unwrap_or(0.0), rss.parse().unwrap_or(0)));
                    }
                }
                _ => {}
            }
            continue;
        }
        let row: Vec<&str> = line.split(',').map(str::trim).collect();
        if row.len() >= 9 && digits(row[0]) {
            let n = &row[row.len() - 6..];
            s.gpus.push(Gpu {
                idx: row[0].parse().unwrap_or(0),
                uuid: row[1].into(),
                name: row[2..row.len() - 6].join(", "),
                util: num(n[0]),
                mem_used: num(n[1]).unwrap_or(0.0),
                mem_total: num(n[2]).unwrap_or(0.0),
                temp: num(n[3]),
                power: num(n[4]),
                power_limit: num(n[5]),
                users: Vec::new(),
            });
        } else if !line.trim().is_empty() {
            junk.push(line.trim());
        }
    }

    let gpus = &mut s.gpus;
    s.gpu_error = if rc == 127 && gpus.is_empty() {
        Some("no NVIDIA driver (NVML / nvidia-smi not found)".to_string())
    } else if rc != 0 || !junk.is_empty() || gpus.is_empty() {
        Some(match junk.first() {
            Some(j) => j.to_string(),
            None if rc != 0 => format!("nvidia-smi exited with code {rc}"),
            None => "no GPUs reported".into(),
        })
    } else {
        None
    };

    s.stat = stats.last().copied().flatten();
    // whole interval since the last frame; first frame: the span inside the agent
    s.cpu_pct = busy(prev_stat, s.stat).or_else(|| busy(stats.first().copied().flatten(), s.stat));
    gpus.sort_by_key(|g| g.idx);

    if want {
        let by_uuid: HashMap<&str, u32> = gpus.iter().map(|g| (g.uuid.as_str(), g.idx)).collect();
        let mut agg: HashMap<u32, (Proc, BTreeSet<u32>)> = HashMap::new();
        let mut per_gpu: HashMap<u32, Vec<(String, f64)>> = HashMap::new();
        let (mut hidden_gpus, mut hidden_mem, mut hidden_n) = (BTreeSet::new(), 0.0, 0);
        for &(uuid, pid, mem) in &apps {
            let idx = by_uuid.get(uuid).copied();
            let m = mem.unwrap_or(0.0);
            let info = pinfo.get(&pid);
            let user = info.map_or(UNKNOWN, |i| i.0);
            if let Some(ix) = idx {
                let d = per_gpu.entry(ix).or_default();
                match d.iter_mut().find(|e| e.0 == user) {
                    Some(e) => e.1 += m,
                    None => d.push((user.into(), m)),
                }
            }
            // not visible on the host: other PID namespace, or exited without
            // releasing its memory -> one summary row
            let Some(&(u, etime, cmd)) = info else {
                hidden_n += 1;
                hidden_mem += m;
                hidden_gpus.extend(idx);
                continue;
            };
            let e = agg.entry(pid).or_insert_with(|| {
                let (cpu, rss_kb) = qinfo.get(&pid).copied().unwrap_or((None, None));
                let p = Proc { pid, user: u.into(), etime, cmd: cmd.into(), gpus: Vec::new(), mem: 0.0, cpu, rss_kb };
                (p, BTreeSet::new())
            });
            e.0.mem += m;
            e.1.extend(idx);
        }
        let mut procs: Vec<Proc> = agg
            .into_values()
            .map(|(mut p, g)| {
                p.gpus = g.into_iter().collect();
                p
            })
            .collect();
        procs.sort_by(|a, b| b.mem.total_cmp(&a.mem).then(a.pid.cmp(&b.pid)));
        if hidden_n > 0 {
            procs.push(Proc {
                pid: 0,
                user: UNKNOWN.into(),
                etime: None,
                cmd: format!(
                    "{hidden_n} process{} not visible here (other container, or exited without freeing VRAM)",
                    if hidden_n > 1 { "es" } else { "" }
                ),
                gpus: hidden_gpus.into_iter().collect(),
                mem: hidden_mem,
                cpu: None,
                rss_kb: None,
            });
        }
        for g in gpus.iter_mut() {
            if let Some(d) = per_gpu.get_mut(&g.idx) {
                // biggest VRAM user first; unknown owners last
                d.sort_by(|a, b| (a.0 == UNKNOWN).cmp(&(b.0 == UNKNOWN)).then(b.1.total_cmp(&a.1)));
                g.users = d.iter().map(|e| e.0.clone()).collect();
            }
        }
        s.procs = Some(procs);
    }
    s
}

// ── per-host state ───────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Connecting,
    Ok,
    Error,
}

/// Utilization history. One point per BUCKET (the max of the samples in it),
/// so a sparkline spans the same time whether sampling every 100 ms or 1 s.
#[derive(Clone, Default)]
pub struct History {
    pub gpu: BTreeMap<u32, VecDeque<f32>>,
    pub cpu: VecDeque<f32>,
    bucket: Option<Instant>,
}

impl History {
    fn push(&mut self, s: &Sample, now: Instant) {
        let fresh = self.bucket.is_none_or(|t| now.duration_since(t) >= BUCKET);
        if fresh {
            self.bucket = Some(now);
        }
        let put = |q: &mut VecDeque<f32>, v: f32| match q.back_mut() {
            Some(last) if !fresh => *last = last.max(v),
            _ => {
                if q.len() == HISTORY {
                    q.pop_front();
                }
                q.push_back(v);
            }
        };
        for g in &s.gpus {
            put(self.gpu.entry(g.idx).or_default(), g.util.unwrap_or(0.0) as f32);
        }
        if let Some(c) = s.cpu_pct {
            put(&mut self.cpu, c as f32);
        }
    }
}

/// Written by the host's worker thread; the UI clones it every frame, which is
/// cheap: the sample and the history are behind Arcs.
#[derive(Clone)]
pub struct Host {
    pub name: String,
    pub status: Status,
    pub since: Instant,
    pub error: Option<String>,
    pub retry_at: Option<Instant>,
    pub sample: Option<Arc<Sample>>,
    pub updated: Instant,
    pub latency: Option<f64>, // request -> frame round trip of the last sample
    pub needs_login: bool,    // failed in a way only an interactive login fixes
    pub history: Arc<History>,
}

impl Host {
    pub fn new(name: &str) -> Self {
        let now = Instant::now();
        Host {
            name: name.into(),
            status: Status::Connecting,
            since: now,
            error: None,
            retry_at: None,
            sample: None,
            updated: now,
            latency: None,
            needs_login: false,
            history: Arc::default(),
        }
    }

    fn connecting(&mut self) {
        (self.status, self.since, self.retry_at) = (Status::Connecting, Instant::now(), None);
    }

    fn failed(&mut self, msg: String, retry_at: Option<Instant>) {
        self.needs_login = is_auth_error(&msg);
        (self.status, self.error, self.retry_at) = (Status::Error, Some(msg), retry_at);
        self.since = Instant::now();
    }

    fn push(&mut self, s: Sample, latency: Option<f64>) {
        let now = Instant::now();
        Arc::make_mut(&mut self.history).push(&s, now); // copies only if the UI still holds the old one
        (self.sample, self.status, self.error, self.retry_at) = (Some(Arc::new(s)), Status::Ok, None, None);
        self.needs_login = false;
        (self.updated, self.latency) = (now, latency);
    }
}

struct Io {
    stdin: Option<ChildStdin>, // the request pipe
    sent: Option<Instant>,     // a request is in flight since
    wrote: Instant,            // last write of any kind (keepalive)
}

// ── worker ───────────────────────────────────────────────────────────────────
/// Owns the SSH session for one host; reconnects with backoff in live mode.
/// `request()` is called by the UI scheduler and is a no-op while a sample is
/// still in flight.
pub struct Worker {
    pub host: Arc<Mutex<Host>>,
    pub timeout: u64,
    once: bool,
    tx: Sender<Msg>,
    stop: Arc<AtomicBool>,
    want: Arc<AtomicU8>, // WANT_*
    io: Mutex<Io>,
    pid: AtomicI32,
    kill_reason: Mutex<Option<String>>,
    pub done: AtomicBool,
    retry_now: AtomicBool,
}

impl Worker {
    pub fn new(
        name: &str,
        timeout: u64,
        once: bool,
        tx: Sender<Msg>,
        stop: Arc<AtomicBool>,
        want: Arc<AtomicU8>,
    ) -> Self {
        Worker {
            host: Arc::new(Mutex::new(Host::new(name))),
            timeout,
            once,
            tx,
            stop,
            want,
            io: Mutex::new(Io { stdin: None, sent: None, wrote: Instant::now() }),
            pid: AtomicI32::new(0),
            kill_reason: Mutex::new(None),
            done: AtomicBool::new(false),
            retry_now: AtomicBool::new(false),
        }
    }

    fn notify(&self) {
        DATA_GEN.fetch_add(1, SeqCst);
        if !UI_BUSY.load(SeqCst) {
            let _ = self.tx.send(Msg::Data);
        }
    }

    pub fn request(&self) -> bool {
        let mut io = self.io.lock().unwrap();
        if io.sent.is_some() {
            return false;
        }
        let Some(stdin) = io.stdin.as_mut() else { return false };
        let req: &[u8] = match self.want.load(SeqCst) {
            WANT_HOST => b"h\n",
            WANT_PROCS => b"p\n",
            _ => b"s\n",
        };
        if stdin.write_all(req).and_then(|_| stdin.flush()).is_err() {
            return false;
        }
        if self.once {
            io.stdin = None; // one frame, then EOF ends the remote loop
        }
        (io.sent, io.wrote) = (Some(Instant::now()), Instant::now());
        true
    }

    /// A no-op line for the agent if we haven't written for KEEPALIVE.
    pub fn keepalive(&self) {
        let mut io = self.io.lock().unwrap();
        if io.wrote.elapsed() >= KEEPALIVE {
            if let Some(stdin) = io.stdin.as_mut() {
                let _ = stdin.write_all(b"k\n").and_then(|_| stdin.flush());
            }
            io.wrote = Instant::now();
        }
    }

    pub fn run(&self) {
        self.session_loop();
        self.done.store(true, SeqCst);
        self.notify();
    }

    fn session_loop(&self) {
        let mut backoff = 2.0;
        while !self.stop.load(SeqCst) {
            self.host.lock().unwrap().connecting();
            self.notify();
            *self.kill_reason.lock().unwrap() = None;
            // macOS has no pipe2(): pipes are made close-on-exec in a second step,
            // so a concurrent spawn could inherit another host's pipe and hold it
            // open (that host would never see EOF). Serialize pipe creation + spawn.
            let guard = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
            let child = std::io::pipe().and_then(|(reader, writer)| {
                // rebuilt every time: a login (mux socket) may have appeared meanwhile
                let args = ssh_command(&self.host.lock().unwrap().name.clone(), self.timeout);
                let mut cmd = Command::new(&args[0]);
                cmd.args(&args[1..]).stdin(Stdio::piped()).stdout(writer.try_clone()?).stderr(writer);
                // ssh gets its own session: no controlling tty (a prompt can't draw
                // over the UI), Ctrl-C doesn't reach it, and killpg also takes down
                // a ProxyCommand child. setsid() is async-signal-safe.
                unsafe {
                    cmd.pre_exec(|| {
                        libc::setsid();
                        Ok(())
                    });
                }
                Ok((cmd.spawn()?, reader)) // the Command and its pipe ends are dropped here
            });
            drop(guard);
            let (mut child, reader) = match child {
                Ok(c) => c,
                Err(e) => {
                    self.host.lock().unwrap().failed(format!("cannot run ssh: {e}"), None);
                    return;
                }
            };
            self.pid.store(child.id() as i32, SeqCst);
            {
                let mut io = self.io.lock().unwrap();
                (io.stdin, io.sent) = (child.stdin.take(), None);
            }
            if self.stop.load(SeqCst) {
                self.kill(None);
            }
            self.request(); // ssh buffers it until the session is up

            let mut diag: VecDeque<String> = VecDeque::new();
            let mut frame: Option<Vec<String>> = None;
            let mut got = false;
            let mut rd = BufReader::new(reader);
            let mut raw = Vec::new();
            loop {
                raw.clear();
                if !matches!(rd.read_until(b'\n', &mut raw), Ok(n) if n > 0) {
                    break;
                }
                let line = String::from_utf8_lossy(&raw);
                let line = line.trim_end_matches(['\r', '\n']);
                if line == "@@B" {
                    frame = Some(Vec::new());
                } else if line == "@@E" && frame.is_some() {
                    let t0 = self.io.lock().unwrap().sent.take();
                    let prev = self.host.lock().unwrap().sample.as_ref().and_then(|s| s.stat);
                    let s = parse_frame(&frame.take().unwrap(), prev);
                    self.host.lock().unwrap().push(s, t0.map(|t| t.elapsed().as_secs_f64()));
                    (got, backoff) = (true, 2.0);
                    diag.clear(); // anything before this frame was shell-rc noise
                    self.notify();
                } else if let Some(f) = frame.as_mut() {
                    f.push(line.to_string());
                } else if !line.trim().is_empty() {
                    if diag.len() == 3 {
                        diag.pop_front();
                    }
                    diag.push_back(line.trim().to_string());
                }
            }
            let rc = child.wait().map(|s| s.code().unwrap_or_else(|| -s.signal().unwrap_or(0))).unwrap_or(-1);
            self.pid.store(0, SeqCst);
            {
                let mut io = self.io.lock().unwrap();
                (io.stdin, io.sent) = (None, None);
            }
            if self.stop.load(SeqCst) {
                return;
            }

            let msg = match self.kill_reason.lock().unwrap().take() {
                Some(r) => r,
                None if got && rc == 0 => "connection closed".into(),
                None => diag.back().cloned().unwrap_or_else(|| format!("ssh exited with code {rc}")),
            };
            if self.once {
                if !got {
                    self.host.lock().unwrap().failed(msg, None);
                }
                return;
            }
            let retry = Duration::from_secs_f64(backoff);
            self.host.lock().unwrap().failed(msg, Some(Instant::now() + retry));
            self.notify();
            let until = Instant::now() + retry;
            while Instant::now() < until && !self.retry_now.swap(false, SeqCst) {
                if self.stop.load(SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            backoff = (backoff * 2.0).min(30.0);
        }
    }

    /// Skip the rest of the backoff wait (e.g. right after a login).
    pub fn retry_now(&self) {
        self.retry_now.store(true, SeqCst);
    }

    pub fn kill(&self, reason: Option<&str>) {
        let pid = self.pid.load(SeqCst);
        if pid > 0 {
            if let Some(r) = reason {
                *self.kill_reason.lock().unwrap() = Some(r.into());
            }
            let sig = if reason.is_some() { libc::SIGKILL } else { libc::SIGTERM };
            unsafe { libc::killpg(pid, sig) };
        }
    }
}

/// Hard deadline for connect + first sample (ConnectTimeout only covers TCP).
pub fn supervise(workers: &[Arc<Worker>], now: Instant, hard_timeout: f64) {
    for w in workers {
        let (status, since) = {
            let h = w.host.lock().unwrap();
            (h.status, h.since)
        };
        if status == Status::Connecting && now.duration_since(since).as_secs_f64() > hard_timeout {
            w.kill(Some(&format!("timed out after {hard_timeout:.0}s")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_parses() {
        let raw = "\
@@S cpu  100 0 100 800 0 0 0 0 0 0
0, GPU-a, NVIDIA H100 80GB HBM3, 100, 40960, 81559, 45, 475.20, 700.00
1, GPU-b, NVIDIA H100 80GB HBM3, 0, 1, 81559, 24, [N/A], 700.00
@@G 0
@@S cpu  200 0 200 1400 200 0 0 0 0 0
@@L 30.10 28.00 27.50 3/2000 12345
@@M 2113798144 1826000000
@@W 900000000 16777212 16518996
@@N 192
@@U alice
@@C 0z0h
@@T 43
@@O 3542400.12
@@I 2 INTEL(R) XEON(R) PLATINUM 8558
@@X
@@A GPU-a, 4242, 30000
@@A GPU-a, 77, 10000
@@A GPU-b, 99, 500
@@P   4242 bob       3725 /opt/conda/bin/python train.py --lr 3e-4
@@Q 4242 99.5 12345678
@@P     77 alice       12 python eval.py
@@Q 77 - 2048
@@H bob 1840 38386560
@@H alice 33 1024";
        let lines: Vec<String> = raw.lines().map(String::from).collect();
        let s = parse_frame(&lines, None);
        assert_eq!(s.gpu_error, None);
        assert_eq!(s.gpus.len(), 2);
        assert_eq!(s.gpus[0].state(), 2);
        assert_eq!(s.gpus[1].state(), 0);
        assert_eq!(s.gpus[1].power, None);
        assert_eq!(s.cpu_pct, Some(20.0)); // 200 busy of 1000 jiffies between the @@S lines
        assert_eq!((s.cores, s.me.as_str()), (Some(192), "alice"));
        assert_eq!(s.core_pct, [0.0, 100.0, 0.0, 17.0 * 100.0 / 35.0]);
        assert_eq!((s.cpu_temp, s.sockets, s.cpu_model.as_str()), (Some(43.0), 2, "INTEL(R) XEON(R) PLATINUM 8558"));
        assert_eq!((s.mem_cache_kb, s.swap_total_kb, s.swap_free_kb), (900000000, 16777212, 16518996));
        assert_eq!(s.host_users[0], ("bob".to_string(), 1840.0, 38386560));
        assert_eq!(s.gpus[0].users, ["bob", "alice"]);
        assert_eq!(s.gpus[1].users, [UNKNOWN]);
        let p = s.procs.unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(
            (p[0].pid, p[0].etime, p[0].cmd.as_str(), p[0].cpu, p[0].rss_kb),
            (4242, Some(3725), "/opt/conda/bin/python train.py --lr 3e-4", Some(99.5), Some(12345678))
        );
        assert_eq!((p[1].cpu, p[1].rss_kb), (None, Some(2048)));
        assert_eq!((p[2].pid, p[2].gpus.as_slice()), (0, [1].as_slice())); // hidden-process summary
    }

    #[test]
    fn missing_driver_and_errors() {
        let err = "NVIDIA-SMI has failed because it couldn't communicate with the NVIDIA driver.";
        let lines: Vec<String> = [err, "@@G 9"].map(String::from).to_vec();
        assert_eq!(parse_frame(&lines, None).gpu_error.as_deref(), Some(err));
        let lines: Vec<String> = ["@@G 127".to_string()].to_vec();
        assert!(parse_frame(&lines, None).gpu_error.unwrap().starts_with("no NVIDIA driver"));
    }

    #[test]
    fn auth_errors_and_logins() {
        assert!(is_auth_error("alice@10.0.0.1: Permission denied (publickey,password)."));
        assert!(is_auth_error("Host key verification failed."));
        assert!(!is_auth_error("ssh: connect to host 10.0.0.1 port 22: Operation timed out"));
        assert!(mux_socket("gpu/a b").ends_with(".ssh/rgtop-gpu+2fa+20b"));
        assert!(mux_socket("BUPT_A40_102").ends_with(".ssh/rgtop-BUPT_A40_102")); // usual names unchanged
        assert_ne!(mux_socket("a/b"), mux_socket("a_b"));
        let cmd = login_command("gpu-1", 8);
        assert!(cmd.contains(&"ControlMaster=yes".to_string()) && !cmd.iter().any(|a| a.contains("BatchMode")));
        assert!(cmd.contains(&"ServerAliveInterval=5".to_string()));
        assert_eq!(cmd.last().map(String::as_str), Some("gpu-1"));
    }

    #[test]
    fn install_key_script() {
        // a scratch $HOME, never the real one
        let home = std::env::temp_dir().join(format!("rgtop-key-test-{}", std::process::id()));
        assert!(home.starts_with(std::env::temp_dir()) && home != std::env::temp_dir());
        let f = home.join(".ssh").join("authorized_keys");
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, "ssh-rsa AAAAB3other old@box").unwrap(); // no newline at the end
        let run = |key: &str| {
            Command::new("sh").args(["-c", INSTALL_KEY, "rgtop", key]).env("HOME", &home).status().unwrap().success()
        };
        let key = "ssh-ed25519 AAAAC3NzaKEY me@laptop";
        assert!(run(key));
        assert!(!run("ssh-ed25519 AAAAC3NzaKEY other-comment")); // the same key: an error, no second copy
        assert_eq!(std::fs::read_to_string(&f).unwrap(), format!("ssh-rsa AAAAB3other old@box\n{key}\n"));
        assert_eq!(pub_key_line("ssh-ed25519 AAAA+/= me@laptop").as_deref(), Some("ssh-ed25519 AAAA+/= me@laptop"));
        assert_eq!(pub_key_line("ssh-ed25519 AAAA x'; touch p").as_deref(), Some("ssh-ed25519 AAAA")); // comment dropped
        assert_eq!(pub_key_line("-----BEGIN OPENSSH PRIVATE KEY-----"), None);
        assert_eq!(pub_key_line("restrict ssh-ed25519 AAAA"), None);
        std::fs::remove_dir_all(&home).unwrap();
        let cmd = install_key_command("gpu-1", 8, key);
        assert!(cmd.contains(&format!("ControlPath={}", mux_socket("gpu-1").display())));
        assert!(cmd.contains(&"BatchMode=yes".to_string()));
    }

    #[test]
    fn history_buckets_fast_samples() {
        let mut h = History::default();
        let t = Instant::now();
        let s = |u: f64| Sample { gpus: vec![Gpu { util: Some(u), ..gpu() }], cpu_pct: Some(u), ..Sample::default() };
        h.push(&s(10.0), t);
        h.push(&s(70.0), t + Duration::from_millis(100)); // same bucket: max
        h.push(&s(20.0), t + Duration::from_millis(200));
        h.push(&s(5.0), t + Duration::from_millis(1000)); // next bucket
        assert_eq!(h.gpu[&0], [70.0, 5.0]);
        assert_eq!(h.cpu, [70.0, 5.0]);
    }

    fn gpu() -> Gpu {
        Gpu {
            idx: 0,
            uuid: String::new(),
            name: String::new(),
            util: None,
            mem_used: 0.0,
            mem_total: 0.0,
            temp: None,
            power: None,
            power_limit: None,
            users: Vec::new(),
        }
    }
}
