//! Niri IPC backend (active window, window list, focused output).
//!
//! Connects to Niri's IPC socket (`$NIRI_SOCKET`) and speaks its JSON-line protocol: write one
//! JSON request encoded on a single line + `\n`, read one JSON reply on a single line,
//! `{"Ok": <Response>}` / `{"Err": "<message>"}`. See
//! <https://github.com/niri-wm/niri/wiki/IPC>.
//!
//! Niri's `Windows`/`FocusedWindow` responses carry no absolute on-screen rect — only a tile
//! position *relative to the current view of the window's workspace*
//! (`layout.tile_pos_in_workspace_view`) and a tile size (`layout.tile_size`). Resolving an
//! absolute logical-pixel rect requires combining three requests: the window's `workspace_id`
//! (`Windows`/`FocusedWindow`) → that workspace's `output` name (`Workspaces`) → that output's
//! logical position (`Outputs`). [`resolve_rect`] does this; it returns `None` for the position
//! when any piece is missing (e.g. the window isn't currently in its workspace's view).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, watch};

use super::{ActiveWindow, WmBackend, WmWindow};

/// Niri [`WmBackend`] implementation.
pub struct Niri;

#[async_trait::async_trait]
impl WmBackend for Niri {
    fn name(&self) -> &'static str {
        "Niri"
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
        focused_output().await
    }

    fn subscribe_focus(
        &self,
        handle: &tokio::runtime::Handle,
        shutdown: oneshot::Receiver<()>,
    ) -> watch::Receiver<Option<String>> {
        subscribe_focus(handle, shutdown)
    }
}

// ---- Wire protocol types ---------------------------------------------------------------

#[derive(Debug, Serialize)]
enum Request {
    FocusedWindow,
    FocusedOutput,
    Windows,
    Outputs,
    Workspaces,
    EventStream,
}

#[derive(Debug, Deserialize)]
enum Reply {
    Ok(Response),
    Err(String),
}

#[derive(Debug, Deserialize)]
enum Response {
    Handled,
    FocusedWindow(Option<WindowRaw>),
    FocusedOutput(Option<OutputRaw>),
    Windows(Vec<WindowRaw>),
    Outputs(HashMap<String, OutputRaw>),
    Workspaces(Vec<WorkspaceRaw>),
}

#[derive(Debug, Deserialize)]
struct WindowRaw {
    id: u64,
    title: Option<String>,
    app_id: Option<String>,
    workspace_id: Option<u64>,
    layout: WindowLayoutRaw,
}

