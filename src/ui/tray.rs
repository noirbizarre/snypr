//! System tray icon (StatusNotifierItem) via `ksni`.
//!
//! The tray is hosted by `hyprsnap daemon` when `[tray].enabled = true` in the config. Menu
//! activations are translated into [`TrayAction`]s and forwarded over a tokio MPSC channel to
//! the daemon's main select-loop, which dispatches the actual screenshot / overlay work on its
//! own runtime. Keeping ksni at arm's length like this avoids tangling its sync `activate`
//! callbacks with our async pipeline.

use std::sync::OnceLock;

use anyhow::{Context as _, Result};
use tokio::sync::mpsc::UnboundedSender;

/// Embedded HyprSnap logo, shipped alongside the source so the tray works out-of-the-box even
/// on systems where the icon theme hasn't been installed system-wide. The file is also the
/// canonical app icon and gets installed to `/usr/share/icons/hicolor/256x256/apps/` by the
/// packaging rules.
const LOGO_PNG: &[u8] =
    include_bytes!("../../data/icons/hicolor/256x256/apps/noirbizar.re.HyprSnap.png");

/// Side-effect requested by the user via the tray menu.
#[derive(Debug, Clone, Copy)]
pub enum TrayAction {
    /// Take a screenshot using the configured default sinks. When `edit` is true the result is
    /// piped through the annotation editor before reaching the sinks.
    Screenshot { edit: bool },
    /// Open the live draw-on-screen overlay.
    OpenDraw,
    /// Tear the daemon down.
    Quit,
}

/// Spawn the tray in the background. Returns a handle that keeps the tray alive — drop it to
/// remove the tray icon. Errors only if registering the StatusNotifierItem on the bus fails.
pub async fn spawn(tx: UnboundedSender<TrayAction>) -> Result<ksni::Handle<HyprSnapTray>> {
    use ksni::TrayMethods;
    let tray = HyprSnapTray { tx };
    let handle = tray
        .spawn()
        .await
        .context("registering tray StatusNotifierItem")?;
    tracing::info!("hyprsnap tray registered");
    Ok(handle)
}

pub struct HyprSnapTray {
    tx: UnboundedSender<TrayAction>,
}

impl ksni::Tray for HyprSnapTray {
    fn id(&self) -> String {
        env!("CARGO_PKG_NAME").into()
    }

    fn title(&self) -> String {
        "HyprSnap".into()
    }

    fn icon_name(&self) -> String {
        // Resolves once the icon is installed under hicolor (or any other theme); the
        // `icon_pixmap` below is the always-available fallback when it isn't.
        crate::ui::APP_ID.into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        // Decoded lazily on first request and cached for the daemon's lifetime — the PNG decode
        // is non-trivial and the SNI host may poll this multiple times per icon refresh.
        static CACHED: OnceLock<Vec<ksni::Icon>> = OnceLock::new();
        CACHED
            .get_or_init(|| match decode_logo() {
                Ok(icon) => vec![icon],
                Err(err) => {
                    tracing::warn!(error = ?err, "failed to decode embedded tray logo; falling back to icon_name");
                    Vec::new()
                }
            })
            .clone()
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        vec![
            StandardItem {
                label: "Screenshot (full)".into(),
                icon_name: "camera-photo".into(),
                activate: Box::new(|this: &mut Self| {
                    this.send(TrayAction::Screenshot { edit: false })
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Annotate region…".into(),
                icon_name: "edit-cut".into(),
                activate: Box::new(|this: &mut Self| {
                    this.send(TrayAction::Screenshot { edit: true })
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Draw on screen".into(),
                icon_name: "draw-freehand".into(),
                activate: Box::new(|this: &mut Self| this.send(TrayAction::OpenDraw)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|this: &mut Self| this.send(TrayAction::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

impl HyprSnapTray {
    fn send(&self, action: TrayAction) {
        // The channel is unbounded so this is non-blocking; failure means the daemon side has
        // dropped the receiver, which only happens during shutdown.
        if let Err(err) = self.tx.send(action) {
            tracing::warn!(error = ?err, "tray action dropped: daemon receiver gone");
        }
    }
}

/// Decode the bundled PNG and convert it to the ARGB32-big-endian byte layout that the
/// StatusNotifierItem spec mandates for `icon_pixmap` (the `Icon::data` field).
fn decode_logo() -> Result<ksni::Icon> {
    let img = image::load_from_memory(LOGO_PNG)
        .context("decoding embedded tray logo PNG")?
        .to_rgba8();
    let (w, h) = img.dimensions();
    let rgba = img.into_raw();
    // ARGB32 in network byte order = byte sequence A, R, G, B.
    let mut argb = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        argb.push(px[3]); // A
        argb.push(px[0]); // R
        argb.push(px[1]); // G
        argb.push(px[2]); // B
    }
    Ok(ksni::Icon {
        width: w as i32,
        height: h as i32,
        data: argb,
    })
}
