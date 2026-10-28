//! Own the outer terminal's surface without changing the virtual pane palette.
use std::io::Write;
use zellij_utils::ipc::TerminalBackground;

#[derive(Default)]
pub(crate) struct Background {
    // Keep the terminal's exact XParseColor spelling/precision for restoration.
    original: Option<String>,
    surface: Option<[u8; 3]>,
    queried_surface: Option<([u8; 3], String)>,
}

impl Background {
    pub(crate) fn prepare_query(&mut self) {
        self.queried_surface = self.surface.zip(self.original.clone());
    }

    pub(crate) fn observe(&mut self, color: &mut String, out: &mut dyn Write) {
        if let Some((_, original)) = self.queried_surface.take().filter(|(surface, _)| {
            parse_rgb(color) == Some(*surface)
        }) {
            // A query can finish after ordinary-frame fallback restored the
            // host. Resolve its reply against ownership when it was issued.
            *color = original;
            return;
        }
        if self.surface.is_some() && parse_rgb(color) == self.surface {
            // A host query must not feed our own shaded surface back into the
            // pane palette (or shade it again on the next render).
            if let Some(original) = &self.original {
                *color = original.clone();
            }
        } else if parse_rgb(color).is_some() {
            self.original = Some(color.clone());
            // A theme reset can restore the original host color without
            // changing its RGB value. Keep ownership and reapply even when
            // the server will deduplicate the unchanged palette snapshot.
            if let Some(surface) = self.surface {
                let _ = write!(out, "\x1b]11;{}\x1b\\", rgb(surface));
                let _ = out.flush();
            }
        }
    }

    pub(crate) fn apply(&mut self, background: Option<TerminalBackground>, out: &mut dyn Write) {
        let Some(background) = background else {
            self.restore(out);
            return;
        };
        self.original
            .get_or_insert_with(|| rgb(background.original));
        if self.surface != Some(background.surface) {
            // Set ownership before writing so error/panic cleanup can restore
            // even after a partial write. A failed write remains retryable.
            self.surface = Some(background.surface);
            if write!(out, "\x1b]11;{}\x1b\\", rgb(background.surface)).is_err() {
                self.restore(out);
            }
        }
    }

    pub(crate) fn restore(&mut self, out: &mut dyn Write) {
        if self.surface.take().is_some() {
            if let Some(original) = &self.original {
                let _ = write!(out, "\x1b]11;{}\x1b\\", original);
                let _ = out.flush();
            }
        }
    }

    pub(crate) fn reply(&self, query: &[u8]) -> Option<Vec<u8>> {
        self.surface?;
        let terminator = match query {
            b"\x1b]11;?\x1b\\" => "\x1b\\",
            b"\x1b]11;?\x07" => "\x07",
            _ => return None,
        };
        Some(format!("\x1b]11;{}{}", self.original.as_ref()?, terminator).into_bytes())
    }
}

fn rgb([r, g, b]: [u8; 3]) -> String {
    format!(
        "rgb:{:04x}/{:04x}/{:04x}",
        u16::from(r) * 257,
        u16::from(g) * 257,
        u16::from(b) * 257
    )
}

fn parse_rgb(color: &str) -> Option<[u8; 3]> {
    if let Some(hex) = color.strip_prefix('#') {
        if !matches!(hex.len(), 3 | 6 | 9 | 12) || !hex.is_ascii() {
            return None;
        }
        let width = hex.len() / 3;
        let mut result = [0; 3];
        for (index, channel) in result.iter_mut().enumerate() {
            let value = u32::from_str_radix(&hex[index * width..(index + 1) * width], 16).ok()?;
            *channel = ((value << 4) >> (4 * width.saturating_sub(1))) as u8;
        }
        return Some(result);
    }
    let mut parts = color.strip_prefix("rgb:")?.split('/');
    let mut result = [0; 3];
    for channel in &mut result {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 4 {
            return None;
        }
        let value = u32::from_str_radix(part, 16).ok()?;
        let maximum = (1u32 << (part.len() * 4)) - 1;
        *channel = ((value * 255) / maximum) as u8;
    }
    parts.next().is_none().then_some(result)
}
