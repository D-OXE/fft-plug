use std::ffi::CString;
use std::os::raw::c_void;
use std::ptr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use clap::ValueEnum;
use gtk::prelude::*;
use gtk4 as gtk;

use fft_plug::pipeline::{ActivityState, Pipeline};

mod color;

const FLOATS_PER_VERTEX: usize = 6;
const FADE_EPSILON: f32 = 0.002;
const VISUAL_INTERPOLATION_SECONDS: f32 = 0.008;
const SLEEP_FADE_SECONDS: f32 = 0.12;
const RAINBOW_DECAY_RATE: f32 = 0.35;
const HIGHLIGHT_HEIGHT: f32 = 0.018;
const MAX_EXPECTED_BANDS: usize = 128;
const VERTEX_SHADER: &str = "#version 330 core\nlayout(location = 0) in vec2 a_position;\nlayout(location = 1) in vec4 a_color;\nout vec4 v_color;\nvoid main() {\n    gl_Position = vec4(a_position, 0.0, 1.0);\n    v_color = a_color;\n}\n";
const FRAGMENT_SHADER: &str = "#version 330 core\nin vec4 v_color;\nout vec4 color;\nvoid main() {\n    // Output premultiplied alpha so GtkGLArea content composites correctly over a transparent\n    // window: straight-alpha output from SRC_ALPHA blending would be squared here.\n    color = vec4(v_color.rgb * v_color.a, v_color.a);\n}\n";

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum RenderMode {
    Bars,
    Curve,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderStats {
    pub frames: u64,
    pub logical_width: i32,
    pub logical_height: i32,
    pub scale_factor: i32,
    /// Age of the most recent frame drawn since the last realize; `None` before the first frame
    /// and after `unrealize`, so a stalled renderer is distinguishable from a cold one.
    pub frame_age_ms: Option<u64>,
}

pub struct GlRenderer {
    program: u32,
    vao: u32,
    vbo: u32,
    vertex_count: i32,
    vertex_data: Vec<f32>,
    displayed_levels: Vec<f32>,
    previous_levels: Vec<f32>,
    current_levels: Vec<f32>,
    snapshot_age: f32,
    last_frame: Option<Instant>,
    last_snapshot: u64,
    last_activity: ActivityState,
    lifecycle_alpha: f32,
    rainbow_phase: f32,
    ready: bool,
    render_frames: u64,
    logical_width: i32,
    logical_height: i32,
    scale_factor: i32,
}

impl GlRenderer {
    pub fn new() -> Self {
        Self {
            program: 0,
            vao: 0,
            vbo: 0,
            vertex_count: 0,
            vertex_data: Vec::with_capacity((1 + 2 * MAX_EXPECTED_BANDS) * 6 * FLOATS_PER_VERTEX),
            displayed_levels: Vec::with_capacity(64),
            previous_levels: Vec::with_capacity(64),
            current_levels: Vec::with_capacity(64),
            snapshot_age: 0.0,
            last_frame: None,
            last_snapshot: 0,
            last_activity: ActivityState::Sleeping,
            lifecycle_alpha: 0.0,
            rainbow_phase: 0.0,
            ready: false,
            render_frames: 0,
            logical_width: 0,
            logical_height: 0,
            scale_factor: 1,
        }
    }

    pub fn realize(&mut self, area: &gtk::GLArea) -> Result<(), String> {
        area.make_current();
        if area.error().is_some() {
            return Err("GtkGLArea could not create an OpenGL context".to_string());
        }
        gl::load_with(load_gl_symbol);

        let program = create_program()?;
        let mut vao = 0;
        let mut vbo = 0;
        // SAFETY: GTK has made the GLArea context current and the generated handles are owned by
        // this renderer until unrealize. All pointers reference the local scalar values required by GL.
        unsafe {
            gl::GenVertexArrays(1, &mut vao);
            gl::GenBuffers(1, &mut vbo);
            gl::BindVertexArray(vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, vbo);
            gl::BufferData(gl::ARRAY_BUFFER, 0, ptr::null(), gl::DYNAMIC_DRAW);
            let stride = (FLOATS_PER_VERTEX * std::mem::size_of::<f32>()) as i32;
            gl::EnableVertexAttribArray(0);
            gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, stride, ptr::null());
            gl::EnableVertexAttribArray(1);
            gl::VertexAttribPointer(
                1,
                4,
                gl::FLOAT,
                gl::FALSE,
                stride,
                (2 * std::mem::size_of::<f32>()) as *const c_void,
            );
            gl::BindVertexArray(0);
        }
        self.program = program;
        self.vao = vao;
        self.vbo = vbo;
        self.ready = vao != 0 && vbo != 0;
        if !self.ready {
            self.delete_resources();
            return Err("OpenGL vertex array or buffer could not be created".to_string());
        }
        Ok(())
    }

    pub fn unrealize(&mut self, area: &gtk::GLArea) {
        area.make_current();
        self.delete_resources();
        self.last_frame = None;
        self.lifecycle_alpha = 0.0;
        self.snapshot_age = 0.0;
        self.last_activity = ActivityState::Sleeping;
        self.logical_width = 0;
        self.logical_height = 0;
        self.scale_factor = 1;
    }

    pub fn render(&mut self, area: &gtk::GLArea, pipeline: &Pipeline, mode: RenderMode) -> bool {
        if !self.ready {
            return false;
        }

        self.render_frames = self.render_frames.saturating_add(1);
        let now = Instant::now();
        let elapsed = self
            .last_frame
            .replace(now)
            .map(|last| now.saturating_duration_since(last))
            .unwrap_or(Duration::from_millis(16))
            .min(Duration::from_millis(100));
        let activity = pipeline.activity_state();
        self.update_target(pipeline);
        self.advance_levels(elapsed.as_secs_f32(), activity);

        self.logical_width = area.width();
        self.logical_height = area.height();
        self.scale_factor = area.scale_factor();
        let width = self.logical_width.saturating_mul(self.scale_factor);
        let height = self.logical_height.saturating_mul(self.scale_factor);
        if width <= 0 || height <= 0 {
            return activity != ActivityState::Sleeping || !self.fade_is_complete();
        }

        self.vertex_data.clear();
        let lifecycle_alpha = self.lifecycle_alpha;
        match mode {
            RenderMode::Bars => self.build_bars(lifecycle_alpha),
            RenderMode::Curve => self.build_curve(lifecycle_alpha),
        }

        // SAFETY: render is called by GtkGLArea while its context is current. The vertex slice is
        // immutable for the duration of BufferData and its byte length fits in GL's pointer-sized API.
        unsafe {
            gl::Viewport(0, 0, width, height);
            gl::ClearColor(0.0, 0.0, 0.0, 0.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);
            gl::Enable(gl::BLEND);
            // Premultiplied-alpha blend to match the fragment shader output.
            gl::BlendFunc(gl::ONE, gl::ONE_MINUS_SRC_ALPHA);
            gl::UseProgram(self.program);
            gl::BindVertexArray(self.vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, self.vbo);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (self.vertex_data.len() * std::mem::size_of::<f32>()) as isize,
                self.vertex_data.as_ptr().cast(),
                gl::STREAM_DRAW,
            );
            let primitive = match mode {
                RenderMode::Bars => gl::TRIANGLES,
                RenderMode::Curve => gl::LINE_STRIP,
            };
            if self.vertex_count > 0 {
                gl::DrawArrays(primitive, 0, self.vertex_count);
            }
            gl::BindVertexArray(0);
            gl::UseProgram(0);
        }

        activity != ActivityState::Sleeping || !self.fade_is_complete()
    }

    pub fn stats(&self) -> RenderStats {
        RenderStats {
            frames: self.render_frames,
            logical_width: self.logical_width,
            logical_height: self.logical_height,
            scale_factor: self.scale_factor,
            frame_age_ms: self
                .last_frame
                .map(|last| u64::try_from(last.elapsed().as_millis()).unwrap_or(u64::MAX)),
        }
    }

    fn update_target(&mut self, pipeline: &Pipeline) {
        let Some(snapshot) = pipeline.latest_snapshot() else {
            return;
        };
        if snapshot.frame_index == self.last_snapshot {
            return;
        }
        self.last_snapshot = snapshot.frame_index;
        self.displayed_levels.resize(snapshot.bands.len(), 0.0);
        self.previous_levels.resize(snapshot.bands.len(), 0.0);
        self.previous_levels.copy_from_slice(&self.displayed_levels);
        self.current_levels.clear();
        self.current_levels.extend(
            snapshot
                .bands
                .iter()
                .map(|band| finite_level(band.normalized_level)),
        );
        self.snapshot_age = 0.0;
    }

    fn advance_levels(&mut self, elapsed_seconds: f32, activity: ActivityState) {
        let elapsed_seconds = elapsed_seconds.max(0.0);
        let phase_elapsed = match activity {
            ActivityState::Active => elapsed_seconds,
            ActivityState::Decaying => elapsed_seconds * RAINBOW_DECAY_RATE,
            ActivityState::Sleeping => 0.0,
        };
        self.rainbow_phase = color::advance_phase(
            self.rainbow_phase,
            phase_elapsed,
            color::RAINBOW_CYCLE_SECONDS,
        );
        if self.last_activity == ActivityState::Sleeping && activity == ActivityState::Active {
            self.previous_levels.clone_from(&self.displayed_levels);
            self.snapshot_age = 0.0;
        }

        if activity == ActivityState::Sleeping {
            let alpha = response_alpha(elapsed_seconds, SLEEP_FADE_SECONDS);
            for displayed in &mut self.displayed_levels {
                *displayed = (*displayed + (0.0 - *displayed) * alpha).clamp(0.0, 1.0);
            }
        } else {
            self.snapshot_age =
                (self.snapshot_age + elapsed_seconds).min(VISUAL_INTERPOLATION_SECONDS);
            let interpolation = (self.snapshot_age / VISUAL_INTERPOLATION_SECONDS).clamp(0.0, 1.0);
            for (index, displayed) in self.displayed_levels.iter_mut().enumerate() {
                let previous = self.previous_levels.get(index).copied().unwrap_or(0.0);
                let current = self.current_levels.get(index).copied().unwrap_or(0.0);
                *displayed = (previous + (current - previous) * interpolation).clamp(0.0, 1.0);
            }
        }

        let target_alpha = if activity == ActivityState::Active {
            0.92
        } else {
            0.0
        };
        let alpha_time = if target_alpha > self.lifecycle_alpha {
            VISUAL_INTERPOLATION_SECONDS
        } else {
            SLEEP_FADE_SECONDS
        };
        let alpha = response_alpha(elapsed_seconds, alpha_time);
        self.lifecycle_alpha = (self.lifecycle_alpha
            + (target_alpha - self.lifecycle_alpha) * alpha.clamp(0.0, 1.0))
        .clamp(0.0, 1.0);
        self.last_activity = activity;
    }

    fn fade_is_complete(&self) -> bool {
        self.lifecycle_alpha <= FADE_EPSILON
            && self
                .displayed_levels
                .iter()
                .all(|level| *level <= FADE_EPSILON)
    }

    fn build_bars(&mut self, lifecycle_alpha: f32) {
        let count = self.displayed_levels.len();
        if count == 0 {
            self.vertex_count = 0;
            return;
        }
        let lifecycle_alpha = finite_level(lifecycle_alpha);
        let width = 2.0 / count as f32;
        let gap = (width * 0.18).min(0.025);
        for index in 0..self.displayed_levels.len() {
            let level = self.displayed_levels[index];
            let left = -1.0 + index as f32 * width + gap * 0.5;
            let right = -1.0 + (index + 1) as f32 * width - gap * 0.5;
            let bottom = -0.98;
            let top = bottom + 1.92 * finite_level(level);

            let left_rgb = color::rainbow_rgb((left + 1.0) * 0.5, self.rainbow_phase);
            let right_rgb = color::rainbow_rgb((right + 1.0) * 0.5, self.rainbow_phase);
            let left_bottom =
                material_color(left_rgb, 0.72, color::MATERIAL_ALPHA * lifecycle_alpha);
            let right_bottom =
                material_color(right_rgb, 0.72, color::MATERIAL_ALPHA * lifecycle_alpha);
            let left_top = material_color(left_rgb, 1.0, color::MATERIAL_ALPHA * lifecycle_alpha);
            let right_top = material_color(right_rgb, 1.0, color::MATERIAL_ALPHA * lifecycle_alpha);
            self.push_vertex(left, bottom, left_bottom);
            self.push_vertex(right, bottom, right_bottom);
            self.push_vertex(right, top, right_top);
            self.push_vertex(left, bottom, left_bottom);
            self.push_vertex(right, top, right_top);
            self.push_vertex(left, top, left_top);

            if top > bottom {
                let highlight_bottom = (top - HIGHLIGHT_HEIGHT).max(bottom);
                let left_highlight =
                    material_color(left_rgb, 1.0, color::HIGHLIGHT_ALPHA * lifecycle_alpha);
                let right_highlight =
                    material_color(right_rgb, 1.0, color::HIGHLIGHT_ALPHA * lifecycle_alpha);
                self.push_vertex(left, highlight_bottom, left_highlight);
                self.push_vertex(right, highlight_bottom, right_highlight);
                self.push_vertex(right, top, right_highlight);
                self.push_vertex(left, highlight_bottom, left_highlight);
                self.push_vertex(right, top, right_highlight);
                self.push_vertex(left, top, left_highlight);
            }
        }
        self.vertex_count = (self.vertex_data.len() / FLOATS_PER_VERTEX) as i32;
    }

    fn build_curve(&mut self, alpha: f32) {
        let count = self.displayed_levels.len();
        if count == 0 {
            self.vertex_count = 0;
            return;
        }
        for index in 0..count {
            let current = self.displayed_levels[index];
            let previous = if index == 0 {
                current
            } else {
                self.displayed_levels[index - 1]
            };
            let next = self
                .displayed_levels
                .get(index + 1)
                .copied()
                .unwrap_or(current);
            let level = finite_level((previous + 2.0 * current + next) * 0.25);
            let x = if count == 1 {
                0.0
            } else {
                -1.0 + 2.0 * index as f32 / (count - 1) as f32
            };
            let y = -0.98 + 1.92 * level;
            let color = [0.15 + 0.8 * level, 0.72 - 0.35 * level, 0.98, alpha];
            self.push_vertex(x, y, color);
        }
        self.vertex_count = (self.vertex_data.len() / FLOATS_PER_VERTEX) as i32;
    }

    fn push_vertex(&mut self, x: f32, y: f32, color: [f32; 4]) {
        self.vertex_data.extend_from_slice(&[
            finite_coordinate(x),
            finite_coordinate(y),
            finite_level(color[0]),
            finite_level(color[1]),
            finite_level(color[2]),
            finite_level(color[3]),
        ]);
    }

    fn delete_resources(&mut self) {
        // SAFETY: delete calls are made only while the GLArea context is current; zero handles are
        // skipped because they are the sentinel used before realization.
        unsafe {
            if self.vbo != 0 {
                gl::DeleteBuffers(1, &self.vbo);
            }
            if self.vao != 0 {
                gl::DeleteVertexArrays(1, &self.vao);
            }
            if self.program != 0 {
                gl::DeleteProgram(self.program);
            }
        }
        self.program = 0;
        self.vao = 0;
        self.vbo = 0;
        self.vertex_count = 0;
        self.ready = false;
    }
}

