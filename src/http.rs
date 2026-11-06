//! HTTP backend for rgtop.
//!
//! When a host entry in the config starts with `http://` (or `https://`), rgtop
//! fetches frames from a web API instead of running an SSH agent. The API
//! contract is documented in docs/RGTOP_HTTP_BACKEND.md of slurm-monitor.
//!
//! Requests are plain HTTP/1.1 over a blocking TCP socket: no async runtime, no
//! TLS dependency (https:// is rejected with a clear message for now).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use crate::collect::{Gpu, Proc, Sample, UNKNOWN};

/// Parts of an http(s) URL. `None` means the entry is an SSH host alias.
pub struct Url<'a> {
    pub scheme: &'a str,
    pub host: &'a str,
    pub port: u16,
    pub path: &'a str, // without the leading '/'
}

/// `http://host:port/path` -> Url. Anything without an http(s) scheme is an
/// SSH alias and returns None.
pub fn parse_url(s: &str) -> Option<Url<'_>> {
    let (scheme, rest) = if let Some(r) = s.strip_prefix("http://") {
        ("http", r)
    } else if let Some(r) = s.strip_prefix("https://") {
        ("https", r)
    } else {
        return None;
    };
    let (host_port, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (host_port, if scheme == "https" { 443 } else { 80 }),
    };
    if host.is_empty() {
        return None;
    }
    Some(Url { scheme, host, port, path })
}

/// The node name this URL refers to: the segment after `.../frame/`, else the
/// host itself. (`#name` would be tempting but `#` starts a comment in the
/// hosts file, so the node always belongs in the path.)
pub fn node_of(u: &Url<'_>, _raw: &str) -> String {
    if let Some(rest) = u.path.split_once("/frame").map(|(_, r)| r.trim_start_matches('/'))
        && !rest.is_empty()
        && !rest.contains('?')
    {
        return rest.to_string();
    }
    u.host.to_string()
}

/// Path to request: an explicit `.../frame/<node>` path is used as-is, else the
/// node is appended to the API base path.
fn request_path(u: &Url<'_>, node: &str, want_procs: bool) -> String {
    let p = if u.path.contains("/frame") { u.path.to_string() } else { format!("api/rgtop/frame/{node}") };
    let sep = if p.contains('?') { '&' } else { '?' };
    format!("/{p}{sep}procs={}", u8::from(want_procs))
}

/// Minimal HTTP/1.1 GET. Returns the body. Requests identity encoding and
/// `Connection: close`, so the body is the rest of the stream (no chunk parsing).
fn get(u: &Url<'_>, path: &str, timeout: u64) -> Result<String, String> {
    if u.scheme == "https" {
        return Err("https is not supported yet (no TLS backend); use http:// or ssh".into());
    }
    let addr = format!("{}:{}", u.host, u.port);
    let sock = addr
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("cannot resolve {addr}"))?;
    let mut s = TcpStream::connect_timeout(&sock, Duration::from_secs(timeout))
        .map_err(|e| format!("connect {addr}: {e}"))?;
    let to = Some(Duration::from_secs(timeout));
    s.set_read_timeout(to).map_err(|e| format!("set timeout: {e}"))?;
    s.set_write_timeout(to).map_err(|e| format!("set timeout: {e}"))?;

    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {}\r\nAccept: application/json\r\n\
         Accept-Encoding: identity\r\nConnection: close\r\nUser-Agent: rgtop\r\n\r\n",
        u.host
    );
    s.write_all(req.as_bytes()).map_err(|e| format!("send: {e}"))?;

    let mut buf = Vec::new();
    s.read_to_end(&mut buf).map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&buf);

    let (head, body) = text.split_once("\r\n\r\n").ok_or("malformed HTTP response")?;
    let status = head.lines().next().unwrap_or("");
    let code: u16 = status.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    if code == 404 {
        return Err("no such node on this server (404)".into());
    }
    if code == 401 || code == 403 {
        return Err(format!("access denied ({code})"));
    }
    if code != 200 {
        let detail = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get("detail").and_then(|d| d.as_str()).map(String::from))
            .unwrap_or_else(|| format!("HTTP {code}"));
        return Err(detail);
    }
    Ok(body.to_string())
}

/// Fetch one frame for `node` and convert it to a [`Sample`].
/// Returns the sample and the round-trip time in seconds.
pub fn fetch(url: &str, node: &str, want_procs: bool, timeout: u64) -> Result<(Sample, f64), String> {
    let u = parse_url(url).ok_or_else(|| format!("not an http url: {url}"))?;
    let path = request_path(&u, node, want_procs);
    let started = Instant::now();
    let body = get(&u, &path, timeout)?;
    let elapsed = started.elapsed().as_secs_f64();
    let sample = parse_body(&body, want_procs)?;
    Ok((sample, elapsed))
}

