//! Layer-shell overlay used by both the live "draw on screen" flow and the in-place
//! annotation-editing flow (`screenshot --edit`, Shift-click / Shift+Enter on the selector's
//! Capture button, and the tray "Annotate region…" entry).
//!
//! Spawns one `gtk4_layer_shell` window per monitor at `Layer::Overlay`. Each hosts an
//! [`AnnotationCanvas`] sized to its monitor, plus a floating bottom-center
//! [`crate::ui::Toolbar`]. The keyboard is grabbed exclusively while the overlay is alive so
//! the user's tool shortcuts always reach us, even when input passthrough lets pointer events
//! fall through to whatever app is underneath.
//!
//! Two modes are supported via [`OverlayMode`]:
//!
//! * [`OverlayMode::Draw`] — Draw-On-Gnome equivalent: transparent canvases, pointer
//!   passthrough toggle, Undo/Clear shortcuts. The overlay stays alive until the user presses
//!   `Esc` (or an external shutdown receiver fires) and writes nothing.
//! * [`OverlayMode::Edit`] — annotate the just-captured image in place. Each per-monitor
//!   canvas renders its slice of the base image; the toolbar adds a Save button that
//!   composes every per-monitor canvas, stitches the slices back together, fans the result
//!   out to the configured sinks, and tears the overlay down.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};

use anyhow::{Result, anyhow, bail};
use gtk4::cairo;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::annotate::{Document, DocumentBase, ToolKind};
use crate::capture::region::{Rect, slice_pixels};
use crate::capture::{CapturedImage, Capturer};
use crate::cli::SinkSpec;
use crate::config::FilenameContext;
use crate::context::Ctx;
use crate::output::Outputs;
use crate::ui::canvas::AnnotationCanvas;
use crate::ui::save::{SaveFn, sinks_save_fn};
use crate::ui::selector;
use crate::ui::toolbar::{EDITOR_TOOLS, OVERLAY_TOOLS, Toolbar, ToolbarAction, ToolbarSpec};
/// How the overlay should behave on this invocation.
pub enum OverlayMode {
    /// Live draw on top of the desktop (today's `hyprsnap draw` flow).
    Draw {
        /// Open the overlay with pointer passthrough enabled (clicks fall through).
        passthrough: bool,
        /// Sink(s) to receive the saved image when the user presses Ctrl+S / Save. An empty
        /// vec means "use `config.default_sinks()`" — same fallback as `screenshot`.
        sinks: Vec<SinkSpec>,
        /// Include the mouse cursor in captures triggered by the overlay's Save action. The
        /// interactive zone selector pop on Save can override this per-save via its own
        /// toggle.
        cursor: bool,
    },
    /// In-place annotation editor for a captured (or loaded) image. The overlay's per-monitor
    /// canvases each render the slice of `base` that falls inside their monitor, the toolbar
    /// grows a Save button, and the result is fanned out to `sinks` on save.
    Edit {
        base: DocumentBase,
        /// Top-left of `base` in compositor logical coordinates. Per-monitor slices are
        /// computed by intersecting each monitor's geometry with the rect
        /// `(origin, base.size)`.
        origin: (i32, i32),
        sinks: Vec<SinkSpec>,
    },
}

/// External commands sent from the daemon (or any other tokio context) to a live overlay.
///
/// Today we only carry passthrough toggles, but the channel is wired as `mpsc` so future
/// commands (cursor change, tool swap, …) can slot in without breaking the GTK plumbing.
#[derive(Debug, Clone, Copy)]
pub enum OverlayCommand {
    /// Flip pointer passthrough on/off as if the user had pressed the toolbar button.
    /// Useful from a Hyprland global keybind when keyboard is detached from the surface
    /// (passthrough turns `KeyboardMode` to `None`, so the overlay can't see `P` itself).
    TogglePassthrough,
}

/// Channel used by the daemon to send [`OverlayCommand`]s into a running overlay.
pub type OverlayCommandRx = tokio::sync::mpsc::UnboundedReceiver<OverlayCommand>;

/// Launch the overlay. Returns once the user presses `Esc`, hits Save (Edit mode), or the
/// supplied `shutdown` receiver fires.
///
/// In Draw mode the returned vector is always empty. In Edit mode it contains the paths the
/// save closure wrote to (clipboard sinks contribute nothing).
pub async fn run(
    ctx: Ctx,
    mode: OverlayMode,
    shutdown: Option<tokio::sync::oneshot::Receiver<()>>,
    commands: Option<OverlayCommandRx>,
) -> Result<Vec<PathBuf>> {
    let written: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::sync_channel::<Result<()>>(1);
    let collected = written.clone();
    tokio::task::spawn_blocking(move || run_gtk(ctx, mode, tx, shutdown, commands, collected))
        .await
        .map_err(|e| anyhow!("overlay task panicked: {e}"))??;
    rx.recv()
        .map_err(|e| anyhow!("overlay channel closed without a result: {e}"))??;
    Ok(std::mem::take(&mut written.lock().unwrap()))
}

type CanvasRegistry = Rc<RefCell<Vec<MonitorCanvas>>>;
type WindowRegistry = Rc<RefCell<Vec<gtk4::ApplicationWindow>>>;
type ToolbarRegistry = Rc<RefCell<Vec<Toolbar>>>;
type ResultSender = Arc<Mutex<Option<mpsc::SyncSender<Result<()>>>>>;

/// Per-monitor canvas paired with the region of the unified Edit-mode buffer it owns. In Draw
/// mode `slice` is `None` and the canvas is empty + transparent.
struct MonitorCanvas {
    canvas: AnnotationCanvas,
    /// Top-left + size of this canvas's pixels in the unified `base` coordinate space. Only
    /// set in Edit mode.
    slice: Option<Rect>,
    /// Logical-coordinate geometry of the monitor this canvas covers. Needed in Draw mode by
    /// the lazy desktop capture (Blur tool) to slice the stitched compositor frame into
    /// per-monitor `DocumentBase` chunks. Mirrors the rect computed in
    /// [`spawn_monitor_overlay`].
    monitor_rect: Rect,
}

