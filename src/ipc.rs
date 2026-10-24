//! Line-based control socket at `$XDG_RUNTIME_DIR/wayhaven.sock`.
//!
//! Commands (one per line):
//!   parallax <output|*> <fx> <fy> [zoom]   fire-and-forget, sent by the shell
//!   set <path> | clear | pause | resume | toggle | reload | status | snapshot <file.ppm> | ping | quit

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    dir.join("wayhaven.sock")
}

pub enum Command {
    Parallax { output: String, fx: f64, fy: f64, zoom: Option<f64> },
    Set(String),
    Clear,
    Pause,
    Resume,
    Toggle,
    Reload,
    Status,
    /// Dump the current video texture to a PPM file (diagnostics)
    Snapshot(String),
    Ping,
    Quit,
}

fn parse(line: &str) -> Result<Command, String> {
    let line = line.trim();
    let (verb, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    Ok(match verb {
        "parallax" => {
            let p: Vec<&str> = rest.split_whitespace().collect();
            if p.len() < 3 {
                return Err("usage: parallax <output|*> <fx> <fy> [zoom]".into());
            }
            let num = |s: &str| s.parse::<f64>().ok().filter(|v| v.is_finite()).ok_or(format!("bad number '{s}'"));
            Command::Parallax {
                output: p[0].to_string(),
                fx: num(p[1])?,
                fy: num(p[2])?,
                zoom: p.get(3).map(|z| num(z)).transpose()?,
            }
        }
        "set" if !rest.is_empty() => Command::Set(rest.to_string()),
        "clear" => Command::Clear,
        "pause" => Command::Pause,
        "resume" => Command::Resume,
        "toggle" => Command::Toggle,
        "reload" => Command::Reload,
        "status" => Command::Status,
        "snapshot" if !rest.is_empty() => Command::Snapshot(rest.to_string()),
        "ping" => Command::Ping,
        "quit" => Command::Quit,
        _ => return Err(format!("unknown command '{line}'")),
    })
}

pub struct Client {
    pub stream: UnixStream,
    buf: Vec<u8>,
}

impl Client {
    /// Read available data; returns parsed commands and whether the client is still connected.
    pub fn read(&mut self) -> (Vec<Result<Command, String>>, bool) {
        let mut chunk = [0u8; 4096];
        let mut alive = true;
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    alive = false;
                    break;
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    alive = false;
                    break;
                }
            }
        }
        if self.buf.len() > 1 << 20 {
            return (vec![Err("line too long".into())], false);
        }
        let mut cmds = Vec::new();
        while let Some(nl) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            if !line.trim().is_empty() {
                cmds.push(parse(&line));
            }
        }
        (cmds, alive)
    }

    pub fn reply(&mut self, msg: &str) {
        // Best effort: replies are tiny, a full socket buffer means the peer isn't reading anyway
        let _ = self.stream.write_all(format!("{msg}\n").as_bytes());
    }
}

pub struct Server {
    pub listener: UnixListener,
    path: PathBuf,
}

impl Server {
    /// Binds the socket. Returns Ok(None) if another daemon is already listening.
    pub fn bind() -> std::io::Result<Option<Self>> {
        let path = socket_path();
        if UnixStream::connect(&path).is_ok() {
            return Ok(None);
        }
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        Ok(Some(Server { listener, path }))
    }

    pub fn accept(&self) -> Vec<Client> {
        let mut out = Vec::new();
        while let Ok((stream, _)) = self.listener.accept() {
            if stream.set_nonblocking(true).is_ok() {
                out.push(Client { stream, buf: Vec::new() });
            }
        }
        out
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// CLI side: send one command and print the reply.
pub fn send(cmd: &str) -> Result<String, String> {
    let mut s = UnixStream::connect(socket_path()).map_err(|e| format!("daemon not running? ({e})"))?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(3))).ok();
    s.write_all(format!("{cmd}\n").as_bytes()).map_err(|e| e.to_string())?;
    let mut reply = Vec::new();
    let mut byte = [0u8; 1];
    while let Ok(1) = s.read(&mut byte) {
        if byte[0] == b'\n' {
            break;
        }
        reply.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&reply).into_owned())
}
