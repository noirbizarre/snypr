//! GTK4 user interface entry points.
//!
//! These modules are gated behind the `ui` cargo feature.

pub mod canvas;
pub mod editor;
pub mod overlay;
pub mod selector;
pub mod style;
#[cfg(feature = "tray")]
pub mod tray;

use std::path::PathBuf;

use anyhow::{Context as _, Result};

use crate::annotate::DocumentBase;
use crate::capture::{Capturer, Selection, wlr::WlrCapturer};
use crate::cli::SinkSpec;
use crate::context::Ctx;

/// Application id used across the GTK windows and the StatusNotifierItem.
pub const APP_ID: &str = "ai.hyprtools.HyprSnap";

/// Run the `capture` flow: interactive selector → screencopy → editor → sinks.
///
/// The captured pixels are handed to the editor as an in-memory [`DocumentBase`] so we skip a
/// PNG encode + decode round-trip. The editor's save action then re-encodes once when the user
/// commits, fanning the bytes out to whatever sinks the caller (or config) provides. Returns
/// the paths written during the editor session (empty if the user closed without saving),
/// matching `cli::screenshot::execute` so the daemon can ferry results back over IPC.
pub async fn run_capture_flow(
    ctx: Ctx,
    sinks: Vec<SinkSpec>,
    cursor: bool,
) -> Result<Vec<PathBuf>> {
    let rect = selector::pick_region(ctx.clone())
        .await
        .context("interactive region selection")?;
    tracing::info!(
        x = rect.x,
        y = rect.y,
        w = rect.w,
        h = rect.h,
        "region selected"
    );

    let selection = Selection::Region(rect);
    let capturer = WlrCapturer::new()?;
    let images = capturer
        .capture(selection.clone(), cursor)
        .await
        .with_context(|| format!("capturing {selection:?}"))?;
    let stitched = crate::capture::region::stitch(&images, &selection)?;
    tracing::info!(
        width = stitched.width,
        height = stitched.height,
        "captured region for editor"
    );

    // BGRA (from screencopy) → RGBA so the canvas's `build_base_surface` swizzle path stays
    // honest. We do the conversion once here rather than teaching the canvas about both layouts.
    let base = base_from_captured(&stitched);
    editor::run_with_base(ctx, base, sinks).await
}

/// Convert a screencopy `CapturedImage` (BGRA, possibly with padded stride) into a tight RGBA
/// `DocumentBase` ready for the annotation canvas.
fn base_from_captured(img: &crate::capture::CapturedImage) -> DocumentBase {
    let w = img.width as usize;
    let h = img.height as usize;
    let row = w * 4;
    let stride = img.stride as usize;
    let mut rgba = vec![0u8; row * h];
    for y in 0..h {
        let src = &img.pixels[y * stride..y * stride + row];
        let dst = &mut rgba[y * row..(y + 1) * row];
        for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = s[3];
        }
    }
    DocumentBase {
        pixels: std::sync::Arc::from(rgba.into_boxed_slice()),
        width: img.width,
        height: img.height,
        stride: img.width * 4,
    }
}

/// Convenience accessor used by binary stubs that don't need the full editor yet.
#[allow(dead_code)]
pub fn placeholder_path() -> Option<PathBuf> {
    None
}
