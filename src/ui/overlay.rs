//! Live "draw on screen" overlay (Draw-On-Gnome equivalent).
//!
//! Spawns one `gtk4_layer_shell` window per monitor at `Layer::Overlay`. Each hosts an
//! [`AnnotationCanvas`] sized to its monitor so the user can sketch directly on top of their
//! desktop, plus a floating bottom-center [`crate::ui::Toolbar`] with tool toggles, undo,
//! clear, and a passthrough toggle. The keyboard is grabbed exclusively while the overlay is
//! alive so the user's tool shortcuts always reach us, even when input passthrough lets pointer
//! events fall through to whatever app is underneath.
//!
//! Shortcuts (mirrored on every monitor's toolbar):
//!   * `R / A / H / F / N / T / X` — switch tool
//!   * `Ctrl+Z` — undo last layer
//!   * `Ctrl+L` — clear all layers
//!   * `P` — toggle pointer passthrough
//!   * `Esc` — quit
//!
//! Crop and Blur are intentionally omitted (no underlying pixels to operate on).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};

use anyhow::{Result, anyhow, bail};
use gtk4::cairo;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::annotate::ToolKind;
use crate::context::Ctx;
use crate::ui::canvas::AnnotationCanvas;
use crate::ui::toolbar::{OVERLAY_TOOLS, Toolbar, ToolbarAction, ToolbarSpec};

