//! `KIGI_SHARE_DIR` override tests in an isolated binary so `kigi_home()`'s
//! process-wide `OnceLock` initializes from the overridden env var.

use std::path::PathBuf;

#[test]
fn kigi_home_override_path_helpers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let kigi_home = tmp.path().to_path_buf();
    unsafe {
        std::env::set_var("KIGI_SHARE_DIR", &kigi_home);
    }

    assert_eq!(
        kigi_tui::util::pager_toml_path(),
        kigi_home.join("pager.toml")
    );
    assert_eq!(
        kigi_tui::util::display_kigi_home_prefix(),
        "$KIGI_SHARE_DIR"
    );
    assert_eq!(
        kigi_tui::util::display_user_grok_path("config.toml"),
        "$KIGI_SHARE_DIR/config.toml"
    );

    let memory_path = kigi_home.join("memory/MEMORY.md");
    assert_eq!(
        kigi_tui::util::abbreviate_path(&memory_path.display().to_string()),
        "$KIGI_SHARE_DIR/memory/MEMORY.md"
    );

    assert!(kigi_tui::util::is_under_user_kigi_home(&memory_path));
    assert!(!kigi_tui::util::is_under_user_kigi_home(
        PathBuf::from("/tmp/other").as_path()
    ));
}
