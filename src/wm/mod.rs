//! Compositor window-manager IPC abstraction (active window, window list, focused output).
//!
//! Snypr's capture pipeline (`crate::capture`) is entirely generic wlroots
//! (`zwlr_screencopy_manager_v1`) and works on any compositor that implements it. A handful of
//! *selection-resolution* features — `--window`, `--focused`, and the interactive selector's
//! Window-mode click-to-pick — need richer information (active window geometry, a window list,
//! the focused output) that only a compositor-specific IPC protocol can provide. This module
//! defines that boundary: a single [`WmBackend`] trait implemented per compositor, and a
//! [`detect`] function that picks a backend at call time based on environment variables the
//! compositor itself sets (falling back to a live Wayland protocol probe — see below).
//!
//! Backends:
//! - [`hyprland`] — Hyprland's command/event sockets.
//! - [`sway`] — Sway's i3ipc socket.
//! - [`niri`] — Niri's JSON-line IPC socket.
//! - [`foreign_toplevel`] — the generic `zwlr_foreign_toplevel_manager_v1` Wayland protocol,
//!   tried as a last resort when none of the above sockets is detected (river, labwc, and other
//!   wlroots compositors that implement it). Unlike the other three, this protocol never
//!   reports window position/size — only identity (title/app_id) and coarse state. So
//!   `active_window()`/`clients()` on that backend report [`ActiveWindow`]/[`WmWindow`] with a
//!   `None` rectangle, and `--window`/the selector's Window-mode click-to-pick have nothing to
//!   crop to or hit-test against there — see [`ActiveWindow::rect`]/[`WmWindow::rect`].
//!
//! Any other compositor gets no backend at all; [`detect`] returns `None` and callers degrade
//! gracefully (see each call site for its fallback).

pub mod foreign_toplevel;
pub mod hyprland;
pub mod niri;
pub mod sway;

use anyhow::Result;
use tokio::sync::{oneshot, watch};

use crate::capture::region::Rect;

/// A snapshot of the currently focused ("active") window.
#[derive(Debug, Clone)]
pub struct ActiveWindow {
    pub title: String,
    pub class: String,
    /// Position/size, when the backend can report it. `None` on backends that only see window
    /// *identity* (title/app_id) rather than geometry — currently only [`foreign_toplevel`].
    pub at: Option<(i32, i32)>,
    pub size: Option<(u32, u32)>,
    /// Compositor output identifier. Backends that only expose a numeric id (Hyprland)
    /// surface it as a string for parity with backends that expose a name (Sway).
    pub monitor: String,
}

