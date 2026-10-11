//! End-to-end recording test that needs a live `org.freedesktop.portal.ScreenCast` backend
//! — and, unlike every other integration test in this crate, a human.
//!
//! # Why this needs a human
//!
//! `tests/wayland.rs`'s `zwlr_screencopy` path is a pure protocol round-trip: no dialog, no
//! user interaction, so it can run unattended. The ScreenCast portal is different by design
//! — it *always* pops an interactive picker for the user to pick a monitor/window and
//! confirm, with no scriptable bypass (see `src/record/portal.rs`'s module docs and the
//! project plan's "Known v1 limitations"). So this test can never be part of an unattended
//! CI run; it exists for a developer on a real desktop session to run manually.
//!
//! # Running this
//!
//! ```sh
//! cargo test --features screencast --test screencast -- --ignored --nocapture
//! ```
//!
//! then switch to the picker dialog that pops up and confirm a monitor within
//! [`REACTION_TIME`]. Set `SNYPR_REQUIRE_SCREENCAST=1` to turn a missing/unresponsive portal
//! into a hard failure instead of a skip (e.g. from a real Hyprland session in CI, if one is
//! ever wired up) — same truthy/falsy convention as `SNYPR_REQUIRE_WAYLAND_CAPTURE` /
//! `SNYPR_REQUIRE_GTK`.
#![cfg(feature = "screencast")]

use std::time::Duration;

use snypr::capture::Selection;
use snypr::config::VideoFormat;
use snypr::record::{RecordOptions, run};

/// How long we wait, after starting the session, for a human to notice and confirm the
/// portal's picker dialog before recording a couple of seconds and stopping.
const REACTION_TIME: Duration = Duration::from_secs(8);

/// Upper bound on the whole session (negotiation + `REACTION_TIME` + a couple of recorded
/// seconds + `ffmpeg` finalization) before giving up. Generous on purpose — this is a manual
/// test, not a latency benchmark.
const OVERALL_TIMEOUT: Duration = Duration::from_secs(60);

fn screencast_is_required() -> bool {
    match std::env::var("SNYPR_REQUIRE_SCREENCAST") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no"
        ),
        Err(_) => false,
    }
}

/// Marked `#[ignore]` so a plain `cargo test --features screencast` (or `cargo nextest run
/// --features screencast`, which CI does run) never blocks on a picker dialog nobody is
/// there to answer. Must be requested explicitly with `--ignored`, matching the doc comment
/// above.
#[ignore = "needs a human to confirm the ScreenCast portal's picker dialog"]
#[tokio::test]
async fn records_a_few_seconds_of_the_picked_monitor_to_a_playable_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snypr-screencast-test.mp4");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let opts = RecordOptions {
        selection: Selection::Full,
        cursor: false,
        fps: 10,
        format: VideoFormat::Mp4,
        crf: 30,
        path: path.clone(),
    };

    let record_task = tokio::spawn(run(opts, stop_rx));

    tokio::time::sleep(REACTION_TIME).await;
    let _ = stop_tx.send(());

    let Ok(join_result) = tokio::time::timeout(OVERALL_TIMEOUT, record_task).await else {
        assert!(
            !screencast_is_required(),
            "SNYPR_REQUIRE_SCREENCAST is set but the session did not finish within \
             {OVERALL_TIMEOUT:?} (did a human confirm the picker dialog in time?)"
        );
        eprintln!("skipping screencast integration test: session timed out");
        return;
    };
    let run_result = join_result.expect("recording task panicked");
    match run_result {
        Ok(written) => {
            assert_eq!(written, path);
            let metadata = std::fs::metadata(&written).expect("recorded file must exist");
            assert!(metadata.len() > 0, "recorded file must not be empty");
        }
        Err(err) => {
            // No portal registered, the user cancelled the picker, `ffmpeg` missing, ... —
            // all indistinguishable from "this environment can't run the test" without
            // parsing the error message, so treat every failure the same way `skip()` does
            // in `tests/wayland.rs`.
            assert!(
                !screencast_is_required(),
                "SNYPR_REQUIRE_SCREENCAST is set but recording failed: {err:#}"
            );
            eprintln!("skipping screencast integration test: {err:#}");
        }
    }
}
