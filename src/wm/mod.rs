//! Compositor window-manager IPC abstraction (active window, window list, focused output).
//!
//! Snypr's capture pipeline (`crate::capture`) is entirely generic wlroots
//! (`zwlr_screencopy_manager_v1`) and works on any compositor that implements it. A handful of
//! *selection-resolution* features — `--window`, `--focused`, and the interactive selector's
//! Window-mode click-to-pick — need richer information (active window geometry, a window list,
//! the focused output) that only a compositor-specific IPC protocol can provide. This module
//! defines that boundary: a single [`WmBackend`] trait implemented per compositor, and a
//! stateless [`detect`] function that picks a backend at call time based on environment
//! variables the compositor itself sets.
//!
//! Backends:
//! - [`hyprland`] — Hyprland's command/event sockets.
//! - [`sway`] — Sway's i3ipc socket.
//!
//! Other wlroots compositors (river, wayfire, …) have no backend today; [`detect`] returns
//! `None` for them and callers degrade gracefully (see each call site for its fallback).

pub mod hyprland;
pub mod sway;

use anyhow::Result;
use tokio::sync::{oneshot, watch};

use crate::capture::region::Rect;

/// A snapshot of the currently focused ("active") window.
#[derive(Debug, Clone)]
pub struct ActiveWindow {
    pub title: String,
    pub class: String,
    pub at: (i32, i32),
    pub size: (u32, u32),
    /// Compositor output identifier. Backends that only expose a numeric id (Hyprland)
    /// surface it as a string for parity with backends that expose a name (Sway).
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

/// A single window as reported by a [`WmBackend`]'s window list.
///
/// Order is best-effort front-to-back (topmost first); callers that want hit-testing should
/// iterate in order and pick the first match. See each backend's `clients()` doc comment for
/// exactly how faithful that ordering is.
#[derive(Debug, Clone)]
pub struct WmWindow {
    /// Opaque per-window identifier (Hyprland address, Sway container id as a string, …).
    pub id: String,
    pub title: String,
    pub class: String,
    pub at: (i32, i32),
    pub size: (u32, u32),
    pub monitor: String,
    pub workspace_id: i64,
    pub mapped: bool,
    pub hidden: bool,
}

impl WmWindow {
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.at.0,
            y: self.at.1,
            w: self.size.0,
            h: self.size.1,
        }
    }
}

/// Topmost mapped, visible window whose rectangle contains the logical point `(x, y)`.
///
/// Assumes `clients` is in the backend's best-effort z-order (front-to-back).
pub fn window_at(clients: &[WmWindow], x: i32, y: i32) -> Option<&WmWindow> {
    clients
        .iter()
        .find(|c| c.mapped && !c.hidden && c.rect().contains(x, y))
}

/// A compositor-specific window-manager IPC backend.
///
/// Every method is best-effort from the caller's point of view: callers that only want a
/// "nice to have" (pre-selection, follow-focus) should treat `Err`/`None` as "not available"
/// rather than propagating it, while `--window`/`--focused` treat `Err` as fatal (there's no
/// sensible fallback for "capture the active window" without a window to point at).
#[async_trait::async_trait]
pub trait WmBackend: Send + Sync {
    /// Human-readable backend name, for logs and `doctor` ("Hyprland", "Sway").
    fn name(&self) -> &'static str;

    /// Resolve the backend's IPC socket path (for `doctor` reporting only).
    fn socket_path(&self) -> Result<std::path::PathBuf>;

    /// Fetch the currently active (focused) window.
    async fn active_window(&self) -> Result<ActiveWindow>;

    /// List every window known to the compositor, best-effort ordered front-to-back.
    async fn clients(&self) -> Result<Vec<WmWindow>>;

    /// Name of the focused output.
    async fn focused_output(&self) -> Result<String>;

