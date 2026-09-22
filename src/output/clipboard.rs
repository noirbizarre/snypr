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
//!   The handshake is actually two-staged: after the "selection claimed" byte, the
//!   pipe is kept open and the parent polls it for up to [`CLIPBOARD_SERVE_GRACE`] for
//!   a *second* byte reporting whether `serve()` itself failed fast (e.g. instantly
//!   preempted by another clipboard tool, or a protocol error) before the parent would
//!   otherwise have already returned `Ok` and let `notify_success` fire. This closes
//!   the common "notification says copied but the paste yields nothing" race for fast
//!   failures. It cannot help with a failure *after* the grace window — by the time
//!   that happens the parent process (and the CLI invocation as a whole) is long gone,
//!   which is inherent to the fire-and-forget serve model this whole module exists to
//!   work around.
//!
//! The strategy is decided by [`crate::context::Context::running_as_daemon`].

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::Duration;

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

    /// Which publish strategy this sink will use. Split out of `write_png` so the choice is
    /// assertable — the fork path itself double-forks and cannot be exercised from a test.
    fn strategy(&self) -> CopyStrategy {
        if self.in_daemon {
            CopyStrategy::Inline
        } else {
            CopyStrategy::Fork
        }
    }
}

/// How the clipboard offer is published. See the module docs for why the choice matters.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum CopyStrategy {
    /// Publish in-process: the daemon outlives the offer.
    Inline,
    /// Fork a detached child: a one-shot CLI process exits immediately, and a selection
    /// whose owner is gone is a selection that pastes nothing.
    Fork,
}

