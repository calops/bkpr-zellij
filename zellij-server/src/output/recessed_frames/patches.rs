//! Local contour tiles, independent of complete card dimensions or position.
//! Straight edges become long repeated strips; only nearby corner geometry is
//! part of a tile's cache key. Both layers share the same mask and blur work.
use super::contour::{distance, Feature, Point, Rect};
use super::*;

const STRIP: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Geometry {
    Line {
        horizontal: bool,
        inward: i32,
        phase: i32,
    },
    Tile(Vec<Feature>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Key {
    pub material: TileKey,
    pub geometry: Geometry,
}

impl Key {
    pub fn dimensions(&self) -> (usize, usize) {
        match self.geometry {
            Geometry::Tile(_) => (self.material.width, self.material.height),
            Geometry::Line { horizontal, .. } => {
                let unit = if horizontal {
                    self.material.height
                } else {
                    self.material.width
                };
                let normal = (2 * reach(self.material)).div_ceil(unit) * unit + unit;
                if horizontal {
                    (STRIP, normal)
                } else {
                    (normal, STRIP)
                }
            },
        }
    }
    pub fn bytes(&self) -> usize {
        let (w, h) = self.dimensions();
        w * h * 8
    }
}

pub(super) fn reach(key: TileKey) -> usize {
    ((key.width as f64 * 0.30).max(0.75) * 5.0 + key.width as f64 * 0.35 + 2.0).ceil() as usize
}

#[derive(Debug)]
pub(super) struct Prepared {
    pub body: raster::EncodedImage,
    pub shadow: raster::EncodedImage,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Images {
    pub body: InternalImageId,
    pub shadow: InternalImageId,
}

#[derive(Default, Debug)]
pub(super) struct Rasterizer {
    basis: HashMap<Key, (Vec<[u16; 4]>, Vec<[u16; 4]>, usize, usize)>,
    bytes: usize,
}

impl Rasterizer {
    pub fn prepare(&mut self, key: &Key) -> Option<Prepared> {
        let mut canonical = key.clone();
        canonical.material.background = [255, 0, 0];
        canonical.material.surface = [0, 255, 0];
        canonical.material.foreground = [0, 0, 255];
        let (w, h) = key.dimensions();
        if !self.basis.contains_key(&canonical) {
            if self.bytes + w * h * 16 > MAX_BYTES {
                return None;
            }
            let basis = sample(&canonical);
            let size = basis.0.len() * 16;
            self.basis.insert(canonical.clone(), basis);
            self.bytes += size;
        }
        let (body, shadow, sw, sh) = &self.basis[&canonical];
        let encode = |basis: &[[u16; 4]]| {
            let small = raster::colorize(key.material, basis);
            let bytes = if *sw == w && *sh == h {
                small
            } else if *sw == 1 {
                let mut bytes = Vec::with_capacity(w * h * 4);
                for pixel in small.chunks_exact(4) {
                    for _ in 0..w {
                        bytes.extend_from_slice(pixel);
                    }
                }
                bytes
            } else {
                small.repeat(h)
            };
            raster::EncodedImage::new(DecodedImage {
                bytes,
                width: w as u32,
                height: h as u32,
                format: KittyFormat::Rgba32,
            })
        };
        Some(Prepared {
            body: encode(body)?,
            shadow: encode(shadow)?,
        })
    }
}

fn sample(key: &Key) -> (Vec<[u16; 4]>, Vec<[u16; 4]>, usize, usize) {
    let material = key.material;
    let (w, h) = key.dimensions();
    let (sw, sh, features) = match &key.geometry {
        Geometry::Tile(features) => (w, h, features.clone()),
        Geometry::Line {
            horizontal,
            inward,
            phase,
        } => {
            let unit = if *horizontal {
                material.height
            } else {
                material.width
            };
            let edge = (reach(material).div_ceil(unit) * unit) as i32 + phase;
            let edge_feature = if *horizontal {
                Feature::Edge {
                    from: Point {
                        x: -100000,
                        y: edge,
                    },
                    to: Point { x: 100000, y: edge },
                    nx: 0,
                    ny: *inward,
                }
            } else {
                Feature::Edge {
                    from: Point {
                        x: edge,
                        y: -100000,
                    },
                    to: Point { x: edge, y: 100000 },
                    nx: *inward,
                    ny: 0,
                }
            };
            if *horizontal {
                (1, h, vec![edge_feature])
            } else {
                (w, 1, vec![edge_feature])
            }
        },
    };
    let pad = reach(material);
    let pw = sw + 2 * pad;
    let ph = sh + 2 * pad;
    let mut mask = vec![0.0; pw * ph];
    for y in 0..ph {
        for x in 0..pw {
            let mut inside = 0.0;
            for sy in 0..4 {
                for sx in 0..4 {
                    if distance(
                        &features,
                        x as f64 - pad as f64 + (sx as f64 + 0.5) / 4.0,
                        y as f64 - pad as f64 + (sy as f64 + 0.5) / 4.0,
                    )
                    .0 >= 0.0
                    {
                        inside += 1.0;
                    }
                }
            }
            mask[y * pw + x] = inside / 16.0;
        }
    }
    let sigma = (material.width as f64 * 0.30).max(0.75);
    let support = (sigma * 5.0).ceil() as isize;
    let mut kernel: Vec<f64> = (-support..=support)
        .map(|i| (-0.5 * (i as f64 / sigma).powi(2)).exp())
        .collect();
    let total: f64 = kernel.iter().sum();
    for weight in &mut kernel {
        *weight /= total;
    }
    let mut horizontal = vec![0.0; mask.len()];
    let mut blurred = vec![0.0; mask.len()];
    for y in 0..ph {
        for x in 0..pw {
            for (i, weight) in kernel.iter().enumerate() {
                let px = (x as isize + i as isize - support).clamp(0, pw as isize - 1) as usize;
                horizontal[y * pw + x] += mask[y * pw + px] * weight;
            }
        }
    }
    for y in 0..ph {
        for x in 0..pw {
            for (i, weight) in kernel.iter().enumerate() {
                let py = (y as isize + i as isize - support).clamp(0, ph as isize - 1) as usize;
                blurred[y * pw + x] += horizontal[py * pw + x] * weight;
            }
        }
    }
    let mut body = Vec::with_capacity(sw * sh);
    let mut shadow = Vec::with_capacity(sw * sh);
    for y in 0..sh {
        for x in 0..sw {
            let px = if sw == 1 { 0 } else { x };
            let py = if sh == 1 { 0 } else { y };
            let cx = (px / material.width * material.width) as f64;
            let cy = (py / material.height * material.height) as f64;
            let partial = [cx, cx + material.width as f64].into_iter().any(|x| {
                [cy, cy + material.height as f64]
                    .into_iter()
                    .any(|y| distance(&features, x, y).0 < 0.0)
            });
            let mut rgba = [0.0; 4];
            for sy in 0..4 {
                for sx in 0..4 {
                    let (d, nx, ny) = distance(
                        &features,
                        px as f64 + (sx as f64 + 0.5) / 4.0,
                        py as f64 + (sy as f64 + 0.5) / 4.0,
                    );
                    if d >= 0.0 && (partial || d < depth::bevel_width(material)) {
                        let rgb = depth::bevel_normal(material, d, nx, ny);
                        for c in 0..3 {
                            rgba[c] += rgb[c];
                        }
                        rgba[3] += 255.0;
                    }
                }
            }
            let weights = [rgba[0] - rgba[2], rgba[1] - rgba[2], rgba[2], rgba[3]];
            body.push(
                weights.map(|v| (v / (16.0 * 255.0) * 65535.0).round().clamp(0.0, 65535.0) as u16),
            );
            let shift = material.width as f64 * 0.35;
            let fx = px as f64 + pad as f64 - shift;
            let fy = py as f64 + pad as f64 - shift;
            let ix = fx.floor() as usize;
            let iy = fy.floor() as usize;
            let dx = fx - ix as f64;
            let dy = fy - iy as f64;
            let a = blurred[iy * pw + ix] * (1.0 - dx) + blurred[iy * pw + ix + 1] * dx;
            let b = blurred[(iy + 1) * pw + ix] * (1.0 - dx) + blurred[(iy + 1) * pw + ix + 1] * dx;
            let alpha = ((a * (1.0 - dy) + b * dy)
                * (1.0 - mask[(py + pad) * pw + px + pad])
                * 0.45
                * 65535.0)
                .round()
                .clamp(0.0, 65535.0) as u16;
            shadow.push([0, 0, 0, alpha]);
        }
    }
    (body, shadow, sw, sh)
}

#[derive(Clone, Debug)]
pub(super) struct Cell {
    pub key: Key,
    pub x: usize,
    pub y: usize,
    pub source_x: usize,
    pub source_y: usize,
}

pub(super) fn cells(contour: &contour::Contour, material: TileKey) -> Vec<Cell> {
    let pad = reach(material) as i32;
    let w = material.width as i32;
    let h = material.height as i32;
    let bounds = contour.bounds;
    let left = (bounds.x - pad).max(0) / w;
    let top = (bounds.y - pad).max(0) / h;
    let right = (bounds.right + pad + w - 1) / w;
    let bottom = (bounds.bottom + pad + h - 1) / h;
    let mut cells = vec![];
    for y in top..bottom {
        for x in left..right {
            let origin = Point { x: x * w, y: y * h };
            let clip = Rect {
                x: origin.x - pad - 1,
                y: origin.y - pad - 1,
                right: origin.x + w + pad + 1,
                bottom: origin.y + h + pad + 1,
            };
            let mut features: Vec<_> = contour
                .features
                .iter()
                .filter(|feature| feature.bounds().intersects(clip))
                .map(|feature| feature.local(origin, clip))
                .collect();
            if features.is_empty() {
                continue;
            }
            features.sort();
            let line = if let [Feature::Edge { from, to, nx, ny }] = &features[..] {
                if from.y == to.y && from.x.min(to.x) <= -pad && from.x.max(to.x) >= w + pad {
                    Some((true, *ny, from.y))
                } else if from.x == to.x && from.y.min(to.y) <= -pad && from.y.max(to.y) >= h + pad
                {
                    Some((false, *nx, from.x))
                } else {
                    None
                }
            } else {
                None
            };
            let (mut source_x, mut source_y) = (0, 0);
            let geometry = if let Some((horizontal, inward, edge)) = line {
                let unit = if horizontal { h } else { w };
                let phase = edge.rem_euclid(unit);
                let center =
                    reach(material).div_ceil(unit as usize) * unit as usize + phase as usize;
                let source = center as i32 - edge;
                if source < 0 {
                    continue;
                }
                if horizontal {
                    source_y = source as usize;
                } else {
                    source_x = source as usize;
                }
                Geometry::Line {
                    horizontal,
                    inward,
                    phase,
                }
            } else {
                Geometry::Tile(features)
            };
            let key = Key { material, geometry };
            let (iw, ih) = key.dimensions();
            if source_x + material.width > iw || source_y + material.height > ih {
                continue;
            }
            cells.push(Cell {
                key,
                x: x as usize,
                y: y as usize,
                source_x,
                source_y,
            });
        }
    }
    cells
}
