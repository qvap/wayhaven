//! Minimal libmpv bindings: core client API + OpenGL render API.
//!
//! The render context uses MPV_RENDER_PARAM_ADVANCED_CONTROL (direct rendering etc.), which makes
//! it a hard rule that the render thread (our only thread) never waits for the mpv core. So after
//! initialization everything here is asynchronous: commands and property writes use the *_async
//! API, and the properties we need are observed and arrive as events.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;

#[repr(C)]
struct MpvHandle {
    _p: [u8; 0],
}
#[repr(C)]
struct MpvRenderContext {
    _p: [u8; 0],
}

#[repr(C)]
struct RenderParam {
    kind: c_int,
    data: *mut c_void,
}

type GetProcAddress = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;

#[repr(C)]
struct OpenGlInitParams {
    get_proc_address: Option<GetProcAddress>,
    get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
struct OpenGlFbo {
    fbo: c_int,
    w: c_int,
    h: c_int,
    internal_format: c_int,
}

#[repr(C)]
struct RawEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct RawEventProperty {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct RawEventEndFile {
    reason: c_int,
    error: c_int,
}

const RENDER_PARAM_API_TYPE: c_int = 1;
const RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const RENDER_PARAM_OPENGL_FBO: c_int = 3;
const RENDER_PARAM_FLIP_Y: c_int = 4;
const RENDER_PARAM_WL_DISPLAY: c_int = 9;
const RENDER_PARAM_ADVANCED_CONTROL: c_int = 10;
const RENDER_PARAM_BLOCK_FOR_TARGET_TIME: c_int = 12;
const RENDER_UPDATE_FRAME: u64 = 1;

const FORMAT_STRING: c_int = 1;
const FORMAT_INT64: c_int = 4;

const EVENT_SHUTDOWN: c_int = 1;
const EVENT_END_FILE: c_int = 7;
const EVENT_FILE_LOADED: c_int = 8;
const EVENT_VIDEO_RECONFIG: c_int = 17;
const EVENT_PROPERTY_CHANGE: c_int = 22;
const END_FILE_REASON_ERROR: c_int = 4;

#[link(name = "mpv")]
extern "C" {
    fn mpv_create() -> *mut MpvHandle;
    fn mpv_initialize(ctx: *mut MpvHandle) -> c_int;
    fn mpv_terminate_destroy(ctx: *mut MpvHandle);
    fn mpv_set_option_string(ctx: *mut MpvHandle, name: *const c_char, data: *const c_char) -> c_int;
    fn mpv_set_property_async(ctx: *mut MpvHandle, userdata: u64, name: *const c_char, format: c_int, data: *mut c_void) -> c_int;
    fn mpv_command_async(ctx: *mut MpvHandle, userdata: u64, args: *mut *const c_char) -> c_int;
    fn mpv_observe_property(ctx: *mut MpvHandle, userdata: u64, name: *const c_char, format: c_int) -> c_int;
    fn mpv_wait_event(ctx: *mut MpvHandle, timeout: f64) -> *mut RawEvent;
    fn mpv_set_wakeup_callback(ctx: *mut MpvHandle, cb: Option<extern "C" fn(*mut c_void)>, d: *mut c_void);
    fn mpv_error_string(error: c_int) -> *const c_char;
    fn mpv_render_context_create(res: *mut *mut MpvRenderContext, mpv: *mut MpvHandle, params: *mut RenderParam) -> c_int;
    fn mpv_render_context_set_update_callback(ctx: *mut MpvRenderContext, cb: Option<extern "C" fn(*mut c_void)>, d: *mut c_void);
    fn mpv_render_context_update(ctx: *mut MpvRenderContext) -> u64;
    fn mpv_render_context_render(ctx: *mut MpvRenderContext, params: *mut RenderParam) -> c_int;
    fn mpv_render_context_report_swap(ctx: *mut MpvRenderContext);
    fn mpv_render_context_free(ctx: *mut MpvRenderContext);
}

extern "C" {
    fn eglGetProcAddress(name: *const c_char) -> *mut c_void;
}

unsafe extern "C" fn get_proc_address(_ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    eglGetProcAddress(name)
}

/// Called from mpv threads: just poke an eventfd so the main loop wakes up.
extern "C" fn poke_eventfd(d: *mut c_void) {
    let one: u64 = 1;
    unsafe { libc::write(d as usize as c_int, &one as *const u64 as *const c_void, 8) };
}

fn err_str(code: c_int) -> String {
    unsafe { CStr::from_ptr(mpv_error_string(code)) }.to_string_lossy().into_owned()
}

fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap()
}

fn new_eventfd() -> std::io::Result<OwnedFd> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn drain_eventfd(fd: c_int) {
    let mut v: u64 = 0;
    unsafe { libc::read(fd, &mut v as *mut u64 as *mut c_void, 8) };
}

const PROP_DWIDTH: u64 = 1;
const PROP_DHEIGHT: u64 = 2;
const PROP_HWDEC: u64 = 3;

pub enum Event {
    FileLoaded,
    /// Video output (re)configured: a new file started decoding or its size changed
    VideoReconfig,
    /// Display size (after aspect correction) changed and is known
    VideoSize,
    PlaybackError(String),
    Shutdown,
}

pub struct Mpv {
    handle: *mut MpvHandle,
    render: *mut MpvRenderContext,
    /// Woken when mpv has client events (property changes, file loaded, ...)
    pub event_fd: OwnedFd,
    /// Woken when the render context wants a new frame drawn
    pub render_fd: OwnedFd,
    dwidth: i64,
    dheight: i64,
    hwdec_current: String,
}

impl Mpv {
    /// `options` are applied before initialization. An OpenGL context must be current.
    pub fn new(options: &[(&str, String)], wl_display: *mut c_void) -> Result<Self, String> {
        let event_fd = new_eventfd().map_err(|e| e.to_string())?;
        let render_fd = new_eventfd().map_err(|e| e.to_string())?;
        unsafe {
            let handle = mpv_create();
            if handle.is_null() {
                return Err("mpv_create failed".into());
            }
            let mut mpv = Mpv {
                handle,
                render: ptr::null_mut(),
                event_fd,
                render_fd,
                dwidth: 0,
                dheight: 0,
                hwdec_current: String::new(),
            };
            for (k, v) in options {
                let r = mpv_set_option_string(handle, cstr(k).as_ptr(), cstr(v).as_ptr());
                if r < 0 {
                    log::warn!("mpv option {k}={v}: {}", err_str(r));
                }
            }
            let r = mpv_initialize(handle);
            if r < 0 {
                return Err(format!("mpv_initialize: {}", err_str(r)));
            }
            mpv_observe_property(handle, PROP_DWIDTH, c"dwidth".as_ptr(), FORMAT_INT64);
            mpv_observe_property(handle, PROP_DHEIGHT, c"dheight".as_ptr(), FORMAT_INT64);
            mpv_observe_property(handle, PROP_HWDEC, c"hwdec-current".as_ptr(), FORMAT_STRING);
            mpv_set_wakeup_callback(handle, Some(poke_eventfd), mpv.event_fd.as_raw_fd() as usize as *mut c_void);

            let mut gl_init = OpenGlInitParams { get_proc_address: Some(get_proc_address), get_proc_address_ctx: ptr::null_mut() };
            let mut advanced: c_int = 1;
            let mut params = [
                RenderParam { kind: RENDER_PARAM_API_TYPE, data: c"opengl".as_ptr() as *mut c_void },
                RenderParam { kind: RENDER_PARAM_OPENGL_INIT_PARAMS, data: &mut gl_init as *mut _ as *mut c_void },
                RenderParam { kind: RENDER_PARAM_WL_DISPLAY, data: wl_display },
                RenderParam { kind: RENDER_PARAM_ADVANCED_CONTROL, data: &mut advanced as *mut _ as *mut c_void },
                RenderParam { kind: 0, data: ptr::null_mut() },
            ];
            let r = mpv_render_context_create(&mut mpv.render, handle, params.as_mut_ptr());
            if r < 0 {
                return Err(format!("mpv_render_context_create: {}", err_str(r)));
            }
            mpv_render_context_set_update_callback(mpv.render, Some(poke_eventfd), mpv.render_fd.as_raw_fd() as usize as *mut c_void);
            Ok(mpv)
        }
    }

