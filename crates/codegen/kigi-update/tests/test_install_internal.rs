//! End-to-end tests for the internal installer — GitHub Releases via a
//! wiremock server shaped like the real API (PRD F8).
//!
//! Wires together a mocked `releases/…` API + an isolated `KIGI_SHARE_DIR`
//! tempdir so we can verify the full install pipeline:
//!   resolve release → download archive → verify SHA-256 → extract →
//!   smoke-test → atomic symlink → cleanup_old_downloads → persist config.
//!
//! The function reads `kigi_home()` (a process-wide `OnceLock`), so all
//! tests in this binary share a single `KIGI_SHARE_DIR` and run serially via
//! `#[serial]`.

#![cfg(unix)]

mod common;

use serial_test::serial;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{
    archive_name, host_platform, make_release_archive, make_update_config, mount_latest,
    mount_release, release_json, reset_home, sha256_hex, small_good_artifact, test_home,
};
use kigi_update::auto_update::install_internal_from_base;
use kigi_update::version::installed_on_disk_version;

/// Base URL for the updater: `{server}/releases`, mirroring the production
/// `https://api.github.com/repos/{owner}/{repo}/releases`.
fn base(server: &MockServer) -> String {
    format!("{}/releases", server.uri())
}

// ─────────────────────────────────────────────────────────────────────────────
// Happy-path
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn install_pinned_version_writes_binary_and_symlink() {
    let _ = test_home();
    reset_home();
    let platform = host_platform();
    let server = MockServer::start().await;
    mount_release(&server, "0.1.181", &small_good_artifact()).await;
    let cfg = make_update_config("stable");

    install_internal_from_base(Some("0.1.181"), &cfg, &base(&server))
        .await
        .unwrap();

    let home = test_home();
    let downloaded = home
        .join("downloads")
        .join(format!("kigi-0.1.181-{platform}"));
    assert!(downloaded.exists(), "binary extracted: {downloaded:?}");
    assert_eq!(std::fs::read(&downloaded).unwrap(), small_good_artifact());

    let symlink = home.join("bin").join("kigi");
    assert!(symlink.is_symlink(), "kigi symlink created");
    let target = std::fs::read_link(&symlink).unwrap();
    assert_eq!(
        target.file_name().unwrap(),
        format!("kigi-0.1.181-{platform}").as_str()
    );

    // The archive must not linger after extraction.
    assert!(
        !home
            .join("downloads")
            .join(archive_name("0.1.181"))
            .exists(),
        "archive should be removed after extraction"
    );

    // The disk-version probe reads the new install back.
    assert_eq!(installed_on_disk_version().as_deref(), Some("0.1.181"));
}

#[tokio::test]
#[serial]
async fn install_chmods_binary_executable() {
    use std::os::unix::fs::PermissionsExt;
    let _ = test_home();
    reset_home();
    let platform = host_platform();
    let server = MockServer::start().await;
    mount_release(&server, "0.1.181", &small_good_artifact()).await;
    let cfg = make_update_config("stable");

    install_internal_from_base(Some("0.1.181"), &cfg, &base(&server))
        .await
        .unwrap();

    let binary = test_home()
        .join("downloads")
        .join(format!("kigi-0.1.181-{platform}"));
    let mode = std::fs::metadata(&binary).unwrap().permissions().mode();
    assert!(mode & 0o111 != 0, "binary must be executable, got {mode:o}");
}

#[tokio::test]
#[serial]
async fn install_persists_installer_config() {
    let _ = test_home();
    reset_home();
    let server = MockServer::start().await;
    mount_release(&server, "0.1.181", &small_good_artifact()).await;
    let cfg = make_update_config("stable");

    install_internal_from_base(Some("0.1.181"), &cfg, &base(&server))
        .await
        .unwrap();

    let cfg_body = std::fs::read_to_string(test_home().join("config.toml")).unwrap();
    assert!(
        cfg_body.contains("installer = \"internal\""),
        "config should set installer = internal: {cfg_body}"
    );
}

