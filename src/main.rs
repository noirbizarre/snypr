use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use hyprsnap::cli::{Cli, dispatch};

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Err(err) = init_tracing(cli.verbose) {
        eprintln!("failed to initialise tracing: {err:#}");
        return ExitCode::FAILURE;
    }

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
            tracing::error!(error = ?err, "hyprsnap failed");
            eprintln!("error: {err:#}");
            #[cfg(feature = "notify")]
            notify_error(&err);
            ExitCode::FAILURE
        }
    }
}

/// Build the tracing subscriber.
///
/// Precedence (highest first):
/// 1. `RUST_LOG` env var — full directive syntax.
/// 2. `-v` / `-vv` CLI count — `1 → debug`, `≥2 → trace`. Applies to the `hyprsnap` crate only
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
            1 => "hyprsnap=debug,info",
            _ => "hyprsnap=trace,debug",
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

/// Emit a desktop notification for a fatal error so the user sees something when hyprsnap
/// was launched from a Hyprland keybind (where stderr is detached). Best-effort: failures
/// to talk to the notification daemon are logged at debug and otherwise ignored.
#[cfg(feature = "notify")]
fn notify_error(err: &anyhow::Error) {
    use notify_rust::Notification;

    let body = format!("{err:#}");
    if let Err(e) = Notification::new()
        .summary("HyprSnap")
        .body(&body)
        .icon("noirbizar.re.HyprSnap")
        .appname("hyprsnap")
        .timeout(notify_rust::Timeout::Milliseconds(6000))
        .show()
    {
        tracing::debug!(error = ?e, "failed to emit desktop notification");
    }
}
