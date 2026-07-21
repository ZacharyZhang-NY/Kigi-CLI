//! models.dev enrichment catalog loading (agent-side IO).
//!
//! Providers whose `/models` wire serves no context/thinking metadata get it
//! from models.dev (see `kigi_models::enrichment`). This module owns the IO:
//! a 24h-TTL disk cache under `~/.kigi`, a runtime refresh of
//! `https://models.dev/api.json` (filtered to registry providers before
//! caching), and the bundled-snapshot fallback. NO NETWORK unless some
//! enabled platform actually needs enrichment (`wire_serves_metadata` false)
//! — with only Kimi/Moonshot configured this module never leaves disk.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kigi_models::enrichment::{EnrichmentCatalog, bundled_enrichment, parse_api_json};

/// Override the refresh URL (e2e mock), or disable refresh entirely with
/// `0`/`off` (bundled snapshot + existing cache only).
pub(crate) const MODELS_DEV_URL_ENV: &str = "KIGI_MODELS_DEV_URL";
const DEFAULT_MODELS_DEV_URL: &str = "https://models.dev/api.json";
const CACHE_FILE: &str = "models_dev_cache.json";
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(serde::Serialize, serde::Deserialize)]
struct DiskCache {
    /// Unix seconds of the successful fetch.
    fetched_at: u64,
    /// The kigi version that wrote the cache — a different binary (upgrade
    /// OR downgrade) refetches rather than trusting old filtering rules.
    #[serde(default)]
    kigi_version: String,
    /// The keep-set the catalog was filtered to. A registry change (new
    /// provider row, models_dev_id rename) invalidates the cache instead of
    /// silently serving a catalog missing the new provider for up to 24h.
    #[serde(default)]
    keep_set: Vec<String>,
    /// Already filtered + transformed catalog.
    catalog: EnrichmentCatalog,
}

fn current_keep_set() -> Vec<String> {
    registry_models_dev_ids()
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// Whether any of `platforms` needs enrichment at all.
pub(crate) fn any_platform_needs_enrichment(platforms: &[kigi_models::PlatformId]) -> bool {
    platforms.iter().any(|p| !p.wire_serves_metadata())
}

/// The registry's models.dev provider ids (the refresh filter).
fn registry_models_dev_ids() -> BTreeSet<&'static str> {
    kigi_models::PlatformId::ALL
        .into_iter()
        .filter_map(|p| p.models_dev_id())
        .collect()
}

fn cache_path() -> std::path::PathBuf {
    crate::util::kigi_home::kigi_home().join(CACHE_FILE)
}

fn refresh_url() -> Option<String> {
    match std::env::var(MODELS_DEV_URL_ENV) {
        Ok(v)
            if matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "off" | "false"
            ) =>
        {
            None
        }
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => Some(DEFAULT_MODELS_DEV_URL.to_string()),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load the enrichment catalog for a fetch pass over `enabled` platforms.
///
/// Fast path: nothing needs enrichment → empty catalog, zero IO (the merge
/// branch is never taken for wire-served platforms, and NOT forcing the
/// bundled parse keeps its cost/panic surface off the kimi/moonshot path).
/// Otherwise: fresh valid disk cache → use it; else refresh over HTTP
/// (filter + transform + best-effort cache write); on refresh failure fall
/// back to a STALE cache, then the bundled snapshot — each step logged,
/// never silent.
pub(crate) fn load_enrichment_catalog(
    enabled: &[kigi_models::PlatformId],
) -> std::borrow::Cow<'static, EnrichmentCatalog> {
    if !any_platform_needs_enrichment(enabled) {
        return std::borrow::Cow::Owned(EnrichmentCatalog::new());
    }
    load_enrichment_catalog_at(&cache_path())
}

