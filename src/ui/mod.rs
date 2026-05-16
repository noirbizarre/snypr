//! GTK4 user interface entry points.
//!
//! These modules are gated behind the `ui` cargo feature.

pub mod canvas;
pub mod editor;
pub mod overlay;
pub mod selector;
pub mod style;
#[cfg(feature = "tray")]
pub mod tray;

use std::path::PathBuf;

use anyhow::Result;

use crate::cli::SinkSpec;
use crate::context::Ctx;

/// Application id used across the GTK windows and the StatusNotifierItem.
pub const APP_ID: &str = "ai.hyprtools.HyprSnap";

/// Run the `capture` flow: interactive selector → screencopy → editor → sinks.
pub async fn run_capture_flow(_ctx: Ctx, _sinks: Vec<SinkSpec>, _cursor: bool) -> Result<()> {
    anyhow::bail!(
        "`capture` UI flow is not yet wired in this build — open `annotate` on a captured PNG instead"
    )
}

/// Convenience accessor used by binary stubs that don't need the full editor yet.
#[allow(dead_code)]
pub fn placeholder_path() -> Option<PathBuf> {
    None
}