impl Default for GlRenderer {
    fn default() -> Self {
        Self::new()
    }
}

fn create_program() -> Result<u32, String> {
    let vertex = compile_shader(gl::VERTEX_SHADER, VERTEX_SHADER)
        .map_err(|error| format!("vertex shader compilation failed: {error}"))?;
    let fragment = match compile_shader(gl::FRAGMENT_SHADER, FRAGMENT_SHADER) {
        Ok(fragment) => fragment,
        Err(error) => {
            // SAFETY: vertex is a valid shader handle in the current context.
            unsafe { gl::DeleteShader(vertex) };
            return Err(format!("fragment shader compilation failed: {error}"));
        }
    };
    // SAFETY: shader handles came from successful compilation and are valid in the current context.
    let program = unsafe { gl::CreateProgram() };
    if program == 0 {
        // SAFETY: both handles are owned locally and can be released in the current context.
        unsafe {
            gl::DeleteShader(vertex);
            gl::DeleteShader(fragment);
        }
        return Err("OpenGL program object could not be created".to_string());
    }
    // SAFETY: all handles are valid and the program is current-context state.
    unsafe {
        gl::AttachShader(program, vertex);
        gl::AttachShader(program, fragment);
        gl::LinkProgram(program);
        gl::DeleteShader(vertex);
        gl::DeleteShader(fragment);
    }
    let mut linked = 0;
    // SAFETY: linked is a valid output pointer for the program query.
    unsafe { gl::GetProgramiv(program, gl::LINK_STATUS, &mut linked) };
    if linked == 0 {
        let log = program_info_log(program);
        // SAFETY: program is a valid handle in the current context.
        unsafe { gl::DeleteProgram(program) };
        Err(format!("program link failed: {log}"))
    } else {
        Ok(program)
    }
}