    /// Fire-and-forget command; failures are reported as events.
    pub fn command(&self, args: &[&str]) -> Result<(), String> {
        let owned: Vec<CString> = args.iter().map(|a| cstr(a)).collect();
        let mut ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(ptr::null());
        let r = unsafe { mpv_command_async(self.handle, 0, ptrs.as_mut_ptr()) };
        if r < 0 {
            return Err(err_str(r));
        }
        Ok(())
    }

    pub fn set(&self, name: &str, value: &str) {
        let v = cstr(value);
        let mut p = v.as_ptr();
        // mpv copies the value before returning
        let r = unsafe { mpv_set_property_async(self.handle, 0, cstr(name).as_ptr(), FORMAT_STRING, &mut p as *mut _ as *mut c_void) };
        if r < 0 {
            log::warn!("mpv set {name}={value}: {}", err_str(r));
        }
    }

    pub fn load(&self, path: &str) -> Result<(), String> {
        self.command(&["loadfile", path, "replace"])
    }

    /// Display size of the current video (after aspect correction), if known.
    pub fn video_size(&self) -> Option<(f64, f64)> {
        (self.dwidth > 0 && self.dheight > 0).then_some((self.dwidth as f64, self.dheight as f64))
    }

    pub fn hwdec_current(&self) -> &str {
        &self.hwdec_current
    }

