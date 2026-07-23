//! Shared helpers for integration tests.
//!
//! Each `tests/*.rs` integration test is its own binary, so each binary has
//! its own `OnceLock<KIGI_SHARE_DIR>`. The helpers below ensure the per-binary
//! initialization is identical: same env-var set, same isolation guarantees,
//! same reset between tests.
//!
//! Mirrors the KIGI_SHARE_DIR isolation pattern used in other integration tests.
//!
//! ## Usage
//!
//! ```ignore
//! mod common;
//! use common::{test_home, reset_home};
//!
//! #[tokio::test]
//! #[serial_test::serial]
//! async fn my_test() {
//!     let _ = test_home();   // initializes KIGI_SHARE_DIR once per binary
//!     reset_home();          // wipes state between tests
//!     // ...
//! }
//! ```

// each test binary uses a different subset
#![allow(dead_code)]

#[cfg(unix)]
pub mod artifact_server;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// KIGI_SHARE_DIR isolation

/// Returns a process-wide test `KIGI_SHARE_DIR`, initialized exactly once per test
/// binary. Once initialized, `kigi_config::kigi_home()` will resolve to
/// this directory for the lifetime of the process.
///
/// Also clears env vars that the auto-update code consults so a parent shell's
/// values can't pollute the baseline.
pub fn test_home() -> &'static PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.keep();
        // SAFETY: called once at OnceLock init, before any other thread touches
        // these env vars. Tests using this helper must be `#[serial]`.
        unsafe {
            std::env::set_var("KIGI_SHARE_DIR", &path);
            std::env::remove_var("KIGI_TEST_VERSION");
            std::env::remove_var("KIGI_INSTALLER");
            std::env::remove_var("KIGI_MANAGED_BY_INTERNAL");
            std::env::remove_var(kigi_env::UPDATE_BASE_URL_ENV);
        }
        path
    })
}

/// Wipe state in `KIGI_SHARE_DIR` between tests so each test sees a clean home.
/// Removes the well-known files and subdirectories the update path writes,
/// and clears env vars that individual tests may set.
pub fn reset_home() {
    let home = test_home();
    let _ = std::fs::remove_file(home.join("config.toml"));
    let _ = std::fs::remove_file(home.join("version.json"));
    let _ = std::fs::remove_file(home.join("version.json.tmp"));
    let _ = std::fs::remove_dir_all(home.join("bin"));
    let _ = std::fs::remove_dir_all(home.join("downloads"));
    // SAFETY: tests using this helper must be `#[serial]`.
    unsafe {
        std::env::remove_var("KIGI_TEST_VERSION");
        std::env::remove_var("KIGI_INSTALLER");
        std::env::remove_var(kigi_env::UPDATE_BASE_URL_ENV);
    }
}

/// Override the version reported by `get_installed_kigi_version()` for the
/// duration of the test (until [`reset_home`] or process exit).
pub fn set_test_version(v: &str) {
    // SAFETY: tests using this helper must be `#[serial]`.
    unsafe { std::env::set_var("KIGI_TEST_VERSION", v) };
}

/// Point the production update flows (`check_update_status`,
/// `ensure_latest_on_disk`, `run_update`) at a mock GitHub Releases API.
/// Cleared by [`reset_home`].
pub fn set_update_base(base: &str) {
    // SAFETY: tests using this helper must be `#[serial]`.
    unsafe { std::env::set_var(kigi_env::UPDATE_BASE_URL_ENV, base) };
}

// Install-test fixtures

/// Host `{os}-{arch}` string matching the versioned binary naming scheme
/// (`kigi-{version}-{platform}`).
pub fn host_platform() -> String {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        panic!("unsupported test platform");
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        panic!("unsupported test arch");
    };
    format!("{os}-{arch}")
}

/// Host Rust target triple, matching `auto_update::target_triple()` and the
/// release-asset naming in `.github/workflows/release.yml`.
pub fn host_triple() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else {
        panic!("unsupported test platform");
    }
}

/// Release-archive asset name for `version` on the host platform.
pub fn archive_name(version: &str) -> String {
    format!("kigi-{version}-{}.tar.gz", host_triple())
}

/// Minimal [`kigi_update::UpdateConfig`] for install tests.
pub fn make_update_config(channel: &str) -> kigi_update::UpdateConfig {
    kigi_update::UpdateConfig {
        proxy_base_url: "http://test.invalid/v1".to_string(),
        auth_scope: "test".to_string(),
        deployment_key: None,
        alpha_test_key: None,
        channel: channel.to_string(),
    }
}

