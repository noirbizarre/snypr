//! Generic `zwlr_foreign_toplevel_manager_v1` backend.
//!
//! Tried by [`super::detect`] as a last resort, for wlroots compositors that have no
//! compositor-specific socket backend here (river, labwc, and others that implement this
//! protocol). Unlike [`super::hyprland`]/[`super::sway`], this is a real Wayland client
//! connection (mirroring `crate::capture::wlr`'s use of `smithay-client-toolkit` for registry
//! plumbing) rather than a request/response Unix socket.
//!
//! # What the protocol can and cannot tell us
//!
//! `zwlr_foreign_toplevel_handle_v1` reports a toplevel's title, app_id, coarse state
//! (`maximized`/`minimized`/`activated`/`fullscreen`), and which outputs it's visible on
//! (`output_enter`/`output_leave`) — but **never** position or size (`set_rectangle` is a
//! client→server hint only, not real geometry). So:
//!
//! - [`focused_output`]/[`subscribe_focus`] work: the activated toplevel's first entered
//!   output is "the focused output".
//! - [`active_window`]/[`clients`] report identity only (title/app_id) — [`super::ActiveWindow`]/
//!   [`super::WmWindow`]'s `at`/`size` are always `None` here, so `rect()` is always `None` too.
//!   `--window` and the selector's Window-mode click-to-pick have nothing to crop to or
//!   hit-test against on this backend (same outcome as "no backend", just via a clean `None`
//!   rather than an absent backend).
//! - There's also no z-order and no workspace concept; `clients()` reports `workspace_id: -1`
//!   as a sentinel and whatever order the compositor sent `toplevel` events in.
//!
//! If more than one toplevel reports `activated` (the protocol doesn't forbid it, even if real
//! compositors rarely do this), the last one seen wins — best-effort, not a guarantee.

use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow};
use smithay_client_toolkit::{
    dispatch2::Dispatch2,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
};
use tokio::sync::{oneshot, watch};
use wayland_client::{
    Connection, EventQueue, Proxy, QueueHandle, globals::registry_queue_init, protocol::wl_output,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use super::{ActiveWindow, WmBackend, WmWindow};

/// [`WmBackend`] implementation for the generic `zwlr_foreign_toplevel_manager_v1` protocol.
/// See the module docs for exactly what it can and cannot report.
pub struct ForeignToplevel;

#[async_trait::async_trait]
impl WmBackend for ForeignToplevel {
    fn name(&self) -> &'static str {
        "wlr-foreign-toplevel"
    }

    fn socket_path(&self) -> Result<PathBuf> {
        wayland_display_path()
    }

    async fn active_window(&self) -> Result<ActiveWindow> {
        let snapshot = spawn_snapshot().await?;
        let active = pick_activated(&snapshot)
            .ok_or_else(|| anyhow!("{}", crate::i18n::fl!("error-no-active-window")))?;
        Ok(ActiveWindow {
            title: active.title.clone(),
            class: active.app_id.clone(),
            at: None,
            size: None,
            monitor: active.outputs.first().cloned().unwrap_or_default(),
        })
    }

    async fn clients(&self) -> Result<Vec<WmWindow>> {
        let snapshot = spawn_snapshot().await?;
        Ok(snapshot
            .into_iter()
            .map(|t| WmWindow {
                id: t.id,
                title: t.title,
                class: t.app_id,
                at: None,
                size: None,
                monitor: t.outputs.first().cloned().unwrap_or_default(),
                // The protocol has no workspace concept at all; -1 is a documented sentinel
                // (nothing today matches on a specific `workspace_id` value).
                workspace_id: -1,
                // Closed toplevels are already excluded from the snapshot.
                mapped: true,
                hidden: t.minimized,
            })
            .collect())
    }

    async fn focused_output(&self) -> Result<String> {
        let snapshot = spawn_snapshot().await?;
        pick_activated(&snapshot)
            .and_then(|t| t.outputs.first().cloned())
            .ok_or_else(|| anyhow!("{}", crate::i18n::fl!("error-no-focused-output")))
    }

    fn subscribe_focus(
        &self,
        handle: &tokio::runtime::Handle,
        shutdown: oneshot::Receiver<()>,
    ) -> watch::Receiver<Option<String>> {
        subscribe_focus(handle, shutdown)
    }
}

