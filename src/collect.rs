//! Remote sampling over SSH.
//!
//! Each host gets ONE long-lived SSH session running a small POSIX-sh loop. Every
//! request line written to its stdin produces exactly one sample frame on stdout,
//! so the local side decides when to sample: the refresh interval can change
//! instantly, pausing costs nothing, and no sample ever pays for a new handshake.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering::SeqCst};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::app::Msg;

pub const HISTORY: usize = 240; // util samples kept per GPU for the sparkline
const IDLE_PCT: f64 = 5.0; // util AND mem below this -> idle  (green)
const LIGHT_PCT: f64 = 40.0; // util AND mem below this -> light (yellow), otherwise busy (red)
pub const UNKNOWN: &str = "?"; // owner of a GPU process whose PID isn't visible on the host

const GPU_FIELDS: &str = "index,uuid,name,utilization.gpu,memory.used,memory.total,\
                          temperature.gpu,power.draw,power.limit";

// dash-compatible. Request "s" = system + GPUs, "p" = also per-GPU processes.
// Every non-GPU line is tagged @@x so shell-rc noise can't be mistaken for data.
const REMOTE: &str = r#"
export LC_ALL=C
n=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null)
me=$(id -un 2>/dev/null)
command -v nvidia-smi >/dev/null 2>&1 || nosmi=1
while read -r req; do
  echo @@B
  echo "@@S $(head -n1 /proc/stat 2>/dev/null)"
  if [ -z "$nosmi" ]; then
    nvidia-smi --query-gpu=@FIELDS@ --format=csv,noheader,nounits 2>&1
    echo "@@G $?"
  else
    echo "@@G 127"
  fi
  echo "@@S $(head -n1 /proc/stat 2>/dev/null)"
  echo "@@L $(cat /proc/loadavg 2>/dev/null)"
  echo "@@M $(awk '/^MemTotal:/{t=$2} /^MemAvailable:/{a=$2} END{print t+0, a+0}' /proc/meminfo 2>/dev/null)"
  echo "@@N $n"
  echo "@@U $me"
  if [ "$req" = p ] && [ -z "$nosmi" ]; then
    echo @@X
    apps=$(nvidia-smi --query-compute-apps=gpu_uuid,pid,used_memory --format=csv,noheader,nounits 2>/dev/null)
    if [ -n "$apps" ]; then
      printf '%s\n' "$apps" | sed 's/^/@@A /'
      pids=$(printf '%s\n' "$apps" | awk -F', *' '$2 ~ /^[0-9]+$/ {print $2}' | sort -u | paste -sd, -)
      [ -n "$pids" ] && ps -o pid= -o user:32= -o etimes= -o args= -p "$pids" 2>/dev/null | cut -c1-300 | sed 's/^/@@P /'
    fi
  fi
  echo @@E
done
"#;

