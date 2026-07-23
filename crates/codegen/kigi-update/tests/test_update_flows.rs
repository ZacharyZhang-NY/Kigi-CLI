//! End-to-end tests for the production update flows (`check_update_status`,
//! `ensure_latest_on_disk`, `run_update`, `run_update_if_available`) against
//! a GitHub-Releases-shaped [`common::artifact_server::ArtifactServer`],
//! injected via the `KIGI_UPDATE_BASE_URL` override that
//! `kigi_env::update_base_url()` honors.
//!
//! Three invariant families:
//!
//! 1. **Convergence**: a binary already on disk (installed by another
//!    process) is never downloaded a second time, but stale runners still
//!    get the relaunch/report signal.
//! 2. **Status**: `kigi update --check` reports upgrades only, surfaces
//!    fetch errors in the `error` field, and never advertises downgrades.
//! 3. **Race integrity**: concurrent installers — even for different
//!    versions — never leave a corrupt active binary.
//!
//! Everything here is `#[serial]`: KIGI_SHARE_DIR, KIGI_UPDATE_BASE_URL and
//! KIGI_TEST_VERSION are process-global.

#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serial_test::serial;

use common::artifact_server::ArtifactServer;
use common::{
    can_exec_shell_scripts, host_platform, make_update_config, reset_home, set_test_version,
    set_update_base, small_good_artifact, test_home,
};
use kigi_update::auto_update::{
    UpdateRunMode, check_update_status, ensure_latest_on_disk, install_internal_from_base,
    run_update, run_update_if_available,
};
use kigi_update::version::installed_on_disk_version;

/// Lay down a managed-install layout in the test KIGI_SHARE_DIR:
/// `bin/kigi -> ../downloads/kigi-<version>-<platform>` (what the installer
/// produces; the canonical link the disk-version probe reads).
fn fake_managed_install(version: &str) {
    let home = test_home();
    let downloads = home.join("downloads");
    let bin = home.join("bin");
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let name = format!("kigi-{version}-{}", host_platform());
    std::fs::write(downloads.join(&name), small_good_artifact()).unwrap();
    std::fs::set_permissions(
        downloads.join(&name),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let _ = std::fs::remove_file(bin.join("kigi"));
    std::os::unix::fs::symlink(Path::new("../downloads").join(&name), bin.join("kigi")).unwrap();
}

/// Assert the active `~/.kigi/bin/kigi` resolves to the expected versioned
/// binary, actually runs, and has exactly the expected content (the content
/// check is what catches a cross-racer temp-file corruption).
fn assert_active_binary(home: &Path, version: &str, platform: &str, expected_content: &[u8]) {
    let link = home.join("bin").join("kigi");
    assert!(link.is_symlink(), "kigi must be a symlink");
    let resolved = dunce::canonicalize(&link)
        .unwrap_or_else(|e| panic!("active kigi symlink does not resolve: {e}"));
    assert_eq!(
        resolved.file_name().unwrap().to_string_lossy(),
        format!("kigi-{version}-{platform}"),
        "active kigi must be the expected version"
    );
    assert_eq!(
        std::fs::read(&resolved).unwrap(),
        expected_content,
        "active binary content must be exactly the served artifact (no \
         partial/interleaved writes from a racing updater)"
    );
    let ran_ok = std::process::Command::new(&resolved)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ran_ok, "active kigi must pass the smoke-test");
}

fn setup(server: &ArtifactServer, latest: &str, running: &str) {
    let _ = test_home();
    reset_home();
    server.set_latest(latest);
    set_update_base(&server.base());
    set_test_version(running);
}

// Convergence: ensure_latest_on_disk downloads once, then every subsequent
// pass (the leader's hourly re-entry) converges without re-downloading.

