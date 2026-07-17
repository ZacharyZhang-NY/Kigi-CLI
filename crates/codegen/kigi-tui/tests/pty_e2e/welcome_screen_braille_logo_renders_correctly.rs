// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// 1b. **Welcome screen renders Unicode Braille logo correctly.**
///
/// The logo uses Unicode Braille Pattern characters (U+2800–U+28FF).
/// A regression in the writer thread (using `WriteFile` instead of
/// `WriteConsoleW` on Windows, or a missing `SetConsoleOutputCP(65001)`)
/// causes these multi-byte UTF-8 characters to be misinterpreted as
/// individual legacy code-page bytes, producing garbled output.
///
/// This test asserts that specific Braille characters from the logo
/// appear intact in the PTY screen buffer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn welcome_screen_braille_logo_renders_correctly() {
    let content = ContentController::start().await.expect("start content");

    let binary = pager_binary().expect("resolve pager binary");
    // Use a tall terminal so pick_logo() selects the full moon (≥26 rows).
    let mut harness =
        PtyHarness::spawn_with_content(&binary, DEFAULT_ROWS, DEFAULT_COLS, &content, &[])
            .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    let screen = harness.screen_contents();

    // The moon logo is drawn from Braille Pattern characters. If the writer
    // thread sends raw UTF-8 bytes through a code-page-dependent API, these
    // 3-byte characters would be mangled into 3 separate single-byte
    // characters each (e.g. Cyrillic). The moon is procedurally generated,
    // so assert on the character class rather than specific glyphs: a full
    // interior cell (⣿) is present through most of the lunation, and there
    // must be a healthy number of non-blank braille cells overall.
    let braille_dots = screen
        .chars()
        .filter(|c| ('\u{2801}'..='\u{28FF}').contains(c))
        .count();
    assert!(
        braille_dots >= 20,
        "expected the braille moon (>= 20 non-blank braille cells), found \
         {braille_dots} — logo may be garbled by code-page \
         misinterpretation.\nScreen contents:\n{screen}"
    );

    harness.quit().expect("clean quit");
}
