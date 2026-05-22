//! Shared, cheaply-cloneable application context.

use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;

/// Long-lived application state shared across commands.
pub struct Context {
    pub config: Config,
    /// True when this process is the long-lived `hyprsnap daemon` server.
    ///
    /// Sinks that would otherwise need to fork-and-detach to outlive a
    /// one-shot CLI invocation (currently only [`crate::output::clipboard::ClipboardSink`])
    /// skip the fork in this case because the daemon already provides the
    /// required lifetime — forking from inside the daemon would leak
    /// detached children on every screenshot and race the daemon's own
    /// Wayland selection.
    pub running_as_daemon: bool,
}

pub type Ctx = Arc<Context>;

impl Context {
    /// Build a context for a short-lived (one-shot) CLI invocation.
    pub async fn new(config: Config) -> Result<Ctx> {
        Ok(Arc::new(Self {
            config,
            running_as_daemon: false,
        }))
    }

    /// Build a context owned by the long-lived `hyprsnap daemon` process.
    ///
    /// The only behavioural difference today is that sinks know they are
    /// running inside a persistent process and can skip lifetime workarounds
    /// (see [`Context::running_as_daemon`]).
    pub async fn new_for_daemon(config: Config) -> Result<Ctx> {
        Ok(Arc::new(Self {
            config,
            running_as_daemon: true,
        }))
    }
}