#[tokio::test]
#[serial]
async fn ensure_latest_downloads_once_then_converges_without_redownload() {
    if !can_exec_shell_scripts() {
        eprintln!("skipping: shell scripts cannot execute in this sandbox");
        return;
    }
    let server = ArtifactServer::start(small_good_artifact());
    setup(&server, "0.2.7", "0.2.5");
    let cfg = make_update_config("stable");

    // Pass 1: disk is empty → downloads and installs.
    let first = ensure_latest_on_disk(&cfg).await.unwrap();
    assert_eq!(first.installed.as_deref(), Some("0.2.7"));
    assert!(first.relaunch_needed, "running 0.2.5 < disk 0.2.7");
    assert_eq!(server.request_count(), 1, "first pass downloads");
    assert_eq!(installed_on_disk_version().as_deref(), Some("0.2.7"));

    // Pass 2 (the pre-fix hourly re-download): disk already current →
    // no download, but the stale running process still gets the relaunch
    // signal.
    let second = ensure_latest_on_disk(&cfg).await.unwrap();
    assert_eq!(second.installed, None, "second pass must not re-download");
    assert!(second.relaunch_needed, "still running 0.2.5 < disk 0.2.7");
    assert_eq!(
        server.request_count(),
        1,
        "hourly re-entry must not download again"
    );
}

#[tokio::test]
#[serial]
async fn run_update_skips_download_when_disk_already_current() {
    if !can_exec_shell_scripts() {
        eprintln!("skipping: shell scripts cannot execute in this sandbox");
        return;
    }
    let server = ArtifactServer::start(small_good_artifact());
    setup(&server, "0.2.7", "0.2.5");
    // Another process (TUI background download) already installed 0.2.7.
    fake_managed_install("0.2.7");
    let mut cfg = make_update_config("stable");

    let result = run_update(false, None, None, &mut cfg).await.unwrap();

    assert_eq!(
        result.as_deref(),
        Some("0.2.7"),
        "run_update must still report the on-disk target so the caller \
         signals stale leaders to relaunch"
    );
    assert_eq!(
        server.request_count(),
        0,
        "a binary someone else installed must not be downloaded again"
    );
}

#[tokio::test]
#[serial]
async fn run_update_force_still_redownloads_when_disk_current() {
    if !can_exec_shell_scripts() {
        eprintln!("skipping: shell scripts cannot execute in this sandbox");
        return;
    }
    let server = ArtifactServer::start(small_good_artifact());
    setup(&server, "0.2.7", "0.2.7");
    fake_managed_install("0.2.7");
    let mut cfg = make_update_config("stable");

    let result = run_update(true, None, None, &mut cfg).await.unwrap();

    assert_eq!(result.as_deref(), Some("0.2.7"));
    assert_eq!(
        server.request_count(),
        1,
        "--force must bypass the disk-current skip and reinstall"
    );
}

#[tokio::test]
#[serial]
async fn run_update_rolls_back_when_latest_moved_backwards() {
    // Release rollback: the latest release points BELOW the on-disk install
    // (a bad release was deleted). The internal installer is authoritative,
    // so run_update must converge the disk down to it.
    if !can_exec_shell_scripts() {
        eprintln!("skipping: shell scripts cannot execute in this sandbox");
        return;
    }
    let server = ArtifactServer::start(small_good_artifact());
    setup(&server, "0.2.5", "0.2.7");
    fake_managed_install("0.2.7");
    let mut cfg = make_update_config("stable");

    let result = run_update(false, None, None, &mut cfg).await.unwrap();

    assert_eq!(
        result.as_deref(),
        Some("0.2.5"),
        "rollback target installed"
    );
    assert_eq!(installed_on_disk_version().as_deref(), Some("0.2.5"));
    assert_eq!(server.request_count(), 1);
}

// Disk-version probe

#[tokio::test]
#[serial]
async fn disk_probe_preserves_prerelease_versions() {
    let _ = test_home();
    reset_home();
    // An alpha install must read back as the full pre-release version —
    // truncating to "0.1.220" would mask the alpha → stable update.
    fake_managed_install("0.1.220-alpha.4");
    assert_eq!(
        installed_on_disk_version().as_deref(),
        Some("0.1.220-alpha.4")
    );
}

