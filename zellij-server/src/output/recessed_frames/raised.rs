//! Raised surfaces share the pane atlas, blur, strip and placement pipeline.
//! Only the material and occlusion mask differ. Floating geometry stays native.
use super::*;
use crate::panes::surface::SurfacePresentation;

#[derive(Clone, Debug)]
pub(crate) struct Scene {
    stack: FloatingPanesStack,
    surfaces: Vec<Rc<Surface>>,
}

#[derive(Clone, Debug)]
pub(crate) struct Surface {
    pub(super) pane: PaneId,
    pub(super) geom: PaneGeom,
    pub(super) key: TileKey,
    pub(super) layer: usize,
    coverage: Vec<PaneGeom>,
    presentation: SurfacePresentation,
    joined: Option<Rc<joined::Plan>>,
    clip_layers: Vec<PaneGeom>,
}

impl Surface {
    pub(super) fn full_cell(&self, x: usize, y: usize) -> bool {
        self.coverage.iter().any(|row| {
            x >= row.x
                && x < row.x + row.cols.as_usize()
                && y >= row.y
                && y < row.y + row.rows.as_usize()
        })
    }
    pub(super) fn corner_cell(&self, x: usize, y: usize) -> bool {
        if let Some(plan) = &self.joined {
            return !plan.full_cell(x, y);
        }
        let radius = self.key.radius as f64;
        let width = (self.geom.cols.as_usize() * self.key.width) as f64;
        let height = (self.geom.rows.as_usize() * self.key.height) as f64;
        let x = ((x - self.geom.x) * self.key.width) as f64;
        let y = ((y - self.geom.y) * self.key.height) as f64;
        let (margin_x, margin_y) = self.key.raised_margins();
        let margin_x = margin_x as f64;
        let margin_y = margin_y as f64;
        if x < margin_x
            || y < margin_y
            || x + self.key.width as f64 > width - margin_x
            || y + self.key.height as f64 > height - margin_y
        {
            return true;
        }
        if (x >= margin_x + radius && x + self.key.width as f64 <= width - margin_x - radius)
            || (y >= margin_y + radius && y + self.key.height as f64 <= height - margin_y - radius)
        {
            return false;
        }
        [x, x + self.key.width as f64].into_iter().any(|x| {
            [y, y + self.key.height as f64].into_iter().any(|y| {
                let (inset_x, inset_y) = self.key.raised_padding();
                depth::corner_distance(
                    self.key,
                    x.min(width - x) + inset_x as f64,
                    y.min(height - y) + inset_y as f64,
                ) < 0.0
            })
        })
    }

    pub(super) fn new(
        geom: PaneGeom,
        key: TileKey,
        layer: usize,
        presentation: SurfacePresentation,
        pane: PaneId,
        joined: Option<Rc<joined::Plan>>,
        clip_layers: Vec<PaneGeom>,
    ) -> Self {
        let mut surface = Self {
            pane,
            geom,
            key,
            layer,
            coverage: vec![],
            presentation,
            joined,
            clip_layers,
        };
        // Expose complete cells intersected by a rounded cutout. The opaque
        // portion of the cached corner image masks the lower cells precisely.
        for y in geom.y..geom.y + geom.rows.as_usize() {
            let mut left = geom.x;
            let right = geom.x + geom.cols.as_usize();
            while left < right && surface.corner_cell(left, y) {
                left += 1;
            }
            let mut end = right;
            while end > left && surface.corner_cell(end - 1, y) {
                end -= 1;
            }
            if left == end {
                continue;
            }
            if let Some(previous) = surface.coverage.last_mut() {
                if previous.x == left && previous.cols.as_usize() == end - left {
                    previous.rows.increase_inner(1);
                    continue;
                }
            }
            let mut row = geom;
            row.x = left;
            row.y = y;
            row.cols.set_inner(end - left);
            row.rows.set_inner(1);
            surface.coverage.push(row);
        }
        surface
    }