fn run_gtk(
    ctx: Ctx,
    mode: OverlayMode,
    tx: mpsc::SyncSender<Result<()>>,
    shutdown: Option<tokio::sync::oneshot::Receiver<()>>,
    commands: Option<OverlayCommandRx>,
    collected: Arc<Mutex<Vec<PathBuf>>>,
) -> Result<()> {
    let app = gtk4::Application::builder()
        .application_id(crate::ui::APP_ID)
        .build();
    let tx: ResultSender = Arc::new(Mutex::new(Some(tx)));

    // `connect_activate` is Fn, so we wrap moveable state in interior-mutability cells that the
    // first activation drains.
    let shutdown_cell: Rc<RefCell<Option<tokio::sync::oneshot::Receiver<()>>>> =
        Rc::new(RefCell::new(shutdown));
    let commands_cell: Rc<RefCell<Option<OverlayCommandRx>>> = Rc::new(RefCell::new(commands));
    let mode_cell: Rc<RefCell<Option<OverlayMode>>> = Rc::new(RefCell::new(Some(mode)));
    let collected_cell = collected.clone();

    {
        let tx = tx.clone();
        let shutdown_cell = shutdown_cell.clone();
        let commands_cell = commands_cell.clone();
        app.connect_activate(move |app| {
            crate::ui::install_icon_resources();
            let Some(mode) = mode_cell.borrow_mut().take() else {
                return;
            };
            match build_overlays(app, ctx.clone(), mode, collected_cell.clone()) {
                Ok(shared) => {
                    if let Some(rx) = shutdown_cell.borrow_mut().take() {
                        attach_shutdown(&shared, rx);
                    }
                    if let Some(rx) = commands_cell.borrow_mut().take() {
                        attach_commands(&shared, rx);
                    }
                }
                Err(err) => {
                    send_once(&tx, Err(err));
                    app.quit();
                }
            }
        });
    }

    let exit = app.run_with_args::<&str>(&[]);
    let code: i32 = exit.into();
    // Treat a normal quit (Esc / Save / external shutdown) as success. If the channel still has
    // a slot, fill it so the caller's recv() doesn't dangle.
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

/// Wire an external command receiver into the GTK main context. Used by the daemon to inject
/// [`OverlayCommand`]s — most importantly `TogglePassthrough`, which lets a Hyprland global
/// keybind flip the overlay back to interactive when `KeyboardMode::None` has detached the
/// surface from the keyboard.
fn attach_commands(shared: &Shared, mut rx: OverlayCommandRx) {
    let passthrough = shared.passthrough.clone();
    let windows = shared.windows.clone();
    let toolbars = shared.toolbars.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                OverlayCommand::TogglePassthrough => {
                    let next = !passthrough.get();
                    apply_passthrough_state(&passthrough, &windows, &toolbars, next);
                }
            }
        }
    });
}

fn send_once(tx: &ResultSender, msg: Result<()>) {
    if let Ok(mut guard) = tx.lock()
        && let Some(sender) = guard.take()
    {
        let _ = sender.send(msg);
    }
}

fn build_overlays(
    app: &gtk4::Application,
    ctx: Ctx,
    mode: OverlayMode,
    collected: Arc<Mutex<Vec<PathBuf>>>,
) -> Result<Shared> {
    crate::ui::style::install();

    let display = gdk4::Display::default().ok_or_else(|| anyhow!("no GDK display available"))?;
    let monitors_list = display.monitors();
    let n = monitors_list.n_items();
    if n == 0 {
        bail!("no monitors reported by GDK");
    }

    let (initial_passthrough, edit, draw_save) = match mode {
        OverlayMode::Draw {
            passthrough,
            sinks,
            cursor,
        } => {
            // Draw mode wires Save through `run_draw_save` (Ctrl+S / Enter / click) which
            // pops the zone selector, captures, encodes, fans out to sinks, then leaves the
            // overlay alive so the user keeps drawing.
            let draw_save = DrawSaveState {
                sinks,
                cursor,
                app_ctx: ctx.clone(),
                runtime: tokio::runtime::Handle::current(),
                collected: collected.clone(),
            };
            (passthrough, None, Some(Rc::new(draw_save)))
        }
        OverlayMode::Edit {
            base,
            origin,
            sinks,
        } => {
            // Save closure is built once, shared across every monitor's toolbar. The
            // `selection_label` populates the `{selection}` token in the filename template.
            let save = sinks_save_fn(ctx.clone(), sinks, "edit", collected);
            (
                false,
                Some(EditState {
                    base: Arc::new(base),
                    origin,
                    save,
                }),
                None,
            )
        }
    };

    let initial_tool = if edit.is_some() {
        ToolKind::Rect
    } else {
        ToolKind::Freehand
    };

    let shared = Shared {
        passthrough: Rc::new(Cell::new(initial_passthrough)),
        current_tool: Rc::new(Cell::new(initial_tool)),
        canvases: Rc::new(RefCell::new(Vec::new())),
        windows: Rc::new(RefCell::new(Vec::new())),
        toolbars: Rc::new(RefCell::new(Vec::new())),
        app_weak: app.downgrade(),
        edit: edit.map(Rc::new),
        draw_save,
        blur_capture_in_flight: Rc::new(Cell::new(false)),
        annotate_colors: Rc::new(ctx.config.annotate.colors.clone()),
    };

    let mut windows = Vec::with_capacity(n as usize);
    for i in 0..n {
        let Some(obj) = monitors_list.item(i) else {
            continue;
        };
        let Ok(monitor) = obj.downcast::<gdk4::Monitor>() else {
            continue;
        };
        if let Some(w) = spawn_monitor_overlay(app, &monitor, &shared) {
            windows.push(w);
        }
    }

    if windows.is_empty() {
        bail!("{}", crate::i18n::fl!("error-overlay-no-monitor"));
    }
    // Two-phase: every per-monitor window is fully built above, so the loop below can
    // commit the initial Wayland surfaces back-to-back without GTK doing any widget /
    // pixel-slice work in between. The compositor then maps every layer surface in the
    // same frame instead of one-monitor-at-a-time staircase pop-in (see §23).
    for w in &windows {
        w.present();
    }
    Ok(shared)
}

