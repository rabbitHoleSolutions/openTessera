use std::cell::RefCell;
use std::ffi::CString;
use std::os::raw::{c_char, c_void};

use glow::HasContext;
use glutin::prelude::*;
use tessera_core::Rect;

thread_local! {
    static GL_LOADER: RefCell<Option<Box<dyn Fn(&str) -> *const c_void>>> = const { RefCell::new(None) };
}

pub fn set_gl_loader(loader: impl Fn(&str) -> *const c_void + 'static) {
    GL_LOADER.with(|l| *l.borrow_mut() = Some(Box::new(loader)));
}

pub extern "C" fn mpv_get_proc_address(_ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    let name = unsafe { std::ffi::CStr::from_ptr(name) }.to_bytes();
    let name = String::from_utf8_lossy(name).into_owned();
    GL_LOADER.with(|l| {
        l.borrow()
            .as_ref()
            .map(|f| f(&name) as *mut c_void)
            .unwrap_or(std::ptr::null_mut())
    })
}

const VERTEX_SRC: &str = r#"
#version 330 core
layout (location = 0) in vec2 pos;
layout (location = 1) in vec2 uv;
uniform vec2 screen;
out vec2 v_uv;
void main() {
    vec2 clip = (pos / screen) * 2.0 - 1.0;
    gl_Position = vec4(clip.x, -clip.y, 0.0, 1.0);
    v_uv = uv;
}
"#;

const FRAGMENT_SRC: &str = r#"
#version 330 core
in vec2 v_uv;
out vec4 frag;
uniform sampler2D tex;
void main() {
    frag = texture(tex, v_uv);
}
"#;

pub struct Compositor {
    program: <glow::Context as HasContext>::Program,
    vao: <glow::Context as HasContext>::VertexArray,
    vbo: <glow::Context as HasContext>::Buffer,
    uniform_screen: <glow::Context as HasContext>::UniformLocation,
    uniform_tex: <glow::Context as HasContext>::UniformLocation,
}

#[allow(clippy::type_complexity)]
impl Compositor {
    pub fn new(gl: &glow::Context) -> Compositor {
        unsafe {
            let program = gl.create_program().expect("program");
            let shader = |kind, src| {
                let sh = gl.create_shader(kind).expect("shader");
                gl.shader_source(sh, src);
                gl.compile_shader(sh);
                if !gl.get_shader_compile_status(sh) {
                    panic!("shader compile: {}", gl.get_shader_info_log(sh));
                }
                gl.attach_shader(program, sh);
                sh
            };
            let vs = shader(glow::VERTEX_SHADER, VERTEX_SRC);
            let fs = shader(glow::FRAGMENT_SHADER, FRAGMENT_SRC);
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                panic!("program link: {}", gl.get_program_info_log(program));
            }
            gl.delete_shader(vs);
            gl.delete_shader(fs);

            let vao = gl.create_vertex_array().expect("vao");
            let vbo = gl.create_buffer().expect("vbo");
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_size(glow::ARRAY_BUFFER, 6 * 4 * std::mem::size_of::<f32>() as i32, glow::STREAM_DRAW);
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 16, 0);
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, 16, 8);
            gl.bind_vertex_array(None);

            let uniform_screen = gl.get_uniform_location(program, "screen").expect("uniform screen");
            let uniform_tex = gl.get_uniform_location(program, "tex").expect("uniform tex");

            Compositor { program, vao, vbo, uniform_screen, uniform_tex }
        }
    }

    /// Draws a tile texture letterboxed into `cell` (top-left pixel origin).
    pub fn draw(
        &self,
        gl: &glow::Context,
        texture: <glow::Context as HasContext>::Texture,
        cell: &Rect,
        video_aspect: f32,
        screen_w: i32,
        screen_h: i32,
    ) {
        let r = tessera_core::layout::fit_aspect(cell, video_aspect);
        // mpv renders into the FBO texture with flip_y=1, so texture row 0 is
        // the bottom of the video: map the top of the rect to v=1.
        let x0 = r.x;
        let y0 = r.y;
        let x1 = r.x + r.w;
        let y1 = r.y + r.h;
        let vertices: [f32; 24] = [
            x0, y0, 0.0, 1.0, //
            x1, y0, 1.0, 1.0, //
            x1, y1, 1.0, 0.0, //
            x0, y0, 0.0, 1.0, //
            x1, y1, 1.0, 0.0, //
            x0, y1, 0.0, 0.0, //
        ];
        unsafe {
            gl.use_program(Some(self.program));
            gl.uniform_2_f32(Some(&self.uniform_screen), screen_w as f32, screen_h as f32);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.uniform_1_i32(Some(&self.uniform_tex), 0);
            gl.bind_vertex_array(Some(self.vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, std::slice::from_raw_parts(
                vertices.as_ptr() as *const u8,
                std::mem::size_of_val(&vertices),
            ));
            gl.draw_arrays(glow::TRIANGLES, 0, 6);
            gl.bind_vertex_array(None);
        }
    }

    pub fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_buffer(self.vbo);
            gl.delete_vertex_array(self.vao);
            gl.delete_program(self.program);
        }
    }
}

/// GL proc-address lookup via a glutin display (context-independent on EGL).
pub fn display_loader(
    display: &glutin::display::Display,
) -> impl Fn(&str) -> *const c_void + use<> {
    let display = display.clone();
    move |name| {
        let c = CString::new(name).unwrap();
        display.get_proc_address(&c) as *const c_void
    }
}