/// Launch the live overlay. Returns once the user presses `Esc` (or the GTK loop otherwise
/// exits). `initial_passthrough` matches the `--passthrough` CLI flag. When `shutdown` is
/// provided, completing the receiver (e.g. from the daemon's DrawToggle handler) tears the
/// overlay down from outside the GTK loop.
pub async fn run(
    _ctx: Ctx,
    initial_passthrough: bool,
    shutdown: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<()> {
    let (tx, rx) = mpsc::sync_channel::<Result<()>>(1);
    tokio::task::spawn_blocking(move || run_gtk(tx, initial_passthrough, shutdown))
        .await
        .map_err(|e| anyhow!("overlay task panicked: {e}"))??;
    rx.recv()
        .map_err(|e| anyhow!("overlay channel closed without a result: {e}"))?
}

type CanvasRegistry = Rc<RefCell<Vec<AnnotationCanvas>>>;
type WindowRegistry = Rc<RefCell<Vec<gtk4::ApplicationWindow>>>;
type ToolbarRegistry = Rc<RefCell<Vec<Toolbar>>>;
type ResultSender = Arc<Mutex<Option<mpsc::SyncSender<Result<()>>>>>;

fn run_gtk(
    tx: mpsc::SyncSender<Result<()>>,
    initial_passthrough: bool,
    shutdown: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<()> {
    let app = gtk4::Application::builder()
        .application_id(crate::ui::APP_ID)
        .build();
    let tx: ResultSender = Arc::new(Mutex::new(Some(tx)));

    // Wrap the shutdown receiver in a Cell so the activate handler can take ownership the
    // first time it fires (Application::connect_activate is Fn, not FnOnce).
    let shutdown_cell: Rc<RefCell<Option<tokio::sync::oneshot::Receiver<()>>>> =
        Rc::new(RefCell::new(shutdown));

    {
        let tx = tx.clone();
        let shutdown_cell = shutdown_cell.clone();
        app.connect_activate(move |app| match build_overlays(app, initial_passthrough) {
            Ok(shared) => {
                if let Some(rx) = shutdown_cell.borrow_mut().take() {
                    attach_shutdown(&shared, rx);
                }
            }
            Err(err) => {
                send_once(&tx, Err(err));
                app.quit();
            }
        });
    }

    let exit = app.run_with_args::<&str>(&[]);
    let code: i32 = exit.into();
    // Treat a normal quit (Esc or external shutdown) as success. If the channel still has a
    // slot, fill it so the caller's recv() doesn't dangle.
    send_once(&tx, Ok(()));
    if code != 0 {
        bail!("GTK exited with status {code}");
    }
    Ok(())
}

/// Wire an external shutdown receiver into the GTK main context so a daemon-driven toggle can
/// tear the overlay down cleanly without racing the GTK thread.
fn attach_shutdown(shared: &Shared, rx: tokio::sync::oneshot::Receiver<()>) {
    let windows = shared.windows.clone();
    let app_weak = shared.app_weak.clone();
    glib::MainContext::default().spawn_local(async move {
        // If the sender is dropped without firing, await yields Err; either way we tear down so
        // the overlay doesn't outlive the daemon-side state.
        let _ = rx.await;
        tear_down(&windows, &app_weak);
    });
}

fn send_once(tx: &ResultSender, msg: Result<()>) {
    if let Ok(mut guard) = tx.lock()
        && let Some(sender) = guard.take()
    {
        let _ = sender.send(msg);
    }
}

fn build_overlays(app: &gtk4::Application, initial_passthrough: bool) -> Result<Shared> {
    crate::ui::style::install();

    let display = gdk4::Display::default().ok_or_else(|| anyhow!("no GDK display available"))?;
    let monitors_list = display.monitors();
    let n = monitors_list.n_items();
    if n == 0 {
        bail!("no monitors reported by GDK");
    }

    let shared = Shared {
        passthrough: Rc::new(Cell::new(initial_passthrough)),
        current_tool: Rc::new(Cell::new(ToolKind::Freehand)),
        canvases: Rc::new(RefCell::new(Vec::new())),
        windows: Rc::new(RefCell::new(Vec::new())),
        toolbars: Rc::new(RefCell::new(Vec::new())),
        app_weak: app.downgrade(),
    };

    for i in 0..n {
        let Some(obj) = monitors_list.item(i) else {
            continue;
        };
        let Ok(monitor) = obj.downcast::<gdk4::Monitor>() else {
            continue;
        };
        spawn_monitor_overlay(app, &monitor, &shared);
    }
    Ok(shared)
}

struct Shared {
    /// Pointer-passthrough toggle shared across all monitors so `P` flips them in lockstep.
    passthrough: Rc<Cell<bool>>,
    /// Active tool, mirrored to every canvas so switching on one monitor takes effect on all.
    current_tool: Rc<Cell<ToolKind>>,
    canvases: CanvasRegistry,
    windows: WindowRegistry,
    toolbars: ToolbarRegistry,
    app_weak: glib::WeakRef<gtk4::Application>,
}

fn spawn_monitor_overlay(app: &gtk4::Application, monitor: &gdk4::Monitor, shared: &Shared) {
    let geo = monitor.geometry();
    let mon_w = geo.width().max(1);
    let mon_h = geo.height().max(1);

    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .decorated(false)
        .resizable(false)
        .build();
    window.add_css_class("hyprsnap-overlay");

    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_namespace(Some("hyprsnap-overlay"));
    window.set_monitor(Some(monitor));
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_exclusive_zone(-1);
    // Exclusive grab so keyboard shortcuts always reach the overlay — required because the
    // pointer-passthrough mode below removes the input region and would otherwise prevent the
    // surface from ever receiving focus.
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.set_default_size(mon_w, mon_h);

    let canvas = AnnotationCanvas::new();
    canvas.set_empty((mon_w as u32, mon_h as u32));
    canvas.set_transparent(true);
    canvas.set_tool(shared.current_tool.get());

    // Floating bottom-center toolbar per monitor. Each monitor gets its own instance, but
    // actions propagate to every canvas via the `Shared` state, and we mirror state changes
    // back to the other monitors' toolbars so the UI stays consistent.
    let toolbar = Toolbar::new(ToolbarSpec {
        tools: OVERLAY_TOOLS,
        show_undo: true,
        show_clear: true,
        show_passthrough_toggle: true,
        initial_tool: Some(shared.current_tool.get()),
        initial_passthrough: shared.passthrough.get(),
        ..Default::default()
    });
    wire_toolbar(&toolbar, shared, &canvas);

    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&canvas));
    toolbar.widget().set_halign(gtk4::Align::Center);
    toolbar.widget().set_valign(gtk4::Align::End);
    toolbar.widget().set_margin_bottom(24);
    overlay.add_overlay(toolbar.widget());

    window.set_child(Some(&overlay));
    install_keys(&window, shared);
    toolbar.install_shortcuts(&window);

    shared.canvases.borrow_mut().push(canvas);
    shared.toolbars.borrow_mut().push(toolbar);
    shared.windows.borrow_mut().push(window.clone());
    window.present();

    // Apply the initial passthrough state once the GTK surface exists. We can't take the
    // GdkSurface before `present()`, hence the deferred lambda.
    let passthrough = shared.passthrough.clone();
    let window_weak = window.downgrade();
    glib::idle_add_local_once(move || {
        if let Some(window) = window_weak.upgrade() {
            apply_passthrough(&window, passthrough.get());
        }
    });
}

