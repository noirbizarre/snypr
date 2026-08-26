//! Hyprland IPC backend (active window, focused monitor).
//!
//! Connects directly to Hyprland's command socket and parses the JSON responses to `activewindow`
//! and `monitors`. We deliberately avoid the upstream `hyprland` crate because it hard-codes the
//! pre-0.42 `/tmp/hypr/...` socket path, panics (rather than returns `Err`) on I/O failure, and
//! pulls in a large dependency tree for the two queries we need.
//!
//! Socket path resolution mirrors Hyprland 0.42+ with a fallback to the legacy location:
//!   1. `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock`
//!   2. `/tmp/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock`

use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, watch};

use super::{ActiveWindow, WmBackend, WmWindow};

/// Hyprland [`WmBackend`] implementation.
pub struct Hyprland;

#[async_trait::async_trait]
impl WmBackend for Hyprland {
    fn name(&self) -> &'static str {
        "Hyprland"
    }

    fn socket_path(&self) -> Result<PathBuf> {
        socket_path()
    }

    async fn active_window(&self) -> Result<ActiveWindow> {
        active_window().await
    }

    async fn clients(&self) -> Result<Vec<WmWindow>> {
        clients().await
    }

    async fn focused_output(&self) -> Result<String> {
        focused_monitor().await
    }

    fn subscribe_focus(
        &self,
        handle: &tokio::runtime::Handle,
        shutdown: oneshot::Receiver<()>,
    ) -> watch::Receiver<Option<String>> {
        subscribe_focus(handle, shutdown)
    }
}

/// Fetch the currently active window via Hyprland IPC.
async fn active_window() -> Result<ActiveWindow> {
    let body = query("j/activewindow").await?;
    // When no client is focused Hyprland answers with an empty object `{}` (or the literal string
    // `none` on older builds). Treat both as "no active window".
    let trimmed = body.trim();
    if trimmed.is_empty() || trimmed == "{}" || trimmed.eq_ignore_ascii_case("none") {
        bail!("{}", crate::i18n::fl!("error-no-active-window"));
    }

    #[derive(Deserialize)]
    struct Raw {
        title: String,
        class: String,
        at: [i32; 2],
        size: [i32; 2],
        monitor: i64,
    }

    let raw: Raw = serde_json::from_str(&body)
        .with_context(|| format!("parsing Hyprland activewindow response: {body}"))?;
    Ok(ActiveWindow {
        title: raw.title,
        class: raw.class,
        at: Some((raw.at[0], raw.at[1])),
        size: Some((raw.size[0].max(0) as u32, raw.size[1].max(0) as u32)),
        monitor: raw.monitor.to_string(),
    })
}

/// List every Hyprland client (window). Order is preserved from the IPC response so
/// front-to-back hit-testing works without re-sorting.
async fn clients() -> Result<Vec<WmWindow>> {
    let body = query("j/clients").await?;

    #[derive(Deserialize)]
    struct RawWorkspace {
        id: i64,
    }
    #[derive(Deserialize)]
    struct Raw {
        address: String,
        title: String,
        class: String,
        at: [i32; 2],
        size: [i32; 2],
        monitor: i64,
        workspace: RawWorkspace,
        mapped: bool,
        hidden: bool,
    }

    let raw: Vec<Raw> = serde_json::from_str(&body)
        .with_context(|| format!("parsing Hyprland clients response: {body}"))?;
    Ok(raw
        .into_iter()
        .map(|r| WmWindow {
            id: r.address,
            title: r.title,
            class: r.class,
            at: Some((r.at[0], r.at[1])),
            size: Some((r.size[0].max(0) as u32, r.size[1].max(0) as u32)),
            monitor: r.monitor.to_string(),
            workspace_id: r.workspace.id,
            mapped: r.mapped,
            hidden: r.hidden,
        })
        .collect())
}

/// Name of the focused monitor.
async fn focused_monitor() -> Result<String> {
    let body = query("j/monitors").await?;

    #[derive(Deserialize)]
    struct Raw {
        name: String,
        focused: bool,
    }

    let monitors: Vec<Raw> = serde_json::from_str(&body)
        .with_context(|| format!("parsing Hyprland monitors response: {body}"))?;
    monitors
        .into_iter()
        .find(|m| m.focused)
        .map(|m| m.name)
        .ok_or_else(|| anyhow!("{}", crate::i18n::fl!("error-no-focused-monitor")))
}