/// State that's only present in Edit mode. `Rc`-shared so the per-monitor save handler can
/// reach it without taking ownership of `Shared`.
struct EditState {
    base: Arc<DocumentBase>,
    origin: (i32, i32),
    save: SaveFn,
}

/// State that's only present in Draw mode (with a save target). Carries everything
/// `run_draw_save` needs to drive a Save action: the sinks list, the cursor default for the
/// zone selector, the shared application context (for filename templating + PNG
/// compression + default sinks fallback + the daemon-mode flag), and the path-collection
/// vec the outer `overlay::run` returns.
struct DrawSaveState {
    sinks: Vec<SinkSpec>,
    cursor: bool,
    app_ctx: Ctx,
    /// Captured tokio runtime handle. The Save flow needs to `await` tokio futures
    /// (`WlrCapturer::capture`, `Outputs::write_png`) from inside a `glib::MainContext`
    /// task; we offload that work to a `tokio::spawn`ed task whose `JoinHandle` we then
    /// await from the GTK thread, with this handle as the runtime anchor.
    runtime: tokio::runtime::Handle,
    collected: Arc<Mutex<Vec<PathBuf>>>,
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
    edit: Option<Rc<EditState>>,
    /// Set in Draw mode when the overlay should respond to Save (Ctrl+S / Enter / click).
    /// `None` would disable the Save UI entirely; today we always set this in Draw mode.
    draw_save: Option<Rc<DrawSaveState>>,
    /// `true` while the lazy Draw-mode Blur desktop capture is in flight. Prevents the user
    /// from re-triggering the capture by tapping Blur again before the first capture lands.
    blur_capture_in_flight: Rc<Cell<bool>>,
    /// Per-tool default colors applied to every freshly-constructed
    /// [`AnnotationCanvas`] in [`spawn_monitor_overlay`]. Cloned from
    /// `ctx.config.annotate.colors` in [`build_overlays`].
    annotate_colors: Rc<crate::config::AnnotateColors>,
}

/// Build (or skip) one overlay window for a monitor. Returns `Some(window)` when a window
/// was built, `None` when the monitor should be skipped (Edit mode only: monitor doesn't
/// intersect the captured rect).
///
/// The returned window is fully wired but **not yet presented** — the caller batches the
/// `present()` calls across all monitors so the compositor maps every layer surface in the
/// same frame instead of in a staircase (see §23).
fn spawn_monitor_overlay(
    app: &gtk4::Application,
    monitor: &gdk4::Monitor,
    shared: &Shared,
) -> Option<gtk4::ApplicationWindow> {
    let geo = monitor.geometry();
    let mon_w = geo.width().max(1);
    let mon_h = geo.height().max(1);
    let mon_rect = Rect {
        x: geo.x(),
        y: geo.y(),
        w: mon_w as u32,
        h: mon_h as u32,
    };

    // Edit-mode bail-out: if this monitor doesn't intersect the captured base, don't open a
    // window at all (otherwise the user sees a stray transparent overlay grabbing keyboard
    // focus on monitors that have nothing to annotate).
    let slice = if let Some(edit) = shared.edit.as_ref() {
        let base_rect = Rect {
            x: edit.origin.0,
            y: edit.origin.1,
            w: edit.base.width,
            h: edit.base.height,
        };
        match mon_rect.intersect(&base_rect) {
            Some(s) => Some(s),
            None => return None,
        }
    } else {
        None
    };

    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .decorated(false)
        .resizable(false)
        .icon_name(crate::ui::APP_ID)
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
    canvas.apply_color_defaults(&shared.annotate_colors);
    if let (Some(edit), Some(slice)) = (shared.edit.as_ref(), slice) {
        // Slice the unified base into this monitor's portion. The canvas widget is sized to
        // the monitor's full logical rect (so layer-shell anchoring lines up); inside the
        // canvas, the document is sized to the slice and rendered at the intra-monitor
        // offset.
        let (pixels, sw, sh) = slice_pixels(
            &edit.base.pixels,
            edit.base.width,
            edit.base.height,
            edit.base.stride,
            edit.origin,
            slice,
        )
        .expect("slice intersects (checked above)");
        let slice_base = DocumentBase {
            pixels: std::sync::Arc::from(pixels.into_boxed_slice()),
            width: sw,
            height: sh,
            stride: sw * 4,
        };
        canvas.set_document(Document::with_base(slice_base));
        canvas.set_transparent(true);
    } else {
        canvas.set_empty((mon_w as u32, mon_h as u32));
        canvas.set_transparent(true);
    }
    canvas.set_tool(shared.current_tool.get());

    // Edit mode shows the full editor toolset (incl. Blur + Crop) plus a Save button. Draw
    // mode keeps the slimmer overlay set (no Crop) and adds the passthrough toggle + Clear
    // shortcut. Blur is in the overlay too: it grabs the desktop into a hidden base on first
    // use so the GSK blur node has real pixels to sample.
    let toolbar = if shared.edit.is_some() {
        Toolbar::new(ToolbarSpec {
            tools: EDITOR_TOOLS,
            show_undo: true,
            show_save: true,
            show_color_picker: true,
            show_style_picker: true,
            show_font_size_picker: true,
            initial_tool: Some(shared.current_tool.get()),
            ..Default::default()
        })
    } else {
        Toolbar::new(ToolbarSpec {
            tools: OVERLAY_TOOLS,
            show_undo: true,
            show_clear: true,
            show_save: shared.draw_save.is_some(),
            show_passthrough_toggle: true,
            show_color_picker: true,
            show_style_picker: true,
            show_font_size_picker: true,
            initial_tool: Some(shared.current_tool.get()),
            initial_passthrough: shared.passthrough.get(),
            ..Default::default()
        })
    };
    wire_toolbar(&toolbar, shared, &canvas);

    let overlay = gtk4::Overlay::new();

    // For Edit mode, anchor the document to its slice's intra-monitor offset so the captured
    // pixels sit exactly where they were on the user's desktop. The canvas widget itself is
    // sized to the slice, not the monitor.
    if let Some(slice) = slice {
        let offset_x = slice.x - geo.x();
        let offset_y = slice.y - geo.y();
        canvas.set_halign(gtk4::Align::Start);
        canvas.set_valign(gtk4::Align::Start);
        canvas.set_margin_start(offset_x);
        canvas.set_margin_top(offset_y);
        canvas.set_size_request(slice.w as i32, slice.h as i32);
    }

    overlay.set_child(Some(&canvas));
    toolbar.widget().set_halign(gtk4::Align::Center);
    toolbar.widget().set_valign(gtk4::Align::End);
    toolbar.widget().set_margin_bottom(24);
    overlay.add_overlay(toolbar.widget());

    window.set_child(Some(&overlay));
    install_keys(&window, shared);
    toolbar.install_shortcuts(&window);

    let toolbar_widget = toolbar.widget().clone();
    shared.canvases.borrow_mut().push(MonitorCanvas {
        canvas: canvas.clone(),
        slice,
        monitor_rect: mon_rect,
    });
    shared.toolbars.borrow_mut().push(toolbar);
    shared.windows.borrow_mut().push(window.clone());

    // GDK4's Wayland backend recomputes wl_surface::set_input_region every frame from the
    // widget allocation, so a one-shot set_input_region call gets clobbered on the next
    // paint. Install a frame-clock::after-paint handler that re-asserts the desired region
    // on every frame, driven by the shared passthrough cell. The handler covers Draw and
    // Edit alike — Edit just keeps the full-window region every frame, a no-op.
    install_passthrough_for_surface(&window, toolbar_widget, shared.passthrough.clone());
    Some(window)
}

