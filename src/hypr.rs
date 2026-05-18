//! Hyprland IPC helpers (active window, focused monitor).
//!
//! Connects directly to Hyprland's command socket and parses the JSON responses to `activewindow`
//! and `monitors`. We deliberately avoid the upstream `hyprland` crate because it hard-codes the
//! pre-0.42 `/tmp/hypr/...` socket path, panics (rather than returns `Err`) on I/O failure, and
//! pulls in a large dependency tree for the two queries we need.
//!
//! Socket path resolution mirrors Hyprland 0.42+ with a fallback to the legacy location:
//!   1. `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock`
//!   2. `/tmp/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock`

use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct ActiveWindow {
    pub title: String,
    pub class: String,
    pub at: (i32, i32),
    pub size: (u32, u32),
    /// Compositor monitor identifier. Hyprland reports this as a numeric ID, not a name, so we
    /// surface it as a string for parity with the previous public API.
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
    let body = query("j/activewindow").await?;
    // When no client is focused Hyprland answers with an empty object `{}` (or the literal string
    // `none` on older builds). Treat both as "no active window".
    let trimmed = body.trim();
    if trimmed.is_empty() || trimmed == "{}" || trimmed.eq_ignore_ascii_case("none") {
        bail!("no active client");
    }

    #[derive(Deserialize)]
    struct Raw {
        title: String,
        class: String,
        at: [i32; 2],
        size: [i32; 2],
        monitor: i64,
    }

    let raw: Raw = serde_json::from_str(&body)
        .with_context(|| format!("parsing Hyprland activewindow response: {body}"))?;
    Ok(ActiveWindow {
        title: raw.title,
        class: raw.class,
        at: (raw.at[0], raw.at[1]),
        size: (raw.size[0].max(0) as u32, raw.size[1].max(0) as u32),
        monitor: raw.monitor.to_string(),
    })
}

/// A single Hyprland client (window) as reported by `j/clients`.
///
/// Order in the returned `Vec` matches Hyprland's IPC response, which is z-ordered
/// front-to-back; callers that want hit-testing should iterate in order and pick the
/// first match.
#[derive(Debug, Clone)]
pub struct HyprWindow {
    pub address: String,
    pub title: String,
    pub class: String,
    pub at: (i32, i32),
    pub size: (u32, u32),
    pub monitor: String,
    pub workspace_id: i64,
    pub mapped: bool,
    pub hidden: bool,
}

impl HyprWindow {
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.at.0,
            y: self.at.1,
            w: self.size.0,
            h: self.size.1,
        }
    }
}

/// List every Hyprland client (window). Order is preserved from the IPC response so
/// front-to-back hit-testing works without re-sorting.
pub async fn clients() -> Result<Vec<HyprWindow>> {
    let body = query("j/clients").await?;

    #[derive(Deserialize)]
    struct RawWorkspace {
        id: i64,
    }
    #[derive(Deserialize)]
    struct Raw {
        address: String,
        title: String,
        class: String,
        at: [i32; 2],
        size: [i32; 2],
        monitor: i64,
        workspace: RawWorkspace,
        mapped: bool,
        hidden: bool,
    }

    let raw: Vec<Raw> = serde_json::from_str(&body)
        .with_context(|| format!("parsing Hyprland clients response: {body}"))?;
    Ok(raw
        .into_iter()
        .map(|r| HyprWindow {
            address: r.address,
            title: r.title,
            class: r.class,
            at: (r.at[0], r.at[1]),
            size: (r.size[0].max(0) as u32, r.size[1].max(0) as u32),
            monitor: r.monitor.to_string(),
            workspace_id: r.workspace.id,
            mapped: r.mapped,
            hidden: r.hidden,
        })
        .collect())
}

/// Topmost mapped, visible client whose rectangle contains the logical point `(x, y)`.
///
/// Assumes `clients` is in Hyprland's z-order (front-to-back), which is how
/// [`clients`] returns them.
pub fn window_at(clients: &[HyprWindow], x: i32, y: i32) -> Option<&HyprWindow> {
    clients
        .iter()
        .find(|c| c.mapped && !c.hidden && c.rect().contains(x, y))
}

