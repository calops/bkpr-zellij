//! Border depth in physical pixels, evaluated only while creating a cached atlas.
use super::TileKey;

const SHADOW_OPACITY: f64 = 0.24;
const GLOW_OPACITY: f64 = 0.12;

pub(super) fn sample(key: TileKey, tile: usize, mut px: f64, mut py: f64) -> [f64; 3] {
    let mirror_x = matches!(tile, 2 | 4 | 7);
    let mirror_y = matches!(tile, 5..=7);
    if mirror_x {
        px = (key.width * 2) as f64 - px;
    }
    if mirror_y {
        py = key.height as f64 - py;
    }
    let thickness = key.thickness as f64;
    let radius = key.radius as f64;
    // Positive distance is inside the content opening. Normals point inward.
    // The circle and straight segments share the same distance/normal at joins.
    let (distance, mut nx, mut ny) = match tile {
        1 | 6 => (py - thickness, 0.0, 1.0),
        3 | 4 => (px - thickness, 1.0, 0.0),
        _ => {
            let qx = thickness + radius - px;
            let qy = thickness + radius - py;
            let dx = qx.max(0.0);
            let dy = qy.max(0.0);
            let length = dx.hypot(dy);
            let distance = radius - length - qx.max(qy).min(0.0);
            let (nx, ny) = if length > 0.0 {
                (dx / length, dy / length)
            } else if qx >= qy {
                (1.0, 0.0)
            } else {
                (0.0, 1.0)
            };
            (distance, nx, ny)
        },
    };
    if mirror_x {
        nx = -nx;
    }
    if mirror_y {
        ny = -ny;
    }

    if distance >= 0.0 {
        // A compact smooth falloff reaches exactly zero before meeting PTY
        // cells. No shadow pixels spill onto application backgrounds or text.
        let reach = (key.width - key.thickness).min(key.height - key.thickness) as f64;
        let falloff = (1.0 - distance / reach).clamp(0.0, 1.0).powi(3);
        let direction = 0.5 + 0.5 * ((nx + ny) * std::f64::consts::FRAC_1_SQRT_2).clamp(0.0, 1.0);
        let darkening = SHADOW_OPACITY * falloff * direction;
        key.background
            .map(|channel| f64::from(channel) * (1.0 - darkening))
    } else {
        // The glow lives in the surrounding surface. Its outer edge matches
        // the sidebar/separator color exactly, so adjacent tile seams stay flat.
        let falloff = (1.0 + distance / thickness).clamp(0.0, 1.0).powi(2);
        let glow = if key.focused {
            GLOW_OPACITY * falloff
        } else {
            0.0
        };
        std::array::from_fn(|channel| {
            let surface = f64::from(key.surface[channel]);
            surface + (f64::from(key.foreground[channel]) - surface) * glow
        })
    }
}