#[derive(Debug, Deserialize)]
struct WindowLayoutRaw {
    tile_size: (f64, f64),
    tile_pos_in_workspace_view: Option<(f64, f64)>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceRaw {
    id: u64,
    output: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OutputRaw {
    name: String,
    logical: Option<LogicalOutputRaw>,
}

#[derive(Debug, Deserialize)]
struct LogicalOutputRaw {
    x: i32,
    y: i32,
}

/// Resolve the Niri IPC socket path from `$NIRI_SOCKET`.
fn socket_path() -> Result<PathBuf> {
    std::env::var_os("NIRI_SOCKET")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("{}", crate::i18n::fl!("error-not-under-niri")))
}

/// Send `request` over a fresh connection and return the decoded `Response`.
async fn query(request: &Request) -> Result<Response> {
    let path = socket_path().context("locating Niri IPC socket")?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to Niri IPC at {}", path.display()))?;
    let mut payload = serde_json::to_vec(request).context("encoding Niri IPC request")?;
    payload.push(b'\n');
    stream
        .write_all(&payload)
        .await
        .context("writing Niri IPC request")?;
    stream
        .shutdown()
        .await
        .context("closing Niri IPC write side")?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .context("reading Niri IPC response")?;
    let reply: Reply = serde_json::from_str(line.trim_end())
        .with_context(|| format!("parsing Niri IPC response: {line}"))?;
    match reply {
        Reply::Ok(response) => Ok(response),
        Reply::Err(message) => bail!("Niri IPC request failed: {message}"),
    }
}

async fn request_focused_window() -> Result<Option<WindowRaw>> {
    match query(&Request::FocusedWindow).await? {
        Response::FocusedWindow(w) => Ok(w),
        other => bail!("unexpected Niri IPC response to FocusedWindow: {other:?}"),
    }
}

async fn request_focused_output_raw() -> Result<Option<OutputRaw>> {
    match query(&Request::FocusedOutput).await? {
        Response::FocusedOutput(o) => Ok(o),
        other => bail!("unexpected Niri IPC response to FocusedOutput: {other:?}"),
    }
}

async fn request_windows() -> Result<Vec<WindowRaw>> {
    match query(&Request::Windows).await? {
        Response::Windows(w) => Ok(w),
        other => bail!("unexpected Niri IPC response to Windows: {other:?}"),
    }
}

async fn request_workspaces() -> Result<Vec<WorkspaceRaw>> {
    match query(&Request::Workspaces).await? {
        Response::Workspaces(w) => Ok(w),
        other => bail!("unexpected Niri IPC response to Workspaces: {other:?}"),
    }
}

async fn request_outputs() -> Result<HashMap<String, OutputRaw>> {
    match query(&Request::Outputs).await? {
        Response::Outputs(o) => Ok(o),
        other => bail!("unexpected Niri IPC response to Outputs: {other:?}"),
    }
}

/// Map each workspace id to its output name (workspaces with no output, e.g. because no
/// outputs are connected, are omitted).
fn workspace_outputs(workspaces: &[WorkspaceRaw]) -> HashMap<u64, String> {
    workspaces
        .iter()
        .filter_map(|w| w.output.clone().map(|o| (w.id, o)))
        .collect()
}

/// `(at, size)`, matching [`WmWindow`]/[`ActiveWindow`]'s own geometry fields.
type ResolvedGeometry = (Option<(i32, i32)>, Option<(u32, u32)>);

/// Resolve a window's absolute logical-pixel position from its tile layout and its workspace's
/// output. The tile size is always known (Niri reports it unconditionally), but the position
/// is `None` when either piece needed to place it absolutely is missing: the window isn't
/// currently in its workspace's view, its workspace has no output, or the output has no
/// logical geometry.
fn resolve_rect(
    layout: &WindowLayoutRaw,
    output_name: Option<&str>,
    outputs: &HashMap<String, OutputRaw>,
) -> ResolvedGeometry {
    let size = Some((
        layout.tile_size.0.round().max(0.0) as u32,
        layout.tile_size.1.round().max(0.0) as u32,
    ));
    let (Some((vx, vy)), Some(name)) = (layout.tile_pos_in_workspace_view, output_name) else {
        return (None, size);
    };
    let Some(logical) = outputs.get(name).and_then(|o| o.logical.as_ref()) else {
        return (None, size);
    };
    (
        Some((vx.round() as i32 + logical.x, vy.round() as i32 + logical.y)),
        size,
    )
}

async fn active_window() -> Result<ActiveWindow> {
    let window = request_focused_window()
        .await?
        .ok_or_else(|| anyhow!("{}", crate::i18n::fl!("error-no-active-window")))?;
    let workspaces = request_workspaces().await?;
    let outputs = request_outputs().await?;
    let ws_outputs = workspace_outputs(&workspaces);
    let output_name = window
        .workspace_id
        .and_then(|id| ws_outputs.get(&id))
        .cloned();
    let (at, size) = resolve_rect(&window.layout, output_name.as_deref(), &outputs);
    Ok(ActiveWindow {
        title: window.title.unwrap_or_default(),
        class: window.app_id.unwrap_or_default(),
        at,
        size,
        monitor: output_name.unwrap_or_default(),
    })
}

/// List every Niri window. Order is whatever Niri's `Windows` response returns (no documented
/// z-order guarantee) — a best-effort limitation, same as Sway's.
async fn clients() -> Result<Vec<WmWindow>> {
    let windows = request_windows().await?;
    let workspaces = request_workspaces().await?;
    let outputs = request_outputs().await?;
    let ws_outputs = workspace_outputs(&workspaces);
    Ok(windows
        .into_iter()
        .map(|w| {
            let output_name = w.workspace_id.and_then(|id| ws_outputs.get(&id)).cloned();
            let (at, size) = resolve_rect(&w.layout, output_name.as_deref(), &outputs);
            WmWindow {
                id: w.id.to_string(),
                title: w.title.unwrap_or_default(),
                class: w.app_id.unwrap_or_default(),
                at,
                size,
                monitor: output_name.unwrap_or_default(),
                workspace_id: w.workspace_id.map(|id| id as i64).unwrap_or(-1),
                mapped: true,
                // Unresolved geometry means we can't place this window; hide it from
                // hit-testing rather than reporting a window with an unknown location as
                // clickable.
                hidden: at.is_none(),
            }
        })
        .collect())
}

async fn focused_output() -> Result<String> {
    request_focused_output_raw()
        .await?
        .map(|o| o.name)
        .ok_or_else(|| anyhow!("{}", crate::i18n::fl!("error-no-focused-monitor")))
}

/// Subscribe to Niri focused-output changes.
///
/// Sends `EventStream`, consumes the `Handled` ack, then on *every* subsequent event line
/// re-queries [`focused_output`] over a fresh connection rather than parsing the full `Event`
/// schema — mirrors Sway's documented trade-off (simple and robust over exact). Note Niri's
/// event stream also replays the full current state as an initial burst of events right after
/// the ack, causing a handful of redundant re-queries at subscribe time; acceptable since this
/// only powers the best-effort "toolbar follows focus" feature.
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
        tracing::debug!("Niri focus subscription stopped");
    });
    rx
}