    /// Subscribe to focused-output changes. Spawns a task on `handle` that publishes the
    /// focused output's name to the returned [`watch::Receiver`] until `shutdown` fires or the
    /// backend's event stream closes. Best-effort: failures are logged and never propagated.
    fn subscribe_focus(
        &self,
        handle: &tokio::runtime::Handle,
        shutdown: oneshot::Receiver<()>,
    ) -> watch::Receiver<Option<String>>;
}

/// Detect which window-manager backend is available in the current environment.
///
/// Checks, in order: `HYPRLAND_INSTANCE_SIGNATURE` (Hyprland), then `SWAYSOCK` (Sway). Returns
/// `None` on any other compositor (river, wayfire, …) or outside a Wayland session entirely —
/// callers must treat that as "no window-manager IPC available", not an error in itself.
///
/// Stateless by design (mirrors the previous `crate::hypr` free-function style): detection is
/// just two environment variable checks, so there is no benefit to caching it on [`crate::context::Context`].
pub fn detect() -> Option<Box<dyn WmBackend>> {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return Some(Box::new(hyprland::Hyprland));
    }
    if std::env::var_os("SWAYSOCK").is_some() {
        return Some(Box::new(sway::Sway));
    }
    None
}

/// Best-effort focus subscription: delegates to [`detect`]'s backend, or returns a receiver
/// that never updates (initial value `None`, forever) when no backend is detected. Consumers
/// already treat `None` as "focus unknown, don't move the toolbar", so this preserves today's
/// "static toolbar on a backend-less compositor" behavior with zero IPC attempts.
pub fn subscribe_focus(
    handle: &tokio::runtime::Handle,
    shutdown: oneshot::Receiver<()>,
) -> watch::Receiver<Option<String>> {
    match detect() {
        Some(backend) => backend.subscribe_focus(handle, shutdown),
        None => {
            // Keep `shutdown` and the sender alive for the lifetime of the (idle) receiver by
            // simply dropping them here; nothing will ever be sent, matching a backend whose
            // event socket never connects.
            let (_tx, rx) = watch::channel(None);
            drop(shutdown);
            rx
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::set_compositor_env;

    #[test]
    fn detect_picks_hyprland_when_its_signature_is_set() {
        set_compositor_env(Some("deadbeef"), None);
        let backend = detect().expect("a backend");
        assert_eq!(backend.name(), "Hyprland");
    }

    #[test]
    fn detect_picks_sway_when_hyprland_is_not_set() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        set_compositor_env(None, Some(&sock));
        let backend = detect().expect("a backend");
        assert_eq!(backend.name(), "Sway");
    }

    #[test]
    fn detect_prefers_hyprland_when_both_are_set() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        set_compositor_env(Some("deadbeef"), Some(&sock));
        let backend = detect().expect("a backend");
        assert_eq!(backend.name(), "Hyprland");
    }

    #[test]
    fn detect_returns_none_on_other_compositors() {
        set_compositor_env(None, None);
        assert!(detect().is_none());
    }

    #[tokio::test]
    async fn subscribe_focus_is_idle_without_a_backend() {
        set_compositor_env(None, None);
        let handle = tokio::runtime::Handle::current();
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let mut rx = subscribe_focus(&handle, shutdown_rx);
        // No backend, no sender kept alive elsewhere: the channel closes immediately and
        // `changed()` reports that rather than hanging forever.
        assert!(rx.changed().await.is_err());
    }

    #[tokio::test]
    async fn subscribe_focus_delegates_to_the_detected_backend() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        set_compositor_env(None, Some(&sock));
        let listener = crate::testing::bind_fake_sway_socket(&sock);
        let server = tokio::spawn(async move {
            // The Sway backend's `subscribe_focus` opens a SUBSCRIBE session; accepting the
            // connection and dropping it immediately is enough to prove `subscribe_focus`
            // reached the backend at all (as opposed to the idle, no-backend path above).
            let _ = listener.accept().await;
        });
        let handle = tokio::runtime::Handle::current();
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let _rx = subscribe_focus(&handle, shutdown_rx);
        server.await.unwrap();
    }
}