#[tokio::test]
#[serial]
async fn disk_probe_rejects_dangling_symlink() {
    // If the symlink survives but its target binary was deleted (manual
    // ~/.kigi/downloads cleanup), the probe must report None — otherwise
    // every updater would claim "already up to date" forever while no
    // runnable binary exists, and nothing would ever repair the install.
    let home = test_home();
    reset_home();
    let platform = host_platform();
    fake_managed_install("0.2.7");
    assert_eq!(installed_on_disk_version().as_deref(), Some("0.2.7"));

    std::fs::remove_file(
        home.join("downloads")
            .join(format!("kigi-0.2.7-{platform}")),
    )
    .unwrap();

    assert_eq!(
        installed_on_disk_version(),
        None,
        "a dangling symlink must not report an installed version"
    );
}

#[tokio::test]
#[serial]
async fn ensure_latest_repairs_dangling_symlink_by_downloading() {
    if !can_exec_shell_scripts() {
        eprintln!("skipping: shell scripts cannot execute in this sandbox");
        return;
    }
    // Dangling symlink + stale running process: the probe returns None, so
    // the decision falls back to the running version and the download runs,
    // repairing the install instead of wedging on "already up to date".
    let server = ArtifactServer::start(small_good_artifact());
    setup(&server, "0.2.7", "0.2.5");
    let home = test_home();
    let platform = host_platform();
    fake_managed_install("0.2.7");
    std::fs::remove_file(
        home.join("downloads")
            .join(format!("kigi-0.2.7-{platform}")),
    )
    .unwrap();
    let cfg = make_update_config("stable");

    let outcome = ensure_latest_on_disk(&cfg).await.unwrap();

    assert_eq!(
        outcome.installed.as_deref(),
        Some("0.2.7"),
        "dangling symlink must be repaired by an actual download"
    );
    assert_eq!(server.request_count(), 1);
    assert_eq!(
        installed_on_disk_version().as_deref(),
        Some("0.2.7"),
        "probe healthy again after the repair install"
    );
}

// check_update_status (`kigi update --check`)

#[tokio::test]
#[serial]
async fn check_status_reports_update_when_release_is_newer() {
    let server = ArtifactServer::start(small_good_artifact());
    setup(&server, "0.2.7", "0.2.5");
    let cfg = make_update_config("stable");

    let status = check_update_status(&cfg).await;

    assert_eq!(status.current_version, "0.2.5");
    assert_eq!(status.latest_version.as_deref(), Some("0.2.7"));
    assert!(status.update_available);
    assert_eq!(status.installer.as_deref(), Some("internal"));
    assert_eq!(status.error, None);
}

#[tokio::test]
#[serial]
async fn check_status_never_reports_downgrade_as_update() {
    // --check reports upgrades only; a rolled-back release is not advertised
    // (auto-update converges separately).
    let server = ArtifactServer::start(small_good_artifact());
    setup(&server, "0.2.5", "0.2.7");
    let cfg = make_update_config("stable");

    let status = check_update_status(&cfg).await;

    assert_eq!(status.latest_version.as_deref(), Some("0.2.5"));
    assert!(!status.update_available, "downgrade must not be advertised");
    assert_eq!(status.error, None);
}

#[tokio::test]
#[serial]
async fn check_status_surfaces_fetch_error_in_error_field() {
    // Point the updater at a dead endpoint (bound then dropped port →
    // connection refused). The status must carry the error rather than
    // pretending "up to date".
    let _ = test_home();
    reset_home();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    set_update_base(&format!("http://127.0.0.1:{port}/releases"));
    set_test_version("0.2.5");
    let cfg = make_update_config("stable");

    let status = check_update_status(&cfg).await;

    assert!(!status.update_available);
    assert_eq!(status.latest_version, None);
    let err = status.error.as_deref().expect("error must be surfaced");
    assert!(!err.is_empty());

    // And it serializes into the --json contract.
    let v = serde_json::to_value(&status).unwrap();
    assert!(v["error"].is_string());
    assert_eq!(v["updateAvailable"], false);
}

#[tokio::test]
#[serial]
async fn check_status_unsupported_channel_reports_error() {
    let server = ArtifactServer::start(small_good_artifact());
    setup(&server, "0.2.7", "0.2.5");
    let cfg = make_update_config("beta");

    let status = check_update_status(&cfg).await;

    assert!(!status.update_available);
    let err = status.error.as_deref().expect("channel error surfaced");
    assert!(
        err.contains("Unsupported release channel 'beta'"),
        "err: {err}"
    );
}

