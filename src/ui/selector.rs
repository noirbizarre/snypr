//! Interactive region selector overlay.
//!
//! Renders one fullscreen `gtk4_layer_shell` window per monitor at `Layer::Overlay`. All
//! overlays share a single selection state (current rectangle + owning monitor + mode) so that
//! starting a new drag on any monitor cancels the previous rectangle, and a mode change in the
//! floating toolbar is reflected on every screen.
//!
//! The first monitor in `display.monitors()` hosts a floating bottom-center
//! [`crate::ui::Toolbar`] with mode toggles, a cursor toggle, and a `Capture` action button.
//! Other monitors show only the dimming/HUD layer.
//!
//! Workflow per mode:
//!   - `Screen` (default): hover a monitor to highlight it, click to commit a per-monitor capture.
//!   - `Region`: drag a rectangle, then press Enter / click Capture.
//!   - `Window`: focused-window bounds outlined; click Capture to commit.
//!   - `Full`: every monitor dim-highlighted as one stitched bbox; click Capture to commit.
//!
//! Esc cancels at any time.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use gtk4::glib;
use gtk4::glib::subclass::prelude::*;
use gtk4::graphene;
use gtk4::pango;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::capture::Selection;
use crate::capture::region::Rect;
use crate::context::Ctx;
use crate::hypr::{self, HyprWindow};
use crate::i18n::fl;
use crate::ui::toolbar::{ModeKind, SELECTOR_MODES, Toolbar, ToolbarAction, ToolbarSpec};

/// Marker error indicating the user dismissed the interactive selector
/// (e.g. pressing Escape). Detected in `main` to exit 0 without logging
/// at error level or emitting a desktop notification.
#[derive(Debug, Clone, Copy)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled by user")
    }
}

impl std::error::Error for Cancelled {}

/// Result of an interactive selector session.
#[derive(Clone, Debug)]
pub struct SelectorOutcome {
    /// Pre-resolution selection: `Region`, `Full`, `Output(name)`, or `Window`.
    /// Compositor-aware variants are resolved by the caller (e.g. `run_capture_flow`).
    pub selection: Selection,
    /// Final cursor toggle from the floating toolbar; overrides any CLI default.
    pub cursor: bool,
    /// Final pre-capture delay picked on the toolbar's delay spinner. Whole seconds (sub-
    /// second precision lives only in the CLI `--delay` flag and isn't surfaced in the UI).
    /// `Duration::ZERO` means no sleep.
    pub delay: std::time::Duration,
    /// User asked to open the annotation editor on the captured image (clicked the Annotate
    /// button or pressed Shift+Enter). Distinct from any CLI-level `--edit` flag: the button
    /// choice wins, so Capture always reports `false` here regardless of how the selector was
    /// invoked.
    pub edit: bool,
}

/// Show the selector and return the chosen selection + cursor toggle. The toolbar's cursor
/// toggle is seeded from `initial_cursor`.
///
/// Standalone entry point: stands up a private `gtk4::Application`, drives its main loop on a
/// blocking thread, and tears the app down on commit / cancel. Use this from CLI flows that
/// own the process. From inside an already-running overlay (e.g. the draw overlay's Save
/// action), call [`pick_region_in_app`] instead so we don't try to spin up a second GTK app.
///
/// `allow_annotate` controls whether the Capture button honors the Shift modifier as a
/// "route through the annotation editor" shortcut. The standalone `hyprsnap screenshot`
/// flow passes `true` (preserving Shift+click / Shift+Enter → Annotate); callers that
/// already provide an annotation surface (the draw overlay) pass `false` so Shift behaves
/// as a no-op modifier.
pub async fn pick_region(
    ctx: Ctx,
    initial_cursor: bool,
    initial_delay: std::time::Duration,
    allow_annotate: bool,
) -> Result<SelectorOutcome> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<SelectorOutcome>>();
    let clients = fetch_clients_or_warn().await;
    let focused_monitor = fetch_focused_monitor_or_log().await;
    let focused_window = fetch_active_window_or_log().await;
    let style = ctx.config.ui.selector.clone();
    tokio::task::spawn_blocking(move || {
        run_gtk(
            tx,
            initial_cursor,
            initial_delay,
            clients,
            focused_monitor,
            focused_window,
            allow_annotate,
            style,
        )
    })
    .await
    .map_err(|e| anyhow!("selector task panicked: {e}"))??;
    let result = rx
        .await
        .map_err(|e| anyhow!("selector channel closed without a result: {e}"))?;
    if result.is_ok() {
        let grace = post_dismiss_grace();
        tracing::debug!(
            grace_ms = grace.as_millis() as u64,
            "selector dismissed; waiting for compositor to unmap before capture"
        );
        tokio::time::sleep(grace).await;
    }
    result
}

/// Embeddable entry point: build the selector inside an existing `gtk4::Application` and
/// resolve when the user commits or cancels. The selector destroys its own per-monitor
/// windows on resolve but does **not** call `app.quit()` — the caller's app keeps running so
/// any sibling overlays (e.g. the draw overlay) stay alive.
///
/// Must be called from the GTK main context (typically via `glib::MainContext::spawn_local`).
/// The 30 ms post-commit grace is honored here too so callers can immediately invoke
/// `zwlr_screencopy` without the selector's dim veil leaking into the captured frame.
///
/// See [`pick_region`] for the semantics of `allow_annotate`.
pub async fn pick_region_in_app(
    app: &gtk4::Application,
    initial_cursor: bool,
    initial_delay: std::time::Duration,
    allow_annotate: bool,
    style: crate::config::SelectorStyleConfig,
) -> Result<SelectorOutcome> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<SelectorOutcome>>();
    let tx: Sender = Arc::new(Mutex::new(Some(tx)));
    let clients = fetch_clients_or_warn().await;
    let focused_monitor = fetch_focused_monitor_or_log().await;
    let focused_window = fetch_active_window_or_log().await;
    // Empty WeakRef → commit/cancel's `app.upgrade()` returns None → no `app.quit()` fires
    // against the caller's app. Windows are still parented to `app` (required by GTK), but
    // they're destroyed by `dismiss_overlays` so they don't keep the app alive on their own.
    let quit_target: glib::WeakRef<gtk4::Application> = glib::WeakRef::new();
    if let Err(err) = build_overlays(
        app,
        &tx,
        initial_cursor,
        initial_delay,
        quit_target,
        clients,
        focused_monitor,
        focused_window,
        allow_annotate,
        style,
    ) {
        send_once(&tx, Err(err));
    }
    let result = rx
        .await
        .map_err(|e| anyhow!("selector channel closed without a result: {e}"))?;
    if result.is_ok() {
        let grace = post_dismiss_grace();
        tracing::debug!(
            grace_ms = grace.as_millis() as u64,
            "embeddable selector dismissed; waiting for compositor to unmap before capture"
        );
        glib::timeout_future(grace).await;
    }
    result
}

