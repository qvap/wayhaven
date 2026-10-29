//! Still wallpapers: decoded on a worker thread (so parallax keeps animating meanwhile),
//! then handed to the main loop through a pipe it polls.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc::{channel, Receiver, Sender};

/// Larger pictures are downscaled before upload; matches the video texture limit
const MAX_SIDE: u32 = 8192;

pub struct Decoded {
    pub path: String,
    /// width, height, tightly packed RGBA8 rows (top row first)
    pub result: Result<(u32, u32, Vec<u8>), String>,
}

pub struct Loader {
    /// Becomes readable when a decode finished
    pub fd: OwnedFd,
    wake: OwnedFd,
    tx: Sender<Decoded>,
    rx: Receiver<Decoded>,
}

impl Loader {
    pub fn new() -> Result<Self, String> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } < 0 {
            return Err(format!("pipe2: {}", std::io::Error::last_os_error()));
        }
        let (fd, wake) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let (tx, rx) = channel();
        Ok(Loader { fd, wake, tx, rx })
    }

    pub fn load(&self, path: String) {
        let tx = self.tx.clone();
        let wake = self.wake.as_raw_fd();
        std::thread::spawn(move || {
            let result = decode(&path);
            let _ = tx.send(Decoded { path, result });
            // The pipe outlives the thread: it is only closed together with the daemon
            unsafe { libc::write(wake, [1u8].as_ptr() as *const _, 1) };
        });
    }

    /// Finished decodes, oldest first.
    pub fn take(&self) -> Vec<Decoded> {
        let mut buf = [0u8; 64];
        while unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr() as *mut _, buf.len()) } > 0 {}
        self.rx.try_iter().collect()
    }
}

fn decode(path: &str) -> Result<(u32, u32, Vec<u8>), String> {
    let mut reader = image::ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    // Huge pictures are downscaled right below; the default 512 MiB cap would reject them first
    reader.no_limits();
    let mut img = reader.decode().map_err(|e| e.to_string())?;
    let (w, h) = (img.width(), img.height());
    if w.max(h) > MAX_SIDE {
        let k = MAX_SIDE as f64 / w.max(h) as f64;
        img = img.resize((w as f64 * k) as u32, (h as f64 * k) as u32, image::imageops::FilterType::Triangle);
    }
    let rgba = img.into_rgba8();
    let (w, h) = rgba.dimensions();
    Ok((w, h, rgba.into_raw()))
}
