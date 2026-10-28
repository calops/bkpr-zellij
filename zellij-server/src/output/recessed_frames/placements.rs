//! Shared eight-slice geometry in physical pixels. Short rectangles split a
//! cell at the midpoint rather than leaving a straight strip between corners.
use super::*;

pub(super) fn cells(key: TileKey, id: InternalImageId, geom: PaneGeom) -> Vec<KittyImageChunk> {
    let width = geom.cols.as_usize() * key.width;
    let height = geom.rows.as_usize() * key.height;
    let tile_width = key.tile_cells() * key.width;
    let tile_height = key.tile_cells() * key.height;
    let left = tile_width.min(width / 2);
    let right = tile_width.min(width - left);
    let top = tile_height.min(height / 2);
    let bottom = tile_height.min(height - top);
    let mut images = Vec::with_capacity((geom.cols.as_usize() + geom.rows.as_usize()) * 4);
    for (tile, x, y, w, h, source_x, source_y) in [
        (0, 0, 0, left, top, 0, 0),
        (1, left, 0, width - left - right, top, 0, 0),
        (2, width - right, 0, right, top, tile_width - right, 0),
        (3, 0, top, left, height - top - bottom, 0, 0),
        (
            4,
            width - right,
            top,
            right,
            height - top - bottom,
            tile_width - right,
            0,
        ),
        (5, 0, height - bottom, left, bottom, 0, tile_height - bottom),
        (
            6,
            left,
            height - bottom,
            width - left - right,
            bottom,
            0,
            tile_height - bottom,
        ),
        (
            7,
            width - right,
            height - bottom,
            right,
            bottom,
            tile_width - right,
            tile_height - bottom,
        ),
    ] {
        let mut py = y;
        while py < y + h {
            let fragment_height = (key.height - py % key.height).min(y + h - py);
            let mut px = x;
            while px < x + w {
                let fragment_width = (key.width - px % key.width).min(x + w - px);
                images.push(KittyImageChunk {
                    cell_x: geom.x + px / key.width,
                    cell_y: geom.y + py / key.height,
                    cell_offset_x: (px % key.width) as u32,
                    cell_offset_y: (py % key.height) as u32,
                    internal_image_id: id,
                    source_px_x: tile * tile_width
                        + source_x
                        + if matches!(tile, 1 | 6) { 0 } else { px - x },
                    source_px_y: source_y + if matches!(tile, 3 | 4) { 0 } else { py - y },
                    source_px_width: fragment_width,
                    source_px_height: fragment_height,
                    z_index: FRAME_Z_INDEX,
                    dest_cells: (0, 0),
                    scaled_px: None,
                    placement_uid: FRAME_UID + images.len() as u64,
                });
                px += fragment_width;
            }
            py += fragment_height;
        }
    }
    images
}