/// Name of the focused monitor.
pub async fn focused_monitor() -> Result<String> {
    let body = query("j/monitors").await?;

    #[derive(Deserialize)]
    struct Raw {
        name: String,
        focused: bool,
    }

    let monitors: Vec<Raw> = serde_json::from_str(&body)
        .with_context(|| format!("parsing Hyprland monitors response: {body}"))?;
    monitors
        .into_iter()
        .find(|m| m.focused)
        .map(|m| m.name)
        .ok_or_else(|| anyhow!("no focused monitor"))
}

/// Send `command` to the Hyprland command socket and return the raw response body.
async fn query(command: &str) -> Result<String> {
    let path = socket_path().context("locating Hyprland IPC socket")?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to Hyprland IPC at {}", path.display()))?;
    stream
        .write_all(command.as_bytes())
        .await
        .context("writing Hyprland IPC command")?;
    stream
        .shutdown()
        .await
        .context("closing Hyprland IPC write side")?;
    let mut body = String::new();
    stream
        .read_to_string(&mut body)
        .await
        .context("reading Hyprland IPC response")?;
    Ok(body)
}

/// Resolve the Hyprland command-socket path.
///
/// Prefers the modern `$XDG_RUNTIME_DIR/hypr/$HIS/.socket.sock` layout (Hyprland ≥ 0.42) and
/// falls back to the legacy `/tmp/hypr/$HIS/.socket.sock` for older builds.
pub(crate) fn socket_path() -> Result<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").map_err(|_| {
        anyhow!("HYPRLAND_INSTANCE_SIGNATURE is not set; not running under Hyprland?")
    })?;
    let candidates = [
        std::env::var("XDG_RUNTIME_DIR").ok().map(|d| {
            PathBuf::from(d)
                .join("hypr")
                .join(&sig)
                .join(".socket.sock")
        }),
        Some(PathBuf::from(format!("/tmp/hypr/{sig}/.socket.sock"))),
    ];
    for c in candidates.into_iter().flatten() {
        if c.exists() {
            return Ok(c);
        }
    }
    bail!(
        "Hyprland IPC socket not found under $XDG_RUNTIME_DIR/hypr/{sig}/.socket.sock or /tmp/hypr/{sig}/.socket.sock",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn win(
        address: &str,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        mapped: bool,
        hidden: bool,
    ) -> HyprWindow {
        HyprWindow {
            address: address.into(),
            title: "t".into(),
            class: "c".into(),
            at: (x, y),
            size: (w, h),
            monitor: "0".into(),
            workspace_id: 1,
            mapped,
            hidden,
        }
    }

    #[test]
    fn window_at_returns_topmost_match() {
        // Hyprland returns clients front-to-back; the first match wins.
        let clients = vec![
            win("top", 0, 0, 100, 100, true, false),
            win("bottom", 0, 0, 200, 200, true, false),
        ];
        assert_eq!(
            window_at(&clients, 50, 50).map(|w| w.address.as_str()),
            Some("top")
        );
    }

    #[test]
    fn window_at_skips_unmapped_and_hidden() {
        let clients = vec![
            win("unmapped", 0, 0, 100, 100, false, false),
            win("hidden", 0, 0, 100, 100, true, true),
            win("visible", 0, 0, 100, 100, true, false),
        ];
        assert_eq!(
            window_at(&clients, 10, 10).map(|w| w.address.as_str()),
            Some("visible")
        );
    }

    #[test]
    fn window_at_returns_none_when_point_outside() {
        let clients = vec![win("only", 0, 0, 10, 10, true, false)];
        assert_eq!(
            window_at(&clients, 100, 100).map(|w| w.address.as_str()),
            None
        );
    }

    #[test]
    fn window_at_picks_correct_one_for_disjoint_clients() {
        let clients = vec![
            win("left", 0, 0, 100, 100, true, false),
            win("right", 200, 0, 100, 100, true, false),
        ];
        assert_eq!(
            window_at(&clients, 250, 50).map(|w| w.address.as_str()),
            Some("right")
        );
        assert_eq!(
            window_at(&clients, 50, 50).map(|w| w.address.as_str()),
            Some("left")
        );
    }
}