    /// Drain pending client events. Call when `event_fd` is readable.
    pub fn poll_events(&mut self) -> Vec<Event> {
        drain_eventfd(self.event_fd.as_raw_fd());
        let mut out = Vec::new();
        loop {
            let ev = unsafe { &*mpv_wait_event(self.handle, 0.0) };
            match ev.event_id {
                0 => break,
                EVENT_SHUTDOWN => out.push(Event::Shutdown),
                EVENT_FILE_LOADED => out.push(Event::FileLoaded),
                EVENT_END_FILE => {
                    let ef = unsafe { &*(ev.data as *const RawEventEndFile) };
                    if ef.reason == END_FILE_REASON_ERROR {
                        out.push(Event::PlaybackError(err_str(ef.error)));
                    }
                }
                EVENT_VIDEO_RECONFIG => out.push(Event::VideoReconfig),
                EVENT_PROPERTY_CHANGE => {
                    // Unavailable properties (e.g. between files) arrive with format NONE
                    let p = unsafe { &*(ev.data as *const RawEventProperty) };
                    match (ev.reply_userdata, p.format) {
                        (PROP_DWIDTH | PROP_DHEIGHT, f) => {
                            let v = if f == FORMAT_INT64 { unsafe { *(p.data as *const i64) } } else { 0 };
                            let old = (self.dwidth, self.dheight);
                            if ev.reply_userdata == PROP_DWIDTH { self.dwidth = v } else { self.dheight = v }
                            if old != (self.dwidth, self.dheight) && self.video_size().is_some() {
                                out.push(Event::VideoSize);
                            }
                        }
                        (PROP_HWDEC, FORMAT_STRING) => {
                            let sp = unsafe { *(p.data as *const *const c_char) };
                            self.hwdec_current = if sp.is_null() { String::new() } else { unsafe { CStr::from_ptr(sp) }.to_string_lossy().into_owned() };
                        }
                        (PROP_HWDEC, _) => self.hwdec_current.clear(),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Call when `render_fd` is readable. Returns true if a new video frame is ready to be rendered.
    pub fn render_update(&self) -> bool {
        drain_eventfd(self.render_fd.as_raw_fd());
        unsafe { mpv_render_context_update(self.render) & RENDER_UPDATE_FRAME != 0 }
    }

    /// Render the current frame into `fbo` (GL name). The GL context must be current.
    pub fn render(&self, fbo: u32, w: i32, h: i32) {
        let mut target = OpenGlFbo { fbo: fbo as c_int, w, h, internal_format: 0 };
        let mut flip: c_int = 0;
        let mut block: c_int = 0;
        let mut params = [
            RenderParam { kind: RENDER_PARAM_OPENGL_FBO, data: &mut target as *mut _ as *mut c_void },
            RenderParam { kind: RENDER_PARAM_FLIP_Y, data: &mut flip as *mut _ as *mut c_void },
            RenderParam { kind: RENDER_PARAM_BLOCK_FOR_TARGET_TIME, data: &mut block as *mut _ as *mut c_void },
            RenderParam { kind: 0, data: ptr::null_mut() },
        ];
        unsafe { mpv_render_context_render(self.render, params.as_mut_ptr()) };
    }

    pub fn report_swap(&self) {
        unsafe { mpv_render_context_report_swap(self.render) };
    }
}

impl Drop for Mpv {
    /// The GL context used at creation must be current while dropping.
    fn drop(&mut self) {
        unsafe {
            if !self.render.is_null() {
                mpv_render_context_free(self.render);
            }
            mpv_set_wakeup_callback(self.handle, None, ptr::null_mut());
            mpv_terminate_destroy(self.handle);
        }
    }
}
