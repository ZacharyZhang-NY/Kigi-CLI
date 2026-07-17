//! Shared harness for the KEYED managed-config integration tests: a test-only
//! signing seam injects a throwaway trusted key so the real
//! sync → verify → persist → gate paths run with verification ACTIVE (the dark
//! behavior is covered by `team_managed_config.rs`).
//!
//! Every test MUST be `#[serial]` and install its own seam keys first: the test
//! binary shares one process-global `KIGI_SHARE_DIR`, process env, and key override.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::OnceLock;

use base64::Engine as _;
use kigi_config::signed_policy::{self, SignedPayload};

pub const MANAGED: &str = "[cli]\ntheme = \"dark\"\n";
pub const REQUIREMENTS_FAIL_CLOSED: &str = "fail_closed = true\n[features]\nweb_fetch = false\n";
/// Far-future expiry — envelopes in these tests never expire.
pub const TEST_EXPIRES_AT: u64 = 4_000_000_000;
/// The sole trusted key id: [`install_test_key`] installs it and [`sign_envelope`]
/// signs under it, so the two can't drift.
pub const TEST_KEY_ID: &str = "v1";

/// Shared temp dir used as KIGI_SHARE_DIR for the whole test binary (the kigi_home
/// `OnceLock` only allows one value per process); scrubs the env this suite
/// depends on before any test thread reads it.
pub fn test_home() -> &'static PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let path = tempfile::TempDir::new().unwrap().keep();
        // SAFETY: set once at init before other threads read the vars.
        unsafe {
            std::env::set_var("KIGI_SHARE_DIR", &path);
            for var in [
                "KIGI_DEPLOYMENT_KEY",
                "KIGI_MANAGED_CONFIG",
                "KIGI_DEPLOYMENT_CONFIG_REFRESH_INTERVAL_SECS",
                "KIGI_DEPLOYMENT_CONFIG_CACHE_TTL_SECS",
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ] {
                std::env::remove_var(var);
            }
            std::env::set_var("KIGI_DEPLOYMENT_CONFIG_BACKOFF_MS", "10");
        }
        path
    })
}

pub fn reset(home: &std::path::Path) {
    for f in [
        "config.toml",
        "auth.json",
        "managed_config.toml",
        "requirements.toml",
        "managed_config_cache.json",
        "managed_config.lock",
        "managed_config.sig.json",
    ] {
        let _ = std::fs::remove_file(home.join(f));
    }
}

/// Minimal mock deployment-config server serving `body` to every request.
pub fn spawn_mock(body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Drain the request headers before responding.
            let mut reader = BufReader::new(&mut stream);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim_end().is_empty() {
                    break;
                }
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{addr}/deployment/config")
}

/// Endpoint config with a `deployment_key` (the only principal that can own
/// managed config now).
pub fn write_dk_config(home: &std::path::Path, managed_config_url: &str, deployment_key: &str) {
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[endpoints]\nmanaged_config_url = \"{managed_config_url}\"\ndeployment_key = \"{deployment_key}\"\n"
        ),
    )
    .unwrap();
}

/// A fresh Ed25519 keypair plus its raw public key, installed as the sole trusted
/// key ([`TEST_KEY_ID`]) via the test seam.
pub fn install_test_key() -> (ring::signature::Ed25519KeyPair, Vec<u8>) {
    use ring::signature::KeyPair as _;
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let kp = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let pubkey = kp.public_key().as_ref().to_vec();
    signed_policy::test_seam::set_embedded_keys(&[(TEST_KEY_ID, &pubkey)]);
    assert!(
        signed_policy::verification_active(),
        "the seam must arm verification"
    );
    (kp, pubkey)
}

/// Serialize → sign → base64: the one `signatures[]` entry for `payload`, signed
/// by `kp` under the payload's own `key_id` (the untrusted outer hint can't drift
/// from the signed one).
pub fn sign_envelope(
    kp: &ring::signature::Ed25519KeyPair,
    payload: &SignedPayload,
) -> serde_json::Value {
    let signed_payload = serde_json::to_string(payload).unwrap();
    let signature = base64::engine::general_purpose::STANDARD
        .encode(kp.sign(signed_payload.as_bytes()).as_ref());
    serde_json::json!({
        "signed_payload": signed_payload,
        "signature": signature,
        "key_id": payload.key_id.as_str(),
    })
}

/// A deployment-key config response signed by `kp` under [`TEST_KEY_ID`]. The
/// body's legacy fields mirror the payload exactly (the client rejects a divergence).
pub fn signed_dk_body(
    kp: &ring::signature::Ed25519KeyPair,
    deployment_id: &str,
    managed: Option<&str>,
    requirements: Option<&str>,
) -> String {
    let payload = SignedPayload {
        version: kigi_config::signed_policy::SIGNED_PAYLOAD_VERSION,
        deployment_id: Some(deployment_id.to_owned()),
        team_id: None,
        managed_config: managed.map(str::to_owned),
        requirements: requirements.map(str::to_owned),
        fail_closed: requirements.is_some_and(kigi_config::fail_closed_flag_from_str),
        expires_at: TEST_EXPIRES_AT,
        key_id: TEST_KEY_ID.into(),
    };
    serde_json::json!({
        "deployment_id": deployment_id,
        "team_id": serde_json::Value::Null,
        "managed_config": managed,
        "requirements": requirements,
        "signatures": [sign_envelope(kp, &payload)],
    })
    .to_string()
}

/// A [`signed_dk_body`] (managed config only) with the signature corrupted —
/// valid base64, wrong bytes — so the verifier must reject the envelope.
pub fn forged_dk_body(kp: &ring::signature::Ed25519KeyPair, deployment_id: &str) -> String {
    let mut body: serde_json::Value =
        serde_json::from_str(&signed_dk_body(kp, deployment_id, Some(MANAGED), None)).unwrap();
    body["signatures"][0]["signature"] = base64::engine::general_purpose::STANDARD
        .encode([0u8; 64])
        .into();
    body.to_string()
}

/// The live serving identity (the configured deployment key's fingerprint).
pub fn dk_identity() -> kigi_shell::config::ServingIdentity {
    kigi_shell::managed_config::current_serving_identity()
}

/// True when `path` reads despite `chmod 000` (root / DAC bypass): chmod-based
/// tests must then skip LOUDLY — a silent return would pass forever. CI runners
/// are assumed unprivileged; the shared guard keeps skips greppable.
#[cfg(unix)]
#[allow(dead_code)]
pub fn skip_as_root(path: &std::path::Path, test: &str) -> bool {
    let skip = std::fs::read_to_string(path).is_ok();
    if skip {
        eprintln!("{test}: skipping — chmod unreadability not enforced (running as root?)");
    }
    skip
}
