//! Geometry is sampled once, independently of palette. Material variants only
//! colorize that basis. A single worker pre-renders and compresses the atlases.
use super::*;
use crate::screen::ScreenInstruction;
use base64::{engine::general_purpose::STANDARD, Engine};
use flate2::{write::ZlibEncoder, Compression};
use std::{io::Write, sync::mpsc};
use zellij_utils::channels::SenderWithContext;

#[derive(Debug)]
pub(super) struct EncodedImage {
    pub image: DecodedImage,
    pub compressed: String,
}

impl EncodedImage {
    fn new(image: DecodedImage) -> Option<Self> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&image.bytes).ok()?;
        let compressed = STANDARD.encode(encoder.finish().ok()?);
        Some(Self { image, compressed })
    }
}

#[derive(Debug)]
pub(super) struct Prepared {
    pub atlas: EncodedImage,
    pub strips: Vec<EncodedImage>,
}

#[derive(Debug, Default)]
pub(super) struct Rasterizer {
    // Sixteen-bit premultiplied coefficients: background, surface, highlight,
    // alpha. Keep more precision than the final eight-bit RGBA transport.
    basis: HashMap<TileKey, Vec<[u16; 4]>>,
    bytes: usize,
}

impl Rasterizer {
    pub fn prepare(&mut self, key: TileKey) -> Option<Prepared> {
        let geometry = TileKey {
            background: [255, 0, 0],
            surface: [0, 255, 0],
            foreground: [0, 0, 255],
            ..key
        };
        let span = key.tile_cells();
        let width = key.width * span * 8;
        let height = key.height * span;
        if !self.basis.contains_key(&geometry) {
            let bytes = width * height * 8;
            if self.bytes + bytes > MAX_BYTES {
                return None;
            }
            self.basis.insert(geometry, basis(geometry));
            self.bytes += bytes;
        }
        let highlight: [u8; 3] =
            std::array::from_fn(|c| key.foreground[c].max(key.background[c]).max(key.surface[c]));
        let mut bytes = Vec::with_capacity(width * height * 4);
        for weights in &self.basis[&geometry] {
            let alpha = (f64::from(weights[3]) * 255.0 / 65535.0).round() as u8;
            if alpha == 0 {
                bytes.extend_from_slice(&[0; 4]);
                continue;
            }
            for channel in 0..3 {
                let value = f64::from(weights[0]) * f64::from(key.background[channel])
                    + f64::from(weights[1]) * f64::from(key.surface[channel])
                    + f64::from(weights[2]) * f64::from(highlight[channel]);
                bytes.push((value / f64::from(weights[3])).round().clamp(0.0, 255.0) as u8);
            }
            bytes.push(alpha);
        }
        let strips = strips::prepare(key, &bytes)
            .into_iter()
            .map(EncodedImage::new)
            .collect::<Option<Vec<_>>>()?;
        let atlas = EncodedImage::new(DecodedImage {
            bytes,
            width: width as u32,
            height: height as u32,
            format: KittyFormat::Rgba32,
        })?;
        Some(Prepared { atlas, strips })
    }
}

fn basis(key: TileKey) -> Vec<[u16; 4]> {
    let span = key.tile_cells();
    let stride = key.width * span;
    let width = stride * 8;
    let mut pixels = Vec::with_capacity(width * key.height * span);
    for y in 0..key.height * span {
        for tile in 0..8 {
            for x in 0..stride {
                let reused = if matches!(tile, 1 | 6) && x > 0 {
                    Some(pixels[y * width + tile * stride])
                } else if matches!(tile, 3 | 4) && y > 0 {
                    Some(pixels[tile * stride + x])
                } else {
                    None
                };
                if let Some(pixel) = reused {
                    pixels.push(pixel);
                    continue;
                }
                let mut sum = [0.0; 4];
                for sy in 0..4 {
                    for sx in 0..4 {
                        let px = x as f64 + (sx as f64 + 0.5) / 4.0;
                        let py = y as f64 + (sy as f64 + 0.5) / 4.0;
                        let sample = if key.raised {
                            raised::sample(key, tile, px, py)
                        } else {
                            depth::sample(key, tile, px, py)
                        };
                        for (sum, value) in sum.iter_mut().zip(sample) {
                            *sum += value;
                        }
                    }
                }
                let weights = [sum[0] - sum[2], sum[1] - sum[2], sum[2], sum[3]];
                pixels.push(weights.map(|weight| {
                    (weight / (16.0 * 255.0) * 65535.0)
                        .round()
                        .clamp(0.0, 65535.0) as u16
                }));
            }
        }
    }
    pixels
}

#[derive(Debug)]
pub(super) struct Worker {
    jobs: mpsc::Sender<TileKey>,
    results: mpsc::Receiver<(TileKey, Option<Prepared>)>,
}

impl Worker {
    pub fn start(notify: SenderWithContext<ScreenInstruction>) -> Option<Self> {
        let (jobs, requests) = mpsc::channel();
        let (complete, results) = mpsc::channel();
        std::thread::Builder::new()
            .name("surface-raster".into())
            .spawn(move || {
                let mut rasterizer = Rasterizer::default();
                while let Ok(key) = requests.recv() {
                    let prepared = rasterizer.prepare(key);
                    if complete.send((key, prepared)).is_err() {
                        break;
                    }
                    if notify.send(ScreenInstruction::Render).is_err() {
                        break;
                    }
                }
            })
            .ok()?;
        Some(Self { jobs, results })
    }

    pub fn queue(&self, key: TileKey) -> bool {
        self.jobs.send(key).is_ok()
    }

    pub fn completed(&self) -> Vec<(TileKey, Option<Prepared>)> {
        self.results.try_iter().collect()
    }
}
