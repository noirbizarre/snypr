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
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle,
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
        .bind(&qh, 1..=3, ())
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
        .bind(&qh, 1..=3, ())
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
        let format = wl_shm_format(slot.format.unwrap());
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

fn resolve_targets(
    data: &AppData,
    selection: &Selection,
) -> Result<Vec<(wl_output::WlOutput, Output)>> {
    // Compositor-aware variants must be resolved upstream (see cli::screenshot::resolve_selection).
    // If one reaches us, that's a bug: capture has no Hyprland IPC of its own.
    match selection {
        Selection::Focused | Selection::Window | Selection::Interactive => {
            bail!(
                "internal: capture received an unresolved selection {:?}; resolve it via cli::screenshot::resolve_selection first",
                selection
            );
        }
        _ => {}
    }

    let mut out = Vec::new();
    for wl_output in data.output_state.outputs() {
        let Some(info) = data.output_state.info(&wl_output) else {
            continue;
        };
        let name = info.name.clone().unwrap_or_else(|| "unknown".to_owned());
        let (x, y) = info.logical_position.unwrap_or((0, 0));
        let (w, h) = info.logical_size.unwrap_or((0, 0));
        let descriptor = Output {
            name: name.clone(),
            logical: Rect {
                x,
                y,
                w: w as u32,
                h: h as u32,
            },
            scale: info.scale_factor,
        };
        let want = match selection {
            Selection::Full | Selection::PerOutput => true,
            Selection::Output(target) => target == &name,
            Selection::Region(rect) => rect.intersect(&descriptor.logical).is_some(),
            // Already bailed above.
            Selection::Focused | Selection::Window | Selection::Interactive => unreachable!(),
        };
        if want {
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

impl Dispatch<ZwlrScreencopyManagerV1, ()> for AppData {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: <ZwlrScreencopyManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, FrameUserData> for AppData {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: <ZwlrScreencopyFrameV1 as wayland_client::Proxy>::Event,
        _: &FrameUserData,
        _: &Connection,
        _: &QueueHandle<Self>,
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

smithay_client_toolkit::delegate_output!(AppData);
smithay_client_toolkit::delegate_shm!(AppData);
smithay_client_toolkit::delegate_registry!(AppData);

// Quiet unused-imports warnings when this module is only stubbed.
#[allow(dead_code)]
fn _unused(_: wl_registry::WlRegistry, _: wl_buffer::WlBuffer, _: &dyn AsFd, _: &dyn Write) {}
