//! Sway IPC backend (active window, window list, focused output).
//!
//! Connects directly to Sway's IPC socket (`$SWAYSOCK`, always set by Sway itself — no path
//! guessing needed, unlike Hyprland) and speaks the binary i3ipc protocol described in
//! `sway-ipc(7)`: a 6-byte `i3-ipc` magic, a `u32` little-endian payload length, a `u32`
//! little-endian message type, then a JSON payload. Replies mirror the request's message type;
//! events set the high bit (`0x8000_0000`) of the type.
//!
//! Sway has no single global z-order list like Hyprland's `clients()` (windows live in a tiling
//! tree, not a stacking list). [`clients`] approximates one: floating windows are listed in
//! `floating_nodes` order (last = topmost, matching how Sway stacks them), followed by tiled
//! windows in tree order (acceptable since tiled windows rarely overlap). This is a known,
//! documented limitation relative to Hyprland's exact z-order.

use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, watch};

use super::{ActiveWindow, WmBackend, WmWindow};

/// Sway [`WmBackend`] implementation.
pub struct Sway;

#[async_trait::async_trait]
impl WmBackend for Sway {
    fn name(&self) -> &'static str {
        "Sway"
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

// ---- i3ipc wire protocol ---------------------------------------------------------------

const MAGIC: &[u8; 6] = b"i3-ipc";
const HEADER_LEN: usize = 6 + 4 + 4;

pub(crate) const SUBSCRIBE: u32 = 2;
pub(crate) const GET_OUTPUTS: u32 = 3;
pub(crate) const GET_TREE: u32 = 4;

/// Read one framed i3ipc message (`(type, payload)`) from `stream`.
async fn read_message(stream: &mut UnixStream) -> Result<(u32, Vec<u8>)> {
    let mut header = [0u8; HEADER_LEN];
    stream
        .read_exact(&mut header)
        .await
        .context("reading Sway IPC message header")?;
    if &header[0..6] != MAGIC {
        bail!("Sway IPC response did not start with the i3-ipc magic");
    }
    let len = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
    let msg_type = u32::from_le_bytes(header[10..14].try_into().unwrap());
    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .await
        .context("reading Sway IPC message payload")?;
    Ok((msg_type, payload))
}

/// Write one framed i3ipc message to `stream`.
async fn write_message(stream: &mut UnixStream, msg_type: u32, payload: &[u8]) -> Result<()> {
    let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&msg_type.to_le_bytes());
    buf.extend_from_slice(payload);
    stream
        .write_all(&buf)
        .await
        .context("writing Sway IPC message")
}

/// Send a request of `msg_type` with `payload` and return the reply payload bytes.
async fn query(msg_type: u32, payload: &[u8]) -> Result<Vec<u8>> {
    let path = socket_path().context("locating Sway IPC socket")?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to Sway IPC at {}", path.display()))?;
    write_message(&mut stream, msg_type, payload).await?;
    let (_reply_type, body) = read_message(&mut stream).await?;
    Ok(body)
}

async fn query_json(msg_type: u32, payload: &[u8]) -> Result<Value> {
    let body = query(msg_type, payload).await?;
    serde_json::from_slice(&body).context("parsing Sway IPC JSON response")
}

/// Resolve the Sway IPC socket path from `$SWAYSOCK`.
///
/// Unlike Hyprland, Sway always points this at the live socket for the running instance — no
/// candidate-path guessing is needed.
fn socket_path() -> Result<PathBuf> {
    std::env::var_os("SWAYSOCK")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("{}", crate::i18n::fl!("error-not-under-sway")))
}

// ---- Tree walking -----------------------------------------------------------------------

/// A leaf window node found while walking `GET_TREE`, plus the ancestor context (output name,
/// workspace id) needed to fill in [`WmWindow`]/[`ActiveWindow`].
struct FoundWindow<'a> {
    node: &'a Value,
    output: String,
    workspace_id: i64,
}

