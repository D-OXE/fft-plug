//! Pure colour and phase helpers for the renderer.
//!
//! These functions deliberately do not depend on GTK, OpenGL, PipeWire, or wall-clock state. The
//! renderer supplies the elapsed frame time and uses the returned bounded values when building its
//! vertex buffer.

pub const RAINBOW_CYCLE_SECONDS: f32 = 30.0;
pub const RAINBOW_SATURATION: f32 = 0.72;
pub const RAINBOW_LIGHTNESS: f32 = 0.62;
pub const MATERIAL_ALPHA: f32 = 0.45;
pub const HIGHLIGHT_ALPHA: f32 = 0.68;

/// Advance a normalized phase by a real elapsed duration and keep it in `[0, 1)`.
pub fn advance_phase(phase: f32, elapsed_seconds: f32, cycle_seconds: f32) -> f32 {
    let phase = wrap_phase(phase);
    if !elapsed_seconds.is_finite() || elapsed_seconds <= 0.0 {
        return phase;
    }
    let cycle_seconds = if cycle_seconds.is_finite() && cycle_seconds > 0.0 {
        cycle_seconds
    } else {
        RAINBOW_CYCLE_SECONDS
    };
    wrap_phase(phase + elapsed_seconds.rem_euclid(cycle_seconds) / cycle_seconds)
}

/// Return a normalized phase in `[0, 1)`, including for non-finite input.
pub fn wrap_phase(value: f32) -> f32 {
    if value.is_finite() {
        value.rem_euclid(1.0)
    } else {
        0.0
    }
}

/// Map a normalized horizontal position and phase to a hue in `[0, 1)`.
pub fn hue_at(normalized_x: f32, phase: f32) -> f32 {
    let normalized_x = if normalized_x.is_finite() {
        normalized_x
    } else {
        0.0
    };
    wrap_phase(normalized_x + wrap_phase(phase))
}

/// Generate the default constrained rainbow colour for a normalized horizontal position.
pub fn rainbow_rgb(normalized_x: f32, phase: f32) -> [f32; 3] {
    hue_to_rgb(
        hue_at(normalized_x, phase),
        RAINBOW_SATURATION,
        RAINBOW_LIGHTNESS,
    )
}

/// Convert HSL to finite, clamped RGB values.
pub fn hue_to_rgb(hue: f32, saturation: f32, lightness: f32) -> [f32; 3] {
    let hue = wrap_phase(hue);
    let saturation = unit(saturation);
    let lightness = unit(lightness);
    if saturation <= f32::EPSILON {
        return [lightness; 3];
    }

    let q = if lightness < 0.5 {
        lightness * (1.0 + saturation)
    } else {
        lightness + saturation - lightness * saturation
    };
    let p = 2.0 * lightness - q;
    [
        hsl_channel(p, q, hue + 1.0 / 3.0),
        hsl_channel(p, q, hue),
        hsl_channel(p, q, hue - 1.0 / 3.0),
    ]
}

fn hsl_channel(p: f32, q: f32, value: f32) -> f32 {
    let value = wrap_phase(value);
    let channel = if value < 1.0 / 6.0 {
        p + (q - p) * 6.0 * value
    } else if value < 0.5 {
        q
    } else if value < 2.0 / 3.0 {
        p + (q - p) * (2.0 / 3.0 - value) * 6.0
    } else {
        p
    };
    unit(channel)
}

fn unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(left: f32, right: f32, tolerance: f32) {
        assert!(
            (left - right).abs() <= tolerance,
            "{left} and {right} differ by more than {tolerance}"
        );
    }

    #[test]
    fn phase_is_equivalent_when_split_across_refresh_rates() {
        let start = 0.37;
        let whole = advance_phase(start, RAINBOW_CYCLE_SECONDS, RAINBOW_CYCLE_SECONDS);
        for frame_count in [60, 120, 160] {
            let frame_seconds = RAINBOW_CYCLE_SECONDS / frame_count as f32;
            let split = (0..frame_count).fold(start, |phase, _| {
                advance_phase(phase, frame_seconds, RAINBOW_CYCLE_SECONDS)
            });
            assert_close(split, whole, 0.0001);
        }
    }

    #[test]
    fn phase_wraps_and_rejects_non_finite_input() {
        assert_close(wrap_phase(-0.25), 0.75, f32::EPSILON);
        assert_close(wrap_phase(1.25), 0.25, f32::EPSILON);
        assert!(advance_phase(f32::NAN, 1.0, RAINBOW_CYCLE_SECONDS).is_finite());
        assert!(advance_phase(0.2, f32::INFINITY, RAINBOW_CYCLE_SECONDS).is_finite());
        assert!(advance_phase(0.2, 1.0, f32::NAN).is_finite());
    }

    #[test]
    fn hue_wrap_is_continuous_at_horizontal_edges() {
        let phase = 0.73;
        assert_close(hue_at(0.0, phase), hue_at(1.0, phase), f32::EPSILON);
        assert_eq!(hue_at(f32::NAN, phase), hue_at(0.0, phase));
    }

    #[test]
    fn generated_colours_are_finite_and_in_range() {
        for position in [-10.0, -0.5, 0.0, 0.25, 0.5, 1.0, 10.0, f32::NAN] {
            let rgb = rainbow_rgb(position, f32::NAN);
            assert!(rgb.iter().all(|channel| channel.is_finite()));
            assert!(rgb.iter().all(|channel| (0.0..=1.0).contains(channel)));
        }

        let grayscale = hue_to_rgb(0.2, f32::NAN, 0.4);
        assert_eq!(grayscale, [0.4; 3]);
    }
}
