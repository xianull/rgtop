//! Persistent UI settings (~/.config/gpustatus/settings.json), like btop.conf.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub const INTERVALS_MS: [u64; 9] = [500, 1000, 1500, 2000, 3000, 5000, 10000, 30000, 60000];
pub const SORTS: [&str; 4] = ["host", "idle", "vram", "util"];
pub const VIEWS: [&str; 2] = ["cards", "list"];
pub const THEMES: [&str; 3] = ["auto", "dark", "light"];

#[derive(Clone, Debug)]
pub struct Settings {
    pub interval_ms: u64,
    pub sort: &'static str, // host (config order) | idle | vram (free) | util
    pub reverse: bool,
    pub free_only: bool, // hide busy GPUs
    pub view: &'static str,
    pub show_cpu: bool,
    pub show_hist: bool,
    pub show_user: bool,
    pub show_proc: bool,
    pub theme: &'static str,
    pub mouse: bool,
    pub collapsed: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            interval_ms: 2000,
            sort: "host",
            reverse: false,
            free_only: false,
            view: "cards",
            show_cpu: true,
            show_hist: true,
            show_user: true,
            show_proc: false,
            theme: "auto",
            mouse: true,
            collapsed: Vec::new(),
        }
    }
}

pub fn config_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"));
    base.join("gpustatus")
}

/// Map a string onto one of the allowed choices (so we can keep `&'static str`).
pub fn choice(v: &str, of: &[&'static str]) -> Option<&'static str> {
    of.iter().copied().find(|c| *c == v)
}

/// Fields with a missing or ill-typed value keep their default; one bad field
/// never throws the whole file away.
pub fn load_settings(path: &Path) -> Settings {
    let mut st = Settings::default();
    let Ok(text) = std::fs::read_to_string(path) else { return st };
    let Ok(Value::Object(d)) = serde_json::from_str::<Value>(&text) else { return st };
    let flag = |k: &str, cur: &mut bool| {
        if let Some(Value::Bool(b)) = d.get(k) {
            *cur = *b;
        }
    };
    flag("reverse", &mut st.reverse);
    flag("free_only", &mut st.free_only);
    flag("show_cpu", &mut st.show_cpu);
    flag("show_hist", &mut st.show_hist);
    flag("show_user", &mut st.show_user);
    flag("show_proc", &mut st.show_proc);
    flag("mouse", &mut st.mouse);
    if let Some(v) = d.get("interval_ms").and_then(Value::as_u64) {
        st.interval_ms = v.clamp(250, 600_000);
    }
    let pick = |k: &str, of: &[&'static str], cur: &mut &'static str| {
        if let Some(v) = d.get(k).and_then(Value::as_str).and_then(|v| choice(v, of)) {
            *cur = v;
        }
    };
    pick("sort", &SORTS, &mut st.sort);
    pick("view", &VIEWS, &mut st.view);
    pick("theme", &THEMES, &mut st.theme);
    if let Some(Value::Array(a)) = d.get("collapsed") {
        st.collapsed = a.iter().filter_map(|x| x.as_str().map(String::from)).collect();
    }
    st
}

pub fn save_settings(path: &Path, st: &Settings) {
    let v = json!({
        "interval_ms": st.interval_ms, "sort": st.sort, "reverse": st.reverse,
        "free_only": st.free_only, "view": st.view, "show_cpu": st.show_cpu,
        "show_hist": st.show_hist, "show_user": st.show_user, "show_proc": st.show_proc,
        "theme": st.theme, "mouse": st.mouse, "collapsed": st.collapsed,
    });
    let Ok(text) = serde_json::to_string_pretty(&v) else { return };
    let tmp = path.with_extension("tmp");
    let _ = path.parent().map(std::fs::create_dir_all);
    if std::fs::write(&tmp, text + "\n").is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Next preset interval up (dir > 0) or down from `ms`.
pub fn step_interval(ms: u64, dir: i32) -> u64 {
    if dir > 0 {
        INTERVALS_MS.into_iter().find(|&v| v > ms).unwrap_or(INTERVALS_MS[8])
    } else {
        INTERVALS_MS.into_iter().rev().find(|&v| v < ms).unwrap_or(INTERVALS_MS[0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_fields_keep_defaults() {
        let dir = std::env::temp_dir().join(format!("gpustatus-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.json");
        std::fs::write(
            &p,
            r#"{"interval_ms": 1, "sort": "bogus", "reverse": "yes",
            "view": "list", "collapsed": ["a", 3]}"#,
        )
        .unwrap();
        let st = load_settings(&p);
        assert_eq!((st.interval_ms, st.sort, st.reverse, st.view), (250, "host", false, "list"));
        assert_eq!(st.collapsed, ["a"]);
        save_settings(&p, &st);
        assert_eq!(load_settings(&p).view, "list");
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(step_interval(2000, 1), 3000);
        assert_eq!(step_interval(2000, -1), 1500);
        assert_eq!(step_interval(60000, 1), 60000);
    }
}