/// Subscribe to Hyprland focused-monitor changes.
///
/// Spawns a task on `handle` that connects to the event socket (`.socket2.sock`), reads the
/// newline-delimited `EVENT>>DATA` stream, and publishes the focused monitor's **connector
/// name** (e.g. `DP-1`, matching GDK's `Monitor::connector()`) to the returned
/// [`watch::Receiver`]. The watch channel coalesces rapid focus changes — a consumer only ever
/// observes the most recent focused monitor.
///
/// Best-effort: the task stops when `shutdown` fires, all receivers are dropped, or the socket
/// closes/errors (e.g. not running under Hyprland). Failures are logged at debug/warn and never
/// propagated — callers keep working with a static (non-following) toolbar.
///
/// The initial value is `None` (focus unknown); consumers should ignore `None` and rely on a
/// one-shot [`focused_monitor`] for the *initial* placement instead.
fn subscribe_focus(
    handle: &tokio::runtime::Handle,
    shutdown: oneshot::Receiver<()>,
) -> watch::Receiver<Option<String>> {
    let (tx, rx) = watch::channel(None);
    handle.spawn(async move {
        tokio::select! {
            _ = shutdown => {}
            _ = pump_focus_events(&tx) => {}
        }
        tracing::debug!("Hyprland focus subscription stopped");
    });
    rx
}

/// Connect to the Hyprland event socket and forward focused-monitor names into `tx` until the
/// stream ends or every receiver is dropped. Returns on any I/O error (logged by the caller's
/// `select!` arm completion); errors here are non-fatal by design.
async fn pump_focus_events(tx: &watch::Sender<Option<String>>) {
    let path = match socket_path_named(".socket2.sock") {
        Ok(p) => p,
        Err(err) => {
            tracing::debug!(error = ?err, "Hyprland event socket unavailable; focus-follow disabled");
            return;
        }
    };
    let stream = match UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = ?err, path = %path.display(), "connecting to Hyprland event socket failed");
            return;
        }
    };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF: compositor closed the socket.
            Ok(_) => {
                if let Some(name) = parse_focused_monitor_event(line.trim_end()) {
                    // `send_replace` ignores the "no receivers" case; we detect it explicitly so
                    // the task can stop once every surface has torn down.
                    if tx.send(Some(name.to_string())).is_err() {
                        break;
                    }
                }
            }
            Err(err) => {
                tracing::warn!(error = ?err, "reading Hyprland event stream failed");
                break;
            }
        }
    }
}

/// Extract the focused monitor connector name from a Hyprland event line.
///
/// Hyprland emits `focusedmon>>MONITORNAME,WORKSPACENAME` and
/// `focusedmonv2>>MONITORNAME,WORKSPACEID` on every monitor focus change; in both the first
/// comma-separated field is the monitor name. Returns `None` for any other event or a malformed
/// line.
fn parse_focused_monitor_event(line: &str) -> Option<&str> {
    let data = line
        .strip_prefix("focusedmonv2>>")
        .or_else(|| line.strip_prefix("focusedmon>>"))?;
    let name = data.split(',').next()?.trim();
    if name.is_empty() { None } else { Some(name) }
}

/// Send `command` to the Hyprland command socket and return the raw response body.
async fn query(command: &str) -> Result<String> {
    let path = socket_path().context("locating Hyprland IPC socket")?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to Hyprland IPC at {}", path.display()))?;
    stream
        .write_all(command.as_bytes())
        .await
        .context("writing Hyprland IPC command")?;
    stream
        .shutdown()
        .await
        .context("closing Hyprland IPC write side")?;
    let mut body = String::new();
    stream
        .read_to_string(&mut body)
        .await
        .context("reading Hyprland IPC response")?;
    Ok(body)
}

/// Resolve the Hyprland command-socket path (`.socket.sock`).
///
/// Prefers the modern `$XDG_RUNTIME_DIR/hypr/$HIS/.socket.sock` layout (Hyprland ≥ 0.42) and
/// falls back to the legacy `/tmp/hypr/$HIS/.socket.sock` for older builds.
fn socket_path() -> Result<PathBuf> {
    socket_path_named(".socket.sock")
}

