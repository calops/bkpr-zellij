//! Pixel-aligned rounded frames. Geometry, focus and input remain native.
//!
//! Eight edge/corner tiles share an atlas. Each covers the full blur support so a
//! translucent inset shadow can extend over content glyphs. The cache
//! owns one store reference per atlas, preventing repeated rasterization. The
//! existing Kitty serializer caches base64 and uploads independently per client.
use super::*;
use crate::panes::kitty_graphics::parser::{DecodedImage, KittyFormat};
use crate::terminal_colors::TerminalColors;

mod cache;
mod clipping;
mod colors;
pub(crate) mod compact;
mod contour;
mod depth;
mod joined;
mod patches;
mod placements;
pub(crate) mod raised;
mod raster;
mod regions;
mod strips;

// Includes queued variants: prewarming cannot overrun the admission budget.
const MAX_BYTES: usize = 64 * 1024 * 1024;
// Grid placement IDs grow from one. Reserve the upper half for native frames.
const FRAME_UID: u64 = 1 << 63;
// Composite the rim and translucent shadow over pane text and application
// images. Native floating-pane clipping is applied before serialization.
const FRAME_Z_INDEX: i32 = i32::MAX;

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
    raised: bool,
    region: bool,
}

impl TileKey {
    fn fits(self, geom: PaneGeom) -> bool {
        let (x, y) = self.insets();
        geom.cols.as_usize() * self.width >= 2 * (x + self.radius)
            && geom.rows.as_usize() * self.height >= 2 * (y + self.radius)
    }
    fn raised_padding(self) -> (usize, usize) {
        let support = (self.width as f64 * 0.30).max(0.75) * 5.0;
        let margin = (self.width as f64 * 0.35 + support + 1.0).ceil() as usize;
        (
            margin.div_ceil(self.width) * self.width,
            margin.div_ceil(self.height) * self.height,
        )
    }

    fn raised_margins(self) -> (usize, usize) {
        if self.region {
            let margin = self.width.div_ceil(2);
            return (margin, margin);
        }
        // Hints retain their existing rows and two-column inset. Place the
        // surface half a cell-width away from that content on every side.
        let padding = self.width.div_ceil(2).min(self.height);
        (self.width * 2 - padding, self.height - padding)
    }

    fn insets(self) -> (usize, usize) {
        if self.raised {
            let (x, y) = self.raised_padding();
            let (margin_x, margin_y) = self.raised_margins();
            (x + margin_x, y + margin_y)
        } else if self.compact {
            (self.width, self.height)
        } else {
            (self.thickness, self.thickness)
        }
    }

    fn tile_cells(self) -> usize {
        let blur = (self.width as f64 * 0.30).max(0.75) * 5.0;
        let tail = self.radius as f64 + self.width as f64 * 0.35 + blur + 1.0;
        let (x, y) = self.insets();
        (((x as f64 + tail) / self.width as f64).ceil() as usize)
            .max(((y as f64 + tail) / self.height as f64).ceil() as usize)
    }
}

#[derive(Debug, Default)]
pub(crate) struct FrameCache {
    clients: HashMap<ClientId, TileKey>,
    accents: HashMap<ClientId, [Option<[u8; 3]>; 16]>,
    atlases: HashMap<TileKey, InternalImageId>,
    strips: HashMap<TileKey, strips::Strips>,
    geometry: HashMap<(ClientId, PaneId), CachedGeometry>,
    raised_masks: HashMap<(ClientId, PaneId), Rc<raised::Surface>>,
    region_plans: HashMap<(ClientId, PaneId), Rc<regions::Plan>>,
    active_frames: HashMap<(ClientId, PaneId), FrameRequest>,
    host_backgrounds: HashMap<ClientId, zellij_utils::ipc::TerminalBackground>,
    worker: Option<raster::Worker>,
    rasterizer: raster::Rasterizer,
    requested: HashSet<TileKey>,
    patch_images: HashMap<patches::Key, patches::Images>,
    patch_requested: HashSet<patches::Key>,
    patch_rasterizer: patches::Rasterizer,
    joined_regions: HashMap<(ClientId, PaneId), [usize; 8]>,
    joined_layouts: std::collections::VecDeque<Rc<joined::Layout>>,
    budget: cache::Budget<cache::Asset>,
    pins: HashSet<TileKey>,
}

