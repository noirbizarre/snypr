//! Shared, cheaply-cloneable application context.

use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;

/// Long-lived application state shared across commands.
pub struct Context {
    pub config: Config,
}

pub type Ctx = Arc<Context>;

impl Context {
    pub async fn new(config: Config) -> Result<Ctx> {
        Ok(Arc::new(Self { config }))
    }
}
