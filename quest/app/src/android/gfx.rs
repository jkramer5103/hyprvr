//! GL ES 3 helpers: shader programs, textures, framebuffers and the few draw calls we need.

use anyhow::{Result, anyhow};
use glam::Mat4;
use glow::HasContext;

use super::egl::GL_TEXTURE_EXTERNAL_OES;

pub const GL_SRGB8_ALPHA8: u32 = 0x8C43;

const BLIT_VS: &str = r#"#version 300 es
uniform vec4 u_dst;
uniform vec4 u_src;
out vec2 v_uv;
void main() {
    vec2 c = vec2(float(gl_VertexID & 1), float((gl_VertexID >> 1) & 1));
    v_uv = mix(u_src.xy, u_src.zw, c);
    gl_Position = vec4(mix(u_dst.xy, u_dst.zw, c), 0.0, 1.0);
}
"#;

const BLIT_FS: &str = r#"#version 300 es
precision mediump float;
uniform sampler2D u_tex;
in vec2 v_uv;
out vec4 o;
void main() { o = texture(u_tex, v_uv); }
"#;

const OES_FS: &str = r#"#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require
precision mediump float;
uniform samplerExternalOES u_tex;
in vec2 v_uv;
out vec4 o;
vec3 to_linear(vec3 c) {
    return mix(c / 12.92, pow((c + 0.055) / 1.055, vec3(2.4)), step(0.04045, c));
}
void main() {
    vec4 c = texture(u_tex, v_uv);
    o = vec4(to_linear(clamp(c.rgb, 0.0, 1.0)), 1.0);
}
"#;

const COLOR_VS: &str = r#"#version 300 es
uniform mat4 u_mvp;
layout(location = 0) in vec3 a_pos;
layout(location = 1) in vec4 a_color;
layout(location = 2) in vec3 a_uvk;
out vec4 v_color;
out vec3 v_uvk;
void main() {
    v_color = a_color;
    v_uvk = a_uvk;
    gl_Position = u_mvp * vec4(a_pos, 1.0);
}
"#;

const COLOR_FS: &str = r#"#version 300 es
precision mediump float;
in vec4 v_color;
in vec3 v_uvk;
out vec4 o;
void main() {
    if (v_uvk.z > 0.5) {
        float d = dot(v_uvk.xy, v_uvk.xy);
        if (d > 1.0) discard;
        float a = clamp((1.0 - d) * 6.0, 0.0, 1.0);
        o = v_color * a;
    } else {
        o = v_color;
    }
}
"#;

const ENV_VS: &str = r#"#version 300 es
out vec2 v_ndc;
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2)) * 2.0 - 1.0;
    v_ndc = p;
    gl_Position = vec4(p, 0.0, 1.0);
}
"#;

const ENV_FS: &str = r#"#version 300 es
precision highp float;
uniform mat4 u_inv_vp;
uniform int u_mode;
uniform vec4 u_color;
uniform sampler2D u_tex;
in vec2 v_ndc;
out vec4 o;
void main() {
    if (u_mode == 0) { o = u_color; return; }
    vec4 p = u_inv_vp * vec4(v_ndc, 1.0, 1.0);
    vec3 dir = normalize(p.xyz / p.w);
    float a = atan(dir.x, -dir.z) / 6.2831853;
    float v = acos(clamp(dir.y, -1.0, 1.0)) / 3.14159265;
    vec2 uva = vec2(a + 0.5, v);
    vec2 uvb = vec2(fract(a + 1.0), v);
    vec2 dxa = dFdx(uva), dya = dFdy(uva), dxb = dFdx(uvb), dyb = dFdy(uvb);
    bool use_b = abs(dxa.x) + abs(dya.x) > abs(dxb.x) + abs(dyb.x);
    o = vec4(textureGrad(u_tex, uva, use_b ? dxb : dxa, use_b ? dyb : dya).rgb, 1.0);
}
"#;

