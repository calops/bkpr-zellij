//! Border depth in physical pixels, evaluated only while creating a cached atlas.
use super::TileKey;

const SHADOW_OPACITY: f64 = 0.72;
const BEVEL_HIGHLIGHT: f64 = 0.24;
const BEVEL_SHADE: f64 = 0.24;

fn local_position(key: TileKey, tile: usize, mut px: f64, mut py: f64) -> (f64, f64) {
    if matches!(tile, 2 | 4 | 7) {
        px = (key.width * 2) as f64 - px;
    }
    if matches!(tile, 5..=7) {
        py = (key.height * 2) as f64 - py;
    }
    (px, py)
}

fn opening_distance(key: TileKey, tile: usize, px: f64, py: f64) -> f64 {
    let (px, py) = local_position(key, tile, px, py);
    let thickness = key.thickness as f64;
    let radius = key.radius as f64;
    // Positive distance is inside the opening. This is the actual circular mask,
    // shared by the visible edge and the shifted shadow caster.
    match tile {
        1 | 6 => py - thickness,
        3 | 4 => px - thickness,
        _ => {
            let qx = thickness + radius - px;
            let qy = thickness + radius - py;
            let dx = qx.max(0.0);
            let dy = qy.max(0.0);
            let length = dx.hypot(dy);
            radius - length - qx.max(qy).min(0.0)
        },
    }
}

fn cast_shadow(key: TileKey, tile: usize, px: f64, py: f64) -> f64 {
    // Offset the surrounding surface's mask down/right, then blur that mask.
    // Translating in screen coordinates before mirroring makes the direction
    // consistent across all eight tiles; this is not a uniform edge gradient.
    // Equal physical-pixel offsets on both axes, independent of cell aspect ratio.
    let offset = key.width as f64 * 0.35;
    let x = px - offset;
    let y = py - offset;
    let sigma = (key.width as f64 * 0.30).max(0.75);
    let support = sigma * 5.0;
    let distance = opening_distance(key, tile, x, y);
    if distance >= support {
        return 0.0;
    }
    if distance <= -support {
        return SHADOW_OPACITY;
    }
    let (x, y) = local_position(key, tile, x, y);
    let thickness = key.thickness as f64;
    let outside = match tile {
        1 | 6 => normal_cdf((thickness - y) / sigma),
        3 | 4 => normal_cdf((thickness - x) / sigma),
        _ => {
            // Integrate the Gaussian over the actual rounded opening. The
            // horizontal integral is analytic; only the curved boundary needs
            // quadrature. Sparse point samples of a binary mask leave terraces
            // as their spacing grows with the font size.
            let low = thickness.max(y - support);
            let high = y + support;
            if high <= low {
                return SHADOW_OPACITY;
            }
            let step = (high - low) / 64.0;
            let radius = key.radius as f64;
            let center = thickness + radius;
            let mut inside = 0.0;
            for index in 0..64 {
                let sample_y = low + (index as f64 + 0.5) * step;
                let dy = (center - sample_y).max(0.0);
                let boundary_x = center - (radius * radius - dy * dy).max(0.0).sqrt();
                let z = (sample_y - y) / sigma;
                let density = (-0.5 * z * z).exp() / (sigma * (2.0 * std::f64::consts::PI).sqrt());
                inside += density * normal_cdf((x - boundary_x) / sigma) * step;
            }
            1.0 - inside
        },
    };
    SHADOW_OPACITY * outside.clamp(0.0, 1.0)
}

// Standard normal CDF, with absolute error below 8e-8. Continuous at every
// physical position; final RGBA quantization happens only after supersampling.
fn normal_cdf(value: f64) -> f64 {
    let x = value.abs();
    let t = 1.0 / (1.0 + 0.2316419 * x);
    let tail = (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt()
        * t
        * (0.319381530
            + t * (-0.356563782 + t * (1.781477937 + t * (-1.821255978 + t * 1.330274429))));
    if value >= 0.0 {
        1.0 - tail
    } else {
        tail
    }
}

fn bevel(key: TileKey, tile: usize, px: f64, py: f64, distance: f64) -> [f64; 3] {
    // A narrow facet on the surrounding lip, not a halo. The recessed upper/
    // left face is shaded; the lower/right face catches the same upper-left
    // light responsible for the cast shadow inside the opening.
    let width = (key.width as f64 * 0.12).clamp(0.75, 2.0);
    let weight = (1.0 + distance / width).clamp(0.0, 1.0);
    if weight == 0.0 {
        return key.surface.map(f64::from);
    }
    // The gradient of the actual circular mask supplies continuous normals
    // through the corners and identical lighting on straight edges.
    let nx =
        opening_distance(key, tile, px + 0.25, py) - opening_distance(key, tile, px - 0.25, py);
    let ny =
        opening_distance(key, tile, px, py + 0.25) - opening_distance(key, tile, px, py - 0.25);
    let length = nx.hypot(ny).max(f64::EPSILON);
    let light = (-(nx + ny) * std::f64::consts::FRAC_1_SQRT_2 / length).clamp(-1.0, 1.0);
    std::array::from_fn(|channel| {
        let surface = f64::from(key.surface[channel]);
        if light > 0.0 {
            let highlight = f64::from(
                key.foreground[channel]
                    .max(key.background[channel])
                    .max(key.surface[channel]),
            );
            surface + (highlight - surface) * BEVEL_HIGHLIGHT * light * weight
        } else {
            surface * (1.0 + BEVEL_SHADE * light * weight)
        }
    })
}

// Premultiplied RGBA. Beyond the reserved frame cells only translucent black
// shadow pixels are emitted, composited below text over the application's bg.
pub(super) fn sample(key: TileKey, tile: usize, px: f64, py: f64) -> [f64; 4] {
    let distance = opening_distance(key, tile, px, py);
    if distance >= 0.0 {
        let shadow = cast_shadow(key, tile, px, py);
        let (x, y) = local_position(key, tile, px, py);
        let reserved = match tile {
            1 | 6 => y < key.height as f64,
            3 | 4 => x < key.width as f64,
            _ => x < key.width as f64 || y < key.height as f64,
        };
        if reserved {
            [
                f64::from(key.background[0]) * (1.0 - shadow),
                f64::from(key.background[1]) * (1.0 - shadow),
                f64::from(key.background[2]) * (1.0 - shadow),
                255.0,
            ]
        } else {
            [0.0, 0.0, 0.0, shadow * 255.0]
        }
    } else {
        let rgb = bevel(key, tile, px, py, distance);
        [rgb[0], rgb[1], rgb[2], 255.0]
    }
}
