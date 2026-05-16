//! Long-lived IPC daemon listening on a Unix socket.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::capture::Selection;
use crate::capture::region::Rect;
use crate::cli::SinkSpec as CliSinkSpec;
use crate::context::Ctx;
use crate::ipc::{Request, Response, ScreenshotRequest, SelectionSpec, SinkSpec};

/// Default IPC socket path: `$XDG_RUNTIME_DIR/hyprsnap.sock`.
pub fn default_socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("hyprsnap.sock")
}

pub async fn serve(ctx: Ctx, socket: PathBuf) -> Result<()> {
    if socket.exists() {
        std::fs::remove_file(&socket)
            .with_context(|| format!("removing stale socket {}", socket.display()))?;
    }
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("binding socket {}", socket.display()))?;
    tracing::info!(path = %socket.display(), "hyprsnap daemon listening");

    // Optional StatusNotifierItem tray. Held in scope here so its Drop runs when the daemon
    // exits. The handle is unused beyond that — actions are funnelled back over `tray_rx`.
    #[cfg(feature = "tray")]
    let (mut tray_rx, _tray_handle) = setup_tray(&ctx).await?;
    #[cfg(not(feature = "tray"))]
    let mut tray_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>> = None;

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            res = listener.accept() => {
                match res {
                    Ok((stream, _addr)) => {
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(ctx, stream).await {
                                tracing::warn!(error = ?err, "daemon client error");
                            }
                        });
                    }
                    Err(err) => tracing::warn!(error = ?err, "accept failed"),
                }
            }
            tray_action = recv_optional(&mut tray_rx) => {
                if handle_tray_action(&ctx, tray_action).await {
                    tracing::info!("tray requested daemon shutdown");
                    break;
                }
            }
            _ = &mut shutdown => {
                tracing::info!("daemon shutting down");
                break;
            }
        }
    }

    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// Awaits the next message from an optional channel; returns `None` forever when the channel is
/// absent (so the `select!` arm never resolves and doesn't tip the loop). This is cleaner than
/// peppering the `select!` with `#[cfg]` gates.
async fn recv_optional<T>(rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match rx.as_mut() {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(feature = "tray")]
async fn setup_tray(
    ctx: &Ctx,
) -> Result<(
    Option<tokio::sync::mpsc::UnboundedReceiver<crate::ui::tray::TrayAction>>,
    Option<ksni::Handle<crate::ui::tray::HyprSnapTray>>,
)> {
    if !ctx.config.tray.enabled {
        return Ok((None, None));
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = crate::ui::tray::spawn(tx).await?;
    Ok((Some(rx), Some(handle)))
}

/// Returns `true` when the action requested a daemon shutdown.
#[cfg(feature = "tray")]
async fn handle_tray_action(ctx: &Ctx, action: Option<crate::ui::tray::TrayAction>) -> bool {
    use crate::ui::tray::TrayAction;
    let Some(action) = action else {
        // Channel closed: the tray went away. Treat as a no-op so the daemon keeps serving the
        // IPC socket.
        return false;
    };
    let ctx = ctx.clone();
    match action {
        TrayAction::Quit => return true,
        TrayAction::Screenshot => {
            tokio::spawn(async move {
                let sinks = ctx.config.default_sinks();
                let result = crate::cli::screenshot::execute(
                    ctx,
                    crate::capture::Selection::Full,
                    false,
                    sinks,
                )
                .await;
                match result {
                    Ok(paths) => {
                        for p in &paths {
                            tracing::info!(path = %p.display(), "tray screenshot saved");
                        }
                    }
                    Err(err) => tracing::warn!(error = ?err, "tray screenshot failed"),
                }
            });
        }
        TrayAction::Capture => {
            tokio::spawn(async move {
                let sinks = ctx.config.default_sinks();
                if let Err(err) = crate::ui::run_capture_flow(ctx, sinks, false).await {
                    tracing::warn!(error = ?err, "tray capture failed");
                }
            });
        }
        TrayAction::OpenDraw => {
            tokio::spawn(async move {
                if let Err(err) = crate::ui::overlay::run(ctx, false).await {
                    tracing::warn!(error = ?err, "tray overlay failed");
                }
            });
        }
    }
    false
}

#[cfg(not(feature = "tray"))]
async fn handle_tray_action(_ctx: &Ctx, _action: Option<()>) -> bool {
    false
}

async fn handle_client(ctx: Ctx, stream: UnixStream) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    while reader.read_line(&mut line).await? > 0 {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            line.clear();
            continue;
        }
        let resp = match serde_json::from_str::<Request>(trimmed) {
            Ok(req) => dispatch(ctx.clone(), req).await,
            Err(err) => Response::Error {
                message: format!("malformed request: {err}"),
            },
        };
        let mut bytes = serde_json::to_vec(&resp)?;
        bytes.push(b'\n');
        write.write_all(&bytes).await?;
        line.clear();
    }
    Ok(())
}