pub struct Program {
    pub prog: glow::Program,
    uniforms: Vec<(&'static str, Option<glow::UniformLocation>)>,
}

impl Program {
    fn new(gl: &glow::Context, vs: &str, fs: &str, names: &[&'static str]) -> Result<Self> {
        unsafe {
            let prog = gl.create_program().map_err(|e| anyhow!(e))?;
            let mut shaders = vec![];
            for (ty, src) in [(glow::VERTEX_SHADER, vs), (glow::FRAGMENT_SHADER, fs)] {
                let s = gl.create_shader(ty).map_err(|e| anyhow!(e))?;
                gl.shader_source(s, src);
                gl.compile_shader(s);
                if !gl.get_shader_compile_status(s) {
                    return Err(anyhow!("shader compile: {}", gl.get_shader_info_log(s)));
                }
                gl.attach_shader(prog, s);
                shaders.push(s);
            }
            gl.link_program(prog);
            if !gl.get_program_link_status(prog) {
                return Err(anyhow!("program link: {}", gl.get_program_info_log(prog)));
            }
            for s in shaders {
                gl.detach_shader(prog, s);
                gl.delete_shader(s);
            }
            let uniforms = names.iter().map(|n| (*n, gl.get_uniform_location(prog, n))).collect();
            Ok(Program { prog, uniforms })
        }
    }

    fn loc(&self, name: &str) -> Option<&glow::UniformLocation> {
        self.uniforms.iter().find(|(n, _)| *n == name).and_then(|(_, l)| l.as_ref())
    }
}

/// One vertex of overlay geometry.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub color: [f32; 4],
    /// xy: disc coordinates, z > 0.5: draw as disc.
    pub uvk: [f32; 3],
}

pub struct Gfx {
    pub gl: glow::Context,
    blit: Program,
    oes: Program,
    color: Program,
    env: Program,
    vao_empty: glow::VertexArray,
    vao_color: glow::VertexArray,
    vbo: glow::Buffer,
    vbo_cap: usize,
    scratch_fbo: glow::Framebuffer,
}

/// Destination rectangle in pixels of the target, origin = first memory row.
#[derive(Clone, Copy, Debug)]
pub struct PxRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Gfx {
    pub fn new(gl: glow::Context) -> Result<Self> {
        unsafe {
            let blit = Program::new(&gl, BLIT_VS, BLIT_FS, &["u_dst", "u_src", "u_tex"])?;
            let oes = Program::new(&gl, BLIT_VS, OES_FS, &["u_dst", "u_src", "u_tex"])?;
            let color = Program::new(&gl, COLOR_VS, COLOR_FS, &["u_mvp"])?;
            let env = Program::new(&gl, ENV_VS, ENV_FS, &["u_inv_vp", "u_mode", "u_color", "u_tex"])?;
            let vao_empty = gl.create_vertex_array().map_err(|e| anyhow!(e))?;
            let vao_color = gl.create_vertex_array().map_err(|e| anyhow!(e))?;
            let vbo = gl.create_buffer().map_err(|e| anyhow!(e))?;
            gl.bind_vertex_array(Some(vao_color));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            let stride = std::mem::size_of::<Vertex>() as i32;
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 3, glow::FLOAT, false, stride, 0);
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(1, 4, glow::FLOAT, false, stride, 12);
            gl.enable_vertex_attrib_array(2);
            gl.vertex_attrib_pointer_f32(2, 3, glow::FLOAT, false, stride, 28);
            gl.bind_vertex_array(None);
            let scratch_fbo = gl.create_framebuffer().map_err(|e| anyhow!(e))?;
            Ok(Gfx { gl, blit, oes, color, env, vao_empty, vao_color, vbo, vbo_cap: 0, scratch_fbo })
        }
    }

    /// Create an RGBA texture (sRGB-encoded storage when `srgb`).
    pub fn texture(&self, w: u32, h: u32, srgb: bool, mips: bool) -> Result<glow::Texture> {
        unsafe {
            let gl = &self.gl;
            let t = gl.create_texture().map_err(|e| anyhow!(e))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(t));
            let levels = if mips { (w.max(h) as f32).log2().floor() as i32 + 1 } else { 1 };
            gl.tex_storage_2d(glow::TEXTURE_2D, levels, if srgb { GL_SRGB8_ALPHA8 } else { glow::RGBA8 }, w as i32, h as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, if mips { glow::LINEAR_MIPMAP_LINEAR } else { glow::LINEAR } as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            Ok(t)
        }
    }