#[tokio::test]
#[serial]
async fn install_resolves_version_via_latest_when_no_target() {
    let _ = test_home();
    reset_home();
    let platform = host_platform();
    let server = MockServer::start().await;
    mount_release(&server, "0.1.181", &small_good_artifact()).await;
    mount_latest(&server, "0.1.181").await;
    let cfg = make_update_config("stable");

    // No pinned version → must resolve GET {base}/latest.
    install_internal_from_base(None, &cfg, &base(&server))
        .await
        .unwrap();

    assert!(
        test_home()
            .join("downloads")
            .join(format!("kigi-0.1.181-{platform}"))
            .exists(),
        "binary at version from /releases/latest"
    );
}

#[tokio::test]
#[serial]
async fn install_alpha_channel_resolves_semver_max_from_release_list() {
    let _ = test_home();
    reset_home();
    let platform = host_platform();
    let server = MockServer::start().await;

    // List endpoint (newest-published first) carries a pre-release AND a
    // semver-higher stable — the alpha channel must pick the stable (the
    // max), never get stuck on the pre-release.
    let list = serde_json::json!([
        release_json(&server.uri(), "0.1.180-alpha.5"),
        release_json(&server.uri(), "0.1.181"),
    ]);
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list))
        .mount(&server)
        .await;
    mount_release(&server, "0.1.181", &small_good_artifact()).await;

    let cfg = make_update_config("alpha");
    install_internal_from_base(None, &cfg, &base(&server))
        .await
        .unwrap();

    assert!(
        test_home()
            .join("downloads")
            .join(format!("kigi-0.1.181-{platform}"))
            .exists()
    );
}

#[tokio::test]
#[serial]
async fn install_removes_legacy_grok_links() {
    // Pre-rewrite installs left grok/agent/grok-pager links in bin/; the
    // installer must retire them.
    let _ = test_home();
    reset_home();
    let server = MockServer::start().await;
    mount_release(&server, "0.1.181", &small_good_artifact()).await;
    let cfg = make_update_config("stable");

    let bin_dir = test_home().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    for legacy in ["grok", "agent", "grok-pager"] {
        std::os::unix::fs::symlink("/tmp/fake-old-target", bin_dir.join(legacy)).unwrap();
    }

    install_internal_from_base(Some("0.1.181"), &cfg, &base(&server))
        .await
        .unwrap();

    for legacy in ["grok", "agent", "grok-pager"] {
        let link = bin_dir.join(legacy);
        assert!(
            !link.exists() && !link.is_symlink(),
            "legacy {legacy} link should be removed"
        );
    }
    assert!(bin_dir.join("kigi").is_symlink(), "kigi link installed");
}

