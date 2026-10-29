//! Several independently shaded cards in one native plugin viewport. The plugin
//! retains all layout, input, cursor and lifecycle ownership.
use super::*;
use crate::panes::surface::SurfacePresentation;

#[derive(Debug)]
pub(super) struct Plan {
    geom: PaneGeom,
    presentation: SurfacePresentation,
    clips: Vec<PaneGeom>,
    layer: usize,
    surfaces: Vec<Rc<raised::Surface>>,
    images: Rc<Vec<KittyImageChunk>>,
}

impl Plan {
    pub(super) fn uses_material(&self, key: TileKey) -> bool {
        self.surfaces.iter().any(|surface| surface.key == key)
    }
}

fn contains(geom: PaneGeom, x: usize, y: usize) -> bool {
    x >= geom.x
        && x < geom.x + geom.cols.as_usize()
        && y >= geom.y
        && y < geom.y + geom.rows.as_usize()
}

impl Output {
    pub(crate) fn register_raised_regions(
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
        if presentation.regions.is_empty() || presentation.attachment.is_some() {
            return false;
        }
        let clips = self
            .floating_panes_stack
            .as_ref()
            .map(|stack| stack.layers.clone())
            .unwrap_or_default();
        let specs: Option<Vec<_>> = presentation
            .regions
            .iter()
            .map(|region| {
                if region.x + region.columns > geom.cols.as_usize()
                    || region.y + region.rows > geom.rows.as_usize()
                {
                    return None;
                }
                let key = cache.borrow().region_key(client, region.accent)?;
                let (mx, my) = key.raised_margins();
                if (region.columns * key.width).saturating_sub(2 * mx) < 2 * key.radius
                    || (region.rows * key.height).saturating_sub(2 * my) < 2 * key.radius
                {
                    return None;
                }
                let mut area = geom;
                area.x += region.x;
                area.y += region.y;
                area.cols.set_inner(region.columns);
                area.rows.set_inner(region.rows);
                Some((key, area))
            })
            .collect();
        let Some(specs) = specs else {
            return false;
        };
        cache
            .borrow_mut()
            .pins
            .extend(specs.iter().map(|(key, _)| *key));
        for (index, (_, area)) in specs.iter().enumerate() {
            if specs.iter().skip(index + 1).any(|(_, other)| {
                area.x < other.x + other.cols.as_usize()
                    && other.x < area.x + area.cols.as_usize()
                    && area.y < other.y + other.rows.as_usize()
                    && other.y < area.y + area.rows.as_usize()
            }) {
                return false;
            }
        }
        let previous = cache
            .borrow()
            .region_plans
            .get(&(client, pane))
            .filter(|plan| {
                plan.geom == geom
                    && plan.presentation == presentation
                    && plan.clips == clips
                    && plan.layer == layer
                    && plan
                        .surfaces
                        .iter()
                        .zip(&specs)
                        .all(|(surface, (key, _))| surface.key == *key)
            })
            .cloned();
        let changed = previous.is_none();
        // Queue every material before waiting; no sequential color warmup on
        // first display. Geometry-independent atlases are prewarmed at attach.
        let mut atlases = vec![];
        for (key, _) in &specs {
            atlases.push(
                cache
                    .borrow_mut()
                    .atlas(*key, &mut self.kitty_image_store.borrow_mut()),
            );
        }
        let Some(atlases) = atlases.into_iter().collect::<Option<Vec<_>>>() else {
            return false;
        };
        let plan = previous.unwrap_or_else(|| {
            let mut images = vec![];
            let mut surfaces = vec![];
            for ((key, area), id) in specs.iter().zip(&atlases) {
                let (ix, iy) = key.raised_padding();
                let mut padded = *area;
                padded.cols.increase_inner(2 * (ix / key.width));
                padded.rows.increase_inner(2 * (iy / key.height));
                let parts = strips::coalesce(
                    placements::cells(*key, *id, padded),
                    *key,
                    cache.borrow().strips[key],
                );
                images.extend(
                    parts
                        .into_iter()
                        .filter_map(|image| raised::position(image, *key)),
                );
                surfaces.push(Rc::new(raised::Surface::new(
                    *area,
                    *key,
                    layer,
                    presentation.clone(),
                    pane,
                    None,
                    clips.clone(),
                )));
            }
            // Placements for all regions belong to the same native pane.
            for (sequence, image) in images.iter_mut().enumerate() {
                image.placement_uid = FRAME_UID | sequence as u64;
            }
            let plan = Rc::new(Plan {
                geom,
                presentation,
                clips,
                layer,
                surfaces,
                images: Rc::new(images),
            });
            cache
                .borrow_mut()
                .region_plans
                .insert((client, pane), plan.clone());
            plan
        });
        for surface in &plan.surfaces {
            self.add_raised_surface(client, surface.clone());
        }
        let request = FrameRequest {
            geom,
            z_index: Some(layer),
            changed,
            atlas: Some((specs[0].0, atlases[0])),
            images_override: Some(plan.images.clone()),
        };
        cache
            .borrow_mut()
            .active_frames
            .insert((client, pane), request.clone());
        self.recessed_frame_requests.insert((client, pane), request);
        changed
    }

    pub(super) fn decorate_regions(
        &self,
        client: ClientId,
        layer: Option<usize>,
        chunks: &mut Vec<CharacterChunk>,
    ) -> bool {
        let Some(cache) = &self.recessed_frames else {
            return false;
        };
        let cache = cache.borrow();
        let Some(plan) = cache.region_plans.iter().find_map(|((owner, pane), plan)| {
            (*owner == client
                && Some(plan.layer) == layer
                && self.recessed_frame_requests.contains_key(&(client, *pane)))
            .then_some(plan)
        }) else {
            return false;
        };
        let mut result = vec![];
        for chunk in chunks.iter() {
            let mut run = chunk.clone();
            run.terminal_characters.clear();
            let mut x = chunk.x;
            for mut character in chunk.terminal_characters.iter().cloned() {
                let width = character.width();
                let surface = plan.surfaces.iter().find(|surface| {
                    contains(surface.geom, x, chunk.y)
                        && (x..x + width).all(|x| {
                            contains(surface.geom, x, chunk.y) && surface.full_cell(x, chunk.y)
                        })
                });
                if let Some(surface) = surface {
                    character.styles.update(|styles| {
                        // Replace the plugin's base surface, retaining intentional
                        // selection/field backgrounds and their project images.
                        let base = match styles.background {
                            None | Some(AnsiCode::Reset) => true,
                            Some(AnsiCode::RgbCode(color)) => color == surface.key.surface.into(),
                            _ => false,
                        };
                        if base {
                            styles.background =
                                Some(AnsiCode::RgbCode(surface.key.background.into()));
                        }
                    });
                    run.terminal_characters.push(character);
                } else {
                    if !run.terminal_characters.is_empty() {
                        result.push(run.clone());
                    }
                    run.terminal_characters.clear();
                    run.x = x + width;
                }
                x += width;
            }
            if !run.terminal_characters.is_empty() {
                result.push(run);
            }
        }
        *chunks = result;
        true
    }
}
