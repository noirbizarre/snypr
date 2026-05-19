//! Long-lived IPC daemon listening on a Unix socket.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, oneshot};

use crate::capture::Selection;
use crate::capture::region::Rect;
use crate::cli::SinkSpec as CliSinkSpec;
use crate::context::Ctx;
use crate::ipc::{Request, Response, ScreenshotRequest, SelectionSpec, SinkSpec};

/// Handle to a daemon-spawned draw overlay. Holds the channels needed to drive it from
/// outside the GTK thread: a oneshot to tear it down, and an mpsc to inject runtime
/// commands (passthrough toggles, future tool changes, …).
struct OverlayHandle {
    shutdown: oneshot::Sender<()>,
    commands: tokio::sync::mpsc::UnboundedSender<crate::ui::overlay::OverlayCommand>,
}

/// Per-daemon mutable state that survives across IPC clients. Shared via `Arc` between the
/// accept loop and every spawned handler so concurrent `Screenshot --edit` / `DrawToggle`
/// requests can coordinate.
#[derive(Default)]
struct DaemonState {
    /// Serialises the editor-bearing screenshot path — GTK's `Application::run` is per-process,
    /// so two editor windows from two clients would clash. Acquired with `try_lock` so a second
    /// client gets an immediate "busy" error instead of queuing. Headless screenshots (no
    /// `--edit`) never touch this lock.
    editor: Mutex<()>,
    /// `Some(handle)` while a daemon-managed draw overlay is alive. A `DrawToggle` request
    /// flips this: present → fire `shutdown` to tear it down; absent → spawn a fresh overlay.
    /// A `PassthroughToggle` request reads the `commands` channel instead.
    overlay: Mutex<Option<OverlayHandle>>,
}

/// Default IPC socket path: `$XDG_RUNTIME_DIR/hyprsnap.sock`.
pub fn default_socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("hyprsnap.sock")
}