/// Post-dismiss grace window. Defaults to 30 ms but can be overridden at runtime via the
/// `HYPRSNAP_CAPTURE_GRACE_MS` env var. The two-phase `blank_and_dismiss` teardown should
/// already make this grace mostly redundant — the surface is fully transparent before
/// destroy, so Hyprland's `fadeOut` has nothing to leak — but a tiny safety margin doesn't
/// hurt and gives users on unusually slow compositors a way to dial it up further.
fn post_dismiss_grace() -> std::time::Duration {
    std::time::Duration::from_millis(
        std::env::var("HYPRSNAP_CAPTURE_GRACE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30),
    )
}

/// Per-monitor descriptor needed by signal handlers.
#[derive(Clone, Debug)]
struct MonitorInfo {
    index: usize,
    connector: Option<String>,
}

/// A window picked from the cached Hyprland clients list, carried as the "selected" zone in
/// Window mode. Stored in compositor logical coordinates so painting can translate to each
/// per-monitor widget-local space without re-querying Hyprland.
#[derive(Clone, Debug)]
struct PickedWindow {
    rect: Rect,
    title: String,
    class: String,
}

impl From<&HyprWindow> for PickedWindow {
    fn from(w: &HyprWindow) -> Self {
        Self {
            rect: w.rect(),
            title: w.title.clone(),
            class: w.class.clone(),
        }
    }
}

/// Shared selection state visible to every monitor overlay.
#[derive(Clone, Debug, Default)]
struct SharedSelection {
    /// Owner of the current dragged rectangle (Region mode only).
    owner: Option<usize>,
    start: Option<(f64, f64)>,
    current: Option<(f64, f64)>,
    /// Active mode picker (driven by the floating toolbar).
    mode: ModeKind,
    /// Cursor toggle from the floating toolbar; final value reported in `SelectorOutcome`.
    cursor: bool,
    /// Pre-capture delay (whole seconds, internally stored as `Duration`) from the floating
    /// toolbar's spinner; final value reported in `SelectorOutcome`.
    delay: std::time::Duration,
    /// Monitor currently under the pointer (Screen mode highlight).
    hover_monitor: Option<usize>,
    /// Monitor explicitly picked by clicking (Screen mode). `None` until the user clicks; then
    /// remains set until they click a different monitor.
    selected_monitor: Option<usize>,
    /// Window currently under the pointer (Window mode hover outline).
    hover_window: Option<PickedWindow>,
    /// Window explicitly picked by clicking (Window mode).
    selected_window: Option<PickedWindow>,
}

impl SharedSelection {
    fn rect_local(&self) -> Option<(f64, f64, f64, f64)> {
        let (sx, sy) = self.start?;
        let (cx, cy) = self.current?;
        let x = sx.min(cx);
        let y = sy.min(cy);
        let w = (sx - cx).abs();
        let h = (sy - cy).abs();
        if w < 1.0 || h < 1.0 {
            None
        } else {
            Some((x, y, w, h))
        }
    }
}

type SelectionCell = Rc<RefCell<SharedSelection>>;
type Sender = Arc<Mutex<Option<tokio::sync::oneshot::Sender<Result<SelectorOutcome>>>>>;
type AreaRegistry = Rc<RefCell<Vec<SelectorOverlay>>>;
type WindowRegistry = Rc<RefCell<Vec<gtk4::ApplicationWindow>>>;
type MonitorList = Rc<RefCell<Vec<MonitorInfo>>>;
type ToolbarRegistry = Rc<RefCell<Vec<Toolbar>>>;
type ClientList = Rc<RefCell<Vec<HyprWindow>>>;

fn send_once(tx: &Sender, msg: Result<SelectorOutcome>) {
    if let Ok(mut guard) = tx.lock()
        && let Some(sender) = guard.take()
    {
        let _ = sender.send(msg);
    }
}

fn redraw_all(areas: &AreaRegistry) {
    for area in areas.borrow().iter() {
        area.queue_draw();
    }
}

/// Translate widget-local coordinates on monitor `monitor_index` to compositor logical
/// coordinates by adding the monitor's logical offset from `gdk::Monitor::geometry()`.
fn local_to_logical(monitor_index: usize, x: f64, y: f64) -> Option<(i32, i32)> {
    let display = gdk4::Display::default()?;
    let monitors = display.monitors();
    let obj = monitors.item(monitor_index as u32)?;
    let monitor = obj.downcast::<gdk4::Monitor>().ok()?;
    let geo = monitor.geometry();
    Some((geo.x() + x.round() as i32, geo.y() + y.round() as i32))
}

/// Inverse of `local_to_logical`: subtract the monitor's logical offset from a rect expressed
/// in compositor logical coordinates so it can be drawn into the monitor's widget. Returns
/// `None` if the monitor index doesn't resolve. Negative widget coordinates are kept so the
/// snapshot code can clip naturally; the caller decides whether the rect intersects the
/// widget.
fn logical_to_local(monitor_index: usize, rect: &Rect) -> Option<(f32, f32, f32, f32)> {
    let display = gdk4::Display::default()?;
    let monitors = display.monitors();
    let obj = monitors.item(monitor_index as u32)?;
    let monitor = obj.downcast::<gdk4::Monitor>().ok()?;
    let geo = monitor.geometry();
    Some((
        (rect.x - geo.x()) as f32,
        (rect.y - geo.y()) as f32,
        rect.w as f32,
        rect.h as f32,
    ))
}

/// Try to snapshot Hyprland's client list. On failure (host isn't Hyprland, socket
/// missing, IPC error) we log a warning and return an empty list — Window mode then falls
/// back to its legacy "capture focused window" behavior via `Selection::Window`.
async fn fetch_clients_or_warn() -> Vec<HyprWindow> {
    match hypr::clients().await {
        Ok(list) => list,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "could not fetch Hyprland clients; Window mode will fall back to focused-window capture"
            );
            Vec::new()
        }
    }
}

/// Try to read Hyprland's focused monitor name. Used to pre-select the current monitor
/// in Screen mode. Failure (non-Hyprland host, IPC error, no focused monitor) is silently
/// dropped so the selector still opens with the legacy "click to pick" UX.
async fn fetch_focused_monitor_or_log() -> Option<String> {
    match hypr::focused_monitor().await {
        Ok(name) => Some(name),
        Err(err) => {
            tracing::debug!(
                error = %err,
                "could not determine focused Hyprland monitor; selector will open with no monitor pre-selected"
            );
            None
        }
    }
}