/// Flip pointer passthrough on every window managed by this overlay.
///
/// Single source of truth shared by the toolbar's own `PassthroughToggled` action and the
/// daemon-driven [`OverlayCommand::TogglePassthrough`]. Beside flipping the shared cell and
/// asking each surface to re-render (so the per-frame `after-paint` handler picks the new
/// state up immediately), this also flips `KeyboardMode` between `Exclusive` and `None`:
/// Hyprland binds the pointer to a keyboard-`Exclusive` layer surface, so an empty input
/// region alone is not enough to let clicks reach the apps below — the keyboard grab has to
/// go away as well. Trade-off: the overlay's keyboard shortcuts (e.g. `P`) stop working
/// while passthrough is on; the daemon IPC `PassthroughToggle` is the user's recovery path,
/// typically bound to a Hyprland global keybind.
fn apply_passthrough_state(
    passthrough: &Rc<Cell<bool>>,
    windows: &WindowRegistry,
    toolbars: &ToolbarRegistry,
    on: bool,
) {
    passthrough.set(on);
    let mode = if on {
        // `None` rather than `OnDemand`: with `OnDemand` Hyprland would still re-grab the
        // pointer the first time the toolbar gets focus. We want the surface fully detached
        // from the keyboard until the user explicitly toggles back.
        KeyboardMode::None
    } else {
        KeyboardMode::Exclusive
    };
    for w in windows.borrow().iter() {
        w.set_keyboard_mode(mode);
        if let Some(s) = w.surface() {
            // queue_render asks for the next frame immediately rather than waiting for damage;
            // the after-paint handler will then re-derive the input region from `on`.
            s.queue_render();
        }
    }
    for t in toolbars.borrow().iter() {
        t.set_passthrough(on);
    }
    tracing::info!(passthrough = on, "overlay passthrough toggled");
}

