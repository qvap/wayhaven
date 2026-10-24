//! Daemon state, Wayland protocol handling and the main poll() loop.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd};
use std::time::{Duration, Instant};

use khronos_egl as egl;
use wayland_client::protocol::{wl_callback, wl_compositor, wl_output, wl_region, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_egl::WlEglSurface;
use wayland_protocols::wp::fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use crate::config::{self, Config, Watcher};
use crate::gfx::{self, Frame, Gfx};
use crate::hypr::Hypr;
use crate::ipc::{Client, Command, Server};
use crate::mpv::{self, Mpv};

/// Same as the shell's `Behavior on x { NumberAnimation { duration: 600; easing.type: Easing.OutCubic } }`
const PARALLAX_DURATION: Duration = Duration::from_millis(600);
const HYPR_DEBOUNCE: Duration = Duration::from_millis(40);
const CONFIG_DEBOUNCE: Duration = Duration::from_millis(120);
/// Never render the video texture larger than this (per side)
const MAX_FRAME_SIZE: f64 = 8192.0;

fn out_cubic(t: f64) -> f64 {
    let t = 1.0 - t.clamp(0.0, 1.0);
    1.0 - t * t * t
}

struct Parallax {
    from: (f64, f64),
    to: (f64, f64),
    start: Instant,
}

impl Parallax {
    fn new(v: (f64, f64)) -> Self {
        Parallax { from: v, to: v, start: Instant::now() - PARALLAX_DURATION }
    }
    fn value(&self, now: Instant) -> (f64, f64) {
        let t = out_cubic(now.duration_since(self.start).as_secs_f64() / PARALLAX_DURATION.as_secs_f64());
        (self.from.0 + (self.to.0 - self.from.0) * t, self.from.1 + (self.to.1 - self.from.1) * t)
    }
    fn animating(&self, now: Instant) -> bool {
        now.duration_since(self.start) < PARALLAX_DURATION && self.from != self.to
    }
    fn retarget(&mut self, to: (f64, f64), now: Instant) {
        if to != self.to {
            self.from = self.value(now);
            self.to = to;
            self.start = now;
        }
    }
}

struct Surface {
    wl: wl_surface::WlSurface,
    layer: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    viewport: Option<wp_viewport::WpViewport>,
    fractional: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    egl_window: Option<WlEglSurface>,
    egl_surface: Option<egl::Surface>,
    logical: (i32, i32),
    /// Preferred fractional scale * 120 (0 = unknown, use integer output scale)
    scale120: u32,
    buffer: (i32, i32),
    configured: bool,
    frame_pending: bool,
    needs_redraw: bool,
}

struct Output {
    global: u32,
    wl: wl_output::WlOutput,
    name: Option<String>,
    scale: i32,
    surface: Option<Surface>,
}

pub struct App {
    qh: QueueHandle<App>,
    wl_display_ptr: *mut std::ffi::c_void,
    compositor: Option<wl_compositor::WlCompositor>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    fractional_mgr: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    outputs: Vec<Output>,

    gfx: Gfx,
    mpv: Option<Mpv>,
    /// Latest video frame, sized for the largest output
    frame: Option<Frame>,
    /// Previous frame, kept while crossfading to a new video
    prev: Option<Frame>,
    fade_start: Option<Instant>,
    fade_duration: Duration,
    /// A new file was loaded and we are waiting for its first decoded frame
    awaiting_first_frame: bool,
    /// mpv has a frame that has not been rendered into `frame` yet
    frame_dirty: bool,

    config: Config,
    override_path: Option<String>,
    current_path: Option<String>,
    parallax: HashMap<String, Parallax>,
    zoom_override: Option<f64>,

    user_paused: bool,
    covered: HashMap<String, bool>,
    applied_pause: Option<bool>,

    hypr: Option<Hypr>,
    hypr_deadline: Option<Instant>,
    watcher: Option<Watcher>,
    config_deadline: Option<Instant>,
    server: Server,
    clients: Vec<Client>,
    running: bool,
}

impl App {
    fn zoom(&self) -> f64 {
        self.zoom_override.unwrap_or(self.config.zoom).max(0.1)
    }

    fn desired_path(&self) -> Option<String> {
        if let Some(p) = &self.override_path {
            return Some(p.clone());
        }
        let p = &self.config.wallpaper_path;
        (config::is_video(p) && std::path::Path::new(p).is_file()).then(|| p.clone())
    }

    fn parallax_of(&self, name: Option<&str>, now: Instant) -> (f64, f64) {
        name.and_then(|n| self.parallax.get(n).or_else(|| self.parallax.get("*")))
            .map(|p| p.value(now))
            .unwrap_or((0.5, 0.5))
    }

    // ---------------------------------------------------------------- lifecycle

    /// Bring playback in line with config/override/pause state.
    fn sync(&mut self) {
        let desired = self.desired_path();
        if desired != self.current_path {
            match &desired {
                Some(p) => self.play(p.clone()),
                None => self.stop(),
            }
        }
        self.sync_pause();
    }

    fn mpv_options(&self) -> Vec<(&str, String)> {
        let c = &self.config;
        let mut o: Vec<(&str, String)> = vec![
            ("vo", "libmpv".into()),
            ("hwdec", c.hwdec.clone()),
            ("loop-file", "inf".into()),
            ("keepaspect", "no".into()),
            ("speed", c.speed.to_string()),
            // Wallpaper-grade scaling: cheap but clean (hermite avoids aliasing when downscaling 4K)
            ("scale", "bilinear".into()),
            ("dscale", "hermite".into()),
            ("cscale", "bilinear".into()),
            ("correct-downscaling", "no".into()),
            ("linear-downscaling", "no".into()),
            ("sigmoid-upscaling", "no".into()),
            ("deband", "no".into()),
            ("interpolation", "no".into()),
            ("hdr-compute-peak", "no".into()),
            ("vd-lavc-dr", "yes".into()),
            // VRAM: mpv preallocates 6 extra GPU frames for interpolation/display-resample which we
            // don't use (~250 MB at 4K); 10-bit RGB intermediates are half the size of the default rgba16f
            ("hwdec-extra-frames", "2".into()),
            ("fbo-format", "rgb10_a2".into()),
            ("demuxer-max-bytes", "32MiB".into()),
            ("demuxer-max-back-bytes", "0".into()),
            ("cache", "no".into()),
            ("sid", "no".into()),
            ("osc", "no".into()),
            ("osd-level", "0".into()),
            ("input-default-bindings", "no".into()),
            ("input-vo-keyboard", "no".into()),
            ("stop-screensaver", "no".into()),
            ("terminal", "no".into()),
            ("config", "no".into()),
            ("load-scripts", "no".into()),
            // Built-in Lua scripts: each is a thread plus a Lua VM we have no use for
            ("load-console", "no".into()),
            ("load-commands", "no".into()),
            ("load-context-menu", "no".into()),
            ("load-select", "no".into()),
            ("load-positioning", "no".into()),
            ("load-stats-overlay", "no".into()),
            ("load-auto-profiles", "no".into()),
            ("ytdl", "no".into()),
        ];
        if c.audio {
            o.push(("volume", c.volume.to_string()));
        } else {
            o.push(("aid", "no".into()));
            o.push(("ao", "null".into()));
        }
        o.extend(c.mpv_options.iter().map(|(k, v)| (k.as_str(), v.clone())));
        o
    }

    fn play(&mut self, path: String) {
        log::info!("playing {path}");
        self.current_path = Some(path.clone());
        self.ensure_surfaces();
        if self.mpv.is_none() {
            self.gfx.make_current(None);
            match Mpv::new(&self.mpv_options(), self.wl_display_ptr) {
                Ok(m) => self.mpv = Some(m),
                Err(e) => {
                    log::error!("mpv init failed: {e}");
                    return;
                }
            }
            self.applied_pause = None;
        }
        // Whatever is on screen becomes the crossfade source
        if let Some(f) = self.frame.take() {
            if f.filled && self.prev.is_none() {
                self.prev = Some(f);
            } else {
                self.gfx.delete_frame(f);
            }
        }
        self.fade_start = None;
        self.fade_duration = Duration::from_millis(self.config.crossfade_ms);
        self.awaiting_first_frame = true;
        self.frame_dirty = false;
        if let Err(e) = self.mpv.as_ref().unwrap().load(&path) {
            log::error!("loadfile {path}: {e}");
        }
    }

    fn drop_player(&mut self) {
        self.gfx.make_current(None);
        if self.mpv.take().is_some() {
            // Decoder buffers are freed by mpv threads; give the memory back to the OS
            unsafe { libc::malloc_trim(0) };
        }
    }

    fn stop(&mut self) {
        log::info!("video wallpaper inactive");
        self.current_path = None;
        self.drop_player();
        for f in [self.frame.take(), self.prev.take()].into_iter().flatten() {
            self.gfx.delete_frame(f);
        }
        for i in 0..self.outputs.len() {
            self.destroy_surface(i);
        }
        self.applied_pause = None;
        self.awaiting_first_frame = false;
        self.frame_dirty = false;
    }

    fn sync_pause(&mut self) {
        let Some(mpv) = &self.mpv else { return };
        let visible: Vec<&Output> = self.outputs.iter().filter(|o| o.surface.is_some()).collect();
        let all_covered = !visible.is_empty()
            && visible.iter().all(|o| o.name.as_ref().and_then(|n| self.covered.get(n)).copied().unwrap_or(false));
        let paused = self.user_paused || all_covered;
        if self.applied_pause != Some(paused) {
            log::debug!("pause = {paused}");
            mpv.set("pause", if paused { "yes" } else { "no" });
            self.applied_pause = Some(paused);
        }
    }

    fn apply_config(&mut self, new: Config) {
        let old = std::mem::replace(&mut self.config, new);
        if let Some(mpv) = &self.mpv {
            let c = &self.config;
            if c.speed != old.speed {
                mpv.set("speed", &c.speed.to_string());
            }
            if c.hwdec != old.hwdec {
                mpv.set("hwdec", &c.hwdec);
            }
            if c.audio != old.audio {
                mpv.set("ao", if c.audio { "" } else { "null" });
                mpv.set("aid", if c.audio { "auto" } else { "no" });
            }
            if c.audio && c.volume != old.volume {
                mpv.set("volume", &c.volume.to_string());
            }
        }
        if self.config.mpv_options != old.mpv_options && self.mpv.is_some() {
            // Options only apply at init: restart the player, keeping the picture for a crossfade
            if let Some(path) = self.current_path.clone() {
                self.drop_player();
                self.play(path);
            }
        }
        if self.config.pause_mode != old.pause_mode {
            self.refresh_covered();
        }
        // A different wallpaper in the config always wins over a manual `set`
        if self.config.wallpaper_path != old.wallpaper_path {
            self.override_path = None;
        }
        if self.config.zoom != old.zoom && self.zoom_override.is_none() {
            self.ensure_frame();
        }
        self.sync();
    }

    fn refresh_covered(&mut self) {
        self.covered = match &self.hypr {
            Some(h) => h.covered_monitors(self.config.pause_mode),
            None => HashMap::new(),
        };
        self.sync_pause();
    }

    // ---------------------------------------------------------------- surfaces

    fn ensure_surfaces(&mut self) {
        for i in 0..self.outputs.len() {
            if self.outputs[i].surface.is_none() {
                self.create_surface(i);
            }
        }
    }

    fn create_surface(&mut self, i: usize) {
        let (Some(compositor), Some(layer_shell)) = (&self.compositor, &self.layer_shell) else { return };
        let qh = &self.qh;
        let out = &mut self.outputs[i];
        let wl = compositor.create_surface(qh, ());
        let layer = layer_shell.get_layer_surface(
            &wl,
            Some(&out.wl),
            zwlr_layer_shell_v1::Layer::Background,
            "wayhaven".into(),
            qh,
            out.global,
        );
        use zwlr_layer_surface_v1::Anchor;
        layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
        layer.set_size(0, 0);
        let empty = compositor.create_region(qh, ());
        wl.set_input_region(Some(&empty));
        empty.destroy();
        let (viewport, fractional) = match (&self.viewporter, &self.fractional_mgr) {
            (Some(vp), Some(fm)) => (Some(vp.get_viewport(&wl, qh, ())), Some(fm.get_fractional_scale(&wl, qh, out.global))),
            _ => (None, None),
        };
        wl.commit();
        out.surface = Some(Surface {
            wl,
            layer,
            viewport,
            fractional,
            egl_window: None,
            egl_surface: None,
            logical: (0, 0),
            scale120: 0,
            buffer: (0, 0),
            configured: false,
            frame_pending: false,
            needs_redraw: true,
        });
    }

    fn destroy_surface(&mut self, i: usize) {
        let Some(s) = self.outputs[i].surface.take() else { return };
        if let Some(es) = s.egl_surface {
            self.gfx.make_current(None);
            let _ = self.gfx.egl.destroy_surface(self.gfx.display, es);
        }
        drop(s.egl_window);
        if let Some(v) = s.viewport {
            v.destroy();
        }
        if let Some(f) = s.fractional {
            f.destroy();
        }
        s.layer.destroy();
        s.wl.destroy();
    }

    /// Recompute buffer size after a configure or scale change.
    fn resize_surface(&mut self, i: usize) {
        let out_scale = self.outputs[i].scale.max(1);
        let Some(compositor) = self.compositor.clone() else { return };
        let Some(s) = self.outputs[i].surface.as_mut() else { return };
        if !s.configured || s.logical.0 <= 0 || s.logical.1 <= 0 {
            return;
        }
        let (lw, lh) = s.logical;
        let buffer = match (&s.viewport, s.scale120) {
            (Some(vp), sc) if sc > 0 => {
                vp.set_destination(lw, lh);
                s.wl.set_buffer_scale(1);
                (((lw as u64 * sc as u64 + 60) / 120) as i32, ((lh as u64 * sc as u64 + 60) / 120) as i32)
            }
            _ => {
                s.wl.set_buffer_scale(out_scale);
                (lw * out_scale, lh * out_scale)
            }
        };
        let region = compositor.create_region(&self.qh, ());
        region.add(0, 0, lw, lh);
        s.wl.set_opaque_region(Some(&region));
        region.destroy();

        let resized = buffer != s.buffer || s.egl_window.is_none();
        if resized {
            s.buffer = buffer;
            match &s.egl_window {
                Some(w) => w.resize(buffer.0, buffer.1, 0, 0),
                None => match WlEglSurface::new(s.wl.id(), buffer.0, buffer.1) {
                    Ok(w) => {
                        let es = unsafe {
                            self.gfx.egl.create_window_surface(self.gfx.display, self.gfx.config, w.ptr() as egl::NativeWindowType, None)
                        };
                        match es {
                            Ok(es) => {
                                self.gfx.make_current(Some(es));
                                // Pace with frame callbacks instead; NVIDIA blocks in eglSwapBuffers otherwise
                                let _ = self.gfx.egl.swap_interval(self.gfx.display, 0);
                                s.egl_surface = Some(es);
                            }
                            Err(e) => log::error!("eglCreateWindowSurface: {e}"),
                        }
                        s.egl_window = Some(w);
                    }
                    Err(e) => log::error!("wl_egl_window: {e}"),
                },
            }
        }
        s.needs_redraw = true;
        if resized {
            self.ensure_frame();
        }
    }

    // ---------------------------------------------------------------- frames

    /// Size of the offscreen video texture: the zoomed cover size on the largest output.
    fn frame_size(&self, vw: f64, vh: f64) -> Option<(i32, i32)> {
        let zoom = self.zoom();
        let mut best: Option<(f64, f64)> = None;
        for s in self.outputs.iter().filter_map(|o| o.surface.as_ref()).filter(|s| s.buffer.0 > 0) {
            let (_, _, w, h) = gfx::layout(s.buffer.0 as f64, s.buffer.1 as f64, vw, vh, zoom, 0.5, 0.5);
            if best.is_none_or(|(bw, bh)| w * h > bw * bh) {
                best = Some((w, h));
            }
        }
        let (mut w, mut h) = best?;
        let k = (MAX_FRAME_SIZE / w.max(h)).min(1.0);
        w *= k;
        h *= k;
        Some((w.round().max(1.0) as i32, h.round().max(1.0) as i32))
    }

    /// (Re)create the video texture if its required size changed.
    /// (`play` drops the texture of the previous file, so any existing one belongs to the current video.)
    fn ensure_frame(&mut self) {
        let Some((vw, vh)) = self.mpv.as_ref().and_then(|m| m.video_size()) else { return };
        let Some((w, h)) = self.frame_size(vw, vh) else { return };
        if let Some(f) = &self.frame {
            if f.w == w && f.h == h && f.video_w == vw && f.video_h == vh {
                return;
            }
        }
        self.gfx.make_current(None);
        if let Some(old) = self.frame.take() {
            if old.filled && self.prev.is_none() {
                // Resize of the same video: swap without a visible fade
                self.fade_duration = Duration::ZERO;
                self.prev = Some(old);
            } else {
                self.gfx.delete_frame(old);
            }
        }
        match self.gfx.create_frame(w, h, vw, vh) {
            Ok(f) => {
                log::debug!("video texture {w}x{h} for {vw}x{vh}");
                self.frame = Some(f);
                // A resize can re-render the current frame right away; a new file must wait for its first frame
                if !self.awaiting_first_frame {
                    self.frame_dirty = true;
                }
            }
            Err(e) => log::error!("create frame: {e}"),
        }
    }

    fn fade_alpha(&self, now: Instant) -> f32 {
        match self.fade_start {
            None => 0.0,
            Some(_) if self.fade_duration.is_zero() => 1.0,
            Some(t) => out_cubic(now.duration_since(t).as_secs_f64() / self.fade_duration.as_secs_f64()) as f32,
        }
    }

    // ---------------------------------------------------------------- rendering

    fn render(&mut self) {
        let now = Instant::now();
        // Finish crossfade
        if self.prev.is_some() && self.fade_start.is_some_and(|t| now.duration_since(t) >= self.fade_duration) {
            self.gfx.make_current(None);
            self.gfx.delete_frame(self.prev.take().unwrap());
            self.fade_start = None;
        }
        let fading = self.fade_start.is_some();

        let ready: Vec<usize> = (0..self.outputs.len())
            .filter(|&i| {
                self.outputs[i].surface.as_ref().is_some_and(|s| s.configured && s.egl_surface.is_some() && !s.frame_pending)
            })
            .collect();
        if ready.is_empty() {
            return;
        }

        // Decode output -> texture, only when someone can actually show it
        let mut rendered_video = false;
        if self.frame_dirty {
            if let (Some(mpv), Some(frame)) = (&self.mpv, &mut self.frame) {
                let es = self.outputs[ready[0]].surface.as_ref().unwrap().egl_surface;
                self.gfx.make_current(es);
                mpv.render(frame.fbo.0.get(), frame.w, frame.h);
                if !frame.filled {
                    log::debug!("first frame rendered into {}x{} texture", frame.w, frame.h);
                    frame.filled = true;
                    // The previous file's demuxer/decoder buffers are gone by now
                    unsafe { libc::malloc_trim(0) };
                    if self.prev.is_some() {
                        self.fade_start = Some(now);
                    }
                }
                self.frame_dirty = false;
                rendered_video = true;
                for o in &mut self.outputs {
                    if let Some(s) = &mut o.surface {
                        s.needs_redraw = true;
                    }
                }
            }
        }

        let alpha = self.fade_alpha(now);
        let zoom = self.zoom();
        for i in ready {
            let name = self.outputs[i].name.clone();
            let animating = name
                .as_deref()
                .and_then(|n| self.parallax.get(n).or_else(|| self.parallax.get("*")))
                .is_some_and(|p| p.animating(now));
            let (fx, fy) = self.parallax_of(name.as_deref(), now);
            let global = self.outputs[i].global;
            let s = self.outputs[i].surface.as_mut().unwrap();
            if !(s.needs_redraw || animating || fading) {
                continue;
            }
            let frame = self.frame.as_ref().filter(|f| f.filled);
            if frame.is_none() && self.prev.is_none() {
                continue; // nothing to show yet; keep the surface unmapped
            }
            let (bw, bh) = (s.buffer.0 as f64, s.buffer.1 as f64);
            self.gfx.make_current(s.egl_surface);
            self.gfx.begin(s.buffer.0, s.buffer.1);
            if let Some(p) = &self.prev {
                self.gfx.draw(p, gfx::layout(bw, bh, p.video_w, p.video_h, zoom, fx, fy), bw, bh, 1.0);
            }
            if let Some(f) = frame {
                let a = if self.prev.is_some() { alpha } else { 1.0 };
                if a > 0.0 {
                    self.gfx.draw(f, gfx::layout(bw, bh, f.video_w, f.video_h, zoom, fx, fy), bw, bh, a);
                }
            }
            s.wl.frame(&self.qh, global);
            if let Err(e) = self.gfx.egl.swap_buffers(self.gfx.display, s.egl_surface.unwrap()) {
                log::error!("eglSwapBuffers: {e}");
            }
            s.frame_pending = true;
            s.needs_redraw = false;
        }
        if rendered_video {
            if let Some(mpv) = &self.mpv {
                mpv.report_swap();
            }
        }
    }

    // ---------------------------------------------------------------- IPC

    fn handle_command(&mut self, cmd: Command) -> Option<String> {
        let now = Instant::now();
        match cmd {
            Command::Parallax { output, fx, fy, zoom } => {
                let target = (fx.clamp(0.0, 1.0), fy.clamp(0.0, 1.0));
                let first = !self.parallax.contains_key(&output);
                let shown = self.frame.as_ref().is_some_and(|f| f.filled);
                let p = self.parallax.entry(output).or_insert_with(|| Parallax::new(target));
                if !first && shown {
                    p.retarget(target, now);
                } else {
                    *p = Parallax::new(target);
                }
                if let Some(z) = zoom.filter(|z| *z > 0.0) {
                    if self.zoom_override != Some(z) {
                        self.zoom_override = Some(z);
                        self.ensure_frame();
                    }
                }
                self.mark_all_redraw();
                None
            }
            Command::Set(path) => {
                let path = shellexpand(&path);
                if !std::path::Path::new(&path).is_file() {
                    return Some(format!("error: no such file: {path}"));
                }
                self.override_path = Some(path);
                self.sync();
                Some("ok".into())
            }
            Command::Clear => {
                self.override_path = None;
                self.sync();
                Some("ok".into())
            }
            Command::Pause | Command::Resume | Command::Toggle => {
                self.user_paused = match cmd {
                    Command::Pause => true,
                    Command::Resume => false,
                    _ => !self.user_paused,
                };
                self.sync_pause();
                Some(if self.user_paused { "paused" } else { "playing" }.into())
            }
            Command::Reload => {
                self.apply_config(Config::load());
                Some("ok".into())
            }
            Command::Status => Some(self.status_json()),
            Command::Snapshot(path) => {
                let Some(f) = self.frame.as_ref().filter(|f| f.filled) else {
                    return Some("error: no video frame yet".into());
                };
                self.gfx.make_current(None);
                Some(match self.gfx.read_frame(f) {
                    Ok(ppm) => match std::fs::write(&path, ppm) {
                        Ok(()) => "ok".into(),
                        Err(e) => format!("error: {e}"),
                    },
                    Err(e) => format!("error: {e}"),
                })
            }
            Command::Ping => Some("pong".into()),
            Command::Quit => {
                self.running = false;
                Some("ok".into())
            }
        }
    }

    fn mark_all_redraw(&mut self) {
        for o in &mut self.outputs {
            if let Some(s) = &mut o.surface {
                s.needs_redraw = true;
            }
        }
    }

    fn status_json(&self) -> String {
        let now = Instant::now();
        let outputs: Vec<serde_json::Value> = self
            .outputs
            .iter()
            .map(|o| {
                let (fx, fy) = self.parallax_of(o.name.as_deref(), now);
                serde_json::json!({
                    "name": o.name,
                    "active": o.surface.is_some(),
                    "buffer": o.surface.as_ref().map(|s| [s.buffer.0, s.buffer.1]),
                    "covered": o.name.as_ref().and_then(|n| self.covered.get(n)).copied().unwrap_or(false),
                    "parallax": [fx, fy],
                })
            })
            .collect();
        serde_json::json!({
            "path": self.current_path,
            "paused": self.applied_pause.unwrap_or(false),
            "userPaused": self.user_paused,
            "zoom": self.zoom(),
            "video": self.mpv.as_ref().and_then(|m| m.video_size()).map(|(w, h)| [w, h]),
            "texture": self.frame.as_ref().map(|f| [f.w, f.h]),
            "hwdec": self.mpv.as_ref().map(|m| m.hwdec_current()),
            "pauseMode": format!("{:?}", self.config.pause_mode).to_lowercase(),
            "hyprland": self.hypr.is_some(),
            "outputs": outputs,
        })
        .to_string()
    }

    // ---------------------------------------------------------------- event sources

    fn on_mpv_events(&mut self) {
        let Some(mpv) = &mut self.mpv else { return };
        for ev in mpv.poll_events() {
            match ev {
                mpv::Event::VideoReconfig | mpv::Event::VideoSize => self.ensure_frame(),
                mpv::Event::FileLoaded => log::debug!("file loaded"),
                mpv::Event::PlaybackError(e) => log::error!("playback error: {e}"),
                mpv::Event::Shutdown => {}
            }
        }
    }

    fn on_mpv_render(&mut self) {
        let Some(mpv) = &self.mpv else { return };
        if mpv.render_update() {
            if self.awaiting_first_frame {
                self.awaiting_first_frame = false;
                // Usually the reconfig already created the texture for the new file
                if self.frame.is_none() {
                    self.ensure_frame();
                }
            }
            self.frame_dirty = true;
        }
    }
}

fn shellexpand(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()),
        None => p.to_string(),
    }
}