/// Try to read Hyprland's active window. Used to pre-select the current window in Window
/// mode. Failure (non-Hyprland host, IPC error, no focused client) is silently dropped.
async fn fetch_active_window_or_log() -> Option<hypr::ActiveWindow> {
    match hypr::active_window().await {
        Ok(aw) => Some(aw),
        Err(err) => {
            tracing::debug!(
                error = %err,
                "could not determine active Hyprland window; selector will open with no window pre-selected"
            );
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_gtk(
    tx: tokio::sync::oneshot::Sender<Result<SelectorOutcome>>,
    initial_cursor: bool,
    initial_delay: std::time::Duration,
    clients: Vec<HyprWindow>,
    focused_monitor: Option<String>,
    focused_window: Option<hypr::ActiveWindow>,
    allow_annotate: bool,
    style: crate::config::SelectorStyleConfig,
) -> Result<()> {
    let app = gtk4::Application::builder()
        .application_id(crate::ui::APP_ID)
        .build();
    let tx: Sender = Arc::new(Mutex::new(Some(tx)));

    {
        let tx = tx.clone();
        let clients = Rc::new(RefCell::new(clients));
        let focused_monitor = Rc::new(RefCell::new(focused_monitor));
        let focused_window = Rc::new(RefCell::new(focused_window));
        let style = Rc::new(style);
        app.connect_activate(move |app| {
            crate::ui::install_icon_resources();
            // Standalone: pass `app.downgrade()` as the quit target so commit / cancel tear
            // down the private app once the user finishes (or escapes). The embeddable
            // `pick_region_in_app` path passes an empty WeakRef to leave the caller's app
            // alone.
            let quit_target = app.downgrade();
            let clients_snapshot = clients.borrow().clone();
            let focused_monitor_snapshot = focused_monitor.borrow().clone();
            let focused_window_snapshot = focused_window.borrow().clone();
            if let Err(err) = build_overlays(
                app,
                &tx,
                initial_cursor,
                initial_delay,
                quit_target,
                clients_snapshot,
                focused_monitor_snapshot,
                focused_window_snapshot,
                allow_annotate,
                (*style).clone(),
            ) {
                send_once(&tx, Err(err));
                app.quit();
            }
        });
    }

    let exit = app.run_with_args::<&str>(&[]);
    let code: i32 = exit.into();
    send_once(&tx, Err(anyhow!("selector closed without a selection")));
    if code != 0 {
        bail!("GTK exited with status {code}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_overlays(
    app: &gtk4::Application,
    tx: &Sender,
    initial_cursor: bool,
    initial_delay: std::time::Duration,
    quit_target: glib::WeakRef<gtk4::Application>,
    clients: Vec<HyprWindow>,
    focused_monitor: Option<String>,
    focused_window: Option<hypr::ActiveWindow>,
    allow_annotate: bool,
    style: crate::config::SelectorStyleConfig,
) -> Result<()> {
    crate::ui::style::install();

    let display = gdk4::Display::default().ok_or_else(|| anyhow!("no GDK display available"))?;
    let monitors_list = display.monitors();
    let n = monitors_list.n_items();
    if n == 0 {
        bail!("no monitors reported by GDK");
    }

    // Build the per-monitor info list up front so we can resolve the focused monitor's
    // connector → index mapping before seeding the shared selection state.
    let mut monitor_infos: Vec<(MonitorInfo, gdk4::Monitor)> = Vec::with_capacity(n as usize);
    for i in 0..n {
        let Some(obj) = monitors_list.item(i) else {
            continue;
        };
        let Ok(monitor) = obj.downcast::<gdk4::Monitor>() else {
            continue;
        };
        let info = MonitorInfo {
            index: i as usize,
            connector: monitor.connector().map(|s| s.to_string()),
        };
        monitor_infos.push((info, monitor));
    }

    // Pre-select the focused monitor (Screen mode default) so users can hit Enter without
    // an extra click. Silent fallback to `None` if Hyprland didn't report a focused
    // monitor or the connector doesn't match any GDK monitor.
    let selected_monitor = focused_monitor.as_deref().and_then(|name| {
        monitor_infos
            .iter()
            .find(|(info, _)| info.connector.as_deref() == Some(name))
            .map(|(info, _)| info.index)
    });

    // Pre-select the focused window (Window mode default). Built from `ActiveWindow`
    // directly so we don't depend on a `focused` flag in the clients snapshot.
    let selected_window = focused_window.as_ref().map(|aw| PickedWindow {
        rect: aw.rect(),
        title: aw.title.clone(),
        class: aw.class.clone(),
    });

    let shared = SharedState {
        selection: Rc::new(RefCell::new(SharedSelection {
            cursor: initial_cursor,
            delay: initial_delay,
            selected_monitor,
            selected_window,
            ..SharedSelection::default()
        })),
        finalised: Rc::new(RefCell::new(false)),
        areas: Rc::new(RefCell::new(Vec::new())),
        windows: Rc::new(RefCell::new(Vec::new())),
        monitors: Rc::new(RefCell::new(Vec::new())),
        tx: tx.clone(),
        app_weak: quit_target,
        toolbars: Rc::new(RefCell::new(Vec::new())),
        countdown_source: Rc::new(RefCell::new(None)),
        initial_cursor,
        initial_delay,
        allow_annotate,
        clients: Rc::new(RefCell::new(clients)),
    };

    let mut windows = Vec::with_capacity(monitor_infos.len());
    let style = Rc::new(style);
    for (info, monitor) in monitor_infos {
        shared.monitors.borrow_mut().push(info.clone());
        windows.push(spawn_monitor_overlay(app, &monitor, info, &shared, &style));
    }
    // Two-phase: build every per-monitor selector window above, then commit them in a
    // tight loop so the compositor maps them in the same frame (see §23).
    for w in &windows {
        w.present();
    }
    Ok(())
}

fn spawn_monitor_overlay(
    app: &gtk4::Application,
    monitor: &gdk4::Monitor,
    info: MonitorInfo,
    shared: &SharedState,
    style: &Rc<crate::config::SelectorStyleConfig>,
) -> gtk4::ApplicationWindow {
    let geo = monitor.geometry();
    let mon_w = geo.width();
    let mon_h = geo.height();

    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .decorated(false)
        .resizable(false)
        .icon_name(crate::ui::APP_ID)
        .build();
    window.add_css_class("hyprsnap-selector");

    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_namespace(Some("hyprsnap-selector"));
    window.set_monitor(Some(monitor));
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_exclusive_zone(-1);
    // Exclusive keyboard: the selector lives or dies by its shortcuts (1/2/3/4, Enter,
    // Shift+Enter, Esc) and needs modifier state for Shift-click on Capture. Toolbar
    // buttons are non-focusable, so KeyboardMode::OnDemand would never trigger a
    // keyboard grab and Shift would always read as un-held.
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.set_default_size(mon_w.max(1), mon_h.max(1));

    let area = SelectorOverlay::new(shared.selection.clone(), info.index, (**style).clone());
    area.set_hexpand(true);
    area.set_vexpand(true);

    install_drag(&area, &shared.selection, info.index, &shared.areas);
    install_hover_and_click(&area, info.index, shared);
    install_keys(
        &window,
        &shared.selection,
        &shared.tx,
        &shared.finalised,
        &shared.windows,
        &shared.monitors,
        &shared.app_weak,
        &shared.areas,
        &shared.toolbars,
        &shared.countdown_source,
        info.clone(),
        shared.allow_annotate,
    );

    // Every monitor gets its own floating toolbar. Mode/cursor changes on any toolbar
    // propagate to all others through the wired callbacks so the UI stays consistent.
    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&area));
    let toolbar = build_toolbar(shared, info.clone());
    toolbar.widget().set_halign(gtk4::Align::Center);
    toolbar.widget().set_valign(gtk4::Align::End);
    toolbar.widget().set_margin_bottom(24);
    overlay.add_overlay(toolbar.widget());
    toolbar.install_shortcuts(&window);
    shared.toolbars.borrow_mut().push(toolbar);
    window.set_child(Some(&overlay));

    shared.areas.borrow_mut().push(area.clone());
    shared.windows.borrow_mut().push(window.clone());
    window
}

/// Build a per-monitor floating toolbar and wire its actions back into the shared selection
/// state. Mode/cursor changes are mirrored to every other monitor's toolbar so the UI stays
/// consistent regardless of which screen the user clicked.
fn build_toolbar(shared: &SharedState, primary: MonitorInfo) -> Toolbar {
    // Round the initial delay to whole seconds for the UI; sub-second precision (e.g. a
    // CLI `--delay 500ms`) is preserved only if the user never touches the spinner — the
    // selector's outcome will then carry whatever `shared.initial_delay` was set to.
    let initial_delay_secs = shared.initial_delay.as_secs().min(u32::MAX as u64) as u32;
    let toolbar = Toolbar::new(ToolbarSpec {
        modes: SELECTOR_MODES,
        show_cursor_toggle: true,
        show_delay_spinner: true,
        show_capture: true,
        capture_shift_annotates: shared.allow_annotate,
        initial_mode: Some(ModeKind::Screen),
        initial_cursor: shared.initial_cursor,
        initial_delay_secs,
        ..Default::default()
    });

    let selection = shared.selection.clone();
    let areas = shared.areas.clone();
    let tx = shared.tx.clone();
    let finalised = shared.finalised.clone();
    let windows = shared.windows.clone();
    let monitors = shared.monitors.clone();
    let app_weak = shared.app_weak.clone();
    let toolbars = shared.toolbars.clone();
    let countdown_source = shared.countdown_source.clone();
    toolbar.connect(move |action| match action {
        ToolbarAction::ModeSelected(mode) => {
            {
                let mut s = selection.borrow_mut();
                s.mode = mode;
                // Reset the dragged rectangle when leaving Region mode so the HUD doesn't
                // linger over a Full/Screen/Window selection.
                if mode != ModeKind::Region {
                    s.owner = None;
                    s.start = None;
                    s.current = None;
                }
                // Preserve per-mode selections across mode switches so the default
                // (focused monitor / window) — or whatever the user previously picked in
                // that mode — is still active when they switch back. Only `hover_window`
                // is cleared, since it's pointer-driven and would be stale until the next
                // `motion` event re-resolves it.
                s.hover_window = None;
            }
            for t in toolbars.borrow().iter() {
                t.set_mode(mode);
            }
            redraw_all(&areas);
        }
        ToolbarAction::CursorToggled(on) => {
            selection.borrow_mut().cursor = on;
            for t in toolbars.borrow().iter() {
                t.set_cursor(on);
            }
        }
        ToolbarAction::DelayChanged(secs) => {
            selection.borrow_mut().delay = std::time::Duration::from_secs(secs as u64);
            for t in toolbars.borrow().iter() {
                t.set_delay(secs);
            }
        }
        ToolbarAction::Capture => {
            commit(
                &selection,
                &tx,
                &finalised,
                &windows,
                &monitors,
                &app_weak,
                &areas,
                &toolbars,
                &countdown_source,
                Some(primary.clone()),
                false,
            );
        }
        ToolbarAction::Annotate => {
            commit(
                &selection,
                &tx,
                &finalised,
                &windows,
                &monitors,
                &app_weak,
                &areas,
                &toolbars,
                &countdown_source,
                Some(primary.clone()),
                true,
            );
        }
        _ => {}
    });
    toolbar
}

/// Lifetimes-shared bag of per-call state passed down through `spawn_monitor_overlay`.
struct SharedState {
    selection: SelectionCell,
    finalised: Rc<RefCell<bool>>,
    areas: AreaRegistry,
    windows: WindowRegistry,
    monitors: MonitorList,
    tx: Sender,
    /// Target of `app.quit()` from `commit` / `cancel`. Set to `app.downgrade()` by the
    /// standalone path (`pick_region`), kept empty by the embeddable path
    /// (`pick_region_in_app`) so the caller's app keeps running after the selector resolves.
    app_weak: glib::WeakRef<gtk4::Application>,
    toolbars: ToolbarRegistry,
    /// `glib::SourceId` of the in-flight 1-second countdown timer, if any. Held so that
    /// pressing Escape during a pre-capture delay cancels both the timer and the eventual
    /// capture. Lives in a `RefCell<Option<_>>` because `SourceId::remove` consumes the
    /// id by value.
    countdown_source: Rc<RefCell<Option<glib::SourceId>>>,
    initial_cursor: bool,
    /// Seed value for the toolbar's delay spinner, propagated to every per-monitor toolbar at
    /// construction. Sourced from the CLI `--delay` flag or, failing that, the
    /// `[capture].delay` config entry. Sub-second values are rounded to the nearest second
    /// for display; the CLI / config value is only used end-to-end when the user does not
    /// touch the spinner.
    initial_delay: std::time::Duration,
    /// When `false`, the Capture button on every per-monitor toolbar ignores the Shift
    /// modifier and the window-level Enter handler always commits with `edit=false`. Set
    /// by the draw overlay's Save flow so Shift+click / Shift+Enter just save the snapshot
    /// instead of redundantly opening another annotation editor.
    allow_annotate: bool,
    /// Snapshot of Hyprland's client list at selector start, used by Window mode for
    /// cursor-based hit-testing. Empty when the query failed or the host isn't Hyprland — in
    /// that case Window mode falls back to the legacy "capture focused window" behavior.
    clients: ClientList,
}

fn install_drag(
    area: &SelectorOverlay,
    selection: &SelectionCell,
    monitor_index: usize,
    areas: &AreaRegistry,
) {
    let drag = gtk4::GestureDrag::new();

    {
        let selection = selection.clone();
        let areas = areas.clone();
        drag.connect_drag_begin(move |g, x, y| {
            let mode = selection.borrow().mode;
            if mode != ModeKind::Region {
                g.reset();
                return;
            }
            let mut s = selection.borrow_mut();
            s.owner = Some(monitor_index);
            s.start = Some((x, y));
            s.current = Some((x, y));
            drop(s);
            redraw_all(&areas);
        });
    }
    {
        let selection = selection.clone();
        let areas = areas.clone();
        drag.connect_drag_update(move |g, dx, dy| {
            if selection.borrow().mode != ModeKind::Region {
                return;
            }
            if let Some((sx, sy)) = g.start_point() {
                let mut s = selection.borrow_mut();
                if s.owner == Some(monitor_index) {
                    s.current = Some((sx + dx, sy + dy));
                    drop(s);
                    redraw_all(&areas);
                }
            }
        });
    }
    {
        let selection = selection.clone();
        let areas = areas.clone();
        drag.connect_drag_end(move |g, dx, dy| {
            if selection.borrow().mode != ModeKind::Region {
                return;
            }
            if let Some((sx, sy)) = g.start_point() {
                let mut s = selection.borrow_mut();
                if s.owner == Some(monitor_index) {
                    s.current = Some((sx + dx, sy + dy));
                    drop(s);
                    redraw_all(&areas);
                }
            }
        });
    }
    area.add_controller(drag);
}

/// Hover tracking + click-to-select for the non-Region modes.
///
/// - `Screen`: `MotionController::enter/leave` updates `hover_monitor`. A click sets
///   `selected_monitor` so the picked screen stays outlined after the pointer moves.
/// - `Window`: `MotionController::motion` hit-tests the cached Hyprland clients list to update
///   `hover_window`. A click promotes the hovered window into `selected_window`.
/// - `Full`: clicking simply triggers a redraw — `Full` is implicitly the whole desktop, so
///   there's nothing to "select"; validation still goes through Enter / Capture / Shift+Enter
///   / Shift+click on the Capture button.
///
/// Critically, **none of these click paths call `commit` directly**: every mode goes through
/// the same Enter / Capture / Shift+Enter / Shift+Capture validation pipeline as Region.
fn install_hover_and_click(area: &SelectorOverlay, monitor_index: usize, shared: &SharedState) {
    let motion = gtk4::EventControllerMotion::new();
    {
        let selection = shared.selection.clone();
        let areas = shared.areas.clone();
        motion.connect_enter(move |_, _, _| {
            let mut s = selection.borrow_mut();
            if s.mode == ModeKind::Screen && s.hover_monitor != Some(monitor_index) {
                s.hover_monitor = Some(monitor_index);
                drop(s);
                redraw_all(&areas);
            }
        });
    }
    {
        let selection = shared.selection.clone();
        let areas = shared.areas.clone();
        motion.connect_leave(move |_| {
            let mut s = selection.borrow_mut();
            let mut changed = false;
            if s.hover_monitor == Some(monitor_index) {
                s.hover_monitor = None;
                changed = true;
            }
            if s.hover_window.is_some() {
                s.hover_window = None;
                changed = true;
            }
            if changed {
                drop(s);
                redraw_all(&areas);
            }
        });
    }
    {
        let selection = shared.selection.clone();
        let areas = shared.areas.clone();
        let clients = shared.clients.clone();
        motion.connect_motion(move |_, x, y| {
            if selection.borrow().mode != ModeKind::Window {
                return;
            }
            let Some((lx, ly)) = local_to_logical(monitor_index, x, y) else {
                return;
            };
            let next = hypr::window_at(&clients.borrow(), lx, ly).map(PickedWindow::from);
            let mut s = selection.borrow_mut();
            let changed = match (&s.hover_window, &next) {
                (Some(a), Some(b)) => a.rect != b.rect,
                (None, None) => false,
                _ => true,
            };
            if changed {
                s.hover_window = next;
                drop(s);
                redraw_all(&areas);
            }
        });
    }
    area.add_controller(motion);

    let click = gtk4::GestureClick::new();
    click.set_button(gdk4::BUTTON_PRIMARY);
    {
        let selection = shared.selection.clone();
        let areas = shared.areas.clone();
        let clients = shared.clients.clone();
        click.connect_pressed(move |_, _, x, y| {
            let mode = selection.borrow().mode;
            // Region mode arms its selection via drag; clicks are ignored here.
            if mode == ModeKind::Region {
                return;
            }
            let mut s = selection.borrow_mut();
            match mode {
                ModeKind::Screen => {
                    s.selected_monitor = Some(monitor_index);
                }
                ModeKind::Window => {
                    let picked = local_to_logical(monitor_index, x, y).and_then(|(lx, ly)| {
                        hypr::window_at(&clients.borrow(), lx, ly).map(PickedWindow::from)
                    });
                    if picked.is_some() {
                        s.selected_window = picked;
                    }
                    // Click on empty space in Window mode: keep the previous selection (if
                    // any) so accidentally missing a window doesn't blow away the user's
                    // pick. A miss is visible — no window is outlined — so the user can
                    // retry.
                }
                ModeKind::Full => {
                    // Implicit selection; nothing to update beyond the redraw triggered below.
                }
                ModeKind::Region => unreachable!(),
            }
            drop(s);
            redraw_all(&areas);
        });
    }
    area.add_controller(click);
}

#[allow(clippy::too_many_arguments)]
fn install_keys(
    window: &gtk4::ApplicationWindow,
    selection: &SelectionCell,
    tx: &Sender,
    finalised: &Rc<RefCell<bool>>,
    windows: &WindowRegistry,
    monitors: &MonitorList,
    app_weak: &glib::WeakRef<gtk4::Application>,
    areas: &AreaRegistry,
    toolbars: &ToolbarRegistry,
    countdown_source: &Rc<RefCell<Option<glib::SourceId>>>,
    info: MonitorInfo,
    allow_annotate: bool,
) {
    let key = gtk4::EventControllerKey::new();
    let selection = selection.clone();
    let tx = tx.clone();
    let finalised = finalised.clone();
    let windows = windows.clone();
    let monitors = monitors.clone();
    let app_weak = app_weak.clone();
    let areas = areas.clone();
    let toolbars = toolbars.clone();
    let countdown_source = countdown_source.clone();
    key.connect_key_pressed(move |_, k, _, modifiers| match k {
        gdk4::Key::Escape => {
            cancel(&tx, &finalised, &windows, &app_weak, &countdown_source);
            glib::Propagation::Stop
        }
        gdk4::Key::Return | gdk4::Key::KP_Enter => {
            // Shift+Enter normally routes to the annotation editor, mirroring the toolbar's
            // `ShortcutAction::Annotate` shortcut so both layers stay in sync. When the
            // selector was opened from a context that already provides an annotation
            // surface (the draw overlay), `allow_annotate` is `false` and Shift is treated
            // as a no-op modifier — every Enter just commits the snapshot.
            let edit = allow_annotate && modifiers.contains(gdk4::ModifierType::SHIFT_MASK);
            commit(
                &selection,
                &tx,
                &finalised,
                &windows,
                &monitors,
                &app_weak,
                &areas,
                &toolbars,
                &countdown_source,
                Some(info.clone()),
                edit,
            );
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    });
    window.add_controller(key);
}

/// Tear down every overlay window synchronously and flush the Wayland connection so the
/// compositor processes the unmap requests *before* we hand control back to the caller.
fn dismiss_overlays(windows: &WindowRegistry) {
    let count = windows.borrow().len();
    let t0 = std::time::Instant::now();
    for window in windows.borrow_mut().drain(..) {
        window.set_visible(false);
        window.destroy();
    }
    if let Some(display) = gdk4::Display::default() {
        display.flush();
    }
    tracing::debug!(
        count,
        elapsed_us = t0.elapsed().as_micros() as u64,
        "dismissed selector overlay windows (set_visible(false) + destroy() + display.flush())"
    );
}

/// Number of milliseconds we wait between blanking the overlay surfaces and destroying
/// them. Has to cover a couple of compositor frames so Hyprland actually composites the
/// fully-transparent state before the destroy triggers its layer `fadeOut` animation —
/// otherwise the animation interpolates from our colored chrome to nothing and leaks the
/// selector into the wlr-screencopy frame. 50 ms ≈ 3 frames at 60 Hz, well under the
/// human-perception threshold for "the selector closed instantly."
const BLANK_FRAME_MS: u64 = 50;

/// Two-phase teardown that hides the visible chrome (toolbars + dim veil + selection rect
/// + outline + legend + countdown numeral) *before* destroying the layer-shell surfaces.
///
/// Hyprland (and other wlroots compositors with layer-shell `fadeOut` animations) keeps the
/// surface composited for the duration of the animation after the client sends
/// `wl_surface.destroy`. With the colored chrome still on the surface, the fade-out frames
/// leak into the subsequent `wlr-screencopy` capture — the user sees a dimmed toolbar, the
/// region outline and the `WxH — Enter…` legend baked into the saved PNG.
///
/// By blanking the snapshot first, hiding the toolbars, flushing the connection, and only
/// then scheduling the destroy a few frames later, the surface goes into fadeOut already
/// fully transparent, so the animation has nothing to leak.
fn blank_and_dismiss(
    windows: &WindowRegistry,
    areas: &AreaRegistry,
    toolbars: &ToolbarRegistry,
    tx: &Sender,
    app_weak: &glib::WeakRef<gtk4::Application>,
    outcome: SelectorOutcome,
) {
    for t in toolbars.borrow().iter() {
        t.widget().set_visible(false);
    }
    for a in areas.borrow().iter() {
        a.blank();
    }
    // Push the new (empty) frame to the compositor immediately so it can start compositing
    // before the post-dismiss grace fires.
    if let Some(display) = gdk4::Display::default() {
        display.flush();
    }

    let windows = windows.clone();
    let tx = tx.clone();
    let app_weak = app_weak.clone();
    glib::timeout_add_local_once(
        std::time::Duration::from_millis(BLANK_FRAME_MS),
        move || {
            dismiss_overlays(&windows);
            send_once(&tx, Ok(outcome));
            if let Some(app) = app_weak.upgrade() {
                app.quit();
            }
        },
    );
}

/// Resolve the current shared state into a `Selection` based on the active mode.
///
/// - `Region`: needs `local_info` so we can translate widget-local coordinates back to
///   compositor logical coords via `gdk::Monitor::geometry()`. Falls through to `None` if no
///   rectangle has been drawn yet.
/// - `Screen`: prefers the explicitly-clicked `selected_monitor`; falls back to `local_info`
///   (the monitor that hosted the Enter / Capture button) when nothing was clicked yet so
///   keyboard-only use keeps working.
/// - `Window`: prefers the explicitly-clicked `selected_window` (resolved locally via the
///   cached Hyprland clients list → `Selection::Region(rect)`). Falls back to
///   `Selection::Window` (focused window) so users that never moved the mouse still get a
///   sensible capture — and so we degrade gracefully when the Hyprland client query failed.
/// - `Full`: returns `Selection::Full`.
fn resolve_selection(
    state: &SharedSelection,
    local_info: Option<&MonitorInfo>,
    monitors: &[MonitorInfo],
) -> Option<Selection> {
    match state.mode {
        ModeKind::Region => {
            let owner = state.owner?;
            let (x, y, w, h) = state.rect_local()?;
            let display = gdk4::Display::default()?;
            let monitors = display.monitors();
            let obj = monitors.item(owner as u32)?;
            let monitor = obj.downcast::<gdk4::Monitor>().ok()?;
            let geo = monitor.geometry();
            Some(Selection::Region(Rect {
                x: geo.x() + x.round() as i32,
                y: geo.y() + y.round() as i32,
                w: w.round() as u32,
                h: h.round() as u32,
            }))
        }
        ModeKind::Screen => {
            let connector = state
                .selected_monitor
                .and_then(|idx| monitors.get(idx))
                .or(local_info)
                .and_then(|info| info.connector.clone())?;
            Some(Selection::Output(connector))
        }
        ModeKind::Window => match &state.selected_window {
            Some(picked) => Some(Selection::Region(picked.rect)),
            None => Some(Selection::Window),
        },
        ModeKind::Full => Some(Selection::Full),
    }
}

#[allow(clippy::too_many_arguments)]
fn commit(
    selection: &SelectionCell,
    tx: &Sender,
    finalised: &Rc<RefCell<bool>>,
    windows: &WindowRegistry,
    monitors: &MonitorList,
    app_weak: &glib::WeakRef<gtk4::Application>,
    areas: &AreaRegistry,
    toolbars: &ToolbarRegistry,
    countdown_source: &Rc<RefCell<Option<glib::SourceId>>>,
    local_info: Option<MonitorInfo>,
    edit: bool,
) {
    if *finalised.borrow() {
        return;
    }
    let state = selection.borrow().clone();
    let monitors_snapshot = monitors.borrow().clone();
    let Some(sel) = resolve_selection(&state, local_info.as_ref(), &monitors_snapshot) else {
        return;
    };
    *finalised.borrow_mut() = true;

    // The countdown happens here (inside the selector), so downstream consumers
    // (`run_capture_flow` / `execute()` / draw save) must not sleep again.
    let outcome = SelectorOutcome {
        selection: sel,
        cursor: state.cursor,
        delay: std::time::Duration::ZERO,
        edit,
    };

    let total_secs = state.delay.as_secs().min(u32::MAX as u64) as u32;
    if total_secs == 0 {
        blank_and_dismiss(windows, areas, toolbars, tx, app_weak, outcome);
        return;
    }

    // Pre-capture delay path: hide the per-monitor toolbars and switch each overlay area
    // into countdown mode. A 1-second timer decrements the remaining seconds; at zero we
    // hand off to `blank_and_dismiss` for the same fadeOut-safe teardown the zero-delay
    // path uses. Pressing Escape during the countdown is handled by `cancel()`, which
    // removes our timer source by id and bails before we ever reach the final dispatch.
    for t in toolbars.borrow().iter() {
        t.widget().set_visible(false);
    }
    for a in areas.borrow().iter() {
        a.set_countdown(Some(total_secs));
    }

    let remaining = Rc::new(Cell::new(total_secs));
    let areas_cloned = areas.clone();
    let toolbars_cloned = toolbars.clone();
    let windows_cloned = windows.clone();
    let tx_cloned = tx.clone();
    let app_weak_cloned = app_weak.clone();
    let countdown_source_cloned = countdown_source.clone();
    let id = glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
        let next = remaining.get().saturating_sub(1);
        remaining.set(next);
        if next == 0 {
            // Drop our own id record before doing dispatch work — `cancel()` checks
            // this on Escape; clearing it here marks the countdown as "complete, no
            // need to remove" so a late Escape can't double-fire.
            countdown_source_cloned.borrow_mut().take();
            // Hand off to the same blank-then-dismiss teardown the zero-delay path uses:
            // it clears the countdown numeral (and everything else) before destroying the
            // surfaces, so Hyprland's layer fadeOut animation has nothing to leak into the
            // captured screenshot.
            blank_and_dismiss(
                &windows_cloned,
                &areas_cloned,
                &toolbars_cloned,
                &tx_cloned,
                &app_weak_cloned,
                outcome.clone(),
            );
            return glib::ControlFlow::Break;
        }
        for a in areas_cloned.borrow().iter() {
            a.set_countdown(Some(next));
        }
        glib::ControlFlow::Continue
    });
    *countdown_source.borrow_mut() = Some(id);
}

fn cancel(
    tx: &Sender,
    finalised: &Rc<RefCell<bool>>,
    windows: &WindowRegistry,
    app_weak: &glib::WeakRef<gtk4::Application>,
    countdown_source: &Rc<RefCell<Option<glib::SourceId>>>,
) {
    // An in-flight countdown means `commit()` has already set `finalised = true`. Escape
    // during the countdown still has to win, so we treat the presence of a live timer
    // source as a force-cancel signal: remove it and proceed to dismiss + send Err even
    // though `finalised` is set.
    let pending = countdown_source.borrow_mut().take();
    if let Some(id) = pending {
        id.remove();
    } else {
        let mut f = finalised.borrow_mut();
        if *f {
            return;
        }
        *f = true;
        drop(f);
    }
    dismiss_overlays(windows);
    send_once(tx, Err(anyhow::Error::new(Cancelled)));
    if let Some(app) = app_weak.upgrade() {
        app.quit();
    }
}

// ---------------------------------------------------------------------------
// Custom GtkWidget for the selector overlay
// ---------------------------------------------------------------------------

glib::wrapper! {
    pub struct SelectorOverlay(ObjectSubclass<imp::SelectorOverlay>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl SelectorOverlay {
    fn new(
        selection: SelectionCell,
        monitor_index: usize,
        style: crate::config::SelectorStyleConfig,
    ) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        imp.selection.replace(Some(selection));
        imp.monitor_index.set(monitor_index);
        imp.style.replace(style);
        obj
    }

    /// Switch the overlay into pre-capture countdown mode (or back out, with `None`).
    /// When set, [`imp::SelectorOverlay::snapshot`] paints a fully dimmed surface plus a
    /// huge centered seconds-remaining numeral, suppressing the regular mode-specific
    /// chrome (selection rectangles, hover outlines, hints).
    fn set_countdown(&self, value: Option<u32>) {
        self.imp().countdown.set(value);
        self.queue_draw();
    }

    /// Stop painting the dim veil, selection rectangle, outline, legend and hint. Used right
    /// before the overlay surfaces are destroyed so the *last* frame the compositor sees is
    /// fully transparent — Hyprland's `fadeOut` layer animation then fades from "blank" to
    /// "gone" instead of fading our colored chrome out into whatever `wlr-screencopy`
    /// happens to grab next.
    fn blank(&self) {
        self.imp().blanked.set(true);
        self.queue_draw();
    }
}

impl Default for SelectorOverlay {
    fn default() -> Self {
        glib::Object::new()
    }
}

mod imp {
    use super::*;

    pub struct SelectorOverlay {
        /// Optional so the widget can be default-constructed (required by GObject) before the
        /// caller wires in the shared `SelectionCell`.
        #[allow(private_interfaces)]
        pub selection: RefCell<Option<SelectionCell>>,
        pub monitor_index: Cell<usize>,
        /// Chrome colors. Populated by [`super::SelectorOverlay::new`] from the active
        /// `[ui.selector]` config table; falls back to defaults for the GObject default
        /// constructor.
        pub style: RefCell<crate::config::SelectorStyleConfig>,
        /// When `Some(n)`, the overlay draws a big centered "n" instead of the usual
        /// mode-specific chrome. Driven by [`super::commit`] during a pre-capture delay.
        pub countdown: Cell<Option<u32>>,
        /// When true, [`WidgetImpl::snapshot`] short-circuits and paints nothing. Set right
        /// before the overlay surfaces are destroyed so the compositor's last composited
        /// frame is fully transparent — defeats Hyprland's layer `fadeOut` animation
        /// leaking the selection chrome into the captured screenshot.
        pub blanked: Cell<bool>,
    }

    impl Default for SelectorOverlay {
        fn default() -> Self {
            Self {
                selection: RefCell::new(None),
                monitor_index: Cell::new(0),
                style: RefCell::new(crate::config::SelectorStyleConfig::default()),
                countdown: Cell::new(None),
                blanked: Cell::new(false),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SelectorOverlay {
        const NAME: &'static str = "HyprsnapSelectorOverlay";
        type Type = super::SelectorOverlay;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for SelectorOverlay {}

    impl WidgetImpl for SelectorOverlay {
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let w = self.obj().width() as f32;
            let h = self.obj().height() as f32;
            if w <= 0.0 || h <= 0.0 {
                return;
            }

            // Pre-dismiss: paint an explicit transparent fill over the whole surface so
            // GTK actually submits a new (empty) `wl_buffer` to the compositor. Returning
            // here without `snapshot.append_*` would let GTK skip the redraw entirely, the
            // compositor would keep our last colored frame as the surface contents, and
            // Hyprland's layer `fadeOut` animation would happily fade that into the
            // screenshot. See [`super::SelectorOverlay::blank`].
            if self.blanked.get() {
                let transparent = gtk4::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0);
                snapshot.append_color(&transparent, &graphene::Rect::new(0.0, 0.0, w, h));
                return;
            }

            let monitor_index = self.monitor_index.get();
            let state = self
                .selection
                .borrow()
                .as_ref()
                .map(|s| s.borrow().clone())
                .unwrap_or_default();

            let dim_strong = self.style.borrow().dim_strong.to_rgba();
            let dim_full = self.style.borrow().dim_full.to_rgba();
            let dim_light = self.style.borrow().dim_light.to_rgba();
            let outline = self.style.borrow().outline.to_rgba();
            let label_color = self.style.borrow().label.to_rgba();

            match state.mode {
                ModeKind::Region => {
                    let rect_here = match state.owner {
                        Some(idx) if idx == monitor_index => state.rect_local(),
                        _ => None,
                    };
                    let Some((rx, ry, rw, rh)) = rect_here else {
                        snapshot.append_color(&dim_full, &graphene::Rect::new(0.0, 0.0, w, h));
                        self.draw_hint(
                            snapshot,
                            w,
                            h,
                            &fl!("selector-hint-region-empty"),
                            &label_color,
                        );
                        return;
                    };
                    let (rx, ry, rw, rh) = (rx as f32, ry as f32, rw as f32, rh as f32);
                    // Four dimmed strips around the selection.
                    snapshot.append_color(&dim_strong, &graphene::Rect::new(0.0, 0.0, w, ry));
                    snapshot.append_color(
                        &dim_strong,
                        &graphene::Rect::new(0.0, ry + rh, w, (h - (ry + rh)).max(0.0)),
                    );
                    snapshot.append_color(&dim_strong, &graphene::Rect::new(0.0, ry, rx, rh));
                    snapshot.append_color(
                        &dim_strong,
                        &graphene::Rect::new(rx + rw, ry, (w - (rx + rw)).max(0.0), rh),
                    );

                    let pb = gtk4::gsk::PathBuilder::new();
                    pb.add_rect(&graphene::Rect::new(rx + 0.5, ry + 0.5, rw - 1.0, rh - 1.0));
                    let stroke = gtk4::gsk::Stroke::new(1.5);
                    snapshot.append_stroke(&pb.to_path(), &stroke, &outline);

                    let hint = fl!(
                        "selector-hint-region-size",
                        width = (rw as i32),
                        height = (rh as i32),
                    );
                    let pango_ctx = self.obj().create_pango_context();
                    let layout = pango::Layout::new(&pango_ctx);
                    let desc = pango::FontDescription::from_string("Sans Bold 11");
                    layout.set_font_description(Some(&desc));
                    layout.set_text(&hint);
                    let (_lw, lh) = layout.pixel_size();
                    snapshot.save();
                    snapshot.translate(&graphene::Point::new(rx + 6.0, ry + rh - 8.0 - lh as f32));
                    snapshot.append_layout(&layout, &label_color);
                    snapshot.restore();
                }
                ModeKind::Full => {
                    // Light dim across the whole desktop to signal "full grab pending".
                    snapshot.append_color(&dim_light, &graphene::Rect::new(0.0, 0.0, w, h));
                    self.draw_hint(snapshot, w, h, &fl!("selector-hint-full"), &label_color);
                }
                ModeKind::Screen => {
                    let selected = state.selected_monitor == Some(monitor_index);
                    let hovered = state.hover_monitor == Some(monitor_index);
                    // Selected screen stays bright (dim_light) until the user picks a
                    // different one; hovered (but not selected) screens get the same lift
                    // as a hint. Everything else is fully dimmed.
                    let dim = if selected || hovered {
                        dim_light
                    } else {
                        dim_strong
                    };
                    snapshot.append_color(&dim, &graphene::Rect::new(0.0, 0.0, w, h));
                    if selected || hovered {
                        let pb = gtk4::gsk::PathBuilder::new();
                        pb.add_rect(&graphene::Rect::new(1.5, 1.5, w - 3.0, h - 3.0));
                        let stroke = gtk4::gsk::Stroke::new(if selected { 4.0 } else { 2.0 });
                        snapshot.append_stroke(&pb.to_path(), &stroke, &outline);
                    }
                    let hint = if state.selected_monitor.is_some() {
                        fl!("selector-hint-screen-selected")
                    } else {
                        fl!("selector-hint-screen-pick")
                    };
                    self.draw_hint(snapshot, w, h, &hint, &label_color);
                }
                ModeKind::Window => {
                    snapshot.append_color(&dim_strong, &graphene::Rect::new(0.0, 0.0, w, h));

                    // Outline the hovered window (thin) and the selected window (thick), each
                    // translated from compositor logical coords into this monitor's
                    // widget-local space. Both can clip out of the widget — gsk's stroke
                    // handles that naturally.
                    if let Some(picked) = &state.hover_window
                        && state
                            .selected_window
                            .as_ref()
                            .is_none_or(|p| p.rect != picked.rect)
                        && let Some((rx, ry, rw, rh)) =
                            logical_to_local(monitor_index, &picked.rect)
                    {
                        let pb = gtk4::gsk::PathBuilder::new();
                        pb.add_rect(&graphene::Rect::new(rx + 0.5, ry + 0.5, rw - 1.0, rh - 1.0));
                        let stroke = gtk4::gsk::Stroke::new(2.0);
                        snapshot.append_stroke(&pb.to_path(), &stroke, &outline);
                    }
                    if let Some(picked) = &state.selected_window
                        && let Some((rx, ry, rw, rh)) =
                            logical_to_local(monitor_index, &picked.rect)
                    {
                        let pb = gtk4::gsk::PathBuilder::new();
                        pb.add_rect(&graphene::Rect::new(rx + 0.5, ry + 0.5, rw - 1.0, rh - 1.0));
                        let stroke = gtk4::gsk::Stroke::new(3.0);
                        snapshot.append_stroke(&pb.to_path(), &stroke, &outline);
                    }

                    let hint = match &state.selected_window {
                        Some(p) if !p.class.is_empty() && !p.title.is_empty() => fl!(
                            "selector-hint-window-class-title",
                            class = p.class.as_str(),
                            title = p.title.as_str()
                        ),
                        Some(p) if !p.class.is_empty() => {
                            fl!("selector-hint-window-class", class = p.class.as_str())
                        }
                        Some(p) if !p.title.is_empty() => {
                            fl!("selector-hint-window-title", title = p.title.as_str())
                        }
                        Some(_) => fl!("selector-hint-window-selected"),
                        None => fl!("selector-hint-window-pick"),
                    };
                    self.draw_hint(snapshot, w, h, &hint, &label_color);
                }
            }

            // Countdown overlay: drawn last so it sits above the mode-specific veil and
            // selection chrome. The selection rectangle and its outline stay visible
            // underneath so the user sees exactly what is about to be captured.
            if let Some(secs) = self.countdown.get() {
                let fg = self.style.borrow().countdown_fg.to_rgba();
                self.draw_countdown(snapshot, w, h, secs, &fg);
            }
        }
    }

    impl SelectorOverlay {
        /// Render a centered hint string near the top of the monitor.
        fn draw_hint(
            &self,
            snapshot: &gtk4::Snapshot,
            w: f32,
            _h: f32,
            text: &str,
            color: &gtk4::gdk::RGBA,
        ) {
            let pango_ctx = self.obj().create_pango_context();
            let layout = pango::Layout::new(&pango_ctx);
            let desc = pango::FontDescription::from_string("Sans 12");
            layout.set_font_description(Some(&desc));
            layout.set_text(text);
            let (lw, _lh) = layout.pixel_size();
            let x = ((w - lw as f32) / 2.0).max(8.0);
            snapshot.save();
            snapshot.translate(&graphene::Point::new(x, 32.0));
            snapshot.append_layout(&layout, color);
            snapshot.restore();
        }

        /// Render the pre-capture countdown numeral, centered on the monitor.
        /// Sized roughly to a quarter of the shorter monitor dimension so a 5-second
        /// countdown is unmistakable from across the room.
        fn draw_countdown(
            &self,
            snapshot: &gtk4::Snapshot,
            w: f32,
            h: f32,
            secs: u32,
            color: &gtk4::gdk::RGBA,
        ) {
            let text = secs.to_string();
            let pt = (h.min(w) / 4.0).max(48.0) as i32;
            let pango_ctx = self.obj().create_pango_context();
            let layout = pango::Layout::new(&pango_ctx);
            let desc = pango::FontDescription::from_string(&format!("Sans Bold {pt}"));
            layout.set_font_description(Some(&desc));
            layout.set_text(&text);
            let (lw, lh) = layout.pixel_size();
            let x = ((w - lw as f32) / 2.0).max(0.0);
            let y = ((h - lh as f32) / 2.0).max(0.0);
            snapshot.save();
            snapshot.translate(&graphene::Point::new(x, y));
            snapshot.append_layout(&layout, color);
            snapshot.restore();
        }
    }
}
