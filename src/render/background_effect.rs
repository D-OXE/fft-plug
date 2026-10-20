//! Per-bar background blur for a Wayland layer surface.
//!
//! The compositor applies the region on the next `wl_surface.commit()`.  This
//! module deliberately does not commit the surface itself: GTK owns that
//! lifecycle for the `GLArea`.

use gtk::prelude::*;
use gtk4 as gtk;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_compositor::WlCompositor, wl_region::WlRegion};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1::ExtBackgroundEffectManagerV1;
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1;

use gdk4_wayland::prelude::WaylandSurfaceExtManual;

const BAR_BOTTOM_NDC: f32 = -0.98;
const BAR_HEIGHT_NDC: f32 = 1.92;
const BLUR_LEVEL_EPSILON: f32 = 0.002;

/// An integer rectangle in surface-local coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BlurRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Default)]
struct WaylandState;

// The registry and the background-effect manager only need their events to be
// accepted.  GTK remains responsible for driving the display; this queue is
// used only for proxies created from that same display connection.
impl
    Dispatch<
        wayland_client::protocol::wl_registry::WlRegistry,
        wayland_client::globals::GlobalListContents,
    > for WaylandState
{
    fn event(
        _state: &mut Self,
        _proxy: &wayland_client::protocol::wl_registry::WlRegistry,
        _event: wayland_client::protocol::wl_registry::Event,
        _data: &wayland_client::globals::GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtBackgroundEffectManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtBackgroundEffectManagerV1,
        _event: <ExtBackgroundEffectManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtBackgroundEffectSurfaceV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtBackgroundEffectSurfaceV1,
        _event: <ExtBackgroundEffectSurfaceV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wayland_client::protocol::wl_region::WlRegion, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegion,
        _event: <WlRegion as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

/// A background effect object attached to the `GLArea`'s Wayland surface.
pub struct BackgroundEffect {
    compositor: WlCompositor,
    effect: ExtBackgroundEffectSurfaceV1,
    queue: wayland_client::EventQueue<WaylandState>,
    state: WaylandState,
    rects: Vec<BlurRect>,
}

impl BackgroundEffect {
    /// Attaches the protocol object to `area` when the current display supports it.
    ///
    /// Missing Wayland objects or an unavailable compositor protocol are a
    /// normal fallback, so they return `None` instead of failing startup.
    pub fn new(area: &gtk::GLArea) -> Option<Self> {
        let surface = area.native()?.surface()?;
        let display = surface
            .display()
            .downcast::<gdk4_wayland::WaylandDisplay>()
            .ok()?;
        let wayland_surface = surface.downcast::<gdk4_wayland::WaylandSurface>().ok()?;
        let wl_surface = wayland_surface.wl_surface()?;
        let wl_display = display.wl_display()?;
        let backend = wl_display.backend().upgrade()?;
        let connection = Connection::from_backend(backend);
        let compositor = display.wl_compositor()?;

        let (globals, queue) = registry_queue_init::<WaylandState>(&connection).ok()?;
        let qh = queue.handle();
        let manager = globals
            .bind::<ExtBackgroundEffectManagerV1, _, _>(&qh, 1..=1, ())
            .ok()?;
        let effect = manager.get_background_effect(&wl_surface, &qh, ());

        Some(Self {
            compositor,
            effect,
            queue,
            state: WaylandState,
            rects: Vec::new(),
        })
    }

    /// Updates the compositor blur region to the union of the visible bars.
    ///
    /// `width` and `height` are logical surface dimensions.  The bar geometry
    /// matches the renderer: each bar starts at `BAR_BOTTOM_NDC` and extends
    /// upward according to its finite, clamped level.  A gap between bars is
    /// intentionally left out of the region.
    pub fn update(&mut self, width: i32, height: i32, levels: &[f32], lifecycle_alpha: f32) {
        self.dispatch_pending();
        self.rects.clear();
        fill_blur_rects(&mut self.rects, width, height, levels, lifecycle_alpha);

        if self.rects.is_empty() {
            self.effect.set_blur_region(None);
            return;
        }

        let region = self.compositor.create_region(&self.queue.handle(), ());
        for rect in &self.rects {
            region.add(rect.x, rect.y, rect.width, rect.height);
        }
        self.effect.set_blur_region(Some(&region));
        // set_blur_region has copy semantics, so the temporary region can be
        // destroyed before GTK commits the surface.
        region.destroy();
    }

    /// Removes the pending blur region.
    pub fn clear(&mut self) {
        self.dispatch_pending();
        self.rects.clear();
        self.effect.set_blur_region(None);
    }

    fn dispatch_pending(&mut self) {
        let _ = self.queue.dispatch_pending(&mut self.state);
    }
}

fn fill_blur_rects(
    rects: &mut Vec<BlurRect>,
    width: i32,
    height: i32,
    levels: &[f32],
    lifecycle_alpha: f32,
) {
    if width <= 0
        || height <= 0
        || !lifecycle_alpha.is_finite()
        || lifecycle_alpha <= BLUR_LEVEL_EPSILON
    {
        return;
    }

    let count = levels.len();
    if count == 0 {
        return;
    }

    let width_f = width as f32;
    let height_f = height as f32;
    let band_width = 2.0 / count as f32;
    let gap = (band_width * 0.18).min(0.025);

    for (index, level) in levels.iter().copied().enumerate() {
        let level = finite_level(level);
        if level <= BLUR_LEVEL_EPSILON {
            continue;
        }

        let left = -1.0 + index as f32 * band_width + gap * 0.5;
        let right = -1.0 + (index + 1) as f32 * band_width - gap * 0.5;
        let top = BAR_BOTTOM_NDC + BAR_HEIGHT_NDC * level;

        let left_px = ndc_x_to_surface(left, width_f).floor().clamp(0.0, width_f);
        let right_px = ndc_x_to_surface(right, width_f).ceil().clamp(0.0, width_f);
        let top_px = ndc_y_to_surface_top(top, height_f)
            .floor()
            .clamp(0.0, height_f);
        let bottom_px = ndc_y_to_surface_top(BAR_BOTTOM_NDC, height_f)
            .ceil()
            .clamp(0.0, height_f);

        let Some(x) = rounded_i32(left_px) else {
            continue;
        };
        let Some(right) = rounded_i32(right_px) else {
            continue;
        };
        let Some(y) = rounded_i32(top_px) else {
            continue;
        };
        let Some(bottom) = rounded_i32(bottom_px) else {
            continue;
        };
        let rect_width = right.saturating_sub(x);
        let rect_height = bottom.saturating_sub(y);
        if rect_width > 0 && rect_height > 0 {
            rects.push(BlurRect {
                x,
                y,
                width: rect_width,
                height: rect_height,
            });
        }
    }
}

fn ndc_x_to_surface(ndc: f32, width: f32) -> f32 {
    (ndc + 1.0) * 0.5 * width
}

fn ndc_y_to_surface_top(ndc: f32, height: f32) -> f32 {
    height - (ndc + 1.0) * 0.5 * height
}

fn finite_level(level: f32) -> f32 {
    if level.is_finite() {
        level.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn rounded_i32(value: f32) -> Option<i32> {
    if !value.is_finite() {
        return None;
    }
    if value < i32::MIN as f32 || value > i32::MAX as f32 {
        return None;
    }
    Some(value as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blur_rects(width: i32, height: i32, levels: &[f32], lifecycle_alpha: f32) -> Vec<BlurRect> {
        let mut rects = Vec::new();
        fill_blur_rects(&mut rects, width, height, levels, lifecycle_alpha);
        rects
    }

    #[test]
    fn geometry_keeps_gaps_out_of_the_blur_region() {
        let rects = blur_rects(1000, 320, &[1.0, 1.0], 1.0);

        assert_eq!(rects.len(), 2);
        assert!(rects[0].x + rects[0].width < rects[1].x);
        assert_eq!(rects[0].y, rects[1].y);
        assert_eq!(rects[0].height, rects[1].height);
    }

    #[test]
    fn geometry_uses_surface_top_left_coordinates() {
        let rects = blur_rects(1000, 320, &[1.0], 1.0);

        assert_eq!(
            rects,
            vec![BlurRect {
                x: 6,
                y: 9,
                width: 988,
                height: 308,
            }]
        );
    }

    #[test]
    fn invalid_levels_and_fade_clear_geometry() {
        assert!(blur_rects(1000, 320, &[0.0, f32::NAN, -1.0], 1.0).is_empty());
        assert!(blur_rects(1000, 320, &[1.0], 0.0).is_empty());
        assert!(blur_rects(0, 320, &[1.0], 1.0).is_empty());
    }
}
