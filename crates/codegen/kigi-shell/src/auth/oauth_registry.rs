//! Process-global per-provider OAuth `AuthManager` pool for INFERENCE-time auth.
//!
//! A session binds its primary (Kimi / first-party) [`AuthManager`] for the
//! subscription path, but a `uses_oauth` platform that carries an
//! [`kigi_models::OAuthConfig`] (xai-grok today) needs its OWN scope-keyed
//! manager for every per-turn decision — bearer resolution, proactive /
//! on-expiry refresh, and 401 recovery. Reusing the Kimi manager for a grok
//! turn would transmit the Kimi subscription bearer to `api.x.ai` (a
//! cross-provider leak, guaranteed 401) and, without proactive refresh, would
//! 401 every turn once the ~1h grok token expired until a process restart.
//!
//! The pool is the SINGLE SOURCE OF TRUTH: one long-lived `AuthManager` per
//! generic-oauth scope, each wired with the SAME lifecycle as the primary Kimi
//! manager (`configure_refresher()` + `start_proactive_refresh()`) so the
//! on-disk token stays fresh and a 401 recovers via the provider's own manager.
//! Managers are built ON DEMAND from the on-disk token ([`global_manager_for`]),
//! so a login landing AFTER a session spawned self-heals — no frozen per-session
//! snapshot.
//!
//! ROUTING LIVES ELSEWHERE. This module is only the pool; the decision of which
//! credential governs a request belongs to the single chokepoint,
//! [`crate::auth::credential_authority::CredentialAuthority`]. Keeping the two
//! apart is deliberate: three rounds of leaks came from routing rules being
//! re-derived per call site.
//!
//! SECURITY: access/refresh tokens and resolved bearers are NEVER logged here.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

use crate::auth::AuthManager;