#[derive(Debug)]
struct CachedGeometry {
    key: TileKey,
    geom: PaneGeom,
    neighbors: Vec<PaneGeom>,
    images: Rc<Vec<KittyImageChunk>>,
}

#[derive(Clone, Debug)]
pub(super) struct FrameRequest {
    geom: PaneGeom,
    z_index: Option<usize>,
    changed: bool,
    atlas: Option<(TileKey, InternalImageId)>,
    images_override: Option<Rc<Vec<KittyImageChunk>>>,
}

impl FrameCache {
    pub(crate) fn update(&mut self, client_id: ClientId, colors: &TerminalColors) -> bool {
        let accents = colors.frame_accents();
        let accents_changed = self.accents.insert(client_id, accents) != Some(accents);
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
                    raised: false,
                    region: false,
                })
            });
        if let Some(key) = next {
            self.clients.insert(client_id, key);
        }
        previous != next || accents_changed
    }

    pub(crate) fn remove_client(&mut self, client_id: ClientId) {
        self.clients.remove(&client_id);
        self.accents.remove(&client_id);
        self.region_plans
            .retain(|(client, _), _| *client != client_id);
        self.joined_regions
            .retain(|(client, _), _| *client != client_id);
        self.host_backgrounds.remove(&client_id);
        self.active_frames
            .retain(|(client, _), _| *client != client_id);
        self.geometry.retain(|(client, _), _| *client != client_id);
        self.raised_masks
            .retain(|(client, _), _| *client != client_id);
    }

    pub(crate) fn warm_up(
        &mut self,
        client: ClientId,
        notify: Option<zellij_utils::channels::SenderWithContext<crate::screen::ScreenInstruction>>,
        store: &mut KittyImageStore,
    ) {
        let Some(base) = self.clients.get(&client).copied() else {
            return;
        };
        self.poll(store);
        if self.worker.is_none() {
            self.worker = notify.and_then(raster::Worker::start);
        }
        if self.worker.is_none() {
            return;
        }
        self.request(base, store);
        for accent in [None, Some(3), Some(4)] {
            if let Some(key) = self.raised_key(client, accent) {
                self.request(key, store);
            }
        }
        for accent in [None, Some(1), Some(2), Some(3), Some(4), Some(6)] {
            if let Some(key) = self.region_key(client, accent) {
                self.request(key, store);
            }
        }
        if let Some(mut key) = self.raised_key(client, None) {
            key.background = colors::lift(key.background, key.foreground);
            self.request(key, store);
        }
    }

    fn raised_key(&self, client: ClientId, accent: Option<u8>) -> Option<TileKey> {
        let mut key = *self.clients.get(&client)?;
        key.raised = true;
        key.compact = false;
        if let Some(accent) = accent.and_then(|index| {
            self.accents
                .get(&client)
                .and_then(|colors| colors.get(usize::from(index)).copied().flatten())
        }) {
            key.background = colors::tint(key.background, accent);
        }
        Some(key)
    }

    fn region_key(&self, client: ClientId, accent: Option<u8>) -> Option<TileKey> {
        let mut key = self.raised_key(client, None)?;
        key.region = true;
        key.background = colors::lift(key.background, key.foreground);
        if let Some(color) = accent.and_then(|index| {
            self.accents
                .get(&client)?
                .get(index as usize)
                .copied()
                .flatten()
        }) {
            key.background = colors::tint_amount(key.background, color, 0.65);
        }
        Some(key)
    }

    fn request(&mut self, key: TileKey, store: &mut KittyImageStore) -> bool {
        if self.requested.contains(&key) || self.atlases.contains_key(&key) {
            return false;
        }
        let span = key.tile_cells();
        let bytes = key.width * span * key.height * span * 8 * 4 + strips::bytes(key);
        if !self.admit(cache::Asset::Atlas(key), bytes, store) {
            return false;
        }
        self.requested.insert(key);
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.queue(key))
        {
            self.stop_worker(Some(cache::Asset::Atlas(key)));
        }
        true
    }

    fn install(
        &mut self,
        key: TileKey,
        prepared: raster::Prepared,
        store: &mut KittyImageStore,
    ) -> Option<InternalImageId> {
        let edge_strips = strips::install(prepared.strips, store)?;
        let id = match store.store_native_image(prepared.atlas.image, prepared.atlas.compressed) {
            Ok(id) => id,
            Err(_) => {
                edge_strips.release(store);
                return None;
            },
        };
        store.add_placement_ref(id);
        self.atlases.insert(key, id);
        self.strips.insert(key, edge_strips);
        Some(id)
    }

    fn poll(&mut self, store: &mut KittyImageStore) {
        let (completed, disconnected) = self
            .worker
            .as_ref()
            .map(|worker| worker.completed())
            .unwrap_or_default();
        for result in completed {
            match result {
                raster::Completed::Atlas(key, prepared) => {
                    self.requested.remove(&key);
                    if prepared
                        .and_then(|prepared| self.install(key, prepared, store))
                        .is_none()
                    {
                        self.budget.remove(&cache::Asset::Atlas(key));
                    }
                },
                raster::Completed::Patch(key, prepared) => {
                    self.patch_requested.remove(&key);
                    if prepared
                        .and_then(|prepared| self.install_patch(key.clone(), prepared, store))
                        .is_none()
                    {
                        self.budget.remove(&cache::Asset::Patch(key));
                    }
                },
            }
        }
        if disconnected {
            self.stop_worker(None);
        }
    }

    fn install_patch(
        &mut self,
        key: patches::Key,
        prepared: patches::Prepared,
        store: &mut KittyImageStore,
    ) -> Option<patches::Images> {
        let body = store
            .store_native_image(prepared.body.image, prepared.body.compressed)
            .ok()?;
        store.add_placement_ref(body);
        let shadow =
            match store.store_native_image(prepared.shadow.image, prepared.shadow.compressed) {
                Ok(id) => id,
                Err(_) => {
                    store.remove_placement_ref(body);
                    return None;
                },
            };
        store.add_placement_ref(shadow);
        let images = patches::Images { body, shadow };
        self.patch_images.insert(key, images);
        Some(images)
    }

    fn patch(
        &mut self,
        key: &patches::Key,
        store: &mut KittyImageStore,
    ) -> Option<patches::Images> {
        if let Some(images) = self.patch_images.get(key) {
            self.budget.touch(&cache::Asset::Patch(key.clone()));
            return Some(*images);
        }
        if self.patch_requested.contains(key)
            || !self.admit(cache::Asset::Patch(key.clone()), key.bytes(), store)
        {
            return None;
        }
        self.patch_requested.insert(key.clone());
        if let Some(worker) = &self.worker {
            if worker.queue_patch(key.clone()) {
                return None;
            }
            self.stop_worker(Some(cache::Asset::Patch(key.clone())));
        }
        let result = self
            .patch_rasterizer
            .prepare(key)
            .and_then(|prepared| self.install_patch(key.clone(), prepared, store));
        self.patch_requested.remove(key);
        if result.is_none() {
            self.budget.remove(&cache::Asset::Patch(key.clone()));
        }
        result
    }

    fn atlas(&mut self, key: TileKey, store: &mut KittyImageStore) -> Option<InternalImageId> {
        self.poll(store);
        if let Some(id) = self.atlases.get(&key) {
            self.budget.touch(&cache::Asset::Atlas(key));
            return Some(*id);
        }
        if self.request(key, store) && self.worker.is_none() {
            let result = self
                .rasterizer
                .prepare(key)
                .and_then(|prepared| self.install(key, prepared, store));
            self.requested.remove(&key);
            if result.is_none() {
                self.budget.remove(&cache::Asset::Atlas(key));
            }
            return result;
        }
        None
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
        cache
            .active_frames
            .iter()
            .find_map(|((client, _), frame)| {
                (*client == client_id)
                    .then_some(frame.atlas)
                    .flatten()
                    .map(|_| ())
            })?;
        let key = cache.clients.get(&client_id)?;
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
        if !key.fits(geom) {
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
        if self
            .recessed_frame_requests
            .get(&(client_id, pane_id))
            .is_some_and(|request| request.atlas.is_some_and(|(key, _)| key.raised))
        {
            return;
        }
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
            images_override: None,
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
            let mut cache = cache.borrow_mut();
            let live: HashSet<_> = cache.active_frames.keys().copied().collect();
            cache.geometry.retain(|pane, _| live.contains(pane));
            cache.raised_masks.retain(|pane, _| live.contains(pane));
            cache.region_plans.retain(|pane, _| live.contains(pane));
        }
        for ((client_id, pane_id), request) in std::mem::take(&mut self.recessed_frame_requests) {
            self.finish_recessed_frame(client_id, pane_id, request);
        }
        if let Some(cache) = &self.recessed_frames {
            cache.borrow_mut().pins.clear();
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
            let mut neighbors: Vec<_> = if key.compact && request.z_index.is_none() {
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
            neighbors
                .sort_by_key(|geom| (geom.x, geom.y, geom.cols.as_usize(), geom.rows.as_usize()));
            let cached = self.recessed_frames.as_ref().and_then(|cache| {
                cache
                    .borrow()
                    .geometry
                    .get(&(client_id, pane_id))
                    .filter(|cached| {
                        cached.key == key && cached.geom == geom && cached.neighbors == neighbors
                    })
                    .map(|cached| cached.images.clone())
            });
            let frame_images = if let Some(images) = request.images_override.clone() {
                images
            } else if let Some(images) = cached {
                images
            } else {
                let mut frame_images = vec![];
                for mut image in placements::cells(key, id, geom) {
                    let continuation = if key.compact {
                        compact::align_horizontal_edge(&mut image, geom, &neighbors, key)
                    } else {
                        None
                    };
                    for mut part in std::iter::once(image).chain(continuation) {
                        if !key.compact
                            || compact::clip_shared_cell(&mut part, geom, &neighbors, key)
                        {
                            frame_images.push(part);
                        }
                    }
                }
                let edge_strips = self.recessed_frames.as_ref().unwrap().borrow().strips[&key];
                let frame_images = Rc::new(strips::coalesce(frame_images, key, edge_strips));
                self.recessed_frames
                    .as_ref()
                    .unwrap()
                    .borrow_mut()
                    .geometry
                    .insert(
                        (client_id, pane_id),
                        CachedGeometry {
                            key,
                            geom,
                            neighbors,
                            images: frame_images.clone(),
                        },
                    );
                frame_images
            };
            let positioned;
            let frame_images = if key.raised && request.images_override.is_none() {
                positioned = frame_images
                    .iter()
                    .filter_map(|image| raised::position(*image, key))
                    .collect::<Vec<_>>();
                &positioned[..]
            } else {
                &frame_images[..]
            };
            let rounded_stack = self.raised_stack(client_id, request.z_index);
            images.extend(clipping::visible(
                frame_images,
                rounded_stack
                    .as_ref()
                    .or(self.floating_panes_stack.as_ref()),
                if rounded_stack.is_some() {
                    None
                } else {
                    request.z_index
                },
                key,
            ));
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
