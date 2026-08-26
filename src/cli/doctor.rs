//! `doctor` subcommand — print a copy-pasteable Markdown diagnostic report.
//!
//! Collects version info, runtime environment, configuration state and live capability
//! probes (window-manager IPC, wlr-screencopy outputs, daemon socket Ping), then writes a
//! single Markdown blob to stdout. All headings are level 3 (`###`) so the output drops
//! cleanly under a user-supplied level-2 heading in issues and PRs.
//!
//! Live probes are best-effort: failures become `FAIL` / `WARN` lines in the report.
//! `doctor` always exits with status `0`, even when checks fail — its purpose is
//! diagnostic output, not gating scripts.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args as ClapArgs;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use crate::config::Config;
use crate::path::{tilde, tilde_str};

/// `doctor` subcommand arguments. No flags today; the struct is kept around to leave room
/// for future opt-outs (e.g. `--no-probe`, `--json`) without breaking the dispatch
/// signature.
#[derive(Debug, Default, ClapArgs)]
pub struct Args {}

/// Per-check status. Renders to the leading `OK` / `WARN` / `FAIL` tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn tag(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }
}

/// Live probe results for whichever [`crate::wm::WmBackend`] `crate::wm::detect()` found.
struct CompositorProbe {
    name: &'static str,
    socket: Result<PathBuf, String>,
    ping: Result<String, String>,
}

/// Snapshot of everything `render` needs. Built by the async collector; passed to a pure
/// formatter so the rendering logic can be unit-tested without touching the system.
struct DoctorState {
    // Version
    version: String,
    git_hash: Option<&'static str>,
    features: Vec<&'static str>,
    rustc: Option<&'static str>,