// -------------------------------------------------------------------- Wayland dispatch

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(app: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match ev {
            wl_registry::Event::Global { name, interface, version } => match interface.as_str() {
                "wl_compositor" => app.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "zwlr_layer_shell_v1" => app.layer_shell = Some(reg.bind(name, version.min(4), qh, ())),
                "wp_viewporter" => app.viewporter = Some(reg.bind(name, 1, qh, ())),
                "wp_fractional_scale_manager_v1" => app.fractional_mgr = Some(reg.bind(name, 1, qh, ())),
                "wl_output" => {
                    let wl = reg.bind(name, version.min(4), qh, name);
                    app.outputs.push(Output { global: name, wl, name: None, scale: 1, surface: None });
                    if app.current_path.is_some() {
                        let i = app.outputs.len() - 1;
                        app.create_surface(i);
                    }
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = app.outputs.iter().position(|o| o.global == name) {
                    app.destroy_surface(i);
                    let o = app.outputs.remove(i);
                    if o.wl.version() >= 3 {
                        o.wl.release();
                    }
                    app.sync_pause();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for App {
    fn event(app: &mut Self, _: &wl_output::WlOutput, ev: wl_output::Event, global: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some(i) = app.outputs.iter().position(|o| o.global == *global) else { return };
        match ev {
            wl_output::Event::Name { name } => app.outputs[i].name = Some(name),
            wl_output::Event::Scale { factor } => {
                if app.outputs[i].scale != factor {
                    app.outputs[i].scale = factor;
                    app.resize_surface(i);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, u32> for App {
    fn event(
        app: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        ev: zwlr_layer_surface_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(i) = app.outputs.iter().position(|o| o.global == *global) else { return };
        match ev {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                layer.ack_configure(serial);
                if let Some(s) = &mut app.outputs[i].surface {
                    s.configured = true;
                    s.logical = (width as i32, height as i32);
                }
                app.resize_surface(i);
            }
            zwlr_layer_surface_v1::Event::Closed => {
                app.destroy_surface(i);
                app.sync_pause();
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, u32> for App {
    fn event(
        app: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        ev: wp_fractional_scale_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = ev {
            let Some(i) = app.outputs.iter().position(|o| o.global == *global) else { return };
            if let Some(s) = &mut app.outputs[i].surface {
                if s.scale120 != scale {
                    s.scale120 = scale;
                    app.resize_surface(i);
                }
            }
        }
    }
}

impl Dispatch<wl_callback::WlCallback, u32> for App {
    fn event(app: &mut Self, _: &wl_callback::WlCallback, ev: wl_callback::Event, global: &u32, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = ev {
            if let Some(s) = app.outputs.iter_mut().find(|o| o.global == *global).and_then(|o| o.surface.as_mut()) {
                s.frame_pending = false;
            }
        }
    }
}

macro_rules! ignore_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for App {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(
    wl_compositor::WlCompositor,
    wl_surface::WlSurface,
    wl_region::WlRegion,
    zwlr_layer_shell_v1::ZwlrLayerShellV1,
    wp_viewporter::WpViewporter,
    wp_viewport::WpViewport,
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1
);

// -------------------------------------------------------------------- main loop

pub fn run(server: Server) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("wayland: {e}"))?;
    let mut queue: EventQueue<App> = conn.new_event_queue();
    let qh = queue.handle();
    let wl_display_ptr = conn.backend().display_ptr() as *mut std::ffi::c_void;
    conn.display().get_registry(&qh, ());

    let gfx = Gfx::new(wl_display_ptr)?;
    let hypr = Hypr::connect();
    if hypr.is_none() {
        log::warn!("Hyprland IPC unavailable: automatic pausing disabled");
    }
    let mut app = App {
        qh,
        wl_display_ptr,
        compositor: None,
        layer_shell: None,
        viewporter: None,
        fractional_mgr: None,
        outputs: Vec::new(),
        gfx,
        mpv: None,
        frame: None,
        prev: None,
        fade_start: None,
        fade_duration: Duration::ZERO,
        awaiting_first_frame: false,
        frame_dirty: false,
        config: Config::load(),
        override_path: None,
        current_path: None,
        parallax: HashMap::new(),
        zoom_override: None,
        user_paused: false,
        covered: HashMap::new(),
        applied_pause: None,
        hypr,
        hypr_deadline: None,
        watcher: Watcher::new(),
        config_deadline: None,
        server,
        clients: Vec::new(),
        running: true,
    };
    // Globals, then output names
    queue.roundtrip(&mut app).map_err(|e| e.to_string())?;
    queue.roundtrip(&mut app).map_err(|e| e.to_string())?;
    if app.compositor.is_none() || app.layer_shell.is_none() {
        return Err("compositor lacks wl_compositor or zwlr_layer_shell_v1".into());
    }
    app.refresh_covered();
    app.sync();

    let mut pollfds: Vec<libc::pollfd> = Vec::new();
    while app.running {
        queue.dispatch_pending(&mut app).map_err(|e| e.to_string())?;
        let now = Instant::now();
        if app.config_deadline.is_some_and(|d| now >= d) {
            app.config_deadline = None;
            app.apply_config(Config::load());
        }
        if app.hypr_deadline.is_some_and(|d| now >= d) {
            app.hypr_deadline = None;
            app.refresh_covered();
        }
        app.render();
        conn.flush().map_err(|e| e.to_string())?;

        let guard = loop {
            match queue.prepare_read() {
                Some(g) => break g,
                None => {
                    queue.dispatch_pending(&mut app).map_err(|e| e.to_string())?;
                }
            }
        };

        // Fixed slots first, then clients
        const WL: usize = 0;
        const SERVER: usize = 1;
        const MPV_EV: usize = 2;
        const MPV_RENDER: usize = 3;
        const INOTIFY: usize = 4;
        const HYPR: usize = 5;
        const CLIENTS: usize = 6;
        let pfd = |fd: i32| libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        pollfds.clear();
        pollfds.push(pfd(guard.connection_fd().as_raw_fd()));
        pollfds.push(pfd(app.server.listener.as_raw_fd()));
        pollfds.push(pfd(app.mpv.as_ref().map_or(-1, |m| m.event_fd.as_raw_fd())));
        pollfds.push(pfd(app.mpv.as_ref().map_or(-1, |m| m.render_fd.as_raw_fd())));
        pollfds.push(pfd(app.watcher.as_ref().map_or(-1, |w| w.fd.as_raw_fd())));
        pollfds.push(pfd(app.hypr.as_ref().map_or(-1, |h| h.events.as_fd().as_raw_fd())));
        for c in &app.clients {
            pollfds.push(pfd(c.stream.as_raw_fd()));
        }

        let timeout = [app.config_deadline, app.hypr_deadline]
            .into_iter()
            .flatten()
            .map(|d| d.saturating_duration_since(Instant::now()).as_millis().min(i32::MAX as u128) as i32)
            .min()
            .unwrap_or(-1);
        let n = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            drop(guard);
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("poll: {err}"));
        }
        let ready = |i: usize| pollfds[i].revents != 0;

        if ready(WL) {
            if let Err(e) = guard.read() {
                if !matches!(e, wayland_client::backend::WaylandError::Io(ref io) if io.kind() == std::io::ErrorKind::WouldBlock) {
                    return Err(format!("wayland read: {e}"));
                }
            }
        } else {
            drop(guard);
        }
        if ready(MPV_EV) {
            app.on_mpv_events();
        }
        if ready(MPV_RENDER) {
            app.on_mpv_render();
        }
        if ready(INOTIFY) && app.watcher.as_ref().is_some_and(|w| w.read()) {
            app.config_deadline = Some(Instant::now() + CONFIG_DEBOUNCE);
        }
        if ready(HYPR) {
            let (relevant, alive) = app.hypr.as_mut().unwrap().read_events();
            if !alive {
                log::warn!("Hyprland event socket closed");
                app.hypr = None;
                app.covered.clear();
                app.sync_pause();
            } else if relevant && app.hypr_deadline.is_none() {
                app.hypr_deadline = Some(Instant::now() + HYPR_DEBOUNCE);
            }
        }
        // Clients: indices in pollfds match app.clients at poll time
        let mut keep = vec![true; app.clients.len()];
        for ci in 0..app.clients.len() {
            if pollfds[CLIENTS + ci].revents == 0 {
                continue;
            }
            let (cmds, alive) = app.clients[ci].read();
            for cmd in cmds {
                let reply = match cmd {
                    Ok(c) => app.handle_command(c),
                    Err(e) => Some(format!("error: {e}")),
                };
                if let Some(r) = reply {
                    app.clients[ci].reply(&r);
                }
            }
            keep[ci] = alive;
        }
        let mut k = keep.into_iter();
        app.clients.retain(|_| k.next().unwrap());
        if ready(SERVER) {
            let new = app.server.accept();
            app.clients.extend(new);
        }
    }
    app.stop();
    Ok(())
}
