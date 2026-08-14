//! Clipboard sink — publish PNG bytes as `image/png` via `wl-clipboard-rs`.
//!
//! There are two execution strategies, chosen at sink construction:
//!
//! * **In-process** — used inside the long-lived `snypr daemon` server. The Wayland
//!   data source is created in this very process and `wl-clipboard-rs` keeps an internal
//!   serving thread alive that responds to paste requests. Because the daemon process
//!   keeps running, the selection survives naturally until another client overtakes it.
//!
//! * **Forked** — used by short-lived (one-shot) CLI invocations. Without this branch,
//!   the CLI would: create the data source, schedule the background serving thread,
//!   then exit; the OS reaps the thread, the compositor drops the `wl_data_source`,
//!   and the selection points at nothing. Clipboard *managers* snapshot the offer for
//!   their history, but the active selection is dead and pasting requires re-picking
//!   from history. To fix this we mirror what the upstream `wl-copy` C binary does:
//!   `fork()`, then have the child set up the Wayland source via
//!   [`wl_clipboard_rs::copy::prepare_copy`] and call
//!   [`wl_clipboard_rs::copy::PreparedCopy::serve`] (blocking until preempted by
//!   another client). The parent returns immediately after a one-byte handshake over
//!   an `os_pipe` so synchronous errors (no Wayland display, missing protocol, …)
//!   still bubble up to the original `Outputs::write_png` caller.
//!
//! The strategy is decided by [`crate::context::Context::running_as_daemon`].

use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow};
use async_trait::async_trait;
use wl_clipboard_rs::copy::{ClipboardType, MimeType, Options, Source};

use super::OutputSink;
use crate::cli::ClipboardKind;

pub struct ClipboardSink {
    kind: ClipboardKind,
    /// When `true`, publish the offer in-process and return; the host process
    /// (the daemon) lives long enough to serve paste requests.
    /// When `false`, fork a detached child that serves the selection so the
    /// offer outlives the originating one-shot CLI process.
    in_daemon: bool,
}

impl ClipboardSink {
    pub fn new(kind: ClipboardKind, in_daemon: bool) -> Self {
        Self { kind, in_daemon }
    }
}

#[async_trait]
impl OutputSink for ClipboardSink {
    async fn write_png(&self, bytes: &[u8]) -> Result<Option<PathBuf>> {
        let bytes = bytes.to_vec();
        let kind = self.kind;
        let in_daemon = self.in_daemon;
        tokio::task::spawn_blocking(move || {
            if in_daemon {
                copy_inline(&bytes, kind)
            } else {
                copy_forked(&bytes, kind)
            }
        })
        .await
        .map_err(|e| anyhow!("clipboard task panicked: {e}"))??;
        Ok(None)
    }
}

/// Map our public [`ClipboardKind`] onto `wl-clipboard-rs`'s [`ClipboardType`].
fn clipboard_type_for(kind: ClipboardKind) -> ClipboardType {
    match kind {
        ClipboardKind::Regular => ClipboardType::Regular,
        ClipboardKind::Primary => ClipboardType::Primary,
        ClipboardKind::Both => ClipboardType::Both,
    }
}

/// In-process publish. Used inside the daemon, where the process itself keeps the Wayland
/// data source alive long enough to serve paste requests.
fn copy_inline(bytes: &[u8], kind: ClipboardKind) -> Result<()> {
    let mut opts = Options::new();
    opts.clipboard(clipboard_type_for(kind));
    opts.copy(
        Source::Bytes(bytes.to_vec().into()),
        MimeType::Specific("image/png".to_owned()),
    )
    .with_context(|| format!("publishing image/png to wayland clipboard ({kind:?})"))?;
    tracing::info!(
        bytes = bytes.len(),
        ?kind,
        "copied PNG to clipboard (in-process)"
    );
    Ok(())
}

