//! Hyprland IPC: listens on socket2 for window/workspace events and works out
//! which monitors have their wallpaper covered (so playback can be paused).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde_json::Value;

use crate::config::PauseMode;

pub struct Hypr {
    dir: PathBuf,
    pub events: UnixStream,
    buf: Vec<u8>,
}

const RELEVANT_EVENTS: &[&str] = &[
    "workspace", "workspacev2", "focusedmon", "focusedmonv2", "activespecial", "activespecialv2",
    "fullscreen", "openwindow", "closewindow", "movewindow", "movewindowv2", "changefloatingmode",
    "monitoradded", "monitoraddedv2", "monitorremoved", "monitorremovedv2", "minimized",
];

impl Hypr {
    pub fn connect() -> Option<Self> {
        let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
        let runtime = std::env::var("XDG_RUNTIME_DIR").ok()?;
        let dir = PathBuf::from(runtime).join("hypr").join(sig);
        let events = UnixStream::connect(dir.join(".socket2.sock")).ok()?;
        events.set_nonblocking(true).ok()?;
        Some(Hypr { dir, events, buf: Vec::new() })
    }

    /// Drain socket2; returns (relevant event seen, connection still alive).
    pub fn read_events(&mut self) -> (bool, bool) {
        let mut chunk = [0u8; 8192];
        let mut relevant = false;
        loop {
            match self.events.read(&mut chunk) {
                Ok(0) => return (relevant, false),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return (relevant, false),
            }
        }
        while let Some(nl) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            if let Some(sep) = line.windows(2).position(|w| w == b">>") {
                if let Ok(name) = std::str::from_utf8(&line[..sep]) {
                    relevant |= RELEVANT_EVENTS.contains(&name);
                }
            }
        }
        (relevant, true)
    }

    fn request(&self, cmd: &str) -> Option<Value> {
        let mut s = UnixStream::connect(self.dir.join(".socket.sock")).ok()?;
        s.set_read_timeout(Some(std::time::Duration::from_millis(500))).ok()?;
        s.write_all(cmd.as_bytes()).ok()?;
        let mut out = Vec::new();
        s.read_to_end(&mut out).ok()?;
        serde_json::from_slice(&out).ok()
    }

    /// Monitor name -> is the wallpaper covered on it.
    pub fn covered_monitors(&self, mode: PauseMode) -> HashMap<String, bool> {
        let mut result = HashMap::new();
        if mode == PauseMode::Never {
            return result;
        }
        let (Some(monitors), Some(clients)) = (self.request("j/monitors"), self.request("j/clients")) else {
            return result;
        };
        let clients = clients.as_array().cloned().unwrap_or_default();
        for m in monitors.as_array().into_iter().flatten() {
            let Some(name) = m["name"].as_str() else { continue };
            let mut workspaces = vec![m["activeWorkspace"]["id"].as_i64().unwrap_or(i64::MIN)];
            if let Some(special) = m["specialWorkspace"]["id"].as_i64().filter(|id| *id != 0) {
                workspaces.push(special);
            }
            let covered = clients.iter().any(|c| {
                let on_ws = c["workspace"]["id"].as_i64().is_some_and(|id| workspaces.contains(&id));
                let visible = c["mapped"].as_bool().unwrap_or(true) && !c["hidden"].as_bool().unwrap_or(false);
                if !on_ws || !visible {
                    return false;
                }
                let fullscreen = c["fullscreen"].as_i64().unwrap_or(0) > 0;
                let tiled = !c["floating"].as_bool().unwrap_or(false);
                fullscreen || (mode == PauseMode::Tiled && tiled)
            });
            result.insert(name.to_string(), covered);
        }
        result
    }
}
