//! A presentation opt-in carried by the existing plugin configuration API.
//! No popup lifecycle, focus, input or placement policy lives here.
use super::*;
use zellij_utils::input::layout::Run;

impl FloatingPanes {
    pub(crate) fn prepare_raised_surfaces(&mut self, output: &mut Output) -> Vec<Viewport> {
        let mut damage = vec![];
        if !self.style.rounded_corners || self.fullscreen_pane_id.is_some() {
            return damage;
        }
        let Some(stack) = self.stack() else {
            return damage;
        };
        for (id, pane) in &mut self.panes {
            if pane.selectable() || !pane.borderless() {
                continue;
            }
            let Some(Run::Plugin(plugin)) = pane.invoked_with() else {
                continue;
            };
            let Some(config) = plugin.get_configuration() else {
                continue;
            };
            if config.inner().get("bkpr_surface").map(String::as_str) != Some("raised-outline") {
                continue;
            }
            let geom = pane.current_geom();
            let Some(layer) = stack.layers.iter().position(|layer| *layer == geom) else {
                continue;
            };
            let mut changed = false;
            for client in self.connected_clients.borrow().iter().copied() {
                let presentation = pane
                    .surface_presentation(client)
                    .cloned()
                    .unwrap_or_default();
                changed |= output.register_raised_popup(client, *id, geom, layer + 1, presentation);
            }
            if changed {
                // A capability, material or caption transition changes existing
                // text cells as well as graphics, even if plugin content idles.
                pane.render_full_viewport();
                pane.set_should_render(true);
                damage.push(Viewport {
                    x: geom.x,
                    y: geom.y,
                    cols: geom.cols.as_usize(),
                    rows: geom.rows.as_usize(),
                });
            }
        }
        damage
    }
}