    fn decorate(&self, chunks: Vec<CharacterChunk>) -> Vec<CharacterChunk> {
        let mut result = vec![];
        let bg = Some(AnsiCode::RgbCode(self.key.background.into()));
        for chunk in chunks {
            let mut run = chunk.clone();
            run.terminal_characters.clear();
            let mut x = chunk.x;
            for mut character in chunk.terminal_characters.iter().cloned() {
                let width = character.width();
                let inside = x >= self.geom.x
                    && x + width <= self.geom.x + self.geom.cols.as_usize()
                    && chunk.y >= self.geom.y
                    && chunk.y < self.geom.y + self.geom.rows.as_usize();
                let outline = self.joined.is_some()
                    && (x == self.geom.x
                        || x + width == self.geom.x + self.geom.cols.as_usize()
                        || chunk.y == self.geom.y
                        || chunk.y + 1 == self.geom.y + self.geom.rows.as_usize());
                let cutout = inside && (x..x + width).any(|x| self.corner_cell(x, chunk.y));
                if cutout {
                    if !run.terminal_characters.is_empty() {
                        result.push(run.clone());
                    }
                    run.terminal_characters.clear();
                    run.x = x + width;
                } else {
                    if inside {
                        if outline {
                            character =
                                TerminalCharacter::new_singlewidth_styled(' ', character.styles);
                        }
                        character.styles.update(|styles| {
                            styles.background = bg;
                        });
                    }
                    run.terminal_characters.push(character);
                }
                x += width;
            }
            if !run.terminal_characters.is_empty() {
                result.push(run);
            }
        }
        result
    }
}

impl Output {
    pub(crate) fn finish_raised_surfaces(&mut self, clients: &HashSet<ClientId>) -> Vec<Viewport> {
        let Some(cache) = self.recessed_frames.as_ref() else {
            return vec![];
        };
        let mut current = HashMap::new();
        for client in clients {
            if let Some(scene) = self.raised_surfaces.get(client) {
                for surface in &scene.surfaces {
                    if let Some(plan) = &surface.joined {
                        let a = &plan.attachment;
                        let g = surface.geom;
                        current.insert(
                            (*client, surface.pane),
                            [
                                a.x,
                                a.y,
                                a.columns,
                                a.rows,
                                g.x,
                                g.y,
                                g.cols.as_usize(),
                                g.rows.as_usize(),
                            ],
                        );
                    }
                }
            }
        }
        let mut cache = cache.borrow_mut();
        let mut damage = vec![];
        let mut add = |rects: [usize; 8]| {
            for rect in rects.chunks_exact(4) {
                damage.push(Viewport {
                    x: rect[0],
                    y: rect[1],
                    cols: rect[2],
                    rows: rect[3],
                });
            }
        };
        for (identity, old) in &cache.joined_regions {
            if clients.contains(&identity.0) && current.get(identity) != Some(old) {
                add(*old);
            }
        }
        for (identity, new) in &current {
            if cache.joined_regions.get(identity) != Some(new) {
                add(*new);
            }
        }
        cache
            .joined_regions
            .retain(|(client, _), _| !clients.contains(client));
        cache.joined_regions.extend(current);
        damage
    }