/// `true` if `node` is a leaf container that hosts an actual client window (as opposed to a
/// pure split/grouping container). Sway sets `pid` on containers that wrap a real surface.
fn is_window_node(node: &Value) -> bool {
    matches!(
        node.get("type").and_then(Value::as_str),
        Some("con") | Some("floating_con")
    ) && node.get("pid").is_some_and(|v| !v.is_null())
}

/// Recursively walk a `GET_TREE` node, collecting every window leaf in best-effort
/// front-to-back order: floating windows (topmost `floating_nodes` entry first) before tiled
/// windows (tree order).
///
/// `output`/`workspace_id` track the nearest enclosing output/workspace as we descend; `root`
/// nodes contain output nodes, output nodes contain (among others) workspace nodes, workspace
/// nodes contain `nodes` (tiled) and `floating_nodes` (floating) container trees.
fn collect_windows<'a>(
    node: &'a Value,
    output: &str,
    workspace_id: i64,
    out: &mut Vec<FoundWindow<'a>>,
) {
    let node_type = node.get("type").and_then(Value::as_str);
    let next_output = if node_type == Some("output") {
        node.get("name")
            .and_then(Value::as_str)
            .unwrap_or(output)
            .to_owned()
    } else {
        output.to_owned()
    };
    let next_workspace_id = if node_type == Some("workspace") {
        node.get("id")
            .and_then(Value::as_i64)
            .unwrap_or(workspace_id)
    } else {
        workspace_id
    };

    // Floating windows first (best-effort "on top"), then tiled — see module doc.
    if let Some(floating) = node.get("floating_nodes").and_then(Value::as_array) {
        // Sway appends newly-focused floating windows to the end; treat last-first as
        // topmost-first.
        for child in floating.iter().rev() {
            if is_window_node(child) {
                out.push(FoundWindow {
                    node: child,
                    output: next_output.clone(),
                    workspace_id: next_workspace_id,
                });
            } else {
                collect_windows(child, &next_output, next_workspace_id, out);
            }
        }
    }
    if let Some(children) = node.get("nodes").and_then(Value::as_array) {
        for child in children {
            if is_window_node(child) {
                out.push(FoundWindow {
                    node: child,
                    output: next_output.clone(),
                    workspace_id: next_workspace_id,
                });
            } else {
                collect_windows(child, &next_output, next_workspace_id, out);
            }
        }
    }
}

/// Find the single focused window leaf anywhere under `node`, tracking ancestor context the
/// same way [`collect_windows`] does.
fn find_focused<'a>(node: &'a Value, output: &str, workspace_id: i64) -> Option<FoundWindow<'a>> {
    let node_type = node.get("type").and_then(Value::as_str);
    let next_output = if node_type == Some("output") {
        node.get("name")
            .and_then(Value::as_str)
            .unwrap_or(output)
            .to_owned()
    } else {
        output.to_owned()
    };
    let next_workspace_id = if node_type == Some("workspace") {
        node.get("id")
            .and_then(Value::as_i64)
            .unwrap_or(workspace_id)
    } else {
        workspace_id
    };

    if is_window_node(node) && node.get("focused").and_then(Value::as_bool) == Some(true) {
        return Some(FoundWindow {
            node,
            output: next_output,
            workspace_id: next_workspace_id,
        });
    }

    let children = node
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            node.get("floating_nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        );
    for child in children {
        if let Some(found) = find_focused(child, &next_output, next_workspace_id) {
            return Some(found);
        }
    }
    None
}

