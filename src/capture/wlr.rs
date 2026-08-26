//! `zwlr_screencopy_manager_v1`-based capture.
//!
//! The implementation uses `smithay-client-toolkit` for registry plumbing, output enumeration,
//! and the SHM pool helper. Each capture flow:
//!
//! 1. Bind the screencopy manager from the registry.
//! 2. For each target output, call `capture_output` (or `capture_output_region`).
//! 3. Wait for the `Buffer` event to learn `format`/`width`/`height`/`stride`.
//! 4. Allocate a buffer in our SHM pool, hand it to `copy`.
//! 5. On `Ready`, read pixels out of the SHM-mapped buffer.

use std::io::Write;
use std::os::fd::AsFd;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use smithay_client_toolkit::{
    dispatch2::Dispatch2,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_output, wl_registry, wl_shm},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use super::region::{Output, Rect, Selection};
use super::{CaptureError, CapturedImage, Capturer};

/// Native wlr-screencopy capturer.
///
/// This is a thin handle — each `capture` call opens its own Wayland connection so it can be
/// used both standalone (CLI) and from a GUI process without sharing display state.
pub struct WlrCapturer {
    _private: (),
}

impl WlrCapturer {
    pub fn new() -> Result<Self> {
        Ok(Self { _private: () })
    }
}

#[async_trait]
impl Capturer for WlrCapturer {
    async fn outputs(&self) -> Result<Vec<Output>> {
        tokio::task::spawn_blocking(enumerate_outputs)
            .await
            .map_err(|e| anyhow!("output enumeration task panicked: {e}"))?
    }

    async fn capture(&self, selection: Selection, cursor: bool) -> Result<Vec<CapturedImage>> {
        let sel = selection.clone();
        tokio::task::spawn_blocking(move || capture_blocking(sel, cursor))
            .await
            .map_err(|e| anyhow!("capture task panicked: {e}"))?
    }
}

fn enumerate_outputs() -> Result<Vec<Output>> {
    let conn = Connection::connect_to_env().context("connecting to wayland display")?;
    let (globals, mut queue) = registry_queue_init::<AppData>(&conn)?;
    let qh = queue.handle();
    let registry_state = RegistryState::new(&globals);
    let output_state = OutputState::new(&globals, &qh);
    let shm = Shm::bind(&globals, &qh).context("binding wl_shm")?;
    let manager: ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 1..=3, ManagerData)
        .map_err(|_| CaptureError::UnsupportedCompositor)?;

    let mut data = AppData {
        registry_state,
        output_state,
        shm,
        manager,
        frames: Vec::new(),
        pool: None,
    };

    // Round-trip so outputs propagate.
    queue.roundtrip(&mut data)?;
    queue.roundtrip(&mut data)?;

    Ok(data
        .output_state
        .outputs()
        .filter_map(|o| {
            let info = data.output_state.info(&o)?;
            let (x, y) = info.logical_position.unwrap_or((0, 0));
            let (w, h) = info
                .logical_size
                .map(|(w, h)| (w as u32, h as u32))
                .unwrap_or_else(|| {
                    info.modes
                        .iter()
                        .find(|m| m.current)
                        .map(|m| (m.dimensions.0 as u32, m.dimensions.1 as u32))
                        .unwrap_or((0, 0))
                });
            Some(Output {
                name: info.name.unwrap_or_else(|| "unknown".to_owned()),
                logical: Rect { x, y, w, h },
                scale: info.scale_factor,
            })
        })
        .collect())
}

