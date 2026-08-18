//! End-to-end capture tests that need a live, `wlr-screencopy`-capable compositor.
//!
//! Gated behind the `integration-wayland` feature. Everything here drives the real
//! `zwlr_screencopy_manager_v1` path in `src/capture/wlr.rs`, which no unit test can reach.
//!
//! # Running these
//!
//! ```sh
//! cargo test --features integration-wayland --test wayland
//! ```
//!
//! from a session on wlroots-based compositor (Hyprland, sway, river, …).
//!
//! # Why CI does not gate on them
//!
//! CI's headless compositor is Weston, which implements its own `weston_screenshooter`
//! rather than the wlroots `zwlr_screencopy_manager_v1` protocol. So these tests **skip**
//! themselves when the protocol is not advertised, exactly like `require_gtk!()` does for a
//! missing `GdkDisplay`. Set `SNYPR_REQUIRE_WAYLAND_CAPTURE` to a truthy value to turn that
//! skip into a hard failure — do that locally, or from a CI job running a real wlroots
//! compositor, so a broken capture path cannot hide behind a silent skip.
#![cfg(feature = "integration-wayland")]

use snypr::capture::region::{Rect, stitch};
use snypr::capture::wlr::WlrCapturer;
use snypr::capture::{Capturer, Output, Selection};

/// Whether a missing compositor must fail the run rather than skip it.
fn capture_is_required() -> bool {
    match std::env::var("SNYPR_REQUIRE_WAYLAND_CAPTURE") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no"
        ),
        Err(_) => false,
    }
}

/// Connect and enumerate outputs, or report that this environment cannot serve us.
///
/// `WlrCapturer::new` is infallible today (it defers every Wayland call), so the real
/// availability probe is the first `outputs()` round-trip: that is where binding
/// `zwlr_screencopy_manager_v1` fails on a compositor that does not implement it.
async fn outputs_or_skip() -> Option<(WlrCapturer, Vec<Output>)> {
    let cap = match WlrCapturer::new() {
        Ok(c) => c,
        Err(err) => return skip(format_args!("capturer could not start: {err:#}")),
    };
    let outputs = match cap.outputs().await {
        Ok(o) => o,
        Err(err) => return skip(format_args!("no wlr-screencopy support: {err:#}")),
    };
    if outputs.is_empty() {
        return skip(format_args!("compositor reported no outputs"));
    }
    Some((cap, outputs))
}

/// Emit the skip notice, or turn it into a failure when the caller demanded a compositor.
fn skip<T>(reason: std::fmt::Arguments<'_>) -> Option<T> {
    assert!(
        !capture_is_required(),
        "SNYPR_REQUIRE_WAYLAND_CAPTURE is set but {reason}"
    );
    eprintln!("skipping wayland integration test: {reason}");
    None
}

/// Bind `(capturer, outputs)` from a live compositor, or return from the enclosing test.
///
/// Like `require_gtk!()`, this expands to a bare `return`, so it only works in tests
/// returning `()`.
macro_rules! require_capture {
    () => {
        match outputs_or_skip().await {
            Some(pair) => pair,
            None => return,
        }
    };
}

#[tokio::test]
async fn outputs_are_enumerable_and_have_sane_geometry() {
    let (cap, _outputs) = require_capture!();
    let Ok(outputs) = cap.outputs().await else {
        assert!(!capture_is_required(), "output enumeration failed");
        eprintln!("compositor does not advertise wlr-screencopy, skipping");
        return;
    };
    assert!(!outputs.is_empty(), "a live session must have an output");
    for o in &outputs {
        assert!(!o.name.is_empty(), "unnamed output: {o:?}");
        assert!(o.logical.w > 0 && o.logical.h > 0, "empty output: {o:?}");
        assert!(o.scale >= 1, "non-positive scale: {o:?}");
    }
}

#[tokio::test]
async fn every_output_can_be_captured_and_the_buffer_matches_its_geometry() {
    let (cap, outputs) = require_capture!();
    let Some(target) = outputs.first() else {
        return;
    };
    let images = cap
        .capture(Selection::Output(target.name.clone()), false)
        .await
        .expect("named-output capture");
    let img = &images[0];
    assert!(img.width > 0 && img.height > 0);
    // BGRA8888: four bytes per pixel, and the buffer must actually hold them.
    assert!(
        img.stride >= img.width * 4,
        "stride {} too small",
        img.stride
    );
    assert_eq!(
        img.pixels.len(),
        img.stride as usize * img.height as usize,
        "pixel buffer does not match its stride and height"
    );
    // Physical pixels are logical times the scale factor.
    let scale = target.scale.max(1) as u32;
    assert_eq!(img.width, target.logical.w * scale);
    assert_eq!(img.height, target.logical.h * scale);
}

