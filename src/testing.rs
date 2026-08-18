//! Shared test fixtures.
//!
//! Only compiled for `cfg(test)`. Building a [`Ctx`] requires a `Config` with notifications
//! disabled (otherwise a test run pops desktop toasts, and fails outright on a machine with
//! no D-Bus session), which three separate modules had grown their own copy of.

use crate::config::Config;
use crate::context::{Context, Ctx};

/// A [`Ctx`] with notifications off and the given `[output].default_sinks`.
///
/// Pass an empty slice for the config default.
pub async fn test_ctx_with_sinks(default_sinks: &[&str]) -> Ctx {
    let mut config = Config::default();
    if !default_sinks.is_empty() {
        config.output.default_sinks = default_sinks.iter().map(|s| (*s).to_owned()).collect();
    }
    // Desktop notifications need a live D-Bus session; a test run must never depend on one,
    // and must never spam the developer's desktop either.
    config.notify.success = false;
    config.notify.error = false;
    Context::new(config).await.expect("building a test Context")
}

/// A [`Ctx`] with notifications off and the default sink configuration.
pub async fn test_ctx() -> Ctx {
    test_ctx_with_sinks(&[]).await
}