    pub(crate) fn register_raised_popup(
        &mut self,
        client: ClientId,
        pane: PaneId,
        geom: PaneGeom,
        layer: usize,
        presentation: SurfacePresentation,
    ) -> bool {
        let Some(cache) = self.recessed_frames.clone() else {
            return false;
        };
        let Some(mut key) = cache.borrow().raised_key(client, presentation.accent) else {
            return false;
        };
        if presentation.attachment.is_some() {
            key.background = colors::lift(key.background, key.foreground);
        }
        cache.borrow_mut().pins.insert(key);
        let clip_layers = self
            .floating_panes_stack
            .as_ref()
            .map(|stack| stack.layers.clone())
            .unwrap_or_default();
        let previous = cache
            .borrow()
            .raised_masks
            .get(&(client, pane))
            .filter(|surface| {
                surface.geom == geom
                    && surface.key == key
                    && surface.layer == layer
                    && surface.presentation == presentation
            })
            .cloned();
        let joined = if let Some(attachment) = &presentation.attachment {
            if let Some(previous) = &previous {
                previous.joined.clone()
            } else {
                let plan = joined::Plan::prepare(
                    &mut cache.borrow_mut(),
                    &mut self.kitty_image_store.borrow_mut(),
                    key,
                    geom,
                    attachment,
                );
                let Some(plan) = plan else {
                    return false;
                };
                Some(Rc::new(plan))
            }
        } else {
            None
        };
        let (margin_x, margin_y) = key.raised_margins();
        if joined.is_none()
            && ((geom.cols.as_usize() * key.width).saturating_sub(2 * margin_x) < key.radius * 2
                || (geom.rows.as_usize() * key.height).saturating_sub(2 * margin_y)
                    < key.radius * 2)
        {
            return false;
        }
        let Some(id) = cache
            .borrow_mut()
            .atlas(key, &mut self.kitty_image_store.borrow_mut())
        else {
            return false;
        };
        let (inset_x, inset_y) = key.raised_padding();
        let mut image_geom = geom;
        image_geom.cols.increase_inner(2 * (inset_x / key.width));
        image_geom.rows.increase_inner(2 * (inset_y / key.height));
        let request = FrameRequest {
            geom: image_geom,
            z_index: Some(layer),
            changed: previous
                .as_ref()
                .is_none_or(|surface| surface.clip_layers != clip_layers),
            atlas: Some((key, id)),
            images_override: joined.as_ref().map(|plan| plan.images.clone()),
        };
        cache
            .borrow_mut()
            .active_frames
            .insert((client, pane), request.clone());
        self.recessed_frame_requests.insert((client, pane), request);
        let mut changed = false;
        let surface = {
            let mut cache = cache.borrow_mut();
            let cached = cache
                .raised_masks
                .get(&(client, pane))
                .filter(|surface| {
                    surface.geom == geom
                        && surface.key == key
                        && surface.layer == layer
                        && surface.presentation == presentation
                        && surface.clip_layers == clip_layers
                })
                .cloned();
            cached.unwrap_or_else(|| {
                changed = true;
                let surface = Rc::new(Surface::new(
                    geom,
                    key,
                    layer,
                    presentation,
                    pane,
                    joined,
                    clip_layers,
                ));
                cache.raised_masks.insert((client, pane), surface.clone());
                surface
            })
        };
        if let Some(stack) = &self.floating_panes_stack {
            self.raised_surfaces
                .entry(client)
                .or_insert_with(|| Scene {
                    stack: stack.clone(),
                    surfaces: vec![],
                })
                .surfaces
                .push(surface);
        }
        changed
    }

    pub(super) fn add_raised_surface(&mut self, client: ClientId, surface: Rc<Surface>) {
        if let Some(stack) = &self.floating_panes_stack {
            self.raised_surfaces
                .entry(client)
                .or_insert_with(|| Scene {
                    stack: stack.clone(),
                    surfaces: vec![],
                })
                .surfaces
                .push(surface);
        }
    }

    pub(in crate::output) fn decorate_raised_popup(
        &self,
        client: ClientId,
        z: Option<usize>,
        chunks: &mut Vec<CharacterChunk>,
    ) {
        if self.decorate_regions(client, z, chunks) {
            return;
        }
        if z.is_none() {
            if let Some(scene) = self.raised_surfaces.get(&client) {
                for surface in &scene.surfaces {
                    let Some(plan) = &surface.joined else {
                        continue;
                    };
                    for chunk in chunks.iter_mut() {
                        let mut x = chunk.x;
                        for character in &mut chunk.terminal_characters {
                            if plan.contains_item_cell(x, chunk.y)
                                || (plan.guide_cell(x, chunk.y) && plan.full_cell(x, chunk.y))
                            {
                                if plan.guide_cell(x, chunk.y) {
                                    *character = TerminalCharacter::new_singlewidth_styled(
                                        ' ',
                                        character.styles.clone(),
                                    );
                                }
                                let color = if surface.corner_cell(x, chunk.y) {
                                    surface.key.surface
                                } else {
                                    surface.key.background
                                };
                                character.styles.update(|styles| {
                                    styles.background = Some(AnsiCode::RgbCode(color.into()));
                                });
                            }
                            x += character.width();
                        }
                    }
                }
            }
        }
        if let Some(surface) = self.raised_surfaces.get(&client).and_then(|scene| {
            scene
                .surfaces
                .iter()
                .find(|surface| Some(surface.layer) == z)
        }) {
            *chunks = surface.decorate(std::mem::take(chunks));
        }
    }