// run_update_if_available — the auto-update opt-out gate.

#[tokio::test]
#[serial]
async fn run_update_if_available_respects_auto_update_false() {
    // With cli.auto_update = false persisted, the startup check must return
    // without ever touching the network (the update base points at a dead
    // port — any fetch would error, any download would install).
    let _ = test_home();
    reset_home();
    std::fs::write(
        test_home().join("config.toml"),
        "[cli]\nauto_update = false\n",
    )
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    set_update_base(&format!("http://127.0.0.1:{port}/releases"));
    set_test_version("0.1.0");
    let cfg = make_update_config("stable");

    let ran = run_update_if_available(UpdateRunMode::Blocking, false, &cfg)
        .await
        .unwrap();
    assert!(!ran, "auto_update=false must suppress the update entirely");
}

// Race integrity: the accepted same-instant race must stay harmless. Two (or
// three) installers running concurrently — even for DIFFERENT versions —
// must never leave a corrupt active binary.

async fn run_concurrent_installs(
    server: &ArtifactServer,
    versions: &[&str],
) -> Vec<anyhow::Result<()>> {
    let base = server.base();
    let mut tasks = Vec::new();
    for version in versions {
        let base = base.clone();
        let version = version.to_string();
        tasks.push(tokio::spawn(async move {
            let cfg = make_update_config("stable");
            install_internal_from_base(Some(&version), &cfg, &base).await
        }));
    }
    let mut results = Vec::new();
    for t in tasks {
        results.push(t.await.expect("install task must not panic"));
    }
    results
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn concurrent_same_version_installs_leave_valid_active_binary() {
    if !can_exec_shell_scripts() {
        eprintln!("skipping: shell scripts cannot execute in this sandbox");
        return;
    }
    let home = test_home();
    reset_home();
    let platform = host_platform();
    let artifact = small_good_artifact();
    let server = ArtifactServer::start(artifact.clone());
    // Hold responses open so the racers genuinely overlap mid-download.
    server.set_slow(true);

    let results = run_concurrent_installs(&server, &["0.1.181", "0.1.181", "0.1.181"]).await;
    for r in results {
        r.expect("every racing install must succeed (atomic swap, last writer wins)");
    }

    // Lock-free model: concurrent racers may each download (accepted waste);
    // the invariant is integrity, not the count.
    assert!(server.request_count() >= 1);
    assert_active_binary(home, "0.1.181", &platform, &artifact);
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn concurrent_different_version_installs_do_not_corrupt_each_other() {
    if !can_exec_shell_scripts() {
        eprintln!("skipping: shell scripts cannot execute in this sandbox");
        return;
    }
    let home = test_home();
    reset_home();
    let platform = host_platform();
    let artifact = small_good_artifact();
    let server = ArtifactServer::start(artifact.clone());
    server.set_slow(true);

    let results = run_concurrent_installs(&server, &["0.1.181", "0.1.182"]).await;
    for r in results {
        r.expect("both racing installs must succeed");
    }

    // Both versioned binaries must exist with full, uncorrupted content.
    for version in ["0.1.181", "0.1.182"] {
        let path = home
            .join("downloads")
            .join(format!("kigi-{version}-{platform}"));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            artifact,
            "binary {version} must contain exactly the served artifact"
        );
    }

    // The active symlink points at whichever racer swapped last; it must
    // resolve and run regardless.
    let resolved = dunce::canonicalize(home.join("bin").join("kigi")).unwrap();
    assert_eq!(std::fs::read(&resolved).unwrap(), artifact);
    let name = resolved.file_name().unwrap().to_string_lossy().to_string();
    assert!(
        !name.contains(".tmp"),
        "active kigi must never be a temp file: {name}"
    );

    // No stray shared temp file left behind (a with_extension-style
    // collision name).
    assert!(
        !home.join("downloads").join("kigi-0.1.tmp").exists(),
        "the shared-temp-name collision must not exist"
    );
}