fn compile_shader(kind: u32, source: &str) -> Result<u32, String> {
    let source = CString::new(source).map_err(|error| error.to_string())?;
    // SAFETY: shader creation and source pointers are valid for the current GL context and call.
    let shader = unsafe { gl::CreateShader(kind) };
    if shader == 0 {
        return Err("shader object could not be created".to_string());
    }
    let source_pointer = source.as_ptr();
    // SAFETY: source_pointer points to a NUL-terminated string that remains alive through the call.
    unsafe {
        gl::ShaderSource(shader, 1, &source_pointer, ptr::null());
        gl::CompileShader(shader);
    }
    let mut compiled = 0;
    // SAFETY: compiled is a valid output pointer for the shader query.
    unsafe { gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut compiled) };
    if compiled == 0 {
        let log = shader_info_log(shader);
        // SAFETY: shader is a valid handle in the current context.
        unsafe { gl::DeleteShader(shader) };
        Err(log)
    } else {
        Ok(shader)
    }
}

fn shader_info_log(shader: u32) -> String {
    let mut length = 0;
    // SAFETY: shader is a valid handle and length is a valid output pointer.
    unsafe { gl::GetShaderiv(shader, gl::INFO_LOG_LENGTH, &mut length) };
    let Ok(length) = usize::try_from(length) else {
        return "invalid shader info-log length".to_string();
    };
    if length == 0 {
        return "no shader info log available".to_string();
    }
    let mut buffer = vec![0_u8; length];
    // SAFETY: buffer is writable for the requested length and shader is a valid handle.
    unsafe {
        gl::GetShaderInfoLog(
            shader,
            i32::try_from(buffer.len()).unwrap_or(i32::MAX),
            ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
        )
    };
    trim_gl_log(&buffer)
}