    // Expand rounded layers only after applying the native z-index. This keeps
    // native input/visibility ordering intact even when one mask has many rows.
    pub(in crate::output) fn raised_stack(
        &self,
        client: ClientId,
        z: Option<usize>,
    ) -> Option<FloatingPanesStack> {
        let scene = self.raised_surfaces.get(&client)?;
        let surfaces = &scene.surfaces;
        let stack = &scene.stack;
        let layers = stack
            .layers
            .iter()
            .enumerate()
            .skip(z.unwrap_or(0))
            .flat_map(|(index, geom)| {
                let matching: Vec<_> = surfaces
                    .iter()
                    .filter(|surface| surface.layer == index + 1)
                    .collect();
                if matching.is_empty() {
                    vec![*geom]
                } else {
                    matching
                        .into_iter()
                        .flat_map(|surface| surface.coverage.clone())
                        .collect()
                }
            })
            .collect();
        Some(FloatingPanesStack { layers })
    }
}

// The same continuous convolution as recessed shadows, with the inside mask
// casting onto the outside instead. No bevel, outline or focus-dependent color.
pub(super) fn sample(key: TileKey, tile: usize, px: f64, py: f64) -> [f64; 4] {
    let distance = depth::opening_distance(key, tile, px, py);
    if distance < 0.0 {
        let alpha = (1.0 - depth::blurred_outside(key, tile, px, py)) * 0.45;
        return [0.0, 0.0, 0.0, alpha * 255.0];
    }
    let left = (px / key.width as f64).floor() * key.width as f64;
    let top = (py / key.height as f64).floor() * key.height as f64;
    let cutout = [left, left + key.width as f64].into_iter().any(|x| {
        [top, top + key.height as f64]
            .into_iter()
            .any(|y| depth::opening_distance(key, tile, x, y) < 0.0)
    });
    if cutout || distance < depth::bevel_width(key) {
        let rgb = depth::bevel(key, tile, px, py, distance);
        [rgb[0], rgb[1], rgb[2], 255.0]
    } else {
        [0.0; 4]
    }
}

// Cached placements use a positive virtual origin. Translate the shadow ring
// back around its unchanged native rectangle, clipping at the screen origin.
pub(super) fn position(mut image: KittyImageChunk, key: TileKey) -> Option<KittyImageChunk> {
    let (inset_x, inset_y) = key.raised_padding();
    let x = image.cell_x * key.width + image.cell_offset_x as usize;
    let y = image.cell_y * key.height + image.cell_offset_y as usize;
    let crop_x = inset_x.saturating_sub(x);
    let crop_y = inset_y.saturating_sub(y);
    if crop_x >= image.source_px_width || crop_y >= image.source_px_height {
        return None;
    }
    image.source_px_x += crop_x;
    image.source_px_y += crop_y;
    image.source_px_width -= crop_x;
    image.source_px_height -= crop_y;
    let x = x.saturating_sub(inset_x);
    let y = y.saturating_sub(inset_y);
    image.cell_x = x / key.width;
    image.cell_y = y / key.height;
    image.cell_offset_x = (x % key.width) as u32;
    image.cell_offset_y = (y % key.height) as u32;
    Some(image)
}
