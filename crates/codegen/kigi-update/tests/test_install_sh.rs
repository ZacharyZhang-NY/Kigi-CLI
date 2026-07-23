//! Harness for the bootstrap installer (`install.sh` at the repo root), the
//! second client that can brick a machine. Runs the REAL shipped script
//! against a fake `curl` that serves GitHub-Releases-shaped JSON, the
//! archive, and SHA256SUMS from local fixtures, and asserts:
//!
//! > After any install attempt, `$KIGI_SHARE_DIR/bin/kigi` resolves to a
//! > binary that runs, OR the install failed cleanly with nothing activated —
//! > never a partial/garbage binary.
//!
//! Each test uses its own tempdir home and passes PATH/KIGI_SHARE_DIR to the
//! child process explicitly, so no process-global state is touched and no
//! `#[serial]` is needed.

#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{archive_name, host_platform, make_release_archive, sha256_hex, small_good_artifact};

fn install_sh_path() -> Option<PathBuf> {
    // crates/codegen/kigi-update → repo root.
    dunce::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../install.sh"))
        .ok()
        .filter(|p| p.exists())
}

/// GitHub release JSON with download URLs whose suffixes the fake curl
/// dispatches on (the host is irrelevant).
fn release_json(version: &str) -> String {
    let name = archive_name(version);
    serde_json::json!({
        "tag_name": format!("v{version}"),
        "draft": false,
        "prerelease": false,
        "assets": [
            { "name": name, "browser_download_url": format!("https://example.test/dl/v{version}/{name}") },
            { "name": "SHA256SUMS", "browser_download_url": format!("https://example.test/dl/v{version}/SHA256SUMS") },
        ],
    })
    .to_string()
}

/// Fixture directory holding the fake curl + canned responses.
struct Fixture {
    dir: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Fixture {
    /// `binary` becomes the `kigi` entry of the served archive; `sums_hash`
    /// overrides the manifest hash when `Some` (to simulate corruption).
    fn new(version: &str, binary: &[u8], sums_hash: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();

        let archive = make_release_archive(binary);
        let hash = match sums_hash {
            Some(h) => h.to_string(),
            None => sha256_hex(&archive),
        };
        std::fs::write(dir.path().join("release.json"), release_json(version)).unwrap();
        std::fs::write(dir.path().join("archive.tar.gz"), &archive).unwrap();
        std::fs::write(
            dir.path().join("SHA256SUMS"),
            format!("{hash}  {}\n", archive_name(version)),
        )
        .unwrap();

        let d = dir.path().to_string_lossy().replace('\'', "'\\''");
        let curl = format!(
            r#"#!/bin/sh
echo "$@" >> '{d}/curl-args.log'
out=""
url=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-o" ]; then out="$a"; fi
  case "$a" in
    -*) ;;
    *) url="$a" ;;
  esac
  prev="$a"
done
serve() {{
  if [ -n "$out" ]; then cat "$1" > "$out"; else cat "$1"; fi
}}
case "$url" in
  */SHA256SUMS) serve '{d}/SHA256SUMS' ;;
  *.tar.gz)     serve '{d}/archive.tar.gz' ;;
  */latest|*/tags/v*) serve '{d}/release.json' ;;
  *) echo "fake curl: unmatched url: $url" >&2; exit 22 ;;
esac
"#
        );
        let curl_path = dir.path().join("curl");
        std::fs::write(&curl_path, curl).unwrap();
        std::fs::set_permissions(&curl_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        Self { dir, home }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        let script = install_sh_path().expect("install.sh present at repo root");
        let path = format!(
            "{}:{}",
            self.dir.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new("sh")
            .arg(&script)
            .args(args)
            .env("PATH", path)
            .env("KIGI_SHARE_DIR", self.home.path())
            .env("HOME", self.home.path())
            .output()
            .expect("install.sh must spawn")
    }

    fn curl_log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("curl-args.log")).unwrap_or_default()
    }

    fn active_kigi(&self) -> PathBuf {
        self.home.path().join("bin").join("kigi")
    }
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