// ─────────────────────────────────────────────────────────────────────────────
// Failure paths
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn install_fails_on_archive_404() {
    let _ = test_home();
    reset_home();
    let server = MockServer::start().await;

    // Release JSON + SHA256SUMS exist, but the archive itself 404s.
    let archive = make_release_archive(&small_good_artifact());
    Mock::given(method("GET"))
        .and(path("/releases/tags/v0.1.181"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(release_json(&server.uri(), "0.1.181")),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dl/v0.1.181/SHA256SUMS"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            "{}  {}\n",
            sha256_hex(&archive),
            archive_name("0.1.181")
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/dl/v0.1.181/{}", archive_name("0.1.181"))))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let cfg = make_update_config("stable");
    let err = install_internal_from_base(Some("0.1.181"), &cfg, &base(&server))
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("Download failed"), "msg: {msg}");
}

#[tokio::test]
#[serial]
async fn install_fails_on_missing_release() {
    let _ = test_home();
    reset_home();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/releases/tags/v0.9.9"))
        .respond_with(ResponseTemplate::new(404).set_body_string(r#"{"message":"Not Found"}"#))
        .mount(&server)
        .await;

    let cfg = make_update_config("stable");
    let err = install_internal_from_base(Some("0.9.9"), &cfg, &base(&server))
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("404"), "msg: {msg}");
}

#[tokio::test]
#[serial]
async fn install_rejects_invalid_pinned_version() {
    let _ = test_home();
    reset_home();
    let server = MockServer::start().await;
    let cfg = make_update_config("stable");

    let err = install_internal_from_base(Some("not-a-version"), &cfg, &base(&server))
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("invalid version format"), "msg: {msg}");
}

#[tokio::test]
#[serial]
async fn install_rejects_checksum_mismatch_and_leaves_no_binary() {
    let _ = test_home();
    reset_home();
    let platform = host_platform();
    let server = MockServer::start().await;

    // SHA256SUMS lists a hash that does NOT match the served archive.
    Mock::given(method("GET"))
        .and(path("/releases/tags/v0.1.181"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(release_json(&server.uri(), "0.1.181")),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/dl/v0.1.181/SHA256SUMS"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            "{}  {}\n",
            "0".repeat(64),
            archive_name("0.1.181")
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/dl/v0.1.181/{}", archive_name("0.1.181"))))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(make_release_archive(&small_good_artifact())),
        )
        .mount(&server)
        .await;

    let cfg = make_update_config("stable");
    let err = install_internal_from_base(Some("0.1.181"), &cfg, &base(&server))
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("SHA256 mismatch"), "msg: {msg}");

    let home = test_home();
    assert!(
        !home
            .join("downloads")
            .join(format!("kigi-0.1.181-{platform}"))
            .exists(),
        "no binary may be published from a checksum-failed archive"
    );
    assert!(
        !home
            .join("downloads")
            .join(archive_name("0.1.181"))
            .exists(),
        "the rejected archive must be deleted"
    );
    assert!(
        !home.join("bin").join("kigi").is_symlink(),
        "no symlink may be activated"
    );
}

#[tokio::test]
#[serial]
async fn install_fails_when_sha256sums_asset_missing() {
    let _ = test_home();
    reset_home();
    let server = MockServer::start().await;

    // Release JSON without a SHA256SUMS asset — must fail up front, before
    // downloading the archive.
    let name = archive_name("0.1.181");
    let json = serde_json::json!({
        "tag_name": "v0.1.181",
        "draft": false,
        "prerelease": false,
        "assets": [
            { "name": name, "browser_download_url": format!("{}/dl/v0.1.181/{name}", server.uri()) },
        ],
    });
    Mock::given(method("GET"))
        .and(path("/releases/tags/v0.1.181"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json))
        .mount(&server)
        .await;
    // The archive endpoint must never be contacted.
    Mock::given(method("GET"))
        .and(path(format!("/dl/v0.1.181/{name}")))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let cfg = make_update_config("stable");
    let err = install_internal_from_base(Some("0.1.181"), &cfg, &base(&server))
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("SHA256SUMS"), "msg: {msg}");
}

#[tokio::test]
#[serial]
async fn install_smoke_test_rejects_bad_binary_with_valid_checksum() {
    let _ = test_home();
    reset_home();
    let platform = host_platform();
    let server = MockServer::start().await;

    // Archive checksums fine but its binary exits 1 — only the smoke test
    // can catch this, and it must leave no active install behind.
    mount_release(&server, "0.1.181", b"#!/bin/sh\nexit 1\n").await;

    let cfg = make_update_config("stable");
    let err = install_internal_from_base(Some("0.1.181"), &cfg, &base(&server))
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("failed to run"), "msg: {msg}");

    let home = test_home();
    assert!(
        !home
            .join("downloads")
            .join(format!("kigi-0.1.181-{platform}"))
            .exists(),
        "smoke-test-failed binary must be deleted"
    );
    assert!(
        !home.join("bin").join("kigi").is_symlink(),
        "no symlink may be activated"
    );
}