/// Best-effort probe: does the compositor advertise `zwlr_foreign_toplevel_manager_v1` at all?
/// `None` on any failure (no Wayland session, connect refused, protocol not advertised) —
/// [`super::detect`] treats that identically to "no backend for this compositor". Blocking (a
/// real Wayland connect + roundtrip); run via `tokio::task::spawn_blocking`, as [`super::detect`]
/// does.
pub(crate) fn probe() -> Option<ForeignToplevel> {
    connect_and_bind().ok().map(|_| ForeignToplevel)
}

/// Run [`snapshot_blocking`] on the blocking pool and flatten its panic/error into one `Result`.
async fn spawn_snapshot() -> Result<Vec<ToplevelSnapshot>> {
    tokio::task::spawn_blocking(snapshot_blocking)
        .await
        .map_err(|e| anyhow!("wlr-foreign-toplevel snapshot task panicked: {e}"))?
}

/// One-shot snapshot of every open toplevel. Connects fresh each call (mirrors
/// `capture::wlr::enumerate_outputs`/`capture_blocking`'s one-connection-per-call idiom) rather
/// than keeping a connection alive between queries.
fn snapshot_blocking() -> Result<Vec<ToplevelSnapshot>> {
    let (mut queue, mut data) = connect_and_bind()?;
    settle(&mut queue, &mut data)?;
    Ok(collect_snapshot(&data))
}

/// Two round-trips: the first lets the manager's initial `toplevel` batch (and each handle's
/// title/app_id/state/output_enter/done) settle — a single `wl_display.sync` is enough since
/// the compositor sends all of that before acking our sync request, the same reasoning that
/// lets `registry_queue_init` populate the initial global list in one hop. The second is for
/// `OutputState`'s own xdg-output info (needed to resolve `output_enter`'s `wl_output` into a
/// name), which itself depends on the first hop's `wl_output` bind — same two-roundtrip
/// requirement as `capture::wlr::enumerate_outputs`. A final non-blocking pass mops up anything
/// only queued, not yet dispatched.
fn settle(queue: &mut EventQueue<AppData>, data: &mut AppData) -> Result<()> {
    queue.roundtrip(data)?;
    queue.roundtrip(data)?;
    queue.dispatch_pending(data)?;
    Ok(())
}

/// Connect, bind the manager (and `OutputState`), and return the not-yet-dispatched queue.
/// Fails with a plain `Err` if the compositor doesn't advertise the manager at all.
fn connect_and_bind() -> Result<(EventQueue<AppData>, AppData)> {
    let conn = Connection::connect_to_env().context("connecting to wayland display")?;
    let (globals, queue) = registry_queue_init::<AppData>(&conn)?;
    let qh = queue.handle();
    let registry_state = RegistryState::new(&globals);
    let output_state = OutputState::new(&globals, &qh);
    let manager: ZwlrForeignToplevelManagerV1 = globals
        .bind(&qh, 1..=3, ManagerData)
        .map_err(|_| anyhow!("compositor does not advertise wlr-foreign-toplevel-management"))?;
    Ok((
        queue,
        AppData {
            registry_state,
            output_state,
            manager,
            toplevels: Vec::new(),
        },
    ))
}

/// Turn the raw `AppData` accumulator into the public snapshot type, resolving each toplevel's
/// `wl_output` proxies into names and dropping anything not yet `done` or already `closed`.
fn collect_snapshot(data: &AppData) -> Vec<ToplevelSnapshot> {
    let output_state = &data.output_state;
    data.toplevels
        .iter()
        .filter(|b| b.done && !b.closed)
        .map(|b| ToplevelSnapshot {
            id: b.handle.id().to_string(),
            title: b.title.clone(),
            app_id: b.app_id.clone(),
            activated: b.activated,
            minimized: b.minimized,
            outputs: b
                .outputs
                .iter()
                .filter_map(|o| output_state.info(o)?.name)
                .collect(),
        })
        .collect()
}

