//! Interactive region selector overlay.
//!
//! Renders one fullscreen `gtk4_layer_shell` window per monitor at `Layer::Overlay`. All
//! overlays share a single selection state (current rectangle + owning monitor) so that
//! starting a new drag on any monitor cancels the previous rectangle.
//!
//! Workflow:
//!   - Drag a rectangle on any monitor.
//!   - Release the mouse: rectangle stays on screen.
//!   - Drag again (same or different monitor): replaces the previous rectangle.
//!   - Enter (or KP Enter): commit and return the rect in compositor logical coordinates.
//!   - Esc: cancel.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};

use anyhow::{Result, anyhow, bail};
use gtk4::glib;
use gtk4::glib::subclass::prelude::*;
use gtk4::graphene;
use gtk4::pango;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::capture::region::Rect;
use crate::context::Ctx;

/// Show the selector and return the chosen region (logical compositor coordinates).
pub async fn pick_region(_ctx: Ctx) -> Result<Rect> {
    let (tx, rx) = mpsc::sync_channel::<Result<Rect>>(1);
    tokio::task::spawn_blocking(move || run_gtk(tx))
        .await
        .map_err(|e| anyhow!("selector task panicked: {e}"))??;
    let result = rx
        .recv()
        .map_err(|e| anyhow!("selector channel closed without a result: {e}"))?;
    if result.is_ok() {
        // Overlays are destroyed + flushed synchronously inside `commit()`, but Hyprland still
        // processes the unmap on its own event loop. A short grace window avoids the dimmed
        // veil leaking into the wlr-screencopy frame.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    result
}

/// Per-monitor descriptor needed by signal handlers. Only `index` is consulted at runtime
/// (the logical x/y are looked up via `gdk::Monitor::geometry()` at commit time), but tracking
/// them here makes the intent of the call sites explicit.
#[derive(Clone, Copy, Debug)]
struct MonitorInfo {
    index: usize,
}

/// Shared selection state: which monitor owns the current rect, plus the local drag points
/// expressed in that monitor's widget-local pixels.
#[derive(Clone, Copy, Debug, Default)]
struct SharedSelection {
    owner: Option<usize>,
    start: Option<(f64, f64)>,
    current: Option<(f64, f64)>,
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
type Sender = Arc<Mutex<Option<mpsc::SyncSender<Result<Rect>>>>>;
type AreaRegistry = Rc<RefCell<Vec<SelectorOverlay>>>;
type WindowRegistry = Rc<RefCell<Vec<gtk4::ApplicationWindow>>>;

fn send_once(tx: &Sender, msg: Result<Rect>) {
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
fn run_gtk(tx: mpsc::SyncSender<Result<Rect>>) -> Result<()> {
    let app = gtk4::Application::builder()
        .application_id(crate::ui::APP_ID)
        .build();
    let tx: Sender = Arc::new(Mutex::new(Some(tx)));

    {
        let tx = tx.clone();
        app.connect_activate(move |app| {
            if let Err(err) = build_overlays(app, &tx) {
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

fn build_overlays(app: &gtk4::Application, tx: &Sender) -> Result<()> {
    crate::ui::style::install();

    let display = gdk4::Display::default().ok_or_else(|| anyhow!("no GDK display available"))?;
    let monitors_list = display.monitors();
    let n = monitors_list.n_items();
    if n == 0 {
        bail!("no monitors reported by GDK");
    }

    let shared = SharedState {
        selection: Rc::new(RefCell::new(SharedSelection::default())),
        finalised: Rc::new(RefCell::new(false)),
        areas: Rc::new(RefCell::new(Vec::new())),
        windows: Rc::new(RefCell::new(Vec::new())),
        tx: tx.clone(),
        app_weak: app.downgrade(),
    };

    for i in 0..n {
        let Some(obj) = monitors_list.item(i) else {
            continue;
        };
        let Ok(monitor) = obj.downcast::<gdk4::Monitor>() else {
            continue;
        };
        spawn_monitor_overlay(app, &monitor, MonitorInfo { index: i as usize }, &shared);
    }
    Ok(())
}

fn spawn_monitor_overlay(
    app: &gtk4::Application,
    monitor: &gdk4::Monitor,
    info: MonitorInfo,
    shared: &SharedState,
) {
    let geo = monitor.geometry();
    let mon_w = geo.width();
    let mon_h = geo.height();

    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .decorated(false)
        .resizable(false)
        .build();
    window.add_css_class("hyprsnap-selector");

    // Layer-shell setup. `init_layer_shell` must come first; the rest is order-insensitive
    // before `present()` realizes the window.
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_namespace(Some("hyprsnap-selector"));
    window.set_monitor(Some(monitor));
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_exclusive_zone(-1);
    // OnDemand (rather than Exclusive) avoids a multi-window keyboard tug-of-war on Hyprland:
    // each surface only requests focus when the user interacts with it, so Enter/Esc reach
    // whichever monitor the user actually used.
    window.set_keyboard_mode(KeyboardMode::OnDemand);

    // Size hint matching the monitor — required on some compositors for the layer-shell
    // anchoring to produce a fullscreen surface; without it GTK reports a 200x200 minimum.
    window.set_default_size(mon_w.max(1), mon_h.max(1));

    let area = SelectorOverlay::new(shared.selection.clone(), info.index);
    area.set_hexpand(true);
    area.set_vexpand(true);

    install_drag(&area, &shared.selection, info.index, &shared.areas);
    install_keys(
        &window,
        &shared.selection,
        &shared.tx,
        &shared.finalised,
        &shared.windows,
        &shared.app_weak,
        info,
    );

    window.set_child(Some(&area));
    shared.areas.borrow_mut().push(area.clone());
    shared.windows.borrow_mut().push(window.clone());
    window.present();
}

/// Lifetimes-shared bag of per-call state passed down through `spawn_monitor_overlay`.
struct SharedState {
    selection: SelectionCell,
    finalised: Rc<RefCell<bool>>,
    areas: AreaRegistry,
    windows: WindowRegistry,
    tx: Sender,
    app_weak: glib::WeakRef<gtk4::Application>,
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
        drag.connect_drag_begin(move |_, x, y| {
            *selection.borrow_mut() = SharedSelection {
                owner: Some(monitor_index),
                start: Some((x, y)),
                current: Some((x, y)),
            };
            redraw_all(&areas);
        });
    }
    {
        let selection = selection.clone();
        let areas = areas.clone();
        drag.connect_drag_update(move |g, dx, dy| {
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

fn install_keys(
    window: &gtk4::ApplicationWindow,
    selection: &SelectionCell,
    tx: &Sender,
    finalised: &Rc<RefCell<bool>>,
    windows: &WindowRegistry,
    app_weak: &glib::WeakRef<gtk4::Application>,
    info: MonitorInfo,
) {
    let key = gtk4::EventControllerKey::new();
    let selection = selection.clone();
    let tx = tx.clone();
    let finalised = finalised.clone();
    let windows = windows.clone();
    let app_weak = app_weak.clone();
    key.connect_key_pressed(move |_, k, _, _| match k {
        gdk4::Key::Escape => {
            cancel(&tx, &finalised, &windows, &app_weak);
            glib::Propagation::Stop
        }
        gdk4::Key::Return | gdk4::Key::KP_Enter => {
            commit(&selection, &tx, &finalised, &windows, &app_weak, info);
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    });
    window.add_controller(key);
}

/// Tear down every overlay window synchronously and flush the Wayland connection so the
/// compositor processes the unmap requests *before* we hand control back to the caller. Without
/// this, `app.quit()` only schedules destruction on the next GLib idle, leaving the dimmed veil
/// visible during capture/encode (often several seconds in dev builds).
fn dismiss_overlays(windows: &WindowRegistry) {
    for window in windows.borrow_mut().drain(..) {
        window.set_visible(false);
        window.destroy();
    }
    if let Some(display) = gdk4::Display::default() {
        display.flush();
    }
}

fn commit(
    selection: &SelectionCell,
    tx: &Sender,
    finalised: &Rc<RefCell<bool>>,
    windows: &WindowRegistry,
    app_weak: &glib::WeakRef<gtk4::Application>,
    _local_info: MonitorInfo,
) {
    if *finalised.borrow() {
        return;
    }
    let state = *selection.borrow();
    let Some(owner) = state.owner else { return };
    let Some((x, y, w, h)) = state.rect_local() else {
        return;
    };
    let Some(display) = gdk4::Display::default() else {
        return;
    };
    let monitors = display.monitors();
    let Some(obj) = monitors.item(owner as u32) else {
        return;
    };
    let Ok(monitor) = obj.downcast::<gdk4::Monitor>() else {
        return;
    };
    let geo = monitor.geometry();
    let rect = Rect {
        x: geo.x() + x.round() as i32,
        y: geo.y() + y.round() as i32,
        w: w.round() as u32,
        h: h.round() as u32,
    };
    *finalised.borrow_mut() = true;
    dismiss_overlays(windows);
    send_once(tx, Ok(rect));
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
    send_once(tx, Err(anyhow!("selection cancelled")));
    if let Some(app) = app_weak.upgrade() {
        app.quit();
    }
}

// ---------------------------------------------------------------------------
// Custom GtkWidget for the selector overlay
// ---------------------------------------------------------------------------
//
// We subclass `gtk4::Widget` and implement `snapshot()` directly so the dimmer, the white
// selection outline, and the size readout are produced as GSK render nodes — same path as the
// annotation canvas. Going custom (instead of a `DrawingArea` Cairo callback) lets us drop the
// last Cairo dependency from the UI.

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
        /// caller wires in the shared `SelectionCell`. The `allow` is necessary because
        /// `SelectionCell` is module-private but the field has to be `pub` so GObject can
        /// access it through `imp()`.
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
                .map(|s| *s.borrow())
                .unwrap_or_default();
            let rect_here = match state.owner {
                Some(idx) if idx == monitor_index => state.rect_local(),
                _ => None,
            };

            let dim_strong = gtk4::gdk::RGBA::new(0.0, 0.0, 0.0, 0.55);
            let dim_full = gtk4::gdk::RGBA::new(0.0, 0.0, 0.0, 0.45);
            let outline = gtk4::gdk::RGBA::new(1.0, 1.0, 1.0, 0.95);
            let label_color = gtk4::gdk::RGBA::new(1.0, 1.0, 1.0, 0.9);

            let Some((rx, ry, rw, rh)) = rect_here else {
                // No selection on this monitor — full dim so the user sees we're in selector mode.
                snapshot.append_color(&dim_full, &graphene::Rect::new(0.0, 0.0, w, h));
                return;
            };
            let (rx, ry, rw, rh) = (rx as f32, ry as f32, rw as f32, rh as f32);

            // Four dimmed strips around the selection. Doing it as separate `append_color`
            // calls (instead of one big rect + a clipped clear) keeps the render tree flat —
            // each node is just a filled rectangle on the GPU.
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

            // Single-pixel outline around the selection. `gsk::PathBuilder::add_rect` traces
            // the perimeter; offsetting by 0.5 px matches the existing Cairo geometry so the
            // stroke sits inside the dim/selection boundary.
            let pb = gtk4::gsk::PathBuilder::new();
            pb.add_rect(&graphene::Rect::new(rx + 0.5, ry + 0.5, rw - 1.0, rh - 1.0));
            let stroke = gtk4::gsk::Stroke::new(1.5);
            snapshot.append_stroke(&pb.to_path(), &stroke, &outline);

            // Size readout. Building a Pango layout via the widget's context lets GTK pick the
            // right font + DPI, and `append_layout` rasterises into a glyph node on the GPU.
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
            // Cairo `show_text` placed the baseline at `ry + rh - 8`; `append_layout` positions
            // the top-left of the layout, so subtract the layout height to roughly preserve
            // where the text sits relative to the bottom edge of the selection rectangle.
            snapshot.save();
            snapshot.translate(&graphene::Point::new(rx + 6.0, ry + rh - 8.0 - lh as f32));
            snapshot.append_layout(&layout, &label_color);
            snapshot.restore();
        }
    }
}
