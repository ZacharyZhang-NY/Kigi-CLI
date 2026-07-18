//! Controllable raw HTTP/1.1 GitHub-Releases-shaped server shared by the
//! blitz download/install tests and the concurrent-update convergence tests.
//!
//! Serves the three routes the updater consumes:
//!
//! - `GET /releases/latest` and `GET /releases/tags/v{version}` — release
//!   JSON in the real GitHub wire shape
//!   (<https://docs.github.com/en/rest/releases/releases>):
//!   `{"tag_name":"v0.1.0","assets":[{"name":"...","browser_download_url":"..."}]}`
//! - `GET /dl/{version}/SHA256SUMS` — checksum manifest for the archive
//! - `GET /dl/{version}/kigi-{version}-{triple}.tar.gz` — the archive itself
//!
//! The archive route can serve the real archive, truncate the body, serve a
//! right-length-but-garbage body (defeated by the SHA-256 gate), serve a
//! correctly-checksummed archive whose binary fails to run (defeated by the
//! smoke test), or hang mid-transfer — for both the parallel byte-range path
//! and the single-connection path. It also counts archive-serving GETs (HEAD
//! probes and metadata routes are excluded) so tests can assert how many
//! downloads actually happened, and supports a "slow" mode that widens the
//! race window so concurrent installers genuinely overlap in flight.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{archive_name, make_release_archive, sha256_hex};

/// How the server corrupts (or doesn't) the next archive download.
#[derive(Clone, Copy, Debug)]
pub enum Mode {
    /// Serve the real archive correctly.
    Full,
    /// Serve a right-length body of wrong bytes — SHA256SUMS still lists the
    /// GOOD archive's hash, so the checksum gate must reject it.
    Garbage,
    /// Serve a correctly-checksummed archive whose `kigi` binary exits
    /// non-zero — only the smoke test can reject it.
    BadBinary,
    /// Advertise the full length but send only `k` bytes then close the socket
    /// (silent truncation: premature EOF / short range chunk).
    Truncate(usize),
    /// Send `k` bytes then hang, so a client-side timeout cancels mid-transfer.
    Hang(usize),
}

/// Precomputed per-version fixtures.
struct VersionFixture {
    /// Real archive: tar.gz containing `kigi` = the good binary body.
    good_archive: Arc<Vec<u8>>,
    /// Same-shape archive whose `kigi` exits 1; its hash is served in
    /// SHA256SUMS while `Mode::BadBinary` is active.
    bad_archive: Arc<Vec<u8>>,
}

struct ServerState {
    versions: HashMap<String, VersionFixture>,
    /// The good binary body used to synthesize fixtures for versions
    /// requested but not yet registered.
    default_binary: Vec<u8>,
    latest: String,
    mode: Mode,
}

impl ServerState {
    fn fixture(&mut self, version: &str) -> &VersionFixture {
        if !self.versions.contains_key(version) {
            let good = make_release_archive(&self.default_binary);
            let bad = make_release_archive(b"#!/bin/sh\nexit 1\n");
            self.versions.insert(
                version.to_string(),
                VersionFixture {
                    good_archive: Arc::new(good),
                    bad_archive: Arc::new(bad),
                },
            );
        }
        &self.versions[version]
    }
}

pub struct ArtifactServer {
    addr: std::net::SocketAddr,
    state: Arc<Mutex<ServerState>>,
    shutdown: Arc<AtomicBool>,
    gets: Arc<AtomicUsize>,
    slow: Arc<AtomicBool>,
}

impl ArtifactServer {
    /// Start a server whose release archives contain `binary_body` as the
    /// `kigi` binary. `latest` starts as `0.0.0`; set it with
    /// [`ArtifactServer::set_latest`] before exercising latest-based flows.
    pub fn start(binary_body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(ServerState {
            versions: HashMap::new(),
            default_binary: binary_body,
            latest: "0.0.0".to_string(),
            mode: Mode::Full,
        }));
        let shutdown = Arc::new(AtomicBool::new(false));
        let gets = Arc::new(AtomicUsize::new(0));
        let slow = Arc::new(AtomicBool::new(false));

        let st = state.clone();
        let sd = shutdown.clone();
        let gc = gets.clone();
        let sl = slow.clone();
        std::thread::spawn(move || {
            while !sd.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let st = st.clone();
                        let sd = sd.clone();
                        let gc = gc.clone();
                        let sl = sl.clone();
                        std::thread::spawn(move || handle_connection(stream, st, sd, gc, sl));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            state,
            shutdown,
            gets,
            slow,
        }
    }

