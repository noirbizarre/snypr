//! Negotiates an `org.freedesktop.portal.ScreenCast` session and returns the resulting
//! PipeWire stream's connection details.
//!
//! PipeWire is a media *transport*, not an encoder: the portal only ever hands back raw
//! video frames on a PipeWire node, the same way a webcam would. Actually decoding/encoding
//! those frames is [`super::stream`] and [`super::encoder`]'s job, not this module's.
//!
//! The portal — not Snypr — owns the actual "which monitor/window" picker UI; there is no
//! standard, scriptable way to pre-select a specific target or skip the dialog (short of
//! `PersistMode::Persistent` + a cached `restore_token`, which is a deliberate v1
//! limitation — see the project plan). Every call here always prompts the user.

use std::os::fd::OwnedFd;

use anyhow::{Context as _, Result, bail};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};

use super::RecordTarget;
use crate::i18n::fl;

/// Everything [`super::stream::spawn`] needs to attach to the negotiated PipeWire node.
pub(crate) struct NegotiatedStream {
    pub pipewire_fd: OwnedFd,
    pub node_id: u32,
}

/// Open the portal's ScreenCast picker for `target` and return the resulting PipeWire
/// stream. `cursor` selects between `CursorMode::Embedded` (compositor bakes the cursor
/// into the frames) and `CursorMode::Hidden` — `CursorMode::Metadata` would need Snypr to
/// composite the cursor itself from separate metadata, which is out of scope for v1.
pub(crate) async fn negotiate(target: &RecordTarget, cursor: bool) -> Result<NegotiatedStream> {
    let source_type = match target {
        RecordTarget::Monitor | RecordTarget::Cropped(_) => SourceType::Monitor,
        RecordTarget::Window => SourceType::Window,
    };

    let proxy = Screencast::new()
        .await
        .context("connecting to the org.freedesktop.portal.ScreenCast proxy")?;
    let session = proxy
        .create_session(Default::default())
        .await
        .context("creating a ScreenCast portal session")?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(if cursor {
                    CursorMode::Embedded
                } else {
                    CursorMode::Hidden
                })
                .set_sources(ashpd::enumflags2::BitFlags::from(source_type))
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await
        .context(
            "selecting a ScreenCast source (does the running portal backend support this \
             source type? e.g. xdg-desktop-portal-wlr does not support window capture)",
        )?;

    let response = proxy
        .start(&session, None, Default::default())
        .await
        .context("starting the ScreenCast session")?
        .response()
        .context("reading the ScreenCast start response")?;

    let streams = response.streams();
    let Some(stream) = streams.first() else {
        bail!("{}", fl!("error-record-no-portal-stream"));
    };
    if streams.len() > 1 {
        // `set_multiple(false)` above should make this unreachable in practice, but a
        // portal backend could still misbehave — warn instead of silently discarding data
        // rather than failing a session that otherwise has a perfectly usable first stream.
        tracing::warn!(
            count = streams.len(),
            "portal returned multiple streams for a single-source request; using the first one"
        );
    }
    let node_id = stream.pipe_wire_node_id();

    let pipewire_fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .context("opening the portal's PipeWire remote")?;

    Ok(NegotiatedStream {
        pipewire_fd,
        node_id,
    })
}
