//! Shared test fixtures.
//!
//! Only compiled for `cfg(test)`. Building a [`Ctx`] requires a `Config` with notifications
//! disabled (otherwise a test run pops desktop toasts, and fails outright on a machine with
//! no D-Bus session), which three separate modules had grown their own copy of.

use crate::config::Config;
use crate::context::{Context, Ctx};

/// A [`Ctx`] with notifications off and the given `[output].default_sinks`.
///
/// Pass an empty slice for the config default.
pub async fn test_ctx_with_sinks(default_sinks: &[&str]) -> Ctx {
    let mut config = Config::default();
    if !default_sinks.is_empty() {
        config.output.default_sinks = default_sinks.iter().map(|s| (*s).to_owned()).collect();
    }
    // Desktop notifications need a live D-Bus session; a test run must never depend on one,
    // and must never spam the developer's desktop either.
    config.notify.success = false;
    config.notify.error = false;
    Context::new(config).await.expect("building a test Context")
}

/// A [`Ctx`] with notifications off and the default sink configuration.
pub async fn test_ctx() -> Ctx {
    test_ctx_with_sinks(&[]).await
}

/// Bind a Unix socket at `sock` for a fake Sway IPC server. Must be called (and the returned
/// listener handed to [`serve_fake_sway_reply`]) *before* the code under test connects, so the
/// socket file exists by the time it tries — mirrors the pattern in `crate::wm::sway`'s own
/// tests.
pub fn bind_fake_sway_socket(sock: &std::path::Path) -> tokio::net::UnixListener {
    tokio::net::UnixListener::bind(sock).expect("binding fake Sway socket")
}

/// Accept one connection on `listener` and reply once with `response`, framed as an i3ipc
/// message of `msg_type`. Mirrors the wire format in `crate::wm::sway` (`i3-ipc` magic + `u32`
/// little-endian length + `u32` little-endian type + JSON payload) so callers can exercise
/// Sway-backed code paths (`crate::wm::detect()` and beyond) end-to-end without a live Sway
/// session. `msg_type` should be one of `crate::wm::sway::{GET_TREE, GET_OUTPUTS}`.
pub async fn serve_fake_sway_reply(
    listener: tokio::net::UnixListener,
    msg_type: u32,
    response: &serde_json::Value,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut stream, _) = listener
        .accept()
        .await
        .expect("accepting fake Sway connection");
    let mut header = [0u8; 14];
    stream
        .read_exact(&mut header)
        .await
        .expect("reading fake Sway request header");
    let len = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .await
        .expect("reading fake Sway request payload");
    let body = serde_json::to_vec(response).expect("encoding fake Sway response");
    let mut out = Vec::with_capacity(14 + body.len());
    out.extend_from_slice(b"i3-ipc");
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&msg_type.to_le_bytes());
    out.extend_from_slice(&body);
    stream
        .write_all(&out)
        .await
        .expect("writing fake Sway response");
}

/// Force a hermetic compositor-detection environment for a test. Removes both env vars by
/// default; pass `Some(sock)` for `sway_sock` to make `crate::wm::detect()` pick the Sway
/// backend. Safe because nextest runs every test in its own process.
pub fn set_compositor_env(hyprland_sig: Option<&str>, sway_sock: Option<&std::path::Path>) {
    unsafe {
        match hyprland_sig {
            Some(v) => std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", v),
            None => std::env::remove_var("HYPRLAND_INSTANCE_SIGNATURE"),
        }
        match sway_sock {
            Some(v) => std::env::set_var("SWAYSOCK", v),
            None => std::env::remove_var("SWAYSOCK"),
        }
    }
}