/// Best-effort: if more than one toplevel reports `activated` (see the module docs), the last
/// one encountered wins — picking *a* window beats picking none, and there's no ordering
/// guarantee to prefer any particular one.
fn pick_activated(snapshot: &[ToplevelSnapshot]) -> Option<&ToplevelSnapshot> {
    snapshot.iter().rfind(|t| t.activated)
}

/// Decode a `state` event's raw byte array into the two flags this backend cares about. The
/// wire array is a sequence of native-endian `u32`s (per `wl_array`'s convention), each one of
/// `zwlr_foreign_toplevel_handle_v1::State`'s values.
fn parse_state_bits(raw: &[u8]) -> (bool /* activated */, bool /* minimized */) {
    let mut activated = false;
    let mut minimized = false;
    for chunk in raw.as_chunks::<4>().0 {
        let value = u32::from_ne_bytes(*chunk);
        match zwlr_foreign_toplevel_handle_v1::State::try_from(value) {
            Ok(zwlr_foreign_toplevel_handle_v1::State::Activated) => activated = true,
            Ok(zwlr_foreign_toplevel_handle_v1::State::Minimized) => minimized = true,
            _ => {}
        }
    }
    (activated, minimized)
}

/// The Wayland display socket path, standing in for a "socket path" in the `doctor` report:
/// this backend has no IPC socket of its own, just the Wayland connection every process using
/// Wayland already has.
fn wayland_display_path() -> Result<PathBuf> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .context("XDG_RUNTIME_DIR is not set; not running in a systemd/XDG-compliant session")?;
    let display = std::env::var("WAYLAND_DISPLAY")
        .context("WAYLAND_DISPLAY is not set; snypr does not appear to be running under Wayland")?;
    Ok(PathBuf::from(runtime_dir).join(display))
}

/// A settled, immutable view of one toplevel — what [`collect_snapshot`] hands to the trait
/// methods above, decoupled from the live Wayland proxies in [`ToplevelBuilder`].
#[derive(Debug, Clone)]
struct ToplevelSnapshot {
    id: String,
    title: String,
    app_id: String,
    activated: bool,
    minimized: bool,
    /// Output names this toplevel is currently visible on, in `output_enter` order.
    outputs: Vec<String>,
}

// ---------------------------------------------------------------------------
// subscribe_focus: a live connection, polled for readiness via `AsyncFd`
// ---------------------------------------------------------------------------

/// Subscribe to focused-output changes.
///
/// Spawns a task on `handle` that keeps a single Wayland connection open and publishes the
/// activated toplevel's first output name to the returned [`watch::Receiver`] every time it
/// changes. Unlike the Hyprland/Sway backends (which poll an async Unix-socket read that
/// `tokio::select!` can cancel outright), a plain `wayland_client::EventQueue` has no async
/// integration on its own (this crate deliberately does not enable `smithay-client-toolkit`'s
/// `calloop` feature — see `Cargo.toml`). Instead, [`pump_focus_events`] wraps the connection's
/// raw fd in [`tokio::io::unix::AsyncFd`] and awaits its readiness directly, which gets the same
/// `select!`-cancellable behavior without a second event loop.
///
/// Best-effort: the task stops when `shutdown` fires, all receivers are dropped, or the
/// connection closes/errors. Failures are logged at debug/warn and never propagated — callers
/// keep working with a static (non-following) toolbar.
fn subscribe_focus(
    handle: &tokio::runtime::Handle,
    shutdown: oneshot::Receiver<()>,
) -> watch::Receiver<Option<String>> {
    let (tx, rx) = watch::channel(None);
    handle.spawn(async move {
        tokio::select! {
            _ = shutdown => {}
            _ = pump_focus_events(&tx) => {}
        }
        tracing::debug!("wlr-foreign-toplevel focus subscription stopped");
    });
    rx
}

