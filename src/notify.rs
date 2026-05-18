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

use crate::config::NotifyConfig;

const APP_ID: &str = "hyprsnap";
const APP_ICON: &str = "noirbizar.re.HyprSnap";
const SUMMARY: &str = "HyprSnap";

/// Emit a desktop notification for a fatal error so the user sees something when hyprsnap
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
        .appname(APP_ID)
        .timeout(notify_rust::Timeout::Milliseconds(cfg.timeout_ms));
    dispatch(builder);
}

/// Emit a success notification for a freshly written screenshot, attaching the screenshot
/// itself as the notification's preview image via the standard `image-path` hint.
///
/// * `paths` — files written to disk by the output sinks (empty for clipboard-only runs).
/// * `png_bytes` — the encoded PNG, used as a thumbnail fallback when `paths` is empty
///   (written to a stable scratch path so the previous one is implicitly recycled).
pub fn notify_success(cfg: &NotifyConfig, paths: &[PathBuf], png_bytes: &[u8]) {
    if !cfg.success {
        return;
    }

    let (summary, body): (&str, Option<String>) = match paths.len() {
        0 => ("Screenshot copied to clipboard", None),
        1 => ("Screenshot saved", Some(paths[0].display().to_string())),
        n => (
            "Screenshots saved",
            Some(format!("{} ({n} files)", paths[0].display())),
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
        .summary(summary)
        .icon(APP_ICON)
        .appname(APP_ID)
        .timeout(notify_rust::Timeout::Milliseconds(cfg.timeout_ms));
    if let Some(body) = body.as_deref() {
        builder.body(body);
    }
    if let Some(thumb) = thumbnail.as_deref().and_then(Path::to_str) {
        builder.image_path(thumb);
    }
    dispatch(builder);
}

/// Send the prepared `Notification` off-thread so the D-Bus round-trip inside
/// `notify-rust` never blocks a tokio runtime worker.
fn dispatch(builder: Notification) {
    let spawned = std::thread::Builder::new()
        .name("hyprsnap-notify".to_owned())
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
    base.join("hyprsnap").join("last-thumbnail.png")
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
        assert!(p.ends_with("hyprsnap/last-thumbnail.png"));
    }
}