/// Path-injectable core (tests use a tempdir path directly — no env, no
/// `kigi_home()` OnceLock interaction).
fn load_enrichment_catalog_at(
    path: &std::path::Path,
) -> std::borrow::Cow<'static, EnrichmentCatalog> {
    use std::borrow::Cow;

    let cached: Option<DiskCache> =
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| match serde_json::from_str(&s) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e,
                    "models.dev cache unreadable; refetching");
                    None
                }
            });
    let now = now_unix();
    let cache_is_fresh = cached.as_ref().is_some_and(|c| {
        // A future fetched_at (clock jump backwards, corrupt stamp) is
        // stale, not fresh-forever; a different binary or keep-set means the
        // cache was filtered under other rules — refetch instead of serving
        // a catalog that may miss newly-registered providers for 24h.
        c.fetched_at <= now
            && now - c.fetched_at < CACHE_TTL.as_secs()
            && c.kigi_version == kigi_version::VERSION
            && c.keep_set == current_keep_set()
    });
    if cache_is_fresh {
        tracing::debug!("models.dev enrichment: fresh disk cache");
        return Cow::Owned(cached.expect("cache_is_fresh implies Some").catalog);
    }

    match refresh_url() {
        Some(url) => match fetch_and_filter(&url) {
            Ok(catalog) => {
                let cache = DiskCache {
                    fetched_at: now_unix(),
                    kigi_version: kigi_version::VERSION.to_string(),
                    keep_set: current_keep_set(),
                    catalog,
                };
                // Best-effort write: a read-only home must not fail the fetch.
                match serde_json::to_string(&cache) {
                    Ok(body) => {
                        if let Err(e) = crate::util::config::atomic_write_string(path, &body) {
                            tracing::warn!(path = %path.display(), error = %e,
                                "models.dev cache write failed; continuing in-memory");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "models.dev cache serialize failed")
                    }
                }
                tracing::info!("models.dev enrichment refreshed");
                Cow::Owned(cache.catalog)
            }
            Err(e) => {
                if let Some(c) = cached {
                    tracing::warn!(error = %e,
                        "models.dev refresh failed; using STALE cache");
                    Cow::Owned(c.catalog)
                } else {
                    tracing::warn!(error = %e,
                        "models.dev refresh failed; using bundled snapshot");
                    Cow::Borrowed(bundled_enrichment())
                }
            }
        },
        None => {
            tracing::info!("models.dev refresh disabled; using cache/bundled");
            match cached {
                Some(c) => Cow::Owned(c.catalog),
                None => Cow::Borrowed(bundled_enrichment()),
            }
        }
    }
}

