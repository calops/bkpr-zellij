//! Pixel-aligned rounded frames. Geometry, focus and input remain native.
//!
//! Eight edge/corner tiles share an atlas. Each spans two rows/columns so a
//! translucent inset shadow can extend below content glyphs. The cache
//! owns one store reference per atlas, preventing repeated rasterization. The
//! existing Kitty serializer caches base64 and uploads independently per client.
use super::*;
use crate::panes::kitty_graphics::parser::{DecodedImage, KittyFormat};
use crate::terminal_colors::TerminalColors;

pub(crate) mod compact;
mod depth;

const MAX_BYTES: usize = 16 * 1024 * 1024;
// Grid placement IDs grow from one. Reserve the upper half for native frames.
const FRAME_UID: u64 = 1 << 63;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct TileKey {
    width: usize,
    height: usize,
    background: [u8; 3],
    surface: [u8; 3],
    foreground: [u8; 3],
    thickness: usize,
    radius: usize,
    compact: bool,
}

impl TileKey {
    fn tile_cells(self) -> usize {
        if self.compact {
            3
        } else {
            2
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct FrameCache {
    clients: HashMap<ClientId, TileKey>,
    atlases: HashMap<TileKey, InternalImageId>,
    active_frames: HashMap<(ClientId, PaneId), FrameRequest>,
    host_backgrounds: HashMap<ClientId, zellij_utils::ipc::TerminalBackground>,
    bytes: usize,
}

#[derive(Clone, Debug)]
pub(super) struct FrameRequest {
    geom: PaneGeom,
    z_index: Option<usize>,
    changed: bool,
    atlas: Option<(TileKey, InternalImageId)>,
}

impl FrameCache {
    pub(crate) fn update(&mut self, client_id: ClientId, colors: &TerminalColors) -> bool {
        let previous = self.clients.remove(&client_id);
        let next = colors
            .frame_colors()
            .and_then(|([width, height], foreground, background)| {
                if !(2..=512).contains(&width) || !(2..=512).contains(&height) {
                    return None;
                }
                // Round half a cell to whole pixels once for all four edges.
                // Let the curve fill the remaining corner area without seams.
                let thickness = width.div_ceil(2).min(height - 1);
                let compact = compact::requested();
                let radius = if compact {
                    (width * 3 / 2).min(2 * width).min(2 * height)
                } else {
                    (width * 2 - thickness).min(height - thickness)
                };
                Some(TileKey {
                    width,
                    height,
                    background,
                    surface: surface(foreground, background),
                    foreground,
                    thickness,
                    radius,
                    compact,
                })
            });
        if let Some(key) = next {
            self.clients.insert(client_id, key);
        }
        previous != next
    }

    pub(crate) fn remove_client(&mut self, client_id: ClientId) {
        self.clients.remove(&client_id);
        self.host_backgrounds.remove(&client_id);
        self.active_frames
            .retain(|(client, _), _| *client != client_id);
    }

    fn atlas(&mut self, key: TileKey, store: &mut KittyImageStore) -> Option<InternalImageId> {
        if let Some(id) = self.atlases.get(&key) {
            return Some(*id);
        }
        let tile_cells = key.tile_cells();
        let bytes = key.width * tile_cells * key.height * tile_cells * 8 * 4;
        if self.bytes + bytes > MAX_BYTES {
            return None;
        }
        let id = store
            .store_image(DecodedImage {
                bytes: rasterize(key),
                width: (key.width * tile_cells * 8) as u32,
                height: (key.height * tile_cells) as u32,
                format: KittyFormat::Rgba32,
            })
            .ok()?;
        store.add_placement_ref(id);
        self.atlases.insert(key, id);
        self.bytes += bytes;
        Some(id)
    }
}

// Keep identical to the sidebar's terminal-derived surface (palette.rs).
fn surface(foreground: [u8; 3], background: [u8; 3]) -> [u8; 3] {
    fn luminance(rgb: [u8; 3]) -> f64 {
        let linear = rgb.map(|channel| {
            let value = f64::from(channel) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        });
        linear[0] * 0.2126 + linear[1] * 0.7152 + linear[2] * 0.0722
    }
    let offset = ((luminance(foreground) - luminance(background)) * 255.0 * 0.06).round() as i16;
    background.map(|channel| (i16::from(channel) - offset).clamp(0, 255) as u8)
}

// TL, top, TR, left, right, BL, bottom, BR. Mirror sample coordinates,
// rather than rounding two independent curves on odd-sized cells.
fn rasterize(key: TileKey) -> Vec<u8> {
    let tile_cells = key.tile_cells();
    let stride = key.width * tile_cells;
    let mut rgba = Vec::with_capacity(stride * key.height * tile_cells * 8 * 4);
    for y in 0..key.height * tile_cells {
        for tile in 0..8 {
            for x in 0..stride {
                let mut channels = [0.0; 4];
                for sy in 0..4 {
                    for sx in 0..4 {
                        let px = x as f64 + (sx as f64 + 0.5) / 4.0;
                        let py = y as f64 + (sy as f64 + 0.5) / 4.0;
                        let sample = depth::sample(key, tile, px, py);
                        for (sum, channel) in channels.iter_mut().zip(sample) {
                            *sum += channel;
                        }
                    }
                }
                // Kitty expects straight RGBA, while supersampling blends
                // premultiplied channels to keep translucent shadow edges clean.
                for channel in &channels[..3] {
                    let value = if channels[3] > 0.0 {
                        channel * 255.0 / channels[3]
                    } else {
                        0.0
                    };
                    rgba.push(value.round().clamp(0.0, 255.0) as u8);
                }
                rgba.push((channels[3] / 16.0).round().clamp(0.0, 255.0) as u8);
            }
        }
    }
    rgba
}

impl Output {
    pub(crate) fn terminal_background_updates(
        &self,
    ) -> Vec<(ClientId, Option<zellij_utils::ipc::TerminalBackground>)> {
        let Some(cache) = &self.recessed_frames else {
            return vec![];
        };
        let clients: HashSet<_> = {
            let cache = cache.borrow();
            cache
                .clients
                .keys()
                .chain(cache.host_backgrounds.keys())
                .copied()
                .collect()
        };
        let mut updates = vec![];
        for client in clients {
            let background = self.terminal_background(client);
            let mut cache = cache.borrow_mut();
            if cache.host_backgrounds.get(&client).copied() != background {
                if let Some(background) = background {
                    cache.host_backgrounds.insert(client, background);
                } else {
                    cache.host_backgrounds.remove(&client);
                }
                updates.push((client, background));
            }
        }
        updates
    }

    pub(crate) fn terminal_background(
        &self,
        client_id: ClientId,
    ) -> Option<zellij_utils::ipc::TerminalBackground> {
        let cache = self.recessed_frames.as_ref()?.borrow();
        let key = cache
            .active_frames
            .iter()
            .find_map(|((client, _), frame)| {
                (*client == client_id)
                    .then_some(frame.atlas)
                    .flatten()
                    .map(|(key, _)| key)
            })?;
        Some(zellij_utils::ipc::TerminalBackground {
            original: key.background,
            surface: key.surface,
        })
    }

    fn frame_atlas(
        &self,
        client_id: ClientId,
        geom: PaneGeom,
        enabled: bool,
    ) -> Option<(TileKey, InternalImageId)> {
        let cache = self.recessed_frames.as_ref()?;
        let key = enabled
            .then(|| cache.borrow().clients.get(&client_id).copied())
            .flatten()?;
        if geom.cols.as_usize() < key.tile_cells() * 2
            || geom.rows.as_usize() < key.tile_cells() * 2
        {
            return None;
        }
        let id = cache
            .borrow_mut()
            .atlas(key, &mut self.kitty_image_store.borrow_mut())?;
        Some((key, id))
    }

    pub(crate) fn recessed_frame_region(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        render: impl FnOnce() -> Result<Vec<CharacterChunk>>,
    ) -> Result<Vec<CharacterChunk>> {
        let request = self.recessed_frames.as_ref().and_then(|cache| {
            cache
                .borrow()
                .active_frames
                .get(&(client_id, pane_id))
                .cloned()
        });
        let mut chunks = render()?;
        if let Some(mut request) = request {
            if let Some((key, _)) = request.atlas {
                decorate_characters(&mut chunks, key);
            }
            request.changed = true;
            self.recessed_frame_requests
                .insert((client_id, pane_id), request);
        }
        Ok(chunks)
    }

    pub(crate) fn recessed_pane_frame(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        geom: PaneGeom,
        enabled: bool,
        z_index: Option<usize>,
        mut characters: Option<&mut Vec<CharacterChunk>>,
    ) {
        let Some(cache) = self.recessed_frames.clone() else {
            return;
        };
        let atlas = self.frame_atlas(client_id, geom, enabled);

        if let (Some((key, _)), Some(chunks)) = (atlas, characters.as_mut()) {
            decorate_characters(chunks, key);
        }
        let changed = characters.is_some()
            || self
                .recessed_frame_requests
                .get(&(client_id, pane_id))
                .is_some_and(|request| request.changed);
        let request = FrameRequest {
            geom,
            z_index,
            changed,
            atlas,
        };
        if atlas.is_some() {
            cache
                .borrow_mut()
                .active_frames
                .insert((client_id, pane_id), request.clone());
        } else {
            cache
                .borrow_mut()
                .active_frames
                .remove(&(client_id, pane_id));
        }
        self.recessed_frame_requests
            .insert((client_id, pane_id), request);
    }

    // Frame and content render order differs between terminal and plugin panes.
    // Compose only after both streams have finished, never from a frame hook.
    pub(super) fn finish_recessed_frames(&mut self) {
        if let Some(cache) = &self.recessed_frames {
            cache
                .borrow_mut()
                .active_frames
                .retain(|(client_id, pane_id), _| {
                    self.client_kitty_visible_panes
                        .get(client_id)
                        .is_some_and(|panes| panes.contains(pane_id))
                });
        }
        for ((client_id, pane_id), request) in std::mem::take(&mut self.recessed_frame_requests) {
            self.finish_recessed_frame(client_id, pane_id, request);
        }
    }

    fn finish_recessed_frame(
        &mut self,
        client_id: ClientId,
        pane_id: PaneId,
        request: FrameRequest,
    ) {
        let content_rendered = self
            .client_rendered_kitty_panes
            .get(&client_id)
            .is_some_and(|panes| panes.contains(&pane_id));
        if !content_rendered && !request.changed {
            return;
        }
        // A frame-only redraw must not remove unchanged application images.
        let mut images: Vec<KittyImageChunk> = if content_rendered {
            vec![]
        } else {
            self.kitty_host_state
                .borrow()
                .get(&client_id)
                .map(|state| {
                    state
                        .live_placements
                        .iter()
                        .filter(|(key, _)| key.pane_id == pane_id && key.placement_uid < FRAME_UID)
                        .map(|(_, record)| record.geometry)
                        .collect()
                })
                .unwrap_or_default()
        };
        if let Some((key, id)) = request.atlas {
            let geom = request.geom;
            let width = geom.cols.as_usize();
            let height = geom.rows.as_usize();
            let tile_cells = key.tile_cells();
            let neighbors: Vec<_> = if key.compact && request.z_index.is_none() {
                self.recessed_frames
                    .as_ref()
                    .map(|cache| {
                        cache
                            .borrow()
                            .active_frames
                            .iter()
                            .filter(|((client, pane), frame)| {
                                *client == client_id && *pane != pane_id && frame.z_index.is_none()
                            })
                            .map(|(_, frame)| frame.geom)
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                vec![]
            };
            let mut frame_images = Vec::with_capacity((width + height) * 4);
            let horizontal = (0..width).flat_map(|x| {
                (0..tile_cells).flat_map(move |y| [(x, y), (x, height - tile_cells + y)])
            });
            let vertical = (tile_cells..height - tile_cells).flat_map(|y| {
                (0..tile_cells).flat_map(move |x| [(x, y), (width - tile_cells + x, y)])
            });
            for (x, y) in horizontal.chain(vertical) {
                let (tile, column, row) = match (x, y) {
                    (_, _) if y < tile_cells && x < tile_cells => (0, x, y),
                    (_, _) if y < tile_cells && x + tile_cells >= width => {
                        (2, x + tile_cells - width, y)
                    },
                    (_, _) if y < tile_cells => (1, 0, y),
                    (_, _) if y + tile_cells >= height && x < tile_cells => {
                        (5, x, y + tile_cells - height)
                    },
                    (_, _) if y + tile_cells >= height && x + tile_cells >= width => {
                        (7, x + tile_cells - width, y + tile_cells - height)
                    },
                    (_, _) if y + tile_cells >= height => (6, 0, y + tile_cells - height),
                    (_, _) if x < tile_cells => (3, x, 0),
                    (_, _) if x + tile_cells >= width => (4, x + tile_cells - width, 0),
                    _ => continue,
                };
                let mut image = KittyImageChunk {
                    cell_x: geom.x + x,
                    cell_y: geom.y + y,
                    internal_image_id: id,
                    source_px_x: (tile * tile_cells + column) * key.width,
                    source_px_y: row * key.height,
                    source_px_width: key.width,
                    source_px_height: key.height,
                    cell_offset_x: 0,
                    cell_offset_y: 0,
                    z_index: -1,
                    dest_cells: (0, 0),
                    scaled_px: None,
                    placement_uid: FRAME_UID + (y * width + x) as u64,
                };
                if !key.compact || compact::clip_shared_cell(&mut image, geom, &neighbors, key) {
                    frame_images.push(image);
                }
            }
            if let Some(stack) = &self.floating_panes_stack {
                frame_images = stack.visible_kitty_image_chunks(
                    frame_images,
                    request.z_index,
                    &SizeInPixels {
                        width: key.width,
                        height: key.height,
                    },
                );
            }
            images.extend(frame_images);
        }
        self.client_rendered_kitty_panes
            .entry(client_id)
            .or_default()
            .insert(pane_id);
        self.client_kitty_chunks
            .entry(client_id)
            .or_default()
            .entry(pane_id)
            .or_default()
            .extend(images);
    }
}

fn decorate_characters(chunks: &mut [CharacterChunk], key: TileKey) {
    let blank = TerminalCharacter::new_styled(
        ' ',
        DEFAULT_STYLES
            .background(Some(AnsiCode::RgbCode(key.background.into())))
            .into(),
    );
    for chunk in chunks {
        // Labels live in the sidebar. Clear complete physical columns, including
        // wide titles and hover hints, so all edges stay uninterrupted surfaces.
        chunk.terminal_characters = vec![blank.clone(); chunk.width()];
    }
}
