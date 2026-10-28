use crate::panes::{alacritty_functions::xparse_color, terminal_character::AnsiCode};
use serde::Serialize;
use zellij_utils::data::Event;

#[derive(Debug, Clone)]
pub enum TerminalColorUpdate {
    Foreground(String),
    Background(String),
    Ansi(Vec<(usize, String)>),
    Truecolor(bool),
    ResetPalette,
    KittyGraphics(bool),
    CellPixels(Option<[usize; 2]>),
    Batch {
        metadata: zellij_utils::ipc::TerminalColorsUpdate,
        ansi: Vec<(usize, String)>,
    },
}

/// Actual attached-terminal responses, never the configured Zellij theme.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TerminalColors {
    truecolor: bool,
    kitty_graphics: bool,
    cell_pixels: Option<[usize; 2]>,
    foreground: Option<[u8; 3]>,
    background: Option<[u8; 3]>,
    ansi: [Option<[u8; 3]>; 16],
}

impl TerminalColors {
    pub(crate) fn frame_accents(&self) -> [Option<[u8; 3]>; 16] {
        self.ansi
    }
    pub(crate) fn frame_colors(&self) -> Option<([usize; 2], [u8; 3], [u8; 3])> {
        if !self.truecolor || !self.kitty_graphics {
            return None;
        }
        Some((self.cell_pixels?, self.foreground?, self.background?))
    }

    pub fn update(&mut self, update: TerminalColorUpdate) -> bool {
        match update {
            TerminalColorUpdate::KittyGraphics(value) => {
                std::mem::replace(&mut self.kitty_graphics, value) != value
            },
            TerminalColorUpdate::CellPixels(value) => {
                std::mem::replace(&mut self.cell_pixels, value) != value
            },
            TerminalColorUpdate::Batch { metadata, ansi } => {
                // A reset+replacement is atomic to consumers. Retain the small
                // fixed-size previous value only when needed to dedupe it.
                let before_reset = metadata.reset.then(|| self.clone());
                let mut changed = metadata.reset && self.update(TerminalColorUpdate::ResetPalette);
                if let Some(color) = metadata.foreground {
                    changed |= self.update(TerminalColorUpdate::Foreground(color));
                }
                if let Some(color) = metadata.background {
                    changed |= self.update(TerminalColorUpdate::Background(color));
                }
                if let Some(supported) = metadata.truecolor {
                    changed |= self.update(TerminalColorUpdate::Truecolor(supported));
                }
                changed |= self.update(TerminalColorUpdate::Ansi(ansi));
                before_reset.map_or(changed, |before| *self != before)
            },
            TerminalColorUpdate::Foreground(color) => {
                let rgb = parse_rgb(&color);
                std::mem::replace(&mut self.foreground, rgb) != rgb
            },
            TerminalColorUpdate::Background(color) => {
                let rgb = parse_rgb(&color);
                std::mem::replace(&mut self.background, rgb) != rgb
            },
            TerminalColorUpdate::Ansi(colors) => {
                let mut changed = false;
                for (index, color) in colors {
                    if let Some(entry) = self.ansi.get_mut(index) {
                        let rgb = parse_rgb(&color);
                        changed |= std::mem::replace(entry, rgb) != rgb;
                    }
                }
                changed
            },
            TerminalColorUpdate::Truecolor(supported) => {
                std::mem::replace(&mut self.truecolor, supported) != supported
            },
            TerminalColorUpdate::ResetPalette => {
                let changed = self.foreground.is_some() || self.background.is_some()
                    || self.ansi.iter().any(Option::is_some);
                self.foreground = None;
                self.background = None;
                self.ansi = [None; 16];
                changed
            },
        }
    }

    pub fn event(&self) -> Event {
        // Fixed-size RGB/bool data cannot fail JSON serialization.
        Event::CustomMessage("bkpr_terminal_colors".into(), serde_json::to_string(self).unwrap())
    }
}

fn parse_rgb(color: &str) -> Option<[u8; 3]> {
    match xparse_color(color.as_bytes()) {
        Some(AnsiCode::RgbCode((r, g, b))) => Some([r, g, b]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_rgb_responses_do_not_imply_truecolor() {
        let mut colors = TerminalColors::default();
        assert!(colors.update(TerminalColorUpdate::Background("rgb:ffff/0000/8080".into())));
        assert_eq!(colors.background, Some([255, 0, 128]));
        assert!(!colors.truecolor);
        assert!(colors.update(TerminalColorUpdate::Truecolor(true)));
        assert!(!colors.update(TerminalColorUpdate::Truecolor(true)));
    }

    #[test]
    fn unknown_entries_remain_null_and_indices_are_bounded() {
        let mut colors = TerminalColors::default();
        colors.update(TerminalColorUpdate::Ansi(vec![
            (2, "rgb:1111/2222/3333".into()),
            (16, "rgb:ffff/ffff/ffff".into()),
        ]));
        assert_eq!(colors.ansi[2], Some([17, 34, 51]));
        assert!(colors.ansi[0].is_none());
        let Event::CustomMessage(name, payload) = colors.event() else { panic!() };
        assert_eq!(name, "bkpr_terminal_colors");
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["ansi"].as_array().unwrap().len(), 16);
        assert!(value["foreground"].is_null());
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;
    #[test]
    fn reset_and_replacement_emit_only_a_changed_final_palette() {
        let mut colors = TerminalColors::default();
        let batch = || TerminalColorUpdate::Batch {
            metadata: zellij_utils::ipc::TerminalColorsUpdate {
                background: Some("rgb:1111/2222/3333".into()),
                truecolor: Some(true),
                reset: true,
                ..Default::default()
            },
            ansi: vec![(1, "rgb:ffff/0000/0000".into())],
        };
        assert!(colors.update(batch()));
        assert!(!colors.update(batch()));
    }
}