/// Resolve a Hyprland IPC socket by file name.
///
/// Hyprland exposes two sockets in the same directory: the request/response command socket
/// (`.socket.sock`) and the streaming event socket (`.socket2.sock`). Both share the layout
/// resolution: modern `$XDG_RUNTIME_DIR/hypr/$HIS/<file>` (Hyprland ≥ 0.42) with a fallback to
/// the legacy `/tmp/hypr/$HIS/<file>` for older builds.
fn socket_path_named(file: &str) -> Result<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .map_err(|_| anyhow!("{}", crate::i18n::fl!("error-not-under-hyprland")))?;
    let candidates = [
        std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .map(|d| PathBuf::from(d).join("hypr").join(&sig).join(file)),
        Some(PathBuf::from(format!("/tmp/hypr/{sig}/{file}"))),
    ];
    for c in candidates.into_iter().flatten() {
        if c.exists() {
            return Ok(c);
        }
    }
    bail!(
        "Hyprland IPC socket not found under $XDG_RUNTIME_DIR/hypr/{sig}/{file} or /tmp/hypr/{sig}/{file}",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    #[rstest]
    #[case("focusedmon>>DP-1,1", Some("DP-1"))]
    #[case("focusedmon>>HDMI-A-1,web", Some("HDMI-A-1"))]
    #[case("focusedmonv2>>HDMI-A-1,2", Some("HDMI-A-1"))]
    #[case("focusedmonv2>>eDP-1,3", Some("eDP-1"))]
    // Unrelated events are ignored.
    #[case("activewindow>>kitty,bash", None)]
    #[case("workspace>>2", None)]
    // A prefix collision must not match (only the exact `focusedmon`/`focusedmonv2` events).
    #[case("focusedmonfoo>>DP-1,1", None)]
    // Malformed: empty monitor name.
    #[case("focusedmon>>,1", None)]
    #[case("focusedmon>>", None)]
    fn parses_focused_monitor_events(#[case] line: &str, #[case] expected: Option<&str>) {
        assert_eq!(parse_focused_monitor_event(line), expected);
    }

    fn win(id: &str, x: i32, y: i32, w: u32, h: u32, mapped: bool, hidden: bool) -> WmWindow {
        WmWindow {
            id: id.into(),
            title: "t".into(),
            class: "c".into(),
            at: Some((x, y)),
            size: Some((w, h)),
            monitor: "0".into(),
            workspace_id: 1,
            mapped,
            hidden,
        }
    }

    #[test]
    fn window_at_returns_topmost_match() {
        // Hyprland returns clients front-to-back; the first match wins.
        let clients = vec![
            win("top", 0, 0, 100, 100, true, false),
            win("bottom", 0, 0, 200, 200, true, false),
        ];
        assert_eq!(
            super::super::window_at(&clients, 50, 50).map(|w| w.id.as_str()),
            Some("top")
        );
    }

    #[test]
    fn window_at_skips_unmapped_and_hidden() {
        let clients = vec![
            win("unmapped", 0, 0, 100, 100, false, false),
            win("hidden", 0, 0, 100, 100, true, true),
            win("visible", 0, 0, 100, 100, true, false),
        ];
        assert_eq!(
            super::super::window_at(&clients, 10, 10).map(|w| w.id.as_str()),
            Some("visible")
        );
    }

    #[test]
    fn window_at_returns_none_when_point_outside() {
        let clients = vec![win("only", 0, 0, 10, 10, true, false)];
        assert_eq!(
            super::super::window_at(&clients, 100, 100).map(|w| w.id.as_str()),
            None
        );
    }

    #[test]
    fn window_at_picks_correct_one_for_disjoint_clients() {
        let clients = vec![
            win("left", 0, 0, 100, 100, true, false),
            win("right", 200, 0, 100, 100, true, false),
        ];
        assert_eq!(
            super::super::window_at(&clients, 250, 50).map(|w| w.id.as_str()),
            Some("right")
        );
        assert_eq!(
            super::super::window_at(&clients, 50, 50).map(|w| w.id.as_str()),
            Some("left")
        );
    }

    #[test]
    fn active_window_rect_maps_position_and_size() {
        let w = ActiveWindow {
            title: "term".into(),
            class: "kitty".into(),
            at: Some((-10, 25)),
            size: Some((800, 600)),
            monitor: "1".into(),
        };
        assert_eq!(
            w.rect(),
            Some(crate::capture::region::Rect {
                x: -10,
                y: 25,
                w: 800,
                h: 600
            })
        );
    }

    /// Force a hermetic environment for the socket-resolution tests. Safe because nextest
    /// runs every test in its own process.
    fn set_env(sig: Option<&str>, runtime_dir: Option<&std::path::Path>) {
        unsafe {
            match sig {
                Some(v) => std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", v),
                None => std::env::remove_var("HYPRLAND_INSTANCE_SIGNATURE"),
            }
            match runtime_dir {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }

    #[test]
    fn socket_path_requires_the_instance_signature() {
        set_env(None, None);
        let err = socket_path_named(".socket.sock").unwrap_err();
        assert!(
            err.to_string().contains("HYPRLAND_INSTANCE_SIGNATURE"),
            "unexpected error: {err}"
        );
    }

    #[rstest]
    #[case(".socket.sock")]
    #[case(".socket2.sock")]
    fn socket_path_prefers_the_xdg_runtime_dir(#[case] file: &str) {
        let runtime = tempfile::tempdir().unwrap();
        let sig = "deadbeef";
        let dir = runtime.path().join("hypr").join(sig);
        std::fs::create_dir_all(&dir).unwrap();
        let expected = dir.join(file);
        std::fs::write(&expected, b"").unwrap();

        set_env(Some(sig), Some(runtime.path()));
        assert_eq!(socket_path_named(file).unwrap(), expected);
    }

    #[test]
    fn socket_path_reports_both_candidates_when_neither_exists() {
        let runtime = tempfile::tempdir().unwrap();
        // A signature that cannot plausibly exist under the legacy /tmp/hypr layout either.
        let sig = "snypr-test-missing-instance";
        set_env(Some(sig), Some(runtime.path()));

        let err = socket_path_named(".socket.sock").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(sig), "unexpected error: {msg}");
        assert!(msg.contains("/tmp/hypr/"), "unexpected error: {msg}");
    }

    /// Prepare a hermetic `$XDG_RUNTIME_DIR/hypr/$sig/` directory and point the env at it.
    /// Returns the directory the two sockets live in.
    fn fake_instance_dir(sig: &str) -> (tempfile::TempDir, PathBuf) {
        let runtime = tempfile::tempdir().unwrap();
        let dir = runtime.path().join("hypr").join(sig);
        std::fs::create_dir_all(&dir).unwrap();
        set_env(Some(sig), Some(runtime.path()));
        (runtime, dir)
    }

    /// Accept one connection on the fake `.socket.sock`, read the command to EOF (ignoring
    /// its content — every query is a fixed literal like `j/activewindow`), and reply with
    /// `response`. Mirrors Hyprland's plain-text (unframed) command socket protocol.
    async fn serve_command(mut stream: UnixStream, response: &str) {
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn hyprland_backend_name_and_socket_path_delegate() {
        let (_runtime, dir) = fake_instance_dir("deadbeef");
        // socket_path() only needs the file to exist; no listener required for this one.
        std::fs::write(dir.join(".socket.sock"), b"").unwrap();

        let backend: &dyn WmBackend = &Hyprland;
        assert_eq!(backend.name(), "Hyprland");
        assert_eq!(backend.socket_path().unwrap(), dir.join(".socket.sock"));
    }

    #[tokio::test]
    async fn hyprland_backend_active_window_delegates_to_the_free_function() {
        let (_runtime, dir) = fake_instance_dir("deadbeef");
        let listener = tokio::net::UnixListener::bind(dir.join(".socket.sock")).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_command(
                stream,
                r#"{"title":"term","class":"kitty","at":[1,2],"size":[3,4],"monitor":0}"#,
            )
            .await;
        });

        let backend: &dyn WmBackend = &Hyprland;
        let win = backend.active_window().await.unwrap();
        assert_eq!(win.title, "term");
        assert_eq!(win.class, "kitty");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn hyprland_backend_clients_delegates_to_the_free_function() {
        let (_runtime, dir) = fake_instance_dir("deadbeef");
        let listener = tokio::net::UnixListener::bind(dir.join(".socket.sock")).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_command(
                stream,
                r#"[{"address":"0x1","title":"t","class":"c","at":[0,0],"size":[10,10],"monitor":0,"workspace":{"id":1},"mapped":true,"hidden":false}]"#,
            )
            .await;
        });

        let backend: &dyn WmBackend = &Hyprland;
        let list = backend.clients().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "0x1");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn hyprland_backend_focused_output_delegates_to_the_free_function() {
        let (_runtime, dir) = fake_instance_dir("deadbeef");
        let listener = tokio::net::UnixListener::bind(dir.join(".socket.sock")).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_command(
                stream,
                r#"[{"name":"DP-1","focused":false},{"name":"HDMI-A-1","focused":true}]"#,
            )
            .await;
        });

        let backend: &dyn WmBackend = &Hyprland;
        assert_eq!(backend.focused_output().await.unwrap(), "HDMI-A-1");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn hyprland_backend_subscribe_focus_publishes_events_from_the_event_socket() {
        let (_runtime, dir) = fake_instance_dir("deadbeef");
        let listener = tokio::net::UnixListener::bind(dir.join(".socket2.sock")).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"focusedmon>>DP-1,1\n").await.unwrap();
        });

        let handle = tokio::runtime::Handle::current();
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let backend: &dyn WmBackend = &Hyprland;
        let mut rx = backend.subscribe_focus(&handle, shutdown_rx);
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), Some("DP-1".to_string()));
        server.await.unwrap();
    }
}
