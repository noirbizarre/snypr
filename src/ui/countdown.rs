//! Pre-capture countdown overlay for non-interactive screenshot paths.
//!
//! When `--full --delay 3s` (or any other selector-less invocation) is used, there is no
//! `SelectorOverlay` to host the countdown — the selector is short-circuited and the
//! capture would otherwise happen behind an unannounced `tokio::time::sleep`. This module
//! spawns a small GTK application that mirrors the selector's countdown rendering: one
//! fullscreen `gtk4_layer_shell` window per monitor at `Layer::Overlay`, with a
//! translucent dim and a huge centered seconds-remaining numeral. The application quits
//! itself when the countdown reaches zero, returning control to the async caller.
//!
//! Interactive selector paths handle their own countdown inside [`super::selector::commit`].

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use gtk4::glib;
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

/// Display a fullscreen pre-capture countdown across every monitor for `duration`, then
/// return. A zero duration is a no-op.
///
/// Spawns a private `gtk4::Application` on a `spawn_blocking` thread because GTK is not
/// `Send` and cannot run on a tokio worker. The function resolves once the timer hits
/// zero (or immediately on any setup error).
pub async fn show_countdown(duration: Duration) -> Result<()> {
    if duration.is_zero() {
        return Ok(());
    }
    tokio::task::spawn_blocking(move || run_gtk(duration))
        .await
        .map_err(|e| anyhow!("countdown task panicked: {e}"))?
}

fn run_gtk(duration: Duration) -> Result<()> {
    let app = gtk4::Application::builder()
        .application_id(crate::ui::APP_ID)
        .build();

    let setup_error: Arc<Mutex<Option<anyhow::Error>>> = Arc::new(Mutex::new(None));
    {
        let setup_error = setup_error.clone();
        app.connect_activate(move |app| {
            crate::ui::install_icon_resources();
            crate::ui::style::install();
            if let Err(err) = build_overlays(app, duration) {
                *setup_error.lock().unwrap() = Some(err);
                app.quit();
            }
        });
    }

    let exit = app.run_with_args::<&str>(&[]);
    let code: i32 = exit.into();
    if let Some(err) = setup_error.lock().unwrap().take() {
        return Err(err);
    }
    if code != 0 {
        bail!("GTK exited with status {code}");
    }
    Ok(())
}

fn build_overlays(app: &gtk4::Application, duration: Duration) -> Result<()> {
    let display = gdk4::Display::default().ok_or_else(|| anyhow!("no GDK display available"))?;
    let monitors_list = display.monitors();
    let n = monitors_list.n_items();
    if n == 0 {
        bail!("no monitors reported by GDK");
    }

    // Whole-seconds resolution matches the selector's countdown — sub-second delays are
    // valid at the CLI / config layer but UI countdowns are not the place to surface them.
    let total_secs = duration.as_secs().min(u32::MAX as u64) as u32;

    let labels: Rc<RefCell<Vec<gtk4::Label>>> = Rc::new(RefCell::new(Vec::new()));
    let windows: Rc<RefCell<Vec<gtk4::ApplicationWindow>>> = Rc::new(RefCell::new(Vec::new()));

    for i in 0..n {
        let Some(obj) = monitors_list.item(i) else {
            continue;
        };
        let Ok(monitor) = obj.downcast::<gdk4::Monitor>() else {
            continue;
        };
        let geom = monitor.geometry();
        let mon_h = geom.height().max(1);
        let mon_w = geom.width().max(1);

        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .decorated(false)
            .resizable(false)
            .icon_name(crate::ui::APP_ID)
            .build();
        window.add_css_class("hyprsnap-countdown");

        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_namespace(Some("hyprsnap-countdown"));
        window.set_monitor(Some(&monitor));
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(edge, true);
        }
        window.set_exclusive_zone(-1);
        // No keyboard focus — non-interactive countdown windows expose no shortcuts and
        // should not steal input from whatever the user happens to be doing while waiting.
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_default_size(mon_w, mon_h);

        // Numeral sized roughly to a quarter of the shorter monitor dimension. Falls
        // back to 96 pt so single-monitor laptops still get a legible figure even on a
        // tiny window report.
        let pt = (mon_h.min(mon_w) / 4).max(96);
        let label = gtk4::Label::new(None);
        label.add_css_class("hyprsnap-countdown-number");
        label.set_halign(gtk4::Align::Center);
        label.set_valign(gtk4::Align::Center);
        label.set_hexpand(true);
        label.set_vexpand(true);
        set_label_value(&label, total_secs, pt);
        window.set_child(Some(&label));
        window.present();

        windows.borrow_mut().push(window);
        labels.borrow_mut().push(label);
    }

    // 1 Hz tick. At zero we drop every window (which unmaps the layer-shell surface) and
    // call `app.quit()`, returning control to `run_gtk`.
    let remaining = Rc::new(Cell::new(total_secs));
    let labels_for_tick = labels.clone();
    let windows_for_tick = windows.clone();
    let app_weak = app.downgrade();
    glib::timeout_add_local(Duration::from_secs(1), move || {
        let next = remaining.get().saturating_sub(1);
        remaining.set(next);
        if next == 0 {
            for w in windows_for_tick.borrow_mut().drain(..) {
                w.close();
            }
            if let Some(app) = app_weak.upgrade() {
                app.quit();
            }
            return glib::ControlFlow::Break;
        }
        for l in labels_for_tick.borrow().iter() {
            // Per-monitor `pt` was sized at construction; reuse the same scale on update
            // by inspecting the label's current allocation via `compute_bounds`. This
            // avoids re-querying GdkMonitor on every tick.
            let bounds = l
                .compute_bounds(l)
                .map(|r| (r.width() as i32, r.height() as i32))
                .unwrap_or((1, 1));
            let pt = (bounds.0.min(bounds.1) / 4).max(96);
            set_label_value(l, next, pt);
        }
        glib::ControlFlow::Continue
    });

    Ok(())
}

/// Update the label's text and font size in one call. Uses `set_markup` so the font
/// size can be varied freely without installing per-window CSS.
fn set_label_value(label: &gtk4::Label, secs: u32, pt: i32) {
    label.set_markup(&format!("<span font_desc=\"Sans Bold {pt}\">{secs}</span>"));
}