fn program_info_log(program: u32) -> String {
    let mut length = 0;
    // SAFETY: program is a valid handle and length is a valid output pointer.
    unsafe { gl::GetProgramiv(program, gl::INFO_LOG_LENGTH, &mut length) };
    let Ok(length) = usize::try_from(length) else {
        return "invalid program info-log length".to_string();
    };
    if length == 0 {
        return "no program info log available".to_string();
    }
    let mut buffer = vec![0_u8; length];
    // SAFETY: buffer is writable for the requested length and program is a valid handle.
    unsafe {
        gl::GetProgramInfoLog(
            program,
            i32::try_from(buffer.len()).unwrap_or(i32::MAX),
            ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
        )
    };
    trim_gl_log(&buffer)
}

fn trim_gl_log(buffer: &[u8]) -> String {
    let length = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    let message = String::from_utf8_lossy(&buffer[..length])
        .trim()
        .to_string();
    if message.is_empty() {
        "empty OpenGL info log".to_string()
    } else {
        message
    }
}

fn material_color(rgb: [f32; 3], brightness: f32, alpha: f32) -> [f32; 4] {
    [
        finite_level(rgb[0] * brightness),
        finite_level(rgb[1] * brightness),
        finite_level(rgb[2] * brightness),
        finite_level(alpha),
    ]
}

