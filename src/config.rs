//! Reads the illogical-impulse shell config (`~/.config/illogical-impulse/config.json`)
//! and watches it with inotify so wallpaper changes made by the shell apply instantly.

use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::PathBuf;

use serde_json::Value;

/// Same list the shell uses in Background.qml / switchwall.sh
pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "webm", "mkv", "avi", "mov"];

pub fn is_video(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    VIDEO_EXTENSIONS.iter().any(|e| lower.ends_with(&format!(".{e}")))
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum PauseMode {
    /// Never pause automatically
    Never,
    /// Pause when the active workspace has a fullscreen/maximized window
    Fullscreen,
    /// Also pause when the active workspace has any tiled window
    Tiled,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Config {
    pub wallpaper_path: String,
    pub zoom: f64,
    pub pause_mode: PauseMode,
    pub audio: bool,
    pub volume: f64,
    pub speed: f64,
    pub hwdec: String,
    pub crossfade_ms: u64,
    /// Extra mpv options (override the built-in ones); applied when playback starts
    pub mpv_options: Vec<(String, String)>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            wallpaper_path: String::new(),
            zoom: 1.07,
            pause_mode: PauseMode::Tiled,
            audio: false,
            volume: 50.0,
            speed: 1.0,
            hwdec: "auto-safe".into(),
            crossfade_ms: 500,
            mpv_options: Vec::new(),
        }
    }
}

pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("illogical-impulse")
}

pub const CONFIG_FILE: &str = "config.json";

impl Config {
    pub fn load() -> Self {
        let path = config_dir().join(CONFIG_FILE);
        let mut cfg = Config::default();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("cannot read {}: {e}", path.display());
                return cfg;
            }
        };
        let root: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            // The shell may be mid-write; the next inotify event will retry
            Err(e) => {
                log::warn!("cannot parse {}: {e}", path.display());
                return cfg;
            }
        };
        let bg = &root["background"];
        if let Some(p) = bg["wallpaperPath"].as_str() {
            cfg.wallpaper_path = p.to_string();
        }
        if let Some(z) = bg["parallax"]["workspaceZoom"].as_f64() {
            cfg.zoom = z;
        }
        let v = &bg["videoWallpaper"];
        if let Some(m) = v["pauseMode"].as_str() {
            cfg.pause_mode = match m {
                "never" => PauseMode::Never,
                "fullscreen" => PauseMode::Fullscreen,
                _ => PauseMode::Tiled,
            };
        }
        if let Some(b) = v["audio"].as_bool() {
            cfg.audio = b;
        }
        if let Some(x) = v["volume"].as_f64() {
            cfg.volume = x.clamp(0.0, 100.0);
        }
        if let Some(x) = v["speed"].as_f64() {
            cfg.speed = x.clamp(0.05, 8.0);
        }
        if let Some(s) = v["hwdec"].as_str() {
            if !s.is_empty() {
                cfg.hwdec = s.to_string();
            }
        }
        if let Some(obj) = v["mpvOptions"].as_object() {
            for (k, val) in obj {
                let val = match val {
                    Value::String(s) => s.clone(),
                    Value::Bool(b) => (if *b { "yes" } else { "no" }).to_string(),
                    other => other.to_string(),
                };
                cfg.mpv_options.push((k.clone(), val));
            }
        }
        if let Some(b) = v["crossfade"].as_bool() {
            if !b {
                cfg.crossfade_ms = 0;
            }
        }
        cfg
    }
}

/// inotify watch on the config directory (the shell and jq replace the file via rename).
pub struct Watcher {
    pub fd: OwnedFd,
}

impl Watcher {
    pub fn new() -> Option<Self> {
        let dir = CString::new(config_dir().into_os_string().into_encoded_bytes()).ok()?;
        unsafe {
            let fd = libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC);
            if fd < 0 {
                return None;
            }
            let fd = OwnedFd::from_raw_fd(fd);
            let mask = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_CREATE;
            if libc::inotify_add_watch(std::os::fd::AsRawFd::as_raw_fd(&fd), dir.as_ptr(), mask) < 0 {
                log::warn!("inotify_add_watch failed: {}", std::io::Error::last_os_error());
                return None;
            }
            Some(Watcher { fd })
        }
    }

    /// Drains events; returns true if config.json was touched.
    pub fn read(&self) -> bool {
        let mut buf = [0u8; 4096];
        let mut touched = false;
        loop {
            let n = unsafe { libc::read(std::os::fd::AsRawFd::as_raw_fd(&self.fd), buf.as_mut_ptr() as *mut _, buf.len()) };
            if n <= 0 {
                break;
            }
            let mut off = 0usize;
            while off + std::mem::size_of::<libc::inotify_event>() <= n as usize {
                let ev = unsafe { &*(buf.as_ptr().add(off) as *const libc::inotify_event) };
                let name_start = off + std::mem::size_of::<libc::inotify_event>();
                let name_bytes = &buf[name_start..name_start + ev.len as usize];
                let name = name_bytes.split(|b| *b == 0).next().unwrap_or(&[]);
                if name == CONFIG_FILE.as_bytes() {
                    touched = true;
                }
                off = name_start + ev.len as usize;
            }
        }
        touched
    }
}