/// A newtype so a bare `RawFd` (the Wayland connection's socket, stable for its lifetime) can
/// be handed to [`tokio::io::unix::AsyncFd`] without implying we own or should close it — the
/// live [`wayland_client::Connection`] captured in this same async task's local variables does.
struct BorrowedConnFd(RawFd);

impl AsRawFd for BorrowedConnFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

/// Connect, settle the initial toplevel batch, then loop: publish the current focused output,
/// wait for the connection's fd to become readable, read + dispatch, repeat. Returns (letting
/// the caller's `select!` log it) on any I/O error or once every receiver is dropped.
async fn pump_focus_events(tx: &watch::Sender<Option<String>>) {
    let (mut queue, mut data) = match connect_and_bind() {
        Ok(t) => t,
        Err(err) => {
            tracing::debug!(error = %err, "wlr-foreign-toplevel unavailable; focus-follow disabled");
            return;
        }
    };
    if let Err(err) = settle(&mut queue, &mut data) {
        tracing::warn!(error = %err, "wlr-foreign-toplevel initial settle failed");
        return;
    }

    let raw_fd = match queue.prepare_read() {
        Some(guard) => guard.connection_fd().as_raw_fd(),
        // Unlikely right after `settle`'s final `dispatch_pending`, but be safe: something
        // else queued events between then and now.
        None => {
            if let Err(err) = queue.dispatch_pending(&mut data) {
                tracing::warn!(error = %err, "wlr-foreign-toplevel dispatch failed");
                return;
            }
            match queue.prepare_read() {
                Some(guard) => guard.connection_fd().as_raw_fd(),
                None => {
                    tracing::warn!(
                        "wlr-foreign-toplevel: could not obtain a pollable connection fd"
                    );
                    return;
                }
            }
        }
    };
    let async_fd = match tokio::io::unix::AsyncFd::new(BorrowedConnFd(raw_fd)) {
        Ok(f) => f,
        Err(err) => {
            tracing::warn!(error = %err, "wlr-foreign-toplevel: registering the connection fd with tokio failed");
            return;
        }
    };

    let mut last = current_focused_output(&data);
    if tx.send(last.clone()).is_err() {
        return;
    }

    loop {
        if let Err(err) = queue.flush() {
            tracing::warn!(error = %err, "flushing the wlr-foreign-toplevel connection failed");
            return;
        }
        let Some(read_guard) = queue.prepare_read() else {
            // Events were already buffered (e.g. from the fallback dispatch above, or a
            // previous wake-up that queued more than it consumed): drain them without waiting
            // on the fd again.
            if let Err(err) = queue.dispatch_pending(&mut data) {
                tracing::warn!(error = %err, "wlr-foreign-toplevel dispatch failed");
                return;
            }
            let next = current_focused_output(&data);
            if next != last {
                last = next.clone();
                if tx.send(next).is_err() {
                    return;
                }
            }
            continue;
        };
        let mut ready = match async_fd.readable().await {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(error = %err, "waiting on the wlr-foreign-toplevel connection fd failed");
                return;
            }
        };
        if let Err(err) = read_guard.read() {
            tracing::warn!(error = %err, "reading wlr-foreign-toplevel events failed");
            return;
        }
        ready.clear_ready();
        if let Err(err) = queue.dispatch_pending(&mut data) {
            tracing::warn!(error = %err, "wlr-foreign-toplevel dispatch failed");
            return;
        }
        let next = current_focused_output(&data);
        if next != last {
            last = next.clone();
            if tx.send(next).is_err() {
                return;
            }
        }
    }
}

