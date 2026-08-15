//! `daemon` subcommand — long-lived IPC server.

use anyhow::{Context as _, Result};
use clap::Args as ClapArgs;

use crate::config::Config;
use crate::context::Context;

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Override the socket path. Defaults to `$XDG_RUNTIME_DIR/snypr.sock`.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<std::path::PathBuf>,

    /// Expose a StatusNotifierItem (system tray) icon while the daemon is running.
    #[arg(long)]
    pub systray: bool,
}

pub async fn run(args: Args, config_override: Option<&std::path::Path>) -> Result<()> {
    let config = Config::resolve(config_override).context("loading configuration")?;
    let ctx = Context::new_for_daemon(config).await?;
    let path = args
        .socket
        .unwrap_or_else(crate::daemon::default_socket_path);
    crate::daemon::serve(ctx, path, args.systray).await
}