fn capture_blocking(selection: Selection, cursor: bool) -> Result<Vec<CapturedImage>> {
    let conn = Connection::connect_to_env().context("connecting to wayland display")?;
    let (globals, mut queue) = registry_queue_init::<AppData>(&conn)?;
    let qh = queue.handle();
    let registry_state = RegistryState::new(&globals);
    let output_state = OutputState::new(&globals, &qh);
    let shm = Shm::bind(&globals, &qh).context("binding wl_shm")?;
    let manager: ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 1..=3, ManagerData)
        .map_err(|_| CaptureError::UnsupportedCompositor)?;

    let mut data = AppData {
        registry_state,
        output_state,
        shm,
        manager,
        frames: Vec::new(),
        pool: None,
    };

    queue.roundtrip(&mut data)?;
    queue.roundtrip(&mut data)?;

    let targets = resolve_targets(&data, &selection)?;
    if targets.is_empty() {
        return Err(CaptureError::NoMatchingOutput(format!("{:?}", selection)).into());
    }

    // Request frames.
    for (wl_output, _output) in &targets {
        let cursor_flag: i32 = if cursor { 1 } else { 0 };
        let frame = data
            .manager
            .capture_output(cursor_flag, wl_output, &qh, FrameUserData);
        data.frames.push(FrameSlot::new(frame));
    }

    // Drive until we have Buffer events for every frame.
    while data.frames.iter().any(|f| f.format.is_none() && !f.failed) {
        queue.blocking_dispatch(&mut data)?;
    }
    if let Some(f) = data.frames.iter().find(|f| f.failed) {
        bail!("compositor failed initial frame negotiation: {:?}", f.error);
    }

    // Allocate SHM buffers and submit copies.
    let total_bytes: usize = data
        .frames
        .iter()
        .map(|f| (f.stride * f.height) as usize)
        .sum();
    let pool = SlotPool::new(total_bytes.max(4096), &data.shm).context("creating SHM pool")?;
    data.pool = Some(pool);

    for slot in data.frames.iter_mut() {
        let pool = data.pool.as_mut().expect("pool just installed");
        let format = wl_shm_format(slot.format.expect("buffer event set the format"));
        let (buffer, _canvas) = pool
            .create_buffer(
                slot.width as i32,
                slot.height as i32,
                slot.stride as i32,
                format,
            )
            .context("creating SHM buffer")?;
        slot.frame.copy(buffer.wl_buffer());
        slot.buffer = Some(buffer);
    }

    while data.frames.iter().any(|f| !f.done && !f.failed) {
        queue.blocking_dispatch(&mut data)?;
    }

    // Collect results.
    let mut results = Vec::with_capacity(data.frames.len());
    for (i, slot) in data.frames.iter().enumerate() {
        if slot.failed {
            bail!("frame copy failed for target {i}: {:?}", slot.error);
        }
        let pool = data.pool.as_mut().expect("pool present");
        let canvas = pool
            .canvas(slot.buffer.as_ref().expect("buffer present"))
            .ok_or_else(|| anyhow!("SHM canvas not available for frame {i}"))?;
        let pixels: Arc<[u8]> = Arc::from(canvas.to_vec().into_boxed_slice());
        let (_, output) = &targets[i];
        results.push(CapturedImage {
            width: slot.width,
            height: slot.height,
            stride: slot.stride,
            pixels,
            source: Some(output.clone()),
        });
    }
    Ok(results)
}

/// Fallback name for an output the compositor never named. Kept as a constant so the
/// `Selection::Output` match and the tests agree on it.
const UNNAMED_OUTPUT: &str = "unknown";

/// Compositor-aware selection variants must be resolved upstream (see
/// `cli::screenshot::resolve_selection`). If one reaches capture, that's a bug: capture has
/// no window-manager IPC of its own (see `crate::wm`).
///
/// Split out of [`resolve_targets`] so the guard is exercised without a Wayland connection.
fn ensure_resolvable(selection: &Selection) -> Result<()> {
    match selection {
        Selection::Focused | Selection::Window | Selection::Interactive => bail!(
            "internal: capture received an unresolved selection {:?}; resolve it via cli::screenshot::resolve_selection first",
            selection
        ),
        _ => Ok(()),
    }
}

/// Build an [`Output`] descriptor from the fields sctk reports for a `wl_output`.
///
/// Takes primitives rather than an `OutputInfo` so it is constructible in tests without a
/// live registry. Missing geometry degrades to a zero rect at the origin, which
/// [`want_output`] then treats as intersecting nothing.
fn descriptor_from_info(
    name: Option<&str>,
    logical_position: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
    scale: i32,
) -> Output {
    let (x, y) = logical_position.unwrap_or((0, 0));
    let (w, h) = logical_size.unwrap_or((0, 0));
    Output {
        name: name.unwrap_or(UNNAMED_OUTPUT).to_owned(),
        logical: Rect {
            x,
            y,
            w: w as u32,
            h: h as u32,
        },
        scale,
    }
}

/// The selection → output matching policy, in one pure place.
///
/// The unresolved variants return `false` rather than panicking: [`ensure_resolvable`] has
/// already rejected them at the top of [`resolve_targets`], and a defensive `false` beats an
/// `unreachable!()` that would abort a capture if that ordering ever changed.
fn want_output(selection: &Selection, descriptor: &Output) -> bool {
    match selection {
        Selection::Full | Selection::PerOutput => true,
        Selection::Output(target) => target == &descriptor.name,
        Selection::Region(rect) => rect.intersect(&descriptor.logical).is_some(),
        Selection::Focused | Selection::Window | Selection::Interactive => false,
    }
}

