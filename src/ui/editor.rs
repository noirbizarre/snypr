//! Annotation editor window.

use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use gtk4::prelude::*;

use crate::annotate::{Document, DocumentBase};
use crate::cli::SinkSpec;
use crate::context::Ctx;
use crate::ui::canvas::AnnotationCanvas;

/// Open the editor standalone (from `hyprsnap annotate <image>`).
///
/// This is a minimal implementation: it loads the image into a `Document`, shows the canvas,
/// and exits cleanly when the window is closed. Annotation tooling and save plumbing are
/// scheduled for the post-bootstrap commits in the plan.
pub async fn run_standalone(_ctx: Ctx, image: PathBuf, _sinks: Vec<SinkSpec>) -> Result<()> {
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

    tokio::task::spawn_blocking(move || run_gtk(base))
        .await
        .map_err(|e| anyhow::anyhow!("editor task panicked: {e}"))??;
    Ok(())
}

fn run_gtk(base: DocumentBase) -> Result<()> {
    let app = gtk4::Application::builder()
        .application_id(crate::ui::APP_ID)
        .build();

    let base = std::sync::Mutex::new(Some(base));
    app.connect_activate(move |app| {
        crate::ui::style::install();
        let canvas = AnnotationCanvas::new();
        if let Some(base) = base.lock().unwrap().take() {
            canvas.set_document(Document::with_base(base));
        }
        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .title("HyprSnap — annotate")
            .default_width(900)
            .default_height(600)
            .child(&canvas)
            .build();
        window.present();
    });

    let exit = app.run_with_args::<&str>(&[]);
    let code: i32 = exit.into();
    if code != 0 {
        bail!("GTK exited with status {code}");
    }
    Ok(())
}
