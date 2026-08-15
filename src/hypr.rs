//! Hyprland IPC helpers (active window, focused monitor).
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

use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct ActiveWindow {
    pub title: String,
    pub class: String,
    pub at: (i32, i32),
    pub size: (u32, u32),
    /// Compositor monitor identifier. Hyprland reports this as a numeric ID, not a name, so we
    /// surface it as a string for parity with the previous public API.
    pub monitor: String,
}

impl ActiveWindow {
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.at.0,
            y: self.at.1,
            w: self.size.0,
            h: self.size.1,
        }
    }
}

/// Fetch the currently active window via Hyprland IPC.
pub async fn active_window() -> Result<ActiveWindow> {
    let body = query("j/activewindow").await?;
    // When no client is focused Hyprland answers with an empty object `{}` (or the literal string
    // `none` on older builds). Treat both as "no active window".
    let trimmed = body.trim();
    if trimmed.is_empty() || trimmed == "{}" || trimmed.eq_ignore_ascii_case("none") {
        bail!("no active client");
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
        at: (raw.at[0], raw.at[1]),
        size: (raw.size[0].max(0) as u32, raw.size[1].max(0) as u32),
        monitor: raw.monitor.to_string(),
    })
}

/// A single Hyprland client (window) as reported by `j/clients`.
///
/// Order in the returned `Vec` matches Hyprland's IPC response, which is z-ordered
/// front-to-back; callers that want hit-testing should iterate in order and pick the
/// first match.
#[derive(Debug, Clone)]
pub struct HyprWindow {
    pub address: String,
    pub title: String,
    pub class: String,
    pub at: (i32, i32),
    pub size: (u32, u32),
    pub monitor: String,
    pub workspace_id: i64,
    pub mapped: bool,
    pub hidden: bool,
}

impl HyprWindow {
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.at.0,
            y: self.at.1,
            w: self.size.0,
            h: self.size.1,
        }
    }
}

/// List every Hyprland client (window). Order is preserved from the IPC response so
/// front-to-back hit-testing works without re-sorting.
pub async fn clients() -> Result<Vec<HyprWindow>> {
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
        .map(|r| HyprWindow {
            address: r.address,
            title: r.title,
            class: r.class,
            at: (r.at[0], r.at[1]),
            size: (r.size[0].max(0) as u32, r.size[1].max(0) as u32),
            monitor: r.monitor.to_string(),
            workspace_id: r.workspace.id,
            mapped: r.mapped,
            hidden: r.hidden,
        })
        .collect())
}

/// Topmost mapped, visible client whose rectangle contains the logical point `(x, y)`.
///
/// Assumes `clients` is in Hyprland's z-order (front-to-back), which is how
/// [`clients`] returns them.
pub fn window_at(clients: &[HyprWindow], x: i32, y: i32) -> Option<&HyprWindow> {
    clients
        .iter()
        .find(|c| c.mapped && !c.hidden && c.rect().contains(x, y))
}

/// Name of the focused monitor.
pub async fn focused_monitor() -> Result<String> {
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
        .ok_or_else(|| anyhow!("no focused monitor"))
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
pub fn subscribe_focus(
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
pub(crate) fn socket_path() -> Result<PathBuf> {
    socket_path_named(".socket.sock")
}

/// Resolve a Hyprland IPC socket by file name.
///
/// Hyprland exposes two sockets in the same directory: the request/response command socket
/// (`.socket.sock`) and the streaming event socket (`.socket2.sock`). Both share the layout
/// resolution: modern `$XDG_RUNTIME_DIR/hypr/$HIS/<file>` (Hyprland ≥ 0.42) with a fallback to
/// the legacy `/tmp/hypr/$HIS/<file>` for older builds.
fn socket_path_named(file: &str) -> Result<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").map_err(|_| {
        anyhow!("HYPRLAND_INSTANCE_SIGNATURE is not set; not running under Hyprland?")
    })?;
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

    fn win(
        address: &str,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        mapped: bool,
        hidden: bool,
    ) -> HyprWindow {
        HyprWindow {
            address: address.into(),
            title: "t".into(),
            class: "c".into(),
            at: (x, y),
            size: (w, h),
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
            window_at(&clients, 50, 50).map(|w| w.address.as_str()),
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
            window_at(&clients, 10, 10).map(|w| w.address.as_str()),
            Some("visible")
        );
    }

    #[test]
    fn window_at_returns_none_when_point_outside() {
        let clients = vec![win("only", 0, 0, 10, 10, true, false)];
        assert_eq!(
            window_at(&clients, 100, 100).map(|w| w.address.as_str()),
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
            window_at(&clients, 250, 50).map(|w| w.address.as_str()),
            Some("right")
        );
        assert_eq!(
            window_at(&clients, 50, 50).map(|w| w.address.as_str()),
            Some("left")
        );
    }

    #[test]
    fn active_window_rect_maps_position_and_size() {
        let w = ActiveWindow {
            title: "term".into(),
            class: "kitty".into(),
            at: (-10, 25),
            size: (800, 600),
            monitor: "1".into(),
        };
        assert_eq!(
            w.rect(),
            Rect {
                x: -10,
                y: 25,
                w: 800,
                h: 600
            }
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
}
