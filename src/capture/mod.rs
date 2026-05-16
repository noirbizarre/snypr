//! Screen capture abstraction.

pub mod region;
pub mod wlr;

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

pub use region::{Output, Rect, Selection};

/// A single, raw, captured image (typically one `wl_output`).
#[derive(Clone)]
pub struct CapturedImage {
    pub width: u32,
    pub height: u32,
    /// Bytes per row.
    pub stride: u32,
    /// Raw pixel data. Format is `BGRA8888` premultiplied.
    pub pixels: Arc<[u8]>,
    /// The output this image was captured from. `None` for synthetic/composited buffers.
    pub source: Option<Output>,
}

impl std::fmt::Debug for CapturedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapturedImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .field("pixels", &format_args!("<{} bytes>", self.pixels.len()))
            .field("source", &self.source)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("no wlr-screencopy support advertised by the compositor")]
    UnsupportedCompositor,
    #[error("no matching output for selection `{0}`")]
    NoMatchingOutput(String),
    #[error("wayland error: {0}")]
    Wayland(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[async_trait]
pub trait Capturer: Send + Sync {
    async fn outputs(&self) -> anyhow::Result<Vec<Output>>;
    async fn capture(
        &self,
        selection: Selection,
        cursor: bool,
    ) -> anyhow::Result<Vec<CapturedImage>>;
}
