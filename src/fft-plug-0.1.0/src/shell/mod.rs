use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gtk::prelude::*;
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use fft_plug::pipeline::Pipeline;

use crate::audio::AudioStatus;
use crate::render::{GlRenderer, RenderMode};

const LAYER_NAMESPACE: &str = "fft-plug";
const DEFAULT_HEIGHT: i32 = 320;
const SLEEP_WAKE_POLL: Duration = Duration::from_millis(80);
const DEBUG_INTERVAL: Duration = Duration::from_secs(1);

/// Formats an optional millisecond measurement as `123` or `none` without allocating.
struct Milliseconds(Option<u64>);

impl fmt::Display for Milliseconds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(value) => write!(f, "{value}"),
            None => f.write_str("none"),
        }
    }
}

#[derive(Debug)]
pub enum ShellError {
    LayerShellUnavailable,
    Application(String),
}

impl fmt::Display for ShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LayerShellUnavailable => {
                f.write_str("wlr-layer-shell is unavailable in the current Wayland session")
            }
            Self::Application(error) => write!(f, "GTK application error: {error}"),
        }
    }
}

impl std::error::Error for ShellError {}

pub fn run(
    mode: RenderMode,
    pipeline: Arc<Pipeline>,
    audio_status: Arc<AudioStatus>,
    debug: bool,
) -> Result<(), ShellError> {
    let application = gtk::Application::builder()
        .application_id("io.github.fft_plug")
        .build();
    let startup_error = Rc::new(RefCell::new(None));
    install_unix_signal_handlers(&application);

    {
        let startup_error = Rc::clone(&startup_error);
        let pipeline = Arc::clone(&pipeline);
        let audio_status = Arc::clone(&audio_status);
        application.connect_activate(move |application| {
            let Some(display) = gtk::gdk::Display::default() else {
                *startup_error.borrow_mut() = Some(
                    "GTK could not open a display; start fft-plug inside a Wayland session"
                        .to_string(),
                );
                application.quit();
                return;
            };
            if !display.backend().is_wayland() {
                *startup_error.borrow_mut() = Some(
                    "wlr-layer-shell requires the Wayland GDK backend; set GDK_BACKEND=wayland"
                        .to_string(),
                );
                application.quit();
                return;
            }
            if !gtk4_layer_shell::is_supported() {
                *startup_error.borrow_mut() = Some(ShellError::LayerShellUnavailable.to_string());
                application.quit();
                return;
            }
            build_window(
                application,
                mode,
                Arc::clone(&pipeline),
                Arc::clone(&audio_status),
                debug,
                Rc::clone(&startup_error),
            );
        });
    }

    // Clap 已经消费了程序参数，GTK 不应再次解析诸如 `--mode` 的业务选项。
    let exit_status = application.run_with_args(&["fft-plug"]);
    if let Some(error) = startup_error.borrow_mut().take() {
        return Err(ShellError::Application(error));
    }
    if exit_status != gtk::glib::ExitCode::SUCCESS {
        return Err(ShellError::Application(format!(
            "application exited with status {exit_status:?}"
        )));
    }
    Ok(())
}

fn install_unix_signal_handlers(application: &gtk::Application) {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let weak_application = application.downgrade();
        glib_unix::unix_signal_add_local(signal, move || {
            if let Some(application) = weak_application.upgrade() {
                application.quit();
            }
            gtk::glib::ControlFlow::Break
        });
    }
}

