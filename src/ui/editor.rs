//! Annotation editor window.
//!
//! Two entry points share the same GTK plumbing:
//!
//! * [`run_standalone`] — `hyprsnap annotate <file>`: loads a PNG from disk and writes back to
//!   the resolved save path on Ctrl+S.
//! * [`run_with_base`] — `hyprsnap capture`: receives an in-memory `DocumentBase` from the
//!   capture pipeline (no PNG round-trip) and dispatches to the configured sinks on save.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow, bail};
use gtk4::prelude::*;

use crate::annotate::{Document, DocumentBase, ToolKind};
use crate::capture::CapturedImage;
use crate::cli::SinkSpec;
use crate::config::{Config, FilenameContext};
use crate::context::Ctx;
use crate::output::Outputs;
use crate::ui::canvas::AnnotationCanvas;
use crate::ui::toolbar::{EDITOR_TOOLS, Toolbar, ToolbarAction, ToolbarSpec};

/// Save action invoked when the user hits Ctrl+S / clicks Save. Returns the paths that were
/// written (clipboard sinks return none) so they can be echoed on stdout.
type SaveFn = Arc<dyn Fn(&CapturedImage) -> Result<Vec<PathBuf>> + Send + Sync + 'static>;

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
    let title = format!("HyprSnap — annotate ({})", save_path.display());
    let save = path_save_fn(save_path, ctx.config.output.compression);

    let setup = EditorSetup { base, title, save };
    tokio::task::spawn_blocking(move || run_gtk(setup))
        .await
        .map_err(|e| anyhow::anyhow!("editor task panicked: {e}"))??;
    Ok(())
}

/// Open the editor against an already-captured image and route Ctrl+S through the configured
/// sinks. Used by the `capture` subcommand so the base buffer never has to round-trip through
/// PNG. Returns the paths written during the editor session (empty if the user closed without
/// saving, multiple entries if they saved more than once). Used by the daemon's
/// Capture-over-IPC handler to ferry the result back to the client.
pub async fn run_with_base(
    ctx: Ctx,
    base: DocumentBase,
    sinks: Vec<SinkSpec>,
) -> Result<Vec<PathBuf>> {
    let title = "HyprSnap — capture".to_owned();
    let written: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let save = sinks_save_fn(ctx.config.clone(), sinks, written.clone());
    let setup = EditorSetup { base, title, save };
    tokio::task::spawn_blocking(move || run_gtk(setup))
        .await
        .map_err(|e| anyhow::anyhow!("editor task panicked: {e}"))??;
    // GTK loop has exited; nobody else holds the Mutex.
    Ok(std::mem::take(&mut written.lock().unwrap()))
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

/// Build a synchronous save closure that writes the composed PNG to a fixed path.
fn path_save_fn(path: PathBuf, compression: crate::config::PngCompression) -> SaveFn {
    Arc::new(move |img: &CapturedImage| {
        let png = crate::output::encode_png(img, compression)?;
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, &png).with_context(|| format!("writing {}", path.display()))?;
        tracing::info!(path = %path.display(), bytes = png.len(), "saved annotated PNG");
        Ok(vec![path.clone()])
    })
}

/// Build a save closure that routes the composed PNG through the configured `Outputs` sinks
/// and records the written paths into `collected` so the caller can return them over IPC.
/// We capture the tokio runtime handle so the GTK thread (inside `spawn_blocking`) can
/// `block_on` the async clipboard/file writes without spinning up a second runtime.
fn sinks_save_fn(
    config: Config,
    sinks: Vec<SinkSpec>,
    collected: Arc<Mutex<Vec<PathBuf>>>,
) -> SaveFn {
    let handle = tokio::runtime::Handle::current();
    let sinks = if sinks.is_empty() {
        config.default_sinks()
    } else {
        sinks
    };
    Arc::new(move |img: &CapturedImage| {
        let png = crate::output::encode_png(img, config.output.compression)?;
        let ctx = FilenameContext {
            output: img.source.as_ref().map(|o| o.name.as_str()),
            selection: Some("capture"),
        };
        let outputs = Outputs::from_specs(&sinks, &config, &ctx)?;
        let paths = handle.block_on(outputs.write_png(&png))?;
        tracing::info!(
            bytes = png.len(),
            paths = ?paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "saved capture"
        );
        if let Ok(mut g) = collected.lock() {
            g.extend(paths.iter().cloned());
        }
        Ok(paths)
    })
}

struct EditorSetup {
    base: DocumentBase,
    title: String,
    save: SaveFn,
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

    let toolbar = Toolbar::new(ToolbarSpec {
        tools: EDITOR_TOOLS,
        show_undo: true,
        show_save: true,
        initial_tool: Some(ToolKind::Rect),
        ..Default::default()
    });
    {
        let canvas = canvas.clone();
        let save = setup.save.clone();
        toolbar.connect(move |action| match action {
            ToolbarAction::ToolSelected(kind) => canvas.set_tool(kind),
            ToolbarAction::Undo => {
                canvas.undo();
            }
            ToolbarAction::Save => {
                if let Err(err) = save_canvas(&canvas, save.as_ref()) {
                    tracing::error!(error = ?err, "save failed");
                }
            }
            _ => {}
        });
    }

    let scroller = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Automatic)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .vexpand(true)
        .child(&canvas)
        .build();

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.append(toolbar.widget());
    root.append(&scroller);

    let window = gtk4::ApplicationWindow::builder()
        .application(app)
        .title(setup.title)
        .default_width(1100)
        .default_height(750)
        .child(&root)
        .build();

    install_shortcuts(&window, &toolbar);
    toolbar.install_shortcuts(&window);
    window.present();
    canvas.grab_focus();
}

/// Window-level shortcuts that aren't covered by the Toolbar (currently just Esc to close).
fn install_shortcuts(window: &gtk4::ApplicationWindow, _toolbar: &Toolbar) {
    let controller = gtk4::EventControllerKey::new();
    let window_weak = window.downgrade();
    controller.connect_key_pressed(move |_, key, _, _| match key {
        gdk4::Key::Escape => {
            if let Some(w) = window_weak.upgrade() {
                w.close();
            }
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    });
    window.add_controller(controller);
}

fn save_canvas(
    canvas: &AnnotationCanvas,
    save: &dyn Fn(&CapturedImage) -> Result<Vec<PathBuf>>,
) -> Result<()> {
    let img = canvas.compose().map_err(|e| anyhow!("composing: {e}"))?;
    let paths = save(&img)?;
    for p in &paths {
        println!("{}", p.display());
    }
    Ok(())
}
