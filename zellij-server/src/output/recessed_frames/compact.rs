//! Spacing prototype. Shared separator cells are split in physical pixels
//! so adjacent frame images never paint over one another.
use super::*;
use zellij_utils::pane_size::Offset;

pub(crate) fn requested() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("BKPR_COMPACT_FRAMES").is_none_or(|v| v != "0"))
}

fn overlaps(a: usize, length: usize, b: usize, other_length: usize) -> bool {
    a < b + other_length && b < a + length
}

pub(crate) fn content_offset(geom: PaneGeom, neighbors: &[PaneGeom]) -> Offset {
    let right = geom.x + geom.cols.as_usize();
    let bottom = geom.y + geom.rows.as_usize();
    let shared_right = neighbors.iter().any(|other| {
        other.x == right && overlaps(geom.y, geom.rows.as_usize(), other.y, other.rows.as_usize())
    });
    let shared_bottom = neighbors.iter().any(|other| {
        other.y == bottom && overlaps(geom.x, geom.cols.as_usize(), other.x, other.cols.as_usize())
    });
    Offset {
        top: 1,
        left: 1,
        right: usize::from(!shared_right),
        bottom: usize::from(!shared_bottom),
    }
}

pub(crate) fn frame_geom(mut geom: PaneGeom, offset: Offset) -> PaneGeom {
    geom.cols.increase_inner(usize::from(offset.right == 0));
    geom.rows.increase_inner(usize::from(offset.bottom == 0));
    geom
}

pub(super) fn clip_shared_cell(
    image: &mut KittyImageChunk,
    geom: PaneGeom,
    neighbors: &[PaneGeom],
    key: TileKey,
) -> bool {
    let x = image.cell_x;
    let y = image.cell_y;
    let right = geom.x + geom.cols.as_usize();
    let bottom = geom.y + geom.rows.as_usize();
    let mut left_crop = 0;
    let mut right_crop = 0;
    let mut top_crop = 0;
    let mut bottom_crop = 0;
    for other in neighbors {
        if y >= other.y && y < other.y + other.rows.as_usize() {
            if x == geom.x && other.x + other.cols.as_usize() == geom.x + 1 {
                left_crop = key.width / 2;
            }
            if x + 1 == right && other.x + 1 == right {
                right_crop = key.width - key.width / 2;
            }
        }
        if x >= other.x && x < other.x + other.cols.as_usize() {
            if y == geom.y && other.y + other.rows.as_usize() == geom.y + 1 {
                top_crop = key.height / 2;
            }
            if y + 1 == bottom && other.y + 1 == bottom {
                bottom_crop = key.height - key.height / 2;
            }
        }
    }
    image.source_px_x += left_crop;
    image.source_px_y += top_crop;
    image.source_px_width -= left_crop + right_crop;
    image.source_px_height -= top_crop + bottom_crop;
    image.cell_offset_x += left_crop as u32;
    image.cell_offset_y += top_crop as u32;
    image.source_px_width > 0 && image.source_px_height > 0
}

impl Output {
    pub(crate) fn supports_compact_frames(&self, clients: impl Iterator<Item = ClientId>) -> bool {
        let Some(cache) = &self.recessed_frames else {
            return false;
        };
        let mut cache = cache.borrow_mut();
        let mut store = self.kitty_image_store.borrow_mut();
        clients.into_iter().all(|client| {
            let Some(key) = cache.clients.get(&client).copied() else {
                return false;
            };
            cache.atlas(key, &mut store).is_some()
        })
    }
}