/// Wire toolbar actions back into the per-overlay shared state. Tool / Clear / Passthrough
/// propagate across monitors so all toolbars stay in lockstep with the canvases.
fn wire_toolbar(toolbar: &Toolbar, shared: &Shared, canvas: &AnnotationCanvas) {
    let canvases = shared.canvases.clone();
    let windows = shared.windows.clone();
    let toolbars = shared.toolbars.clone();
    let passthrough = shared.passthrough.clone();
    let current_tool = shared.current_tool.clone();
    let canvas_weak = canvas.downgrade();
    toolbar.connect(move |action| match action {
        ToolbarAction::ToolSelected(kind) => {
            current_tool.set(kind);
            for c in canvases.borrow().iter() {
                c.set_tool(kind);
            }
            for t in toolbars.borrow().iter() {
                t.set_tool(kind);
            }
        }
        ToolbarAction::Undo => {
            if let Some(c) = canvas_weak.upgrade() {
                c.undo();
            }
        }
        ToolbarAction::Clear => {
            for c in canvases.borrow().iter() {
                c.clear_layers();
            }
        }
        ToolbarAction::PassthroughToggled(on) => {
            passthrough.set(on);
            for w in windows.borrow().iter() {
                apply_passthrough(w, on);
            }
            for t in toolbars.borrow().iter() {
                t.set_passthrough(on);
            }
            tracing::info!(passthrough = on, "overlay passthrough toggled");
        }
        _ => {}
    });
}

/// Window-level keys not owned by the toolbar (Esc to quit).
fn install_keys(window: &gtk4::ApplicationWindow, shared: &Shared) {
    let key = gtk4::EventControllerKey::new();
    let windows = shared.windows.clone();
    let app_weak = shared.app_weak.clone();
    key.connect_key_pressed(move |_, k, _, _| match k {
        gdk4::Key::Escape => {
            tear_down(&windows, &app_weak);
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    });
    window.add_controller(key);
}

/// Toggle pointer passthrough on a single overlay window. An empty input region tells the
/// compositor to forward pointer events to the surface beneath; `None` restores the default
/// (the window absorbs everything inside its bounds).
fn apply_passthrough(window: &gtk4::ApplicationWindow, passthrough: bool) {
    let Some(surface) = window.surface() else {
        return;
    };
    if passthrough {
        let empty = cairo::Region::create();
        surface.set_input_region(Some(&empty));
    } else {
        let r = cairo::RectangleInt::new(0, 0, surface.width(), surface.height());
        let region = cairo::Region::create_rectangle(&r);
        surface.set_input_region(Some(&region));
    }
}

fn tear_down(windows: &WindowRegistry, app_weak: &glib::WeakRef<gtk4::Application>) {
    for window in windows.borrow_mut().drain(..) {
        window.set_visible(false);
        window.destroy();
    }
    if let Some(display) = gdk4::Display::default() {
        display.flush();
    }
    if let Some(app) = app_weak.upgrade() {
        app.quit();
    }
}
