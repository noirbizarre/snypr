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
use snypr::wm::WmBackend;
use snypr::wm::foreign_toplevel::ForeignToplevel;

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

/// `WlrCapturer::probe_pixel_formats` drives the same `zwlr_screencopy_frame_v1` Buffer-event
/// negotiation `capture()` does, but stops short of allocating/copying — this is the only
/// place that exercises it, since it's specifically meant to run without a live editor/save
/// flow (see `doctor`'s "Wayland capture" section).
#[tokio::test]
async fn probe_pixel_formats_reports_every_output_with_a_recognized_format() {
    let (cap, outputs) = require_capture!();
    let Ok(formats) = cap.probe_pixel_formats().await else {
        assert!(!capture_is_required(), "pixel-format probe failed");
        eprintln!("compositor does not advertise wlr-screencopy, skipping");
        return;
    };
    assert_eq!(
        formats.len(),
        outputs.len(),
        "expected one negotiated format per enumerated output"
    );
    for (name, fourcc) in &formats {
        assert!(
            outputs.iter().any(|o| &o.name == name),
            "probe reported an output `{name}` that `outputs()` never listed"
        );
        // Note: 0 is a legitimate fourcc (Argb8888, the wl_shm default) — not a sentinel for
        // "missing" — so the real sanity check is that it decodes to a known wl_shm format,
        // not that it's non-zero.
        assert!(
            wayland_client::protocol::wl_shm::Format::try_from(*fourcc).is_ok(),
            "output `{name}` reported an unrecognized fourcc 0x{fourcc:08x}"
        );
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
    // capture has no window-manager IPC of its own (see `crate::wm`). Reaching here is a
    // wiring bug, and the error message says so.
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

// ---------------------------------------------------------------------------
// `crate::wm::foreign_toplevel` — needs a compositor advertising
// `zwlr_foreign_toplevel_manager_v1` specifically, independent of `zwlr_screencopy_manager_v1`
// above: nothing here reuses `require_capture!()`, since a compositor could implement one
// protocol without the other.
// ---------------------------------------------------------------------------

/// Whether a compositor not advertising `zwlr_foreign_toplevel_manager_v1` must fail the run
/// rather than skip it. Named distinctly from `SNYPR_REQUIRE_WAYLAND_CAPTURE` because a given
/// test compositor may support one protocol and not the other.
fn wm_is_required() -> bool {
    match std::env::var("SNYPR_REQUIRE_WAYLAND_WM") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no"
        ),
        Err(_) => false,
    }
}

/// Emit the skip notice, or turn it into a failure when the caller demanded the protocol.
fn skip_wm(reason: std::fmt::Arguments<'_>) {
    assert!(
        !wm_is_required(),
        "SNYPR_REQUIRE_WAYLAND_WM is set but {reason}"
    );
    eprintln!("skipping wlr-foreign-toplevel integration test: {reason}");
}

#[tokio::test]
async fn foreign_toplevel_focused_output_names_a_real_output() {
    let backend = ForeignToplevel;
    let name = match backend.focused_output().await {
        Ok(n) => n,
        Err(err) => return skip_wm(format_args!("no focused output: {err:#}")),
    };
    // Cross-check against the same compositor's `zwlr_screencopy` output list — both
    // protocols must agree on what a real output is called.
    let Some((_cap, outputs)) = outputs_or_skip().await else {
        // `zwlr_screencopy` unsupported here even though foreign-toplevel is: still a useful
        // partial result, just nothing to cross-check against.
        assert!(!name.is_empty(), "empty output name");
        return;
    };
    assert!(
        outputs.iter().any(|o| o.name == name),
        "focused_output() returned {name:?}, not among the outputs {outputs:?}"
    );
}

#[tokio::test]
async fn foreign_toplevel_clients_never_report_geometry() {
    let backend = ForeignToplevel;
    let clients = match backend.clients().await {
        Ok(c) => c,
        Err(err) => return skip_wm(format_args!("clients() failed: {err:#}")),
    };
    // The protocol has no position/size at all — asserting `None` here pins that down for
    // real, rather than just at the unit-test level.
    for w in &clients {
        assert!(w.at.is_none(), "unexpected geometry on {w:?}", w = w.title);
        assert!(
            w.size.is_none(),
            "unexpected geometry on {w:?}",
            w = w.title
        );
        assert_eq!(
            w.workspace_id,
            -1,
            "unexpected workspace id on {w:?}",
            w = w.title
        );
    }
}

#[tokio::test]
async fn foreign_toplevel_active_window_has_no_geometry_either() {
    let backend = ForeignToplevel;
    let win = match backend.active_window().await {
        Ok(w) => w,
        Err(err) => return skip_wm(format_args!("active_window() failed: {err:#}")),
    };
    assert!(
        win.rect().is_none(),
        "unexpected geometry on {win:?}",
        win = win.title
    );
}

#[tokio::test]
async fn foreign_toplevel_subscribe_focus_publishes_and_stops_on_shutdown() {
    let backend = ForeignToplevel;
    let handle = tokio::runtime::Handle::current();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let mut rx = backend.subscribe_focus(&handle, shutdown_rx);
    if rx.changed().await.is_err() {
        // The task exited immediately: the compositor doesn't advertise the protocol at all
        // (matches the same "no backend" outcome `probe()` would report).
        return skip_wm(format_args!(
            "subscribe_focus's background task exited without publishing a value"
        ));
    }
    // Shutdown must be observed promptly (bounded by the next Wayland event at worst — see
    // the module docs), not linger for the rest of the process.
    drop(shutdown_tx);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while rx.changed().await.is_ok() {}
    })
    .await
    .expect("subscribe_focus did not stop within 5s of shutdown firing");
}
