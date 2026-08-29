//! Long-lived IPC daemon listening on a Unix socket.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
// Only the overlay lifecycle uses a oneshot, and that whole path is `ui`-gated.
#[cfg(feature = "ui")]
use tokio::sync::oneshot;

use crate::capture::Selection;
use crate::capture::region::Rect;
use crate::cli::SinkSpec as CliSinkSpec;
use crate::context::Ctx;
use crate::ipc::{Request, Response, ScreenshotRequest, SelectionSpec, SinkSpec};

/// Handle to a daemon-spawned draw overlay. Holds the channels needed to drive it from
/// outside the GTK thread: a oneshot to tear it down, and an mpsc to inject runtime
/// commands (passthrough toggles, future tool changes, …).
///
/// Only exists in `ui` builds: without the feature there is no overlay to hold a handle to.
#[cfg(feature = "ui")]
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
    #[cfg(feature = "ui")]
    overlay: Mutex<Option<OverlayHandle>>,
}

/// Default IPC socket path: `$XDG_RUNTIME_DIR/snypr.sock`, falling back to the OS temp
/// directory if `$XDG_RUNTIME_DIR` is unset.
pub fn default_socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join("snypr.sock")
}

pub async fn serve(ctx: Ctx, socket: PathBuf, systray: bool) -> Result<()> {
    if socket.exists() {
        std::fs::remove_file(&socket)
            .with_context(|| format!("removing stale socket {}", socket.display()))?;
    }
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("binding socket {}", socket.display()))?;
    tracing::info!(path = %socket.display(), "snypr daemon listening");

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
    Option<ksni::Handle<crate::ui::tray::SnyprTray>>,
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
                message: crate::i18n::fl!("error-malformed-request", reason = err.to_string()),
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
#[cfg(feature = "ui")]
async fn toggle_overlay_passthrough(state: &Arc<DaemonState>) -> Result<()> {
    let guard = state.overlay.lock().await;
    let handle = guard
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("{}", crate::i18n::fl!("error-no-draw-overlay")))?;
    handle
        .commands
        .send(crate::ui::overlay::OverlayCommand::TogglePassthrough)
        .map_err(|_| anyhow::anyhow!("{}", crate::i18n::fl!("error-overlay-channel-closed")))?;
    Ok(())
}

/// Without the `ui` feature there is no overlay to toggle passthrough on.
#[cfg(not(feature = "ui"))]
async fn toggle_overlay_passthrough(_state: &Arc<DaemonState>) -> Result<()> {
    anyhow::bail!("{}", crate::i18n::fl!("error-draw-requires-ui-feature"))
}

/// Acquire `state.editor` with `try_lock` (only when `edit` is true) so a second editor request
/// gets an immediate "busy" error rather than queuing behind a GTK editor window that may sit
/// open for minutes. Headless screenshots bypass the lock entirely so multiple concurrent
/// `snypr screenshot` calls keep working.
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
            .map_err(|_| anyhow::anyhow!("{}", crate::i18n::fl!("error-editor-busy")))?;
        crate::cli::screenshot::execute(ctx.clone(), selection, cursor, sinks, true, delay).await
    } else {
        crate::cli::screenshot::execute(ctx.clone(), selection, cursor, sinks, false, delay).await
    }
}

/// Toggle the daemon-managed draw overlay: kill the running instance if present, otherwise
/// spawn a fresh one. The `oneshot::Sender` stored in `state.overlay` is the shutdown signal
/// the overlay's GTK task awaits via `attach_shutdown`; a spawned task clears the slot when
/// the overlay actually exits so a follow-up toggle starts a new one.
#[cfg(feature = "ui")]
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

