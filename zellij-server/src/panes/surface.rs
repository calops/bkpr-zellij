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
    #[serde(default)]
    pub regions: Vec<SurfaceRegion>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceRegion {
    pub x: usize,
    pub y: usize,
    pub columns: usize,
    pub rows: usize,
    pub accent: Option<u8>,
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
            regions: vec![],
        }
    }
}

impl SurfacePresentation {
    pub(super) fn from_frame(bytes: &[u8]) -> Option<Self> {
        let body = bytes.strip_prefix(b"\x1b]7502;")?;
        let end = body.iter().take(1025).position(|byte| *byte == b'\x07')?;
        let presentation: Self = serde_json::from_slice(&body[..end]).ok()?;
        (presentation.version == 1
            && presentation.regions.len() <= 3
            && presentation.regions.iter().all(|r| {
                r.columns > 0
                    && r.rows > 0
                    && r.x.checked_add(r.columns).is_some_and(|v| v <= 100000)
                    && r.y.checked_add(r.rows).is_some_and(|v| v <= 100000)
                    && r.accent.is_none_or(|index| index < 16)
            })
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