impl ActiveWindow {
    /// The window's capture rectangle, or `None` if the backend never reported geometry.
    pub fn rect(&self) -> Option<Rect> {
        let (x, y) = self.at?;
        let (w, h) = self.size?;
        Some(Rect { x, y, w, h })
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
    /// Position/size, when the backend can report it. `None` on [`foreign_toplevel`], which
    /// only ever sees window identity, never geometry.
    pub at: Option<(i32, i32)>,
    pub size: Option<(u32, u32)>,
    pub monitor: String,
    pub workspace_id: i64,
    pub mapped: bool,
    pub hidden: bool,
}

impl WmWindow {
    /// The window's rectangle, or `None` if the backend never reported geometry.
    pub fn rect(&self) -> Option<Rect> {
        let (x, y) = self.at?;
        let (w, h) = self.size?;
        Some(Rect { x, y, w, h })
    }
}

/// Topmost mapped, visible window whose rectangle contains the logical point `(x, y)`.
///
/// Assumes `clients` is in the backend's best-effort z-order (front-to-back). Windows with no
/// known geometry (see [`WmWindow::rect`]) never match — there's nothing to hit-test.
pub fn window_at(clients: &[WmWindow], x: i32, y: i32) -> Option<&WmWindow> {
    clients
        .iter()
        .find(|c| c.mapped && !c.hidden && c.rect().is_some_and(|r| r.contains(x, y)))
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
/// Checks, in order:
/// 1. `HYPRLAND_INSTANCE_SIGNATURE` (Hyprland).
/// 2. `SWAYSOCK` (Sway).
/// 3. `NIRI_SOCKET` (Niri).
/// 4. Last resort, only when none of the above env vars are set: does the compositor advertise
///    `zwlr_foreign_toplevel_manager_v1` over Wayland itself (river, labwc, …)? This is a real
///    Wayland connect + roundtrip, so it's the one case that makes `detect` genuinely
///    asynchronous (pushed to a blocking-pool thread — see [`foreign_toplevel::probe`]).
///
/// Returns `None` if none of the above match, or outside a Wayland session entirely — callers
/// must treat that as "no window-manager IPC available", not an error in itself.
pub async fn detect() -> Option<Box<dyn WmBackend>> {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return Some(Box::new(hyprland::Hyprland));
    }
    if std::env::var_os("SWAYSOCK").is_some() {
        return Some(Box::new(sway::Sway));
    }
    if std::env::var_os("NIRI_SOCKET").is_some() {
        return Some(Box::new(niri::Niri));
    }
    tokio::task::spawn_blocking(foreign_toplevel::probe)
        .await
        .ok()
        .flatten()
        .map(|backend| Box::new(backend) as Box<dyn WmBackend>)
}

/// Best-effort focus subscription: delegates to [`detect`]'s backend, or returns a receiver
/// that never updates (initial value `None`, forever) when no backend is detected. Consumers
/// already treat `None` as "focus unknown, don't move the toolbar", so this preserves today's
/// "static toolbar on a backend-less compositor" behavior with zero IPC attempts.
///
/// `detect` is async (its last-resort path is a real Wayland probe), but this function itself
/// must stay synchronous — callers wire it up before entering an async context. So detection is
/// deferred into a task spawned on `handle`, which then either forwards the detected backend's
/// own focus stream into the receiver returned here, or drops `shutdown` to go idle, exactly
/// like the "no backend" case did before `detect` needed an `.await`.
pub fn subscribe_focus(
    handle: &tokio::runtime::Handle,
    shutdown: oneshot::Receiver<()>,
) -> watch::Receiver<Option<String>> {
    let (tx, rx) = watch::channel(None);
    let inner_handle = handle.clone();
    handle.spawn(async move {
        let Some(backend) = detect().await else {
            // No backend: drop `shutdown` and let `tx` drop at the end of this task, matching
            // "nothing will ever be sent" for a backend whose event socket never connects.
            drop(shutdown);
            return;
        };
        let (inner_shutdown_tx, inner_shutdown_rx) = oneshot::channel();
        let mut inner_rx = backend.subscribe_focus(&inner_handle, inner_shutdown_rx);
        tokio::select! {
            _ = shutdown => {
                // Tell the backend's own subscription to stop; dropping `inner_shutdown_tx`
                // fires its `shutdown` receiver.
                drop(inner_shutdown_tx);
            }
            _ = async {
                while inner_rx.changed().await.is_ok() {
                    if tx.send(inner_rx.borrow().clone()).is_err() {
                        break;
                    }
                }
            } => {}
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::set_compositor_env;

    #[tokio::test]
    async fn detect_picks_hyprland_when_its_signature_is_set() {
        set_compositor_env(Some("deadbeef"), None, None);
        let backend = detect().await.expect("a backend");
        assert_eq!(backend.name(), "Hyprland");
    }

    #[tokio::test]
    async fn detect_picks_sway_when_hyprland_is_not_set() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        set_compositor_env(None, Some(&sock), None);
        let backend = detect().await.expect("a backend");
        assert_eq!(backend.name(), "Sway");
    }

    #[tokio::test]
    async fn detect_picks_niri_when_only_its_socket_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("niri.sock");
        set_compositor_env(None, None, Some(&sock));
        let backend = detect().await.expect("a backend");
        assert_eq!(backend.name(), "Niri");
    }

    #[tokio::test]
    async fn detect_prefers_hyprland_when_both_are_set() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        set_compositor_env(Some("deadbeef"), Some(&sock), None);
        let backend = detect().await.expect("a backend");
        assert_eq!(backend.name(), "Hyprland");
    }

    #[tokio::test]
    async fn detect_prefers_hyprland_over_niri() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("niri.sock");
        set_compositor_env(Some("deadbeef"), None, Some(&sock));
        let backend = detect().await.expect("a backend");
        assert_eq!(backend.name(), "Hyprland");
    }

    #[tokio::test]
    async fn detect_prefers_sway_over_niri() {
        let dir = tempfile::tempdir().unwrap();
        let sway_sock = dir.path().join("sway.sock");
        let niri_sock = dir.path().join("niri.sock");
        set_compositor_env(None, Some(&sway_sock), Some(&niri_sock));
        let backend = detect().await.expect("a backend");
        assert_eq!(backend.name(), "Sway");
    }

    #[tokio::test]
    async fn detect_returns_none_on_other_compositors() {
        // `set_compositor_env` also clears `WAYLAND_DISPLAY`/`WAYLAND_SOCKET`, so the
        // `foreign_toplevel` last-resort probe deterministically fails to connect regardless
        // of the host running this test (e.g. a real Hyprland/river dev session).
        set_compositor_env(None, None, None);
        assert!(detect().await.is_none());
    }

    #[tokio::test]
    async fn subscribe_focus_is_idle_without_a_backend() {
        set_compositor_env(None, None, None);
        let handle = tokio::runtime::Handle::current();
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let mut rx = subscribe_focus(&handle, shutdown_rx);
        // No backend, no sender kept alive elsewhere: the channel closes once the detection
        // task finishes and drops its sender, rather than hanging forever.
        assert!(rx.changed().await.is_err());
    }

    #[tokio::test]
    async fn subscribe_focus_delegates_to_the_detected_backend() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        set_compositor_env(None, Some(&sock), None);
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
