//! Keep the experimental shared-cell layout separate from native layout policy.
use super::*;
use crate::output::recessed_frames::compact;

impl TiledPanes {
    pub(super) fn update_compact_frames(&mut self, output: &Output) {
        let enabled = compact::requested()
            && self.style.rounded_corners
            && self.pane_frame_style.draws_full_frames()
            && self.fullscreen_is_active.is_none()
            && self
                .reserved_top_rows
                .borrow()
                .values()
                .all(|rows| *rows == 0)
            && output.supports_compact_frames(self.connected_clients.borrow().iter().copied())
            && self
                .panes
                .values()
                .filter(|pane| !pane.borderless())
                .all(|pane| {
                    let geom = pane.current_geom();
                    !geom.is_stacked() && geom.cols.as_usize() >= 6 && geom.rows.as_usize() >= 6
                });
        let needs_offsets = enabled && {
            let geoms: Vec<_> = self
                .panes
                .values()
                .filter(|pane| !pane.borderless() && !self.panes_to_hide.contains(&pane.pid()))
                .map(|pane| pane.current_geom())
                .collect();
            self.panes
                .values()
                .filter(|pane| !pane.borderless() && !self.panes_to_hide.contains(&pane.pid()))
                .any(|pane| {
                    pane.get_content_offset()
                        != compact::content_offset(pane.current_geom(), &geoms)
                })
        };
        if enabled != self.compact_recessed_frames || needs_offsets {
            self.compact_recessed_frames = enabled;
            self.set_pane_frames(self.pane_frame_style);
            self.set_force_render();
        }
    }
}
