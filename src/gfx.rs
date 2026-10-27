//! EGL context + a tiny GL compositor: the wallpaper (a still picture, or a video rendered by
//! mpv once per video frame) lives in an offscreen texture, and each output then just samples
//! its (parallax-shifted) window of that texture.

use std::ffi::c_void;

use glow::HasContext;
use khronos_egl as egl;

pub type Egl = egl::Instance<egl::Static>;

const PLATFORM_WAYLAND_KHR: egl::Enum = 0x31D8;

pub struct Gfx {
    pub egl: Egl,
    pub display: egl::Display,
    pub config: egl::Config,
    pub context: egl::Context,
    pub gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    u_dst: glow::UniformLocation,
    u_alpha: glow::UniformLocation,
    u_lod: glow::UniformLocation,
}

/// An offscreen render target holding the wallpaper picture or the latest video frame.
pub struct Frame {
    pub fbo: glow::Framebuffer,
    pub tex: glow::Texture,
    pub w: i32,
    pub h: i32,
    /// Display size of the video this frame belongs to
    pub video_w: f64,
    pub video_h: f64,
    /// Whether a frame has been rendered into it yet
    pub filled: bool,
    /// Mip levels match level 0 (stale after every video frame; needed for blur)
    pub mips: bool,
    /// A still picture: always sampled with mipmaps for clean downscaling
    pub still: bool,
}

const VS: &str = r#"#version 330 core
uniform vec4 u_dst; // left, top, right, bottom in NDC
out vec2 v_uv;
void main() {
    vec2 p = vec2(float(gl_VertexID & 1), float((gl_VertexID >> 1) & 1));
    v_uv = p; // mpv renders top row first when not flipping
    gl_Position = vec4(mix(u_dst.xy, u_dst.zw, p), 0.0, 1.0);
}
"#;

const FS: &str = r#"#version 330 core
in vec2 v_uv;
uniform sampler2D u_tex;
uniform float u_alpha;
uniform float u_lod; // blur: sample this mip level with a 3x3 tent
out vec4 o_color;
void main() {
    vec3 c;
    if (u_lod <= 0.0) {
        c = texture(u_tex, v_uv).rgb;
    } else {
        vec2 step = exp2(u_lod) / vec2(textureSize(u_tex, 0));
        c = vec3(0.0);
        for (int i = -1; i <= 1; i++)
            for (int j = -1; j <= 1; j++)
                c += (2.0 - abs(float(i))) * (2.0 - abs(float(j))) * textureLod(u_tex, v_uv + vec2(i, j) * step, u_lod).rgb;
        c /= 16.0;
    }
    o_color = vec4(c, u_alpha);
}
"#;

