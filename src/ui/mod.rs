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

use anyhow::{Result, bail};
use gtk4::prelude::*;

use crate::i18n::fl;

/// Enumerate the connected monitors, or fail with a translated message.
///
/// Shared by the overlay, selector and countdown surfaces: all three open one layer-shell
/// window per monitor and all three had a byte-identical copy of this prologue, which is how
/// the messages drifted out of the Fluent catalog in the first place.
pub(crate) fn monitors() -> Result<(gtk4::gio::ListModel, u32)> {
    let display =
        gdk4::Display::default().ok_or_else(|| anyhow::anyhow!(fl!("error-no-display")))?;
    let list = display.monitors();
    let n = list.n_items();
    if n == 0 {
        bail!("{}", fl!("error-no-monitors"));
    }
    Ok((list, n))
}

/// Translate a GTK exit status into a `Result`.
///
/// A non-zero status means GTK itself failed rather than the user quitting, which every
/// surface reports the same way.
pub(crate) fn check_gtk_exit(code: i32) -> Result<()> {
    if code != 0 {
        bail!("{}", fl!("error-gtk-exit", code = code));
    }
    Ok(())
}

/// Application id used across the GTK windows and the StatusNotifierItem.
pub const APP_ID: &str = "noirbizar.re.Snypr";

static ICON_RESOURCES: OnceLock<()> = OnceLock::new();

/// Register the bundled gresource and extend the default `IconTheme`'s search path so
/// `Image::from_icon_name("foo-symbolic")` resolves our vendored SVGs in addition to the
/// system theme. Safe to call from every GTK activation; the gresource registration
/// itself happens at most once per process.
pub(crate) fn install_icon_resources() {
    ICON_RESOURCES.get_or_init(|| {
        gtk4::gio::resources_register_include!("snypr.gresource")
            .expect("bundled snypr.gresource should always be registerable");
    });

    if let Some(display) = gdk4::Display::default() {
        gtk4::IconTheme::for_display(&display).add_resource_path("/re/noirbizar/Snypr/icons");
    }
}
