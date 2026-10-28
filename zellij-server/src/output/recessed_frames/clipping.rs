//! Exact pixel clipping for multi-cell frame strips, including sub-cell offsets.
use super::*;

pub(super) fn visible(
    images: &[KittyImageChunk],
    stack: Option<&FloatingPanesStack>,
    z: Option<usize>,
    key: TileKey,
) -> Vec<KittyImageChunk> {
    let mut images = images.to_vec();
    let Some(stack) = stack else {
        return images;
    };
    for pane in stack.layers.iter().skip(z.unwrap_or(0)) {
        let cover = (
            pane.x * key.width,
            pane.y * key.height,
            (pane.x + pane.cols.as_usize()) * key.width,
            (pane.y + pane.rows.as_usize()) * key.height,
        );
        let mut next = Vec::with_capacity(images.len());
        for image in images {
            let x = image.cell_x * key.width + image.cell_offset_x as usize;
            let y = image.cell_y * key.height + image.cell_offset_y as usize;
            let right = x + image.source_px_width;
            let bottom = y + image.source_px_height;
            let left_cut = x.max(cover.0);
            let top_cut = y.max(cover.1);
            let right_cut = right.min(cover.2);
            let bottom_cut = bottom.min(cover.3);
            if left_cut >= right_cut || top_cut >= bottom_cut {
                next.push(image);
                continue;
            }
            for (left, top, right, bottom) in [
                (x, y, right, top_cut),
                (x, bottom_cut, right, bottom),
                (x, top_cut, left_cut, bottom_cut),
                (right_cut, top_cut, right, bottom_cut),
            ] {
                if left >= right || top >= bottom {
                    continue;
                }
                next.push(KittyImageChunk {
                    cell_x: left / key.width,
                    cell_y: top / key.height,
                    cell_offset_x: (left % key.width) as u32,
                    cell_offset_y: (top % key.height) as u32,
                    source_px_x: image.source_px_x + left - x,
                    source_px_y: image.source_px_y + top - y,
                    source_px_width: right - left,
                    source_px_height: bottom - top,
                    ..image
                });
            }
        }
        images = next;
    }
    images
}