async fn pump_focus_events(tx: &watch::Sender<Option<String>>) {
    let path = match socket_path() {
        Ok(p) => p,
        Err(err) => {
            tracing::debug!(error = ?err, "Niri IPC socket unavailable; focus-follow disabled");
            return;
        }
    };
    let mut stream = match UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = ?err, path = %path.display(), "connecting to Niri IPC socket failed");
            return;
        }
    };
    let mut payload = match serde_json::to_vec(&Request::EventStream) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = ?err, "encoding Niri EventStream request failed");
            return;
        }
    };
    payload.push(b'\n');
    if let Err(err) = stream.write_all(&payload).await {
        tracing::warn!(error = ?err, "subscribing to Niri event stream failed");
        return;
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    // Consume the EventStream ack (`{"Ok":"Handled"}`) before reading events.
    if let Err(err) = reader.read_line(&mut line).await {
        tracing::warn!(error = ?err, "reading Niri EventStream ack failed");
        return;
    }
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF: compositor closed the socket.
            Ok(_) => match focused_output().await {
                Ok(name) => {
                    if tx.send(Some(name)).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    tracing::debug!(error = %err, "querying focused Niri output after an event failed");
                }
            },
            Err(err) => {
                tracing::warn!(error = ?err, "reading Niri event stream failed");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::{Value, json};
    use tokio::net::UnixListener;

    /// Force a hermetic environment for the socket-resolution tests. Safe because nextest
    /// runs every test in its own process.
    fn set_env(sock: Option<&std::path::Path>) {
        unsafe {
            match sock {
                Some(v) => std::env::set_var("NIRI_SOCKET", v),
                None => std::env::remove_var("NIRI_SOCKET"),
            }
        }
    }

    #[test]
    fn socket_path_requires_niri_socket() {
        set_env(None);
        let err = socket_path().unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    /// Accept one connection on `listener`, read+discard the request line, and reply with
    /// `{"Ok": response}`.
    async fn serve_once(listener: &UnixListener, response: &Value) {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).await.unwrap();
        let mut stream = reader.into_inner();
        let body = json!({ "Ok": response }).to_string();
        stream.write_all(body.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
    }

    /// Serve `responses` in order, each on its own accepted connection — every Niri query
    /// opens a fresh connection, so multi-request free functions (`active_window`, `clients`)
    /// see one `accept()` per request.
    async fn serve_sequence(listener: UnixListener, responses: Vec<Value>) {
        for response in responses {
            serve_once(&listener, &response).await;
        }
    }

    fn sample_window() -> Value {
        json!({
            "id": 7,
            "title": "term",
            "app_id": "kitty",
            "workspace_id": 42,
            "layout": {
                "tile_size": [800.0, 600.0],
                "tile_pos_in_workspace_view": [10.0, 20.0],
            }
        })
    }

    fn sample_workspaces() -> Value {
        json!([{ "id": 42, "output": "eDP-1" }])
    }

    fn sample_outputs() -> Value {
        json!({ "eDP-1": { "name": "eDP-1", "logical": { "x": 1920, "y": 0 } } })
    }

    #[test]
    fn resolve_rect_combines_view_position_and_output_offset() {
        let layout: WindowLayoutRaw =
            serde_json::from_value(sample_window()["layout"].clone()).unwrap();
        let outputs: HashMap<String, OutputRaw> = serde_json::from_value(sample_outputs()).unwrap();
        let (at, size) = resolve_rect(&layout, Some("eDP-1"), &outputs);
        assert_eq!(at, Some((1930, 20)));
        assert_eq!(size, Some((800, 600)));
    }

    #[test]
    fn resolve_rect_position_is_none_when_tile_pos_is_unset() {
        let layout = WindowLayoutRaw {
            tile_size: (800.0, 600.0),
            tile_pos_in_workspace_view: None,
        };
        let outputs: HashMap<String, OutputRaw> = serde_json::from_value(sample_outputs()).unwrap();
        let (at, size) = resolve_rect(&layout, Some("eDP-1"), &outputs);
        // The tile size is still reported even when the position can't be resolved.
        assert_eq!(at, None);
        assert_eq!(size, Some((800, 600)));
    }

    #[test]
    fn resolve_rect_position_is_none_when_output_is_unknown() {
        let layout: WindowLayoutRaw =
            serde_json::from_value(sample_window()["layout"].clone()).unwrap();
        let outputs: HashMap<String, OutputRaw> = HashMap::new();
        let (at, size) = resolve_rect(&layout, Some("eDP-1"), &outputs);
        assert_eq!(at, None);
        assert_eq!(size, Some((800, 600)));
    }

    #[test]
    fn workspace_outputs_skips_workspaces_without_an_output() {
        let workspaces: Vec<WorkspaceRaw> = serde_json::from_value(json!([
            { "id": 1, "output": "eDP-1" },
            { "id": 2, "output": null },
        ]))
        .unwrap();
        let map = workspace_outputs(&workspaces);
        assert_eq!(map.get(&1).map(String::as_str), Some("eDP-1"));
        assert_eq!(map.get(&2), None);
    }

    #[tokio::test]
    async fn focused_output_parses_focused_output_reply() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("niri.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        set_env(Some(&sock));

        let response =
            json!({ "FocusedOutput": { "name": "DP-2", "logical": { "x": 0, "y": 0 } } });
        let server = tokio::spawn(async move { serve_once(&listener, &response).await });

        let name = focused_output().await.unwrap();
        assert_eq!(name, "DP-2");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn focused_output_errors_when_nothing_is_focused() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("niri.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        set_env(Some(&sock));

        let response = json!({ "FocusedOutput": null });
        let server = tokio::spawn(async move { serve_once(&listener, &response).await });

        let err = focused_output().await.unwrap_err();
        assert!(!err.to_string().is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn active_window_resolves_absolute_position_from_workspace_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("niri.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        set_env(Some(&sock));

        let responses = vec![
            json!({ "FocusedWindow": sample_window() }),
            json!({ "Workspaces": sample_workspaces() }),
            json!({ "Outputs": sample_outputs() }),
        ];
        let server = tokio::spawn(async move { serve_sequence(listener, responses).await });

        let win = active_window().await.unwrap();
        assert_eq!(win.title, "term");
        assert_eq!(win.class, "kitty");
        assert_eq!(win.monitor, "eDP-1");
        assert_eq!(win.at, Some((1930, 20)));
        assert_eq!(win.size, Some((800, 600)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn clients_lists_windows_with_resolved_geometry_and_marks_unresolved_ones_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("niri.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        set_env(Some(&sock));

        let unresolvable = json!({
            "id": 9,
            "title": "orphan",
            "app_id": "orphan",
            "workspace_id": null,
            "layout": {
                "tile_size": [100.0, 100.0],
                "tile_pos_in_workspace_view": null,
            }
        });
        let windows = json!([sample_window(), unresolvable]);
        let responses = vec![
            json!({ "Windows": windows }),
            json!({ "Workspaces": sample_workspaces() }),
            json!({ "Outputs": sample_outputs() }),
        ];
        let server = tokio::spawn(async move { serve_sequence(listener, responses).await });

        let list = clients().await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "7");
        assert_eq!(list[0].at, Some((1930, 20)));
        assert_eq!(list[0].size, Some((800, 600)));
        assert!(!list[0].hidden);
        assert_eq!(list[1].id, "9");
        assert_eq!(list[1].at, None);
        assert_eq!(list[1].size, Some((100, 100)));
        assert!(list[1].hidden);
        assert!(list.iter().all(|w| w.mapped));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn niri_backend_name_and_socket_path_delegate() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("niri.sock");
        std::fs::write(&sock, b"").unwrap();
        set_env(Some(&sock));

        let backend: &dyn WmBackend = &Niri;
        assert_eq!(backend.name(), "Niri");
        assert_eq!(backend.socket_path().unwrap(), sock);
    }

    #[tokio::test]
    async fn niri_backend_active_window_delegates_to_the_free_function() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("niri.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        set_env(Some(&sock));

        let responses = vec![
            json!({ "FocusedWindow": sample_window() }),
            json!({ "Workspaces": sample_workspaces() }),
            json!({ "Outputs": sample_outputs() }),
        ];
        let server = tokio::spawn(async move { serve_sequence(listener, responses).await });

        let backend: &dyn WmBackend = &Niri;
        let win = backend.active_window().await.unwrap();
        assert_eq!(win.title, "term");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn niri_backend_clients_delegates_to_the_free_function() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("niri.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        set_env(Some(&sock));

        let windows = json!([sample_window()]);
        let responses = vec![
            json!({ "Windows": windows }),
            json!({ "Workspaces": sample_workspaces() }),
            json!({ "Outputs": sample_outputs() }),
        ];
        let server = tokio::spawn(async move { serve_sequence(listener, responses).await });

        let backend: &dyn WmBackend = &Niri;
        let list = backend.clients().await.unwrap();
        assert_eq!(list.len(), 1);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn niri_backend_focused_output_delegates_to_the_free_function() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("niri.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        set_env(Some(&sock));

        let response =
            json!({ "FocusedOutput": { "name": "eDP-1", "logical": { "x": 0, "y": 0 } } });
        let server = tokio::spawn(async move { serve_once(&listener, &response).await });

        let backend: &dyn WmBackend = &Niri;
        assert_eq!(backend.focused_output().await.unwrap(), "eDP-1");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn subscribe_focus_publishes_the_focused_output_after_an_event() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("niri.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        set_env(Some(&sock));

        let server = tokio::spawn(async move {
            // First connection: the EventStream session pump_focus_events opens and keeps
            // open.
            let (sub_conn, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(sub_conn);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).await.unwrap();
            assert_eq!(request_line.trim_end(), "\"EventStream\"");
            let mut sub_conn = reader.into_inner();
            // Ack the EventStream request, then push one fake event line to trigger a
            // re-query.
            sub_conn.write_all(b"{\"Ok\":\"Handled\"}\n").await.unwrap();
            sub_conn
                .write_all(b"{\"WindowFocusChanged\":{\"id\":1}}\n")
                .await
                .unwrap();

            // Second connection: the fresh `focused_output()` query triggered by that event.
            let response =
                json!({ "FocusedOutput": { "name": "DP-3", "logical": { "x": 0, "y": 0 } } });
            serve_once(&listener, &response).await;
            // Dropping the listener here lets pump_focus_events' next read see EOF and stop
            // the task cleanly once the test is done observing the update.
        });

        let handle = tokio::runtime::Handle::current();
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let mut rx = subscribe_focus(&handle, shutdown_rx);
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), Some("DP-3".to_string()));
        server.await.unwrap();
    }
}