pub async fn serve(ctx: Ctx, socket: PathBuf, systray: bool) -> Result<()> {
    if socket.exists() {
        std::fs::remove_file(&socket)
            .with_context(|| format!("removing stale socket {}", socket.display()))?;
    }
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("binding socket {}", socket.display()))?;
    tracing::info!(path = %socket.display(), "hyprsnap daemon listening");

    let state = Arc::new(DaemonState::default());

    // Optional StatusNotifierItem tray. Held in scope here so its Drop runs when the daemon
    // exits. The handle is unused beyond that — actions are funnelled back over `tray_rx`.
    #[cfg(feature = "tray")]
    let (mut tray_rx, _tray_handle) = setup_tray(systray).await?;
    #[cfg(not(feature = "tray"))]
    let mut tray_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>> = {
        if systray {
            tracing::warn!("--systray ignored: built without the `tray` feature");
        }
        None
    };

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            res = listener.accept() => {
                match res {
                    Ok((stream, _addr)) => {
                        let ctx = ctx.clone();
                        let state = state.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(ctx, state, stream).await {
                                tracing::warn!(error = ?err, "daemon client error");
                            }
                        });
                    }
                    Err(err) => tracing::warn!(error = ?err, "accept failed"),
                }
            }
            tray_action = recv_optional(&mut tray_rx) => {
                if handle_tray_action(&ctx, &state, tray_action).await {
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
    enabled: bool,
) -> Result<(
    Option<tokio::sync::mpsc::UnboundedReceiver<crate::ui::tray::TrayAction>>,
    Option<ksni::Handle<crate::ui::tray::HyprSnapTray>>,
)> {
    if !enabled {
        return Ok((None, None));
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = crate::ui::tray::spawn(tx).await?;
    Ok((Some(rx), Some(handle)))
}

/// Returns `true` when the action requested a daemon shutdown.
#[cfg(feature = "tray")]
async fn handle_tray_action(
    ctx: &Ctx,
    state: &Arc<DaemonState>,
    action: Option<crate::ui::tray::TrayAction>,
) -> bool {
    use crate::ui::tray::TrayAction;
    let Some(action) = action else {
        // Channel closed: the tray went away. Treat as a no-op so the daemon keeps serving the
        // IPC socket.
        return false;
    };
    let ctx = ctx.clone();
    let state = state.clone();
    match action {
        TrayAction::Quit => return true,
        TrayAction::Screenshot { edit } => {
            tokio::spawn(async move {
                let sinks = ctx.config.default_sinks();
                // Editor flow uses the interactive selector so users can pick a region before
                // annotating; the plain screenshot goes straight to a full-desktop capture.
                let selection = if edit {
                    Selection::Interactive
                } else {
                    Selection::Full
                };
                // Tray entry points have no CLI flag to override; honor `[capture].delay`
                // straight from config so the persistent default still applies.
                let delay = ctx.config.capture.delay;
                let result = run_screenshot_with_optional_lock(
                    &ctx, &state, selection, false, sinks, edit, delay,
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
        TrayAction::OpenDraw => {
            tokio::spawn(async move {
                if let Err(err) = toggle_overlay(&ctx, &state).await {
                    tracing::warn!(error = ?err, "tray overlay toggle failed");
                }
            });
        }
    }
    false
}

#[cfg(not(feature = "tray"))]
async fn handle_tray_action(_ctx: &Ctx, _state: &Arc<DaemonState>, _action: Option<()>) -> bool {
    false
}

async fn handle_client(ctx: Ctx, state: Arc<DaemonState>, stream: UnixStream) -> Result<()> {
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
            Ok(req) => dispatch(ctx.clone(), state.clone(), req).await,
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
async fn dispatch(ctx: Ctx, state: Arc<DaemonState>, req: Request) -> Response {
    let result = match req {
        Request::Ping => Ok(Response::Ok),
        Request::Screenshot(req) => run_screenshot(ctx, state, req).await,
        Request::DrawToggle => toggle_overlay(&ctx, &state).await.map(|_| Response::Ok),
        Request::PassthroughToggle => toggle_overlay_passthrough(&state)
            .await
            .map(|_| Response::Ok),
    };
    match result {
        Ok(resp) => resp,
        Err(err) => Response::Error {
            message: format!("{err:#}"),
        },
    }
}

/// Send a [`crate::ui::overlay::OverlayCommand::TogglePassthrough`] to a live daemon-managed
/// overlay. Errors when no overlay is alive — the user is expected to wire this to a
/// Hyprland global keybind that's only meaningful when an overlay is up.
async fn toggle_overlay_passthrough(state: &Arc<DaemonState>) -> Result<()> {
    let guard = state.overlay.lock().await;
    let handle = guard
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no draw overlay is currently running"))?;
    handle
        .commands
        .send(crate::ui::overlay::OverlayCommand::TogglePassthrough)
        .map_err(|_| anyhow::anyhow!("overlay command channel closed"))?;
    Ok(())
}

/// Acquire `state.editor` with `try_lock` (only when `edit` is true) so a second editor request
/// gets an immediate "busy" error rather than queuing behind a GTK editor window that may sit
/// open for minutes. Headless screenshots bypass the lock entirely so multiple concurrent
/// `hyprsnap screenshot` calls keep working.
async fn run_screenshot_with_optional_lock(
    ctx: &Ctx,
    state: &Arc<DaemonState>,
    selection: Selection,
    cursor: bool,
    sinks: Vec<CliSinkSpec>,
    edit: bool,
    delay: Option<u32>,
) -> Result<Vec<PathBuf>> {
    if edit {
        let _guard = state
            .editor
            .try_lock()
            .map_err(|_| anyhow::anyhow!("another editor session is already in progress"))?;
        crate::cli::screenshot::execute(ctx.clone(), selection, cursor, sinks, true, delay).await
    } else {
        crate::cli::screenshot::execute(ctx.clone(), selection, cursor, sinks, false, delay).await
    }
}

/// Toggle the daemon-managed draw overlay: kill the running instance if present, otherwise
/// spawn a fresh one. The `oneshot::Sender` stored in `state.overlay` is the shutdown signal
/// the overlay's GTK task awaits via `attach_shutdown`; a spawned task clears the slot when
/// the overlay actually exits so a follow-up toggle starts a new one.
async fn toggle_overlay(ctx: &Ctx, state: &Arc<DaemonState>) -> Result<Response> {
    let mut guard = state.overlay.lock().await;
    if let Some(handle) = guard.take() {
        // Sender ignores its return value: if the receiver was already dropped (overlay died on
        // its own) the next branch would have been taken instead. Either way the slot is now
        // empty and a follow-up toggle will spawn a new overlay.
        let _ = handle.shutdown.send(());
        return Ok(Response::Ok);
    }
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    *guard = Some(OverlayHandle {
        shutdown: shutdown_tx,
        commands: cmd_tx,
    });
    let ctx = ctx.clone();
    let state = state.clone();
    tokio::spawn(async move {
        let sinks = ctx.config.default_sinks();
        if let Err(err) = crate::ui::overlay::run(
            ctx,
            crate::ui::overlay::OverlayMode::Draw {
                passthrough: false,
                sinks,
                cursor: false,
            },
            Some(shutdown_rx),
            Some(cmd_rx),
        )
        .await
        {
            tracing::warn!(error = ?err, "overlay task failed");
        }
        // Drop the stored handle so the *next* DrawToggle spawns a fresh overlay instead of
        // sending into dead channels.
        let mut guard = state.overlay.lock().await;
        *guard = None;
    });
    Ok(Response::Ok)
}

async fn run_screenshot(
    ctx: Ctx,
    state: Arc<DaemonState>,
    req: ScreenshotRequest,
) -> Result<Response> {
    let selection = selection_from_spec(req.selection);
    let sinks = sinks_from_specs(req.sinks, &ctx);
    // CLI-supplied delay wins; otherwise fall back to the daemon's loaded config. Wire
    // representation is whole seconds (see `crate::ipc::ScreenshotRequest::delay_secs`).
    let delay = crate::cli::screenshot::effective_delay(req.delay_secs, ctx.config.capture.delay);
    let paths = run_screenshot_with_optional_lock(
        &ctx, &state, selection, req.cursor, sinks, req.edit, delay,
    )
    .await?;
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