fn resolve_targets(
    data: &AppData,
    selection: &Selection,
) -> Result<Vec<(wl_output::WlOutput, Output)>> {
    ensure_resolvable(selection)?;

    let mut out = Vec::new();
    for wl_output in data.output_state.outputs() {
        let Some(info) = data.output_state.info(&wl_output) else {
            continue;
        };
        let descriptor = descriptor_from_info(
            info.name.as_deref(),
            info.logical_position,
            info.logical_size,
            info.scale_factor,
        );
        if want_output(selection, &descriptor) {
            out.push((wl_output, descriptor));
        }
    }
    Ok(out)
}

fn wl_shm_format(fourcc: u32) -> wl_shm::Format {
    // The frame `Buffer` event reports a wl_shm format encoded as u32.
    wl_shm::Format::try_from(fourcc).unwrap_or(wl_shm::Format::Xrgb8888)
}

// ---------------------------------------------------------------------------
// sctk plumbing
// ---------------------------------------------------------------------------

struct AppData {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    manager: ZwlrScreencopyManagerV1,
    frames: Vec<FrameSlot>,
    pool: Option<SlotPool>,
}

/// User data attached to the bound `zwlr_screencopy_manager_v1` global.
#[derive(Default)]
struct ManagerData;

#[derive(Default)]
struct FrameUserData;

struct FrameSlot {
    frame: ZwlrScreencopyFrameV1,
    format: Option<u32>,
    width: u32,
    height: u32,
    stride: u32,
    buffer: Option<smithay_client_toolkit::shm::slot::Buffer>,
    done: bool,
    failed: bool,
    error: Option<String>,
}

impl FrameSlot {
    fn new(frame: ZwlrScreencopyFrameV1) -> Self {
        Self {
            frame,
            format: None,
            width: 0,
            height: 0,
            stride: 0,
            buffer: None,
            done: false,
            failed: false,
            error: None,
        }
    }
}

impl ProvidesRegistryState for AppData {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

impl OutputHandler for AppData {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for AppData {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl Dispatch2<ZwlrScreencopyManagerV1, AppData> for ManagerData {
    fn event(
        &self,
        _: &mut AppData,
        _: &ZwlrScreencopyManagerV1,
        _: <ZwlrScreencopyManagerV1 as wayland_client::Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<AppData>,
    ) {
    }
}

impl Dispatch2<ZwlrScreencopyFrameV1, AppData> for FrameUserData {
    fn event(
        &self,
        state: &mut AppData,
        frame: &ZwlrScreencopyFrameV1,
        event: <ZwlrScreencopyFrameV1 as wayland_client::Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<AppData>,
    ) {
        let Some(slot) = state.frames.iter_mut().find(|s| &s.frame == frame) else {
            return;
        };
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                slot.format = Some(format.into_result().map(|f| f as u32).unwrap_or(0));
                slot.width = width;
                slot.height = height;
                slot.stride = stride;
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                slot.done = true;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                slot.failed = true;
                slot.error = Some("compositor reported frame failure".to_owned());
            }
            _ => {}
        }
    }
}

smithay_client_toolkit::delegate_registry!(AppData);
smithay_client_toolkit::delegate_dispatch2!(AppData);

// Quiet unused-imports warnings when this module is only stubbed.
#[allow(dead_code)]
fn _unused(_: wl_registry::WlRegistry, _: wl_buffer::WlBuffer, _: &dyn AsFd, _: &dyn Write) {}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    fn out(name: &str, x: i32, y: i32, w: u32, h: u32) -> Output {
        Output {
            name: name.to_owned(),
            logical: Rect { x, y, w, h },
            scale: 1,
        }
    }

    #[rstest]
    #[case(Selection::Focused)]
    #[case(Selection::Window)]
    #[case(Selection::Interactive)]
    fn ensure_resolvable_rejects_compositor_aware_selections(#[case] selection: Selection) {
        let err = ensure_resolvable(&selection).unwrap_err();
        // The message names the offending variant and points at the resolver, because this
        // only ever fires as an internal wiring bug.
        assert!(err.to_string().contains("unresolved selection"), "{err}");
        assert!(err.to_string().contains("resolve_selection"), "{err}");
    }

    #[rstest]
    #[case(Selection::Full)]
    #[case(Selection::PerOutput)]
    #[case(Selection::Output("DP-1".into()))]
    #[case(Selection::Region(Rect { x: 0, y: 0, w: 10, h: 10 }))]
    fn ensure_resolvable_accepts_concrete_selections(#[case] selection: Selection) {
        assert!(ensure_resolvable(&selection).is_ok());
    }

    #[test]
    fn descriptor_from_info_maps_every_field() {
        let d = descriptor_from_info(Some("DP-1"), Some((100, 200)), Some((1920, 1080)), 2);
        assert_eq!(
            d,
            Output {
                name: "DP-1".into(),
                logical: Rect {
                    x: 100,
                    y: 200,
                    w: 1920,
                    h: 1080
                },
                scale: 2,
            }
        );
    }