/// Wire toolbar actions back into the per-overlay shared state. Tool / Clear / Passthrough
/// propagate across monitors so all toolbars stay in lockstep with the canvases. Save (Edit
/// mode only) composes every per-monitor canvas, stitches the slices back into a single
/// buffer at the original base size, fans the result out to sinks, and tears down.
fn wire_toolbar(toolbar: &Toolbar, shared: &Shared, canvas: &AnnotationCanvas) {
    let canvases = shared.canvases.clone();
    let windows = shared.windows.clone();
    let toolbars = shared.toolbars.clone();
    let passthrough = shared.passthrough.clone();
    let current_tool = shared.current_tool.clone();
    let canvas_weak = canvas.downgrade();
    let edit = shared.edit.clone();
    let draw_save = shared.draw_save.clone();
    let app_weak = shared.app_weak.clone();
    let blur_in_flight = shared.blur_capture_in_flight.clone();

    // Seed the picker with the initial tool's color + correct sensitivity. Done up front so
    // the picker doesn't show its built-in default before the user touches a tool button.
    let initial_kind = current_tool.get();
    if let Some(color) = canvas.tool_color(initial_kind) {
        toolbar.set_color(color);
    }
    toolbar.set_color_picker_sensitive(kind_is_colorable(initial_kind));
    if let Some(style) = canvas.tool_style(initial_kind) {
        toolbar.set_stroke_style(style);
    }
    toolbar.set_style_picker_sensitive(kind_is_styleable(initial_kind));
    if let Some(size) = canvas.tool_font_size(initial_kind) {
        toolbar.set_font_size(size);
    }
    toolbar.set_font_size_picker_sensitive(kind_has_font_size(initial_kind));

    toolbar.connect(move |action| match action {
        ToolbarAction::ToolSelected(kind) => {
            current_tool.set(kind);
            for c in canvases.borrow().iter() {
                c.canvas.set_tool(kind);
            }
            // Lazy desktop capture for Draw-mode Blur. The overlay is transparent so the
            // GSK blur node has nothing to sample by default; the first time the user picks
            // the Blur tool we briefly hide the overlay surfaces, grab the desktop via
            // wlr-screencopy, and attach the result as a hidden base on every canvas.
            // Subsequent Blur uses reuse this base. `Crop` and other tools take no part.
            if matches!(kind, ToolKind::Blur)
                && let Some(ds) = draw_save.as_ref()
                && canvases
                    .borrow()
                    .first()
                    .map(|c| !c.canvas.has_base())
                    .unwrap_or(false)
                && !blur_in_flight.replace(true)
            {
                glib::MainContext::default().spawn_local(ensure_draw_blur_base(
                    canvases.clone(),
                    windows.clone(),
                    toolbars.clone(),
                    passthrough.clone(),
                    ds.runtime.clone(),
                    blur_in_flight.clone(),
                ));
            }
            // Push the new tool's stored color into every peer toolbar's swatch (silently),
            // and toggle picker sensitivity for tools with hardcoded appearance.
            let color = canvases
                .borrow()
                .first()
                .and_then(|c| c.canvas.tool_color(kind));
            let style = canvases
                .borrow()
                .first()
                .and_then(|c| c.canvas.tool_style(kind));
            let font_size = canvases
                .borrow()
                .first()
                .and_then(|c| c.canvas.tool_font_size(kind));
            let colorable = kind_is_colorable(kind);
            let styleable = kind_is_styleable(kind);
            let has_font_size = kind_has_font_size(kind);
            for t in toolbars.borrow().iter() {
                t.set_tool(kind);
                if let Some(c) = color {
                    t.set_color(c);
                }
                t.set_color_picker_sensitive(colorable);
                if let Some(s) = style {
                    t.set_stroke_style(s);
                }
                t.set_style_picker_sensitive(styleable);
                if let Some(s) = font_size {
                    t.set_font_size(s);
                }
                t.set_font_size_picker_sensitive(has_font_size);
            }
        }
        ToolbarAction::ColorChanged(color) => {
            let kind = current_tool.get();
            if !kind_is_colorable(kind) {
                return;
            }
            for c in canvases.borrow().iter() {
                c.canvas.set_tool_color(kind, color);
            }
            // Sync peer toolbars on other monitors so their swatches reflect the new color.
            for t in toolbars.borrow().iter() {
                t.set_color(color);
            }
        }
        ToolbarAction::StrokeStyleChanged(style) => {
            let kind = current_tool.get();
            if !kind_is_styleable(kind) {
                return;
            }
            for c in canvases.borrow().iter() {
                c.canvas.set_tool_style(kind, style);
            }
            for t in toolbars.borrow().iter() {
                t.set_stroke_style(style);
            }
        }
        ToolbarAction::FontSizeChanged(size) => {
            let kind = current_tool.get();
            if !kind_has_font_size(kind) {
                return;
            }
            for c in canvases.borrow().iter() {
                c.canvas.set_tool_font_size(kind, size);
            }
            for t in toolbars.borrow().iter() {
                t.set_font_size(size);
            }
        }
        ToolbarAction::Undo => {
            if let Some(c) = canvas_weak.upgrade() {
                c.undo();
            }
        }
        ToolbarAction::Clear => {
            for c in canvases.borrow().iter() {
                c.canvas.clear_layers();
            }
        }
        ToolbarAction::PassthroughToggled(on) => {
            apply_passthrough_state(&passthrough, &windows, &toolbars, on);
        }
        ToolbarAction::Save => {
            if let Some(edit) = edit.as_ref() {
                match compose_edit(&canvases.borrow(), edit) {
                    Ok(stitched) => match (edit.save)(&stitched) {
                        Ok(paths) => {
                            for p in &paths {
                                println!("{}", p.display());
                            }
                            tear_down(&windows, &app_weak);
                        }
                        Err(err) => tracing::error!(error = ?err, "save failed"),
                    },
                    Err(err) => tracing::error!(error = ?err, "composing edit failed"),
                }
            } else if let Some(draw_save) = draw_save.as_ref() {
                // Draw-mode save: hand off to `run_draw_save` on the GLib main context so
                // we can `await` `pick_region_in_app` + the tokio-backed capture pipeline.
                // The overlay stays alive afterwards — Draw save is non-terminating.
                glib::MainContext::default().spawn_local(run_draw_save(
                    app_weak.clone(),
                    windows.clone(),
                    toolbars.clone(),
                    passthrough.clone(),
                    draw_save.clone(),
                ));
            }
        }
        _ => {}
    });
}

/// Tools whose appearance is driven by the picker. Blur, Crop and Redact have hardcoded
/// rendering and disable the picker when active.
fn kind_is_colorable(kind: ToolKind) -> bool {
    matches!(
        kind,
        ToolKind::Rect
            | ToolKind::Ellipse
            | ToolKind::Arrow
            | ToolKind::Line
            | ToolKind::Highlight
            | ToolKind::Freehand
            | ToolKind::Number
            | ToolKind::Text
    )
}

/// Tools whose stroke is styleable via the dash picker. A subset of `kind_is_colorable`:
/// only outline-rendering tools qualify. Highlight (filled rectangle), Number (text-on-disc)
/// and Text (glyph rendering) have no outline to style. Arrow's arrowhead stays solid but
/// its shaft is styled — see `styled_stroke` in `canvas.rs`.
fn kind_is_styleable(kind: ToolKind) -> bool {
    matches!(
        kind,
        ToolKind::Rect | ToolKind::Ellipse | ToolKind::Arrow | ToolKind::Line | ToolKind::Freehand
    )
}

/// Tools whose appearance includes a configurable font size. Currently only the Text
/// tool exposes one; Number's font size is derived from its disc radius and stays
/// implicit.
fn kind_has_font_size(kind: ToolKind) -> bool {
    matches!(kind, ToolKind::Text)
}

