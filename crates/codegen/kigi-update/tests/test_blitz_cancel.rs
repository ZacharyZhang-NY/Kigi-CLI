//! Blitz harness: hammer the download + install lifecycle while injecting a
//! truncation / corruption / cancel at every point, and after every iteration
//! assert the single invariant that makes the brick impossible:
//!
//! > `~/.kigi/bin/kigi` resolves to a binary that passes the smoke-test, OR it
//! > is still the previous-good binary. It is never a broken/partial binary,
//! > and a `.tmp` never masquerades as the active binary.
//!
//! The invariant is checked by RE-RESOLVING the symlink and RE-RUNNING the
//! binary from disk every time — never by re-reading a value the harness set.
//!
//! A controllable raw HTTP/1.1 GitHub-Releases-shaped server serves release
//! JSON, SHA256SUMS, and the archive, and can truncate the archive body,
//! close the connection early, serve a right-length-but-garbage body (caught
//! by the SHA-256 gate), serve a correctly-checksummed archive whose binary
//! fails to run (caught by the smoke test), or hang mid-transfer — for both
//! the parallel byte-range path and the single-connection path.

#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serial_test::serial;

use common::artifact_server::{ArtifactServer, Mode};
use common::{
    can_exec_shell_scripts, host_platform, make_update_config, reset_home, small_good_artifact,
    test_home,
};
use kigi_update::auto_update::install_internal_from_base;

// Artifacts + fixtures

/// A real executable whose ARCHIVE clears the 16 MiB parallel threshold (at
/// least 2 chunks), so the parallel byte-range path is exercised. The shell
/// exits on line 2, never reading the padding — which is pseudo-random bytes
/// so gzip cannot compress the archive below the threshold.
fn large_good_artifact() -> Vec<u8> {
    let mut v = b"#!/bin/sh\nexit 0\n".to_vec();
    v.reserve(34 * 1024 * 1024);
    // xorshift64* keeps the padding incompressible without an RNG dependency.
    let mut x: u64 = 0x243F6A8885A308D3;
    while v.len() < 34 * 1024 * 1024 {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        v.extend_from_slice(&x.wrapping_mul(0x2545F4914F6CDD1D).to_le_bytes());
    }
    v
}

/// Seed a previous-good versioned binary + the managed `kigi` symlink.
/// Returns the absolute path of the seeded binary.
fn seed_previous_good(home: &Path, version: &str, platform: &str) -> PathBuf {
    let downloads = home.join("downloads");
    let bin = home.join("bin");
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::create_dir_all(&bin).unwrap();

    let prev = downloads.join(format!("kigi-{version}-{platform}"));
    std::fs::write(&prev, small_good_artifact()).unwrap();
    std::fs::set_permissions(&prev, std::fs::Permissions::from_mode(0o755)).unwrap();

    let rel = format!("../downloads/kigi-{version}-{platform}");
    let link = bin.join("kigi");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&rel, &link).unwrap();
    dunce::canonicalize(&prev).unwrap()
}

/// What the active `kigi` should resolve to after an install attempt.
#[derive(Clone, Copy, PartialEq)]
enum Expect {
    /// The new version was installed and activated.
    NewBinary,
    /// The install was rejected/cancelled; the previous-good binary stays live.
    PreviousGood,
}

/// THE invariant. Re-resolves the on-disk symlink and RE-EXECUTES the resolved
/// binary; never inspects a harness-held value. Guarantees the active managed
/// link is always runnable and is never a `.tmp` or a partial file.
fn assert_invariant(home: &Path, prev_good: &Path, new_binary: &Path, expect: Expect) {
    let link = home.join("bin").join("kigi");
    assert!(link.is_symlink(), "kigi must remain a symlink");

    // Resolve from disk. canonicalize fails on a dangling link — that alone
    // would be a brick.
    let resolved = dunce::canonicalize(&link)
        .unwrap_or_else(|e| panic!("active kigi symlink does not resolve: {e}"));

    // A `.tmp` file must never be the live target.
    let resolved_name = resolved.file_name().unwrap().to_string_lossy().to_string();
    assert!(
        !resolved_name.contains(".tmp"),
        "active kigi must not be a temp file: {resolved_name}"
    );

    // Re-run the resolved binary from disk: the active link must always run.
    let ran_ok = std::process::Command::new(&resolved)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(
        ran_ok,
        "active kigi must pass the smoke-test, but {} did not run",
        resolved.display()
    );

    match expect {
        Expect::NewBinary => assert_eq!(
            resolved,
            dunce::canonicalize(new_binary).unwrap(),
            "expected the newly-installed binary to be active"
        ),
        Expect::PreviousGood => assert_eq!(
            resolved, prev_good,
            "expected the previous-good binary to stay active after a rejected install"
        ),
    }
}

