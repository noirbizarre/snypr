//! System tray icon (StatusNotifierItem) via `ksni`.
//!
//! The tray is hosted by `hyprsnap daemon` when `[tray].enabled = true` in the config. Menu
//! activations are translated into [`TrayAction`]s and forwarded over a tokio MPSC channel to
//! the daemon's main select-loop, which dispatches the actual screenshot / capture / overlay
//! work on its own runtime. Keeping ksni at arm's length like this avoids tangling its sync
//! `activate` callbacks with our async pipeline.

use anyhow::{Context as _, Result};
use tokio::sync::mpsc::UnboundedSender;

/// Side-effect requested by the user via the tray menu.
#[derive(Debug, Clone, Copy)]
pub enum TrayAction {
    /// Take a full-desktop screenshot using the configured default sinks.
    Screenshot,
    /// Run the capture flow: interactive selector → annotation editor → sinks.
    Capture,
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
        // Fall back to the freedesktop icon for screenshots; users can override via
        // their icon theme.
        "applets-screenshooter".into()
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        vec![
            StandardItem {
                label: "Screenshot (full)".into(),
                icon_name: "camera-photo".into(),
                activate: Box::new(|this: &mut Self| this.send(TrayAction::Screenshot)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Capture (region + annotate)".into(),
                icon_name: "edit-cut".into(),
                activate: Box::new(|this: &mut Self| this.send(TrayAction::Capture)),
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