impl Gfx {
    pub fn new(wl_display: *mut c_void) -> Result<Self, String> {
        let egl = egl::Instance::new(egl::Static);
        let display = unsafe { egl.get_platform_display(PLATFORM_WAYLAND_KHR, wl_display, &[egl::ATTRIB_NONE]) }
            .map_err(|e| format!("eglGetPlatformDisplay: {e}"))?;
        egl.initialize(display).map_err(|e| format!("eglInitialize: {e}"))?;
        egl.bind_api(egl::OPENGL_API).map_err(|e| format!("eglBindAPI: {e}"))?;

        let attrs = [
            egl::SURFACE_TYPE, egl::WINDOW_BIT,
            egl::RENDERABLE_TYPE, egl::OPENGL_BIT,
            egl::RED_SIZE, 8,
            egl::GREEN_SIZE, 8,
            egl::BLUE_SIZE, 8,
            egl::ALPHA_SIZE, 0,
            egl::NONE,
        ];
        let config = egl
            .choose_first_config(display, &attrs)
            .map_err(|e| format!("eglChooseConfig: {e}"))?
            .ok_or("no suitable EGL config")?;
        let ctx_attrs = [
            egl::CONTEXT_MAJOR_VERSION, 3,
            egl::CONTEXT_MINOR_VERSION, 3,
            egl::CONTEXT_OPENGL_PROFILE_MASK, egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
            egl::NONE,
        ];
        let context = egl
            .create_context(display, config, None, &ctx_attrs)
            .map_err(|e| format!("eglCreateContext: {e}"))?;
        // Surfaceless is enough to compile shaders; NVIDIA and Mesa both support it.
        egl.make_current(display, None, None, Some(context))
            .map_err(|e| format!("eglMakeCurrent (surfaceless): {e}"))?;

        let gl = unsafe {
            glow::Context::from_loader_function(|name| match egl.get_proc_address(name) {
                Some(f) => f as *const c_void,
                None => std::ptr::null(),
            })
        };

        unsafe {
            let program = gl.create_program()?;
            let mut shaders = Vec::new();
            for (kind, src) in [(glow::VERTEX_SHADER, VS), (glow::FRAGMENT_SHADER, FS)] {
                let s = gl.create_shader(kind)?;
                gl.shader_source(s, src);
                gl.compile_shader(s);
                if !gl.get_shader_compile_status(s) {
                    return Err(format!("shader: {}", gl.get_shader_info_log(s)));
                }
                gl.attach_shader(program, s);
                shaders.push(s);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(format!("link: {}", gl.get_program_info_log(program)));
            }
            for s in shaders {
                gl.detach_shader(program, s);
                gl.delete_shader(s);
            }
            let u_dst = gl.get_uniform_location(program, "u_dst").ok_or("u_dst")?;
            let u_alpha = gl.get_uniform_location(program, "u_alpha").ok_or("u_alpha")?;
            let u_lod = gl.get_uniform_location(program, "u_lod").ok_or("u_lod")?;
            let vao = gl.create_vertex_array()?;
            Ok(Gfx { egl, display, config, context, gl, program, vao, u_dst, u_alpha, u_lod })
        }
    }

    pub fn make_current(&self, surface: Option<egl::Surface>) {
        if let Err(e) = self.egl.make_current(self.display, surface, surface, Some(self.context)) {
            log::error!("eglMakeCurrent: {e}");
        }
    }

    pub fn create_frame(&self, w: i32, h: i32, video_w: f64, video_h: f64) -> Result<Frame, String> {
        unsafe {
            let gl = &self.gl;
            let tex = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            // Full mip chain: stills always use it, videos only while blurred
            let levels = 32 - (w.max(h).max(1) as u32).leading_zeros() as i32;
            gl.tex_storage_2d(glow::TEXTURE_2D, levels, glow::RGBA8, w, h);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            let fbo = gl.create_framebuffer()?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            if status != glow::FRAMEBUFFER_COMPLETE {
                gl.delete_framebuffer(fbo);
                gl.delete_texture(tex);
                return Err(format!("framebuffer incomplete: {status:#x}"));
            }
            Ok(Frame { fbo, tex, w, h, video_w, video_h, filled: false, mips: false, still: false })
        }
    }