fn fetch_and_filter(url: &str) -> anyhow::Result<EnrichmentCatalog> {
    let response = crate::http::shared_blocking_client().get(url).send()?;
    let status = response.status();
    anyhow::ensure!(status.is_success(), "GET {url}: HTTP {}", status.as_u16());
    let body = response.text()?;
    let keep = registry_models_dev_ids();
    Ok(parse_api_json(&body, Some(&keep))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kigi_test_support::EnvGuard;
    use serial_test::serial;

    /// Wire-served-only platform sets never trigger IO — and never force the
    /// bundled parse (empty owned catalog; the merge branch is gated off).
    #[test]
    fn wire_served_platforms_get_empty_catalog_without_io() {
        assert!(!any_platform_needs_enrichment(
            &kigi_models::PlatformId::ALL
        ));
        let catalog = load_enrichment_catalog(&kigi_models::PlatformId::ALL);
        assert!(catalog.is_empty());
    }

    fn cache_file_in(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join(CACHE_FILE)
    }

    fn write_cache(path: &std::path::Path, fetched_at: u64, versioned: bool) {
        let cache = DiskCache {
            fetched_at,
            kigi_version: if versioned {
                kigi_version::VERSION.to_string()
            } else {
                "0.0.0-other".to_string()
            },
            keep_set: current_keep_set(),
            catalog: EnrichmentCatalog::from([(
                "moonshotai".to_string(),
                std::collections::BTreeMap::from([(
                    "from-cache".to_string(),
                    kigi_models::enrichment::EnrichmentModel {
                        context: 111,
                        ..Default::default()
                    },
                )]),
            )]),
        };
        std::fs::write(path, serde_json::to_string(&cache).unwrap()).unwrap();
    }

    /// Fresh valid cache short-circuits — no HTTP (no mock server mounted:
    /// a fetch attempt would fail and fall to bundled, failing the assert).
    #[test]
    #[serial]
    fn fresh_valid_cache_is_served_without_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_file_in(&dir);
        write_cache(&path, now_unix(), true);
        let _url = EnvGuard::set(MODELS_DEV_URL_ENV, "http://127.0.0.1:1/api.json");
        let catalog = load_enrichment_catalog_at(&path);
        assert!(
            kigi_models::enrichment::lookup(&catalog, "moonshotai", "from-cache").is_some(),
            "fresh cache must be served"
        );
    }

    /// Version/keep-set/future-stamp guards: each invalidates a fresh-aged
    /// cache. Refresh is disabled here, so invalidation falls through to the
    /// STALE cache (resource degradation, not data loss) — proving both the
    /// guard firing and the fallback order.
    #[test]
    #[serial]
    fn cache_guards_invalidate_and_fall_back_to_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_file_in(&dir);
        let _url = EnvGuard::set(MODELS_DEV_URL_ENV, "0");
        // Wrong binary version → not fresh → (refresh disabled) → stale used.
        write_cache(&path, now_unix(), false);
        let catalog = load_enrichment_catalog_at(&path);
        assert!(
            kigi_models::enrichment::lookup(&catalog, "moonshotai", "from-cache").is_some(),
            "stale-fallback must still serve the cached data"
        );
        // Future fetched_at → same path (guard fired: debug-log absence is
        // not observable here; the behavioral pin is refresh-disabled + the
        // wiremock test below proving a fired guard refetches).
        write_cache(&path, now_unix() + 10_000, true);
        let catalog = load_enrichment_catalog_at(&path);
        assert!(kigi_models::enrichment::lookup(&catalog, "moonshotai", "from-cache").is_some());
    }

    /// An invalidated cache (wrong version) REFETCHES when refresh is
    /// enabled: wiremock expect(1) proves the HTTP call happened; the new
    /// cache file carries the current version + keep-set and the fetched
    /// content replaces the stale entry.
    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn invalidated_cache_refetches_and_rewrites() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "moonshotai": { "models": { "from-refresh": {
                        "limit": {"context": 222}
                    }}}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let path = cache_file_in(&dir);
        write_cache(&path, now_unix(), false);
        let _url = EnvGuard::set(MODELS_DEV_URL_ENV, format!("{}/api.json", server.uri()));
        let path2 = path.clone();
        let catalog =
            tokio::task::spawn_blocking(move || load_enrichment_catalog_at(&path2).into_owned())
                .await
                .unwrap();
        assert!(
            kigi_models::enrichment::lookup(&catalog, "moonshotai", "from-refresh").is_some(),
            "guard-invalidated cache must refetch"
        );
        let rewritten: DiskCache =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(rewritten.kigi_version, kigi_version::VERSION);
        assert_eq!(rewritten.keep_set, current_keep_set());
        assert!(rewritten.catalog.contains_key("moonshotai"));
    }

    /// Corrupted cache file → refetch (not a crash, not trust-garbage).
    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn corrupted_cache_refetches() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "moonshotai": { "models": {} } })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let path = cache_file_in(&dir);
        std::fs::write(&path, "not json {").unwrap();
        let _url = EnvGuard::set(MODELS_DEV_URL_ENV, format!("{}/api.json", server.uri()));
        let path2 = path.clone();
        let catalog =
            tokio::task::spawn_blocking(move || load_enrichment_catalog_at(&path2).into_owned())
                .await
                .unwrap();
        assert!(catalog.contains_key("moonshotai"));
    }

    /// Refresh failure with NO cache → bundled snapshot fallback.
    #[test]
    #[serial]
    fn refresh_failure_without_cache_falls_back_to_bundled() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_file_in(&dir);
        let _url = EnvGuard::set(MODELS_DEV_URL_ENV, "http://127.0.0.1:1/api.json");
        let catalog = load_enrichment_catalog_at(&path);
        assert!(
            kigi_models::enrichment::lookup(&catalog, "kimi-for-coding", "k3").is_some(),
            "bundled snapshot must back a total refresh failure"
        );
        assert!(!path.exists(), "failed refresh must not write a cache");
    }

    /// Kill switch through the FULL load path (not just refresh_url): no
    /// cache + refresh disabled → bundled, no HTTP attempted (an attempt
    /// against the sentinel URL would be a hang/refusal, not bundled data).
    #[test]
    #[serial]
    fn kill_switch_full_path_serves_bundled() {
        let dir = tempfile::tempdir().unwrap();
        let path = cache_file_in(&dir);
        for token in ["0", "off", "FALSE", " Off "] {
            let _url = EnvGuard::set(MODELS_DEV_URL_ENV, token);
            assert!(refresh_url().is_none(), "token {token:?} must disable");
            let catalog = load_enrichment_catalog_at(&path);
            assert!(kigi_models::enrichment::lookup(&catalog, "kimi-for-coding", "k3").is_some());
        }
    }

    /// Refresh path: mock server → transform runs and the keep-set filters
    /// to registry providers (`moonshotai` is a real registry models_dev id;
    /// unknown providers are dropped).
    #[tokio::test(flavor = "multi_thread")]
    #[serial]
    async fn refresh_transforms_and_filters_to_registry_ids() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api.json"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "moonshotai": { "models": { "kimi-test": {
                        "limit": {"context": 262144},
                        "reasoning": true,
                        "reasoning_options": [
                            {"type": "effort", "values": ["low", "high"]}
                        ]
                    }}},
                    "not-in-registry": { "models": { "m": {} } }
                })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let url = format!("{}/api.json", server.uri());
        let catalog = tokio::task::spawn_blocking(move || fetch_and_filter(&url))
            .await
            .unwrap()
            .expect("fetch must succeed");
        assert!(
            !catalog.contains_key("not-in-registry"),
            "keep-set must drop providers outside the registry"
        );
        let m = kigi_models::enrichment::lookup(&catalog, "moonshotai", "kimi-test")
            .expect("registry provider survives the filter");
        assert_eq!(m.context, 262_144);
        assert_eq!(m.efforts, ["low", "high"]);
    }
}
