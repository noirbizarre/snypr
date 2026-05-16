//! `daemon` subcommand — long-lived IPC server.

use anyhow::{Context as _, Result};
use clap::Args as ClapArgs;

use crate::config::Config;
use crate::context::Context;

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Override the socket path. Defaults to `$XDG_RUNTIME_DIR/hyprsnap.sock`.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<std::path::PathBuf>,
}

pub async fn run(args: Args) -> Result<()> {
    let config = Config::load_default().context("loading configuration")?;
    let ctx = Context::new(config).await?;
    let path = args
        .socket
        .unwrap_or_else(crate::daemon::default_socket_path);
    crate::daemon::serve(ctx, path).await
}
