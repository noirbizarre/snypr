//! GTK4 user interface entry points.
//!
//! These modules are gated behind the `ui` cargo feature.

pub mod canvas;
pub mod editor;
pub mod overlay;
pub mod selector;
pub mod style;
pub mod toolbar;
#[cfg(feature = "tray")]
pub mod tray;

pub use toolbar::{ModeKind, Toolbar, ToolbarAction, ToolbarSpec};

use std::path::PathBuf;

/// Application id used across the GTK windows and the StatusNotifierItem.
pub const APP_ID: &str = "ai.hyprtools.HyprSnap";

/// Convenience accessor used by binary stubs that don't need the full editor yet.
#[allow(dead_code)]
pub fn placeholder_path() -> Option<PathBuf> {
    None
}
