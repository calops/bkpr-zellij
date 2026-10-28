//! Frame-local presentation for explicitly opted-in passive plugin surfaces.
//! This is neither a terminal title nor application/harness semantic state.
use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfacePresentation {
    version: u8,
    pub accent: Option<u8>,
    #[serde(default)]
    pub attachment: Option<SurfaceAttachment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceAttachment {
    pub x: usize,
    pub y: usize,
    pub columns: usize,
    pub rows: usize,
    pub guide_columns: Vec<usize>,
    pub guide_y: usize,
}

impl Default for SurfacePresentation {
    fn default() -> Self {
        Self {
            version: 1,
            accent: None,
            attachment: None,
        }
    }
}

impl SurfacePresentation {
    pub(super) fn from_frame(bytes: &[u8]) -> Option<Self> {
        let body = bytes.strip_prefix(b"\x1b]7502;")?;
        let end = body.iter().take(1025).position(|byte| *byte == b'\x07')?;
        let presentation: Self = serde_json::from_slice(&body[..end]).ok()?;
        (presentation.version == 1
            && presentation.accent.is_none_or(|index| index < 16)
            && presentation.attachment.as_ref().is_none_or(|rect| {
                rect.columns > 0
                    && rect.rows > 0
                    && rect.guide_y >= rect.y.saturating_sub(1)
                    && rect.guide_y <= rect.y
                    && rect.guide_columns.len() <= rect.rows.saturating_add(2)
                    && rect
                        .guide_columns
                        .iter()
                        .all(|columns| *columns <= rect.columns)
                    && rect
                        .x
                        .checked_add(rect.columns)
                        .is_some_and(|right| right <= 100000)
                    && rect
                        .y
                        .checked_add(rect.rows)
                        .is_some_and(|bottom| bottom <= 100000)
            }))
        .then_some(presentation)
    }
}