/// Run one install attempt against `server` in `mode`, optionally cancelling it
/// after `cancel_after`, then assert the invariant.
async fn run_one(
    server: &ArtifactServer,
    mode: Mode,
    version: &str,
    cancel_after: Option<Duration>,
) {
    let home = test_home();
    reset_home();
    let platform = host_platform();
    let prev_good = seed_previous_good(home, "0.1.100", &platform);
    let new_binary = home
        .join("downloads")
        .join(format!("kigi-{version}-{platform}"));
    let cfg = make_update_config("stable");

    server.set_mode(mode);

    let base = server.base();
    let install = install_internal_from_base(Some(version), &cfg, &base);
    let expect = match (mode, cancel_after) {
        (Mode::Full, None) => {
            install.await.expect("full artifact install should succeed");
            Expect::NewBinary
        }
        (_, Some(deadline)) => {
            // Cancel mid-flight by dropping the future at the timeout.
            let _ = tokio::time::timeout(deadline, install).await;
            Expect::PreviousGood
        }
        _ => {
            let result = install.await;
            assert!(
                result.is_err(),
                "corrupt artifact ({mode:?}) must not install successfully"
            );
            Expect::PreviousGood
        }
    };

    assert_invariant(home, &prev_good, &new_binary, expect);
}

// Deterministic matrix — single-connection path (small archive)

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn blitz_single_connection_matrix() {
    if !can_exec_shell_scripts() {
        eprintln!("skipping: shell scripts cannot execute in this sandbox");
        return;
    }
    let server = ArtifactServer::start(small_good_artifact());
    let len = server.archive_len("0.1.181");

    // Happy path first so we know the symlink CAN move to the new binary.
    run_one(&server, Mode::Full, "0.1.181", None).await;

    // Right-length garbage — caught by the SHA-256 gate.
    run_one(&server, Mode::Garbage, "0.1.181", None).await;

    // Correctly-checksummed archive with a broken binary — caught by the
    // smoke test.
    run_one(&server, Mode::BadBinary, "0.1.181", None).await;

    // Premature EOF at several offsets — caught by the length/transport
    // checks (and the checksum gate as belt-and-suspenders).
    for k in [0usize, 1, len / 2, len.saturating_sub(1)] {
        run_one(&server, Mode::Truncate(k), "0.1.181", None).await;
    }

    // Cancel mid-transfer at several offsets (incl. before any byte and before
    // the HEAD completes), each dropping the in-flight future.
    for k in [0usize, len / 2, len.saturating_sub(1)] {
        run_one(
            &server,
            Mode::Hang(k),
            "0.1.181",
            Some(Duration::from_millis(300)),
        )
        .await;
    }

    // A clean serve still succeeds after the failure matrix. NOTE: run_one
    // calls reset_home() at the start of every case, so this checks the happy
    // path stays reachable — not recovery over a dirty dir. The genuine
    // recovery-without-reset assertion lives in
    // smoke_and_checksum_failures_keep_previous_good_then_recover.
    run_one(&server, Mode::Full, "0.1.182", None).await;
}

// Deterministic matrix — parallel byte-range path (>= 16 MiB archive)

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn blitz_parallel_path_matrix() {
    if !can_exec_shell_scripts() {
        eprintln!("skipping: shell scripts cannot execute in this sandbox");
        return;
    }
    let server = ArtifactServer::start(large_good_artifact());
    let len = server.archive_len("0.1.181");
    assert!(
        len >= 16 * 1024 * 1024,
        "archive must clear the parallel threshold (got {len} bytes)"
    );

    // Happy path through the parallel reassembly.
    run_one(&server, Mode::Full, "0.1.181", None).await;

    // Right-length garbage reassembled from range chunks — checksum catches.
    run_one(&server, Mode::Garbage, "0.1.181", None).await;

    // Short chunk inside the range / set_len zero region. With Content-Length
    // present (the blitz server always sends it), a premature close surfaces as
    // a reqwest stream error that rejects the chunk; the parallel path falls
    // back to single-connection, which hits the same truncation.
    for k in [0usize, 1024, len / 3, len - 4096] {
        run_one(&server, Mode::Truncate(k), "0.1.181", None).await;
    }

    // Cancel mid-chunk.
    run_one(
        &server,
        Mode::Hang(len / 4),
        "0.1.181",
        Some(Duration::from_millis(400)),
    )
    .await;

    // Clean serve recovers.
    run_one(&server, Mode::Full, "0.1.182", None).await;
}