    pub fn upload(&self, tex: glow::Texture, x: u32, y: u32, w: u32, h: u32, rgba: &[u8]) {
        debug_assert!(rgba.len() >= (w * h * 4) as usize);
        unsafe {
            let gl = &self.gl;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                x as i32,
                y as i32,
                w as i32,
                h as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&rgba[..(w * h * 4) as usize])),
            );
        }
    }

    pub fn generate_mipmaps(&self, tex: glow::Texture) {
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            self.gl.generate_mipmap(glow::TEXTURE_2D);
        }
    }

    pub fn delete_texture(&self, t: glow::Texture) {
        unsafe { self.gl.delete_texture(t) }
    }

    pub fn external_texture(&self) -> Result<glow::Texture> {
        unsafe {
            let gl = &self.gl;
            let t = gl.create_texture().map_err(|e| anyhow!(e))?;
            gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(t));
            gl.tex_parameter_i32(GL_TEXTURE_EXTERNAL_OES, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(GL_TEXTURE_EXTERNAL_OES, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(GL_TEXTURE_EXTERNAL_OES, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(GL_TEXTURE_EXTERNAL_OES, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            Ok(t)
        }
    }

    pub fn bind_external(&self, t: glow::Texture) {
        unsafe { self.gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(t)) }
    }

    /// Bind `tex` (size w x h) as the render target.
    pub fn target(&self, tex: glow::Texture, w: u32, h: u32) -> bool {
        unsafe {
            let gl = &self.gl;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.scratch_fbo));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
            let ok = gl.check_framebuffer_status(glow::FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE;
            gl.viewport(0, 0, w as i32, h as i32);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            ok
        }
    }

    pub fn viewport(&self, x: i32, y: i32, w: i32, h: i32) {
        unsafe {
            self.gl.viewport(x, y, w, h);
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl.scissor(x, y, w, h);
        }
    }

    pub fn clear(&self, c: [f32; 4]) {
        unsafe {
            self.gl.clear_color(c[0], c[1], c[2], c[3]);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }
    }

    pub fn finish_target(&self) {
        unsafe {
            self.gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, None, 0);
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.disable(glow::SCISSOR_TEST);
        }
    }

    pub fn blend(&self, mode: Blend) {
        unsafe {
            let gl = &self.gl;
            match mode {
                Blend::None => gl.disable(glow::BLEND),
                Blend::Straight => {
                    gl.enable(glow::BLEND);
                    gl.blend_func_separate(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA, glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                }
                Blend::Premultiplied => {
                    gl.enable(glow::BLEND);
                    gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                }
            }
        }
    }

    /// Draw `src_uv` (u0, v0, u1, v1) of a texture into the target pixel rect `dst`
    /// (target size tw x th). `flip_y` maps the source's v0 to the top of the target as
    /// displayed by OpenXR (last memory row).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_tex(&self, tex: glow::Texture, external: bool, src_uv: [f32; 4], dst: PxRect, tw: u32, th: u32, flip_y: bool) {
        unsafe {
            let gl = &self.gl;
            let p = if external { &self.oes } else { &self.blit };
            gl.use_program(Some(p.prog));
            let x0 = dst.x / tw as f32 * 2.0 - 1.0;
            let x1 = (dst.x + dst.w) / tw as f32 * 2.0 - 1.0;
            let (y0, y1) = if flip_y {
                (1.0 - dst.y / th as f32 * 2.0, 1.0 - (dst.y + dst.h) / th as f32 * 2.0)
            } else {
                (dst.y / th as f32 * 2.0 - 1.0, (dst.y + dst.h) / th as f32 * 2.0 - 1.0)
            };
            gl.uniform_4_f32(p.loc("u_dst"), x0, y0, x1, y1);
            gl.uniform_4_f32(p.loc("u_src"), src_uv[0], src_uv[1], src_uv[2], src_uv[3]);
            gl.uniform_1_i32(p.loc("u_tex"), 0);
            gl.active_texture(glow::TEXTURE0);
            if external {
                gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(tex));
            } else {
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            }
            gl.bind_vertex_array(Some(self.vao_empty));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.bind_vertex_array(None);
        }
    }

    /// Draw triangles (premultiplied colors) with an MVP.
    pub fn draw_triangles(&mut self, mvp: &Mat4, verts: &[Vertex]) {
        if verts.is_empty() {
            return;
        }
        unsafe {
            let gl = &self.gl;
            gl.use_program(Some(self.color.prog));
            gl.uniform_matrix_4_f32_slice(self.color.loc("u_mvp"), false, &mvp.to_cols_array());
            gl.bind_vertex_array(Some(self.vao_color));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            let bytes = std::slice::from_raw_parts(verts.as_ptr() as *const u8, std::mem::size_of_val(verts));
            if bytes.len() > self.vbo_cap {
                self.vbo_cap = bytes.len().next_power_of_two();
                gl.buffer_data_size(glow::ARRAY_BUFFER, self.vbo_cap as i32, glow::DYNAMIC_DRAW);
            }
            gl.buffer_sub_data_u8_slice(glow::ARRAY_BUFFER, 0, bytes);
            gl.draw_arrays(glow::TRIANGLES, 0, verts.len() as i32);
            gl.bind_vertex_array(None);
        }
    }

    /// Environment background: solid premultiplied color, or an equirect texture.
    pub fn draw_env(&self, inv_vp_rot: &Mat4, tex: Option<glow::Texture>, color: [f32; 4]) {
        unsafe {
            let gl = &self.gl;
            gl.use_program(Some(self.env.prog));
            gl.uniform_matrix_4_f32_slice(self.env.loc("u_inv_vp"), false, &inv_vp_rot.to_cols_array());
            gl.uniform_1_i32(self.env.loc("u_mode"), if tex.is_some() { 1 } else { 0 });
            gl.uniform_4_f32(self.env.loc("u_color"), color[0], color[1], color[2], color[3]);
            gl.uniform_1_i32(self.env.loc("u_tex"), 0);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, tex);
            gl.bind_vertex_array(Some(self.vao_empty));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_vertex_array(None);
        }
    }
}

#[derive(Clone, Copy)]
pub enum Blend {
    None,
    Straight,
    Premultiplied,
}

/// Projection matrix from OpenXR field-of-view tangents (GL clip space).
pub fn projection(fov: &openxr::Fovf, near: f32, far: f32) -> Mat4 {
    let l = fov.angle_left.tan();
    let r = fov.angle_right.tan();
    let u = fov.angle_up.tan();
    let d = fov.angle_down.tan();
    let w = r - l;
    let h = u - d;
    Mat4::from_cols_array(&[
        2.0 / w,
        0.0,
        0.0,
        0.0,
        0.0,
        2.0 / h,
        0.0,
        0.0,
        (r + l) / w,
        (u + d) / h,
        -(far + near) / (far - near),
        -1.0,
        0.0,
        0.0,
        -(2.0 * far * near) / (far - near),
        0.0,
    ])
}
