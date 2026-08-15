use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use snypr::cli::{Cli, dispatch};

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Err(err) = init_tracing(cli.verbose) {
        eprintln!("failed to initialise tracing: {err:#}");
        return ExitCode::FAILURE;
    }

    // Resolve the active locale before any user-facing string is emitted.
    // Precedence: `--lang` flag > `[language]` in config > env > English fallback.
    // Config load is best-effort: failures here just fall through to env detection.
    let lang_override = cli.lang.clone().or_else(|| {
        snypr::config::Config::resolve(cli.config.as_deref())
            .ok()
            .and_then(|c| c.language)
    });
    snypr::i18n::init(lang_override.as_deref());

    // Kept out of `cli` because `dispatch` consumes it, and the error path below still
    // needs the override to decide whether error notifications are enabled.
    let cli_config = cli.config.clone();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            tracing::error!(error = ?err, "failed to build tokio runtime");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(dispatch(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if is_user_cancelled(&err) {
                tracing::info!("cancelled by user");
                ExitCode::SUCCESS
            } else {
                tracing::error!(error = ?err, "snypr failed");
                eprintln!("error: {err:#}");
                #[cfg(feature = "notify")]
                {
                    // Load the config best-effort so users can disable error notifications
                    // via `[notify] error = false`. If the config can't be loaded we fall
                    // back to defaults (notifications enabled) — matches prior behaviour.
                    let cfg = snypr::config::Config::resolve(cli_config.as_deref())
                        .unwrap_or_default()
                        .notify;
                    snypr::notify::notify_error(&cfg, &err);
                }
                ExitCode::FAILURE
            }
        }
    }
}

/// Detect a user-driven cancellation (e.g. Escape in the interactive selector)
/// anywhere in the error chain, so we can exit cleanly without logging at error
/// level or emitting a desktop notification.
fn is_user_cancelled(err: &anyhow::Error) -> bool {
    #[cfg(feature = "ui")]
    {
        err.chain()
            .any(|e| e.is::<snypr::ui::selector::Cancelled>())
    }
    #[cfg(not(feature = "ui"))]
    {
        let _ = err;
        false
    }
}

/// Build the tracing subscriber.
///
/// Precedence (highest first):
/// 1. `RUST_LOG` env var — full directive syntax.
/// 2. `-v` / `-vv` CLI count — `1 → debug`, `≥2 → trace`. Applies to the `snypr` crate only
///    so we don't drown in GTK/wayland chatter; bump RUST_LOG for that.
/// 3. Default `info`.
///
/// We parse `cli.verbose` *before* tracing init so the verbose flag actually drives the log
/// level instead of being silently dropped.
fn init_tracing(verbose: u8) -> anyhow::Result<()> {
    let filter = if let Ok(f) = EnvFilter::try_from_default_env() {
        f
    } else {
        let level = match verbose {
            0 => "info",
            1 => "snypr=debug,info",
            _ => "snypr=trace,debug",
        };
        EnvFilter::new(level)
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init()
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    Ok(())
}