#[tokio::test]
#[serial]
async fn install_swap_failure_leaves_prior_install_active() {
    // Sabotage activation: bin/kigi as a non-empty directory makes the
    // symlink rename fail. The download itself lands, but the prior state
    // of bin/ must be untouched (nothing half-activated).
    let _ = test_home();
    reset_home();
    let server = MockServer::start().await;
    mount_release(&server, "0.1.181", &small_good_artifact()).await;
    let cfg = make_update_config("stable");

    let bin_dir = test_home().join("bin");
    let kigi_dir = bin_dir.join("kigi");
    std::fs::create_dir_all(&kigi_dir).unwrap();
    std::fs::write(kigi_dir.join("blocker"), b"x").unwrap();

    let err = install_internal_from_base(Some("0.1.181"), &cfg, &base(&server))
        .await
        .expect_err("swap must fail when the link path is a non-empty dir");
    let msg = format!("{err:#}");
    assert!(msg.contains("swapping managed bin link"), "msg: {msg}");

    assert!(
        kigi_dir.is_dir() && kigi_dir.join("blocker").exists(),
        "failed swap must not clobber the existing path"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Rollback semantics: pinned installs move DOWN as well as up (the release
// channel is authoritative for the internal installer).
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn install_rollback_then_upgrade_sequence() {
    let _ = test_home();
    reset_home();
    let platform = host_platform();
    let server = MockServer::start().await;
    for v in ["0.2.5", "0.2.7"] {
        mount_release(&server, v, &small_good_artifact()).await;
    }
    let cfg = make_update_config("stable");

    // Up, back down (rollback), then up again.
    for version in ["0.2.7", "0.2.5", "0.2.7"] {
        install_internal_from_base(Some(version), &cfg, &base(&server))
            .await
            .unwrap();
        let target = std::fs::read_link(test_home().join("bin").join("kigi")).unwrap();
        assert_eq!(
            target.file_name().unwrap(),
            format!("kigi-{version}-{platform}").as_str(),
            "active binary must follow the pinned install"
        );
        assert_eq!(installed_on_disk_version().as_deref(), Some(version));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Cleanup integration: install v1..v3, verify N-1 retention.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn install_cleans_up_old_versions_keeping_n_minus_one() {
    let _ = test_home();
    reset_home();
    let platform = host_platform();
    let server = MockServer::start().await;
    for v in ["0.1.179", "0.1.180", "0.1.181"] {
        mount_release(&server, v, &small_good_artifact()).await;
    }
    let cfg = make_update_config("stable");

    for v in ["0.1.179", "0.1.180", "0.1.181"] {
        // Age earlier installs: cleanup never deletes freshly-written
        // binaries (concurrent-racer protection), so retention assertions
        // need the previous installs to look like old leftovers.
        common::backdate_downloads();
        install_internal_from_base(Some(v), &cfg, &base(&server))
            .await
            .unwrap();
    }

    let downloads = test_home().join("downloads");
    assert!(
        downloads.join(format!("kigi-0.1.181-{platform}")).exists(),
        "current"
    );
    assert!(
        downloads.join(format!("kigi-0.1.180-{platform}")).exists(),
        "N-1 retained"
    );
    assert!(
        !downloads.join(format!("kigi-0.1.179-{platform}")).exists(),
        "oldest deleted"
    );

    let target = std::fs::read_link(test_home().join("bin").join("kigi")).unwrap();
    assert!(
        target
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("0.1.181"),
        "symlink points to latest: {target:?}"
    );
}

#[tokio::test]
#[serial]
async fn install_idempotent_for_same_version() {
    let _ = test_home();
    reset_home();
    let platform = host_platform();
    let server = MockServer::start().await;
    mount_release(&server, "0.1.181", &small_good_artifact()).await;
    let cfg = make_update_config("stable");

    install_internal_from_base(Some("0.1.181"), &cfg, &base(&server))
        .await
        .unwrap();
    let path_installed = test_home()
        .join("downloads")
        .join(format!("kigi-0.1.181-{platform}"));
    let first = std::fs::read(&path_installed).unwrap();

    install_internal_from_base(Some("0.1.181"), &cfg, &base(&server))
        .await
        .unwrap();
    let second = std::fs::read(&path_installed).unwrap();

    assert_eq!(first, second);
    let target = std::fs::read_link(test_home().join("bin").join("kigi")).unwrap();
    assert!(target.to_string_lossy().contains("0.1.181"));
}

#[tokio::test]
#[serial]
async fn install_creates_kigi_home_subdirs_if_missing() {
    let _ = test_home();
    reset_home();
    let _ = std::fs::remove_dir_all(test_home().join("bin"));
    let _ = std::fs::remove_dir_all(test_home().join("downloads"));

    let server = MockServer::start().await;
    mount_release(&server, "0.1.181", &small_good_artifact()).await;
    let cfg = make_update_config("stable");

    install_internal_from_base(Some("0.1.181"), &cfg, &base(&server))
        .await
        .unwrap();

    assert!(test_home().join("bin").is_dir());
    assert!(test_home().join("downloads").is_dir());
}