static GL_LIBRARIES: OnceLock<[usize; 2]> = OnceLock::new();

fn load_gl_symbol(name: &str) -> *const c_void {
    let Ok(name) = CString::new(name) else {
        return ptr::null();
    };
    // SAFETY: dlsym reads the NUL-terminated symbol name and returns an opaque function pointer;
    // the returned address is only passed to gl::load_with.
    unsafe {
        let symbol = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr());
        if !symbol.is_null() {
            return symbol.cast_const();
        }
        let libraries = GL_LIBRARIES.get_or_init(|| {
            [
                libc::dlopen(
                    c"libGL.so.1".as_ptr().cast(),
                    libc::RTLD_LAZY | libc::RTLD_LOCAL,
                ) as usize,
                libc::dlopen(
                    c"libGLESv2.so.2".as_ptr().cast(),
                    libc::RTLD_LAZY | libc::RTLD_LOCAL,
                ) as usize,
            ]
        });
        for handle in libraries.iter().copied().filter(|handle| *handle != 0) {
            let symbol = libc::dlsym(handle as *mut c_void, name.as_ptr());
            if !symbol.is_null() {
                return symbol.cast_const();
            }
        }
    }
    ptr::null()
}

fn finite_level(level: f32) -> f32 {
    if level.is_finite() {
        level.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn response_alpha(elapsed_seconds: f32, time_constant: f32) -> f32 {
    if !elapsed_seconds.is_finite() || !time_constant.is_finite() || time_constant <= 0.0 {
        return 1.0;
    }
    (1.0 - (-elapsed_seconds.max(0.0) / time_constant).exp()).clamp(0.0, 1.0)
}

fn finite_coordinate(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn renderer_with_levels(levels: &[f32]) -> GlRenderer {
        let mut renderer = GlRenderer::new();
        renderer.displayed_levels.extend_from_slice(levels);
        renderer.rainbow_phase = 0.25;
        renderer
    }

    #[test]
    fn renderer_stats_reports_none_before_first_frame_and_age_after_frame() {
        let mut renderer = GlRenderer::new();
        assert!(renderer.stats().frame_age_ms.is_none());

        renderer.last_frame = Some(Instant::now());
        let age = renderer.stats().frame_age_ms.expect("age after a frame");
        assert!(age <= 1_000);
    }

    #[test]
    fn bars_keep_vertex_data_finite_and_add_only_expected_geometry() {
        let mut renderer = renderer_with_levels(&[0.0, 0.25, 1.0, f32::NAN]);
        renderer.build_bars(0.92);

        // One body per band and one highlight for each positive band. The pixels between bars
        // stay fully transparent, so gaps remain clear over the wallpaper.
        let expected_vertices = (4 + 2) * 6;
        assert_eq!(renderer.vertex_count as usize, expected_vertices);
        assert_eq!(
            renderer.vertex_data.len(),
            expected_vertices * FLOATS_PER_VERTEX
        );
        for vertex in renderer.vertex_data.chunks_exact(FLOATS_PER_VERTEX) {
            assert!(vertex.iter().all(|value| value.is_finite()));
            assert!((-1.0..=1.0).contains(&vertex[0]));
            assert!((-1.0..=1.0).contains(&vertex[1]));
            assert!(vertex[2..].iter().all(|value| (0.0..=1.0).contains(value)));
        }
    }

    #[test]
    fn bars_keep_material_alpha_separate_from_lifecycle_alpha() {
        let mut active = renderer_with_levels(&[0.5]);
        active.build_bars(1.0);
        let active_body_alpha = active.vertex_data[5];

        let mut fading = renderer_with_levels(&[0.5]);
        fading.build_bars(0.5);
        let fading_body_alpha = fading.vertex_data[5];

        assert!((active_body_alpha - color::MATERIAL_ALPHA).abs() <= f32::EPSILON);
        assert!((fading_body_alpha - active_body_alpha * 0.5).abs() <= f32::EPSILON);
    }

    #[test]
    fn bars_do_not_add_a_full_surface_overlay() {
        let mut renderer = renderer_with_levels(&[0.5]);
        renderer.build_bars(1.0);

        assert!(
            renderer
                .vertex_data
                .chunks_exact(FLOATS_PER_VERTEX)
                .all(|vertex| vertex[0] > -1.0 && vertex[0] < 1.0)
        );
    }

    #[test]
    fn sleeping_freezes_phase_and_decay_moves_more_slowly_than_active() {
        let mut renderer = GlRenderer::new();
        renderer.rainbow_phase = 0.25;
        renderer.advance_levels(1.0, ActivityState::Sleeping);
        assert!((renderer.rainbow_phase - 0.25).abs() <= f32::EPSILON);

        renderer.rainbow_phase = 0.25;
        renderer.advance_levels(1.0, ActivityState::Active);
        let active_delta = renderer.rainbow_phase - 0.25;

        renderer.rainbow_phase = 0.25;
        renderer.advance_levels(1.0, ActivityState::Decaying);
        let decaying_delta = renderer.rainbow_phase - 0.25;

        assert!(active_delta > decaying_delta);
        assert!(decaying_delta > 0.0);
    }
}
