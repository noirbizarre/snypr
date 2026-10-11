//! Video recording (`snypr record`) via the `org.freedesktop.portal.ScreenCast` portal +
//! PipeWire, encoded by shelling out to the system `ffmpeg` binary.
//!
//! ```text
//! portal::negotiate  ──►  stream::spawn (own OS thread, pipewire main loop)
//!                              │  tokio::sync::watch::Sender<Option<Frame>>
//!                              ▼
//!                     run()'s fps-driven ticker ──► encoder::Encoder (ffmpeg child)
//! ```
//!
//! Unlike `screenshot`'s wlr-screencopy path, the portal — not Snypr — owns the actual
//! monitor/window picker UI, and PipeWire delivers frames on damage rather than a fixed
//! clock. See [`resolve_target`] and the project plan's "Known v1 limitations" for exactly
//! how `capture::Selection` maps onto this model, and [`run`]'s docs for the duplicate-last-
//! frame pacing strategy.

pub(crate) mod encoder;
pub(crate) mod portal;
pub(crate) mod stream;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use tokio::sync::oneshot;

use crate::capture::Selection;
use crate::capture::region::Rect;
use crate::config::VideoFormat;
use crate::i18n::fl;

pub use stream::Frame;

/// Where the recording session should point the portal, resolved from a (fully-resolved,
/// non-`Interactive`) [`Selection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordTarget {
    /// `Full`/`Output(_)`/`Focused`: opens the portal's monitor picker. Snypr cannot force a
    /// specific monitor through the standard portal API — whichever the user picks is what
    /// gets recorded, regardless of which of these three variants asked for it.
    Monitor,
    /// `Window`: opens the portal's window picker. Requires the running portal backend to
    /// support `SourceType::Window` (true for `xdg-desktop-portal-hyprland`, generally not
    /// for `-wlr`) — `portal::negotiate` surfaces the backend's own rejection otherwise.
    Window,
    /// `Region`/`Interactive`: opens the portal's monitor picker, then every frame is
    /// software-cropped to this rect (in the same logical-pixel coordinate space
    /// `screenshot --region` uses). Assumes the user picks the monitor the rect belongs to.
    Cropped(Rect),
}

/// Resolve a concrete [`Selection`] (already past `cli::selection::resolve` — no
/// `Interactive` left) into a [`RecordTarget`]. `PerOutput` is rejected: multi-stream
/// recording (one file per output) isn't implemented yet.
pub(crate) fn resolve_target(selection: &Selection) -> Result<RecordTarget> {
    match selection {
        Selection::Full | Selection::Output(_) | Selection::Focused => Ok(RecordTarget::Monitor),
        Selection::Window => Ok(RecordTarget::Window),
        Selection::Region(rect) => Ok(RecordTarget::Cropped(*rect)),
        Selection::PerOutput => bail!("{}", fl!("error-record-per-output-unsupported")),
        Selection::Interactive => {
            unreachable!("Interactive must be resolved to a concrete Selection before recording")
        }
    }
}

/// Parameters for a single recording session, fully resolved (concrete `Selection`, real
/// output path) by the caller — `cli::record` for the foreground path, `daemon` for the
/// `--via-daemon` toggle.
pub struct RecordOptions {
    pub selection: Selection,
    pub cursor: bool,
    pub fps: u32,
    pub format: VideoFormat,
    pub crf: u8,
    pub path: PathBuf,
}

/// Drive a full recording session until `stop` resolves (or the stream/encoder fails), then
/// finalize and return the written file path.
///
/// Frame pacing: the portal delivers frames on damage, not a fixed clock, so this function
/// doesn't feed `ffmpeg` as frames arrive. Instead it ticks at `opts.fps` and feeds whatever
/// the latest frame is at each tick — duplicating it if the screen hasn't changed since the
/// last tick. This is the same technique used by most screen-recording tools and keeps the
/// encoder's declared constant framerate honest without needing per-frame timestamps.
pub async fn run(opts: RecordOptions, mut stop: oneshot::Receiver<()>) -> Result<PathBuf> {
    let target = resolve_target(&opts.selection)?;
    let negotiated = portal::negotiate(&target, opts.cursor)
        .await
        .context("negotiating the ScreenCast portal session")?;

    let crop = match target {
        RecordTarget::Cropped(rect) => Some(rect),
        RecordTarget::Monitor | RecordTarget::Window => None,
    };

    let (frame_tx, mut frame_rx) = tokio::sync::watch::channel(None::<Frame>);
    let stream_handle = stream::spawn(negotiated.pipewire_fd, negotiated.node_id, crop, frame_tx)
        .context("starting the pipewire video stream")?;

    // Wait for the first frame so `ffmpeg` can be spawned with the real (post-negotiation,
    // post-crop) dimensions and pixel format instead of guessing.
    tokio::select! {
        biased;
        _ = &mut stop => {
            stream_handle.stop();
            bail!("{}", fl!("error-record-stopped-before-first-frame"));
        }
        res = frame_rx.wait_for(|f| f.is_some()) => {
            if let Err(err) = res {
                stream_handle.stop();
                return Err(err).context(fl!("error-record-stream-closed"));
            }
        }
    }
    let first = frame_rx
        .borrow()
        .clone()
        .expect("wait_for guarantees a frame is present");

    let mut encoder = encoder::Encoder::spawn(
        &opts.path,
        first.width,
        first.height,
        first.format,
        opts.fps,
        opts.format,
        opts.crf,
    )
    .await
    .context("spawning the ffmpeg encoder")?;
    encoder
        .write_frame(&first.pixels)
        .await
        .context("writing the first frame")?;

    let period = Duration::from_secs_f64(1.0 / f64::from(opts.fps.max(1)));
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The very first tick fires immediately; we've already written `first` above.
    ticker.tick().await;

    let result: Result<()> = loop {
        tokio::select! {
            biased;
            _ = &mut stop => break Ok(()),
            _ = ticker.tick() => {
                let frame = frame_rx.borrow().clone();
                let Some(frame) = frame else { continue };
                if let Err(err) = encoder.write_frame(&frame.pixels).await {
                    break Err(err);
                }
            }
        }
    };

    stream_handle.stop();
    let path = encoder.finish(opts.path).await;
    // Surface whichever failure happened first: an encoder write failure almost always means
    // ffmpeg already died, in which case `finish` below would just report the same thing
    // less specifically.
    result?;
    path
}