/// Fork-and-serve publish. Used in short-lived CLI invocations so the Wayland selection
/// survives the parent's exit.
///
/// Implementation notes:
///
/// * Fork happens *before* any Wayland connection is made. The child does the full
///   `Options::foreground(true) → prepare_copy → serve()` dance; the parent never
///   touches the Wayland fd. This avoids `std::mem::forget` tricks around the
///   `PreparedCopy` destructor and double-close hazards on inherited fds.
/// * A one-byte handshake pipe (`os_pipe`) lets the child report early failures
///   synchronously so the original `Outputs::write_png` caller still sees a
///   meaningful `Err`. Once the child has successfully claimed the selection it
///   writes `0x00` and continues into `serve()`; the parent reads the byte and
///   returns `Ok(())`.
/// * The child calls `setsid()` to detach from the controlling terminal so signals
///   sent to the parent's shell (Ctrl-C after a screenshot) don't kill the
///   serving process. Standard streams are redirected to `/dev/null` after the
///   handshake so tracing output doesn't leak into terminals after detachment.
fn copy_forked(bytes: &[u8], kind: ClipboardKind) -> Result<()> {
    let (mut reader, mut writer) = os_pipe::pipe().context("creating handshake pipe")?;

    // SAFETY: this is a single-threaded `tokio::task::spawn_blocking` thread; we have
    // not yet initialised any Wayland client connection or taken any locks that could
    // deadlock across the fork. The other tokio worker threads still exist in the
    // parent but the child will exec straight into the wayland-client code below and
    // not interact with them.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(anyhow!(
            "fork() failed while preparing detached clipboard server: {}",
            std::io::Error::last_os_error()
        ));
    }

    if pid == 0 {
        // ─── Child ────────────────────────────────────────────────────────────
        drop(reader);
        // Detach from the parent's session so signals to the shell don't kill us
        // and the kernel reparents us to init when the CLI exits. The return
        // value is ignored: `setsid` only fails if we are already a session
        // leader, which we are not.
        unsafe {
            libc::setsid();
        }

        let result: Result<wl_clipboard_rs::copy::PreparedCopy> = (|| {
            let mut opts = Options::new();
            opts.foreground(true);
            opts.clipboard(clipboard_type_for(kind));
            let prepared = wl_clipboard_rs::copy::prepare_copy(
                opts,
                Source::Bytes(bytes.to_vec().into()),
                MimeType::Specific("image/png".to_owned()),
            )
            .with_context(|| format!("preparing image/png wayland clipboard offer ({kind:?})"))?;
            Ok(prepared)
        })();

        match result {
            Ok(prepared) => {
                // Tell the parent we successfully claimed the selection.
                let _ = writer.write_all(&[0u8]);
                drop(writer);
                redirect_stdio_to_devnull();
                // Block until another client takes over the selection. On any
                // error here we have already returned success to the parent —
                // best we can do is log and exit non-zero, which only affects
                // this background child.
                if let Err(err) = prepared.serve() {
                    tracing::warn!(?err, "detached clipboard server: serve() failed");
                    unsafe { libc::_exit(1) };
                }
                unsafe { libc::_exit(0) };
            }
            Err(err) => {
                // Frame: status byte (1 = error) followed by the message bytes.
                let msg = format!("{err:#}");
                let _ = writer.write_all(&[1u8]);
                let _ = writer.write_all(msg.as_bytes());
                drop(writer);
                unsafe { libc::_exit(1) };
            }
        }
    }

    // ─── Parent ──────────────────────────────────────────────────────────────
    drop(writer);
    let mut status = [0u8; 1];
    reader
        .read_exact(&mut status)
        .context("reading handshake from detached clipboard server")?;
    if status[0] == 0 {
        tracing::info!(
            bytes = bytes.len(),
            ?kind,
            child_pid = pid as i64,
            "spawned detached clipboard server"
        );
        Ok(())
    } else {
        let mut msg = String::new();
        let _ = reader.read_to_string(&mut msg);
        if msg.is_empty() {
            msg = "detached clipboard server failed during prepare_copy".to_owned();
        }
        Err(anyhow!("{msg}"))
    }
}

/// Redirect stdin/stdout/stderr to `/dev/null` in the current process. Called by the
/// detached clipboard child after the handshake completes so subsequent tracing output
/// doesn't leak into terminals that no longer expect it.
fn redirect_stdio_to_devnull() {
    use std::ffi::CString;
    // SAFETY: standard libc calls; failure is non-fatal (we just keep the inherited fds).
    unsafe {
        let path = CString::new("/dev/null").expect("no NUL in literal");
        let fd = libc::open(path.as_ptr(), libc::O_RDWR);
        if fd >= 0 {
            libc::dup2(fd, libc::STDIN_FILENO);
            libc::dup2(fd, libc::STDOUT_FILENO);
            libc::dup2(fd, libc::STDERR_FILENO);
            if fd > libc::STDERR_FILENO {
                libc::close(fd);
            }
        }
    }
}
