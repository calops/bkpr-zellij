//! Bounded LRU admission shared by image assets and raster geometry bases.
use super::*;
use std::hash::Hash;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Asset {
    Atlas(TileKey),
    Patch(patches::Key),
}

impl Asset {
    fn material(&self) -> TileKey {
        match self {
            Self::Atlas(key) => *key,
            Self::Patch(key) => key.material,
        }
    }
}

#[derive(Debug)]
pub(super) struct Budget<K> {
    entries: HashMap<K, (usize, u64)>,
    bytes: usize,
    limit: usize,
    stamp: u64,
}

impl<K> Default for Budget<K> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            limit: MAX_BYTES,
            stamp: 0,
        }
    }
}

impl<K: Eq + Hash + Clone> Budget<K> {
    pub fn fits(&self, bytes: usize) -> bool {
        bytes <= self.limit.saturating_sub(self.bytes)
    }
    pub fn possible(&self, bytes: usize) -> bool {
        bytes <= self.limit
    }
    pub fn insert(&mut self, key: K, bytes: usize) {
        self.remove(&key);
        self.stamp += 1;
        self.entries.insert(key, (bytes, self.stamp));
        self.bytes += bytes;
    }
    pub fn touch(&mut self, key: &K) {
        self.stamp += 1;
        if let Some((_, stamp)) = self.entries.get_mut(key) {
            *stamp = self.stamp;
        }
    }
    pub fn remove(&mut self, key: &K) {
        if let Some((bytes, _)) = self.entries.remove(key) {
            self.bytes -= bytes;
        }
    }
    pub fn oldest(&self, allowed: impl Fn(&K) -> bool) -> Option<K> {
        self.entries
            .iter()
            .filter(|(key, _)| allowed(key))
            .min_by_key(|(_, (_, stamp))| stamp)
            .map(|(key, _)| key.clone())
    }
}

impl FrameCache {
    pub(super) fn stop_worker(&mut self, keep: Option<Asset>) {
        self.worker = None;
        self.requested.retain(|key| {
            let asset = Asset::Atlas(*key);
            if keep.as_ref() == Some(&asset) {
                true
            } else {
                self.budget.remove(&asset);
                false
            }
        });
        self.patch_requested.retain(|key| {
            let asset = Asset::Patch(key.clone());
            if keep.as_ref() == Some(&asset) {
                true
            } else {
                self.budget.remove(&asset);
                false
            }
        });
    }
    pub(super) fn admit(
        &mut self,
        asset: Asset,
        bytes: usize,
        store: &mut KittyImageStore,
    ) -> bool {
        if !self.budget.possible(bytes) {
            return false;
        }
        while !self.budget.fits(bytes) {
            let victim = self.budget.oldest(|candidate| {
                let pending = match candidate {
                    Asset::Atlas(key) => self.requested.contains(key),
                    Asset::Patch(key) => self.patch_requested.contains(key),
                };
                let material = candidate.material();
                let current_client = |client: &ClientId| {
                    self.clients.get(client).is_some_and(|base| {
                        base.width == material.width
                            && base.height == material.height
                            && base.foreground == material.foreground
                            && base.surface == material.surface
                    })
                };
                let current = self.pins.contains(&material)
                    || self.clients.values().any(|base| *base == material)
                    || self.active_frames.iter().any(|((client, _), frame)| {
                        current_client(client)
                            && frame.atlas.is_some_and(|(key, _)| key == material)
                    })
                    || self.region_plans.iter().any(|((client, _), plan)| {
                        current_client(client) && plan.uses_material(material)
                    });
                !pending && !current
            });
            let Some(victim) = victim else {
                return false;
            };
            self.budget.remove(&victim);
            let material = victim.material();
            match victim {
                Asset::Atlas(key) => {
                    if let Some(id) = self.atlases.remove(&key) {
                        store.free(id);
                    }
                    if let Some(strips) = self.strips.remove(&key) {
                        strips.release(store);
                    }
                },
                Asset::Patch(key) => {
                    if let Some(images) = self.patch_images.remove(&key) {
                        store.free(images.body);
                        store.free(images.shadow);
                    }
                },
            }
            // These contain image IDs, not ownership references. Never reuse
            // placements built from a reclaimed asset, including idle hovers.
            self.geometry.retain(|_, geometry| geometry.key != material);
            self.raised_masks
                .retain(|_, surface| surface.key != material);
            self.region_plans
                .retain(|_, plan| !plan.uses_material(material));
            self.joined_layouts.retain(|layout| layout.key != material);
        }
        self.budget.insert(asset, bytes);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(width: usize) -> TileKey {
        TileKey {
            width,
            height: 17,
            background: [30; 3],
            foreground: [200; 3],
            surface: [20; 3],
            thickness: 4,
            radius: 12,
            compact: true,
            raised: false,
            region: false,
        }
    }

    #[test]
    fn short_panes_need_room_for_the_curve_not_two_whole_blur_tiles() {
        let key = key(8);
        let mut geom = PaneGeom::default();
        geom.cols.set_inner(6);
        geom.rows.set_inner(4);
        assert!(key.fits(geom));
        assert!(geom.rows.as_usize() < 2 * key.tile_cells());
        geom.rows.set_inner(3);
        assert!(!key.fits(geom));
    }

    #[test]
    fn a_new_font_can_reclaim_old_assets_without_evicting_another_clients_font() {
        let mut cache = FrameCache::default();
        cache.budget.limit = 16;
        let mut store = KittyImageStore::default();
        let old = key(8);
        let new = key(9);
        let id = store
            .store_native_image(
                DecodedImage {
                    bytes: vec![0; 4],
                    width: 1,
                    height: 1,
                    format: KittyFormat::Rgba32,
                },
                String::new(),
            )
            .unwrap();
        store.add_placement_ref(id);
        cache.atlases.insert(old, id);
        cache.budget.insert(Asset::Atlas(old), 16);
        cache.clients.insert(1, new);
        cache.clients.insert(2, old);
        assert!(!cache.admit(Asset::Atlas(new), 16, &mut store));
        assert!(store.get(id).is_some());
        cache.remove_client(2);
        cache.pins.insert(old);
        assert!(!cache.admit(Asset::Atlas(new), 16, &mut store));
        cache.pins.clear();
        assert!(cache.admit(Asset::Atlas(new), 16, &mut store));
        assert!(store.get(id).is_none());
        assert!(!cache.atlases.contains_key(&old));
        assert_eq!(cache.budget.bytes, 16);
    }
    #[test]
    fn bounded_admission_can_reuse_space_and_protect_live_entries() {
        let mut cache = Budget {
            limit: 10,
            ..Budget::default()
        };
        cache.insert("live", 4);
        cache.insert("old", 6);
        cache.touch(&"old");
        assert!(!cache.fits(6));
        let victim = cache.oldest(|key| *key != "live").unwrap();
        assert_eq!(victim, "old");
        cache.remove(&victim);
        assert!(cache.fits(6));
        cache.insert("new", 6);
        cache.remove(&"new"); // Failed preparation releases its reservation.
        assert!(cache.fits(6));
        assert!(!cache.possible(11));
    }
}
