//! Annotation editor window.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context as _, Result, anyhow, bail};
use gtk4::prelude::*;

use crate::annotate::{Document, DocumentBase, ToolKind};
use crate::cli::SinkSpec;
use crate::config::{Config, FilenameContext};
use crate::context::Ctx;
use crate::ui::canvas::AnnotationCanvas;

/// Open the editor standalone (from `hyprsnap annotate <image>`).
pub async fn run_standalone(ctx: Ctx, image: PathBuf, sinks: Vec<SinkSpec>) -> Result<()> {
    let bytes = tokio::fs::read(&image)
        .await
        .with_context(|| format!("reading {}", image.display()))?;
    let decoded = image::load_from_memory(&bytes)
        .with_context(|| format!("decoding {}", image.display()))?
        .to_rgba8();
    let (w, h) = decoded.dimensions();
    let base = DocumentBase {
        pixels: std::sync::Arc::from(decoded.into_raw().into_boxed_slice()),
        width: w,
        height: h,
        stride: w * 4,
    };

    // The save destination is resolved up-front so the GTK thread doesn't need to know about
    // XDG dirs or template expansion.
    let save_path = resolve_save_path(&ctx.config, &sinks, &image)?;

    let setup = EditorSetup { base, save_path };
    tokio::task::spawn_blocking(move || run_gtk(setup))
        .await
        .map_err(|e| anyhow::anyhow!("editor task panicked: {e}"))??;
    Ok(())
}

/// Compute where `Ctrl+S` writes the edited image. CLI `--to file=PATH` wins; otherwise the
/// configured save directory plus a filename template with `{selection} = "annotated"` is used;
/// otherwise we save next to the source image as `<stem>-annotated.<ext>`.
fn resolve_save_path(
    config: &Config,
    sinks: &[SinkSpec],
    source: &std::path::Path,
) -> Result<PathBuf> {
    for spec in sinks {
        if let SinkSpec::File(Some(p)) = spec {
            return Ok(p.clone());
        }
    }
    if !sinks.is_empty() {
        let dir = config.save_directory();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        return Ok(dir.join(config.expand_filename(&FilenameContext {
            output: None,
            selection: Some("annotated"),
        })));
    }
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("hyprsnap");
    let ext = source.extension().and_then(|s| s.to_str()).unwrap_or("png");
    let parent = source.parent().unwrap_or_else(|| std::path::Path::new("."));
    Ok(parent.join(format!("{stem}-annotated.{ext}")))
}

struct EditorSetup {
    base: DocumentBase,
    save_path: PathBuf,
}

fn run_gtk(setup: EditorSetup) -> Result<()> {
    let app = gtk4::Application::builder()
        .application_id(crate::ui::APP_ID)
        .build();

    // Wrapped in a Mutex<Option<_>> because `connect_activate` is `Fn`, not `FnOnce`, so we need
    // interior mutability to move the setup into the first activation only.
    let setup = Mutex::new(Some(setup));
    app.connect_activate(move |app| {
        let Some(setup) = setup.lock().unwrap().take() else {
            return;
        };
        crate::ui::style::install();
        build_window(app, setup);
    });

    let exit = app.run_with_args::<&str>(&[]);
    let code: i32 = exit.into();
    if code != 0 {
        bail!("GTK exited with status {code}");
    }
    Ok(())
}

fn build_window(app: &gtk4::Application, setup: EditorSetup) {
    let canvas = AnnotationCanvas::new();
    canvas.set_document(Document::with_base(setup.base));
    canvas.set_tool(ToolKind::Rect);

    // Toolbar with tool selection, undo, and save buttons.
    let toolbar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    toolbar.add_css_class("hyprsnap-toolbar");

    let rect_btn = gtk4::ToggleButton::builder()
        .label("Rect")
        .active(true)
        .build();
    let arrow_btn = gtk4::ToggleButton::with_label("Arrow");
    arrow_btn.set_group(Some(&rect_btn));
    let undo_btn = gtk4::Button::with_label("Undo");
    let save_btn = gtk4::Button::with_label("Save");

    {
        let canvas = canvas.clone();
        rect_btn.connect_toggled(move |b| {
            if b.is_active() {
                canvas.set_tool(ToolKind::Rect);
            }
        });
    }
    {
        let canvas = canvas.clone();
        arrow_btn.connect_toggled(move |b| {
            if b.is_active() {
                canvas.set_tool(ToolKind::Arrow);
            }
        });
    }
    {
        let canvas = canvas.clone();
        undo_btn.connect_clicked(move |_| {
            canvas.undo();
        });
    }
    let save_path = setup.save_path.clone();
    {
        let canvas = canvas.clone();
        let save_path = save_path.clone();
        save_btn.connect_clicked(move |_| {
            if let Err(err) = save_canvas(&canvas, &save_path) {
                tracing::error!(error = ?err, path = %save_path.display(), "save failed");
            }
        });
    }

    toolbar.append(&rect_btn);
    toolbar.append(&arrow_btn);
    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    toolbar.append(&spacer);
    toolbar.append(&undo_btn);
    toolbar.append(&save_btn);

    let scroller = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Automatic)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .vexpand(true)
        .child(&canvas)
        .build();

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.append(&toolbar);
    root.append(&scroller);

    let title = format!("HyprSnap — annotate ({})", save_path.display());
    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .title(title)
        .default_width(1100)
        .default_height(750)
        .child(&root)
        .build();

    install_shortcuts(&window, &canvas, &save_path);
    window.present();
    canvas.grab_focus();
}

/// Keyboard shortcuts: Ctrl+S save, Ctrl+Z undo, R/A switch tool, Esc closes the window.
fn install_shortcuts(
    window: &gtk4::ApplicationWindow,
    canvas: &AnnotationCanvas,
    save_path: &std::path::Path,
) {
    let controller = gtk4::EventControllerKey::new();
    let canvas = canvas.clone();
    let window_weak = window.downgrade();
    let save_path = save_path.to_path_buf();
    controller.connect_key_pressed(move |_, key, _, state| {
        let ctrl = state.contains(gdk4::ModifierType::CONTROL_MASK);
        match (ctrl, key) {
            (true, gdk4::Key::s) => {
                if let Err(err) = save_canvas(&canvas, &save_path) {
                    tracing::error!(error = ?err, "save failed");
                }
                glib::Propagation::Stop
            }
            (true, gdk4::Key::z) => {
                canvas.undo();
                glib::Propagation::Stop
            }
            (false, gdk4::Key::r | gdk4::Key::R) => {
                canvas.set_tool(ToolKind::Rect);
                glib::Propagation::Stop
            }
            (false, gdk4::Key::a | gdk4::Key::A) => {
                canvas.set_tool(ToolKind::Arrow);
                glib::Propagation::Stop
            }
            (false, gdk4::Key::Escape) => {
                if let Some(w) = window_weak.upgrade() {
                    w.close();
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    window.add_controller(controller);
}

fn save_canvas(canvas: &AnnotationCanvas, path: &std::path::Path) -> Result<()> {
    let png = canvas
        .compose_png()
        .map_err(|e| anyhow!("composing PNG: {e}"))?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, &png).with_context(|| format!("writing {}", path.display()))?;
    tracing::info!(path = %path.display(), bytes = png.len(), "saved annotated PNG");
    println!("{}", path.display());
    Ok(())
}
