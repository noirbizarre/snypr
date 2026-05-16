//! Interactive region/window selector overlay.
//!
//! Renders a fullscreen layer-shell window per monitor and emits the chosen rectangle through
//! an `async_channel`. The detailed implementation arrives with step 8 of the plan; this stub
//! keeps the module wiring intact.

use anyhow::Result;

use crate::capture::region::Rect;
use crate::context::Ctx;

pub async fn pick_region(_ctx: Ctx) -> Result<Rect> {
    anyhow::bail!("selector overlay not yet implemented (planned for plan step 8)")
}