/// Run a single IPC `Request` against the daemon's context and return the matching `Response`.
/// Errors propagated by helpers are flattened into [`Response::Error`] so the client always
/// gets a structured frame back instead of a torn connection.
async fn dispatch(ctx: Ctx, req: Request) -> Response {
    let result = match req {
        Request::Ping => Ok(Response::Ok),
        Request::Screenshot(req) => run_screenshot(ctx, req).await,
        // The capture flow opens an interactive GTK editor; ferrying its in-memory result back
        // over the socket (and the cleanup that implies) is complex enough to deserve its own
        // commit. Surfaces an explicit error so clients can fall back to running locally.
        Request::Capture(_) => Err(anyhow::anyhow!(
            "Capture-over-IPC is not yet implemented; run `hyprsnap capture` directly"
        )),
        Request::DrawToggle => Err(anyhow::anyhow!(
            "DrawToggle-over-IPC is not yet implemented; run `hyprsnap draw` directly"
        )),
    };
    match result {
        Ok(resp) => resp,
        Err(err) => Response::Error {
            message: format!("{err:#}"),
        },
    }
}

async fn run_screenshot(ctx: Ctx, req: ScreenshotRequest) -> Result<Response> {
    let selection = selection_from_spec(req.selection);
    let sinks = sinks_from_specs(req.sinks, &ctx);
    let paths = crate::cli::screenshot::execute(ctx, selection, req.cursor, sinks).await?;
    Ok(Response::Paths { paths })
}

/// Convert the wire `SelectionSpec` into the in-process `Selection`. They mirror each other 1:1
/// today; the indirection keeps `crate::capture` free of serde derives.
pub fn selection_from_spec(spec: SelectionSpec) -> Selection {
    match spec {
        SelectionSpec::Full => Selection::Full,
        SelectionSpec::PerOutput => Selection::PerOutput,
        SelectionSpec::Focused => Selection::Focused,
        SelectionSpec::Output { name } => Selection::Output(name),
        SelectionSpec::Window => Selection::Window,
        SelectionSpec::Region { x, y, w, h } => Selection::Region(Rect { x, y, w, h }),
        SelectionSpec::Interactive => Selection::Interactive,
    }
}

/// Reverse of [`selection_from_spec`], used by the IPC client when forwarding CLI args to the
/// daemon.
pub fn selection_to_spec(selection: &Selection) -> SelectionSpec {
    match selection {
        Selection::Full => SelectionSpec::Full,
        Selection::PerOutput => SelectionSpec::PerOutput,
        Selection::Focused => SelectionSpec::Focused,
        Selection::Output(name) => SelectionSpec::Output { name: name.clone() },
        Selection::Window => SelectionSpec::Window,
        Selection::Region(r) => SelectionSpec::Region {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
        },
        Selection::Interactive => SelectionSpec::Interactive,
    }
}

fn sinks_from_specs(specs: Vec<SinkSpec>, ctx: &Ctx) -> Vec<CliSinkSpec> {
    if specs.is_empty() {
        return ctx.config.default_sinks();
    }
    specs
        .into_iter()
        .map(|s| match s {
            SinkSpec::File { path } => CliSinkSpec::File(path),
            SinkSpec::Clipboard => CliSinkSpec::Clipboard,
        })
        .collect()
}

/// Convert the CLI sink list into the IPC wire form. Mirror of [`sinks_from_specs`] used by the
/// `--via-daemon` client path.
pub fn sinks_to_specs(sinks: &[CliSinkSpec]) -> Vec<SinkSpec> {
    sinks
        .iter()
        .map(|s| match s {
            CliSinkSpec::File(path) => SinkSpec::File { path: path.clone() },
            CliSinkSpec::Clipboard => SinkSpec::Clipboard,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn selection_spec_round_trips() {
        for s in [
            Selection::Full,
            Selection::PerOutput,
            Selection::Focused,
            Selection::Window,
            Selection::Interactive,
            Selection::Output("DP-1".into()),
            Selection::Region(Rect {
                x: 1,
                y: 2,
                w: 3,
                h: 4,
            }),
        ] {
            let back = selection_from_spec(selection_to_spec(&s));
            assert_eq!(format!("{s:?}"), format!("{back:?}"));
        }
    }

    #[test]
    fn sinks_round_trip() {
        let cli = vec![
            CliSinkSpec::File(None),
            CliSinkSpec::File(Some("/tmp/x.png".into())),
            CliSinkSpec::Clipboard,
        ];
        let wire = sinks_to_specs(&cli);
        // We can't easily fake a Ctx here, so emulate the no-default fast-path manually.
        let back: Vec<CliSinkSpec> = wire
            .into_iter()
            .map(|s| match s {
                SinkSpec::File { path } => CliSinkSpec::File(path),
                SinkSpec::Clipboard => CliSinkSpec::Clipboard,
            })
            .collect();
        assert_eq!(cli, back);
    }
}
