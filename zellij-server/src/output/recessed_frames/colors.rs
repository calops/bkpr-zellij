//! Blend hue in linear light while retaining the locked surface's luminance.
fn linear(value: u8) -> f64 {
    let value = f64::from(value) / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn encoded(value: f64) -> u8 {
    let value = if value <= 0.0031308 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    (value * 255.0).round().clamp(0.0, 255.0) as u8
}

fn luminance(rgb: [f64; 3]) -> f64 {
    rgb[0] * 0.2126 + rgb[1] * 0.7152 + rgb[2] * 0.0722
}

// Lift raised details toward the confirmed terminal foreground, preserving
// the palette's hue and its light/dark contrast direction.
pub(super) fn lift(background: [u8; 3], foreground: [u8; 3]) -> [u8; 3] {
    std::array::from_fn(|channel| {
        encoded(linear(background[channel]) * 0.94 + linear(foreground[channel]) * 0.06)
    })
}

pub(super) fn tint(background: [u8; 3], accent: [u8; 3]) -> [u8; 3] {
    let base = background.map(linear);
    let accent = accent.map(linear);
    let light = luminance(base);
    let accent_light = luminance(accent);
    if light == 0.0 || accent_light == 0.0 {
        return background;
    }
    let target = accent.map(|channel| channel * light / accent_light);
    // Both endpoints have the same luminance. Reduce chroma at the gamut edge
    // rather than clipping channels and accidentally changing brightness.
    let mut amount: f64 = 0.40;
    for channel in 0..3 {
        let delta = target[channel] - base[channel];
        if delta > 0.0 {
            amount = amount.min((1.0 - base[channel]) / delta);
        }
    }
    std::array::from_fn(|channel| {
        encoded(base[channel] + amount * (target[channel] - base[channel]))
    })
}