    #[test]
    fn descriptor_from_info_falls_back_for_an_unnamed_output() {
        let d = descriptor_from_info(None, None, None, 1);
        assert_eq!(d.name, UNNAMED_OUTPUT);
        // No geometry reported: a zero rect at the origin, which intersects nothing.
        assert_eq!(
            d.logical,
            Rect {
                x: 0,
                y: 0,
                w: 0,
                h: 0
            }
        );
    }

    #[test]
    fn an_unnamed_output_is_still_addressable_by_the_fallback_name() {
        let d = descriptor_from_info(None, Some((0, 0)), Some((800, 600)), 1);
        assert!(want_output(&Selection::Output(UNNAMED_OUTPUT.into()), &d));
    }

    #[rstest]
    #[case(Selection::Full, true)]
    #[case(Selection::PerOutput, true)]
    fn full_and_per_output_take_every_output(#[case] selection: Selection, #[case] expected: bool) {
        assert_eq!(
            want_output(&selection, &out("DP-1", 0, 0, 1920, 1080)),
            expected
        );
        assert_eq!(
            want_output(&selection, &out("HDMI-A-1", 1920, 0, 1280, 720)),
            expected
        );
    }

    #[rstest]
    #[case("DP-1", true)]
    #[case("HDMI-A-1", false)]
    // Output names are matched exactly — no prefix or case folding.
    #[case("DP-11", false)]
    #[case("dp-1", false)]
    #[case("", false)]
    fn output_selection_matches_by_exact_name(#[case] target: &str, #[case] expected: bool) {
        let d = out("DP-1", 0, 0, 1920, 1080);
        assert_eq!(want_output(&Selection::Output(target.into()), &d), expected);
    }

    #[rstest]
    // Fully inside.
    #[case(Rect { x: 10, y: 10, w: 100, h: 100 }, true)]
    // Straddling the right edge.
    #[case(Rect { x: 1900, y: 0, w: 100, h: 100 }, true)]
    // Covering the whole output and more.
    #[case(Rect { x: -100, y: -100, w: 4000, h: 4000 }, true)]
    // Entirely to the right.
    #[case(Rect { x: 1920, y: 0, w: 100, h: 100 }, false)]
    // Entirely below.
    #[case(Rect { x: 0, y: 1080, w: 100, h: 100 }, false)]
    // Entirely to the left.
    #[case(Rect { x: -100, y: 0, w: 100, h: 100 }, false)]
    // Touching the edge only: rects are half-open, so adjacency is not an intersection.
    #[case(Rect { x: 1919, y: 0, w: 1, h: 1 }, true)]
    fn region_selection_matches_by_intersection(#[case] region: Rect, #[case] expected: bool) {
        let d = out("DP-1", 0, 0, 1920, 1080);
        assert_eq!(want_output(&Selection::Region(region), &d), expected);
    }

    #[test]
    fn region_selection_picks_only_the_overlapped_output_in_a_dual_head_layout() {
        let left = out("DP-1", 0, 0, 1920, 1080);
        let right = out("HDMI-A-1", 1920, 0, 1280, 720);
        // A region wholly inside the right-hand monitor.
        let region = Selection::Region(Rect {
            x: 2000,
            y: 100,
            w: 200,
            h: 200,
        });
        assert!(!want_output(&region, &left));
        assert!(want_output(&region, &right));
    }

    #[rstest]
    #[case(Selection::Focused)]
    #[case(Selection::Window)]
    #[case(Selection::Interactive)]
    fn unresolved_selections_match_nothing_rather_than_panicking(#[case] selection: Selection) {
        // `ensure_resolvable` rejects these first; this asserts the defensive fallback so a
        // future reordering degrades to "no targets" instead of aborting the capture.
        assert!(!want_output(&selection, &out("DP-1", 0, 0, 1920, 1080)));
    }

    #[rstest]
    #[case(wl_shm::Format::Xrgb8888)]
    #[case(wl_shm::Format::Argb8888)]
    #[case(wl_shm::Format::Xbgr8888)]
    #[case(wl_shm::Format::Abgr8888)]
    fn wl_shm_format_round_trips_known_formats(#[case] format: wl_shm::Format) {
        assert_eq!(wl_shm_format(format as u32), format);
    }

    #[test]
    fn wl_shm_format_falls_back_to_xrgb8888_for_an_unknown_fourcc() {
        assert_eq!(wl_shm_format(u32::MAX), wl_shm::Format::Xrgb8888);
    }
}
