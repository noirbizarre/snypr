//! Live draw-on-screen overlay (Draw-On-Gnome equivalent).

use anyhow::Result;

use crate::context::Ctx;

pub async fn run(_ctx: Ctx, _passthrough: bool) -> Result<()> {
    anyhow::bail!("live overlay not yet implemented (planned for plan step 12)")
}