/// True if shell-script artifacts can execute in this environment. False in
/// restricted sandboxes (e.g. hermetic remote execution) that lack /bin/sh.
#[cfg(unix)]
pub fn can_exec_shell_scripts() -> bool {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("probe");
    std::fs::write(&p, b"#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::process::Command::new(&p)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A small real executable: exits 0 for `--version`, so the smoke-test passes.
pub fn small_good_artifact() -> Vec<u8> {
    b"#!/bin/sh\nexit 0\n".to_vec()
}

/// Backdate every file in `KIGI_SHARE_DIR/downloads` by ~2 hours.
///
/// `cleanup_old_downloads` deliberately never deletes a freshly-written
/// binary or temp file (it may belong to a concurrent in-flight install), so
/// tests asserting the retention policy must age their fixtures to look like
/// real leftovers from previous releases.
pub fn backdate_downloads() {
    let downloads = test_home().join("downloads");
    let Ok(entries) = std::fs::read_dir(&downloads) else {
        return;
    };
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_file()
            && let Ok(f) = std::fs::File::options().write(true).open(&p)
        {
            let _ = f.set_times(std::fs::FileTimes::new().set_modified(old));
        }
    }
}

// GitHub Releases fixtures
//
// Wire shapes mirror the real GitHub REST API
// (https://docs.github.com/en/rest/releases/releases):
//   GET /repos/{o}/{r}/releases/latest      → release object
//   GET /repos/{o}/{r}/releases/tags/{tag}  → release object
//   GET /repos/{o}/{r}/releases             → array of release objects
// Release object: {"tag_name":"v0.1.0","assets":[{"name":"...",
// "browser_download_url":"..."}]}

/// Build a tar.gz archive from `(name, bytes)` entries.
#[cfg(unix)]
pub fn make_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(gz);
    for (name, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, name, *data).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

/// Release archive containing a single `kigi` entry with `binary` as its body.
#[cfg(unix)]
pub fn make_release_archive(binary: &[u8]) -> Vec<u8> {
    make_tar_gz(&[("kigi", binary)])
}

/// Hex SHA-256 of `bytes` (as written into SHA256SUMS manifests).
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

/// GitHub release JSON for `version`, with asset download URLs rooted at
/// `{server_uri}/dl/v{version}/…`.
pub fn release_json(server_uri: &str, version: &str) -> serde_json::Value {
    let name = archive_name(version);
    serde_json::json!({
        "tag_name": format!("v{version}"),
        "draft": false,
        "prerelease": !semver::Version::parse(version).unwrap().pre.is_empty(),
        "assets": [
            {
                "name": name,
                "browser_download_url": format!("{server_uri}/dl/v{version}/{name}"),
            },
            {
                "name": "SHA256SUMS",
                "browser_download_url": format!("{server_uri}/dl/v{version}/SHA256SUMS"),
            },
        ],
    })
}

/// Mount the per-release endpoints for `version` on a wiremock server:
/// `GET /releases/tags/v{version}` plus the archive and SHA256SUMS asset
/// downloads. Callers that need `latest` also call [`mount_latest`].
///
/// The base URL to hand the updater is `format!("{}/releases", server.uri())`.
#[cfg(unix)]
pub async fn mount_release(server: &wiremock::MockServer, version: &str, binary: &[u8]) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let archive = make_release_archive(binary);
    let sums = format!("{}  {}\n", sha256_hex(&archive), archive_name(version));

    Mock::given(method("GET"))
        .and(path(format!("/releases/tags/v{version}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(release_json(&server.uri(), version)),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/dl/v{version}/{}", archive_name(version))))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(archive))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/dl/v{version}/SHA256SUMS")))
        .respond_with(ResponseTemplate::new(200).set_body_string(sums))
        .mount(server)
        .await;
}

/// Mount `GET /releases/latest` returning `version`.
pub async fn mount_latest(server: &wiremock::MockServer, version: &str) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    Mock::given(method("GET"))
        .and(path("/releases/latest"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(release_json(&server.uri(), version)),
        )
        .mount(server)
        .await;
}

// PATH-override fake binary (used by the install.sh harness)

/// RAII guard that places a sh-script with name `name` at the head of `PATH`.
/// Restores `PATH` on drop.
///
/// All tests using this MUST be `#[serial]` because `PATH` is process-global.
pub struct FakeBinGuard {
    pub tmp: tempfile::TempDir,
    pub name: String,
    prev_path: OsString,
}

impl FakeBinGuard {
    /// Install a fake binary at `<tmp>/<name>` whose body is produced by
    /// `script_body(<tmp>)`, and prepend `<tmp>` to `PATH`.
    pub fn install<F>(name: &str, script_body: F) -> Self
    where
        F: FnOnce(&Path) -> String,
    {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let body = script_body(&dir);

        let script_path = dir.join(name);
        std::fs::write(&script_path, body).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let prev_path = std::env::var_os("PATH").unwrap_or_default();
        let mut new_path = OsString::from(&dir);
        new_path.push(":");
        new_path.push(&prev_path);
        // SAFETY: serial_test ensures no other thread races on PATH.
        unsafe { std::env::set_var("PATH", &new_path) };

        Self {
            tmp,
            name: name.to_string(),
            prev_path,
        }
    }

    /// The tempdir backing this guard (where canned response files can be
    /// written by tests, and where `<name>-args.log` is appended).
    pub fn dir(&self) -> PathBuf {
        self.tmp.path().to_path_buf()
    }

    /// Argv lines logged by the fake script — one line per invocation.
    pub fn args_log(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir().join(format!("{}-args.log", self.name)))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
}

impl Drop for FakeBinGuard {
    fn drop(&mut self) {
        // SAFETY: serial_test ensures no other thread races on PATH.
        unsafe { std::env::set_var("PATH", &self.prev_path) };
    }
}