    /// Base URL to hand the updater (`…/releases`, mirroring the production
    /// `https://api.github.com/repos/{owner}/{repo}/releases`).
    pub fn base(&self) -> String {
        format!("http://{}/releases", self.addr)
    }

    /// Version served by `GET /releases/latest`.
    pub fn set_latest(&self, version: &str) {
        self.state.lock().unwrap().latest = version.to_string();
    }

    pub fn set_mode(&self, mode: Mode) {
        self.state.lock().unwrap().mode = mode;
    }

    /// Length of the (good) archive for `version` — corruption offsets for
    /// [`Mode::Truncate`]/[`Mode::Hang`] are positions within this body.
    pub fn archive_len(&self, version: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .fixture(version)
            .good_archive
            .len()
    }

    /// Number of archive-serving GET requests handled so far (HEAD probes
    /// from the parallel-download path and metadata/SHA256SUMS routes are
    /// excluded). Tests use this to assert how many downloads actually
    /// happened — e.g. that a sequential updater converged onto an
    /// already-installed binary without re-downloading. One download may span
    /// multiple GETs when the parallel byte-range path splits it, so tests
    /// asserting exact counts use a small artifact (single-connection path,
    /// 1 GET per download).
    pub fn request_count(&self) -> usize {
        self.gets.load(Ordering::Relaxed)
    }

    /// When enabled, hold each archive response open ~500ms before sending
    /// the body. This keeps an installer in flight long enough for concurrent
    /// installers to genuinely overlap even on a heavily loaded CI host — a
    /// too-short hold would let race tests run the installers back-to-back
    /// and never exercise the concurrent window.
    pub fn set_slow(&self, slow: bool) {
        self.slow.store(slow, Ordering::Relaxed);
    }
}

impl Drop for ArtifactServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// Parse `Range: bytes=a-b` from a raw request header block (case-insensitive).
fn parse_range(request: &str) -> Option<(usize, usize)> {
    for line in request.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("range:") {
            let spec = rest.trim().strip_prefix("bytes=")?;
            let (a, b) = spec.split_once('-')?;
            return Some((a.trim().parse().ok()?, b.trim().parse().ok()?));
        }
    }
    None
}

/// The request path, without query string.
fn parse_path(request: &str) -> String {
    request
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_string()
}

/// Release JSON for `version` with asset URLs rooted at this server.
fn release_json(addr: &std::net::SocketAddr, version: &str) -> String {
    let name = archive_name(version);
    format!(
        r#"{{"tag_name":"v{version}","draft":false,"prerelease":false,"assets":[{{"name":"{name}","browser_download_url":"http://{addr}/dl/{version}/{name}"}},{{"name":"SHA256SUMS","browser_download_url":"http://{addr}/dl/{version}/SHA256SUMS"}}]}}"#
    )
}

fn write_simple_response(stream: &mut TcpStream, status: &str, body: &[u8], is_head: bool) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    if !is_head {
        let _ = stream.write_all(body);
    }
    let _ = stream.flush();
}