    /// Upload a decoded picture (RGBA8, top row first, `f.w`x`f.h`) into level 0 and build its mips.
    pub fn upload(&self, f: &mut Frame, rgba: &[u8]) {
        unsafe {
            let gl = &self.gl;
            gl.bind_texture(glow::TEXTURE_2D, Some(f.tex));
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, f.w, f.h, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(rgba)));
        }
        f.filled = true;
        f.still = true;
        self.generate_mips(f);
    }

    pub fn generate_mips(&self, f: &mut Frame) {
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, Some(f.tex));
            self.gl.generate_mipmap(glow::TEXTURE_2D);
        }
        f.mips = true;
    }

    pub fn delete_frame(&self, f: Frame) {
        unsafe {
            self.gl.delete_framebuffer(f.fbo);
            self.gl.delete_texture(f.tex);
        }
    }

    /// Read back a frame as a binary PPM image.
    pub fn read_frame(&self, f: &Frame) -> Result<Vec<u8>, String> {
        let mut px = vec![0u8; (f.w * f.h * 4) as usize];
        unsafe {
            let gl = &self.gl;
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(f.fbo));
            gl.read_pixels(0, 0, f.w, f.h, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut px)));
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            let err = gl.get_error();
            if err != glow::NO_ERROR {
                return Err(format!("glReadPixels: {err:#x}"));
            }
        }
        let mut out = format!("P6\n{} {}\n255\n", f.w, f.h).into_bytes();
        out.reserve((f.w * f.h * 3) as usize);
        for p in px.chunks_exact(4) {
            out.extend_from_slice(&p[..3]);
        }
        Ok(out)
    }

    /// Start drawing to the window surface: bind default framebuffer and clear to black.
    pub fn begin(&self, w: i32, h: i32) {
        unsafe {
            let gl = &self.gl;
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            // The context was first made current surfaceless, which leaves the default
            // framebuffer's draw/read buffers at GL_NONE (EGL_KHR_surfaceless_context)
            gl.draw_buffer(glow::BACK);
            gl.read_buffer(glow::BACK);
            gl.viewport(0, 0, w, h);
            // mpv's renderer shares this context and may leave state behind
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::STENCIL_TEST);
            gl.disable(glow::CULL_FACE);
            gl.disable(glow::RASTERIZER_DISCARD);
            gl.disable(glow::FRAMEBUFFER_SRGB);
            gl.color_mask(true, true, true, true);
            gl.bind_sampler(0, None);
            gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
            gl.bind_buffer(glow::PIXEL_PACK_BUFFER, None);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);
        }
    }

    /// Draw `frame` covering the pixel rect (x, y, w, h) (top-left origin) of a `sw`x`sh` surface,
    /// blurred by sampling mip level `lod` (0 = sharp; needs `frame.mips`).
    pub fn draw(&self, frame: &Frame, rect: (f64, f64, f64, f64), sw: f64, sh: f64, alpha: f32, lod: f32) {
        let (x, y, w, h) = rect;
        let l = (x / sw * 2.0 - 1.0) as f32;
        let r = ((x + w) / sw * 2.0 - 1.0) as f32;
        let t = (1.0 - y / sh * 2.0) as f32;
        let b = (1.0 - (y + h) / sh * 2.0) as f32;
        unsafe {
            let gl = &self.gl;
            if alpha < 1.0 {
                gl.enable(glow::BLEND);
                gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            } else {
                gl.disable(glow::BLEND);
            }
            gl.bind_texture(glow::TEXTURE_2D, Some(frame.tex));
            let use_mips = frame.mips && (frame.still || lod > 0.0);
            let min = if use_mips { glow::LINEAR_MIPMAP_LINEAR } else { glow::LINEAR };
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, min as i32);
            gl.uniform_4_f32(Some(&self.u_dst), l, t, r, b);
            gl.uniform_1_f32(Some(&self.u_alpha), alpha);
            gl.uniform_1_f32(Some(&self.u_lod), if use_mips { lod } else { 0.0 });
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }
}

/// Where the (zoomed, aspect-covering) video lands on a `sw`x`sh` surface.
/// Mirrors the math in the shell's Background.qml so widgets and video move in sync.
pub fn layout(sw: f64, sh: f64, vw: f64, vh: f64, zoom: f64, fx: f64, fy: f64) -> (f64, f64, f64, f64) {
    let cover = (sw / vw).max(sh / vh);
    let w = vw * cover * zoom;
    let h = vh * cover * zoom;
    let x = if w <= sw { (sw - w) / 2.0 } else { -(w - sw) * fx.clamp(0.0, 1.0) };
    let y = if h <= sh { (sh - h) / 2.0 } else { -(h - sh) * fy.clamp(0.0, 1.0) };
    (x, y, w, h)
}

/// Scale a placed rect about the surface center (lock-screen zoom).
pub fn zoom_about_center(rect: (f64, f64, f64, f64), sw: f64, sh: f64, k: f64) -> (f64, f64, f64, f64) {
    let (x, y, w, h) = rect;
    let (cx, cy) = (sw / 2.0, sh / 2.0);
    (cx + (x - cx) * k, cy + (y - cy) * k, w * k, h * k)
}
