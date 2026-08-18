//! GTK4 user interface entry points.
//!
//! These modules are gated behind the `ui` cargo feature.

pub mod canvas;
pub mod countdown;
pub mod overlay;
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

/// Test-only GTK bootstrap.
///
/// Widget-level tests need a `GdkDisplay`, which means a running compositor. CI provides a
/// headless one; a developer's machine usually has a real session. When neither is present
/// the caller skips, so `cargo test` still works over SSH or in a bare container.
///
/// Set `SNYPR_REQUIRE_GTK` to a truthy value (`1`, `true`, `yes`) to turn "no display" into a
/// hard failure instead — CI does this so a broken compositor step surfaces as a red build
/// rather than as silently skipped tests. `0`, `false`, `no` and the empty string disable the
/// requirement, so `SNYPR_REQUIRE_GTK=0` can be used to opt out locally without unsetting it.
#[cfg(test)]
pub(crate) fn try_init_gtk() -> bool {
    // `gtk4::init` is idempotent, but nextest runs each test in its own process anyway, so
    // this is a single init per test.
    if gtk4::init().is_ok() {
        install_icon_resources();
        return true;
    }
    assert!(
        !gtk_is_required(),
        "SNYPR_REQUIRE_GTK is set but GTK could not connect to a display; \
         the headless compositor is not running"
    );
    eprintln!("no Wayland display available, skipping GTK-backed test");
    false
}

/// Whether `SNYPR_REQUIRE_GTK` demands a working display.
///
/// Gated on the *value*, not mere presence: an exported-but-empty or explicitly disabled
/// variable used to hard-fail, which made `SNYPR_REQUIRE_GTK=0` mean the opposite of what it
/// reads.
#[cfg(test)]
pub(crate) fn gtk_is_required() -> bool {
    match std::env::var("SNYPR_REQUIRE_GTK") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no"
        ),
        Err(_) => false,
    }
}

/// Skip the enclosing test when no display is available. See [`try_init_gtk`].
///
/// Expands to a bare `return`, so it only works in tests returning `()`. A test that returns
/// `Result` must call [`try_init_gtk`] directly.
#[cfg(test)]
macro_rules! require_gtk {
    () => {
        if !$crate::ui::try_init_gtk() {
            return;
        }
    };
}
#[cfg(test)]
pub(crate) use require_gtk;

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::clean_quit(0)]
    fn check_gtk_exit_accepts_a_clean_quit(#[case] code: i32) {
        assert!(check_gtk_exit(code).is_ok());
    }

    #[rstest]
    #[case::generic_failure(1)]
    #[case::signal(139)]
    #[case::negative(-1)]
    fn check_gtk_exit_reports_a_non_zero_status(#[case] code: i32) {
        let err = check_gtk_exit(code).unwrap_err();
        // The status must survive into the message: it is the only diagnostic the user gets
        // when GTK dies without the surface having a chance to report anything itself.
        assert!(
            format!("{err}").contains(&code.to_string()),
            "status {code} missing from {err}"
        );
    }
}
