//! Core end-to-end KEYED managed-config tests — verified persist, rejected
//! persist-nothing, and the stripped-sidecar refusal. The harness (and the
//! seam/serial constraints every test here must follow) lives in
//! `signed_managed_config/common.rs`.
//!
//! Placement rule: this binary pins the review-cited security claims
//! (verify-persists / reject-persists-nothing / sidecar-deletion-refuses); new
//! keyed scenarios go in `signed_managed_config_extended.rs` unless they alter
//! one of those three claims.

#[path = "signed_managed_config/common.rs"]
mod common;

use common::{
    MANAGED, REQUIREMENTS_FAIL_CLOSED, dk_identity, forged_dk_body, install_test_key, reset,
    signed_dk_body, spawn_mock, test_home, write_dk_config,
};
use kigi_config::signed_policy;
use serial_test::serial;

/// A rejected envelope persists NOTHING: the prior principal's files survive
/// (verify-before-evict), no sidecar appears, and the marker is not rewritten.
#[tokio::test]
#[serial]
async fn rejected_signature_persists_nothing_and_records_no_marker() {
    let home = test_home().clone();
    reset(&home);
    let (kp, _pubkey) = install_test_key();

    // Prior trusted state: an earlier principal's files + marker.
    std::fs::write(home.join("managed_config.toml"), "[cli]\nprior = true\n").unwrap();
    std::fs::write(home.join("requirements.toml"), "[features]\n").unwrap();
    kigi_shell::config::mark_managed_config_synced(kigi_shell::config::SyncMarker {
        principal: Some("dep-old"),
        had_managed_config: true,
        had_requirements: true,
        key_fingerprint: None,
        fail_closed: false,
    });

    let url = spawn_mock(forged_dk_body(&kp, "dep-42"));
    write_dk_config(&home, &url, "dep-key-1");

    let wrote = kigi_shell::managed_config::sync()
        .await
        .expect("a rejected signature is a no-op, not a transport error");
    assert!(!wrote, "nothing may be persisted for a rejected envelope");

    assert_eq!(
        std::fs::read_to_string(home.join("managed_config.toml")).unwrap(),
        "[cli]\nprior = true\n",
        "verify-before-evict: the prior policy must survive the identity switch"
    );
    assert!(home.join("requirements.toml").exists());
    assert!(
        !home.join("managed_config.sig.json").exists(),
        "no sidecar may be written for a rejected envelope"
    );
    let marker = std::fs::read_to_string(home.join("managed_config_cache.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&marker).unwrap();
    assert_eq!(
        v["principal"].as_str(),
        Some("dep-old"),
        "the marker must not be rewritten for a rejected fetch: {marker}"
    );
}

/// A good envelope persists the policy files AND a sidecar that verifies over the
/// exact on-disk bytes; the cache then reads fresh and the gate allows.
#[tokio::test]
#[serial]
async fn verified_envelope_persists_policy_and_sidecar() {
    let home = test_home().clone();
    reset(&home);
    let (kp, pubkey) = install_test_key();

    let url = spawn_mock(signed_dk_body(
        &kp,
        "dep-42",
        Some(MANAGED),
        Some(REQUIREMENTS_FAIL_CLOSED),
    ));
    write_dk_config(&home, &url, "dep-key-1");

    let wrote = kigi_shell::managed_config::sync()
        .await
        .expect("a verified sync should succeed");
    assert!(wrote);

    let on_disk_managed = std::fs::read_to_string(home.join("managed_config.toml")).unwrap();
    let on_disk_requirements = std::fs::read_to_string(home.join("requirements.toml")).unwrap();
    let sidecar: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.join("managed_config.sig.json")).unwrap(),
    )
    .unwrap();
    let payload = signed_policy::verify_signed_payload(
        sidecar["signed_payload"].as_str().unwrap(),
        sidecar["signature"].as_str().unwrap(),
        &[("v1", &pubkey)],
    )
    .expect("the persisted sidecar must verify");
    assert_eq!(
        payload.managed_config.as_deref(),
        Some(on_disk_managed.as_str()),
        "the sidecar covers the exact on-disk managed_config bytes"
    );
    assert_eq!(
        payload.requirements.as_deref(),
        Some(on_disk_requirements.as_str()),
        "the sidecar covers the exact on-disk requirements bytes"
    );
    assert!(payload.fail_closed, "the signed opt-in is carried");

    assert!(
        !kigi_shell::config::is_managed_config_hard_stale_for(&dk_identity()),
        "a covered cache is not hard-stale"
    );
    assert!(
        kigi_shell::managed_config::managed_policy_gate().is_ok(),
        "an intact verified policy must not be refused"
    );
}

/// Deleting the sidecar under a fail-closed marker REFUSES at the gate (stripping it
/// must not downgrade enforcement to the forgeable marker path); the refetch triggers
/// fire so an online start self-heals.
#[tokio::test]
#[serial]
async fn deleted_sidecar_under_fail_closed_marker_refuses_at_gate() {
    let home = test_home().clone();
    reset(&home);
    let (kp, _pubkey) = install_test_key();

    let url = spawn_mock(signed_dk_body(
        &kp,
        "dep-42",
        Some(MANAGED),
        Some(REQUIREMENTS_FAIL_CLOSED),
    ));
    write_dk_config(&home, &url, "dep-key-1");
    kigi_shell::managed_config::sync()
        .await
        .expect("initial sync should succeed");
    assert!(
        kigi_shell::managed_config::managed_policy_gate().is_ok(),
        "the covered fail-closed policy is allowed"
    );

    std::fs::remove_file(home.join("managed_config.sig.json")).unwrap();

    assert!(
        kigi_shell::config::is_managed_config_hard_stale_for(&dk_identity()),
        "a stripped sidecar must trigger the session-start refetch"
    );
    assert!(
        kigi_shell::config::is_managed_config_stale_for(&dk_identity()),
        "the TIMER staleness sibling must fire too (background tick self-heal), even though the marker is timer-fresh"
    );
    let gate = kigi_shell::managed_config::managed_policy_gate();
    assert!(
        gate.is_err(),
        "a fail-closed policy without its sidecar must refuse offline"
    );
    assert!(
        gate.unwrap_err()
            .contains("Managed policy is required for this account"),
        "the refusal is the managed-policy gate message"
    );
}