#[tokio::test]
async fn per_output_returns_one_image_per_output() {
    let (cap, outputs) = require_capture!();
    let images = cap
        .capture(Selection::PerOutput, false)
        .await
        .expect("per-output capture");
    assert_eq!(images.len(), outputs.len());
    for img in &images {
        // Every per-output image carries its provenance; only stitched buffers have `None`.
        let source = img.source.as_ref().expect("per-output image has a source");
        assert!(outputs.iter().any(|o| o.name == source.name));
    }
}

#[tokio::test]
async fn capturing_a_named_output_returns_just_that_one() {
    let (cap, outputs) = require_capture!();
    let Some(target) = outputs.first() else {
        return;
    };
    let images = cap
        .capture(Selection::Output(target.name.clone()), false)
        .await
        .expect("named-output capture");
    assert_eq!(images.len(), 1);
    assert_eq!(
        images[0].source.as_ref().map(|o| o.name.as_str()),
        Some(target.name.as_str())
    );
}

#[tokio::test]
async fn capturing_an_unknown_output_name_fails_rather_than_capturing_everything() {
    let (cap, _outputs) = require_capture!();
    // Silently falling back to a full-desktop capture would leak whatever is on screen into
    // a file the user asked to contain one monitor.
    let err = cap
        .capture(Selection::Output("NO-SUCH-OUTPUT-9999".into()), false)
        .await
        .expect_err("an unknown output must be an error");
    assert!(!err.to_string().is_empty());
}

#[tokio::test]
async fn a_region_capture_returns_the_intersecting_outputs_uncropped() {
    let (cap, outputs) = require_capture!();
    let Some(first) = outputs.first() else {
        return;
    };
    // A small rectangle wholly inside the first output.
    let region = Rect {
        x: first.logical.x + 1,
        y: first.logical.y + 1,
        w: (first.logical.w / 4).max(2),
        h: (first.logical.h / 4).max(2),
    };
    let images = cap
        .capture(Selection::Region(region), false)
        .await
        .expect("region capture");
    // The capture backend deliberately returns *whole* outputs: cropping to the requested
    // rectangle is `region::stitch`'s job (see cli::screenshot::execute). Asserting that here
    // pins the layering down, because a crop that silently moved into the backend would make
    // `stitch` double-crop.
    assert_eq!(images.len(), 1, "the region only touches one output");
    let scale = first.scale.max(1) as u32;
    assert_eq!(images[0].width, first.logical.w * scale);
}

#[tokio::test]
async fn capture_then_stitch_crops_to_the_requested_region() {
    let (cap, outputs) = require_capture!();
    let Some(first) = outputs.first() else {
        return;
    };
    let region = Rect {
        x: first.logical.x + 1,
        y: first.logical.y + 1,
        w: (first.logical.w / 4).max(2),
        h: (first.logical.h / 4).max(2),
    };
    let selection = Selection::Region(region);
    let images = cap
        .capture(selection.clone(), false)
        .await
        .expect("capture");
    // The full pipeline the CLI runs (cli::screenshot::execute).
    let stitched = stitch(&images, &selection).expect("stitch");
    assert_eq!(stitched.width, region.w, "stitch did not crop the width");
    assert_eq!(stitched.height, region.h, "stitch did not crop the height");
    assert_eq!(
        stitched.pixels.len(),
        stitched.stride as usize * stitched.height as usize
    );
    // A stitched buffer is synthetic and has no single source output.
    assert!(stitched.source.is_none());
}

#[tokio::test]
async fn unresolved_selections_are_rejected_by_the_capture_backend() {
    let (cap, _outputs) = require_capture!();
    // These must be resolved by `cli::screenshot::resolve_selection` before reaching capture;
    // capture has no Hyprland IPC of its own. Reaching here is a wiring bug, and the error
    // message says so.
    for selection in [
        Selection::Focused,
        Selection::Window,
        Selection::Interactive,
    ] {
        let err = cap
            .capture(selection.clone(), false)
            .await
            .expect_err("unresolved selection must be rejected");
        assert!(
            err.to_string().contains("unresolved selection"),
            "unexpected error for {selection:?}: {err:#}"
        );
    }
}