// Checksum + smoke-test rejections keep previous-good, then recover WITHOUT
// a reset in between.

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn smoke_and_checksum_failures_keep_previous_good_then_recover() {
    if !can_exec_shell_scripts() {
        eprintln!("skipping: shell scripts cannot execute in this sandbox");
        return;
    }
    let server = ArtifactServer::start(small_good_artifact());
    let home = test_home();
    reset_home();
    let platform = host_platform();
    let prev_good = seed_previous_good(home, "0.1.100", &platform);
    let cfg = make_update_config("stable");
    let base = server.base();
    let new_binary = home
        .join("downloads")
        .join(format!("kigi-0.1.181-{platform}"));

    // Checksum failure (garbage body) keeps previous good.
    server.set_mode(Mode::Garbage);
    let result = install_internal_from_base(Some("0.1.181"), &cfg, &base).await;
    assert!(result.is_err(), "garbage archive must not install");
    assert_invariant(home, &prev_good, &new_binary, Expect::PreviousGood);

    // Smoke-test failure (valid checksum, broken binary) keeps previous good.
    server.set_mode(Mode::BadBinary);
    let result = install_internal_from_base(Some("0.1.181"), &cfg, &base).await;
    assert!(result.is_err(), "broken binary must not install");
    assert_invariant(home, &prev_good, &new_binary, Expect::PreviousGood);

    // A subsequent clean serve must succeed over the SAME dirty state.
    server.set_mode(Mode::Full);
    install_internal_from_base(Some("0.1.181"), &cfg, &base)
        .await
        .expect("clean serve after failures should succeed");
    assert_invariant(home, &prev_good, &new_binary, Expect::NewBinary);
}

// Bounded randomized fuzz (CI) + ignored stress (1e5+ iterations).

/// Cheap deterministic PRNG so the fuzz needs no extra dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

async fn fuzz_loop(iterations: usize, seed: u64) {
    let server = ArtifactServer::start(small_good_artifact());
    let len = server.archive_len("0.1.181");
    let mut rng = Rng(seed);

    for i in 0..iterations {
        let version = if i % 2 == 0 { "0.1.181" } else { "0.1.182" };
        // Periodically verify a clean serve still installs (recovery), but keep
        // the bulk on the fast corruption/cancel paths so the loop stays cheap
        // enough for high iteration counts.
        if i % 10 == 9 {
            run_one(&server, Mode::Full, version, None).await;
            continue;
        }
        match rng.below(4) {
            0 => run_one(&server, Mode::Garbage, version, None).await,
            1 => run_one(&server, Mode::BadBinary, version, None).await,
            2 => {
                // k in [0, len): always strictly truncating (k == len would be
                // a complete transfer).
                let k = rng.below(len);
                run_one(&server, Mode::Truncate(k), version, None).await;
            }
            _ => {
                // k in [0, len): Hang holds the socket after k bytes without
                // ever meeting Content-Length, so the client always cancels
                // mid-flight. k == len would transmit the whole body, letting
                // the install complete and the swap land before the deadline —
                // contradicting run_one's PreviousGood expectation (the same
                // reason the Truncate branch above uses rng.below(len)).
                let k = rng.below(len);
                run_one(
                    &server,
                    Mode::Hang(k),
                    version,
                    Some(Duration::from_millis(80)),
                )
                .await;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn blitz_fuzz_bounded() {
    if !can_exec_shell_scripts() {
        eprintln!("skipping: shell scripts cannot execute in this sandbox");
        return;
    }
    // Kept bounded so CI stays fast; the exhaustive run is the ignored test
    // below. Every iteration still re-resolves and re-runs the on-disk binary.
    fuzz_loop(120, 0x9E3779B97F4A7C15).await;
}

/// The "test it a million times, cancelling at every point" stress run. Gated
/// behind `#[ignore]`; invoke via
/// `cargo nextest run -p kigi-update --run-ignored all`.
#[tokio::test(flavor = "multi_thread")]
#[serial]
#[ignore = "stress: 100k iterations"]
async fn blitz_fuzz_stress() {
    if !can_exec_shell_scripts() {
        eprintln!("skipping: shell scripts cannot execute in this sandbox");
        return;
    }
    let iterations: usize = std::env::var("KIGI_BLITZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    fuzz_loop(iterations, 0xDEADBEEFCAFEF00D).await;
}
