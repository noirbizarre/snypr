//! Interactive region selector overlay.
//!
//! Renders one fullscreen `gtk4_layer_shell` window per monitor at `Layer::Overlay` with
//! exclusive keyboard interactivity. The user drags a rectangle on any monitor; the first
//! window to finalise (mouse release, Enter, or Esc on any surface) commits the result and
//! tears down the rest.
//!
//! The selection is returned in compositor logical coordinates (the same space Hyprland and
//! `wlr-screencopy` report), so it can be plugged straight into `Selection::Region`.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};

use anyhow::{Result, anyhow, bail};
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::capture::region::Rect;
use crate::context::Ctx;

/// Show the selector and return the chosen region (in logical compositor coordinates).
///
/// Cancellation (`Esc`, window close, or an empty drag) surfaces as an error so callers can
/// short-circuit the rest of the screenshot pipeline.
pub async fn pick_region(_ctx: Ctx) -> Result<Rect> {
    let (tx, rx) = mpsc::sync_channel::<Result<Rect>>(1);
    tokio::task::spawn_blocking(move || run_gtk(tx))
        .await
        .map_err(|e| anyhow!("selector task panicked: {e}"))??;
    rx.recv()
        .map_err(|e| anyhow!("selector channel closed without a result: {e}"))?
}

/// Per-overlay live selection state (widget-local pixels).
#[derive(Clone, Copy, Debug, Default)]
struct LiveSelection {
    start: Option<(f64, f64)>,
    current: Option<(f64, f64)>,
}