/// A batch entry names no node: `http://h:p`, `http://h:p/`, or a path that ends
/// at `/frame` (`.../api/rgtop/frame`). Such an entry lists every node it serves.
pub fn is_batch(s: &str) -> bool {
    let Some(u) = parse_url(s) else { return false };
    if u.path.trim_end_matches('/').is_empty() {
        return true;
    }
    match u.path.split_once("/frame") {
        Some((_, rest)) => rest.trim_matches('/').is_empty(),
        None => false,
    }
}

/// `(origin, frame path)` for a batch entry, e.g.
/// `("http://h:8000", "api/rgtop/frame")`. The frame path keeps any prefix
/// the server is mounted under.
fn batch_parts(s: &str) -> Option<(String, String)> {
    let u = parse_url(s)?;
    let frame = match u.path.split_once("/frame") {
        Some((pre, _)) => format!("{}/frame", pre.trim_matches('/')).trim_start_matches('/').to_string(),
        None => "api/rgtop/frame".to_string(),
    };
    Some((format!("{}://{}:{}", u.scheme, u.host, u.port), frame))
}

/// Node names in a batch frame (`data.nodes[].node`). A name that could be
/// unsafe in a URL path is skipped rather than requested.
fn parse_node_names(body: &str) -> Result<Vec<String>, String> {
    let root: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("bad JSON: {e}"))?;
    let nodes = root
        .get("data")
        .and_then(|d| d.get("nodes"))
        .and_then(|v| v.as_array())
        .ok_or("response has no data.nodes list")?;
    Ok(nodes
        .iter()
        .filter_map(|n| n.get("node").and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)))
        .map(String::from)
        .collect())
}

/// Expand every batch entry into one `.../frame/<node>` entry per node the
/// server lists. Other entries pass through unchanged, and duplicates are
/// dropped. Fails when a batch endpoint can't be listed, so the user sees why
/// instead of a silently short list.
pub fn expand(hosts: Vec<String>, timeout: u64) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for h in hosts {
        if !is_batch(&h) {
            out.push(h);
            continue;
        }
        let (origin, frame) = batch_parts(&h).ok_or_else(|| format!("not a valid URL: {h}"))?;
        let u = parse_url(&origin).ok_or_else(|| format!("not a valid URL: {h}"))?;
        let body = get(&u, &format!("/{frame}?procs=0"), timeout)
            .map_err(|e| format!("cannot list nodes from {h}: {e}"))?;
        let names = parse_node_names(&body).map_err(|e| format!("cannot list nodes from {h}: {e}"))?;
        if names.is_empty() {
            return Err(format!("{h} lists no nodes"));
        }
        out.extend(names.into_iter().map(|n| format!("{origin}/{frame}/{n}")));
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|h| seen.insert(h.clone()));
    Ok(out)
}

fn num(v: &serde_json::Value, key: &str) -> Option<f64> {
    v.get(key).and_then(|x| x.as_f64())
}