pub fn ssh_command(host: &str, timeout: u64) -> Vec<String> {
    let script = REMOTE.replace("@FIELDS@", GPU_FIELDS);
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
    v.extend(["--".into(), host.into(), format!("sh -c '{}'", script.replace('\'', r#"'"'"'"#))]);
    v
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
    pub mem: f64, // MiB, summed over its GPUs
}

#[derive(Clone, Debug)]
pub struct Sample {
    pub gpus: Vec<Gpu>,
    pub gpu_error: Option<String>,
    pub cpu_pct: Option<f64>,
    pub load: Option<[f64; 3]>,
    pub cores: Option<u32>,
    pub mem_total_kb: u64,
    pub mem_avail_kb: u64,
    pub stat: Option<(u64, u64)>, // (total, idle) jiffies at the end of the frame
    pub procs: Option<Vec<Proc>>, // None = processes weren't requested
    pub me: String,               // remote login user
}

fn num(s: &str) -> Option<f64> {
    s.parse().ok() // "[N/A]", "[Not Supported]", ... -> None
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

pub fn parse_frame(lines: &[String], prev_stat: Option<(u64, u64)>) -> Sample {
    let mut gpus = Vec::new();
    let mut junk: Vec<&str> = Vec::new();
    let mut stats: Vec<Option<(u64, u64)>> = Vec::new();
    let mut apps: Vec<(&str, u32, Option<f64>)> = Vec::new();
    let mut pinfo: HashMap<u32, (&str, Option<u64>, &str)> = HashMap::new();
    let (mut rc, mut load, mut cores, mut mem_t, mut mem_a, mut me, mut want) =
        (0i32, None, None, 0u64, 0u64, "", false);
    for line in lines {
        if let Some(tagged) = line.strip_prefix("@@") {
            let (tag, rest) = tagged.split_once(' ').unwrap_or((tagged, ""));
            match tag {
                "S" => stats.push(parse_stat(rest)),
                "G" => rc = rest.trim().parse().unwrap_or(rc),
                "L" => {
                    let t: Vec<f64> = rest.split_whitespace().take(3).map_while(|x| x.parse().ok()).collect();
                    load = (t.len() == 3).then(|| [t[0], t[1], t[2]]);
                }
                "M" => {
                    let v: Vec<u64> = rest.split_whitespace().take(2).map_while(|x| x.parse().ok()).collect();
                    if v.len() == 2 {
                        (mem_t, mem_a) = (v[0], v[1]);
                    }
                }
                "N" => cores = rest.trim().parse().ok(),
                "U" => me = rest.trim(),
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
                _ => {}
            }
            continue;
        }
        let row: Vec<&str> = line.split(',').map(str::trim).collect();
        if row.len() >= 9 && digits(row[0]) {
            let n = &row[row.len() - 6..];
            gpus.push(Gpu {
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

    let gpu_error = if rc == 127 && gpus.is_empty() {
        Some("nvidia-smi not found".to_string())
    } else if rc != 0 || !junk.is_empty() || gpus.is_empty() {
        Some(match junk.first() {
            Some(j) => j.to_string(),
            None if rc != 0 => format!("nvidia-smi exited with code {rc}"),
            None => "no GPUs reported".into(),
        })
    } else {
        None
    };

    let end = stats.last().copied().flatten();
    // whole interval since the last frame; first frame: the span of nvidia-smi
    let cpu_pct = busy(prev_stat, end).or_else(|| busy(stats.first().copied().flatten(), end));
    gpus.sort_by_key(|g| g.idx);

    let procs = if want {
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
            // nvidia-smi "[Not Found]": other PID namespace, or exited without
            // releasing its memory -> one summary row
            let Some(&(u, etime, cmd)) = info else {
                hidden_n += 1;
                hidden_mem += m;
                hidden_gpus.extend(idx);
                continue;
            };
            let e = agg.entry(pid).or_insert_with(|| {
                (Proc { pid, user: u.into(), etime, cmd: cmd.into(), gpus: Vec::new(), mem: 0.0 }, BTreeSet::new())
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
                    "{hidden_n} process{} not visible here (other container, or exited \
                              without freeing VRAM)",
                    if hidden_n > 1 { "es" } else { "" }
                ),
                gpus: hidden_gpus.into_iter().collect(),
                mem: hidden_mem,
            });
        }
        for g in gpus.iter_mut() {
            if let Some(d) = per_gpu.get_mut(&g.idx) {
                // biggest VRAM user first; unknown owners last
                d.sort_by(|a, b| (a.0 == UNKNOWN).cmp(&(b.0 == UNKNOWN)).then(b.1.total_cmp(&a.1)));
                g.users = d.iter().map(|e| e.0.clone()).collect();
            }
        }
        Some(procs)
    } else {
        None
    };
    Sample {
        gpus,
        gpu_error,
        cpu_pct,
        load,
        cores,
        mem_total_kb: mem_t,
        mem_avail_kb: mem_a,
        stat: end,
        procs,
        me: me.into(),
    }
}

// ── per-host state ───────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Connecting,
    Ok,
    Error,
}

/// Shared between a host's worker thread (writer) and the UI (clones a copy per frame).
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
    pub history: BTreeMap<u32, VecDeque<f32>>,
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
            history: BTreeMap::new(),
        }
    }

    fn connecting(&mut self) {
        (self.status, self.since, self.retry_at) = (Status::Connecting, Instant::now(), None);
    }

    fn failed(&mut self, msg: String, retry_at: Option<Instant>) {
        (self.status, self.error, self.retry_at) = (Status::Error, Some(msg), retry_at);
        self.since = Instant::now();
    }

    fn push(&mut self, s: Sample, latency: Option<f64>) {
        for g in &s.gpus {
            let h = self.history.entry(g.idx).or_default();
            if h.len() == HISTORY {
                h.pop_front();
            }
            h.push_back(g.util.unwrap_or(0.0) as f32);
        }
        (self.sample, self.status, self.error, self.retry_at) = (Some(Arc::new(s)), Status::Ok, None, None);
        (self.updated, self.latency) = (Instant::now(), latency);
    }
}

// ── worker ───────────────────────────────────────────────────────────────────
/// Owns the SSH session for one host; reconnects with backoff in live mode.
/// `request()` is called by the UI scheduler and is a no-op while a sample is
/// still in flight.
pub struct Worker {
    pub host: Arc<Mutex<Host>>,
    cmd: Vec<String>,
    once: bool,
    tx: Sender<Msg>,
    stop: Arc<AtomicBool>,
    want_procs: Arc<AtomicBool>,
    io: Mutex<(Option<ChildStdin>, Option<Instant>)>, // (request pipe, request sent at)
    pid: AtomicI32,
    kill_reason: Mutex<Option<String>>,
    pub done: AtomicBool,
}

impl Worker {
    pub fn new(
        name: &str,
        cmd: Vec<String>,
        once: bool,
        tx: Sender<Msg>,
        stop: Arc<AtomicBool>,
        want_procs: Arc<AtomicBool>,
    ) -> Self {
        Worker {
            host: Arc::new(Mutex::new(Host::new(name))),
            cmd,
            once,
            tx,
            stop,
            want_procs,
            io: Mutex::new((None, None)),
            pid: AtomicI32::new(0),
            kill_reason: Mutex::new(None),
            done: AtomicBool::new(false),
        }
    }

    fn notify(&self) {
        let _ = self.tx.send(Msg::Data);
    }

    pub fn request(&self) -> bool {
        let mut io = self.io.lock().unwrap();
        if io.1.is_some() {
            return false;
        }
        let Some(stdin) = io.0.as_mut() else { return false };
        let req = if self.want_procs.load(SeqCst) { b"p\n" } else { b"s\n" };
        if stdin.write_all(req).and_then(|_| stdin.flush()).is_err() {
            return false;
        }
        if self.once {
            io.0 = None; // one frame, then EOF ends the remote loop
        }
        io.1 = Some(Instant::now());
        true
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
            let child = std::io::pipe().and_then(|(reader, writer)| {
                // ssh gets its own process group: Ctrl-C in our terminal doesn't
                // reach it, and killpg also takes down a ProxyCommand child
                let child = Command::new(&self.cmd[0])
                    .args(&self.cmd[1..])
                    .stdin(Stdio::piped())
                    .stdout(writer.try_clone()?)
                    .stderr(writer)
                    .process_group(0)
                    .spawn()?;
                Ok((child, reader)) // the Command and its pipe ends are dropped here
            });
            let (mut child, reader) = match child {
                Ok(c) => c,
                Err(e) => {
                    self.host.lock().unwrap().failed(format!("cannot run ssh: {e}"), None);
                    return;
                }
            };
            self.pid.store(child.id() as i32, SeqCst);
            *self.io.lock().unwrap() = (child.stdin.take(), None);
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
                    let t0 = self.io.lock().unwrap().1.take();
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
            *self.io.lock().unwrap() = (None, None);
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
            while Instant::now() < until {
                if self.stop.load(SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            backoff = (backoff * 2.0).min(30.0);
        }
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
@@N 192
@@U alice
@@X
@@A GPU-a, 4242, 30000
@@A GPU-a, 77, 10000
@@A GPU-b, 99, 500
@@P   4242 bob       3725 /opt/conda/bin/python train.py --lr 3e-4
@@P     77 alice       12 python eval.py";
        let lines: Vec<String> = raw.lines().map(String::from).collect();
        let s = parse_frame(&lines, None);
        assert_eq!(s.gpu_error, None);
        assert_eq!(s.gpus.len(), 2);
        assert_eq!(s.gpus[0].state(), 2);
        assert_eq!(s.gpus[1].state(), 0);
        assert_eq!(s.gpus[1].power, None);
        assert_eq!(s.cpu_pct, Some(20.0)); // 200 busy of 1000 jiffies between the @@S lines
        assert_eq!((s.cores, s.me.as_str()), (Some(192), "alice"));
        assert_eq!(s.gpus[0].users, ["bob", "alice"]);
        assert_eq!(s.gpus[1].users, [UNKNOWN]);
        let p = s.procs.unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(
            (p[0].pid, p[0].etime, p[0].cmd.as_str()),
            (4242, Some(3725), "/opt/conda/bin/python train.py --lr 3e-4")
        );
        assert_eq!((p[2].pid, p[2].gpus.as_slice()), (0, [1].as_slice())); // hidden-process summary
    }

    #[test]
    fn missing_smi_and_noise() {
        let err = "NVIDIA-SMI has failed because it couldn't communicate with the NVIDIA driver.";
        let lines: Vec<String> = [err, "@@G 9"].map(String::from).to_vec();
        assert_eq!(parse_frame(&lines, None).gpu_error.as_deref(), Some(err));
        let lines: Vec<String> = ["@@G 127".to_string()].to_vec();
        assert_eq!(parse_frame(&lines, None).gpu_error.as_deref(), Some("nvidia-smi not found"));
    }
}
