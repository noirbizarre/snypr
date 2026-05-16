//! Hyprland IPC helpers (active window, monitors, focused output).
//!
//! Wraps the `hyprland` crate; isolated here so the rest of the codebase can stub it during
//! tests without pulling Hyprland-specific types into capture/UI code.

use anyhow::Result;

use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct ActiveWindow {
    pub title: String,
    pub class: String,
    pub at: (i32, i32),
    pub size: (u32, u32),
    pub monitor: String,
}

impl ActiveWindow {
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.at.0,
            y: self.at.1,
            w: self.size.0,
            h: self.size.1,
        }
    }
}

/// Fetch the currently active window via Hyprland IPC.
pub async fn active_window() -> Result<ActiveWindow> {
    use hyprland::data::Client;
    use hyprland::shared::HyprDataActiveOptional;

    let client = Client::get_active_async()
        .await
        .map_err(|e| anyhow::anyhow!("hyprctl active client: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no active client"))?;
    Ok(ActiveWindow {
        title: client.title,
        class: client.class,
        at: (client.at.0 as i32, client.at.1 as i32),
        size: (client.size.0 as u32, client.size.1 as u32),
        monitor: client.monitor.to_string(),
    })
}

/// Name of the focused monitor.
pub async fn focused_monitor() -> Result<String> {
    use hyprland::data::Monitors;
    use hyprland::shared::HyprData;

    let monitors = Monitors::get_async()
        .await
        .map_err(|e| anyhow::anyhow!("hyprctl monitors: {e}"))?;
    monitors
        .iter()
        .find(|m| m.focused)
        .map(|m| m.name.clone())
        .ok_or_else(|| anyhow::anyhow!("no focused monitor"))
}
