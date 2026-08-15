//! Desktop notifications for screenshot success and fatal errors.
//!
//! Notifications go out over the freedesktop Desktop Notifications spec via `notify-rust`,
//! which talks D-Bus internally. All emission is best-effort: failures (no daemon, no bus,
//! malformed reply…) are logged at `debug!` and otherwise swallowed so we never derail the
//! caller's happy path.
//!
//! `notify-rust`'s `.show()` blocks on a D-Bus call internally, which panics if invoked
//! from a tokio runtime thread ("Cannot start a runtime from within a runtime"). To stay
//! agnostic of the caller's context, we dispatch every notification on a detached OS
//! thread; the callers don't await delivery (it's best-effort) so fire-and-forget is fine.

use std::path::{Path, PathBuf};

use notify_rust::Notification;

use crate::config::Config;
use crate::config::NotifyConfig;
use crate::path::tilde;

// The notification daemon distinguishes between the human-facing application name
// (`appname`, surfaced in some notification UIs as a group label) and the desktop-file
// / theme application id (used to resolve the icon). We deliberately use a lowercase
// bare name as the appname so the grouping label reads cleanly, and reuse
// `crate::ui::APP_ID` for the icon so the bundled `noirbizar.re.Snypr` icon resolves.
const APP_NAME: &str = "snypr";
#[cfg(feature = "ui")]
const APP_ICON: &str = crate::ui::APP_ID;
#[cfg(not(feature = "ui"))]
const APP_ICON: &str = "noirbizar.re.Snypr";
const SUMMARY: &str = "Snypr";

/// Emit a desktop notification for a fatal error so the user sees something when snypr
/// was launched from a Hyprland keybind (where stderr is detached).
pub fn notify_error(cfg: &NotifyConfig, err: &anyhow::Error) {
    if !cfg.error {
        return;
    }
    let mut builder = Notification::new();
    builder
        .summary(SUMMARY)
        .body(&format!("{err:#}"))
        .icon(APP_ICON)
        .appname(APP_NAME)
        .timeout(notify_rust::Timeout::Milliseconds(cfg.timeout_ms));
    dispatch(builder);
}

/// Emit a success notification for a freshly written screenshot, attaching the screenshot
/// itself as the notification's preview image via the standard `image-path` hint.
///
/// * `paths` — files written to disk by the output sinks (empty for clipboard-only runs).
/// * `png_bytes` — the encoded PNG, used as a thumbnail fallback when `paths` is empty
///   (written to a stable scratch path so the previous one is implicitly recycled).
pub fn notify_success(cfg: &Config, paths: &[PathBuf], png_bytes: &[u8]) {
    if !cfg.notify.success {
        return;
    }

    let default_dir = cfg.save_directory();
    let (summary, body): (String, Option<String>) = match paths.len() {
        0 => (crate::i18n::fl!("notify-copied"), None),
        1 => (
            crate::i18n::fl!("notify-saved-single"),
            Some(display_path(&paths[0], &default_dir)),
        ),
        n => (
            crate::i18n::fl!("notify-saved-multi"),
            Some(crate::i18n::fl!(
                "notify-saved-multi-body",
                first = display_path(&paths[0], &default_dir),
                count = n
            )),
        ),
    };

    // image-path expects a real file. For clipboard-only runs we materialise one in a
    // deterministic scratch location so it gets overwritten on the next call (no cleanup
    // book-keeping required).
    let thumbnail = if let Some(p) = paths.first() {
        Some(p.clone())
    } else {
        match write_scratch_thumbnail(png_bytes) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::debug!(error = ?e, "failed to stage notification thumbnail");
                None
            }
        }
    };

    let mut builder = Notification::new();
    builder
        .summary(&summary)
        .icon(APP_ICON)
        .appname(APP_NAME)
        .timeout(notify_rust::Timeout::Milliseconds(cfg.notify.timeout_ms));
    if let Some(body) = body.as_deref() {
        builder.body(body);
    }
    if let Some(thumb) = thumbnail.as_deref().and_then(Path::to_str) {
        builder.image_path(thumb);
    }
    dispatch(builder);
}

/// Format a saved screenshot path for display in a notification body.
///
/// When the file sits directly in `default_dir` (the configured/derived save directory),
/// only the basename is shown so the notification stays short. Otherwise the full path
/// is rendered with `$HOME` collapsed to `~`.
fn display_path(path: &Path, default_dir: &Path) -> String {
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && parent == default_dir
    {
        return name.to_string_lossy().into_owned();
    }
    tilde(path)
}

/// Send the prepared `Notification` off-thread so the D-Bus round-trip inside
/// `notify-rust` never blocks a tokio runtime worker.
fn dispatch(builder: Notification) {
    let spawned = std::thread::Builder::new()
        .name("snypr-notify".to_owned())
        .spawn(move || {
            if let Err(e) = builder.show() {
                tracing::debug!(error = ?e, "failed to emit desktop notification");
            }
        });
    if let Err(e) = spawned {
        tracing::debug!(error = ?e, "failed to spawn notification thread");
    }
}

/// Stable per-user scratch path for the clipboard-thumbnail fallback. Prefers
/// `$XDG_RUNTIME_DIR` (tmpfs, wiped on logout); falls back to `std::env::temp_dir()`.
fn scratch_thumbnail_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("snypr").join("last-thumbnail.png")
}

fn write_scratch_thumbnail(png_bytes: &[u8]) -> std::io::Result<PathBuf> {
    let path = scratch_thumbnail_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, png_bytes)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_path_ends_with_expected_filename() {
        let p = scratch_thumbnail_path();
        assert!(p.ends_with("snypr/last-thumbnail.png"));
    }

    #[test]
    fn display_path_shortens_default_dir_to_basename() {
        let default = Path::new("/home/u/Pictures/Screenshots");
        assert_eq!(
            display_path(
                &PathBuf::from("/home/u/Pictures/Screenshots/shot.png"),
                default,
            ),
            "shot.png"
        );
    }

    #[test]
    fn display_path_falls_back_to_tilde_outside_default_dir() {
        // SAFETY: tests are single-threaded under cargo nextest's per-process model.
        unsafe {
            std::env::set_var("HOME", "/home/u");
        }
        let default = Path::new("/home/u/Pictures/Screenshots");
        // Sibling directory under $HOME → tilde-collapsed full path.
        assert_eq!(
            display_path(&PathBuf::from("/home/u/Other/shot.png"), default),
            "~/Other/shot.png"
        );
        // Nested directory under the default dir → keeps relative path (subdir + name).
        assert_eq!(
            display_path(
                &PathBuf::from("/home/u/Pictures/Screenshots/sub/shot.png"),
                default,
            ),
            "~/Pictures/Screenshots/sub/shot.png"
        );
    }

    #[test]
    fn display_path_handles_paths_without_a_parent_or_name() {
        // SAFETY: nextest runs each test in its own process.
        unsafe {
            std::env::set_var("HOME", "/home/u");
        }
        let default = Path::new("/home/u/Pictures/Screenshots");
        // The root has no file name, and a bare filename has an empty parent: both must fall
        // through to the tilde rendering rather than panicking or returning an empty string.
        assert_eq!(display_path(Path::new("/"), default), "/");
        assert_eq!(display_path(Path::new("shot.png"), default), "shot.png");
    }
}