/// Compose every per-monitor canvas into its slice, then stitch the slices back into a single
/// `CapturedImage` matching the original `base` rectangle. Strokes that straddle two monitors
/// are *not* unified — each canvas owns its own layers, so a stroke drawn on monitor A and
/// continued on monitor B appears as two separate layers in the final image.
fn compose_edit(canvases: &[MonitorCanvas], edit: &EditState) -> Result<CapturedImage> {
    let base = &edit.base;
    let dst_stride = (base.width as usize) * 4;
    let mut buf = vec![0u8; dst_stride * base.height as usize];

    // Start from the original BGRA-equivalent base so any monitor we didn't touch (or any
    // pixel a slice didn't cover) still shows the captured pixels. `DocumentBase` is RGBA;
    // we swizzle to BGRA on the fly to match `encode_png`'s expectations.
    let src_stride = base.stride as usize;
    for y in 0..base.height as usize {
        for x in 0..base.width as usize {
            let s = y * src_stride + x * 4;
            let d = y * dst_stride + x * 4;
            buf[d] = base.pixels[s + 2];
            buf[d + 1] = base.pixels[s + 1];
            buf[d + 2] = base.pixels[s];
            buf[d + 3] = base.pixels[s + 3];
        }
    }

    for mc in canvases {
        let Some(slice) = mc.slice else { continue };
        let composed = mc
            .canvas
            .compose()
            .map_err(|e| anyhow!("composing per-monitor canvas: {e}"))?;
        let off_x = (slice.x - edit.origin.0) as usize;
        let off_y = (slice.y - edit.origin.1) as usize;
        let copy_w = (composed.width as usize).min(base.width as usize - off_x);
        let copy_h = (composed.height as usize).min(base.height as usize - off_y);
        let src_stride = composed.stride as usize;
        let copy_bytes = copy_w * 4;
        for y in 0..copy_h {
            let s = y * src_stride;
            let d = (off_y + y) * dst_stride + off_x * 4;
            buf[d..d + copy_bytes].copy_from_slice(&composed.pixels[s..s + copy_bytes]);
        }
    }

    Ok(CapturedImage {
        width: base.width,
        height: base.height,
        stride: dst_stride as u32,
        pixels: std::sync::Arc::from(buf.into_boxed_slice()),
        source: None,
    })
}

/// Draw-mode save flow. Pops the screenshot zone selector to let the user pick what region
/// to save, then captures + encodes + writes via the same path `screenshot` uses. Strokes
/// are already painted on the layer-shell surfaces and therefore baked into whatever
/// `zwlr_screencopy` returns — no in-process compositing needed. Hides toolbar widgets
/// during the selector + capture so they don't appear in the saved PNG; re-shows them on
/// the way out so the user keeps drawing.
///
/// Grab the underlying desktop into a hidden base on every Draw-mode canvas so the Blur
/// tool's GSK render node has real pixels to sample. The overlay layer-shell surfaces are
/// transparent above the desktop, but the compositor framebuffer also contains our toolbar
/// chrome; we hide the toolbars for the duration of the capture so the blur source stays
/// clean of UI. Existing strokes are intentionally left visible: they're already part of
/// what the user is "looking at", so blurring includes them — same semantics as Edit mode
/// where the layered annotations sit on top of the captured pixels.
///
/// We deliberately do **not** flip passthrough or unmap the canvas. Both would race a fast
/// click-drag the user makes immediately after selecting Blur — the input region / keyboard
/// mode commit takes ~1 frame to reach the compositor and any drag begun in that window
/// would be lost. The toolbar widget hide is local-only (no Wayland commit) and harmless.
///
/// `blur_in_flight` is reset to `false` no matter the outcome so a future Blur selection
/// (after a failed capture, e.g. the user denied screencopy permission) can retry.
async fn ensure_draw_blur_base(
    canvases: CanvasRegistry,
    _windows: WindowRegistry,
    toolbars: ToolbarRegistry,
    _passthrough: Rc<Cell<bool>>,
    runtime: tokio::runtime::Handle,
    blur_in_flight: Rc<Cell<bool>>,
) {
    // Toolbar chrome is opaque; hide it so it doesn't end up in the blur source. Snapshot
    // visibility so we restore exactly what the user had (in case a future feature toggles
    // it externally).
    let prev_toolbar_visible: Vec<bool> = toolbars
        .borrow()
        .iter()
        .map(|t| t.widget().is_visible())
        .collect();
    for t in toolbars.borrow().iter() {
        t.widget().set_visible(false);
    }

    // Yield long enough for the toolbar-hide commit to reach the compositor. One frame at
    // 60 Hz + a small margin matches what `run_draw_save` uses for the same purpose.
    glib::timeout_future(std::time::Duration::from_millis(32)).await;

    // Capture the full desktop bounding box. We slice per-monitor below using each
    // canvas's `monitor_rect`, so the wlr capture only needs a single round-trip.
    let join = runtime.spawn(async move {
        let capturer = crate::capture::wlr::WlrCapturer::new()?;
        let images = capturer
            .capture(crate::capture::Selection::Full, false)
            .await?;
        let stitched = crate::capture::region::stitch(&images, &crate::capture::Selection::Full)?;
        anyhow::Ok(stitched)
    });

    let stitched = match join.await {
        Ok(Ok(img)) => Some(img),
        Ok(Err(err)) => {
            tracing::warn!(error = ?err, "draw-blur: desktop capture failed; blur will fall back to the grey-wash preview");
            None
        }
        Err(err) => {
            tracing::warn!(error = ?err, "draw-blur: capture task panicked");
            None
        }
    };

    // Restore toolbar visibility before we touch the canvases — keeps the visual blip as
    // short as possible (the post-capture set_hidden_base only queues a redraw).
    for (t, vis) in toolbars.borrow().iter().zip(&prev_toolbar_visible) {
        t.widget().set_visible(*vis);
    }

    if let Some(stitched) = stitched {
        // Stitched origin is the bounding-box top-left in compositor logical coordinates;
        // see `capture::region::stitch` for the math. We need that to slice per monitor.
        let origin = stitched_origin(&stitched);
        for c in canvases.borrow().iter() {
            let Some((bgra, w, h)) = slice_pixels(
                &stitched.pixels,
                stitched.width,
                stitched.height,
                stitched.stride,
                origin,
                c.monitor_rect,
            ) else {
                continue;
            };
            // wlr-screencopy hands back BGRA8888 premultiplied; `DocumentBase` /
            // `build_base_texture` expect RGBA (`gdk::MemoryFormat::R8g8b8a8`). Without
            // this swap the blurred region looks yellow/red-tinted because the R and B
            // channels are exchanged. Mirrors `cli::screenshot::base_from_captured`.
            let mut rgba = bgra;
            for px in rgba.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
            let base = DocumentBase {
                pixels: Arc::from(rgba.into_boxed_slice()),
                width: w,
                height: h,
                stride: w * 4,
            };
            c.canvas.set_hidden_base(base);
        }
    }

    blur_in_flight.set(false);
}