#[test]
fn install_sh_happy_path_installs_versioned_binary_and_symlink() {
    if install_sh_path().is_none() {
        eprintln!("skipping: install.sh not found (vendored sandbox)");
        return;
    }
    let fx = Fixture::new("0.1.5", &small_good_artifact(), None);

    let out = fx.run(&[]);
    assert!(
        out.status.success(),
        "install.sh must succeed: stderr={}",
        stderr_of(&out)
    );

    // Managed layout: versioned binary + relative symlink, same as the
    // self-updater produces.
    let versioned = fx
        .home
        .path()
        .join("downloads")
        .join(format!("kigi-0.1.5-{}", host_platform()));
    assert!(versioned.exists(), "versioned binary installed");
    assert_eq!(std::fs::read(&versioned).unwrap(), small_good_artifact());

    let link = fx.active_kigi();
    assert!(link.is_symlink(), "bin/kigi is a symlink");
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        Path::new("..")
            .join("downloads")
            .join(format!("kigi-0.1.5-{}", host_platform())),
        "symlink must be relative (survives bind-mounted homes)"
    );

    let status = Command::new(&link).arg("--version").status().unwrap();
    assert!(status.success(), "installed kigi must run");

    assert!(
        fx.curl_log().contains("/latest"),
        "must resolve via /latest: {}",
        fx.curl_log()
    );
}

#[test]
fn install_sh_pinned_version_uses_tag_endpoint() {
    if install_sh_path().is_none() {
        eprintln!("skipping: install.sh not found (vendored sandbox)");
        return;
    }
    let fx = Fixture::new("0.1.5", &small_good_artifact(), None);

    let out = fx.run(&["--version", "v0.1.5"]);
    assert!(
        out.status.success(),
        "pinned install must succeed: stderr={}",
        stderr_of(&out)
    );
    assert!(
        fx.curl_log().contains("/tags/v0.1.5"),
        "must resolve via /tags/v0.1.5: {}",
        fx.curl_log()
    );
    assert!(fx.active_kigi().is_symlink());
}

#[test]
fn install_sh_rejects_checksum_mismatch_and_activates_nothing() {
    if install_sh_path().is_none() {
        eprintln!("skipping: install.sh not found (vendored sandbox)");
        return;
    }
    let fx = Fixture::new("0.1.5", &small_good_artifact(), Some(&"0".repeat(64)));

    let out = fx.run(&[]);
    assert!(
        !out.status.success(),
        "checksum mismatch must fail the install"
    );
    assert!(
        stderr_of(&out).contains("SHA256 mismatch"),
        "stderr: {}",
        stderr_of(&out)
    );
    let link = fx.active_kigi();
    assert!(
        !link.exists() && !link.is_symlink(),
        "nothing may be activated after a checksum failure"
    );
}

#[test]
fn install_sh_rejects_invalid_version_argument() {
    if install_sh_path().is_none() {
        eprintln!("skipping: install.sh not found (vendored sandbox)");
        return;
    }
    let fx = Fixture::new("0.1.5", &small_good_artifact(), None);

    let out = fx.run(&["--version", "not-a-version"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("invalid version"),
        "stderr: {}",
        stderr_of(&out)
    );
    assert!(
        fx.curl_log().is_empty(),
        "invalid arguments must fail before any network access"
    );
}

#[test]
fn install_sh_fails_when_release_lacks_platform_asset() {
    if install_sh_path().is_none() {
        eprintln!("skipping: install.sh not found (vendored sandbox)");
        return;
    }
    let fx = Fixture::new("0.1.5", &small_good_artifact(), None);
    let json = serde_json::json!({
        "tag_name": "v0.1.5",
        "assets": [
            { "name": "SHA256SUMS", "browser_download_url": "https://example.test/dl/v0.1.5/SHA256SUMS" },
        ],
    });
    std::fs::write(fx.dir.path().join("release.json"), json.to_string()).unwrap();

    let out = fx.run(&[]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("no asset"),
        "stderr: {}",
        stderr_of(&out)
    );
    let link = fx.active_kigi();
    assert!(!link.exists() && !link.is_symlink());
}

#[test]
fn install_sh_fails_when_archive_lacks_kigi_binary() {
    if install_sh_path().is_none() {
        eprintln!("skipping: install.sh not found (vendored sandbox)");
        return;
    }
    let fx = Fixture::new("0.1.5", &small_good_artifact(), None);
    // The manifest hash is kept consistent so the checksum gate passes and the
    // extraction check is what trips.
    let archive = common::make_tar_gz(&[("LICENSE", b"license only")]);
    std::fs::write(
        fx.dir.path().join("SHA256SUMS"),
        format!("{}  {}\n", sha256_hex(&archive), archive_name("0.1.5")),
    )
    .unwrap();
    std::fs::write(fx.dir.path().join("archive.tar.gz"), &archive).unwrap();

    let out = fx.run(&[]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("does not contain a 'kigi' binary"),
        "stderr: {}",
        stderr_of(&out)
    );
    let link = fx.active_kigi();
    assert!(!link.exists() && !link.is_symlink());
}
