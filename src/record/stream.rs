//! Consumes the PipeWire video stream negotiated by [`super::portal`] on a dedicated OS
//! thread — PipeWire's main loop is a plain C event loop, not `Send`/async-friendly, so it
//! cannot run on a tokio worker — and forwards decoded frames to a [`tokio::sync::watch`]
//! channel (we only ever care about the *latest* frame; the portal delivers frames on
//! damage, not on a fixed clock, so [`super::run`] pulls from this channel on its own timer
//! and duplicates the last frame when nothing new has arrived).

use std::os::fd::OwnedFd;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use pipewire as pw;
use pw::spa;

use crate::capture::region::{Rect, slice_pixels};

/// 32-bits-per-pixel raw formats this module is willing to negotiate. Restricted to
/// 4-byte-per-pixel layouts so frame cropping/compaction can reuse
/// [`crate::capture::region::slice_pixels`] (which hardcodes 4 bytes/pixel) unmodified —
/// screen-content formats offered by wlr-screencopy-backed portals are 32bpp in practice, so
/// this isn't a real-world limitation today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawPixelFormat {
    Rgbx,
    Bgrx,
    XRgb,
    XBgr,
    Rgba,
    Bgra,
    Argb,
    Abgr,
}

impl RawPixelFormat {
    /// ffmpeg `-pix_fmt` name understood by its `rawvideo` demuxer.
    pub fn ffmpeg_pix_fmt(self) -> &'static str {
        match self {
            RawPixelFormat::Rgbx => "rgb0",
            RawPixelFormat::Bgrx => "bgr0",
            RawPixelFormat::XRgb => "0rgb",
            RawPixelFormat::XBgr => "0bgr",
            RawPixelFormat::Rgba => "rgba",
            RawPixelFormat::Bgra => "bgra",
            RawPixelFormat::Argb => "argb",
            RawPixelFormat::Abgr => "abgr",
        }
    }

    fn from_spa(fmt: spa::param::video::VideoFormat) -> Option<Self> {
        use spa::param::video::VideoFormat as F;
        Some(match fmt {
            F::RGBx => RawPixelFormat::Rgbx,
            F::BGRx => RawPixelFormat::Bgrx,
            F::xRGB => RawPixelFormat::XRgb,
            F::xBGR => RawPixelFormat::XBgr,
            F::RGBA => RawPixelFormat::Rgba,
            F::BGRA => RawPixelFormat::Bgra,
            F::ARGB => RawPixelFormat::Argb,
            F::ABGR => RawPixelFormat::Abgr,
            _ => return None,
        })
    }
}

/// A single decoded video frame, already cropped and compacted to a tight `width * 4` byte
/// stride (no source padding). `pixels` is `Arc`-shared, mirroring
/// [`crate::capture::CapturedImage`], so duplicating a frame across encoder ticks (when the
/// portal hasn't sent a new one) is cheap.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub format: RawPixelFormat,
    pub pixels: Arc<[u8]>,
}

/// Sent over the PipeWire channel to ask the consumer thread's main loop to quit.
struct Terminate;

/// Handle to the background PipeWire consumer thread.
pub struct StreamHandle {
    terminate: pw::channel::Sender<Terminate>,
    join: std::thread::JoinHandle<()>,
}

impl StreamHandle {
    /// Signal the PipeWire loop to quit and block until the consumer thread has actually
    /// exited, so the fd/session are guaranteed released before the caller tears down the
    /// portal session.
    pub fn stop(self) {
        let _ = self.terminate.send(Terminate);
        if let Err(err) = self.join.join() {
            tracing::warn!(?err, "pipewire consumer thread panicked");
        }
    }
}

/// State threaded through the stream's callbacks (all run on the same single-threaded
/// PipeWire loop, so no synchronization is needed between them).
struct StreamData {
    format: spa::param::video::VideoInfoRaw,
    tx: tokio::sync::watch::Sender<Option<Frame>>,
    crop: Option<Rect>,
}

/// Spawn the PipeWire consumer thread for the stream at `node_id` on the connection behind
/// `fd` (both from [`super::portal::negotiate`]). Frames land on `tx` as they arrive; `crop`,
/// when set, is applied — and the buffer compacted to a tight stride either way — before
/// sending.
pub fn spawn(
    fd: OwnedFd,
    node_id: u32,
    crop: Option<Rect>,
    tx: tokio::sync::watch::Sender<Option<Frame>>,
) -> Result<StreamHandle> {
    let (terminate, term_rx) = pw::channel::channel::<Terminate>();
    let join = std::thread::Builder::new()
        .name("snypr-pipewire".to_owned())
        .spawn(move || {
            if let Err(err) = run(fd, node_id, crop, tx, term_rx) {
                tracing::warn!(error = ?err, "pipewire video stream thread exited with an error");
            }
        })
        .context("spawning the pipewire consumer thread")?;
    Ok(StreamHandle { terminate, join })
}

