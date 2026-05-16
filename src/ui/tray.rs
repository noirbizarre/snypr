//! System tray icon (StatusNotifierItem) via `ksni`.
//!
//! Wired in plan step 15; this module currently registers a no-op item under a feature gate.

use anyhow::Result;

pub struct TrayHandle;

pub fn spawn() -> Result<TrayHandle> {
    anyhow::bail!("tray integration not yet implemented (planned for plan step 15)")
}