/// Process-wide pool of live per-scope OAuth managers.
///
/// Auth is process-global (one user), so a single manager per scope is correct
/// and lets the proactive-refresh task start exactly once per scope no matter
/// how many sessions spawn. Keyed by the OAuth `scope_key` (`oauth/xai`, …).
fn oauth_manager_pool() -> &'static Mutex<HashMap<&'static str, Arc<AuthManager>>> {
    static POOL: OnceLock<Mutex<HashMap<&'static str, Arc<AuthManager>>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The kigi home EVERY OAuth-provider construction in this crate resolves from
/// — the pool, the catalog fetch's per-platform token resolution, the
/// aux/summary token routing and the session's inference manager — so they can
/// never read different homes.
///
/// Production: [`crate::util::kigi_home::kigi_home`]. LIB TESTS: a
/// per-process path under the system temp dir that is deliberately **never
/// created**. The pool is process-global and every manager it builds starts a
/// never-cancelled proactive-refresh loop, so a unit test resolving the real
/// `~/.kigi` would read the developer's stored OAuth tokens and, 60 s later,
/// fire REAL refresh requests against them. Deliberately not a per-test opt-in
/// that can be forgotten: `kigi_home()` is itself a `OnceLock` an earlier test
/// has usually already resolved to the real home, so setting `KIGI_SHARE_DIR`
/// in a test cannot pin it after the fact.
///
/// M4 — THE LIMIT, STATED: `cfg(test)` is set only for THIS crate's `--lib`
/// tests. `crates/codegen/kigi-shell/tests/*.rs` link the library built WITHOUT
/// it, so for an integration test this resolves the real home unless that test
/// binary itself isolates one, which it must do through the two overrides the
/// auth stack already honours and BEFORE anything resolves `kigi_home()`:
/// `KIGI_SHARE_DIR` (read by `kigi_home()`, a `OnceLock`) or `KIGI_AUTH_PATH`
/// (read by [`AuthManager::new_oauth_provider`], which pins the token file
/// outright and so overrides this home entirely). 12 of the 28 integration
/// binaries under `crates/codegen/kigi-shell/tests/` set `KIGI_SHARE_DIR`; the
/// other 16 never reach an OAuth-platform inference path today, which is a
/// property of those tests, not a guarantee of this function. No
/// production-readable env override is added here on purpose: a knob that
/// redirects where OAuth tokens are read from is not worth a test convenience.
///
/// Nothing is created here, and a `static OnceLock<TempDir>` would be wrong: a
/// static is never dropped, so it would leak one temp directory per test binary,
/// against the project's "tests are TempDir self-cleaning" discipline. Nothing
/// in the lib-test suite creates the directory either: a manager reads a missing
/// `auth.json` as "no session", and the only two paths that WRITE one are a
/// successful token refresh (which needs a stored refresh token that by
/// construction does not exist here) and a completed device login
/// ([`crate::agent::mvp_agent::MvpAgent::authenticate_oauth_platform`], which
/// targets this same home). Both require the network, so no lib test performs
/// either. That is an observation about the suite, not an invariant of this
/// function — [`tests::test_pool_home_is_disposable_and_never_the_real_home`]
/// asserts the directory does not exist and is the tripwire if one ever does
/// (the path is per-PROCESS under the system temp dir, so the blast radius of a
/// future login-driving test is one disposable directory, never `~/.kigi`).
pub(crate) fn pool_home() -> std::path::PathBuf {
    #[cfg(test)]
    {
        std::env::temp_dir().join(format!("kigi-oauth-pool-test-{}", std::process::id()))
    }
    #[cfg(not(test))]
    crate::util::kigi_home::kigi_home()
}

/// Get-or-create the process-global manager for `oauth`, wiring the same
/// refresher + proactive-refresh lifecycle as the primary Kimi manager the
/// FIRST time a scope is seen. The manager reads the on-disk token at
/// construction (thereafter kept fresh by the proactive-refresh loop), so a
/// grok login that lands after this scope was first built is adopted on the
/// manager's own refresh tick — no session ever needs re-spawning.
///
/// MUST be called from within a Tokio runtime (the proactive-refresh loop
/// spawns a task, mirroring the primary).
pub(crate) fn global_manager_for(
    kigi_home: &Path,
    oauth: &'static kigi_models::OAuthConfig,
) -> Arc<AuthManager> {
    let mut pool = oauth_manager_pool().lock();
    if let Some(existing) = pool.get(oauth.scope_key) {
        return existing.clone();
    }
    let manager = Arc::new(AuthManager::new_oauth_provider(kigi_home, oauth));
    manager.configure_refresher();
    // Never-cancelled token = process-lifetime, matching the api-server /
    // per-session eager-refresh sites that pass a fresh token.
    manager.start_proactive_refresh(tokio_util::sync::CancellationToken::new());
    pool.insert(oauth.scope_key, manager.clone());
    manager
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oauth_for(id: &str) -> &'static kigi_models::OAuthConfig {
        kigi_models::PlatformId::parse(id)
            .expect("known platform")
            .oauth()
            .expect("subscription-OAuth platform carries an OAuthConfig")
    }

    #[tokio::test]
    async fn every_oauth_scope_gets_its_own_pooled_manager() {
        let home = tempfile::tempdir().unwrap();
        let ids = [
            "xai-grok",
            "claude-pro-max",
            "github-copilot",
            "openai-codex",
        ];
        let managers: Vec<_> = ids
            .iter()
            .map(|id| global_manager_for(home.path(), oauth_for(id)))
            .collect();
        for (i, a) in managers.iter().enumerate() {
            assert!(
                Arc::ptr_eq(a, &global_manager_for(home.path(), oauth_for(ids[i]))),
                "{}: the pool must return the SAME manager for a scope",
                ids[i]
            );
            for (j, b) in managers.iter().enumerate() {
                if i != j {
                    assert!(
                        !Arc::ptr_eq(a, b),
                        "{} and {} must not share a pooled manager",
                        ids[i],
                        ids[j]
                    );
                }
            }
        }
    }

    /// The test pool home is a per-process path that is never created, so a
    /// test binary leaves nothing behind (and never resolves the developer's
    /// real `~/.kigi`, whose stored OAuth tokens the pool would otherwise read
    /// and proactively refresh over the network).
    ///
    /// This is also the tripwire for the cleanup claim in [`pool_home`]: a
    /// completed device login through `authenticate_oauth_platform` WOULD create
    /// this directory, so if a lib test ever drives one, this assertion fires
    /// and the cleanup has to be added rather than silently regressing the
    /// "tests are TempDir self-cleaning" discipline.
    #[test]
    fn test_pool_home_is_disposable_and_never_the_real_home() {
        let home = pool_home();
        assert!(
            home.starts_with(std::env::temp_dir()),
            "the test pool home must live under the system temp dir, got {home:?}"
        );
        assert!(
            !home.exists(),
            "the test pool home must not be created — nothing to clean up"
        );
    }
}