/// The bounding box that [`crate::capture::region::stitch`] uses for a `Full` selection is
/// the union of every output's logical rect, with the resulting pixels laid out from the
/// top-left of that union. Reconstruct that origin so `slice_pixels` can map monitor logical
/// coordinates back into the stitched buffer.
fn stitched_origin(_stitched: &CapturedImage) -> (i32, i32) {
    let Some(display) = gtk4::gdk::Display::default() else {
        return (0, 0);
    };
    let monitors = display.monitors();
    let mut origin: Option<(i32, i32)> = None;
    for i in 0..monitors.n_items() {
        let Some(obj) = monitors.item(i) else {
            continue;
        };
        let Ok(m) = obj.downcast::<gtk4::gdk::Monitor>() else {
            continue;
        };
        let g = m.geometry();
        origin = Some(match origin {
            None => (g.x(), g.y()),
            Some((x, y)) => (x.min(g.x()), y.min(g.y())),
        });
    }
    origin.unwrap_or((0, 0))
}

/// Runs on the GLib main context (`spawn_local`). Long-running async work (the actual
/// `WlrCapturer::capture` + `Outputs::write_png`) is offloaded onto the tokio runtime via
/// `runtime.spawn`, then awaited from here — the GTK loop stays responsive while the
/// capture is in flight.
async fn run_draw_save(
    app_weak: glib::WeakRef<gtk4::Application>,
    windows: WindowRegistry,
    toolbars: ToolbarRegistry,
    passthrough: Rc<Cell<bool>>,
    draw_save: Rc<DrawSaveState>,
) {
    let Some(app) = app_weak.upgrade() else {
        return;
    };

    // Snapshot the current passthrough state so we can restore it on the way out. Then hide
    // every per-monitor draw toolbar and force passthrough ON: with the toolbars unmapped,
    // `apply_passthrough_state` will fall back to an empty input region (no toolbar bounds
    // to keep clickable), and `KeyboardMode::None` detaches the keyboard. Net effect: the
    // draw overlay is fully transparent to input, so the selector that we map next
    // receives every pointer + keyboard event.
    let prev_passthrough = passthrough.get();
    for t in toolbars.borrow().iter() {
        t.widget().set_visible(false);
    }
    apply_passthrough_state(&passthrough, &windows, &toolbars, true);

    // Yield once so the parking commit (KeyboardMode swap + toolbar unmap) reaches the
    // compositor before the selector maps. Without this, Hyprland can route the first
    // pointer event back to the draw overlay because it sees the parking change one frame
    // later than the selector's `present()`.
    glib::timeout_future(std::time::Duration::from_millis(16)).await;

    // The draw overlay is itself an annotation surface, so the selector must not offer
    // its Shift→Annotate shortcut — going draw → pick zone → annotate again would be a
    // dead-end loop. Pass `allow_annotate = false` so Shift+click / Shift+Enter behave
    // exactly like a plain Capture. No pre-capture delay is plumbed through the draw
    // overlay flow today: the user is already on an annotation surface, so a timed delay
    // would just blur the workflow without an obvious entry point.
    let outcome = match selector::pick_region_in_app(
        &app,
        draw_save.cursor,
        std::time::Duration::ZERO,
        false,
        draw_save.app_ctx.config.ui.selector.clone(),
        draw_save.app_ctx.config.capture.initial_mode.into(),
    )
    .await
    {
        Ok(o) => o,
        Err(err) => {
            if err.chain().any(|e| e.is::<selector::Cancelled>()) {
                tracing::info!("draw-save: selector cancelled");
            } else {
                tracing::info!(error = ?err, "draw-save: selector cancelled");
            }
            for t in toolbars.borrow().iter() {
                t.widget().set_visible(true);
            }
            apply_passthrough_state(&passthrough, &windows, &toolbars, prev_passthrough);
            return;
        }
    };

    // Keep toolbars hidden through the capture so they don't appear in the saved PNG. The
    // selector's own dim veil is already gone (it destroyed its windows on commit and
    // honored the 30 ms post-commit grace internally), so the only thing the screencopy
    // can still pick up that we don't want is our own toolbar chrome.
    let selection = outcome.selection.clone();
    let cursor = outcome.cursor;
    let sinks = if draw_save.sinks.is_empty() {
        draw_save.app_ctx.config.default_sinks()
    } else {
        draw_save.sinks.clone()
    };
    let app_ctx = draw_save.app_ctx.clone();
    let collected = draw_save.collected.clone();
    let label = crate::cli::screenshot::selection_label(&selection);

    let join = draw_save.runtime.spawn(async move {
        let capturer = crate::capture::wlr::WlrCapturer::new()?;
        let images = capturer
            .capture(selection.clone(), cursor)
            .await
            .map_err(|e| anyhow!("capturing {selection:?}: {e}"))?;
        let stitched = crate::capture::region::stitch(&images, &selection)?;
        let png = crate::output::encode_png(&stitched, app_ctx.config.output.compression)?;
        let ctx_fname = FilenameContext {
            output: None,
            selection: Some(label),
        };
        let outputs = Outputs::from_specs(&sinks, &app_ctx, &ctx_fname)?;
        let paths = outputs.write_png(&png).await?;
        anyhow::Ok(paths)
    });

    match join.await {
        Ok(Ok(paths)) => {
            for p in &paths {
                println!("{}", p.display());
            }
            tracing::info!(
                count = paths.len(),
                "draw-save: wrote {} path(s)",
                paths.len()
            );
            if let Ok(mut g) = collected.lock() {
                g.extend(paths.iter().cloned());
            }
        }
        Ok(Err(err)) => tracing::error!(error = ?err, "draw-save: capture/write failed"),
        Err(err) => tracing::error!(error = ?err, "draw-save: capture task panicked"),
    }

    for t in toolbars.borrow().iter() {
        t.widget().set_visible(true);
    }
    apply_passthrough_state(&passthrough, &windows, &toolbars, prev_passthrough);
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

/// Hook a per-frame input-region reassertion onto `window`'s surface, driven by `passthrough`.
///
/// GDK4's Wayland backend re-derives `wl_surface::set_input_region` every paint from the
/// widget tree, which clobbers any manual one-shot call. We connect to the surface's frame
/// clock `after-paint` signal so we get to set the region *after* GDK's per-frame
/// recompute. The handler reads the shared `Cell` so all per-monitor handlers stay in
/// lockstep with the toggle without any cross-talk.
///
/// While passthrough is *on*, the region is shrunk to the toolbar widget's allocated bounds
/// (computed in window/surface coordinates) so the toolbar stays clickable — that's the
/// only escape hatch out of passthrough mode when the keyboard has been detached via
/// `KeyboardMode::None`. While passthrough is *off*, the full surface absorbs every event.
///
/// The surface usually exists by the time we reach this — `install_passthrough_for_surface`
/// is called from `spawn_monitor_overlay` after `present()` is queued in the caller's
/// batched present pass — but we defer via `connect_realize` if it isn't, so this stays
/// robust against future ordering changes.
fn install_passthrough_for_surface(
    window: &gtk4::ApplicationWindow,
    toolbar: gtk4::Widget,
    passthrough: Rc<Cell<bool>>,
) {
    fn attach(
        window: &gtk4::ApplicationWindow,
        surface: &gdk4::Surface,
        toolbar: gtk4::Widget,
        passthrough: Rc<Cell<bool>>,
    ) {
        let clock = surface.frame_clock();
        let surface_weak = surface.downgrade();
        let window_weak = window.downgrade();
        let toolbar_weak = toolbar.downgrade();
        let pt = passthrough.clone();
        clock.connect_after_paint(move |_| {
            if let (Some(s), Some(w), Some(tb)) = (
                surface_weak.upgrade(),
                window_weak.upgrade(),
                toolbar_weak.upgrade(),
            ) {
                apply_passthrough_to(&s, &w, &tb, pt.get());
            }
        });
        // Apply once immediately so the very first frame already has the right region —
        // we don't have to wait for the first `after-paint` to fire.
        apply_passthrough_to(surface, window, &toolbar, passthrough.get());
    }

    if let Some(s) = window.surface() {
        attach(window, &s, toolbar, passthrough);
    } else {
        let cell = std::cell::RefCell::new(Some((toolbar, passthrough)));
        window.connect_realize(move |w| {
            if let (Some((tb, pt)), Some(s)) = (cell.borrow_mut().take(), w.surface()) {
                attach(w, &s, tb, pt);
            }
        });
    }
}

/// Set the surface's input region. When `passthrough` is off, the full surface absorbs all
/// pointer events. When it is on, only the toolbar widget's bounding box absorbs events;
/// everything else (the transparent canvas) falls through to the application underneath.
///
/// Keeping the toolbar clickable in passthrough mode is what lets the user — or anyone
/// without a Hyprland keybind wired to the IPC — recover: a click on the toolbar's
/// passthrough button flips the cell back, the next frame restores the full input region,
/// and `KeyboardMode::Exclusive` re-enables every other shortcut.
fn apply_passthrough_to(
    surface: &gdk4::Surface,
    window: &gtk4::ApplicationWindow,
    toolbar: &gtk4::Widget,
    passthrough: bool,
) {
    let region = if passthrough {
        toolbar_input_region(window, toolbar).unwrap_or_else(cairo::Region::create)
    } else {
        let r = cairo::RectangleInt::new(0, 0, surface.width(), surface.height());
        cairo::Region::create_rectangle(&r)
    };
    surface.set_input_region(Some(&region));
}

/// Compute the toolbar widget's allocation expressed in the window's coordinate space and
/// wrap it in a single-rectangle `cairo::Region`. Returns `None` when the widget hasn't
/// been allocated yet (first frame between realize and the layer-shell map ack) so the
/// caller can fall back to an empty region (full passthrough, just for that frame).
fn toolbar_input_region(
    window: &gtk4::ApplicationWindow,
    toolbar: &gtk4::Widget,
) -> Option<cairo::Region> {
    let bounds = toolbar.compute_bounds(window)?;
    // graphene::Rect uses f32; round outward so we never clip the visible chrome. A 1-pixel
    // slack on each side absorbs subpixel positioning without leaking real estate to the
    // surface beyond what the eye sees.
    let x = bounds.x().floor() as i32;
    let y = bounds.y().floor() as i32;
    let w = bounds.width().ceil() as i32;
    let h = bounds.height().ceil() as i32;
    if w <= 0 || h <= 0 {
        return None;
    }
    Some(cairo::Region::create_rectangle(&cairo::RectangleInt::new(
        x, y, w, h,
    )))
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