fn build_window(
    application: &gtk::Application,
    mode: RenderMode,
    pipeline: Arc<Pipeline>,
    audio_status: Arc<AudioStatus>,
    debug: bool,
    startup_error: Rc<RefCell<Option<String>>>,
) {
    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .decorated(false)
        // 左右 anchor 同时启用时，0 表示把宽度交给 layer-shell/compositor 协商。
        .default_width(0)
        .default_height(DEFAULT_HEIGHT)
        .build();
    window.add_css_class("fft-plug");
    window.set_focusable(false);
    window.set_can_target(false);

    window.init_layer_shell();
    window.set_namespace(Some(LAYER_NAMESPACE));
    window.set_layer(Layer::Bottom);
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Right, true);
    window.set_anchor(Edge::Bottom, true);
    window.set_exclusive_zone(0);
    window.set_keyboard_mode(KeyboardMode::None);

    install_transparent_css();

    let area = gtk::GLArea::new();
    area.set_auto_render(false);
    area.set_required_version(3, 3);
    area.set_has_depth_buffer(false);
    area.set_has_stencil_buffer(false);
    area.set_hexpand(true);
    area.set_vexpand(true);
    area.set_can_target(false);

    let renderer = Rc::new(RefCell::new(GlRenderer::new()));
    let render_requested = Rc::new(Cell::new(false));
    let tick_callback = Rc::new(RefCell::new(None));
    {
        let renderer = Rc::clone(&renderer);
        let startup_error = Rc::clone(&startup_error);
        let weak_application = application.downgrade();
        area.connect_realize(move |area| {
            let mut renderer = renderer.borrow_mut();
            if let Err(error) = renderer.realize(area) {
                *startup_error.borrow_mut() = Some(error);
                if let Some(application) = weak_application.upgrade() {
                    application.quit();
                }
                area.queue_render();
            }
        });
    }
    {
        let renderer = Rc::clone(&renderer);
        area.connect_unrealize(move |area| renderer.borrow_mut().unrealize(area));
    }
    {
        let renderer = Rc::clone(&renderer);
        let pipeline = Arc::clone(&pipeline);
        let render_requested = Rc::clone(&render_requested);
        area.connect_render(move |area, _context| {
            let keep_rendering = renderer.borrow_mut().render(area, &pipeline, mode);
            render_requested.set(keep_rendering);
            gtk::glib::Propagation::Stop
        });
    }

    // GtkWidget tick callback 与 frame clock 对齐；睡眠且淡出完成后移除 callback，避免
    // 空闲时继续随显示器刷新唤醒 GTK 主线程。
    {
        ensure_tick_callback(
            &area,
            Rc::clone(&render_requested),
            Rc::clone(&tick_callback),
        );
    }

    window.set_child(Some(&area));
    {
        let weak_area = area.downgrade();
        let pipeline = Arc::clone(&pipeline);
        let render_requested = Rc::clone(&render_requested);
        let tick_callback = Rc::clone(&tick_callback);
        gtk::glib::timeout_add_local(SLEEP_WAKE_POLL, move || {
            if pipeline.take_wake_request()
                && let Some(area) = weak_area.upgrade()
            {
                render_requested.set(true);
                ensure_tick_callback(
                    &area,
                    Rc::clone(&render_requested),
                    Rc::clone(&tick_callback),
                );
                area.queue_render();
            }
            gtk::glib::ControlFlow::Continue
        });
    }
    {
        let weak_application = application.downgrade();
        window.connect_close_request(move |_| {
            if let Some(application) = weak_application.upgrade() {
                application.quit();
            }
            gtk::glib::Propagation::Proceed
        });
    }

    if debug {
        let weak_area = area.downgrade();
        let renderer = Rc::clone(&renderer);
        let pipeline = Arc::clone(&pipeline);
        let audio_status = Arc::clone(&audio_status);
        gtk::glib::timeout_add_local(DEBUG_INTERVAL, move || {
            let Some(_area) = weak_area.upgrade() else {
                return gtk::glib::ControlFlow::Break;
            };
            let Ok(renderer) = renderer.try_borrow() else {
                return gtk::glib::ControlFlow::Continue;
            };
            let audio = audio_status.snapshot();
            let pipeline_stats = pipeline.stats();
            let render_stats = renderer.stats();
            let ring = pipeline_stats.ring;
            let overruns = ring.overwritten_blocks.saturating_add(ring.rejected_blocks);
            eprintln!(
                "fft-plug debug: audio.state={:?} rate={} channels={} audio.sessions={} audio.reconnects={} format={} callbacks={} peak={:.3} drops={} unsupported={} pcm.written={} pcm.consumed={} pcm.queued={} pcm.overruns={} dsp.frames={} dsp.last_peak_hz={:.1} dsp.snapshot_age_ms={} activity={:?} render.frames={} render.frame_age_ms={} surface={}x{} logical scale={}",
                audio.state,
                audio.sample_rate,
                audio.channels,
                audio.session_generation,
                audio.reconnects,
                audio.format_code,
                audio.callbacks,
                audio.peak,
                audio.callback_drops,
                audio.unsupported_buffers,
                ring.published_frames,
                ring.consumed_frames,
                ring.queued_blocks,
                overruns,
                pipeline_stats.dsp_frames,
                pipeline_stats.last_peak_hz,
                Milliseconds(pipeline_stats.snapshot_age_ms),
                pipeline_stats.activity,
                render_stats.frames,
                Milliseconds(render_stats.frame_age_ms),
                render_stats.logical_width,
                render_stats.logical_height,
                render_stats.scale_factor,
            );
            gtk::glib::ControlFlow::Continue
        });
    }

    window.present();
    area.queue_render();
}

fn ensure_tick_callback(
    area: &gtk::GLArea,
    render_requested: Rc<Cell<bool>>,
    tick_callback: Rc<RefCell<Option<gtk::TickCallbackId>>>,
) {
    if tick_callback.borrow().is_some() {
        return;
    }

    let callback_slot = Rc::clone(&tick_callback);
    let callback_id = area.add_tick_callback(move |area, _frame_clock| {
        if render_requested.get() {
            area.queue_render();
            gtk::glib::ControlFlow::Continue
        } else {
            // Returning Break removes the GTK callback. Drop the handle from the slot so the
            // wake path can install a fresh callback later.
            callback_slot.borrow_mut().take();
            gtk::glib::ControlFlow::Break
        }
    });
    *tick_callback.borrow_mut() = Some(callback_id);
}

fn install_transparent_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        "window.fft-plug, window.fft-plug contents { background-color: transparent; }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
