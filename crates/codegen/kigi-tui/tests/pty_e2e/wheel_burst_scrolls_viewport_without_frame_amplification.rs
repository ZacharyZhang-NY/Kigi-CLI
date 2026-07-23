// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;
#[allow(unused_imports)]
use super::scroll::*;

// Infrastructure smoke test for the scroll-test primitives (marker
// transcript position tracking, wheel burst/sequence drivers, and the
// harness's live frame capture) against the real pager — not a regression
// repro; see the driver doc in `scroll.rs` for wheel/trackpad
// classification. The frame bound asserted here is an AMPLIFICATION bound
// (repaints never exceed one per wheel event); real cadence coalescing is
// left to the behavioral tests. Only byte-deterministic quantities are
// asserted, so the assertions hold under host-load jitter.

/// Marker count: 120 one-row lines ≫ the 50-row PTY, so early markers sit
/// off-screen-top once the finished stream pins the view to the bottom.
const MARKER_COUNT: usize = 120;

/// 30 spaced single reports at a nominal 6ms — a trackpad-classified flood
/// under the harness terminal.
const BURST_EVENTS: usize = 30;

const BURST_INTERVAL: Duration = Duration::from_millis(6);

/// **Wheel-burst scroll infra smoke.** A closely spaced wheel-up burst over a
/// marker transcript must scroll off-screen-top markers into view, without a
/// panic, producing at least one repaint frame and at most one frame per
/// wheel event (no frame amplification).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn wheel_burst_scrolls_viewport_without_frame_amplification() {
    let (mut harness, _content, top_before) =
        spawn_bottom_pinned_marker_scrollback(MARKER_COUNT).await;

    send_wheel_burst(
        &mut harness,
        SGR_SCROLL_UP,
        BURST_EVENTS,
        WHEEL_ROW,
        WHEEL_COL,
        BURST_INTERVAL,
    );
    harness.update(Duration::from_millis(600));

    assert!(
        harness.is_running(),
        "pager exited during the wheel burst\nscreen:\n{}",
        harness.screen_contents()
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager rendered 'panicked' during the wheel burst\nscreen:\n{}",
        harness.screen_contents()
    );

    let top_after = topmost_visible_marker(&harness).unwrap_or_else(|| {
        panic!(
            "no marker visible after the wheel burst\nscreen:\n{}",
            harness.screen_contents()
        )
    });
    assert!(
        top_after < top_before,
        "wheel-up burst did not scroll the viewport: topmost visible marker \
         {} → {} (expected a decrease)\nscreen:\n{}",
        marker_line(top_before),
        marker_line(top_after),
        harness.screen_contents()
    );

    // Durations are not asserted: the no-drain driver means chunks were
    // parsed at drain time, and wall-clock is load-sensitive anyway.
    let frames = harness.frame_count();
    assert!(
        frames >= 1,
        "wheel burst produced no repaint frames (no ?2026h/l pairs after reset_timing)"
    );
    assert!(
        frames <= BURST_EVENTS as u64,
        "burst of {BURST_EVENTS} wheel events produced {frames} frames — more than one \
         repaint per event (frame amplification)"
    );

    // Driver shape check: a mixed-direction sequence (momentum reversal) must
    // keep the pager alive — no position assertion, direction handling is for
    // the behavioral tests.
    let reversal = [
        SGR_SCROLL_UP,
        SGR_SCROLL_UP,
        SGR_SCROLL_DOWN,
        SGR_SCROLL_UP,
        SGR_SCROLL_DOWN,
        SGR_SCROLL_DOWN,
    ];
    send_wheel_sequence(
        &mut harness,
        &reversal,
        WHEEL_ROW,
        WHEEL_COL,
        BURST_INTERVAL,
    );
    harness.update(Duration::from_millis(300));
    assert!(
        harness.is_running() && !harness.contains_text("panicked"),
        "pager broke on a mixed-direction wheel sequence\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
