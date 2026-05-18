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
//!   - `Region` (default): drag a rectangle, then press Enter / click Capture.
//!   - `Screen`: hover a monitor to highlight it, click to commit a per-monitor capture.
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
pub async fn pick_region(_ctx: Ctx, initial_cursor: bool) -> Result<SelectorOutcome> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<SelectorOutcome>>();
    let clients = fetch_clients_or_warn().await;
    tokio::task::spawn_blocking(move || run_gtk(tx, initial_cursor, clients))
        .await
        .map_err(|e| anyhow!("selector task panicked: {e}"))??;
    let result = rx
        .await
        .map_err(|e| anyhow!("selector channel closed without a result: {e}"))?;
    if result.is_ok() {
        // Overlays are destroyed + flushed synchronously inside `commit()`, but Hyprland still
        // processes the unmap on its own event loop. A short grace window avoids the dimmed
        // veil leaking into the wlr-screencopy frame.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
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
pub async fn pick_region_in_app(
    app: &gtk4::Application,
    initial_cursor: bool,
) -> Result<SelectorOutcome> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<SelectorOutcome>>();
    let tx: Sender = Arc::new(Mutex::new(Some(tx)));
    let clients = fetch_clients_or_warn().await;
    // Empty WeakRef → commit/cancel's `app.upgrade()` returns None → no `app.quit()` fires
    // against the caller's app. Windows are still parented to `app` (required by GTK), but
    // they're destroyed by `dismiss_overlays` so they don't keep the app alive on their own.
    let quit_target: glib::WeakRef<gtk4::Application> = glib::WeakRef::new();
    if let Err(err) = build_overlays(app, &tx, initial_cursor, quit_target, clients) {
        send_once(&tx, Err(err));
    }
    let result = rx
        .await
        .map_err(|e| anyhow!("selector channel closed without a result: {e}"))?;
    if result.is_ok() {
        glib::timeout_future(std::time::Duration::from_millis(30)).await;
    }
    result
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

fn run_gtk(
    tx: tokio::sync::oneshot::Sender<Result<SelectorOutcome>>,
    initial_cursor: bool,
    clients: Vec<HyprWindow>,
) -> Result<()> {
    let app = gtk4::Application::builder()
        .application_id(crate::ui::APP_ID)
        .build();
    let tx: Sender = Arc::new(Mutex::new(Some(tx)));

    {
        let tx = tx.clone();
        let clients = Rc::new(RefCell::new(clients));
        app.connect_activate(move |app| {
            crate::ui::install_icon_resources();
            // Standalone: pass `app.downgrade()` as the quit target so commit / cancel tear
            // down the private app once the user finishes (or escapes). The embeddable
            // `pick_region_in_app` path passes an empty WeakRef to leave the caller's app
            // alone.
            let quit_target = app.downgrade();
            let clients_snapshot = clients.borrow().clone();
            if let Err(err) =
                build_overlays(app, &tx, initial_cursor, quit_target, clients_snapshot)
            {
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

fn build_overlays(
    app: &gtk4::Application,
    tx: &Sender,
    initial_cursor: bool,
    quit_target: glib::WeakRef<gtk4::Application>,
    clients: Vec<HyprWindow>,
) -> Result<()> {
    crate::ui::style::install();

    let display = gdk4::Display::default().ok_or_else(|| anyhow!("no GDK display available"))?;
    let monitors_list = display.monitors();
    let n = monitors_list.n_items();
    if n == 0 {
        bail!("no monitors reported by GDK");
    }

    let shared = SharedState {
        selection: Rc::new(RefCell::new(SharedSelection {
            cursor: initial_cursor,
            ..SharedSelection::default()
        })),
        finalised: Rc::new(RefCell::new(false)),
        areas: Rc::new(RefCell::new(Vec::new())),
        windows: Rc::new(RefCell::new(Vec::new())),
        monitors: Rc::new(RefCell::new(Vec::new())),
        tx: tx.clone(),
        app_weak: quit_target,
        toolbars: Rc::new(RefCell::new(Vec::new())),
        initial_cursor,
        clients: Rc::new(RefCell::new(clients)),
    };

    let mut windows = Vec::with_capacity(n as usize);
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
        shared.monitors.borrow_mut().push(info.clone());
        windows.push(spawn_monitor_overlay(app, &monitor, info, &shared));
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

    let area = SelectorOverlay::new(shared.selection.clone(), info.index);
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
        info.clone(),
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
    let toolbar = Toolbar::new(ToolbarSpec {
        modes: SELECTOR_MODES,
        show_cursor_toggle: true,
        show_capture: true,
        initial_mode: Some(ModeKind::Region),
        initial_cursor: shared.initial_cursor,
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
                // Switching modes always clears any previously "armed" zone so the user
                // doesn't accidentally commit a stale selection from a different mode.
                s.selected_monitor = None;
                s.selected_window = None;
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
        ToolbarAction::Capture => {
            commit(
                &selection,
                &tx,
                &finalised,
                &windows,
                &monitors,
                &app_weak,
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
    initial_cursor: bool,
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
                    s.selected_window = None;
                }
                ModeKind::Window => {
                    let picked = local_to_logical(monitor_index, x, y).and_then(|(lx, ly)| {
                        hypr::window_at(&clients.borrow(), lx, ly).map(PickedWindow::from)
                    });
                    if picked.is_some() {
                        s.selected_window = picked;
                        s.selected_monitor = None;
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
    info: MonitorInfo,
) {
    let key = gtk4::EventControllerKey::new();
    let selection = selection.clone();
    let tx = tx.clone();
    let finalised = finalised.clone();
    let windows = windows.clone();
    let monitors = monitors.clone();
    let app_weak = app_weak.clone();
    key.connect_key_pressed(move |_, k, _, modifiers| match k {
        gdk4::Key::Escape => {
            cancel(&tx, &finalised, &windows, &app_weak);
            glib::Propagation::Stop
        }
        gdk4::Key::Return | gdk4::Key::KP_Enter => {
            // Shift+Enter routes to the annotation editor, mirroring the toolbar's
            // `ShortcutAction::Annotate` shortcut so both layers stay in sync.
            let edit = modifiers.contains(gdk4::ModifierType::SHIFT_MASK);
            commit(
                &selection,
                &tx,
                &finalised,
                &windows,
                &monitors,
                &app_weak,
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
    for window in windows.borrow_mut().drain(..) {
        window.set_visible(false);
        window.destroy();
    }
    if let Some(display) = gdk4::Display::default() {
        display.flush();
    }
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
    dismiss_overlays(windows);
    let outcome = SelectorOutcome {
        selection: sel,
        cursor: state.cursor,
        edit,
    };
    send_once(tx, Ok(outcome));
    if let Some(app) = app_weak.upgrade() {
        app.quit();
    }
}

fn cancel(
    tx: &Sender,
    finalised: &Rc<RefCell<bool>>,
    windows: &WindowRegistry,
    app_weak: &glib::WeakRef<gtk4::Application>,
) {
    let mut f = finalised.borrow_mut();
    if *f {
        return;
    }
    *f = true;
    drop(f);
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
    fn new(selection: SelectionCell, monitor_index: usize) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        imp.selection.replace(Some(selection));
        imp.monitor_index.set(monitor_index);
        obj
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
    }

    impl Default for SelectorOverlay {
        fn default() -> Self {
            Self {
                selection: RefCell::new(None),
                monitor_index: Cell::new(0),
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
            let monitor_index = self.monitor_index.get();
            let state = self
                .selection
                .borrow()
                .as_ref()
                .map(|s| s.borrow().clone())
                .unwrap_or_default();

            let dim_strong = gtk4::gdk::RGBA::new(0.0, 0.0, 0.0, 0.55);
            let dim_full = gtk4::gdk::RGBA::new(0.0, 0.0, 0.0, 0.45);
            let dim_light = gtk4::gdk::RGBA::new(0.0, 0.0, 0.0, 0.25);
            let outline = gtk4::gdk::RGBA::new(1.0, 1.0, 1.0, 0.95);
            let label_color = gtk4::gdk::RGBA::new(1.0, 1.0, 1.0, 0.9);

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
                            "Drag to select a region — Enter to confirm, Esc to cancel",
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

                    let hint = format!(
                        "{} × {} — Enter to confirm, Esc to cancel",
                        rw as i32, rh as i32
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
                    self.draw_hint(
                        snapshot,
                        w,
                        h,
                        "Full desktop — Enter to confirm, Esc to cancel",
                        &label_color,
                    );
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
                        "Screen selected — Enter to confirm, Esc to cancel"
                    } else {
                        "Click a screen — Enter to confirm, Esc to cancel"
                    };
                    self.draw_hint(snapshot, w, h, hint, &label_color);
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
                        Some(p) if !p.class.is_empty() && !p.title.is_empty() => {
                            format!("{}: {} — Enter to confirm, Esc to cancel", p.class, p.title)
                        }
                        Some(p) if !p.class.is_empty() => {
                            format!("{} — Enter to confirm, Esc to cancel", p.class)
                        }
                        Some(p) if !p.title.is_empty() => {
                            format!("{} — Enter to confirm, Esc to cancel", p.title)
                        }
                        Some(_) => "Window selected — Enter to confirm, Esc to cancel".to_owned(),
                        None => "Click a window — Enter to confirm, Esc to cancel".to_owned(),
                    };
                    self.draw_hint(snapshot, w, h, &hint, &label_color);
                }
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
    }
}
