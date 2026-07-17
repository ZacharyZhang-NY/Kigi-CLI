//! Backend endpoint defaults for the Kigi crate family.
//!
//! Kigi talks to exactly four first-party endpoints (PRD §8.2); everything
//! else (MCP servers, GitHub release assets) is user- or release-configured.
//! Each value resolves as an env-var override when set, else the compiled
//! production default.

/// The complete set of first-party endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KigiEndpoints {
    /// Kimi Code subscription inference API (OpenAI chat/completions compatible).
    pub coding_api_base_url: &'static str,
    /// OAuth device-flow host for Kimi Code subscription login.
    pub oauth_host: &'static str,
    /// GitHub Releases API endpoint the self-updater polls.
    pub update_base_url: &'static str,
    /// Human-facing page for subscription upgrade guidance.
    pub upgrade_page_url: &'static str,
}

pub const PRODUCTION_ENDPOINTS: KigiEndpoints = KigiEndpoints {
    coding_api_base_url: "https://api.kimi.com/coding/v1",
    oauth_host: "https://auth.kimi.com",
    update_base_url: "https://api.github.com/repos/ZacharyZhang-NY/Kigi-CLI/releases",
    upgrade_page_url: "https://www.kimi.com/code/",
};

/// Env var overriding [`coding_api_base_url`] (PRD F3).
pub const CODE_BASE_URL_ENV: &str = "KIGI_CODE_BASE_URL";
/// Env var overriding [`oauth_host`] (PRD F1).
pub const OAUTH_HOST_ENV: &str = "KIGI_OAUTH_HOST";

fn resolve(var: &str, compiled: &'static str) -> String {
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => compiled.to_string(),
    }
}

/// Subscription inference base URL: `KIGI_CODE_BASE_URL` override when set,
/// else the compiled production endpoint.
pub fn coding_api_base_url() -> String {
    resolve(CODE_BASE_URL_ENV, PRODUCTION_ENDPOINTS.coding_api_base_url)
}

/// OAuth host: `KIGI_OAUTH_HOST` override when set, else the compiled
/// production endpoint.
pub fn oauth_host() -> String {
    resolve(OAUTH_HOST_ENV, PRODUCTION_ENDPOINTS.oauth_host)
}

/// GitHub Releases API endpoint for the self-updater. Compile-time constant;
/// the updater's channel/rollback semantics layer on top of it.
pub fn update_base_url() -> &'static str {
    PRODUCTION_ENDPOINTS.update_base_url
}

/// Subscription upgrade page shown in rate-limit and upsell surfaces.
pub fn upgrade_page_url() -> &'static str {
    PRODUCTION_ENDPOINTS.upgrade_page_url
}

/// Serializes env-var mutation across tests; `std::env` is process-global.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// RAII env-var override for tests: constructors snapshot the prior value
/// under [`ENV_LOCK`], `Drop` restores it, panics included.
pub struct EnvVarGuard {
    key: &'static str,
    prev: Option<String>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvVarGuard {
    pub fn set(key: &'static str, value: &str) -> Self {
        let lock = env_lock();
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, value) };
        Self {
            key,
            prev,
            _lock: lock,
        }
    }

    pub fn remove(key: &'static str) -> Self {
        let lock = env_lock();
        let prev = std::env::var(key).ok();
        unsafe { std::env::remove_var(key) };
        Self {
            key,
            prev,
            _lock: lock,
        }
    }

    /// Update the value while still holding the env lock.
    pub fn set_value(&self, value: &str) {
        unsafe { std::env::set_var(self.key, value) };
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(prev) => unsafe { std::env::set_var(self.key, prev) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_guard_set_value_updates_then_restores_on_drop() {
        const KEY: &str = "KIGI_ENV_VAR_GUARD_SET_VALUE_PROBE";
        let before = std::env::var(KEY).ok();
        {
            let guard = EnvVarGuard::set(KEY, "initial");
            assert_eq!(std::env::var(KEY).ok().as_deref(), Some("initial"));
            guard.set_value("updated");
            assert_eq!(
                std::env::var(KEY).ok().as_deref(),
                Some("updated"),
                "set_value must update the env var while the guard is live"
            );
        }
        assert_eq!(
            std::env::var(KEY).ok(),
            before,
            "Drop must restore the pre-guard snapshot (was {before:?})"
        );
    }

    #[test]
    fn override_env_vars_win_over_compiled_defaults() {
        let _g = EnvVarGuard::set(CODE_BASE_URL_ENV, "https://example.test/v1");
        assert_eq!(coding_api_base_url(), "https://example.test/v1");
        drop(_g);
        let _g2 = EnvVarGuard::set(OAUTH_HOST_ENV, "https://auth.example.test");
        assert_eq!(oauth_host(), "https://auth.example.test");
    }

    #[test]
    fn blank_override_falls_back_to_compiled_default() {
        let _g = EnvVarGuard::set(CODE_BASE_URL_ENV, "  ");
        assert_eq!(
            coding_api_base_url(),
            PRODUCTION_ENDPOINTS.coding_api_base_url
        );
    }

    #[test]
    fn production_endpoints_are_https() {
        for url in [
            PRODUCTION_ENDPOINTS.coding_api_base_url,
            PRODUCTION_ENDPOINTS.oauth_host,
            PRODUCTION_ENDPOINTS.update_base_url,
            PRODUCTION_ENDPOINTS.upgrade_page_url,
        ] {
            assert!(url.starts_with("https://"), "{url} must be https");
        }
    }
}
