//! Screen capture abstraction.

pub mod region;
pub mod wlr;

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

pub use region::{Output, Rect, Selection};

/// Byte order of [`CapturedImage::pixels`].
///
/// `zwlr_screencopy_frame_v1` negotiates the actual `wl_shm` buffer format per output/frame
/// (see `capture::wlr`'s `Buffer` event handling) — most compositor/GPU/driver combinations
/// report a BGRA-ordered format (`Argb8888`/`Xrgb8888`), but some report an already
/// RGBA-ordered one (`Abgr8888`/`Xbgr8888`). Consumers that assume a fixed channel order
/// (PNG encoding, GTK texture upload, ...) must check this before swizzling, or a
/// RGBA-ordered buffer gets its R/B channels needlessly (and incorrectly) swapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PixelFormat {
    /// Byte order B, G, R, A. Needs an R<->B swizzle before use as RGBA (PNG, GTK, ...).
    #[default]
    Bgra,
    /// Byte order R, G, B, A. Already correct for RGBA consumers — no swizzle needed.
    Rgba,
}

/// A single, raw, captured image (typically one `wl_output`).
#[derive(Clone)]
pub struct CapturedImage {
    pub width: u32,
    pub height: u32,
    /// Bytes per row.
    pub stride: u32,
    /// Raw pixel data, premultiplied. Byte order given by `format`.
    pub pixels: Arc<[u8]>,
    /// Byte order of `pixels`, as negotiated with the compositor for this image.
    pub format: PixelFormat,
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
            .field("format", &self.format)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_image_debug_includes_the_format() {
        let img = CapturedImage {
            width: 2,
            height: 2,
            stride: 8,
            pixels: Arc::from(vec![0u8; 16].into_boxed_slice()),
            format: PixelFormat::Rgba,
            source: None,
        };
        let debug = format!("{img:?}");
        assert!(debug.contains("format: Rgba"), "{debug}");
        assert!(debug.contains("<16 bytes>"), "{debug}");
    }
}