    // Environment
    os: &'static str,
    arch: &'static str,
    env: Vec<(&'static str, Option<String>, bool /* required */)>,

    // Configuration
    config_source: Option<PathBuf>,
    config_source_exists: bool,
    default_path: Option<PathBuf>,
    config_override: Option<PathBuf>,
    parse_status: Status,
    parse_error: Option<String>,
    save_directory: PathBuf,
    save_dir_exists: bool,
    save_dir_writable: bool,
    parsed_sinks: Vec<String>,
    invalid_sinks: Vec<String>,
    config_toml: Option<String>,

    // Window-manager IPC (Hyprland / Sway / Niri / generic wlr-foreign-toplevel)
    compositor: Option<CompositorProbe>,

    // wlr-screencopy
    wlr_init: Result<(), String>,
    wlr_outputs: Result<Vec<(String, u32, u32)>, String>,

    // Daemon
    daemon_socket: PathBuf,
    daemon_ping: Result<(), String>,
}

/// Build the doctor report and write it to stdout.
pub async fn run(_args: Args, config_override: Option<PathBuf>) -> Result<()> {
    let state = collect(config_override).await;
    print!("{}", render(&state));
    Ok(())
}

async fn collect(config_override: Option<PathBuf>) -> DoctorState {
    // ---- Version --------------------------------------------------------------
    let version = env!("CARGO_PKG_VERSION").to_owned();
    let git_hash = option_env!("GIT_HASH");
    let rustc = option_env!("RUSTC_VERSION");
    let mut features = Vec::new();
    if cfg!(feature = "ui") {
        features.push("ui");
    }
    if cfg!(feature = "tray") {
        features.push("tray");
    }
    if cfg!(feature = "notify") {
        features.push("notify");
    }

    // ---- Environment ---------------------------------------------------------
    let env = vec![
        (
            "XDG_SESSION_TYPE",
            std::env::var("XDG_SESSION_TYPE").ok(),
            false,
        ),
        (
            "WAYLAND_DISPLAY",
            std::env::var("WAYLAND_DISPLAY").ok(),
            false,
        ),
        (
            "XDG_RUNTIME_DIR",
            std::env::var("XDG_RUNTIME_DIR").ok(),
            false,
        ),
        (
            "XDG_CONFIG_HOME",
            std::env::var("XDG_CONFIG_HOME").ok(),
            false,
        ),
        (
            // None of these three is universally required: exactly one is set when running
            // under its respective compositor, and all three are absent on other wlroots
            // compositors (river, wayfire, …), which is a supported (if more limited) state.
            "HYPRLAND_INSTANCE_SIGNATURE",
            std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok(),
            false,
        ),
        ("SWAYSOCK", std::env::var("SWAYSOCK").ok(), false),
        ("NIRI_SOCKET", std::env::var("NIRI_SOCKET").ok(), false),
        ("SNYPR_CONFIG", std::env::var("SNYPR_CONFIG").ok(), false),
    ];

    // ---- Configuration -------------------------------------------------------
    let default_path = Config::default_path();
    let (config_source, config_source_exists) = match &config_override {
        Some(p) => (Some(p.clone()), p.exists()),
        None => match default_path.clone() {
            Some(p) => {
                let exists = p.exists();
                (Some(p), exists)
            }
            None => (None, false),
        },
    };
    let (config, parse_status, parse_error) = match (&config_source, config_source_exists) {
        (Some(p), true) => match Config::load(p) {
            Ok(c) => (c, Status::Ok, None),
            Err(e) => (Config::default(), Status::Fail, Some(format!("{e:#}"))),
        },
        _ => (Config::default(), Status::Warn, None),
    };

    let save_directory = config.save_directory();
    let save_dir_exists = save_directory.is_dir();
    let save_dir_writable = save_dir_exists && is_writable(&save_directory);

    let mut parsed_sinks: Vec<String> = config
        .default_sinks()
        .iter()
        .map(|s| format!("{s:?}"))
        .collect();
    if parsed_sinks.is_empty() {
        parsed_sinks.push("(none)".to_owned());
    }
    let invalid_sinks: Vec<String> = config
        .output
        .default_sinks
        .iter()
        .filter(|raw| raw.parse::<crate::cli::SinkSpec>().is_err())
        .cloned()
        .collect();

    let config_toml = toml::to_string_pretty(&config).ok();

    // ---- Compositor (Hyprland / Sway / Niri / generic wlr-foreign-toplevel) IPC ----------
    let compositor = match crate::wm::detect().await {
        Some(backend) => {
            let socket = backend.socket_path().map_err(|e| format!("{e:#}"));
            let ping = backend.focused_output().await.map_err(|e| format!("{e:#}"));
            Some(CompositorProbe {
                name: backend.name(),
                socket,
                ping,
            })
        }
        None => None,
    };

    // ---- wlr-screencopy ------------------------------------------------------
    let (wlr_init, wlr_outputs) = match crate::capture::wlr::WlrCapturer::new() {
        Ok(cap) => {
            use crate::capture::Capturer as _;
            let outputs = cap
                .outputs()
                .await
                .map(|outs| {
                    outs.into_iter()
                        .map(|o| (o.name, o.logical.w, o.logical.h))
                        .collect()
                })
                .map_err(|e| format!("{e:#}"));
            (Ok(()), outputs)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            (Err(msg.clone()), Err(msg))
        }
    };

    // ---- Daemon --------------------------------------------------------------
    let daemon_socket = crate::daemon::default_socket_path();
    let daemon_ping = daemon_ping(&daemon_socket).await;

    DoctorState {
        version,
        git_hash,
        features,
        rustc,
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        env,
        config_source,
        config_source_exists,
        default_path,
        config_override,
        parse_status,
        parse_error,
        save_directory,
        save_dir_exists,
        save_dir_writable,
        parsed_sinks,
        invalid_sinks,
        config_toml,
        compositor,
        wlr_init,
        wlr_outputs,
        daemon_socket,
        daemon_ping,
    }
}

/// Best-effort daemon liveness probe. Connects to the IPC socket, sends a `Ping` request,
/// expects an `Ok` response. Times out after a second so a wedged daemon does not stall the
/// whole report.
async fn daemon_ping(socket: &Path) -> Result<(), String> {
    if !socket.exists() {
        return Err("socket does not exist".to_owned());
    }
    let fut = async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let (read, mut write) = stream.into_split();
        let req = crate::ipc::Request::Ping;
        let mut payload = serde_json::to_vec(&req).map_err(|e| format!("encode: {e}"))?;
        payload.push(b'\n');
        write
            .write_all(&payload)
            .await
            .map_err(|e| format!("write: {e}"))?;
        write.shutdown().await.ok();
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if line.trim().is_empty() {
            return Err("daemon closed connection without responding".to_owned());
        }
        let resp: crate::ipc::Response =
            serde_json::from_str(line.trim()).map_err(|e| format!("decode: {e}"))?;
        match resp {
            crate::ipc::Response::Ok => Ok(()),
            crate::ipc::Response::Paths { .. } => Ok(()),
            crate::ipc::Response::Error { message } => Err(message),
        }
    };
    match tokio::time::timeout(std::time::Duration::from_secs(1), fut).await {
        Ok(r) => r,
        Err(_) => Err("timeout after 1s".to_owned()),
    }
}

