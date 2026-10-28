//! Repeated straight-edge pixels are copied from the once-rasterized atlas.
//! Long cached strips let many neighboring cells share one native placement.
use super::*;

const STRIP_PIXELS: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub(super) struct Strips {
    ids: [InternalImageId; 4], // top, bottom, left, right
}

impl Strips {
    pub(super) fn release(self, store: &mut KittyImageStore) {
        for id in self.ids {
            store.remove_placement_ref(id);
        }
    }
}

pub(super) fn bytes(key: TileKey) -> usize {
    STRIP_PIXELS * key.tile_cells() * (key.height + key.width) * 2 * 4
}

pub(super) fn prepare(key: TileKey, raster: &[u8]) -> Vec<DecodedImage> {
    let span = key.tile_cells();
    let atlas_width = key.width * span * 8;
    let mut images = Vec::with_capacity(4);
    for tile in [1, 6, 3, 4] {
        let horizontal = matches!(tile, 1 | 6);
        let width = if horizontal {
            STRIP_PIXELS
        } else {
            key.width * span
        };
        let height = if horizontal {
            key.height * span
        } else {
            STRIP_PIXELS
        };
        let mut bytes = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                let source_x = tile * key.width * span + if horizontal { 0 } else { x };
                let source_y = if horizontal { y } else { 0 };
                let offset = (source_y * atlas_width + source_x) * 4;
                bytes.extend_from_slice(&raster[offset..offset + 4]);
            }
        }
        images.push(DecodedImage {
            bytes,
            width: width as u32,
            height: height as u32,
            format: KittyFormat::Rgba32,
        });
    }
    images
}

pub(super) fn install(
    images: Vec<super::raster::EncodedImage>,
    store: &mut KittyImageStore,
) -> Option<Strips> {
    let mut ids = Vec::with_capacity(4);
    for image in images {
        let result = store.store_native_image(image.image, image.compressed);
        let Ok(id) = result else {
            for id in ids {
                store.remove_placement_ref(id);
            }
            return None;
        };
        store.add_placement_ref(id);
        ids.push(id);
    }
    Some(Strips {
        ids: ids.try_into().ok()?,
    })
}

pub(super) fn coalesce(
    mut images: Vec<KittyImageChunk>,
    key: TileKey,
    strips: Strips,
) -> Vec<KittyImageChunk> {
    let tile_width = key.width * key.tile_cells();
    for image in &mut images {
        let tile = image.source_px_x / tile_width;
        let side = match tile {
            1 => 0,
            6 => 1,
            3 => 2,
            4 => 3,
            _ => continue,
        };
        image.internal_image_id = strips.ids[side];
        image.source_px_x = if side < 2 {
            0
        } else {
            image.source_px_x % tile_width
        };
        if side >= 2 {
            image.source_px_y = 0;
        }
    }
    let repeated: Vec<_> = strips
        .ids
        .iter()
        .enumerate()
        .map(|(side, id)| (*id, side < 2))
        .collect();
    images = merge(images, key, &repeated);
    // Region-local IDs stay stable when the pane width changes.
    let mut sequences = [0u64; 8];
    for image in &mut images {
        let region = strips
            .ids
            .iter()
            .position(|id| *id == image.internal_image_id)
            .map(|side| side + 4)
            .unwrap_or_else(|| match image.source_px_x / tile_width {
                0 => 0,
                2 => 1,
                5 => 2,
                _ => 3,
            });
        image.placement_uid = FRAME_UID | ((region as u64) << 48) | sequences[region];
        sequences[region] += 1;
    }
    images
}

pub(super) fn merge(
    mut images: Vec<KittyImageChunk>,
    key: TileKey,
    repeated: &[(InternalImageId, bool)],
) -> Vec<KittyImageChunk> {
    for horizontal in [true, false] {
        images.sort_by_key(|image| {
            let (x, y) = position(*image, key);
            if horizontal {
                (
                    image.internal_image_id,
                    y,
                    image.source_px_y,
                    image.source_px_height,
                    x,
                )
            } else {
                (
                    image.internal_image_id,
                    x,
                    image.source_px_x,
                    image.source_px_width,
                    y,
                )
            }
        });
        let mut merged: Vec<KittyImageChunk> = Vec::with_capacity(images.len());
        for image in images {
            if let Some(previous) = merged.last_mut() {
                let (px, py) = position(*previous, key);
                let (x, y) = position(image, key);
                let repeated = repeated
                    .iter()
                    .any(|(id, axis)| *id == image.internal_image_id && *axis == horizontal);
                let compatible = previous.internal_image_id == image.internal_image_id
                    && if horizontal {
                        py == y
                            && px + previous.source_px_width == x
                            && previous.source_px_y == image.source_px_y
                            && previous.source_px_height == image.source_px_height
                            && if repeated {
                                previous.source_px_x
                                    + previous.source_px_width
                                    + image.source_px_width
                                    <= STRIP_PIXELS
                            } else {
                                previous.source_px_x + previous.source_px_width == image.source_px_x
                            }
                    } else {
                        px == x
                            && py + previous.source_px_height == y
                            && previous.source_px_x == image.source_px_x
                            && previous.source_px_width == image.source_px_width
                            && if repeated {
                                previous.source_px_y
                                    + previous.source_px_height
                                    + image.source_px_height
                                    <= STRIP_PIXELS
                            } else {
                                previous.source_px_y + previous.source_px_height
                                    == image.source_px_y
                            }
                    };
                if compatible {
                    if horizontal {
                        previous.source_px_width += image.source_px_width;
                    } else {
                        previous.source_px_height += image.source_px_height;
                    }
                    continue;
                }
            }
            merged.push(image);
        }
        images = merged;
    }
    images
}

fn position(image: KittyImageChunk, key: TileKey) -> (usize, usize) {
    (
        image.cell_x * key.width + image.cell_offset_x as usize,
        image.cell_y * key.height + image.cell_offset_y as usize,
    )
}
