mod app;
mod config;
mod gfx;
mod hypr;
mod ipc;
mod mpv;

use std::process::ExitCode;

const USAGE: &str = "wayhaven — video wallpaper daemon for Wayland (end-4 / illogical-impulse integration)

Usage:
  wayhaven daemon [-v]          Run the daemon (follows background.wallpaperPath in the shell config)
  wayhaven set <video>          Play a video now (until the shell config's wallpaper changes)
  wayhaven clear                Drop a manual `set` and follow the shell config again
  wayhaven pause|resume|toggle  Manual pause control
  wayhaven parallax <output|*> <fx> <fy> [zoom]
                                Move the visible window (fractions 0..1), as the shell does
  wayhaven reload               Re-read the shell config
  wayhaven status               Print daemon state as JSON
  wayhaven quit                 Stop the daemon
  wayhaven --version";

struct Logger {
    level: log::LevelFilter,
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.level && m.target().starts_with("wayhaven")
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("[wayhaven {}] {}", r.level().as_str().to_lowercase(), r.args());
        }
    }
    fn flush(&self) {}
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verb = args.first().map(String::as_str).unwrap_or("daemon");

    match verb {
        "daemon" => {
            // mpv runs a dozen threads; per-thread malloc arenas fragment badly when switching
            // between videos of different sizes. Two arenas are plenty for this workload.
            unsafe { libc::mallopt(libc::M_ARENA_MAX, 2) };
            let verbose = args.iter().any(|a| a == "-v" || a == "--verbose");
            let level = if verbose { log::LevelFilter::Debug } else { log::LevelFilter::Info };
            let _ = log::set_boxed_logger(Box::new(Logger { level })).map(|()| log::set_max_level(level));
            let server = match ipc::Server::bind() {
                Ok(Some(s)) => s,
                Ok(None) => {
                    log::info!("already running");
                    return ExitCode::SUCCESS;
                }
                Err(e) => {
                    log::error!("cannot bind {}: {e}", ipc::socket_path().display());
                    return ExitCode::FAILURE;
                }
            };
            match app::run(server) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    log::error!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
        "-V" | "--version" | "version" => {
            println!("wayhaven {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            let mut line = args.join(" ");
            if verb == "set" {
                // Resolve relative paths against the caller's cwd
                if let Some(p) = args.get(1..).map(|r| r.join(" ")) {
                    let abs = std::path::absolute(&p).map(|a| a.display().to_string()).unwrap_or(p);
                    line = format!("set {abs}");
                }
            }
            match ipc::send(&line) {
                Ok(reply) => {
                    if verb == "quit" {
                        // Wait until the daemon is really gone, so an immediate restart can bind
                        for _ in 0..60 {
                            if std::os::unix::net::UnixStream::connect(ipc::socket_path()).is_err() {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                    }
                    println!("{reply}");
                    if reply.starts_with("error") { ExitCode::FAILURE } else { ExitCode::SUCCESS }
                }
                Err(e) => {
                    eprintln!("wayhaven: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