/// Without the `ui` feature the draw overlay cannot be built, so the toggle is a hard error
/// rather than a silent no-op — the caller asked for something this binary cannot do.
#[cfg(not(feature = "ui"))]
async fn toggle_overlay(_ctx: &Ctx, _state: &Arc<DaemonState>) -> Result<Response> {
    anyhow::bail!("{}", crate::i18n::fl!("error-draw-requires-ui-feature"))
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
    // Same resolution as the CLI: the request flag turns the cursor on, otherwise the
    // daemon's `[capture].cursor` decides.
    let cursor = crate::cli::screenshot::effective_cursor(req.cursor, ctx.config.capture.cursor);
    let paths =
        run_screenshot_with_optional_lock(&ctx, &state, selection, cursor, sinks, req.edit, delay)
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
    let default_kind = ctx.config.clipboard.default_kind;
    specs
        .into_iter()
        .map(|s| match s {
            SinkSpec::File { path } => CliSinkSpec::File(path),
            SinkSpec::Clipboard { clipboard_kind } => {
                // Apply the daemon's configured default when the wire field is missing so
                // clients that never set `--clipboard-type` still observe the daemon's
                // `[clipboard].default_kind`.
                CliSinkSpec::Clipboard(Some(clipboard_kind.unwrap_or(default_kind)))
            }
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
            CliSinkSpec::Clipboard(kind) => SinkSpec::Clipboard {
                clipboard_kind: *kind,
            },
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
        use crate::cli::ClipboardKind;
        let cli = vec![
            CliSinkSpec::File(None),
            CliSinkSpec::File(Some("/tmp/x.png".into())),
            CliSinkSpec::Clipboard(Some(ClipboardKind::Regular)),
            CliSinkSpec::Clipboard(Some(ClipboardKind::Primary)),
            CliSinkSpec::Clipboard(Some(ClipboardKind::Both)),
        ];
        let wire = sinks_to_specs(&cli);
        // We can't easily fake a Ctx here, so emulate the no-default fast-path manually.
        let back: Vec<CliSinkSpec> = wire
            .into_iter()
            .map(|s| match s {
                SinkSpec::File { path } => CliSinkSpec::File(path),
                SinkSpec::Clipboard { clipboard_kind } => CliSinkSpec::Clipboard(clipboard_kind),
            })
            .collect();
        assert_eq!(cli, back);
    }

    // --- dispatch ---------------------------------------------------------
    //
    // `dispatch` flattens every helper error into a `Response::Error` so a client always
    // gets a structured frame back instead of a torn connection. These assert that
    // contract without touching a socket.

    use crate::testing::test_ctx;

    fn state() -> Arc<DaemonState> {
        Arc::new(DaemonState::default())
    }

    #[tokio::test]
    async fn ping_is_answered_ok() {
        let ctx = test_ctx().await;
        assert!(matches!(
            dispatch(ctx, state(), Request::Ping).await,
            Response::Ok
        ));
    }

    #[tokio::test]
    async fn toggling_passthrough_without_an_overlay_is_a_structured_error() {
        let ctx = test_ctx().await;
        // The client binds this to a global keybind, so pressing it with no overlay up must
        // produce a readable message rather than dropping the connection.
        match dispatch(ctx, state(), Request::PassthroughToggle).await {
            Response::Error { message } => assert!(!message.is_empty(), "empty error message"),
            other => panic!("expected an error response, got {other:?}"),
        }
    }

    #[cfg(not(feature = "ui"))]
    #[tokio::test]
    async fn draw_toggle_reports_the_missing_ui_feature() {
        let ctx = test_ctx().await;
        // Without `ui` there is no overlay to spawn; the daemon must say so rather than
        // silently answering Ok and leaving the user waiting for a window.
        match dispatch(ctx, state(), Request::DrawToggle).await {
            Response::Error { message } => assert!(!message.is_empty()),
            other => panic!("expected an error response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_editor_request_cannot_run_twice_concurrently() {
        let state = state();
        // Hold the editor lock the way an open editor window does.
        let _guard = state.editor.try_lock().expect("uncontended");
        let ctx = test_ctx().await;
        let err = run_screenshot_with_optional_lock(
            &ctx,
            &state,
            Selection::Full,
            false,
            vec![],
            true,
            None,
        )
        .await
        .unwrap_err();
        // GTK's Application::run is per-process, so the second client is refused immediately
        // rather than queued behind a window that may sit open for minutes.
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn sinks_from_specs_applies_the_daemon_default_kind_to_a_bare_clipboard_entry() {
        use crate::cli::ClipboardKind;
        // A client that never passed `--clipboard-type` sends `clipboard_kind: None`; the
        // daemon's own `[clipboard].default_kind` must fill it in.
        let specs = vec![SinkSpec::Clipboard {
            clipboard_kind: None,
        }];
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ctx = rt.block_on(test_ctx());
        let out = sinks_from_specs(specs, &ctx);
        assert_eq!(
            out,
            vec![CliSinkSpec::Clipboard(Some(
                ctx.config.clipboard.default_kind
            ))]
        );
        // Sanity: the default really is a concrete kind, not a placeholder.
        assert!(matches!(
            ctx.config.clipboard.default_kind,
            ClipboardKind::Regular | ClipboardKind::Primary | ClipboardKind::Both
        ));
    }

    #[test]
    fn an_empty_sink_list_falls_back_to_the_configured_defaults() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let ctx = rt.block_on(test_ctx());
        assert_eq!(sinks_from_specs(vec![], &ctx), ctx.config.default_sinks());
    }

    #[test]
    fn the_default_socket_path_follows_xdg_runtime_dir() {
        // Not parallel-safe against other env-mutating tests, but nextest runs each test in
        // its own process.
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", dir.path()) };
        assert_eq!(default_socket_path(), dir.path().join("snypr.sock"));
    }

    #[test]
    fn the_socket_path_falls_back_to_the_temp_dir() {
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
        assert_eq!(
            default_socket_path(),
            std::env::temp_dir().join("snypr.sock")
        );
    }
}