fn run(
    fd: OwnedFd,
    node_id: u32,
    crop: Option<Rect>,
    tx: tokio::sync::watch::Sender<Option<Frame>>,
    term_rx: pw::channel::Receiver<Terminate>,
) -> Result<()> {
    let mainloop = pw::main_loop::MainLoopRc::new(None).context("creating pipewire main loop")?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).context("creating pipewire context")?;
    let core = context
        .connect_fd_rc(fd, None)
        .context("connecting pipewire core to the portal-provided remote")?;

    let data = StreamData {
        format: Default::default(),
        tx,
        crop,
    };

    let stream = {
        use pipewire::properties::properties;
        pw::stream::StreamRc::new(
            core,
            "snypr-record",
            properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
        )
        .context("creating pipewire stream")?
    };

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, _, old, new| {
            tracing::debug!(?old, ?new, "pipewire stream state changed");
        })
        .param_changed(|_, data, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != spa::param::format::MediaType::Video
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            if let Err(err) = data.format.parse(param) {
                tracing::warn!(error = ?err, "failed to parse negotiated pipewire video format");
                return;
            }
            let size = data.format.size();
            tracing::info!(
                width = size.width,
                height = size.height,
                format = ?data.format.format(),
                "pipewire video format negotiated"
            );
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(plane) = datas.first_mut() else {
                return;
            };
            let chunk_size = plane.chunk().size() as usize;
            let stride = plane.chunk().stride().max(0) as u32;
            let offset = plane.chunk().offset() as usize;
            if chunk_size == 0 || stride == 0 {
                return;
            }
            let Some(format) = RawPixelFormat::from_spa(data.format.format()) else {
                tracing::warn!(
                    format = ?data.format.format(),
                    "compositor negotiated an unsupported pixel format; dropping frame"
                );
                return;
            };
            let size = data.format.size();
            let (width, height) = (size.width, size.height);
            if width == 0 || height == 0 {
                return;
            }
            let Some(raw) = plane.data() else {
                return;
            };
            let Some(bytes) = raw.get(offset..offset + chunk_size) else {
                return;
            };
            // slice_pixels indexes full rows; an incomplete PipeWire chunk must be
            // dropped rather than crashing the recording thread.
            let Some(required) = (stride as usize)
                .checked_mul(height.saturating_sub(1) as usize)
                .and_then(|n| n.checked_add(width as usize * 4))
            else {
                return;
            };
            if bytes.len() < required || stride < width.saturating_mul(4) {
                tracing::warn!(
                    actual = bytes.len(),
                    required,
                    width,
                    height,
                    stride,
                    "dropping incomplete pipewire frame"
                );
                return;
            }

            // Compact stride padding and (optionally) crop in one pass — `slice_pixels`
            // hardcodes 4 bytes/pixel, matching every format in `RawPixelFormat`.
            let rect = data.crop.unwrap_or(Rect {
                x: 0,
                y: 0,
                w: width,
                h: height,
            });
            let Some((packed, out_w, out_h)) =
                slice_pixels(bytes, width, height, stride, (0, 0), rect)
            else {
                tracing::warn!(
                    ?rect,
                    width,
                    height,
                    "recording crop rect does not intersect the captured frame"
                );
                return;
            };

            let frame = Frame {
                width: out_w,
                height: out_h,
                format,
                pixels: Arc::from(packed.into_boxed_slice()),
            };
            // Ignore send errors: they only happen once every receiver has been dropped,
            // which means the session is already shutting down.
            let _ = data.tx.send(Some(frame));
        })
        .register()
        .context("registering pipewire stream listener")?;

    // Bridge the cross-thread termination signal into the loop: `pw::channel` is the
    // documented way to wake a running pipewire loop from another thread (its `Sender` is a
    // plain `Arc<Mutex<_>>`, so it's `Send` even though nothing else here is).
    let _receiver = term_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |Terminate| mainloop.quit()
    });

    let format_pod = build_format_pod();
    let mut params = [spa::pod::Pod::from_bytes(&format_pod)
        .ok_or_else(|| anyhow!("building the format negotiation pod"))?];

    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("connecting the pipewire stream to the portal-selected node")?;

    mainloop.run();
    Ok(())
}

/// Serialize the `EnumFormat` POD offering every format in [`RawPixelFormat`], any reasonable
/// size, and a generous framerate range — mirrors `ashpd`'s own `screen_cast_pw` example.
fn build_format_pod() -> Vec<u8> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    use spa::pod::Value;
    use spa::utils::{Fraction, Rectangle, SpaTypes};

    let obj = spa::pod::object!(
        SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::RGBx,
            VideoFormat::BGRA,
            VideoFormat::RGBA,
            VideoFormat::xRGB,
            VideoFormat::xBGR,
            VideoFormat::ARGB,
            VideoFormat::ABGR,
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1920,
                height: 1080
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 30, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );

    // This structure is entirely static (no user input feeds it), so a serialization
    // failure would be a programming error, not a runtime condition to recover from.
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    )
    .expect("serializing the static format negotiation pod")
    .0
    .into_inner()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case(RawPixelFormat::Rgbx, "rgb0")]
    #[case(RawPixelFormat::Bgrx, "bgr0")]
    #[case(RawPixelFormat::XRgb, "0rgb")]
    #[case(RawPixelFormat::XBgr, "0bgr")]
    #[case(RawPixelFormat::Rgba, "rgba")]
    #[case(RawPixelFormat::Bgra, "bgra")]
    #[case(RawPixelFormat::Argb, "argb")]
    #[case(RawPixelFormat::Abgr, "abgr")]
    fn ffmpeg_pix_fmt_matches_rawvideo_names(#[case] fmt: RawPixelFormat, #[case] expected: &str) {
        assert_eq!(fmt.ffmpeg_pix_fmt(), expected);
    }

    #[test]
    fn build_format_pod_produces_a_parseable_pod() {
        let bytes = build_format_pod();
        assert!(!bytes.is_empty());
        assert!(spa::pod::Pod::from_bytes(&bytes).is_some());
    }
}