fn parse_body(body: &str, want_procs: bool) -> Result<Sample, String> {
    let root: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("bad JSON: {e}"))?;
    let data = root.get("data").ok_or("response has no 'data' field")?;
    // Both /api/rgtop/frame (a "nodes" array) and /api/rgtop/frame/{name}
    // (a single frame) are accepted.
    let node = match data.get("nodes") {
        Some(nodes) => nodes.get(0).ok_or("server returned no nodes")?,
        None => data,
    };
    if let Some(err) = node.get("error").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        return Err(err.to_string());
    }

    let mut gpus = Vec::new();
    // pid -> merged process, because the API lists processes per GPU while
    // rgtop's Sample keeps one row per process across all its cards.
    let mut merged: HashMap<u32, Proc> = HashMap::new();

    for g in node.get("gpus").and_then(|v| v.as_array()).map(|v| v.as_slice()).unwrap_or(&[]) {
        let idx = g.get("idx").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let mut users: Vec<(String, f64)> = Vec::new();

        if want_procs {
            for p in g.get("procs").and_then(|v| v.as_array()).map(|v| v.as_slice()).unwrap_or(&[]) {
                let pid = p.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                if pid == 0 {
                    continue;
                }
                let mem = num(p, "mem_mb").unwrap_or(0.0);
                let user = p.get("user").and_then(|v| v.as_str()).unwrap_or(UNKNOWN).to_string();

                match users.iter_mut().find(|e| e.0 == user) {
                    Some(e) => e.1 += mem,
                    None => users.push((user.clone(), mem)),
                }

                let e = merged.entry(pid).or_insert_with(|| Proc {
                    pid,
                    user: user.clone(),
                    etime: num(p, "etime_s").map(|v| v as u64),
                    cmd: p.get("cmd").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    gpus: Vec::new(),
                    mem: 0.0,
                    cpu: num(p, "cpu_pct"),
                    rss_kb: p.get("rss_kb").and_then(|v| v.as_u64()),
                });
                e.mem += mem;
                if !e.gpus.contains(&idx) {
                    e.gpus.push(idx);
                }
            }
        }

        // biggest VRAM user first; the API already reports per-card totals
        users.sort_by(|a, b| b.1.total_cmp(&a.1));
        gpus.push(Gpu {
            idx,
            uuid: g.get("uuid").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            name: g.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            util: num(g, "util_pct"),
            mem_used: num(g, "mem_used_mb").unwrap_or(0.0),
            mem_total: num(g, "mem_total_mb").unwrap_or(0.0),
            temp: num(g, "temp_c"),
            power: num(g, "power_w"),
            power_limit: num(g, "power_limit_w"),
            users: users.into_iter().map(|e| e.0).collect(),
        });
    }
    gpus.sort_by_key(|g| g.idx);

    let mut procs: Vec<Proc> = merged.into_values().collect();
    procs.sort_by(|a, b| b.mem.total_cmp(&a.mem).then(a.pid.cmp(&b.pid)));

    let host = node.get("host").filter(|v| !v.is_null());
    let host_get = |k: &str| host.and_then(|h| num(h, k));
    let host_u64 = |k: &str| host.and_then(|h| h.get(k)).and_then(|v| v.as_u64());

    let mib_to_kb = |mb: Option<u64>| mb.unwrap_or(0) * 1024;

    // cpu_cores: one base36 digit per logical core, 0..35 for 0..100%
    let core_pct: Vec<f32> = host
        .and_then(|h| h.get("cpu_cores"))
        .and_then(|v| v.as_str())
        .map(|s| s.chars().filter_map(|c| c.to_digit(36)).map(|d| d as f32 * 100.0 / 35.0).collect())
        .unwrap_or_default();

    let gpu_error = if gpus.is_empty() {
        Some("no GPUs reported".to_string())
    } else if gpus.iter().all(|g| g.util.is_none() && g.mem_total <= 0.0) {
        Some("no usable GPU metrics".to_string())
    } else {
        None
    };

    // Server-side sample time, so the UI can show how stale the data is. Prefer
    // recomputing it from the epoch: `age_seconds` was measured when the server
    // answered and keeps growing, which is the whole point of the distinction.
    let data_age = node
        .get("collected_at_epoch")
        .and_then(|v| v.as_i64())
        .and_then(|epoch| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|now| (now.as_secs() as i64 - epoch).max(0) as f64)
        })
        .or_else(|| num(node, "age_seconds"));

    Ok(Sample {
        gpus,
        gpu_error,
        data_age,
        cpu_pct: host_get("cpu_util_pct"),
        core_pct,
        cpu_temp: host_get("cpu_temp_c"),
        cpu_model: host
            .and_then(|h| h.get("cpu_model"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        sockets: host_u64("sockets").unwrap_or(0) as u32,
        load: match (host_get("load1"), host_get("load5"), host_get("load15")) {
            (Some(a), Some(b), Some(c)) => Some([a, b, c]),
            _ => None,
        },
        cores: host_u64("cpu_count").map(|v| v as u32),
        mem_total_kb: mib_to_kb(host_u64("mem_total_mb")),
        mem_avail_kb: mib_to_kb(host_u64("mem_avail_mb")),
        mem_cache_kb: mib_to_kb(host_u64("mem_cache_mb")),
        swap_total_kb: mib_to_kb(host_u64("swap_total_mb")),
        swap_free_kb: mib_to_kb(host_u64("swap_free_mb")),
        uptime: host_get("uptime_s"),
        stat: None, // the server already did the CPU delta for us
        procs: want_procs.then_some(procs),
        host_users: Vec::new(), // not part of the HTTP contract
        me: std::env::var("USER").unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_entries_expand() {
        assert!(is_batch("http://h:8000"));
        assert!(is_batch("http://h:8000/"));
        assert!(is_batch("http://h:8000/api/rgtop/frame"));
        assert!(is_batch("http://h:8000/api/rgtop/frame/"));
        assert!(!is_batch("http://h:8000/api/rgtop/frame/h100-3"));
        assert!(!is_batch("h100-3"));

        assert_eq!(batch_parts("http://h:8000").unwrap(), ("http://h:8000".into(), "api/rgtop/frame".into()));
        // a server mounted under a prefix keeps it
        assert_eq!(
            batch_parts("http://h:8000/mon/api/rgtop/frame").unwrap(),
            ("http://h:8000".into(), "mon/api/rgtop/frame".into())
        );

        let body = r#"{"code":0,"data":{"nodes":[{"node":"h100-3"},{"node":"h200-1"},
                       {"node":"bad/../x"},{"node":""}]}}"#;
        assert_eq!(parse_node_names(body).unwrap(), ["h100-3", "h200-1"]); // unsafe names skipped
        assert!(parse_node_names(r#"{"data":{}}"#).is_err());

        // entries that name a node (or are SSH aliases) pass through untouched
        let kept = vec!["h1".to_string(), "http://h:1/api/rgtop/frame/n1".to_string(), "h1".to_string()];
        assert_eq!(expand(kept, 1).unwrap(), ["h1", "http://h:1/api/rgtop/frame/n1"]);
    }

    #[test]
    fn urls() {
        let u = parse_url("http://192.168.3.102:8000").unwrap();
        assert_eq!((u.scheme, u.host, u.port, u.path), ("http", "192.168.3.102", 8000, ""));
        assert_eq!(node_of(&u, "http://192.168.3.102:8000"), "192.168.3.102");

        let u = parse_url("http://10.0.0.1/api/rgtop/frame/h100-3").unwrap();
        assert_eq!(node_of(&u, "http://10.0.0.1/api/rgtop/frame/h100-3"), "h100-3");
        assert_eq!(request_path(&u, "h100-3", true), "/api/rgtop/frame/h100-3?procs=1");

        // base URL: the node is appended to the API path
        let u = parse_url("http://10.0.0.1:9000/").unwrap();
        assert_eq!(request_path(&u, "gpu-7", false), "/api/rgtop/frame/gpu-7?procs=0");

        assert!(parse_url("h100-3").is_none());
        assert_eq!(parse_url("https://x/y").unwrap().port, 443);
    }

    #[test]
    fn frame_parses() {
        let body = r#"{"code":0,"data":{"node":"g1","error":null,"gpus":[
            {"idx":0,"uuid":"GPU-a","name":"NVIDIA H100","util_pct":100,
             "mem_used_mb":1024,"mem_total_mb":81920,"temp_c":45,
             "power_w":475.2,"power_limit_w":700.0,"job_id":7,
             "procs":[{"pid":42,"user":"bob","cmd":"python train.py","mem_mb":700,
                       "rss_kb":1048576,"etime_s":60,"cpu_pct":99.5},
                      {"pid":43,"user":"bob","cmd":"python eval.py","mem_mb":100,
                       "rss_kb":2048,"etime_s":10,"cpu_pct":1.0}]},
            {"idx":1,"uuid":"GPU-b","name":"NVIDIA H100","util_pct":0,
             "mem_used_mb":0,"mem_total_mb":81920,"temp_c":24,
             "power_w":null,"power_limit_w":700.0,"job_id":null,"procs":[]}],
            "host":{"cpu_count":192,"cpu_util_pct":20.0,"load1":1.5,"load5":2.5,"load15":3.5,
                    "mem_total_mb":2048000,"mem_used_mb":1024000,"mem_avail_mb":1024000,
                    "mem_cache_mb":512000,"swap_total_mb":16383,"swap_free_mb":15000,
                    "cpu_cores":"0z","cpu_model":"Xeon","sockets":2,"cpu_temp_c":43.0,
                    "uptime_s":3600}}}"#;
        let s = parse_body(body, true).unwrap();
        assert_eq!(s.gpus.len(), 2);
        assert_eq!(s.gpus[0].idx, 0);
        assert_eq!(s.gpus[0].state(), 2); // 100% util
        assert_eq!(s.gpus[1].state(), 0); // idle
        assert_eq!(s.gpus[0].users, ["bob"]); // merged per user
        assert_eq!(s.gpus[1].power, None);

        let p = s.procs.unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].pid, p[0].etime, p[0].cpu), (42, Some(60), Some(99.5)));
        assert_eq!(p[0].gpus, [0]);

        assert_eq!(s.cpu_pct, Some(20.0));
        assert_eq!(s.core_pct, [0.0, 100.0]);
        assert_eq!((s.sockets, s.cores), (2, Some(192)));
        assert_eq!(s.mem_total_kb, 2048000 * 1024);
        assert_eq!((s.cpu_temp, s.uptime), (Some(43.0), Some(3600.0)));

        // procs=0: no process rows, no user lists
        let s = parse_body(body, false).unwrap();
        assert!(s.procs.is_none());
        assert!(s.gpus[0].users.is_empty());
    }

    #[test]
    fn node_errors() {
        let body = r#"{"code":0,"data":{"node":"g9","error":"no GPU data collected for this node",
                       "gpus":[],"host":null}}"#;
        assert!(parse_body(body, true).unwrap_err().contains("no GPU data"));
    }
}
