//! Frame-local presentation for explicitly opted-in passive plugin surfaces.
//! This is neither a terminal title nor application/harness semantic state.
use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfacePresentation {
    version: u8,
    pub accent: Option<u8>,
}

impl Default for SurfacePresentation {
    fn default() -> Self {
        Self {
            version: 1,
            accent: None,
        }
    }
}

impl SurfacePresentation {
    pub(super) fn from_frame(bytes: &[u8]) -> Option<Self> {
        let body = bytes.strip_prefix(b"\x1b]7502;")?;
        let end = body.iter().take(1025).position(|byte| *byte == b'\x07')?;
        let presentation: Self = serde_json::from_slice(&body[..end]).ok()?;
        (presentation.version == 1 && presentation.accent.is_none_or(|index| index < 16))
            .then_some(presentation)
    }
}