impl LiveSelection {
    fn rect(&self) -> Option<(f64, f64, f64, f64)> {
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

/// One-shot sender shared across all overlays so only the first finaliser wins.
type Sender = Arc<Mutex<Option<mpsc::SyncSender<Result<Rect>>>>>;

fn send_once(tx: &Sender, msg: Result<Rect>) {
    if let Ok(mut guard) = tx.lock()
        && let Some(sender) = guard.take()
    {
        let _ = sender.send(msg);
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
    // Make sure callers awaiting on the channel never block forever if GTK exited without a
    // selection (e.g. compositor closed our surfaces).
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

    let finalised = Rc::new(Cell::new(false));
    let app_weak = app.downgrade();

    for i in 0..n {
        let Some(obj) = monitors_list.item(i) else {
            continue;
        };
        let Ok(monitor) = obj.downcast::<gdk4::Monitor>() else {
            continue;
        };
        spawn_monitor_overlay(app, &monitor, tx, &finalised, &app_weak);
    }
    Ok(())
}

fn spawn_monitor_overlay(
    app: &gtk4::Application,
    monitor: &gdk4::Monitor,
    tx: &Sender,
    finalised: &Rc<Cell<bool>>,
    app_weak: &glib::WeakRef<gtk4::Application>,
) {
    let geo = monitor.geometry();
    let geo_x = geo.x();
    let geo_y = geo.y();

    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .decorated(false)
        .resizable(false)
        .build();

    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.set_monitor(Some(monitor));
    window.set_namespace(Some("hyprsnap-selector"));
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_exclusive_zone(-1);
    window.add_css_class("hyprsnap-selector");

    let area = gtk4::DrawingArea::new();
    area.set_hexpand(true);
    area.set_vexpand(true);

    let selection: Rc<Cell<LiveSelection>> = Rc::new(Cell::new(LiveSelection::default()));

    install_draw(&area, &selection);
    install_drag(&area, &selection, tx, finalised, app_weak, geo_x, geo_y);
    install_keys(&window, &selection, tx, finalised, app_weak, geo_x, geo_y);

    window.set_child(Some(&area));
    window.present();
}

fn install_draw(area: &gtk4::DrawingArea, selection: &Rc<Cell<LiveSelection>>) {
    let selection = selection.clone();
    area.set_draw_func(move |_area, cr, w, h| {
        let (w, h) = (w as f64, h as f64);
        // Whole-surface dim.
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.45);
        cr.rectangle(0.0, 0.0, w, h);
        let _ = cr.fill();

        if let Some((rx, ry, rw, rh)) = selection.get().rect() {
            // Cut out the selection (Clear operator) and stroke a thin highlight.
            cr.set_operator(gtk4::cairo::Operator::Clear);
            cr.rectangle(rx, ry, rw, rh);
            let _ = cr.fill();
            cr.set_operator(gtk4::cairo::Operator::Over);

            cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
            cr.set_line_width(1.5);
            cr.rectangle(rx + 0.5, ry + 0.5, rw - 1.0, rh - 1.0);
            let _ = cr.stroke();
        }
    });
}

fn install_drag(
    area: &gtk4::DrawingArea,
    selection: &Rc<Cell<LiveSelection>>,
    tx: &Sender,
    finalised: &Rc<Cell<bool>>,
    app_weak: &glib::WeakRef<gtk4::Application>,
    geo_x: i32,
    geo_y: i32,
) {
    let drag = gtk4::GestureDrag::new();

    {
        let selection = selection.clone();
        let area_weak = area.downgrade();
        drag.connect_drag_begin(move |_, x, y| {
            selection.set(LiveSelection {
                start: Some((x, y)),
                current: Some((x, y)),
            });
            if let Some(a) = area_weak.upgrade() {
                a.queue_draw();
            }
        });
    }
    {
        let selection = selection.clone();
        let area_weak = area.downgrade();
        drag.connect_drag_update(move |g, dx, dy| {
            if let Some((sx, sy)) = g.start_point() {
                let mut s = selection.get();
                s.current = Some((sx + dx, sy + dy));
                selection.set(s);
                if let Some(a) = area_weak.upgrade() {
                    a.queue_draw();
                }
            }
        });
    }
    {
        let selection = selection.clone();
        let tx = tx.clone();
        let finalised = finalised.clone();
        let app_weak = app_weak.clone();
        drag.connect_drag_end(move |g, dx, dy| {
            if let Some((sx, sy)) = g.start_point() {
                let mut s = selection.get();
                s.current = Some((sx + dx, sy + dy));
                selection.set(s);
            }
            commit(&selection, geo_x, geo_y, &tx, &finalised, &app_weak);
        });
    }
    area.add_controller(drag);
}

fn install_keys(
    window: &gtk4::ApplicationWindow,
    selection: &Rc<Cell<LiveSelection>>,
    tx: &Sender,
    finalised: &Rc<Cell<bool>>,
    app_weak: &glib::WeakRef<gtk4::Application>,
    geo_x: i32,
    geo_y: i32,
) {
    let key = gtk4::EventControllerKey::new();
    let selection = selection.clone();
    let tx = tx.clone();
    let finalised = finalised.clone();
    let app_weak = app_weak.clone();
    key.connect_key_pressed(move |_, key, _, _| match key {
        gdk4::Key::Escape => {
            cancel(&tx, &finalised, &app_weak);
            glib::Propagation::Stop
        }
        gdk4::Key::Return | gdk4::Key::KP_Enter => {
            commit(&selection, geo_x, geo_y, &tx, &finalised, &app_weak);
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    });
    window.add_controller(key);
}

fn commit(
    selection: &Rc<Cell<LiveSelection>>,
    geo_x: i32,
    geo_y: i32,
    tx: &Sender,
    finalised: &Rc<Cell<bool>>,
    app_weak: &glib::WeakRef<gtk4::Application>,
) {
    if finalised.get() {
        return;
    }
    let Some((x, y, w, h)) = selection.get().rect() else {
        // Empty drag — keep the overlay open, await another attempt.
        return;
    };
    finalised.set(true);
    let rect = Rect {
        x: geo_x + x.round() as i32,
        y: geo_y + y.round() as i32,
        w: w.round() as u32,
        h: h.round() as u32,
    };
    send_once(tx, Ok(rect));
    if let Some(app) = app_weak.upgrade() {
        app.quit();
    }
}

fn cancel(tx: &Sender, finalised: &Rc<Cell<bool>>, app_weak: &glib::WeakRef<gtk4::Application>) {
    if finalised.replace(true) {
        return;
    }
    send_once(tx, Err(anyhow!("selection cancelled")));
    if let Some(app) = app_weak.upgrade() {
        app.quit();
    }
}