/// The activated toplevel's first output name, if any — recomputed from scratch on every
/// focus-relevant event. Cosmetic feature, infrequent events: simplicity over micro-optimizing
/// away the snapshot allocation (same trade-off `sway::pump_focus_events` documents for its own
/// per-event re-query).
fn current_focused_output(data: &AppData) -> Option<String> {
    let snapshot = collect_snapshot(data);
    pick_activated(&snapshot).and_then(|t| t.outputs.first().cloned())
}

// ---------------------------------------------------------------------------
// sctk plumbing
// ---------------------------------------------------------------------------

struct AppData {
    registry_state: RegistryState,
    output_state: OutputState,
    // Never read, but must stay alive for the connection's lifetime: dropping the manager
    // proxy would let the backend destroy the object and stop sending `toplevel` events.
    #[allow(dead_code)]
    manager: ZwlrForeignToplevelManagerV1,
    toplevels: Vec<ToplevelBuilder>,
}

/// Mutable accumulator for one toplevel handle's events, pushed the moment the manager's
/// `Toplevel` event hands us the (already-created, via `event_created_child`) child proxy.
/// Mirrors `capture::wlr::FrameSlot`: state lives centrally in [`AppData`], matched by proxy
/// identity, rather than in each proxy's own per-object user data.
struct ToplevelBuilder {
    handle: ZwlrForeignToplevelHandleV1,
    title: String,
    app_id: String,
    activated: bool,
    minimized: bool,
    outputs: Vec<wl_output::WlOutput>,
    /// Set once the compositor has sent every initial event for this toplevel.
    done: bool,
    closed: bool,
}

impl ToplevelBuilder {
    fn new(handle: ZwlrForeignToplevelHandleV1) -> Self {
        Self {
            handle,
            title: String::new(),
            app_id: String::new(),
            activated: false,
            minimized: false,
            outputs: Vec::new(),
            done: false,
            closed: false,
        }
    }
}

/// User data attached to the bound `zwlr_foreign_toplevel_manager_v1` global.
#[derive(Default)]
struct ManagerData;

/// User data attached to each `zwlr_foreign_toplevel_handle_v1` the manager hands us. Stateless
/// on purpose — the actual accumulator lives in `AppData::toplevels` (see [`ToplevelBuilder`]).
#[derive(Default)]
struct HandleData;

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

impl Dispatch2<ZwlrForeignToplevelManagerV1, AppData> for ManagerData {
    fn event(
        &self,
        state: &mut AppData,
        _proxy: &ZwlrForeignToplevelManagerV1,
        event: <ZwlrForeignToplevelManagerV1 as wayland_client::Proxy>::Event,
        _conn: &Connection,
        _qh: &QueueHandle<AppData>,
    ) {
        // `Finished` (a destructor) needs no handling: the manager proxy is invalidated by the
        // backend once the server-side destroy lands, and we never issue further requests on
        // it from a one-shot query anyway.
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.push(ToplevelBuilder::new(toplevel));
        }
    }

    wayland_client::event_created_child!(AppData, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, HandleData)
    ]);
}

impl Dispatch2<ZwlrForeignToplevelHandleV1, AppData> for HandleData {
    fn event(
        &self,
        state: &mut AppData,
        handle: &ZwlrForeignToplevelHandleV1,
        event: <ZwlrForeignToplevelHandleV1 as wayland_client::Proxy>::Event,
        _conn: &Connection,
        _qh: &QueueHandle<AppData>,
    ) {
        let Some(builder) = state.toplevels.iter_mut().find(|b| &b.handle == handle) else {
            return;
        };
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => builder.title = title,
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => builder.app_id = app_id,
            zwlr_foreign_toplevel_handle_v1::Event::OutputEnter { output } => {
                builder.outputs.push(output);
            }
            zwlr_foreign_toplevel_handle_v1::Event::OutputLeave { output } => {
                builder.outputs.retain(|o| o != &output);
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: raw } => {
                let (activated, minimized) = parse_state_bits(&raw);
                builder.activated = activated;
                builder.minimized = minimized;
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => builder.done = true,
            zwlr_foreign_toplevel_handle_v1::Event::Closed => builder.closed = true,
            _ => {}
        }
    }
}