/// Cheap writability probe — try to create (then remove) a temp file in `dir`.
fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(".snypr-doctor-write-check");
    match std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Pure formatter — turns a `DoctorState` into the Markdown report. Tested in isolation.
fn render(state: &DoctorState) -> String {
    let mut out = String::new();
    let mut counts = (0usize, 0usize, 0usize); // (ok, warn, fail)
    let mut bump = |s: Status| match s {
        Status::Ok => counts.0 += 1,
        Status::Warn => counts.1 += 1,
        Status::Fail => counts.2 += 1,
    };

    // ---- Version -------------------------------------------------------------
    let _ = writeln!(out, "### Version");
    let git = state.git_hash.unwrap_or("n/a");
    let _ = writeln!(out, "- snypr: {} (git: {git})", state.version);
    let features = if state.features.is_empty() {
        "(none)".to_owned()
    } else {
        state.features.join(", ")
    };
    let _ = writeln!(out, "- Built features: {features}");
    let _ = writeln!(out, "- rustc: {}", state.rustc.unwrap_or("n/a"));
    out.push('\n');

    // ---- Environment ---------------------------------------------------------
    let _ = writeln!(out, "### Environment");
    let _ = writeln!(out, "- OS: {} / {}", state.os, state.arch);
    for (name, value, required) in &state.env {
        let req = if *required { " [REQUIRED]" } else { "" };
        let status = match value {
            Some(_) => Status::Ok,
            None if *required => Status::Fail,
            None => Status::Warn,
        };
        bump(status);
        let rendered = match value {
            Some(v) => tilde_str(v),
            None => "(unset)".to_owned(),
        };
        let _ = writeln!(out, "- {}: {rendered}{req} [{}]", name, status.tag());
    }
    out.push('\n');

    // ---- Configuration -------------------------------------------------------
    let _ = writeln!(out, "### Configuration");
    let default_path_display = state
        .default_path
        .as_deref()
        .map(tilde)
        .unwrap_or_else(|| "(unknown)".to_owned());
    let override_display = state
        .config_override
        .as_deref()
        .map(tilde)
        .unwrap_or_else(|| "(none)".to_owned());
    let _ = writeln!(out, "- Default path: {default_path_display}");
    let _ = writeln!(
        out,
        "- Override (--config / SNYPR_CONFIG): {override_display}"
    );
    let (src_str, src_status, src_note) = match (&state.config_source, state.config_source_exists) {
        (Some(p), true) => (tilde(p), Status::Ok, ""),
        (Some(p), false) => (tilde(p), Status::Warn, " (missing — defaults used)"),
        (None, _) => ("(unknown)".to_owned(), Status::Warn, ""),
    };
    bump(src_status);
    let _ = writeln!(out, "- Source: {src_str}{src_note} [{}]", src_status.tag());
    bump(state.parse_status);
    let parsed_line = match (&state.parse_status, &state.parse_error) {
        (Status::Ok, _) => "OK".to_owned(),
        (Status::Warn, _) => "skipped (no file)".to_owned(),
        (Status::Fail, Some(e)) => format!("FAIL: {e}"),
        (Status::Fail, None) => "FAIL".to_owned(),
    };
    let _ = writeln!(
        out,
        "- Parsed: {parsed_line} [{}]",
        state.parse_status.tag()
    );
    let dir_status = if state.save_dir_exists && state.save_dir_writable {
        Status::Ok
    } else {
        Status::Warn
    };
    bump(dir_status);
    let _ = writeln!(
        out,
        "- Save directory: {} (exists: {}, writable: {}) [{}]",
        tilde(&state.save_directory),
        yes_no(state.save_dir_exists),
        yes_no(state.save_dir_writable),
        dir_status.tag(),
    );
    let _ = writeln!(out, "- Default sinks: {}", state.parsed_sinks.join(", "));
    if state.invalid_sinks.is_empty() {
        let _ = writeln!(out, "- Invalid sink entries: none");
    } else {
        bump(Status::Warn);
        let _ = writeln!(
            out,
            "- Invalid sink entries (silently dropped): {} [WARN]",
            state.invalid_sinks.join(", ")
        );
    }
    if let Some(toml_text) = &state.config_toml {
        let _ = writeln!(out, "- Effective config:");
        out.push_str("  ```toml\n");
        for line in toml_text.lines() {
            let _ = writeln!(out, "  {line}");
        }
        out.push_str("  ```\n");
    } else {
        bump(Status::Warn);
        let _ = writeln!(out, "- Effective config: (serialisation failed) [WARN]");
    }
    out.push('\n');

    // ---- Compositor (Hyprland / Sway / Niri) IPC -------------------------------------
    let _ = writeln!(out, "### Compositor");
    match &state.compositor {
        Some(probe) => {
            let _ = writeln!(out, "- Backend: {}", probe.name);
            match &probe.socket {
                Ok(p) => {
                    bump(Status::Ok);
                    let _ = writeln!(out, "- Socket path: {} [OK]", tilde(p));
                }
                Err(e) => {
                    bump(Status::Fail);
                    let _ = writeln!(out, "- Socket path: FAIL: {e} [FAIL]");
                }
            }
            match &probe.ping {
                Ok(name) => {
                    bump(Status::Ok);
                    let _ = writeln!(out, "- IPC ping (`focused_output`): OK ({name}) [OK]");
                }
                Err(e) => {
                    bump(Status::Fail);
                    let _ = writeln!(out, "- IPC ping (`focused_output`): FAIL: {e} [FAIL]");
                }
            }
        }
        None => {
            // No backend detected is the common case on river, wayfire, and other wlroots
            // compositors without a window-manager IPC integration — report as WARN, not
            // FAIL: `--window`/`--focused`/selector Window-mode won't work, but the install
            // isn't broken.
            bump(Status::Warn);
            let _ = writeln!(
                out,
                "- Backend: none detected (HYPRLAND_INSTANCE_SIGNATURE / SWAYSOCK / NIRI_SOCKET not set) [WARN]"
            );
            let _ = writeln!(
                out,
                "  `--window`, `--focused`, and the selector's Window mode click-to-pick will not work."
            );
        }
    }
    out.push('\n');

    // ---- wlr-screencopy ------------------------------------------------------
    let _ = writeln!(out, "### Wayland capture (wlr-screencopy)");
    match &state.wlr_init {
        Ok(()) => {
            bump(Status::Ok);
            let _ = writeln!(out, "- Capturer init: OK [OK]");
        }
        Err(e) => {
            bump(Status::Fail);
            let _ = writeln!(out, "- Capturer init: FAIL: {e} [FAIL]");
        }
    }
    match &state.wlr_outputs {
        Ok(outs) => {
            bump(Status::Ok);
            let _ = writeln!(out, "- Outputs detected: {} [OK]", outs.len());
            for (name, w, h) in outs {
                let _ = writeln!(out, "  - {name} {w}x{h}");
            }
        }
        Err(e) => {
            bump(Status::Fail);
            let _ = writeln!(out, "- Outputs detected: FAIL: {e} [FAIL]");
        }
    }
    out.push('\n');

    // ---- Daemon --------------------------------------------------------------
    let _ = writeln!(out, "### Daemon");
    let _ = writeln!(out, "- Socket path: {}", tilde(&state.daemon_socket));
    match &state.daemon_ping {
        Ok(()) => {
            bump(Status::Ok);
            let _ = writeln!(out, "- Listening: yes (Ping OK) [OK]");
        }
        Err(e) => {
            // Not running is the common case; report as WARN, not FAIL.
            bump(Status::Warn);
            let _ = writeln!(out, "- Listening: no ({e}) [WARN]");
        }
    }
    out.push('\n');

    // ---- Summary -------------------------------------------------------------
    let _ = writeln!(out, "### Summary");
    let _ = writeln!(
        out,
        "- {} OK, {} WARN, {} FAIL",
        counts.0, counts.1, counts.2
    );

    out
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn sample_state() -> DoctorState {
        let config = Config::default();
        DoctorState {
            version: "9.9.9".to_owned(),
            git_hash: Some("deadbeef"),
            features: vec!["ui", "tray"],
            rustc: Some("rustc 1.99.0"),
            os: "linux",
            arch: "x86_64",
            env: vec![
                ("WAYLAND_DISPLAY", Some("wayland-1".to_owned()), false),
                ("HYPRLAND_INSTANCE_SIGNATURE", None, false),
                ("SWAYSOCK", None, false),
                ("NIRI_SOCKET", None, false),
            ],
            config_source: Some(PathBuf::from("/home/u/.config/snypr/config.toml")),
            config_source_exists: false,
            default_path: Some(PathBuf::from("/home/u/.config/snypr/config.toml")),
            config_override: None,
            parse_status: Status::Warn,
            parse_error: None,
            save_directory: PathBuf::from("/tmp/shots"),
            save_dir_exists: true,
            save_dir_writable: true,
            parsed_sinks: vec!["File(None)".to_owned()],
            invalid_sinks: vec![],
            config_toml: toml::to_string_pretty(&config).ok(),
            compositor: Some(CompositorProbe {
                name: "Hyprland",
                socket: Err("HYPRLAND_INSTANCE_SIGNATURE is not set".to_owned()),
                ping: Err("no compositor".to_owned()),
            }),
            wlr_init: Ok(()),
            wlr_outputs: Ok(vec![("DP-1".to_owned(), 2560, 1440)]),
            daemon_socket: PathBuf::from("/run/user/1000/snypr.sock"),
            daemon_ping: Err("socket does not exist".to_owned()),
        }
    }

    #[test]
    fn renders_expected_sections() {
        let report = render(&sample_state());
        for header in [
            "### Version",
            "### Environment",
            "### Configuration",
            "### Compositor",
            "### Wayland capture (wlr-screencopy)",
            "### Daemon",
            "### Summary",
        ] {
            assert!(
                report.contains(header),
                "missing header `{header}`\n{report}"
            );
        }
    }

    #[test]
    fn report_uses_only_level_three_headings() {
        let report = render(&sample_state());
        for line in report.lines() {
            if line.starts_with('#') {
                assert!(
                    line.starts_with("### "),
                    "non-level-3 heading found: {line:?}"
                );
            }
        }
    }

    #[test]
    fn report_includes_fenced_toml_block() {
        let report = render(&sample_state());
        assert!(report.contains("```toml"));
        assert!(report.contains("[output]"));
    }

    #[test]
    fn report_summary_counts_statuses() {
        let report = render(&sample_state());
        // Sample has at least one FAIL (compositor socket + ping) and one WARN (daemon).
        // Just check the line shape, not exact counts (other lines may move around).
        let last = report.lines().last().unwrap();
        assert!(last.contains("OK,"));
        assert!(last.contains("WARN,"));
        assert!(last.contains("FAIL"));
    }

    #[rstest]
    #[case(Status::Ok, "OK")]
    #[case(Status::Warn, "WARN")]
    #[case(Status::Fail, "FAIL")]
    fn status_tags_are_stable(#[case] status: Status, #[case] expected: &str) {
        assert_eq!(status.tag(), expected);
    }

    /// No window-manager backend detected (e.g. river, wayfire) must report `WARN`, not
    /// `FAIL` — it's an expected, non-broken state, mirroring the daemon-not-running case.
    #[test]
    fn missing_compositor_backend_is_a_warning_not_a_failure() {
        let mut state = sample_state();
        state.compositor = None;
        let report = render(&state);
        assert!(report.contains("Backend: none detected"));
        assert!(report.contains("[WARN]"));
        // The `- Backend: none detected ...` line itself must not carry a `[FAIL]` tag.
        for line in report.lines() {
            if line.contains("Backend: none detected") {
                assert!(!line.contains("[FAIL]"), "unexpected FAIL line: {line:?}");
            }
        }
    }

    #[test]
    fn yes_no_renders_booleans() {
        assert_eq!(yes_no(true), "yes");
        assert_eq!(yes_no(false), "no");
    }

    /// `collect()`'s compositor probe must be `None` when no backend is detected — exercised
    /// against the *real* async collector (not just `render`'s pure formatting) so a future
    /// regression in the `crate::wm::detect()` wiring itself would fail this test too.
    #[tokio::test]
    async fn collect_reports_no_compositor_without_a_detected_backend() {
        crate::testing::set_compositor_env(None, None, None);
        let state = collect(None).await;
        assert!(state.compositor.is_none());
    }

    /// Same, but with a fake Sway IPC server standing in for a live session: `collect()`
    /// should report the backend's name and a successful socket/ping probe.
    #[tokio::test]
    async fn collect_reports_a_detected_sway_backend() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        crate::testing::set_compositor_env(None, Some(&sock), None);
        let listener = crate::testing::bind_fake_sway_socket(&sock);
        let outputs = serde_json::json!([{"name": "eDP-1", "focused": true}]);
        let server = tokio::spawn(async move {
            crate::testing::serve_fake_sway_reply(listener, crate::wm::sway::GET_OUTPUTS, &outputs)
                .await;
        });

        let state = collect(None).await;
        let probe = state.compositor.expect("a Sway backend was detected");
        assert_eq!(probe.name, "Sway");
        assert_eq!(probe.socket.unwrap(), sock);
        assert_eq!(probe.ping.unwrap(), "eDP-1");
        server.await.unwrap();
    }

    /// Same, but with a fake Niri IPC server standing in for a live session: `collect()`
    /// should report the backend's name and a successful socket/ping probe.
    #[tokio::test]
    async fn collect_reports_a_detected_niri_backend() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("niri.sock");
        crate::testing::set_compositor_env(None, None, Some(&sock));
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).await.unwrap();
            assert_eq!(request_line.trim_end(), "\"FocusedOutput\"");
            let mut stream = reader.into_inner();
            stream
                .write_all(br#"{"Ok":{"FocusedOutput":{"name":"eDP-1","logical":{"x":0,"y":0}}}}"#)
                .await
                .unwrap();
            stream.write_all(b"\n").await.unwrap();
        });

        let state = collect(None).await;
        let probe = state.compositor.expect("a Niri backend was detected");
        assert_eq!(probe.name, "Niri");
        assert_eq!(probe.socket.unwrap(), sock);
        assert_eq!(probe.ping.unwrap(), "eDP-1");
        server.await.unwrap();
    }
}