/// Extract `(title, class, at, size)` from a window-leaf node.
///
/// `name` is the window title. The window class comes from `app_id` (native Wayland clients)
/// or falls back to `window_properties.class` (XWayland clients, via xwayland-satellite or
/// Sway's built-in XWayland support); Sway reports geometry in `rect`, already in absolute
/// (not workspace-relative) logical coordinates.
fn window_fields(node: &Value) -> (String, String, (i32, i32), (u32, u32)) {
    let title = node
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let class = node
        .get("app_id")
        .and_then(Value::as_str)
        .or_else(|| {
            node.get("window_properties")
                .and_then(|p| p.get("class"))
                .and_then(Value::as_str)
        })
        .unwrap_or_default()
        .to_owned();
    let rect = node.get("rect");
    let x = rect
        .and_then(|r| r.get("x"))
        .and_then(Value::as_i64)
        .unwrap_or(0) as i32;
    let y = rect
        .and_then(|r| r.get("y"))
        .and_then(Value::as_i64)
        .unwrap_or(0) as i32;
    let w = rect
        .and_then(|r| r.get("width"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as u32;
    let h = rect
        .and_then(|r| r.get("height"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as u32;
    (title, class, (x, y), (w, h))
}

async fn active_window() -> Result<ActiveWindow> {
    let tree = query_json(GET_TREE, b"").await?;
    let found = find_focused(&tree, "", 0)
        .ok_or_else(|| anyhow!("{}", crate::i18n::fl!("error-no-active-window")))?;
    let (title, class, at, size) = window_fields(found.node);
    Ok(ActiveWindow {
        title,
        class,
        at,
        size,
        monitor: found.output,
    })
}

async fn clients() -> Result<Vec<WmWindow>> {
    let tree = query_json(GET_TREE, b"").await?;
    let mut found = Vec::new();
    collect_windows(&tree, "", 0, &mut found);
    Ok(found
        .into_iter()
        .map(|f| {
            let (title, class, at, size) = window_fields(f.node);
            let id = f
                .node
                .get("id")
                .and_then(Value::as_i64)
                .map(|id| id.to_string())
                .unwrap_or_default();
            let visible = f
                .node
                .get("visible")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            WmWindow {
                id,
                title,
                class,
                at,
                size,
                monitor: f.output,
                workspace_id: f.workspace_id,
                mapped: true,
                hidden: !visible,
            }
        })
        .collect())
}

async fn focused_output() -> Result<String> {
    let outputs = query_json(GET_OUTPUTS, b"").await?;
    let outputs = outputs
        .as_array()
        .ok_or_else(|| anyhow!("parsing Sway GET_OUTPUTS response: not an array"))?;
    outputs
        .iter()
        .find(|o| o.get("focused").and_then(Value::as_bool) == Some(true))
        .and_then(|o| o.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("{}", crate::i18n::fl!("error-no-focused-monitor")))
}

/// Subscribe to Sway focused-output changes.
///
/// Sway's event stream does not carry the focused output's name inline the way Hyprland's
/// `focusedmon` event does, so each relevant `workspace` focus event triggers a fresh
/// [`focused_output`] query over a short-lived second connection. This trades a little extra
/// latency per focus change for a simple, robust implementation — acceptable since this powers
/// only the "toolbar follows focus" cosmetic feature, which is best-effort by design.
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
        tracing::debug!("Sway focus subscription stopped");
    });
    rx
}

async fn pump_focus_events(tx: &watch::Sender<Option<String>>) {
    let path = match socket_path() {
        Ok(p) => p,
        Err(err) => {
            tracing::debug!(error = ?err, "Sway IPC socket unavailable; focus-follow disabled");
            return;
        }
    };
    let mut stream = match UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = ?err, path = %path.display(), "connecting to Sway IPC socket failed");
            return;
        }
    };
    if let Err(err) = write_message(&mut stream, SUBSCRIBE, br#"["workspace"]"#).await {
        tracing::warn!(error = ?err, "subscribing to Sway workspace events failed");
        return;
    }
    // Consume the SUBSCRIBE reply (`{"success": true}`) before reading events.
    if let Err(err) = read_message(&mut stream).await {
        tracing::warn!(error = ?err, "reading Sway SUBSCRIBE reply failed");
        return;
    }
    loop {
        match read_message(&mut stream).await {
            Ok((_event_type, _payload)) => {
                // Any workspace event (focus, init, move, …) may have changed which output is
                // focused; re-query rather than trying to parse the focused output out of the
                // event payload (Sway does not guarantee an `output` field on workspace nodes).
                match focused_output().await {
                    Ok(name) => {
                        if tx.send(Some(name)).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "querying focused Sway output after a workspace event failed");
                    }
                }
            }
            Err(err) => {
                tracing::warn!(error = ?err, "reading Sway event stream failed");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::net::UnixListener;

    /// Force a hermetic environment for the socket-resolution tests. Safe because nextest
    /// runs every test in its own process.
    fn set_env(sock: Option<&std::path::Path>) {
        unsafe {
            match sock {
                Some(v) => std::env::set_var("SWAYSOCK", v),
                None => std::env::remove_var("SWAYSOCK"),
            }
        }
    }

    #[test]
    fn socket_path_requires_swaysock() {
        set_env(None);
        let err = socket_path().unwrap_err();
        assert!(err.to_string().contains("SWAYSOCK") || !err.to_string().is_empty());
    }

    /// A minimal fake Sway IPC tree: one output, one workspace, one focused tiled window,
    /// and one floating window on top.
    fn sample_tree() -> Value {
        json!({
            "type": "root",
            "nodes": [{
                "type": "output",
                "name": "eDP-1",
                "nodes": [{
                    "type": "workspace",
                    "id": 42,
                    "name": "1",
                    "nodes": [{
                        "type": "con",
                        "id": 1,
                        "pid": 1234,
                        "app_id": "kitty",
                        "name": "term",
                        "focused": true,
                        "visible": true,
                        "rect": {"x": 0, "y": 0, "width": 800, "height": 600}
                    }],
                    "floating_nodes": [{
                        "type": "floating_con",
                        "id": 2,
                        "pid": 5678,
                        "app_id": "firefox",
                        "name": "Mozilla Firefox",
                        "focused": false,
                        "visible": true,
                        "rect": {"x": 100, "y": 100, "width": 400, "height": 300}
                    }]
                }]
            }]
        })
    }

    #[test]
    fn find_focused_returns_the_focused_leaf_with_output_and_workspace() {
        let tree = sample_tree();
        let found = find_focused(&tree, "", 0).expect("a focused window");
        assert_eq!(found.output, "eDP-1");
        assert_eq!(found.workspace_id, 42);
        let (title, class, at, size) = window_fields(found.node);
        assert_eq!(title, "term");
        assert_eq!(class, "kitty");
        assert_eq!(at, (0, 0));
        assert_eq!(size, (800, 600));
    }

    #[test]
    fn collect_windows_lists_floating_before_tiled() {
        let tree = sample_tree();
        let mut found = Vec::new();
        collect_windows(&tree, "", 0, &mut found);
        assert_eq!(found.len(), 2);
        let (title0, ..) = window_fields(found[0].node);
        let (title1, ..) = window_fields(found[1].node);
        assert_eq!(title0, "Mozilla Firefox");
        assert_eq!(title1, "term");
        assert_eq!(found[0].output, "eDP-1");
        assert_eq!(found[1].workspace_id, 42);
    }

    #[test]
    fn window_fields_falls_back_to_xwayland_class() {
        let node = json!({
            "type": "con",
            "pid": 1,
            "name": "xterm",
            "window_properties": {"class": "XTerm"},
            "rect": {"x": 10, "y": -5, "width": 640, "height": 480}
        });
        let (title, class, at, size) = window_fields(&node);
        assert_eq!(title, "xterm");
        assert_eq!(class, "XTerm");
        assert_eq!(at, (10, -5));
        assert_eq!(size, (640, 480));
    }

    #[test]
    fn is_window_node_requires_a_pid() {
        let split = json!({"type": "con"});
        let window = json!({"type": "con", "pid": 99});
        assert!(!is_window_node(&split));
        assert!(is_window_node(&window));
    }

    async fn serve_once(listener: UnixListener, expected_type: u32, response: &Value) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let (msg_type, _payload) = read_message(&mut stream).await.unwrap();
        assert_eq!(msg_type, expected_type);
        let body = serde_json::to_vec(response).unwrap();
        write_message(&mut stream, expected_type, &body)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn focused_output_parses_get_outputs_reply() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        set_env(Some(&sock));

        let response = json!([
            {"name": "eDP-1", "focused": false},
            {"name": "DP-2", "focused": true},
        ]);
        let server =
            tokio::spawn(async move { serve_once(listener, GET_OUTPUTS, &response).await });

        let name = focused_output().await.unwrap();
        assert_eq!(name, "DP-2");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn active_window_parses_get_tree_reply() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        set_env(Some(&sock));

        let tree = sample_tree();
        let server = tokio::spawn(async move { serve_once(listener, GET_TREE, &tree).await });

        let win = active_window().await.unwrap();
        assert_eq!(win.title, "term");
        assert_eq!(win.class, "kitty");
        assert_eq!(win.monitor, "eDP-1");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn clients_parses_get_tree_reply_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        set_env(Some(&sock));

        let tree = sample_tree();
        let server = tokio::spawn(async move { serve_once(listener, GET_TREE, &tree).await });

        let list = clients().await.unwrap();
        assert_eq!(list.len(), 2);
        // Floating before tiled (see module doc); IDs and geometry survive the round-trip.
        assert_eq!(list[0].title, "Mozilla Firefox");
        assert_eq!(list[0].id, "2");
        assert_eq!(list[0].at, (100, 100));
        assert_eq!(list[0].size, (400, 300));
        assert_eq!(list[1].title, "term");
        assert_eq!(list[1].id, "1");
        assert_eq!(list[1].workspace_id, 42);
        assert!(list.iter().all(|w| w.mapped && !w.hidden));
        server.await.unwrap();
    }

    #[test]
    fn find_focused_returns_none_when_nothing_is_focused() {
        let tree = json!({
            "type": "root",
            "nodes": [{
                "type": "output",
                "name": "eDP-1",
                "nodes": [{
                    "type": "workspace",
                    "id": 1,
                    "nodes": [{
                        "type": "con",
                        "id": 1,
                        "pid": 1,
                        "name": "unfocused",
                        "focused": false
                    }]
                }]
            }]
        });
        assert!(find_focused(&tree, "", 0).is_none());
    }

    #[test]
    fn collect_windows_recurses_into_nested_split_containers() {
        // Both `nodes` and `floating_nodes` can themselves contain plain split containers
        // (no `pid`) that wrap further windows, not just direct leaves.
        let tree = json!({
            "type": "root",
            "nodes": [{
                "type": "output",
                "name": "eDP-1",
                "nodes": [{
                    "type": "workspace",
                    "id": 7,
                    "nodes": [{
                        // Tiled split container wrapping two windows.
                        "type": "con",
                        "nodes": [
                            {"type": "con", "id": 10, "pid": 1, "name": "left"},
                            {"type": "con", "id": 11, "pid": 2, "name": "right"}
                        ]
                    }],
                    "floating_nodes": [{
                        // Floating split container (rare in practice, but the tree schema
                        // allows it) wrapping one window.
                        "type": "floating_con",
                        "floating_nodes": [
                            {"type": "floating_con", "id": 20, "pid": 3, "name": "nested-float"}
                        ]
                    }]
                }]
            }]
        });
        let mut found = Vec::new();
        collect_windows(&tree, "", 0, &mut found);
        let titles: Vec<String> = found.iter().map(|f| window_fields(f.node).0).collect();
        assert_eq!(titles, vec!["nested-float", "left", "right"]);
        assert!(
            found
                .iter()
                .all(|f| f.output == "eDP-1" && f.workspace_id == 7)
        );
    }

    #[tokio::test]
    async fn read_message_rejects_a_bad_magic() {
        let (mut client, mut server) = tokio::net::UnixStream::pair().unwrap();
        // 6 bytes of garbage instead of `i3-ipc`, plus a zeroed length+type so the header is
        // still 14 bytes (read_message must reject on the magic check, not a short read).
        client.write_all(b"XXXXXX\0\0\0\0\0\0\0\0").await.unwrap();
        let err = read_message(&mut server).await.unwrap_err();
        assert!(err.to_string().contains("i3-ipc magic"), "{err:#}");
    }

    #[tokio::test]
    async fn sway_backend_name_and_socket_path_delegate() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        // socket_path() only reads $SWAYSOCK; no listener needed for this one.
        std::fs::write(&sock, b"").unwrap();
        set_env(Some(&sock));

        let backend: &dyn WmBackend = &Sway;
        assert_eq!(backend.name(), "Sway");
        assert_eq!(backend.socket_path().unwrap(), sock);
    }

    #[tokio::test]
    async fn sway_backend_active_window_delegates_to_the_free_function() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        set_env(Some(&sock));

        let tree = sample_tree();
        let server = tokio::spawn(async move { serve_once(listener, GET_TREE, &tree).await });

        let backend: &dyn WmBackend = &Sway;
        let win = backend.active_window().await.unwrap();
        assert_eq!(win.title, "term");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sway_backend_clients_delegates_to_the_free_function() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        set_env(Some(&sock));

        let tree = sample_tree();
        let server = tokio::spawn(async move { serve_once(listener, GET_TREE, &tree).await });

        let backend: &dyn WmBackend = &Sway;
        let list = backend.clients().await.unwrap();
        assert_eq!(list.len(), 2);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sway_backend_focused_output_delegates_to_the_free_function() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        set_env(Some(&sock));

        let response = json!([{"name": "eDP-1", "focused": true}]);
        let server =
            tokio::spawn(async move { serve_once(listener, GET_OUTPUTS, &response).await });

        let backend: &dyn WmBackend = &Sway;
        assert_eq!(backend.focused_output().await.unwrap(), "eDP-1");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn subscribe_focus_publishes_the_focused_output_after_a_workspace_event() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        set_env(Some(&sock));

        let server = tokio::spawn(async move {
            // First connection: the SUBSCRIBE session pump_focus_events opens and keeps open.
            let (mut sub_conn, _) = listener.accept().await.unwrap();
            let (msg_type, payload) = read_message(&mut sub_conn).await.unwrap();
            assert_eq!(msg_type, SUBSCRIBE);
            assert_eq!(payload, br#"["workspace"]"#);
            // Ack the SUBSCRIBE request, then push one fake event to trigger a re-query.
            write_message(&mut sub_conn, SUBSCRIBE, br#"{"success":true}"#)
                .await
                .unwrap();
            write_message(&mut sub_conn, 0x8000_0000, b"{}")
                .await
                .unwrap();

            // Second connection: the fresh `focused_output()` query triggered by that event.
            let (mut query_conn, _) = listener.accept().await.unwrap();
            let (msg_type, _payload) = read_message(&mut query_conn).await.unwrap();
            assert_eq!(msg_type, GET_OUTPUTS);
            let outputs = json!([{"name": "DP-3", "focused": true}]);
            let body = serde_json::to_vec(&outputs).unwrap();
            write_message(&mut query_conn, GET_OUTPUTS, &body)
                .await
                .unwrap();
            // Dropping both connections here lets pump_focus_events' next read see EOF and
            // stop the task cleanly once the test is done observing the update.
        });

        let handle = tokio::runtime::Handle::current();
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let mut rx = subscribe_focus(&handle, shutdown_rx);
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), Some("DP-3".to_string()));
        server.await.unwrap();
    }
}