smithay_client_toolkit::delegate_registry!(AppData);
smithay_client_toolkit::delegate_dispatch2!(AppData);

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    fn snap(id: &str, activated: bool, outputs: &[&str]) -> ToplevelSnapshot {
        ToplevelSnapshot {
            id: id.to_owned(),
            title: "t".into(),
            app_id: "c".into(),
            activated,
            minimized: false,
            outputs: outputs.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn pick_activated_returns_none_without_any_activated_toplevel() {
        let snapshot = vec![snap("a", false, &["DP-1"]), snap("b", false, &[])];
        assert!(pick_activated(&snapshot).is_none());
    }

    #[test]
    fn pick_activated_returns_the_sole_activated_toplevel() {
        let snapshot = vec![snap("a", false, &[]), snap("b", true, &["DP-1"])];
        assert_eq!(pick_activated(&snapshot).map(|t| t.id.as_str()), Some("b"));
    }

    #[test]
    fn pick_activated_prefers_the_last_one_when_several_report_activated() {
        // The protocol doesn't forbid more than one `activated` toplevel; this is a
        // documented best-effort choice, not a correctness guarantee.
        let snapshot = vec![snap("a", true, &["DP-1"]), snap("b", true, &["HDMI-A-1"])];
        assert_eq!(pick_activated(&snapshot).map(|t| t.id.as_str()), Some("b"));
    }

    #[rstest]
    // Activated only.
    #[case(&[2u32], (true, false))]
    // Minimized only.
    #[case(&[1u32], (false, true))]
    // Maximized (0) and fullscreen (3) are decoded but don't set either flag.
    #[case(&[0u32, 3u32], (false, false))]
    // Both, regardless of order.
    #[case(&[1u32, 2u32], (true, true))]
    #[case(&[2u32, 1u32], (true, true))]
    // Empty array: no state at all.
    #[case(&[], (false, false))]
    fn parse_state_bits_decodes_the_flags_this_backend_cares_about(
        #[case] values: &[u32],
        #[case] expected: (bool, bool),
    ) {
        let raw: Vec<u8> = values.iter().flat_map(|v| v.to_ne_bytes()).collect();
        assert_eq!(parse_state_bits(&raw), expected);
    }

    #[test]
    fn parse_state_bits_ignores_a_truncated_trailing_chunk() {
        // A malformed/truncated array must not panic; `as_chunks::<4>()` already drops the
        // remainder for us.
        let mut raw: Vec<u8> = 2u32.to_ne_bytes().to_vec();
        raw.push(0xff);
        assert_eq!(parse_state_bits(&raw), (true, false));
    }

    /// Force a hermetic environment for the `wayland_display_path` tests. Safe because
    /// nextest runs every test in its own process.
    fn set_wayland_env(runtime_dir: Option<&str>, display: Option<&str>) {
        unsafe {
            match runtime_dir {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
            match display {
                Some(v) => std::env::set_var("WAYLAND_DISPLAY", v),
                None => std::env::remove_var("WAYLAND_DISPLAY"),
            }
        }
    }

    #[test]
    fn wayland_display_path_combines_runtime_dir_and_display() {
        set_wayland_env(Some("/run/user/1000"), Some("wayland-1"));
        assert_eq!(
            wayland_display_path().unwrap(),
            std::path::PathBuf::from("/run/user/1000/wayland-1")
        );
    }

    #[test]
    fn wayland_display_path_requires_xdg_runtime_dir() {
        set_wayland_env(None, Some("wayland-1"));
        let err = wayland_display_path().unwrap_err();
        assert!(err.to_string().contains("XDG_RUNTIME_DIR"), "{err}");
    }

    #[test]
    fn wayland_display_path_requires_wayland_display() {
        set_wayland_env(Some("/run/user/1000"), None);
        let err = wayland_display_path().unwrap_err();
        assert!(err.to_string().contains("WAYLAND_DISPLAY"), "{err}");
    }
}
