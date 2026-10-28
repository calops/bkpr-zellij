//! A single raised material spanning an item and its passive details pane.
use super::*;
use crate::panes::surface::SurfaceAttachment;

#[derive(Clone, Debug)]
pub(super) struct Plan {
    layout: Rc<Layout>,
    pub attachment: SurfaceAttachment,
    pub images: Rc<Vec<KittyImageChunk>>,
}

#[derive(Debug)]
pub(super) struct Layout {
    key: TileKey,
    geom: PaneGeom,
    attachment: SurfaceAttachment,
    cells: Vec<patches::Cell>,
    keys: Vec<patches::Key>,
    full_rows: Vec<(usize, usize)>,
    first_row: usize,
    images: std::cell::OnceCell<Rc<Vec<KittyImageChunk>>>,
}

impl Plan {
    pub fn prepare(
        cache: &mut FrameCache,
        store: &mut KittyImageStore,
        key: TileKey,
        geom: PaneGeom,
        attachment: &SurfaceAttachment,
    ) -> Option<Self> {
        let cached = cache.joined_layouts.iter().position(|layout| {
            layout.key == key && layout.geom == geom && &layout.attachment == attachment
        });
        let layout = if let Some(index) = cached {
            let layout = cache.joined_layouts.remove(index)?;
            cache.joined_layouts.push_back(layout.clone());
            layout
        } else {
            let w = key.width as i32;
            let h = key.height as i32;
            let pad = key.width.div_ceil(2).min(key.height) as i32;
            let margin_x = w - pad;
            // Both rectangles share the same vertical padding so aligned text
            // rows have aligned edges. Keep it clear of neighboring glyphs.
            let vertical_pad = key.width.div_ceil(4).min(key.height / 8) as i32;
            let radius = key.radius as i32;
            let popup = contour::Rect {
                x: geom.x as i32 * w + margin_x,
                y: geom.y as i32 * h + h - vertical_pad,
                right: (geom.x + geom.cols.as_usize()) as i32 * w - margin_x,
                bottom: (geom.y + geom.rows.as_usize()) as i32 * h - h + vertical_pad,
            };
            if popup.right - popup.x < 2 * radius || popup.bottom - popup.y < 2 * radius {
                return None;
            }
            let mut item = contour::Rect {
                x: attachment.x as i32 * w + margin_x,
                y: (attachment.y as i32 * h - vertical_pad).max(0),
                right: (attachment.x + attachment.columns) as i32 * w - margin_x,
                bottom: (attachment.y + attachment.rows) as i32 * h + vertical_pad,
            };
            if attachment.x + attachment.columns <= geom.x + 1 {
                item.right = popup.x + radius;
            } else if geom.x + geom.cols.as_usize() <= attachment.x + 1 {
                item.x = popup.right - radius;
            } else {
                return None;
            }
            let contour = contour::Contour::new([popup, item], radius)?;
            // Cache cell coverage with the contour, rather than evaluating all
            // of its arcs for every character on every native render.
            let first_row = popup.y.min(item.y).max(0) as usize / key.height;
            let last_row = popup.bottom.max(item.bottom).max(0) as usize / key.height;
            let left = popup.x.min(item.x).max(0) as usize / key.width;
            let right = popup.right.max(item.right).max(0) as usize / key.width;
            let full_rows = (first_row..=last_row)
                .map(|y| {
                    let full = |x: usize| {
                        contour.full_cell(contour::Rect {
                            x: (x * key.width) as i32,
                            y: (y * key.height) as i32,
                            right: ((x + 1) * key.width) as i32,
                            bottom: ((y + 1) * key.height) as i32,
                        })
                    };
                    let start = (left..right).find(|x| full(*x)).unwrap_or(right);
                    let end = (start..right)
                        .rev()
                        .find(|x| full(*x))
                        .map_or(start, |x| x + 1);
                    (start, end)
                })
                .collect();
            let cells = patches::cells(&contour, key);
            let mut unique = HashSet::new();
            let keys = cells
                .iter()
                .filter_map(|cell| unique.insert(cell.key.clone()).then_some(cell.key.clone()))
                .collect();
            let layout = Rc::new(Layout {
                key,
                geom,
                attachment: attachment.clone(),
                cells,
                keys,
                full_rows,
                first_row,
                images: std::cell::OnceCell::new(),
            });
            cache.joined_layouts.push_back(layout.clone());
            while cache.joined_layouts.len() > 16 {
                cache.joined_layouts.pop_front();
            }
            layout
        };
        if let Some(images) = layout.images.get() {
            return Some(Self {
                images: images.clone(),
                attachment: attachment.clone(),
                layout,
            });
        }
        cache.poll(store);
        let mut pending = false;
        for key in &layout.keys {
            if cache.patch(key, store).is_none() {
                pending = true;
            }
        }
        if pending {
            return None;
        }
        let mut parts = vec![];
        let mut repeated = vec![];
        for cell in &layout.cells {
            let images = cache.patch_images[&cell.key];
            if let patches::Geometry::Line { horizontal, .. } = cell.key.geometry {
                repeated.push((images.body, horizontal));
                repeated.push((images.shadow, horizontal));
            }
            for (image, z) in [
                (
                    images.body,
                    if (cell.x < geom.x || cell.x >= geom.x + geom.cols.as_usize())
                        && !guide_cell(attachment, cell.x, cell.y)
                    {
                        -1
                    } else {
                        FRAME_Z_INDEX
                    },
                ),
                (images.shadow, FRAME_Z_INDEX),
            ] {
                parts.push(KittyImageChunk {
                    cell_x: cell.x,
                    cell_y: cell.y,
                    internal_image_id: image,
                    source_px_x: cell.source_x,
                    source_px_y: cell.source_y,
                    source_px_width: key.width,
                    source_px_height: key.height,
                    cell_offset_x: 0,
                    cell_offset_y: 0,
                    z_index: z,
                    dest_cells: (0, 0),
                    scaled_px: None,
                    placement_uid: 0,
                });
            }
        }
        repeated.sort();
        repeated.dedup();
        let mut images = vec![];
        // Do not merge the below-text item surface with above-text edge masks.
        for (index, z) in [-1, FRAME_Z_INDEX].into_iter().enumerate() {
            let layer = parts
                .iter()
                .filter(|image| image.z_index == z)
                .copied()
                .collect();
            for (sequence, mut image) in
                strips::merge(layer, key, &repeated).into_iter().enumerate()
            {
                image.placement_uid = FRAME_UID | ((index as u64 + 1) << 60) | sequence as u64;
                images.push(image);
            }
        }
        let images = Rc::new(images);
        let _ = layout.images.set(images.clone());
        Some(Self {
            layout,
            attachment: attachment.clone(),
            images,
        })
    }

    pub fn full_cell(&self, x: usize, y: usize) -> bool {
        y.checked_sub(self.layout.first_row)
            .and_then(|row| self.layout.full_rows.get(row))
            .is_some_and(|&(left, right)| x >= left && x < right)
    }

    pub fn guide_cell(&self, x: usize, y: usize) -> bool {
        guide_cell(&self.attachment, x, y)
    }

    pub fn contains_item_cell(&self, x: usize, y: usize) -> bool {
        x >= self.attachment.x
            && x < self.attachment.x + self.attachment.columns
            && y >= self.attachment.y
            && y < self.attachment.y + self.attachment.rows
    }
}

fn guide_cell(attachment: &SurfaceAttachment, x: usize, y: usize) -> bool {
    x >= attachment.x
        && y.checked_sub(attachment.guide_y)
            .and_then(|row| attachment.guide_columns.get(row))
            .is_some_and(|columns| x - attachment.x < *columns)
}
