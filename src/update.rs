//! Update notice and `rgtop --update`. The latest release tag comes from the
//! github.com/<repo>/releases/latest redirect (what install.sh follows too; no
//! API, no rate limit), at most once a day, in the background.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::settings::config_dir;

const REPO: &str = "xianull/rgtop";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const DAY: u64 = 86_400;

/// A release newer than this binary ("v4.4.0"), once known.
pub static NEWER: OnceLock<String> = OnceLock::new();

fn version(tag: &str) -> Option<(u64, u64, u64)> {
    let mut it = tag.trim().trim_start_matches('v').splitn(3, '.').map(|p| p.parse().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

fn note(tag: &str) {
    if version(tag) > version(VERSION) {
        let _ = NEWER.set(tag.trim().into());
    }
}

/// The latest release tag, or None (offline, no curl, ...).
fn latest() -> Option<String> {
    let out = Command::new("curl")
        .args(["-fsSI", "--max-time", "5", "-o", "/dev/null", "-w", "%{redirect_url}"])
        .arg(format!("https://github.com/{REPO}/releases/latest"))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let url = String::from_utf8(out.stdout).ok()?;
    let tag = url.trim().rsplit_once("/tag/")?.1;
    version(tag).map(|_| tag.to_string())
}

/// Today's answer from the cache file, else ask GitHub in a thread. Never
/// blocks, and stays quiet when it can't find out.
pub fn check() {
    let file = config_dir().join("latest-release");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let cached = std::fs::read_to_string(&file).unwrap_or_default();
    if let Some((t, tag)) = cached.split_once(' ')
        && t.parse::<u64>().is_ok_and(|t| now.saturating_sub(t) < DAY)
    {
        return note(tag);
    }
    std::thread::spawn(move || {
        if let Some(tag) = latest() {
            let _ = std::fs::create_dir_all(config_dir());
            let _ = std::fs::write(&file, format!("{now} {tag}\n"));
            note(&tag);
        }
    });
}

/// `rgtop --update`: install.sh, latest release, over this binary.
pub fn update() -> i32 {
    match latest() {
        Some(tag) if version(&tag) <= version(VERSION) => {
            println!("rgtop {VERSION} is up to date (latest release: {tag})");
            return 0;
        }
        _ => {} // newer, or unknown: the installer says what's wrong
    }
    let exe = std::env::current_exe().and_then(|p| p.canonicalize());
    let Some(dir) = exe.as_deref().ok().and_then(Path::parent) else {
        eprintln!("rgtop: can't tell where this binary is; run install.sh yourself");
        return 1;
    };
    println!("rgtop: updating {} from github.com/{REPO}", dir.join("rgtop").display());
    // fetched first: `curl | sh` would run an empty script, and succeed, if the download failed
    let script = format!(
        "s=$(curl -fsSL https://raw.githubusercontent.com/{REPO}/main/install.sh) || exit 1; \
         RGTOP_INSTALL_DIR=\"$1\" exec sh -c \"$s\""
    );
    match Command::new("sh").args(["-c", &script, "rgtop"]).arg(dir).status() {
        Ok(s) if s.success() => 0,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert_eq!(version("v4.10.0"), Some((4, 10, 0)));
        assert!(version("v4.10.0") > version("4.9.9"));
        assert_eq!(version("v4.3"), None);
        assert_eq!(version("nightly"), None);
        assert!(version("nightly") < version(VERSION)); // an odd tag never counts as newer
    }
}
