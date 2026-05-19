//! GTK4 user interface entry points.
//!
//! These modules are gated behind the `ui` cargo feature.

pub mod canvas;
pub mod countdown;
pub mod overlay;
pub mod save;
pub mod selector;
pub mod style;
pub mod toolbar;
#[cfg(feature = "tray")]
pub mod tray;

pub use toolbar::{ModeKind, Toolbar, ToolbarAction, ToolbarSpec};

use std::sync::OnceLock;

/// Application id used across the GTK windows and the StatusNotifierItem.
pub const APP_ID: &str = "noirbizar.re.HyprSnap";

static ICON_RESOURCES: OnceLock<()> = OnceLock::new();

/// Register the bundled gresource and extend the default `IconTheme`'s search path so
/// `Image::from_icon_name("foo-symbolic")` resolves our vendored SVGs in addition to the
/// system theme. Safe to call from every GTK activation; the gresource registration
/// itself happens at most once per process.
pub(crate) fn install_icon_resources() {
    ICON_RESOURCES.get_or_init(|| {
        gtk4::gio::resources_register_include!("hyprsnap.gresource")
            .expect("bundled hyprsnap.gresource should always be registerable");
    });

    if let Some(display) = gdk4::Display::default() {
        gtk4::IconTheme::for_display(&display).add_resource_path("/re/noirbizar/HyprSnap/icons");
    }
}