#[async_trait]
impl OutputSink for ClipboardSink {
    async fn write_png(&self, bytes: &[u8]) -> Result<Option<PathBuf>> {
        let bytes = bytes.to_vec();
        let kind = self.kind;
        let strategy = self.strategy();
        tokio::task::spawn_blocking(move || match strategy {
            CopyStrategy::Inline => copy_inline(&bytes, kind),
            CopyStrategy::Fork => copy_forked(&bytes, kind),
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

/// How long the parent waits, after the child reports "selection claimed", for a
/// *second* handshake byte reporting a fast `serve()` failure before deciding the
/// offer is alive and returning success. See the module docs for the race this closes
/// and its inherent limit (a failure after this window can't be reported).
const CLIPBOARD_SERVE_GRACE: Duration = Duration::from_millis(250);

/// Fork-and-serve publish. Used in short-lived CLI invocations so the Wayland selection
/// survives the parent's exit.
///
/// Implementation notes:
///
/// * Fork happens *before* any Wayland connection is made. The child does the full
///   `Options::foreground(true) → prepare_copy → serve()` dance; the parent never
///   touches the Wayland fd. This avoids `std::mem::forget` tricks around the
///   `PreparedCopy` destructor and double-close hazards on inherited fds.
/// * A two-byte handshake pipe (`os_pipe`) lets the child report both early failures
///   (`prepare_copy`) and fast `serve()` failures synchronously so the original
///   `Outputs::write_png` caller still sees a meaningful `Err` in either case. Once
///   the child has successfully claimed the selection it writes `0x00` for the first
///   byte and keeps the pipe open into `serve()`; the parent reads that byte, then
///   polls the same pipe for up to [`CLIPBOARD_SERVE_GRACE`] for a second byte (see
///   [`read_serve_outcome`]/[`classify_serve_byte`]) before returning.
/// * The child calls `setsid()` to detach from the controlling terminal so signals
///   sent to the parent's shell (Ctrl-C after a screenshot) don't kill the
///   serving process. Standard streams are redirected to `/dev/null` after the
///   first handshake byte so tracing output doesn't leak into terminals after
///   detachment.
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
                // Tell the parent we successfully claimed the selection. Deliberately
                // *not* dropping `writer` yet: the parent watches this same pipe for a
                // second, fast-failure byte (see `read_serve_outcome`) before deciding
                // the offer is alive.
                let _ = writer.write_all(&[0u8]);
                redirect_stdio_to_devnull();
                // Block until another client takes over the selection (the common,
                // long-lived outcome) or `serve()` fails. Report whichever happens as
                // a second handshake frame: a failure within the parent's grace
                // window still surfaces as an `Err` there (so a bogus "copied"
                // notification never fires); a failure afterwards can only be logged
                // here, since the parent has already moved on — inherent to serving
                // asynchronously past the point the CLI invocation returns.
                match prepared.serve() {
                    Ok(()) => {
                        let _ = writer.write_all(&[0u8]);
                        drop(writer);
                        unsafe { libc::_exit(0) };
                    }
                    Err(err) => {
                        tracing::warn!(?err, "detached clipboard server: serve() failed");
                        let msg = format!("{err:#}");
                        let _ = writer.write_all(&[1u8]);
                        let _ = writer.write_all(msg.as_bytes());
                        drop(writer);
                        unsafe { libc::_exit(1) };
                    }
                }
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
    if status[0] != 0 {
        let mut msg = String::new();
        let _ = reader.read_to_string(&mut msg);
        if msg.is_empty() {
            msg = "detached clipboard server failed during prepare_copy".to_owned();
        }
        return Err(anyhow!("{msg}"));
    }

    match read_serve_outcome(&mut reader, CLIPBOARD_SERVE_GRACE)
        .context("watching the detached clipboard server for a fast serve() failure")?
    {
        ServeOutcome::StillServing | ServeOutcome::Ok => {
            tracing::info!(
                bytes = bytes.len(),
                ?kind,
                child_pid = pid as i64,
                "spawned detached clipboard server"
            );
            Ok(())
        }
        ServeOutcome::Failed(msg) => Err(anyhow!("{msg}")),
    }
}

/// Outcome of the parent's bounded second-stage read after the "selection claimed"
/// handshake byte. See [`copy_forked`]'s doc comment for the wire format.
#[derive(Debug, PartialEq, Eq)]
enum ServeOutcome {
    /// The grace window elapsed with the pipe still open: `serve()` is presumably
    /// blocked happily serving future paste requests (the common, desired case).
    StillServing,
    /// The child reported `serve()` returning `Ok` within the grace window (rare —
    /// e.g. instantly preempted with no error).
    Ok,
    /// The child reported `serve()` failing within the grace window.
    Failed(String),
}

/// Pure classification of the second handshake byte (and any message that follows
/// it), split out from the actual pipe I/O in [`read_serve_outcome`] so it's
/// unit-testable without a real pipe/fork.
fn classify_serve_byte(byte: Option<u8>, message: String) -> ServeOutcome {
    match byte {
        None => ServeOutcome::StillServing,
        Some(0) => ServeOutcome::Ok,
        Some(_) => ServeOutcome::Failed(if message.is_empty() {
            "detached clipboard server: serve() failed".to_owned()
        } else {
            message
        }),
    }
}

/// Poll `reader`'s pipe for up to `grace`, then decide what happened using
/// [`classify_serve_byte`]. An EOF with no byte at all (the child exited without
/// writing a second frame — e.g. killed) is treated the same as "still serving": we
/// have no evidence of failure, and this matches the pre-existing (single-byte)
/// handshake's behavior of trusting the claim once it's made.
fn read_serve_outcome(
    reader: &mut os_pipe::PipeReader,
    grace: std::time::Duration,
) -> Result<ServeOutcome> {
    if !poll_readable(reader.as_raw_fd(), grace)? {
        return Ok(ServeOutcome::StillServing);
    }
    let mut byte = [0u8; 1];
    match reader.read(&mut byte) {
        Ok(0) => Ok(ServeOutcome::StillServing),
        Ok(_) => {
            let mut msg = String::new();
            let _ = reader.read_to_string(&mut msg);
            Ok(classify_serve_byte(Some(byte[0]), msg))
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(ServeOutcome::StillServing),
        Err(e) => Err(e.into()),
    }
}

/// Block on `fd` becoming readable *or* reaching EOF for up to `timeout`, or return
/// `false` if neither happens. Used to bound the second-stage clipboard handshake read
/// (see [`read_serve_outcome`]) the same way [`crate::capture::wlr`]'s screencopy waits
/// are bounded — a raw `poll(2)` rather than anything Tokio-specific since this runs
/// inside a `spawn_blocking` closure with no async context available.
///
/// Checks `POLLHUP`/`POLLERR` alongside `POLLIN`: when the write end of a pipe closes
/// with nothing left to read, Linux reports that as `POLLHUP` (not `POLLIN`) — a
/// pipe-only quirk (regular sockets/fds do set `POLLIN` on EOF too), but pipes are the
/// only thing this is ever called on. Missing it would make an already-closed pipe look
/// exactly like an indefinite timeout instead of "read to find out what happened".
fn poll_readable(fd: std::os::fd::RawFd, timeout: std::time::Duration) -> Result<bool> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    loop {
        let ret = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err).context("polling a pipe fd");
        }
        return Ok(pollfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0);
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

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// Everything else in this module needs a live Wayland data device, but the kind mapping
    /// is pure — and getting it wrong silently publishes to the wrong selection.
    #[rstest]
    #[case(ClipboardKind::Regular, ClipboardType::Regular)]
    #[case(ClipboardKind::Primary, ClipboardType::Primary)]
    #[case(ClipboardKind::Both, ClipboardType::Both)]
    fn clipboard_type_maps_every_kind(
        #[case] kind: ClipboardKind,
        #[case] expected: ClipboardType,
    ) {
        assert_eq!(
            format!("{:?}", clipboard_type_for(kind)),
            format!("{expected:?}")
        );
    }

    #[rstest]
    #[case(ClipboardKind::Regular)]
    #[case(ClipboardKind::Primary)]
    #[case(ClipboardKind::Both)]
    fn the_daemon_publishes_inline_and_the_cli_forks(#[case] kind: ClipboardKind) {
        // The daemon outlives the offer, so it can own the Wayland data source directly.
        assert_eq!(
            ClipboardSink::new(kind, true).strategy(),
            CopyStrategy::Inline
        );
        // A one-shot CLI process exits the moment the command finishes; without the fork the
        // selection dies with it and the paste yields nothing.
        assert_eq!(
            ClipboardSink::new(kind, false).strategy(),
            CopyStrategy::Fork
        );
    }

    #[test]
    fn the_strategy_does_not_depend_on_the_clipboard_kind() {
        // Regular / Primary / Both change *which* selection is offered, never *how*.
        for in_daemon in [true, false] {
            let strategies: Vec<CopyStrategy> = [
                ClipboardKind::Regular,
                ClipboardKind::Primary,
                ClipboardKind::Both,
            ]
            .into_iter()
            .map(|k| ClipboardSink::new(k, in_daemon).strategy())
            .collect();
            assert!(
                strategies.windows(2).all(|w| w[0] == w[1]),
                "strategies diverged by kind: {strategies:?}"
            );
        }
    }

    #[test]
    fn classify_serve_byte_with_no_second_byte_is_still_serving() {
        // Grace window elapsed with the pipe still open: the common, desired case.
        assert_eq!(
            classify_serve_byte(None, String::new()),
            ServeOutcome::StillServing
        );
    }

    #[test]
    fn classify_serve_byte_zero_is_ok() {
        assert_eq!(
            classify_serve_byte(Some(0), String::new()),
            ServeOutcome::Ok
        );
    }

    #[test]
    fn classify_serve_byte_nonzero_is_failed_with_the_message() {
        assert_eq!(
            classify_serve_byte(Some(1), "selection preempted".to_owned()),
            ServeOutcome::Failed("selection preempted".to_owned())
        );
    }

    #[test]
    fn classify_serve_byte_nonzero_with_no_message_falls_back_to_a_generic_one() {
        assert_eq!(
            classify_serve_byte(Some(1), String::new()),
            ServeOutcome::Failed("detached clipboard server: serve() failed".to_owned())
        );
    }

    #[test]
    fn poll_readable_is_false_when_nothing_is_ever_written() {
        let (reader, writer) = os_pipe::pipe().expect("creating a test pipe");
        let readable = poll_readable(reader.as_raw_fd(), std::time::Duration::from_millis(50))
            .expect("poll should not error on a healthy pipe");
        assert!(
            !readable,
            "an empty pipe with no EOF must not report readable"
        );
        drop(writer); // keep the writer alive for the whole poll window above
    }

    #[test]
    fn poll_readable_is_true_once_data_is_written() {
        let (reader, mut writer) = os_pipe::pipe().expect("creating a test pipe");
        writer.write_all(&[0u8]).expect("writing to a test pipe");
        let readable = poll_readable(reader.as_raw_fd(), std::time::Duration::from_secs(1))
            .expect("poll should not error on a healthy pipe");
        assert!(readable);
    }

    #[test]
    fn poll_readable_is_true_on_eof() {
        let (reader, writer) = os_pipe::pipe().expect("creating a test pipe");
        drop(writer); // closing the write end makes the read end immediately readable (EOF)
        let readable = poll_readable(reader.as_raw_fd(), std::time::Duration::from_secs(1))
            .expect("poll should not error on a closed pipe");
        assert!(readable);
    }

    #[test]
    fn read_serve_outcome_reports_still_serving_when_the_grace_window_elapses() {
        let (mut reader, writer) = os_pipe::pipe().expect("creating a test pipe");
        let outcome = read_serve_outcome(&mut reader, std::time::Duration::from_millis(50))
            .expect("reading should not error");
        assert_eq!(outcome, ServeOutcome::StillServing);
        drop(writer);
    }

    #[test]
    fn read_serve_outcome_reports_failed_when_the_child_writes_an_error_frame() {
        let (mut reader, mut writer) = os_pipe::pipe().expect("creating a test pipe");
        writer.write_all(&[1u8]).expect("writing the failure byte");
        writer
            .write_all(b"preempted")
            .expect("writing the failure message");
        drop(writer);
        let outcome = read_serve_outcome(&mut reader, std::time::Duration::from_secs(1))
            .expect("reading should not error");
        assert_eq!(outcome, ServeOutcome::Failed("preempted".to_owned()));
    }

    #[test]
    fn read_serve_outcome_reports_ok_when_the_child_writes_a_success_frame() {
        let (mut reader, mut writer) = os_pipe::pipe().expect("creating a test pipe");
        writer.write_all(&[0u8]).expect("writing the success byte");
        drop(writer);
        let outcome = read_serve_outcome(&mut reader, std::time::Duration::from_secs(1))
            .expect("reading should not error");
        assert_eq!(outcome, ServeOutcome::Ok);
    }
}