fn handle_connection(
    mut stream: TcpStream,
    state: Arc<Mutex<ServerState>>,
    shutdown: Arc<AtomicBool>,
    gets: Arc<AtomicUsize>,
    slow: Arc<AtomicBool>,
) {
    // A stream accepted from a non-blocking listener can inherit non-blocking
    // mode; force blocking so large `write_all`s don't short-write on WouldBlock.
    let _ = stream.set_nonblocking(false);
    // Avoid Nagle/delayed-ACK stalls on the header-then-body writes.
    let _ = stream.set_nodelay(true);

    // Read the request header block (until CRLFCRLF). Bodies are never sent by
    // the client, so headers are all we need.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if buf.len() > 64 * 1024 {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let request = String::from_utf8_lossy(&buf).to_string();
    let is_head = request.starts_with("HEAD");
    let path = parse_path(&request);
    let range = parse_range(&request);
    let addr = stream.local_addr().unwrap();

    // ── Metadata routes ─────────────────────────────────────────────────────
    if path == "/releases/latest" {
        let latest = state.lock().unwrap().latest.clone();
        let body = release_json(&addr, &latest);
        write_simple_response(&mut stream, "200 OK", body.as_bytes(), is_head);
        return;
    }
    if let Some(tag) = path.strip_prefix("/releases/tags/v") {
        let body = release_json(&addr, tag);
        write_simple_response(&mut stream, "200 OK", body.as_bytes(), is_head);
        return;
    }

    // ── Asset routes: /dl/{version}/{name} ──────────────────────────────────
    let Some(rest) = path.strip_prefix("/dl/") else {
        write_simple_response(&mut stream, "404 Not Found", b"not found", is_head);
        return;
    };
    let Some((version, asset)) = rest.split_once('/') else {
        write_simple_response(&mut stream, "404 Not Found", b"not found", is_head);
        return;
    };

    let (good, bad, mode) = {
        let mut st = state.lock().unwrap();
        let mode = st.mode;
        let fixture = st.fixture(version);
        (
            fixture.good_archive.clone(),
            fixture.bad_archive.clone(),
            mode,
        )
    };

    if asset == "SHA256SUMS" {
        // BadBinary serves the bad archive WITH its correct hash (a release
        // whose binary is broken but whose checksums are fine); every other
        // mode lists the good archive's hash so in-transit corruption is
        // caught by the checksum gate.
        let hashed: &[u8] = match mode {
            Mode::BadBinary => &bad,
            _ => &good,
        };
        let body = format!("{}  {}\n", sha256_hex(hashed), archive_name(version));
        write_simple_response(&mut stream, "200 OK", body.as_bytes(), is_head);
        return;
    }

    if asset != archive_name(version) {
        write_simple_response(&mut stream, "404 Not Found", b"not found", is_head);
        return;
    }

    // ── Archive body with corruption modes ──────────────────────────────────
    // Count only body-serving GETs; the parallel path's HEAD probe is excluded.
    if !is_head {
        gets.fetch_add(1, Ordering::Relaxed);
    }

    let body: &[u8] = match mode {
        Mode::BadBinary => &bad,
        _ => &good,
    };
    let total = body.len();

    // Determine the byte slice this request is for, plus the length we will
    // claim in Content-Length.
    let (slice_start, slice_end_excl) = match range {
        Some((a, b)) => (a.min(total), (b + 1).min(total)),
        None => (0, total),
    };
    let claimed_len = slice_end_excl - slice_start;

    // For truncation/hang, `k` is a GLOBAL cutoff across the whole archive:
    // a slice that reaches past byte `k` is sent short, so the parallel path's
    // later chunk (or the single-connection body) is the one truncated.
    let send_end = match mode {
        Mode::Truncate(k) | Mode::Hang(k) => slice_end_excl.min(k).max(slice_start),
        _ => slice_end_excl,
    };
    // `payload` is what we actually transmit before any early close; for the
    // truncated modes it may be shorter than the advertised `claimed_len`.
    let payload: Vec<u8> = match mode {
        Mode::Garbage => {
            let mut bad_bytes = b"not the archive you checksummed".to_vec();
            bad_bytes.resize(claimed_len, b'\n');
            bad_bytes
        }
        _ => body[slice_start..send_end].to_vec(),
    };

    // Status line + headers. For range requests we answer 206; HEAD is 200.
    let mut head = String::new();
    if range.is_some() && !is_head {
        head.push_str("HTTP/1.1 206 Partial Content\r\n");
        head.push_str(&format!(
            "Content-Range: bytes {}-{}/{}\r\n",
            slice_start,
            slice_end_excl.saturating_sub(1),
            total
        ));
    } else {
        head.push_str("HTTP/1.1 200 OK\r\n");
        head.push_str("Accept-Ranges: bytes\r\n");
    }
    // Always advertise the (claimed) full length so a truncated transfer is a
    // genuine premature EOF rather than a short-but-consistent body.
    head.push_str(&format!("Content-Length: {}\r\n", claimed_len));
    head.push_str("Connection: close\r\n\r\n");

    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    if is_head {
        let _ = stream.flush();
        return;
    }

    match mode {
        Mode::Full | Mode::Garbage | Mode::BadBinary => {
            // Hold the connection open longer so concurrent installers
            // genuinely overlap mid-download (see `set_slow`).
            if slow.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
            }
            let _ = stream.write_all(&payload);
        }
        Mode::Truncate(_) => {
            // Send the (possibly short) payload then drop the connection without
            // meeting Content-Length — the client sees a premature EOF.
            let _ = stream.write_all(&payload);
        }
        Mode::Hang(_) => {
            let _ = stream.write_all(&payload);
            let _ = stream.flush();
            // Hold the connection open longer than any client-side cancel
            // timeout so the client times out and cancels (a genuine mid-flight
            // cancel rather than a server-side close).
            for _ in 0..30 {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    let _ = stream.flush();
}
