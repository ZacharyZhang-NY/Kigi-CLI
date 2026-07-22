//! Model fetching, resolution, and management.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::RwLock;

use agent_client_protocol as acp;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use indexmap::IndexMap;

use crate::agent::config::{self, ModelEntry, resolve_credentials, sampling_config_for_model};
use crate::agent::models_fetch::{FetchModelsResult, fetch_models_blocking};
use crate::auth::{AuthManager, KimiAuth, KimiCodeConfig};
use crate::sampling::SamplerConfig as SamplingConfig;
use globset::{Glob, GlobSet, GlobSetBuilder};
use kigi_sampling_types::{ReasoningEffort, ReasoningEffortOption};

/// The agent-wide baseline sampling config together with the registry platform
/// of the catalog entry it was BUILT from.
///
/// Returned as ONE value by [`ModelsManager::sampling_config`] so a holder
/// cannot end up with the config but not the platform its credential was
/// resolved against — the drift that made the login stamp guard answer about a
/// model the config no longer represents (H-a).
pub struct BaselineSamplingConfig {
    pub config: SamplingConfig,
    /// `None` for a bare / `[model.*]` entry.
    pub platform: Option<kigi_models::PlatformId>,
}

// ── Auth method for model fetching ──────────────────────────────────────────

/// How the model catalog is fetched (PRD F4). The old xAI tier-gated proxy
/// fetch is gone; there are exactly two shapes now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelFetchAuth {
    /// Fixed platform registry: `kimi-code` via the F1 OAuth bearer plus the
    /// Moonshot open platforms via configured API keys.
    Platforms,
    /// `KIGI_MODELS_BASE_URL` / `models_list_url` BYOK escape hatch: a single
    /// OpenAI-compatible listing.
    CustomEndpoint,
}

impl ModelFetchAuth {
    /// Custom endpoint when configured, else the platform registry.
    pub(crate) fn resolve(endpoints: &config::EndpointsConfig) -> Self {
        if endpoints.has_custom_endpoint() {
            Self::CustomEndpoint
        } else {
            Self::Platforms
        }
    }

    fn cache_auth_method(&self) -> CacheAuthMethod {
        match self {
            Self::CustomEndpoint => CacheAuthMethod::ApiKey,
            Self::Platforms => CacheAuthMethod::Platforms,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Debug)]
#[serde(rename_all = "snake_case")]
enum CacheAuthMethod {
    ApiKey,
    Platforms,
}

/// Resolved API-key platform credentials (PRD F2), one entry per registry
/// platform with a usable key: platform env var(s) > auth.json platform
/// scope > `[platforms.*]` config.
///
/// SECURITY: values are secrets — the manual `Debug` impl prints presence
/// only, and nothing here may be logged or persisted.
#[derive(Clone, Default)]
pub(crate) struct PlatformApiKeys {
    keys: std::collections::BTreeMap<kigi_models::PlatformId, String>,
}

impl std::fmt::Debug for PlatformApiKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("PlatformApiKeys");
        for platform in kigi_models::PlatformId::ALL {
            if !platform.uses_oauth() {
                s.field(platform.as_str(), &self.keys.contains_key(&platform));
            }
        }
        s.finish()
    }
}

impl PlatformApiKeys {
    pub(crate) fn resolve(platforms: &config::PlatformsConfig) -> Self {
        // Read auth.json ONCE for the whole registry sweep — per-platform
        // re-reads would mean one file parse per provider on every resolve.
        let stored =
            crate::auth::read_auth_json(&crate::util::kigi_home::kigi_home().join("auth.json"))
                .ok();
        let mut keys = std::collections::BTreeMap::new();
        for platform in kigi_models::PlatformId::ALL {
            if platform.uses_oauth() {
                continue;
            }
            let key = config::resolve_platform_api_key_with(
                platform,
                platforms,
                |name| std::env::var(name).ok(),
                |p| {
                    stored
                        .as_ref()
                        .and_then(|m| m.get(p.as_str()))
                        .map(|a| a.key.clone())
                },
            );
            if let Some(key) = key {
                keys.insert(platform, key);
            }
        }
        Self { keys }
    }

    /// Resolve from the effective on-disk config (startup paths that have no
    /// parsed `Config` yet).
    pub(crate) fn resolve_from_effective_config() -> Self {
        let platforms = crate::config::load_effective_config()
            .ok()
            .and_then(|raw| raw.get("platforms").cloned())
            .and_then(|v| v.try_into::<config::PlatformsConfig>().ok())
            .unwrap_or_default();
        Self::resolve(&platforms)
    }

    pub(crate) fn key_for(&self, platform: kigi_models::PlatformId) -> Option<&str> {
        self.keys.get(&platform).map(String::as_str)
    }

    /// Any API-key platform credentialed? Drives "should we prefetch without
    /// a session" and the F2 acceptance path (platform key only, no login).
    pub(crate) fn any(&self) -> bool {
        !self.keys.is_empty()
    }

    /// Test-only constructor (fields are private to this module).
    #[cfg(test)]
    pub(crate) fn test_keys(cn: Option<&str>, ai: Option<&str>) -> Self {
        let mut keys = std::collections::BTreeMap::new();
        if let Some(k) = cn {
            keys.insert(kigi_models::PlatformId::MoonshotCn, k.to_owned());
        }
        if let Some(k) = ai {
            keys.insert(kigi_models::PlatformId::MoonshotAi, k.to_owned());
        }
        Self { keys }
    }

    /// Test-only constructor for a single API-key platform.
    #[cfg(test)]
    pub(crate) fn test_single(platform: kigi_models::PlatformId, key: &str) -> Self {
        Self {
            keys: std::collections::BTreeMap::from([(platform, key.to_owned())]),
        }
    }
}

pub(crate) fn task_model_error_for_catalog(
    requested: &str,
    available: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
) -> Option<String> {
    let is_available = |entry: &ModelEntry| {
        entry.info.user_selectable && entry.info.visible_for_auth(is_session_auth)
    };
    if config::find_model_by_id(available, requested).is_some_and(&is_available) {
        return None;
    }

    let mut slugs = available
        .iter()
        .filter(|(_, entry)| is_available(entry))
        .map(|(slug, _)| slug.as_str())
        .collect::<Vec<_>>();
    slugs.sort_unstable();
    let guidance = if slugs.is_empty() {
        "No valid model slugs are currently available. Omit `model` to inherit the parent model."
            .to_string()
    } else {
        format!(
            "Valid model slugs: {}. Omit `model` to inherit the parent model.",
            slugs.join(", ")
        )
    };
    Some(format!("Unknown Task.model slug '{requested}'. {guidance}"))
}

/// Thread-safe model manager.
///
/// Owns the auth manager, config, and gateway needed to refresh models.
/// Uses `parking_lot::RwLock` for short clone-and-release access.
#[derive(Clone)]
pub struct ModelsManager {
    inner: Arc<Inner>,
}

struct Inner {
    prefetched: RwLock<Option<IndexMap<String, ModelEntry>>>,
    models: RwLock<IndexMap<String, ModelEntry>>,
    current_model_id: RwLock<acp::ModelId>,
    current_reasoning_effort: RwLock<Option<ReasoningEffort>>,
    etag: RwLock<Option<String>>,
    /// Set once a real catalog has been fetched; gates whether
    /// `apply_refresh_result` calls `reselect_default_model` (first
    /// time) or `reselect_current_model_if_missing` (subsequent).
    /// Reset in `clear()` for identity changes.
    has_fetched_real_catalog: RwLock<bool>,
    // ── Owned context for self-contained refresh ────────────────
    auth_manager: Arc<AuthManager>,
    cfg: RwLock<config::Config>,
    fetch_auth: RwLock<ModelFetchAuth>,
    gateway: RwLock<Option<kigi_acp_lib::AcpAgentGatewaySender>>,
    cache: ModelsCacheManager,
    /// Guard to prevent overlapping retry loops.
    retry_in_flight: AtomicBool,
    /// `allowed_models` matched nothing in the fetched catalog; the prompt path
    /// blocks rather than run on the bundled default. Set in `apply_refresh_result`.
    allowlist_excludes_all: AtomicBool,
    /// Layer-3 LazinessDetector model-switch signal. Carries a
    /// monotonically-increasing generation counter (`u64`) that is
    /// bumped whenever the current model id actually changes via
    /// [`Self::set_current_model_id`].
    ///
    /// Two consumer patterns:
    /// 1. `subscribe_model_switch().changed().await` — used by the
    ///    `SessionActor` main loop to react to a switch (e.g. zero
    ///    the per-session nudge counter). Critically, `watch::Receiver`
    ///    only resolves `.changed()` on changes that happen **after**
    ///    subscription — there is no stored-permit hazard akin to
    ///    `tokio::sync::Notify::notify_one()`.
    /// 2. `model_switch_generation()` — cheap snapshot read used by
    ///    `maybe_fire_laziness_check`'s polling loop to detect a
    ///    switch that occurred during the idle wait or sampler call.
    ///
    /// `watch::Sender` natively fans out to every subscriber, so this
    /// replaces the previous `RwLock<Vec<Arc<Notify>>>` listener
    /// registry — no manual fan-out, no listener-leak risk, no
    /// `unregister` API to maintain.
    model_switch_watch: tokio::sync::watch::Sender<u64>,
}

impl Default for ModelsManager {
    fn default() -> Self {
        let kigi_home = crate::util::kigi_home::kigi_home();
        let auth_manager = Arc::new(AuthManager::new(&kigi_home, KimiCodeConfig::default()));
        Self::new(
            None,
            IndexMap::new(),
            acp::ModelId::new("default"),
            auth_manager,
            config::Config::default(),
        )
    }
}

impl ModelsManager {
    pub(crate) fn new(
        prefetched: Option<IndexMap<String, ModelEntry>>,
        models: IndexMap<String, ModelEntry>,
        current_model_id: acp::ModelId,
        auth_manager: Arc<AuthManager>,
        cfg: config::Config,
    ) -> Self {
        let fetch_auth = ModelFetchAuth::resolve(&cfg.endpoints);
        let current_reasoning_effort = cfg.models.default_reasoning_effort;
        Self {
            inner: Arc::new(Inner {
                prefetched: RwLock::new(prefetched),
                models: RwLock::new(models),
                current_model_id: RwLock::new(current_model_id),
                current_reasoning_effort: RwLock::new(current_reasoning_effort),
                etag: RwLock::new(None),
                has_fetched_real_catalog: RwLock::new(false),
                auth_manager,
                cfg: RwLock::new(cfg),
                fetch_auth: RwLock::new(fetch_auth),
                gateway: RwLock::new(None),
                cache: ModelsCacheManager::new(),
                retry_in_flight: AtomicBool::new(false),
                allowlist_excludes_all: AtomicBool::new(false),
                model_switch_watch: tokio::sync::watch::channel(0u64).0,
            }),
        }
    }

    /// Subscribe to model-switch events. Returns a `watch::Receiver`
    /// carrying the monotonic generation counter. `.changed()` only
    /// resolves on switches that occur **after** subscription, so
    /// there is no stored-permit hazard (the bug that motivated
    /// replacing the previous `Arc<Notify>` design).
    pub fn subscribe_model_switch(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.model_switch_watch.subscribe()
    }

    /// Cheap snapshot of the current model-switch generation. Used by
    /// `maybe_fire_laziness_check`'s polling loop to detect a switch
    /// that occurred during the idle wait or sampler call without
    /// having to allocate a fresh `Receiver` per fire.
    pub fn model_switch_generation(&self) -> u64 {
        *self.inner.model_switch_watch.borrow()
    }

    /// Build from a resolved config. Falls back to bundled default if no models available.
    ///
    /// When `prefetched_models` is `None`, the disk cache is consulted so that
    /// server-side models are available for default-model resolution even when
    /// the caller didn't do an explicit prefetch.
    pub fn from_config(
        cfg: &config::Config,
        prefetched_models: Option<IndexMap<String, ModelEntry>>,
        auth_manager: Arc<AuthManager>,
    ) -> Result<Self, String> {
        let has_session = auth_manager.current_or_expired().is_some();
        let is_session_auth = auth_manager
            .current_or_expired()
            .is_some_and(|a| a.is_session_auth());
        let fetch_auth = ModelFetchAuth::resolve(&cfg.endpoints);
        let prefetched_models = prefetched_models.or_else(|| {
            let cache = ModelsCacheManager::new();
            let platform_keys = PlatformApiKeys::resolve(&cfg.platforms);
            cache
                .load_fresh(
                    &fetch_auth.cache_auth_method(),
                    &crate::agent::models_fetch::models_fetch_origin(
                        &cfg.endpoints,
                        fetch_auth,
                        has_session,
                        // Presence-only stubs: the origin encodes enabled
                        // platform NAMES, and it must match the fetch path's
                        // (which enables stored subscription-OAuth platforms)
                        // or their cached catalog never loads at startup.
                        &crate::agent::models_fetch::stored_oauth_token_stubs(
                            &crate::auth::oauth_registry::pool_home(),
                        ),
                        &platform_keys,
                    ),
                )
                .map(|c| c.models)
        });
        let has_prefetched = prefetched_models.is_some();
        let catalog = resolve_model_catalog(
            cfg,
            prefetched_models.clone(),
            &PlatformApiKeys::resolve(&cfg.platforms),
        );

        // Validate only against a real catalog; a bundled-only first run defers
        // to the async fetch (`apply_refresh_result`).
        if has_prefetched {
            validate_selectable(cfg, &catalog)?;
        }

        let (current_model_key, current_model, model_source) =
            resolve_default_model(cfg, &catalog, is_session_auth);

        tracing::info!(
            model_id = %current_model.model,
            source = %model_source,
            "default model resolved"
        );

        let current_model_id = acp::ModelId::new(Arc::from(current_model_key));

        let mgr = Self::new(
            prefetched_models,
            catalog,
            current_model_id,
            auth_manager,
            cfg.clone(),
        );
        if has_prefetched {
            *mgr.inner.has_fetched_real_catalog.write() = true;
        }
        Ok(mgr)
    }

    pub(crate) fn set_gateway(&self, gateway: kigi_acp_lib::AcpAgentGatewaySender) {
        *self.inner.gateway.write() = Some(gateway);
    }

    /// Swap config, rebuild catalog, and reselect the model.
    ///
    /// Calls `reselect_default_model` when the preferred model changed
    /// (and is `Some`); otherwise `reselect_current_model_if_missing`.
    pub fn apply_config(&self, new_config: config::Config) {
        // Reject an invalid reload instead of mutating live state: bad globs or
        // (once a real catalog exists) an allowlist that excludes everything.
        if let Err(e) = new_config.validate_model_filters() {
            tracing::error!(error = %e, "ignoring config reload: invalid model filters");
            return;
        }
        let prefetched = self.inner.prefetched.read().clone();
        let new_catalog = resolve_model_catalog(
            &new_config,
            prefetched,
            &PlatformApiKeys::resolve(&new_config.platforms),
        );
        let has_real_catalog = *self.inner.has_fetched_real_catalog.read();
        if has_real_catalog && let Err(e) = validate_selectable(&new_config, &new_catalog) {
            tracing::error!(error = %e, "ignoring config reload: allowed_models excludes all models");
            return;
        }

        let (old_preferred, old_default_is_campaign) = {
            let cfg = self.inner.cfg.read();
            (
                cfg.models.default.clone(),
                cfg.models.default_is_campaign_driven,
            )
        };
        let new_preferred = new_config.models.default.clone();
        *self.inner.fetch_auth.write() = ModelFetchAuth::resolve(&new_config.endpoints);
        *self.inner.cfg.write() = new_config.clone();
        // Recompute the prompt-block flag so a corrective reload unblocks.
        if has_real_catalog {
            let excludes_all = allowlist_matches_nothing(&new_config, &new_catalog);
            self.inner
                .allowlist_excludes_all
                .store(excludes_all, Ordering::Relaxed);
        }
        *self.inner.models.write() = new_catalog;

        // A preferred-model flip caused only by a campaign overlay appearing or
        // disappearing must not yank an in-flight session whose current model is
        // still usable — the campaign applies to /new sessions only.
        let preferred_changed = new_preferred != old_preferred && new_preferred.is_some();
        // Recognize an appearing OR withdrawing campaign from the
        // `default_is_campaign_driven` flag on each config (no disk I/O); correct
        // even when the user has no base default (where a value compare would miss).
        let mut campaign_defaults = std::collections::HashSet::new();
        if new_config.models.default_is_campaign_driven
            && let Some(d) = &new_preferred
        {
            campaign_defaults.insert(d.clone());
        }
        if old_default_is_campaign && let Some(d) = &old_preferred {
            campaign_defaults.insert(d.clone());
        }
        let campaign_only_flip =
            is_campaign_only_flip(&old_preferred, &new_preferred, &campaign_defaults);
        let current_still_ok = {
            let models = self.inner.models.read();
            let cur = self.inner.current_model_id.read();
            models
                .get(cur.0.as_ref())
                .is_some_and(|e| e.info.user_selectable)
        };
        if preferred_changed && !(campaign_only_flip && current_still_ok) {
            self.reselect_default_model(&new_config);
        } else {
            self.reselect_current_model_if_missing(&new_config);
        }

        // Push the new catalog to connected clients (`kigi/models/update`).
        // Without this, a long-running agent (leader mode) correctly swaps
        // its in-memory catalog on a config.toml `[model.*]`/`[models]` edit,
        // but already-connected clients keep rendering the stale model list
        // until they reconnect. No-op when no gateway is attached (tests,
        // pre-init).
        self.notify_models_updated();
    }

    // ── Accessors ───────────────────────────────────────────────────

    pub fn models(&self) -> IndexMap<String, ModelEntry> {
        self.inner.models.read().clone()
    }

    pub fn endpoints(&self) -> config::EndpointsConfig {
        self.inner.cfg.read().endpoints.clone()
    }

    /// Does the current credential grant access to OAuth-only models?
    fn is_session_auth(&self) -> bool {
        self.inner
            .auth_manager
            .current_or_expired()
            .is_some_and(|a| a.is_session_auth())
    }

    /// ACP-visible (non-hidden) projection of the catalog.
    /// The catalog coming from `resolve_model_catalog` already has
    /// allowed_models + disabled_models + hidden_models applied.
    pub fn available(&self) -> IndexMap<acp::ModelId, acp::ModelInfo> {
        let snapshot = {
            let models = self.inner.models.read();
            models.clone()
        };

        let selectable: IndexMap<_, _> = snapshot
            .into_iter()
            .filter(|(_, e)| e.info.user_selectable)
            .collect();

        available_models(&selectable, self.is_session_auth())
    }

    pub(crate) fn task_model_error(&self, requested: &str) -> Option<String> {
        let is_session_auth = self.is_session_auth();
        let models = self.inner.models.read();
        task_model_error_for_catalog(requested, &models, is_session_auth)
    }

    pub fn current_model_id(&self) -> acp::ModelId {
        self.inner.current_model_id.read().clone()
    }

    pub fn set_current_model_id(&self, id: acp::ModelId) {
        // Only bump the model-switch generation on a real change.
        // The pager's `/model` handler can call this with the
        // already-active id during re-resolution; bumping the counter
        // in that case would needlessly cancel a healthy in-flight
        // classifier call and zero the per-session nudge counter.
        let changed = {
            let mut cur = self.inner.current_model_id.write();
            let changed = *cur != id;
            *cur = id;
            changed
        };
        if changed {
            self.inner
                .model_switch_watch
                .send_modify(|generation| *generation += 1);
        }
    }

    /// Look up the per-model Layer-3 LazinessDetector config for the
    /// model identified by `model_id`. Returns the default (disabled)
    /// config when the id isn't in the catalog — same fallback
    /// semantics as the `auto_compact_threshold_percent` lookup.
    pub fn laziness_detector_for(&self, model_id: &str) -> config::LazinessDetectorPerModelConfig {
        self.inner
            .models
            .read()
            .get(model_id)
            .map(|e| e.info().laziness_detector.clone())
            .unwrap_or_default()
    }

    /// Test-only catalog poke: inserts a `ModelEntry` keyed by `id`,
    /// allowing integration tests to enable Layer-3 features per
    /// model without spinning up the full config-merge pipeline.
    #[cfg(test)]
    pub(crate) fn insert_test_entry(&self, id: impl Into<String>, entry: ModelEntry) {
        self.inner.models.write().insert(id.into(), entry);
    }

    /// Kimi capability set for `model_id` from the live catalog (PRD F4).
    /// Empty when the model is unknown or declared no capabilities.
    pub fn model_capabilities(&self, model_id: &str) -> Vec<kigi_models::ModelCapability> {
        let models = self.inner.models.read();
        resolve_catalog_key(&models, &acp::ModelId::new(model_id))
            .and_then(|key| models.get(key.0.as_ref()))
            .map(|e| e.info().capabilities.clone())
            .unwrap_or_default()
    }

    /// PRD F4: thinking defaults ON iff the model's capabilities include
    /// `thinking` or `always_thinking`. This is the F3 seam for the sampler's
    /// thinking toggle; unknown models default OFF.
    pub fn model_default_thinking(&self, model_id: &str) -> bool {
        kigi_models::default_thinking_enabled(&self.model_capabilities(model_id))
    }

    pub fn current_reasoning_effort(&self) -> Option<ReasoningEffort> {
        *self.inner.current_reasoning_effort.read()
    }

    pub fn set_current_reasoning_effort(&self, effort: Option<ReasoningEffort>) {
        *self.inner.current_reasoning_effort.write() = effort;
    }

    /// Whether the given model supports reasoning effort according to the catalog.
    pub fn model_supports_reasoning_effort(&self, model_id: &str) -> bool {
        self.inner
            .models
            .read()
            .get(model_id)
            .map(|e| e.info().supports_reasoning_effort)
            .unwrap_or(false)
    }

    /// Whether the model's effort menu offers this canonical value (legacy
    /// built-in set when the menu is empty). Used to migrate pre-split
    /// `xhigh` overrides onto `max`-vocabulary models at restore.
    pub fn model_offers_effort(
        &self,
        model_id: &str,
        effort: kigi_sampling_types::ReasoningEffort,
    ) -> bool {
        self.inner
            .models
            .read()
            .get(model_id)
            .map(|e| model_offers_reasoning_effort(e.info(), effort))
            .unwrap_or(false)
    }

    /// The catalog default reasoning effort for `model_id`, if the catalog
    /// pins one. Used as the final fallback when neither the session handle
    /// nor the global config sets an explicit effort, so surfaced config stays
    /// consistent with the effort sampling actually uses.
    pub fn model_default_reasoning_effort(&self, model_id: &str) -> Option<ReasoningEffort> {
        self.inner
            .models
            .read()
            .get(model_id)
            .and_then(|e| e.info().reasoning_effort)
    }

    /// The raw catalog `reasoning_efforts` list for `model_id` with no fallback,
    /// empty when the catalog pins none (caller falls back to the built-in
    /// session modes). Distinct from the pager's gate-first, fallback-applied
    /// `ModelState::reasoning_effort_options`.
    pub fn model_reasoning_efforts(&self, model_id: &str) -> Vec<ReasoningEffortOption> {
        self.inner
            .models
            .read()
            .get(model_id)
            .map(|e| e.info().reasoning_efforts.clone())
            .unwrap_or_default()
    }

    pub fn model_supports_backend_search(&self, model_id: &str) -> bool {
        self.inner
            .models
            .read()
            .get(model_id)
            .map(|e| e.info().supports_backend_search)
            .unwrap_or(false)
    }

    pub fn model_compactions_remaining(
        &self,
        model_id: &str,
    ) -> Option<kigi_sampling_types::CompactionsRemaining> {
        self.inner
            .models
            .read()
            .get(model_id)
            .and_then(|e| e.info().compactions_remaining)
    }

    pub fn model_compaction_at_tokens(
        &self,
        model_id: &str,
    ) -> Option<kigi_sampling_types::CompactionAtTokens> {
        self.inner
            .models
            .read()
            .get(model_id)
            .and_then(|e| e.info().compaction_at_tokens)
    }

    /// Catalog opt-in to display the served-checkpoint fingerprint for this model.
    ///
    /// `model_id` may be a routing slug (`config.model`, e.g. `kigi-4.5`)
    /// OR a catalog key; the catalog map is keyed by the config key, which can
    /// differ from the slug for custom/enterprise ids (e.g. key `enterprise-kigi`
    /// → slug `kigi-4.5`). Resolve to the catalog key first so a slug
    /// caller still finds the opted-in entry.
    pub fn model_show_model_fingerprint(&self, model_id: &str) -> bool {
        let models = self.inner.models.read();
        resolve_catalog_key(&models, &acp::ModelId::new(model_id))
            .and_then(|key| models.get(key.0.as_ref()))
            .map(|e| e.info().show_model_fingerprint)
            .unwrap_or(false)
    }

    /// Resolved next-prompt-suggestion model pin from the live config
    /// (`env > [models] prompt_suggestion > remote settings`); tracks config
    /// hot-reloads via [`Self::apply_config`]. Consumed catalog-guarded by
    /// `handle_suggest_prompt`.
    pub fn prompt_suggest_model_pin(&self) -> crate::config::PromptSuggestModelPin {
        self.inner.cfg.read().prompt_suggest_model_pin.clone()
    }

    /// Whether `model_id` resolves in the current catalog — as a config key
    /// or a routing slug (see [`resolve_catalog_key`]). Deliberately checks
    /// the full catalog rather than the user-selectable projection: auxiliary
    /// background calls need a *sampleable* model, and hidden or
    /// non-selectable entries are still sampleable.
    pub fn model_in_catalog(&self, model_id: &str) -> bool {
        let models = self.inner.models.read();
        resolve_catalog_key(&models, &acp::ModelId::new(model_id)).is_some()
    }

    #[cfg(test)]
    fn prefetched(&self) -> Option<IndexMap<String, ModelEntry>> {
        self.inner.prefetched.read().clone()
    }

    #[cfg(test)]
    fn has_fetched_real_catalog(&self) -> bool {
        *self.inner.has_fetched_real_catalog.read()
    }

    // ── Mutations ───────────────────────────────────────────────────

    fn rebuild(&self, cfg: &config::Config, prefetched: Option<IndexMap<String, ModelEntry>>) {
        *self.inner.models.write() =
            resolve_model_catalog(cfg, prefetched, &PlatformApiKeys::resolve(&cfg.platforms));
    }

    /// Refresh models when the etag changes.
    ///
    /// Writes etag optimistically before spawning the fetch to coalesce
    /// concurrent callers seeing the same new etag.
    pub async fn refresh_if_new_etag(&self, etag: String) {
        let same_etag = {
            let current = self.inner.etag.read();
            current.as_deref() == Some(etag.as_str())
        };
        if same_etag {
            let fetch_auth = *self.inner.fetch_auth.read();
            self.inner
                .cache
                .renew_ttl(&fetch_auth.cache_auth_method(), &self.cache_origin())
                .await;
            return;
        }
        *self.inner.etag.write() = Some(etag.clone());
        tracing::info!(etag = %etag, "models etag changed, refreshing");
        self.do_refresh(Some(etag), RefreshStrategy::Online);
    }

    /// Auth identity changed: invalidate disk cache and refresh the catalog.
    ///
    /// Safe on OIDC token recovery after idle: we never drop a successfully-fetched
    /// catalog on transient failure. Only fall back to the bundled default when
    /// we have never had a real catalog (`!has_fetched_real_catalog`), or via
    /// the genuine no-auth path (`clear()`).
    ///
    /// Respects the auth snapshot / hot-swap discipline.
    pub async fn on_auth_changed(&self) {
        let config = self.inner.cfg.read().clone();
        self.inner.cache.invalidate();
        let fetch_auth = ModelFetchAuth::resolve(&config.endpoints);
        *self.inner.fetch_auth.write() = fetch_auth;
        if should_wipe_catalog_on_auth_change(
            self.inner.auth_manager.current_or_expired().is_some(),
            fetch_auth,
            PlatformApiKeys::resolve(&config.platforms).any(),
            !crate::agent::models_fetch::stored_oauth_platforms(
                &crate::auth::oauth_registry::pool_home(),
            )
            .is_empty(),
        ) {
            self.clear();
            return;
        }

        // Never eagerly drop prefetched on auth recovery. Only fall back to
        // bundled defaults when we have never had a real catalog. Resolved once
        // so the fetch and the failure-vs-disabled classification below agree.
        let remote_fetch_enabled = crate::util::config::resolve_remote_fetch_enabled();
        self.fetch_and_apply_inner(remote_fetch_enabled).await;

        if !*self.inner.has_fetched_real_catalog.read() && self.inner.prefetched.read().is_none() {
            if remote_fetch_enabled {
                kigi_log::unified_log::warn(
                    "model catalog: falling back to bundled defaults only",
                    None,
                    Some(serde_json::json!({
                        "trigger": "on_auth_changed",
                        "had_real_catalog": false,
                    })),
                );
            } else {
                // Deliberate no-fetch state, not a failure: no warn-class log.
                tracing::debug!("model catalog: bundled defaults in use (remote_fetch disabled)");
            }
            self.rebuild(&config, None); // first-time only: no fetched catalog, use bundled defaults
            self.reselect_current_model_if_missing(&config);

            // Schedule background retries so we recover once the network is
            // back (e.g. after sleep/resume when the first fetch races DNS).
            // With remote_fetch disabled a retry can never succeed, so none is
            // scheduled.
            if remote_fetch_enabled {
                self.spawn_catalog_retry();
            }
        }

        self.notify_models_updated();
    }

    /// Notify clients about the current model catalog.
    fn notify_models_updated(&self) {
        let available = self.available();
        let current = self.current_model_id();
        let count = available.len();
        kigi_log::unified_log::info(
            "model catalog: notifying clients",
            None,
            Some(serde_json::json!({
                "model_count": count,
                "current_model_id": current.0.as_ref(),
            })),
        );
        if let Some(ref gw) = *self.inner.gateway.read() {
            let model_state =
                acp::SessionModelState::new(current, available.values().cloned().collect());
            if let Ok(params) = serde_json::value::to_raw_value(&model_state) {
                gw.forward_fire_and_forget(acp::ExtNotification::new(
                    "kigi/models/update",
                    params.into(),
                ));
            }
        }
    }

    /// Hot-reload the catalog from `~/.kigi/models_cache.json` after an
    /// external write (detected by the config file watcher).
    ///
    /// A long-running leader otherwise only refreshes its catalog from its
    /// *own* fetch paths (startup prefetch, auth change, response-header etag).
    /// When another kigi process sharing `~/.kigi` (a `--no-leader` run, a
    /// newer client, kigi-desktop) fetches a fresher `/v1/models` catalog and
    /// persists it, this picks it up without a network round-trip.
    ///
    /// Guards, in order:
    /// 1. `load_fresh` — rejects stale (TTL), version-mismatched,
    ///    auth-method-mismatched, or origin-mismatched cache files (another
    ///    process running with different credentials or pointed at a
    ///    different backend must not poison this catalog).
    /// 2. Content dedup — the leader itself rewrites the cache file
    ///    (`persist` after fetch, `renew_ttl` on same-etag responses), and the
    ///    watcher has no self-write suppression. If the cached models match
    ///    the in-memory prefetched catalog this is a no-op (the etag is still
    ///    adopted so `refresh_if_new_etag` doesn't refetch needlessly).
    ///
    /// On a real change: swaps the prefetched catalog, rebuilds, re-resolves
    /// the configured default when this is the first real catalog (otherwise
    /// reselects the current model if it disappeared), and notifies clients.
    pub fn reload_from_disk_cache(&self) {
        self.reload_from_cache_manager(&self.inner.cache);
    }

    /// Core of [`Self::reload_from_disk_cache`], parameterized over the cache
    /// manager so tests can point it at a temp file (the production
    /// `ModelsCacheManager` path is fixed to `kigi_home()`, a process-wide
    /// `OnceLock`).
    fn reload_from_cache_manager(&self, cache: &ModelsCacheManager) {
        let fetch_auth = *self.inner.fetch_auth.read();
        let Some(cached) = cache.load_fresh(&fetch_auth.cache_auth_method(), &self.cache_origin())
        else {
            tracing::debug!("models cache changed on disk but is not loadable; ignoring");
            return;
        };

        // Self-write / no-change dedup by content. `ModelEntry` doesn't impl
        // `PartialEq` (nested config types), so compare the serialized form —
        // catalogs are small (tens of entries) and writes are debounced.
        let same_content = {
            let prefetched = self.inner.prefetched.read();
            prefetched.as_ref().is_some_and(|current| {
                serde_json::to_string(current).ok() == serde_json::to_string(&cached.models).ok()
            })
        };
        if same_content {
            // Adopt the (possibly newer) etag without a rebuild so the next
            // response-header comparison in `refresh_if_new_etag` is accurate.
            if cached.etag.is_some() {
                *self.inner.etag.write() = cached.etag;
            }
            tracing::debug!("models cache changed on disk but catalog is identical; skipping");
            return;
        }

        let cfg = self.inner.cfg.read().clone();
        let count = cached.models.len();
        // Capture whether this is the first real catalog (mirrors
        // `apply_refresh_result`): if the leader bootstrapped on bundled
        // defaults, the configured default must be re-resolved against the
        // real catalog rather than left on a placeholder.
        let first_real_catalog = {
            let mut flag = self.inner.has_fetched_real_catalog.write();
            let was_first = !*flag;
            *flag = true;
            was_first
        };
        *self.inner.prefetched.write() = Some(cached.models.clone());
        self.rebuild(&cfg, Some(cached.models));
        *self.inner.etag.write() = cached.etag;
        if first_real_catalog {
            self.reselect_default_model(&cfg);
        } else {
            self.reselect_current_model_if_missing(&cfg);
        }

        // Recompute the prompt-block flag (mirrors `apply_refresh_result`) so
        // a corrective external cache write unlatches a previously latched
        // "allowlist excludes everything" state instead of keeping prompts
        // blocked against a stale catalog.
        let excludes_all = allowlist_matches_nothing(&cfg, &self.inner.models.read());
        self.inner
            .allowlist_excludes_all
            .store(excludes_all, Ordering::Relaxed);
        if excludes_all {
            tracing::error!("allowed_models excludes all fetched models; prompts will be blocked");
        }

        tracing::info!(count, "model catalog hot-reloaded from disk cache");
        kigi_log::unified_log::info(
            "model catalog: reloaded from external disk-cache write",
            None,
            Some(serde_json::json!({ "model_count": count })),
        );
        self.notify_models_updated();
    }

    /// Retry model catalog fetch in the background with exponential backoff.
    ///
    /// Spawned when `on_auth_changed` falls back to bundled defaults. Uses the
    /// crate-standard `execute_with_backoff` (5 attempts, 5s base, 60s cap) and
    /// notifies clients on success so the UI recovers after sleep/resume without
    /// requiring a manual restart.
    fn spawn_catalog_retry(&self) {
        // Deliberate no-fetch state: a retry loop can never succeed, so don't
        // start one (defensive re-check; the spawn site already gates).
        if !crate::util::config::resolve_remote_fetch_enabled() {
            return;
        }
        // Prevent overlapping retry loops.
        if self
            .inner
            .retry_in_flight
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            tracing::debug!("model catalog retry already in flight, skipping");
            return;
        }

        let mgr = self.clone();
        tokio::task::spawn(async move {
            let backoff = crate::tools::retry::BackoffConfig::new(5, 5_000, 60_000);

            let result = crate::tools::retry::execute_with_backoff(
                &backoff,
                || {
                    let mgr = mgr.clone();
                    async move {
                        // Bail out early if another code path already loaded a real catalog.
                        if *mgr.inner.has_fetched_real_catalog.read() {
                            return Ok(());
                        }

                        mgr.fetch_and_apply().await;

                        if *mgr.inner.has_fetched_real_catalog.read() {
                            Ok(())
                        } else {
                            Err("model catalog fetch returned no models")
                        }
                    }
                },
                |attempt, max_retries, delay| async move {
                    kigi_log::unified_log::warn(
                        "model catalog: retry scheduled",
                        None,
                        Some(serde_json::json!({
                            "attempt": attempt,
                            "max_retries": max_retries,
                            "delay_ms": delay.as_millis() as u64,
                        })),
                    );
                },
            )
            .await;

            match result {
                Ok(()) => {
                    let count = mgr.available().len();
                    kigi_log::unified_log::info(
                        "model catalog: retry succeeded",
                        None,
                        Some(serde_json::json!({ "model_count": count })),
                    );
                    mgr.notify_models_updated();
                }
                Err(e) => {
                    kigi_log::unified_log::warn(
                        "model catalog: all retries exhausted",
                        None,
                        Some(serde_json::json!({ "error": e })),
                    );
                }
            }

            mgr.inner.retry_in_flight.store(false, Ordering::Release);
        });
    }

    /// Refresh the model catalog on every auth token refresh.
    ///
    /// Listens for [`AuthManager::refresh_notifier`] signals directly,
    /// bypassing the FSEvents file watcher which can silently stop
    /// delivering events on macOS after resume from sleep. On each
    /// notification the catalog is re-fetched from the server; if the
    /// fetch succeeds and the catalog changed, clients are notified
    /// via `kigi/models/update`.
    pub fn start_auth_refresh_watcher(&self, notify: Arc<tokio::sync::Notify>) {
        let mgr = self.clone();
        let had_catalog_at_start = *self.inner.has_fetched_real_catalog.read();
        kigi_log::unified_log::info(
            "model catalog: auth refresh watcher started",
            None,
            Some(serde_json::json!({
                "had_real_catalog": had_catalog_at_start,
                "model_count": self.available().len(),
            })),
        );
        tokio::spawn(async move {
            loop {
                notify.notified().await;
                // Deliberate no-fetch state: skip the refresh entirely so the
                // failure-classifying logs below keep meaning "actually failed".
                if !crate::util::config::resolve_remote_fetch_enabled() {
                    tracing::debug!(
                        "model catalog: auth refresh watcher skipped (remote_fetch disabled)"
                    );
                    continue;
                }
                let had_catalog = *mgr.inner.has_fetched_real_catalog.read();
                let old_count = mgr.available().len();
                kigi_log::unified_log::info(
                    "model catalog: auth refresh watcher triggered",
                    None,
                    Some(serde_json::json!({
                        "had_real_catalog": had_catalog,
                        "model_count_before": old_count,
                    })),
                );
                mgr.fetch_and_apply().await;
                let has_catalog = *mgr.inner.has_fetched_real_catalog.read();
                let new_count = mgr.available().len();
                if has_catalog {
                    if !had_catalog || new_count != old_count {
                        kigi_log::unified_log::info(
                            "model catalog: auth refresh watcher updated catalog",
                            None,
                            Some(serde_json::json!({
                                "model_count_before": old_count,
                                "model_count_after": new_count,
                                "was_recovery": !had_catalog,
                            })),
                        );
                    }
                    mgr.notify_models_updated();
                } else {
                    kigi_log::unified_log::warn(
                        "model catalog: auth refresh watcher fetch failed",
                        None,
                        Some(serde_json::json!({
                            "model_count": old_count,
                        })),
                    );
                }
            }
        });
    }

    /// Wipe in-memory state so a previous identity's catalog doesn't leak.
    fn clear(&self) {
        *self.inner.prefetched.write() = None;
        *self.inner.models.write() = IndexMap::new();
        *self.inner.etag.write() = None;
        *self.inner.has_fetched_real_catalog.write() = false;
        self.inner
            .allowlist_excludes_all
            .store(false, Ordering::Relaxed);
    }

    /// Build the agent-wide baseline `SamplingConfig` from the current model +
    /// auth state, together with the registry platform of the catalog entry it
    /// was BUILT from.
    ///
    /// H-a: the platform is returned WITH the config, never re-derived later.
    /// The shared config is built exactly once (`MvpAgent::with_models`) and
    /// never rebuilt, while [`Self::current_model_id`] moves on every non-Leader
    /// model switch and on catalog reselection — so a guard that re-resolved
    /// `config.model` against the LIVE cell answers about a DIFFERENT entry as
    /// soon as the two drift.
    pub fn sampling_config(&self) -> BaselineSamplingConfig {
        let config = self.inner.cfg.read().clone();
        let current_model_id = self.current_model_id();
        let all_models = self.models();
        let fallback;
        let current_model = match all_models
            .get(current_model_id.0.as_ref())
            .or_else(|| all_models.values().next())
        {
            Some(m) => m,
            None => {
                tracing::warn!("no models available in catalog; defaulting to bundled model");
                let default_id = crate::models::default_model().to_string();
                fallback = ModelEntry::fallback(&default_id, &config.endpoints);
                &fallback
            }
        };

        // H1: the session bearer comes from the ONE credential chokepoint,
        // resolved against the CURRENT MODEL's own platform AND endpoint. This
        // used to be a local re-derivation that fell through to
        // `auth_manager.current_or_expired()` for every non-OAuth platform —
        // byte-for-byte the round-1 defect, reachable with ZERO configuration
        // (`default_models.json` bundles `moonshot-cn/*` entries a Kimi
        // subscription user sees on first launch / offline, and this config is
        // the `MvpAgent` baseline that seeds subagents and the
        // unresolved-model fallback).
        let credentials = resolve_credentials(
            current_model,
            self.credential_authority()
                .credential_for_model(current_model)
                .as_ref(),
        );

        BaselineSamplingConfig {
            // The SAME entry the credential above was resolved against, so the
            // config's guards ask about the model the config represents.
            platform: crate::auth::credential_authority::entry_platform(current_model),
            config: sampling_config_for_model(
                current_model,
                credentials,
                config.endpoints.alpha_test_key.clone(),
            ),
        }
    }

    /// The credential chokepoint for this manager: the session's EFFECTIVE
    /// endpoints (so a managed `[endpoints] coding_api_base_url` deployment is
    /// recognised) plus the primary session manager, which the authority keeps
    /// private.
    pub(crate) fn credential_authority(
        &self,
    ) -> crate::auth::credential_authority::CredentialAuthority {
        crate::auth::credential_authority::CredentialAuthority::new(
            self.inner.cfg.read().endpoints.clone(),
            Some(self.inner.auth_manager.clone()),
        )
    }

    /// Disk-cache origin key for this manager's current endpoints/auth shape
    /// (see [`ModelsCache::origin`]).
    fn cache_origin(&self) -> String {
        let (endpoints, platforms) = {
            let cfg = self.inner.cfg.read();
            (cfg.endpoints.clone(), cfg.platforms.clone())
        };
        let fetch_auth = *self.inner.fetch_auth.read();
        let has_oauth = self.inner.auth_manager.current_or_expired().is_some();
        let platform_keys = PlatformApiKeys::resolve(&platforms);
        // The origin key encodes only enabled-platform NAMES + URLs (never
        // tokens). Stored subscription-OAuth platforms join via presence-only
        // stubs (cheap auth.json scan — no per-provider AuthManager on this
        // sync path) so this origin matches the fetch path's, which enables
        // those platforms with real bearers.
        crate::agent::models_fetch::models_fetch_origin(
            &endpoints,
            fetch_auth,
            has_oauth,
            &crate::agent::models_fetch::stored_oauth_token_stubs(
                &crate::auth::oauth_registry::pool_home(),
            ),
            &platform_keys,
        )
    }

    fn try_load_cache(&self) -> bool {
        let fetch_auth = *self.inner.fetch_auth.read();
        let Some(cached) = self
            .inner
            .cache
            .load_fresh(&fetch_auth.cache_auth_method(), &self.cache_origin())
        else {
            return false;
        };
        let cfg = self.inner.cfg.read().clone();
        *self.inner.has_fetched_real_catalog.write() = true;
        *self.inner.prefetched.write() = Some(cached.models.clone());
        self.rebuild(&cfg, Some(cached.models));
        *self.inner.etag.write() = cached.etag;
        true
    }

    fn spawn_fetch(&self, new_etag: Option<String>) {
        // Degrade to Offline: keep serving the current (cache/static) catalog.
        if !crate::util::config::resolve_remote_fetch_enabled() {
            tracing::info!("model catalog refresh skipped: remote_fetch disabled");
            return;
        }
        let cfg = self.inner.cfg.read().clone();
        let mgr = self.clone();

        tokio::task::spawn(async move {
            let new_prefetched = mgr.fetch_catalog_with_oauth_retry(&cfg).await;
            if !mgr.apply_refresh_result(&cfg, new_prefetched, new_etag) {
                return;
            }
            tracing::info!("models manager refreshed");
            mgr.notify_models_updated();
        });
    }

    /// Fetch the catalog; on an OAuth-platform 401, force a token refresh via
    /// the 401-recovery state machine and retry ONCE with the rotated bearer
    /// (port of kimi-cli `refresh_managed_models`' 401 retry).
    async fn fetch_catalog_with_oauth_retry(
        &self,
        cfg: &config::Config,
    ) -> Option<IndexMap<String, ModelEntry>> {
        let endpoints = cfg.endpoints.clone();
        let fetch_auth = *self.inner.fetch_auth.read();
        let platform_keys = PlatformApiKeys::resolve(&cfg.platforms);
        let auth = self.inner.auth_manager.auth().await.ok();
        // Resolve each generic device-code OAuth platform's OWN session token
        // (refreshed on expiry) from its own scope — independent of the Kimi
        // session above.
        let oauth_tokens = crate::agent::models_fetch::resolve_generic_oauth_tokens(
            // M7: the SAME home the OAuth pool resolves from. Under `cargo
            // test` that is a disposable path, so a catalog fetch can never
            // read the developer's real `~/.kigi` tokens — nor fire a real
            // refresh request against them via `configure_refresher()`.
            &crate::auth::oauth_registry::pool_home(),
        )
        .await;
        let outcome = fetch_models_async(
            endpoints.clone(),
            auth,
            oauth_tokens.clone(),
            fetch_auth,
            platform_keys.clone(),
        )
        .await;
        if outcome.models.is_some() {
            return outcome.models;
        }
        if !outcome.oauth_unauthorized {
            return None;
        }
        kigi_log::unified_log::warn(
            "model catalog: OAuth platform returned 401; forcing token refresh and retrying once",
            None,
            None,
        );
        if !self.inner.auth_manager.try_recover_unauthorized().await {
            tracing::warn!("model catalog: token refresh after 401 failed; giving up");
            return None;
        }
        let auth = self.inner.auth_manager.auth().await.ok();
        let oauth_tokens = crate::agent::models_fetch::resolve_generic_oauth_tokens(
            // M7: the SAME home the OAuth pool resolves from. Under `cargo
            // test` that is a disposable path, so a catalog fetch can never
            // read the developer's real `~/.kigi` tokens — nor fire a real
            // refresh request against them via `configure_refresher()`.
            &crate::auth::oauth_registry::pool_home(),
        )
        .await;
        let retry =
            fetch_models_async(endpoints, auth, oauth_tokens, fetch_auth, platform_keys).await;
        if retry.oauth_unauthorized {
            tracing::warn!("model catalog: still unauthorized after token refresh");
        }
        retry.models
    }

    /// Fetch models, rebuild state, and notify clients.
    fn do_refresh(&self, new_etag: Option<String>, strategy: RefreshStrategy) {
        match strategy {
            RefreshStrategy::Offline => {
                if self.try_load_cache() {
                    tracing::info!("models manager refreshed from cache (offline)");
                }
            }
            RefreshStrategy::OnlineIfUncached => {
                if self.try_load_cache() {
                    tracing::info!("models manager refreshed from cache (online_if_uncached)");
                    return;
                }
                self.spawn_fetch(new_etag);
            }
            RefreshStrategy::Online => {
                self.spawn_fetch(new_etag);
            }
        }
    }

    /// Resolve the model list: tries cache first, then fetches from the network.
    pub async fn list_models(&self, strategy: RefreshStrategy) {
        match strategy {
            RefreshStrategy::Offline => {
                self.try_load_cache();
            }
            RefreshStrategy::OnlineIfUncached => {
                if self.try_load_cache() {
                    return;
                }
                self.fetch_and_apply().await;
            }
            RefreshStrategy::Online => {
                self.fetch_and_apply().await;
            }
        }
    }

    async fn fetch_and_apply(&self) {
        self.fetch_and_apply_inner(crate::util::config::resolve_remote_fetch_enabled())
            .await
    }

    /// `remote_fetch_enabled` is a parameter so tests can drive the gate
    /// without touching on-disk config layers.
    async fn fetch_and_apply_inner(&self, remote_fetch_enabled: bool) {
        // Degrade to Offline: keep serving the current (cache/static) catalog.
        if !remote_fetch_enabled {
            tracing::info!("model catalog refresh skipped: remote_fetch disabled");
            return;
        }
        let has_auth = self.inner.auth_manager.current_or_expired().is_some();
        let fetch_auth = *self.inner.fetch_auth.read();
        let cfg = self.inner.cfg.read().clone();
        kigi_log::unified_log::info(
            "model catalog: fetching",
            None,
            Some(serde_json::json!({
                "has_auth": has_auth,
                "fetch_auth": format!("{fetch_auth:?}"),
            })),
        );
        let new_prefetched = self.fetch_catalog_with_oauth_retry(&cfg).await;
        let success = self.apply_refresh_result(&cfg, new_prefetched, None);
        if success {
            kigi_log::unified_log::info(
                "model catalog: fetch succeeded",
                None,
                Some(serde_json::json!({
                    "model_count": self.available().len(),
                })),
            );
        }
    }

    fn apply_refresh_result(
        &self,
        config: &config::Config,
        new_prefetched: Option<IndexMap<String, ModelEntry>>,
        new_etag: Option<String>,
    ) -> bool {
        let Some(new_prefetched) = new_prefetched else {
            tracing::warn!("model refresh failed, leaving existing models unchanged");
            kigi_log::unified_log::warn(
                "model catalog refresh failed",
                None,
                Some(serde_json::json!({
                    "had_real_catalog": *self.inner.has_fetched_real_catalog.read(),
                })),
            );
            return false;
        };

        let first_real_catalog = {
            let mut flag = self.inner.has_fetched_real_catalog.write();
            let was_first = !*flag;
            *flag = true;
            was_first
        };
        *self.inner.prefetched.write() = Some(new_prefetched.clone());
        self.rebuild(config, Some(new_prefetched));
        *self.inner.etag.write() = new_etag;

        // Can't exit a running app; flag it so the prompt path blocks instead.
        let excludes_all = allowlist_matches_nothing(config, &self.inner.models.read());
        self.inner
            .allowlist_excludes_all
            .store(excludes_all, Ordering::Relaxed);
        if excludes_all {
            tracing::error!("allowed_models excludes all fetched models; prompts will be blocked");
        }

        if first_real_catalog {
            self.reselect_default_model(config);
        } else {
            self.reselect_current_model_if_missing(config);
        }
        true
    }

    pub fn allowlist_excludes_all(&self) -> bool {
        self.inner.allowlist_excludes_all.load(Ordering::Relaxed)
    }

    /// Re-pick the default if `current_model_id` is gone from the catalog *or*
    /// is no longer `user_selectable` (e.g. a config reload narrowed
    /// `allowed_models`), so UI and sampling don't disagree on the active model.
    fn reselect_current_model_if_missing(&self, config: &config::Config) {
        let current = self.inner.current_model_id.read().clone();
        let needs_reselection = {
            let models = self.inner.models.read();
            match models.get(current.0.as_ref()) {
                None => true,
                Some(entry) => !entry.info.user_selectable,
            }
        };
        if !needs_reselection {
            return;
        }
        let (key, _, source) = {
            let models = self.inner.models.read();
            resolve_default_model(config, &models, self.is_session_auth())
        };
        let new_id = acp::ModelId::new(Arc::from(key));
        tracing::info!(
            old = %current.0, new = %new_id.0, source = %source,
            "current model not in new catalog, reselecting default"
        );
        *self.inner.current_model_id.write() = new_id;
    }

    /// Re-resolve the default model against the current catalog.
    ///
    /// Called on first catalog fetch and when `apply_config` detects a
    /// preferred-model change.
    fn reselect_default_model(&self, config: &config::Config) {
        let (key, _, source) = {
            let models = self.inner.models.read();
            resolve_default_model(config, &models, self.is_session_auth())
        };
        let new_id = acp::ModelId::new(Arc::from(key));
        let current = self.inner.current_model_id.read().clone();
        if current.0.as_ref() != new_id.0.as_ref() {
            tracing::info!(
                old = %current.0, new = %new_id.0, source = %source,
                "re-resolved default model after catalog populated"
            );
            *self.inner.current_model_id.write() = new_id;
        }
    }
}

// ── Refresh strategy ────────────────────────────────────────────────────────

/// How to resolve the model list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStrategy {
    /// Always fetch from network, ignore cache.
    Online,
    /// Only use cached data, never fetch.
    Offline,
    /// Use cache if fresh, otherwise fetch.
    OnlineIfUncached,
}

// ── Disk cache ──────────────────────────────────────────────────────────────

const MODELS_CACHE_FILE: &str = "models_cache.json";
const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(serde::Serialize, serde::Deserialize)]
struct ModelsCache {
    fetched_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kigi_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth_method: Option<CacheAuthMethod>,
    /// Models-list URL this catalog was fetched from
    /// ([`crate::agent::models_fetch::models_fetch_origin`]). Compared on load so a cache
    /// written against one backend is a miss for another: entries embed
    /// absolute `base_url`s, so adopting a foreign-origin cache silently
    /// re-points inference (the windows lifecycle e2e failed exactly this
    /// way — test 1's mock-server catalog, cached in the shared profile,
    /// sent test 2's prompts to a dead port). `None` (legacy files) never
    /// matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    models: IndexMap<String, ModelEntry>,
}

impl ModelsCache {
    fn is_fresh(&self, ttl: std::time::Duration) -> bool {
        let Ok(ttl) = ChronoDuration::from_std(ttl) else {
            return false;
        };
        let age = Utc::now().signed_duration_since(self.fetched_at);
        age >= ChronoDuration::zero() && age < ttl
    }
}

struct CacheResult {
    models: IndexMap<String, ModelEntry>,
    etag: Option<String>,
}

struct ModelsCacheManager {
    path: std::path::PathBuf,
    ttl: std::time::Duration,
}

impl ModelsCacheManager {
    fn new() -> Self {
        // `KIGI_MODELS_CACHE_DIR` re-homes the cache file; primarily a seam
        // for tests (the unit-test process shares one `kigi_home()` OnceLock)
        // and for e2e runs that must not touch the real profile.
        let dir = std::env::var("KIGI_MODELS_CACHE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| crate::util::kigi_home::kigi_home());
        Self {
            path: dir.join(MODELS_CACHE_FILE),
            ttl: CACHE_TTL,
        }
    }

    /// Sync; used by `prefetch_models_blocking`. Will be removed once startup
    /// prefetch is async.
    fn load_fresh(
        &self,
        expected_auth: &CacheAuthMethod,
        expected_origin: &str,
    ) -> Option<CacheResult> {
        let cache = self.load_matching(expected_auth, expected_origin)?;
        if !cache.is_fresh(self.ttl) {
            tracing::debug!("models cache is stale");
            return None;
        }
        tracing::debug!(count = cache.models.len(), "loaded models from disk cache");
        Some(CacheResult {
            models: cache.models,
            etag: cache.etag,
        })
    }

    /// Last-resort cache read after a FAILED sync (PRD F4: "sync failure →
    /// use last cache"): same version/auth/origin guards as [`Self::load_fresh`]
    /// but ignores the TTL — a stale catalog from the same fetch plan beats
    /// the bundled offline table.
    fn load_ignoring_ttl(
        &self,
        expected_auth: &CacheAuthMethod,
        expected_origin: &str,
    ) -> Option<CacheResult> {
        let cache = self.load_matching(expected_auth, expected_origin)?;
        Some(CacheResult {
            models: cache.models,
            etag: cache.etag,
        })
    }

    /// Shared read + version/auth/origin guards (no TTL check).
    fn load_matching(
        &self,
        expected_auth: &CacheAuthMethod,
        expected_origin: &str,
    ) -> Option<ModelsCache> {
        let data = std::fs::read(&self.path).ok()?;
        let cache: ModelsCache = serde_json::from_slice(&data).ok()?;
        if cache.kigi_version.as_deref() != Some(kigi_version::VERSION) {
            tracing::debug!("models cache version mismatch");
            return None;
        }
        if cache.auth_method.as_ref() != Some(expected_auth) {
            tracing::debug!("models cache auth method mismatch");
            return None;
        }
        if cache.origin.as_deref() != Some(expected_origin) {
            tracing::debug!(
                cached = ?cache.origin,
                expected = expected_origin,
                "models cache origin mismatch"
            );
            return None;
        }
        Some(cache)
    }

    /// Sync; see `load_fresh` note.
    fn persist(
        &self,
        models: &IndexMap<String, ModelEntry>,
        etag: Option<&str>,
        auth_method: CacheAuthMethod,
        origin: &str,
    ) {
        let cache = ModelsCache {
            fetched_at: Utc::now(),
            kigi_version: Some(kigi_version::VERSION.to_string()),
            auth_method: Some(auth_method),
            origin: Some(origin.to_string()),
            etag: etag.map(|s| s.to_string()),
            models: models.clone(),
        };
        self.atomic_write(&cache);
    }

    async fn renew_ttl(&self, expected_auth: &CacheAuthMethod, expected_origin: &str) {
        let data = match tokio::fs::read(&self.path).await {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!(error = %e, "models cache TTL renewal: read failed");
                return;
            }
        };
        let Ok(mut cache) = serde_json::from_slice::<ModelsCache>(&data) else {
            return;
        };
        if cache.auth_method.as_ref() != Some(expected_auth) {
            tracing::debug!("models cache TTL renewal skipped: auth method mismatch");
            return;
        }
        if cache.origin.as_deref() != Some(expected_origin) {
            tracing::debug!("models cache TTL renewal skipped: origin mismatch");
            return;
        }
        cache.fetched_at = Utc::now();
        self.atomic_write_async(&cache).await;
        tracing::debug!("models cache TTL renewed");
    }

    /// Sync; see `load_fresh` note.
    fn invalidate(&self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => tracing::info!("models disk cache invalidated"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(error = %e, "failed to invalidate models disk cache"),
        }
    }

    /// Unique tmp suffix (PID + nanos) so concurrent writers never share an
    /// inode (mirrors `util::config::persist`).
    fn tmp_path(&self) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        self.path
            .with_extension(format!("json.tmp.{}.{}", std::process::id(), nanos))
    }

    /// Sync; see `load_fresh` note. Best-effort, but NEVER silent: a failed
    /// cache write leaves a stale catalog on disk, which on Windows (sharing
    /// violations) previously diverged picker behavior with zero trace.
    fn atomic_write(&self, cache: &ModelsCache) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.tmp_path();
        let result = serde_json::to_vec_pretty(cache)
            .map_err(std::io::Error::other)
            .and_then(|json| std::fs::write(&tmp, &json))
            .and_then(|()| crate::util::fs::replace_file(&tmp, &self.path));
        if let Err(e) = result {
            tracing::warn!(error = %e, path = %self.path.display(), "models cache write failed");
        }
    }

    async fn atomic_write_async(&self, cache: &ModelsCache) {
        if let Some(parent) = self.path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let tmp = self.tmp_path();
        let json = match serde_json::to_vec_pretty(cache) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!(error = %e, "models cache serialize failed");
                return;
            }
        };
        let result = match tokio::fs::write(&tmp, &json).await {
            Ok(()) => {
                let dest = self.path.clone();
                tokio::task::spawn_blocking(move || crate::util::fs::replace_file(&tmp, &dest))
                    .await
                    .unwrap_or_else(|e| Err(std::io::Error::other(e)))
            }
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, path = %self.path.display(), "models cache write failed");
        }
    }
}

// ── Fetch ───────────────────────────────────────────────────────────────────

/// Build the prefetched model map from a flat list of entries.
///
/// Each entry is keyed by its `id` field (falling back to the `model` slug
/// when `id` is absent). Platform-registry entries carry
/// `{platform_id}/{model_id}` ids (PRD F4 managed keys), so the same bare
/// model id can coexist for several platforms without collision.
fn build_prefetched_map(models: Vec<config::ModelEntryConfig>) -> IndexMap<String, ModelEntry> {
    let mut map: IndexMap<String, ModelEntry> = IndexMap::with_capacity(models.len());
    for m in models {
        let key = m.id.clone().unwrap_or_else(|| m.model.clone());
        let info = config::ModelInfo::from_config(&m);
        let entry = ModelEntry {
            info,
            api_key: None,
            // Env-var NAMES only (open-platform key lookup); never values.
            env_key: m.env_key.clone(),
            api_base_url: m.api_base_url.clone(),
        };
        map.insert(key, entry);
    }
    map
}

/// Outcome of a gated catalog fetch. `oauth_unauthorized` survives total
/// failure so the async layer can force a token refresh and retry.
pub(crate) struct ModelsFetchOutcome {
    pub models: Option<IndexMap<String, ModelEntry>>,
    pub oauth_unauthorized: bool,
}

impl ModelsFetchOutcome {
    fn failed(oauth_unauthorized: bool) -> Self {
        Self {
            models: None,
            oauth_unauthorized,
        }
    }
}

/// Fetch remote models. Checks disk cache first; persists after fetch.
pub(crate) fn prefetch_models_blocking(
    endpoints: &config::EndpointsConfig,
    auth: Option<&KimiAuth>,
    oauth_tokens: &crate::agent::models_fetch::OAuthSessionTokens,
    fetch_auth: ModelFetchAuth,
    platform_keys: &PlatformApiKeys,
) -> Option<IndexMap<String, ModelEntry>> {
    prefetch_models_blocking_gated(
        endpoints,
        auth,
        oauth_tokens,
        fetch_auth,
        platform_keys,
        crate::util::config::resolve_remote_fetch_enabled(),
    )
    .models
}

/// `remote_fetch_enabled` is a parameter so the pair helper above resolves the
/// knob once for both halves.
fn prefetch_models_blocking_gated(
    endpoints: &config::EndpointsConfig,
    auth: Option<&KimiAuth>,
    oauth_tokens: &crate::agent::models_fetch::OAuthSessionTokens,
    fetch_auth: ModelFetchAuth,
    platform_keys: &PlatformApiKeys,
    remote_fetch_enabled: bool,
) -> ModelsFetchOutcome {
    let cache_auth = fetch_auth.cache_auth_method();
    // Same fetch plan the network path below executes — the cache is only
    // valid for it.
    let cache_origin = crate::agent::models_fetch::models_fetch_origin(
        endpoints,
        fetch_auth,
        auth.is_some(),
        oauth_tokens,
        platform_keys,
    );
    let cache = ModelsCacheManager::new();
    if let Some(cached) = cache.load_fresh(&cache_auth, &cache_origin) {
        tracing::info!(
            count = cached.models.len(),
            "model sync: serving fresh disk cache"
        );
        return ModelsFetchOutcome {
            models: Some(cached.models),
            oauth_unauthorized: false,
        };
    }

    // Every catalog fetch in the product funnels through here, so this single
    // gate also covers callers that don't go through the prefetch-env check
    // (leader, headless, stdio, server). Cache above is local and stays usable.
    if !remote_fetch_enabled {
        tracing::info!("models fetch skipped: remote_fetch disabled");
        return ModelsFetchOutcome::failed(false);
    }

    let _timer = crate::instrumentation_timer!("startup.fetch_models_blocking");
    match fetch_models_blocking(endpoints, auth, oauth_tokens, fetch_auth, platform_keys) {
        Ok(FetchModelsResult {
            models,
            etag,
            oauth_unauthorized,
        }) if !models.is_empty() => {
            let map = build_prefetched_map(models);

            // NOTE: inheriting context_window / agent_type / api_backend
            // from hardcoded defaults is handled centrally in
            // `resolve_model_list` (config.rs), not here. Don't re-add it.

            tracing::info!(count = map.len(), etag = ?etag, "model sync: fetched catalog");
            cache.persist(&map, etag.as_deref(), cache_auth, &cache_origin);
            ModelsFetchOutcome {
                models: Some(map),
                oauth_unauthorized,
            }
        }
        Ok(FetchModelsResult {
            oauth_unauthorized, ..
        }) => {
            tracing::warn!(oauth_unauthorized, "model sync: no models fetched");
            stale_cache_or_failure(&cache, &cache_auth, &cache_origin, oauth_unauthorized)
        }
        Err(e) => {
            tracing::warn!("model sync failed: {e:?}");
            stale_cache_or_failure(&cache, &cache_auth, &cache_origin, false)
        }
    }
}

/// PRD F4 failure ladder: sync failed → last (possibly stale) cache for the
/// same fetch plan; no usable cache → the caller falls back to the bundled
/// offline table. `oauth_unauthorized` is preserved either way so the async
/// 401 refresh-retry still fires (a stale cache must not mask a dead token).
fn stale_cache_or_failure(
    cache: &ModelsCacheManager,
    cache_auth: &CacheAuthMethod,
    cache_origin: &str,
    oauth_unauthorized: bool,
) -> ModelsFetchOutcome {
    if let Some(cached) = cache.load_ignoring_ttl(cache_auth, cache_origin) {
        tracing::warn!(
            count = cached.models.len(),
            "model sync failed; serving last cached catalog (may be stale)"
        );
        return ModelsFetchOutcome {
            models: Some(cached.models),
            oauth_unauthorized,
        };
    }
    tracing::warn!("model sync failed and no usable cache; falling back to bundled catalog");
    ModelsFetchOutcome::failed(oauth_unauthorized)
}

/// Startup prefetch result: the model catalog, when a fetch plan existed.
pub struct EarlyPrefetchResult {
    pub models: Option<IndexMap<String, ModelEntry>>,
}

/// Handle for a startup prefetch thread.
pub type EarlyPrefetchHandle = std::thread::JoinHandle<EarlyPrefetchResult>;

struct PrefetchEnv {
    auth: Option<KimiAuth>,
    endpoints: config::EndpointsConfig,
    model_fetch_auth: ModelFetchAuth,
    platform_keys: PlatformApiKeys,
}

fn resolve_prefetch_env_with_auth(auth: Option<KimiAuth>) -> Option<PrefetchEnv> {
    let _timer = crate::instrumentation_timer!("startup.early_prefetch_launch");
    // Config-aware (not env-only) so the prefetch can't leak the bearer to the BYOK endpoint.
    let mut endpoints = config::EndpointsConfig::from_effective_config();

    if endpoints.deployment_key.is_none() {
        endpoints.deployment_key = crate::managed_config::resolve_deployment_key();
    }

    resolve_prefetch_env_from_parts(
        auth,
        endpoints,
        PlatformApiKeys::resolve_from_effective_config(),
        !crate::agent::models_fetch::stored_oauth_platforms(
            &crate::auth::oauth_registry::pool_home(),
        )
        .is_empty(),
        crate::util::config::resolve_remote_fetch_enabled(),
    )
}

/// Decision core of [`resolve_prefetch_env_with_auth`], split from the config
/// loading so the gate is unit-testable.
///
/// `remote_fetch_enabled = false` wins over every credential shape AND over
/// `has_custom_endpoint()` (which otherwise forces the prefetch to run): the
/// explicit off switch must hold even when a stray login, a platform API key,
/// or a `deployment_key` would re-arm the prefetch — and with it the
/// deployment-config sync on the prefetch thread.
///
/// Decision core of [`ModelsManager::on_auth_changed`]'s wipe guard, split
/// from the disk probes so it is unit-testable: `true` = no fetch source
/// exists, wipe the previous identity's catalog.
///
/// A stored subscription-OAuth session IS a fetch source (its platform
/// passes `enabled_platforms`), so it vetoes the wipe even with no primary
/// (Kimi) session and no API key. Regression: ignoring it meant a
/// claude-pro-max-only login wiped the catalog and returned before the
/// fetch — the session stayed on the bundled Kimi table with an empty
/// picker ("unknown" model).
fn should_wipe_catalog_on_auth_change(
    has_primary_session: bool,
    fetch_auth: ModelFetchAuth,
    has_platform_keys: bool,
    has_stored_oauth: bool,
) -> bool {
    !has_primary_session
        && fetch_auth == ModelFetchAuth::Platforms
        && !has_platform_keys
        && !has_stored_oauth
}

/// PRD F2 acceptance: a moonshot API key alone (no subscription login) must
/// arm the prefetch so the catalog syncs on startup. Likewise a stored
/// subscription-OAuth session alone (`has_stored_oauth`, e.g. a
/// claude-pro-max login with no Kimi session): its models are the user's
/// ONLY models, so the prefetch must run for them.
fn resolve_prefetch_env_from_parts(
    auth: Option<KimiAuth>,
    endpoints: config::EndpointsConfig,
    platform_keys: PlatformApiKeys,
    has_stored_oauth: bool,
    remote_fetch_enabled: bool,
) -> Option<PrefetchEnv> {
    if !remote_fetch_enabled {
        tracing::info!("startup model prefetch skipped: remote_fetch disabled");
        return None;
    }

    let model_fetch_auth = ModelFetchAuth::resolve(&endpoints);

    if auth.is_none()
        && !endpoints.has_custom_endpoint()
        && !platform_keys.any()
        && !has_stored_oauth
    {
        return None;
    }

    Some(PrefetchEnv {
        auth,
        endpoints,
        model_fetch_auth,
        platform_keys,
    })
}

fn resolve_prefetch_env(kimi_code_config: Option<KimiCodeConfig>) -> Option<PrefetchEnv> {
    let kigi_home = crate::util::kigi_home::kigi_home();
    let auth_manager = AuthManager::new(&kigi_home, kimi_code_config.unwrap_or_default());
    let auth = auth_manager.current();
    resolve_prefetch_env_with_auth(auth)
}

/// Start the model-catalog prefetch on a background thread using pre-resolved auth.
///
/// When the caller has already obtained valid credentials (e.g. via
/// `try_ensure_fresh_auth`), pass them here to avoid re-reading stale cached
/// credentials from disk.
pub fn start_early_prefetch_with_auth(auth: Option<KimiAuth>) -> Option<EarlyPrefetchHandle> {
    let env = resolve_prefetch_env_with_auth(auth)?;
    Some(spawn_prefetch_thread(env))
}

/// Start the model-catalog prefetch on a background thread.
///
/// Convenience wrapper that reads cached auth from disk. Prefer
/// `start_early_prefetch_with_auth` when you have pre-resolved credentials.
pub fn start_early_prefetch(
    kimi_code_config: Option<KimiCodeConfig>,
) -> Option<EarlyPrefetchHandle> {
    let env = resolve_prefetch_env(kimi_code_config)?;
    Some(spawn_prefetch_thread(env))
}

fn spawn_prefetch_thread(env: PrefetchEnv) -> EarlyPrefetchHandle {
    std::thread::spawn(move || {
        let mut timer = crate::instrumentation_timer!("startup.early_prefetch");
        let proxy_endpoint = env.endpoints.proxy_url();
        timer.with_field("endpoint", proxy_endpoint.as_str());
        // Resolve each stored subscription-OAuth session's bearer (refreshed
        // on expiry) so those platforms are part of the STARTUP fetch plan —
        // otherwise a claude-pro-max-only user boots onto the bundled Kimi
        // table until some later async refresh happens to run, and the cache
        // origin (which encodes enabled platforms) never matches the
        // async-path's claude-inclusive origin.
        let oauth_tokens = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|rt| {
                rt.block_on(crate::agent::models_fetch::resolve_generic_oauth_tokens(
                    &crate::auth::oauth_registry::pool_home(),
                ))
            })
            .unwrap_or_default();
        let models = prefetch_models_blocking(
            &env.endpoints,
            env.auth.as_ref(),
            &oauth_tokens,
            env.model_fetch_auth,
            &env.platform_keys,
        );
        if (env.endpoints.deployment_key.is_some() || crate::managed_config::has_active_team_auth())
            && crate::config::is_managed_config_stale_for(
                &crate::managed_config::current_serving_identity(),
            )
            && crate::managed_config::is_fetch_enabled()
            && let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
        {
            crate::managed_config::clear_orphan();
            let _ = rt.block_on(crate::managed_config::sync());
        }

        EarlyPrefetchResult { models }
    })
}

/// Map a model id (catalog key or routing slug) to its catalog key.
///
/// Sessions persist the routing slug (`[model.X].model`, e.g. `kigi-4.5`);
/// the catalog and `/model` picker use config keys (e.g. `enterprise-kigi`).
/// Last slug match wins so user overrides beat defaults (matches `MvpAgent::resolve_model_id`).
pub(crate) fn resolve_catalog_key(
    models: &IndexMap<String, ModelEntry>,
    id: &acp::ModelId,
) -> Option<acp::ModelId> {
    let id_str = id.0.as_ref();
    if models.contains_key(id_str) {
        return Some(id.clone());
    }
    models
        .iter()
        .rev()
        .find(|(_, entry)| entry.info.model == id_str)
        .map(|(key, _)| acp::ModelId::new(key.clone()))
}

/// The catalog ENTRY a routing slug resolves to. The single lookup behind both
/// the managed key and the endpoint, so a caller can never take the platform
/// from one entry and the `base_url` from another (M6: the aux path used to
/// resolve the platform with `current_key` and the credential with a separate
/// `find_model_by_id`).
///
/// H5: `SamplingConfig::model` is the BARE routing slug, never the catalog key,
/// and duplicate slugs across platforms are BY DESIGN — the registry guarantees
/// an API-key platform and its subscription-OAuth twin list the SAME ids
/// (`xai`/`xai-grok`, `anthropic`/`claude-pro-max`, `openai`/`openai-codex`),
/// and `PlatformId::ALL` orders every API-key platform FIRST. A slug scan
/// therefore resolves the WRONG platform for a user holding both credentials:
/// the OAuth twin loses its live `bearer_resolver` (no mid-session refresh → an
/// unrecoverable 401 ~1h in), its Messages adaptation and its Copilot/Codex
/// identity headers.
///
/// `current_key` — the SESSION's own selected catalog key
/// ([`crate::session::acp_session::SessionActor::selected_catalog_key`], seeded
/// by [`selected_catalog_key_for_spawn`] and rewritten by `SetSessionModel`) —
/// is therefore authoritative whenever it names this slug. It is NOT
/// [`ModelsManager::current_model_id`]: that cell is process-global,
/// last-writer-wins across concurrent sessions and never written at all in
/// Leader mode (H4). Anything else (aux models, subagent overrides, unlisted
/// slugs) falls back to the picker's OWN lookup, [`resolve_catalog_key`], so the
/// auth layer and the picker can never resolve different entries.
///
/// The one caller that legitimately passes `current_model_id` is the SHARED
/// `MvpAgent::sampling_config`, which `ModelsManager::sampling_config` builds
/// from exactly that key — there the two lookups must agree (H-a).
///
/// (L: this rule used to be stated on a `managed_key_for_slug` wrapper that no
/// caller needed once [`platform_for_slug`] resolved the entry itself. The
/// crate-level `#![allow(dead_code)]` in `lib.rs` means an unused helper on a
/// credential path raises no warning, so dead ones are deleted on sight rather
/// than left as a second, unexercised way to answer the same question.)
pub(crate) fn entry_for_slug<'a>(
    models: &'a IndexMap<String, ModelEntry>,
    current_key: Option<&str>,
    slug: &str,
) -> Option<&'a ModelEntry> {
    entry_for_slug_resolution(models, current_key, slug).map(|(entry, _)| entry)
}

/// [`entry_for_slug`] plus whether the SESSION's own selected catalog key is
/// what resolved it. `false` means the entry came from the picker's slug scan —
/// a resolution that is only a GUESS when the slug collides across platforms
/// (H-b).
fn entry_for_slug_resolution<'a>(
    models: &'a IndexMap<String, ModelEntry>,
    current_key: Option<&str>,
    slug: &str,
) -> Option<(&'a ModelEntry, bool)> {
    if let Some(entry) = current_key.and_then(|key| models.get(key))
        && (entry.info.model == slug || current_key == Some(slug))
    {
        return Some((entry, true));
    }
    let key = resolve_catalog_key(models, &acp::ModelId::new(slug.to_string()))?;
    models.get(key.0.as_ref()).map(|entry| (entry, false))
}

/// Whether `slug` is carried as a routing slug by catalog entries belonging to
/// MORE THAN ONE platform — the dual-credential collision the registry creates
/// BY DESIGN (`anthropic` + `claude-pro-max` both list `claude-opus-4-8`;
/// likewise `xai`/`xai-grok`, `openai`/`openai-codex`).
///
/// An exact catalog-key match is never ambiguous: it names exactly one entry.
fn slug_collides_across_platforms(models: &IndexMap<String, ModelEntry>, slug: &str) -> bool {
    if models.contains_key(slug) {
        return false;
    }
    let mut seen: Option<Option<kigi_models::PlatformId>> = None;
    for entry in models.values().filter(|entry| entry.info.model == slug) {
        let platform = crate::auth::credential_authority::entry_platform(entry);
        match seen {
            None => seen = Some(platform),
            Some(first) if first == platform => {}
            Some(_) => return true,
        }
    }
    false
}

/// The registry platform a routing slug resolves to, via [`entry_for_slug`].
/// `None` for a bare / `[model.*]` / unlisted model. Single definition shared by
/// the session actor's inference-auth chokepoints and the aux/summary paths, so
/// the gate, the manager and the wire adaptations can never disagree.
///
/// H-b — REFUSE RATHER THAN GUESS. When `current_key` does not name this slug
/// (`None`, or stale after an `OverrideModelName` rename) the resolution falls
/// through to [`resolve_catalog_key`]'s `.rev()` scan, whose LAST match is the
/// subscription-OAuth twin because `PlatformId::ALL` orders every API-key
/// platform first. Trusting that guess hands an API-KEY session the twin's
/// POOLED bearer — `SamplingClient::post` REPLACES the user's own `sk-ant-…` on
/// the wire — plus the OAuth Messages adaptation, which is precisely what H4
/// exists to prevent. A collided slug the session did not disambiguate
/// therefore resolves to NO platform, which the chokepoint then decides purely
/// by the ENDPOINT: the OAuth host is not the session's coding endpoint, so no
/// session credential, no resolver and no adaptation ride. First-party is
/// untouched — a platform-less model and a `kimi-code` model take the identical
/// endpoint arm.
pub(crate) fn platform_for_slug(
    models: &IndexMap<String, ModelEntry>,
    current_key: Option<&str>,
    slug: &str,
) -> Option<kigi_models::PlatformId> {
    let (key, disambiguated) = match entry_for_slug_resolution(models, current_key, slug) {
        Some((entry, disambiguated)) => (entry.info.id.clone(), disambiguated),
        None => (None, false),
    };
    let platform = kigi_models::parse_managed_model_key(key.as_deref().unwrap_or(slug))
        .map(|(platform, _)| platform)?;
    if platform.oauth().is_some() && !disambiguated && slug_collides_across_platforms(models, slug)
    {
        tracing::warn!(
            slug,
            platform = platform.as_str(),
            "auth: routing slug collides across platforms and this session did not select \
             one — refusing to guess the subscription-OAuth twin (no session credential)"
        );
        return None;
    }
    Some(platform)
}

/// The per-session selected catalog key (H4) a session spawned on
/// `session_model_id` must record: the picker's OWN lookup, never the
/// process-global [`ModelsManager::current_model_id`].
///
/// `session_model_id` is a catalog key on the `new_session` path (the picker's
/// `current_model_id()`), so this is idempotent there. On `load_session` it is
/// the RAW persisted `summary.current_model_id` (`acp_agent.rs`'s spawn call) —
/// `resolve_catalog_key` runs LATER, on the model-state / availability path, not
/// before the spawn — and after any `SetSessionModel` that persisted value is a
/// BARE routing slug, because `handle_set_session_model` persists
/// `sampling_config.model`. The slug branch below is therefore the live resume
/// path, not a theoretical one: it resolves through the picker's OWN lookup, and
/// for a slug that collides across platforms the `.rev()` last match is the
/// resume default (H-b then applies to anything that lookup could not pin).
///
/// Named — rather than inlined at the spawn site — so it is reachable from a
/// test: it is one of only two production writers of the field the whole
/// model→platform rule keys on, and a wrong seed is silent (H-c).
pub(crate) fn selected_catalog_key_for_spawn(
    models: &IndexMap<String, ModelEntry>,
    session_model_id: &acp::ModelId,
) -> Option<String> {
    resolve_catalog_key(models, session_model_id).map(|key| key.0.to_string())
}

/// Catalog key for a persisted session model id, restricted to **selectable**
/// entries. A selectable exact-key match wins (as in [`resolve_catalog_key`]);
/// otherwise the last selectable entry whose routing slug matches `id`, so a
/// non-selectable exact-key entry never shadows a selectable slug match.
pub(crate) fn selectable_catalog_key_for_persisted(
    models: &IndexMap<String, ModelEntry>,
    available: &IndexMap<acp::ModelId, acp::ModelInfo>,
    id: &acp::ModelId,
) -> Option<acp::ModelId> {
    if available.contains_key(id) {
        return Some(id.clone());
    }
    let id_str = id.0.as_ref();
    if let Some((key, _)) = models.iter().rev().find(|(key, entry)| {
        available.contains_key(&acp::ModelId::new((*key).clone())) && entry.info.model == id_str
    }) {
        return Some(acp::ModelId::new(key.clone()));
    }
    resolve_catalog_key(models, id).filter(|key| available.contains_key(key))
}

/// A "campaign-only" preferred flip: the default changed and either side's value
/// is an active campaign default, i.e. the change is attributable to a campaign
/// overlay appearing/disappearing rather than a user/CLI/env edit.
fn is_campaign_only_flip(
    old_preferred: &Option<String>,
    new_preferred: &Option<String>,
    campaign_defaults: &std::collections::HashSet<String>,
) -> bool {
    if new_preferred == old_preferred || new_preferred.is_none() {
        return false;
    }
    new_preferred
        .as_ref()
        .is_some_and(|p| campaign_defaults.contains(p))
        || old_preferred
            .as_ref()
            .is_some_and(|p| campaign_defaults.contains(p))
}

/// Pick the default model: CLI > env > config > remote-settings hint, falling
/// back to the bundled default when the catalog is empty or the preferred
/// model isn't present.
pub(crate) fn resolve_default_model(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
) -> (String, ModelEntry, config::ConfigSource) {
    let visible: IndexMap<String, ModelEntry> = catalog
        .iter()
        .filter(|(_, e)| e.info.visible_for_auth(is_session_auth) && e.info.user_selectable)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let model_pref = config::resolve_string_flag(
        cfg.default_model_override.as_deref(),
        "KIGI_DEFAULT_MODEL",
        cfg.models.default.as_deref(),
        cfg.remote_settings
            .as_ref()
            .and_then(|rs| rs.default_model.as_deref()),
    );

    let first_or_fallback = || -> (String, ModelEntry) {
        if let Some((key, first)) = visible.first() {
            return (key.clone(), first.clone());
        }
        if let Some((key, entry)) = catalog.iter().find(|(_, e)| e.info.user_selectable) {
            tracing::warn!("no auth-visible selectable model; using first selectable entry");
            return (key.clone(), entry.clone());
        }
        // Pre-catalog/degenerate only: nothing selectable. Set the bundled
        // default's flag from `allowed_models` so no reader treats it as allowed.
        tracing::warn!("no selectable models; falling back to bundled default (pre-catalog)");
        let default_id = crate::models::default_model().to_string();
        let mut entry = ModelEntry::fallback(&default_id, &cfg.endpoints);
        entry.info.user_selectable = match ModelGlobSet::compile(cfg.models.allowed_models.as_ref())
        {
            Ok(None) => true,
            Ok(Some(set)) => set.matches(&default_id, &default_id),
            Err(_) => false,
        };
        (default_id, entry)
    };

    match &model_pref {
        None => {
            let (key, first) = first_or_fallback();
            (key, first, config::ConfigSource::Default)
        }
        Some(pref) => {
            let found = visible
                .get_key_value(&pref.value)
                .or_else(|| visible.iter().find(|(_, m)| m.model == pref.value));

            if let Some((key, entry)) = found {
                (key.clone(), entry.clone(), pref.source)
            } else {
                let is_explicit = matches!(
                    pref.source,
                    config::ConfigSource::Cli
                        | config::ConfigSource::Env
                        | config::ConfigSource::Config
                );
                if is_explicit {
                    tracing::warn!(
                        model_id = %pref.value, source = %pref.source,
                        "preferred model not in available models, falling back"
                    );
                } else {
                    tracing::debug!(
                        model_id = %pref.value, source = %pref.source,
                        "remote default_model not in available models, skipping"
                    );
                }
                // A campaign default missing from the catalog falls back to the
                // pre-campaign default before the first-visible fallback. Gated
                // on the missing pref actually being the campaign-driven config
                // value — a CLI/env pref that misses the catalog is not a
                // campaign problem and must not detour through campaign state.
                let campaign_pref_missing = cfg.models.default_is_campaign_driven
                    && matches!(pref.source, config::ConfigSource::Config);
                if campaign_pref_missing
                    && let Some(prev) = cfg
                        .models
                        .pre_campaign_default
                        .as_deref()
                        .filter(|s| !s.is_empty())
                    && let Some((key, entry)) = visible
                        .get_key_value(prev)
                        .or_else(|| visible.iter().find(|(_, m)| m.model == prev))
                {
                    tracing::info!(
                        unavailable = %pref.value, fallback = %prev,
                        "campaign-driven default unavailable in catalog; recovering the pre-campaign default"
                    );
                    return (key.clone(), entry.clone(), config::ConfigSource::Config);
                }
                let (key, first) = first_or_fallback();
                (key, first, config::ConfigSource::Default)
            }
        }
    }
}

/// Filter hidden and auth-gated entries out of `catalog` and convert to ACP wire format.
pub fn available_models(
    catalog: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
) -> IndexMap<acp::ModelId, acp::ModelInfo> {
    let visible: IndexMap<String, ModelEntry> = catalog
        .iter()
        .filter(|(_, e)| e.info.visible_for_auth(is_session_auth))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    config::to_acp_model_info(&visible)
}

/// Compiled glob matcher shared by `allowed_models`, `disabled_models`, and
/// `hidden_models`. Patterns (globset syntax: `*`, `?`, `[...]`) are matched
/// against either the catalog key or the model id.
pub(crate) struct ModelGlobSet(GlobSet);

impl ModelGlobSet {
    /// Compile a filter list (`Ok(None)` for `None`/empty). Fails **closed**: an
    /// invalid pattern returns `Err` listing every bad one for config to reject.
    pub(crate) fn compile(patterns: Option<&Vec<String>>) -> Result<Option<Self>, Vec<String>> {
        let patterns = match patterns {
            Some(p) if !p.is_empty() => p,
            _ => return Ok(None),
        };
        let mut builder = GlobSetBuilder::new();
        let mut invalid = Vec::new();
        for pat in patterns {
            match Glob::new(pat) {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(_) => invalid.push(pat.clone()),
            }
        }
        if !invalid.is_empty() {
            return Err(invalid);
        }
        builder
            .build()
            .map(|set| Some(Self(set)))
            .map_err(|e| vec![e.to_string()])
    }

    fn matches(&self, key: &str, model: &str) -> bool {
        self.0.is_match(key) || self.0.is_match(model)
    }
}

/// Single source of truth for the catalog. Applies, in order: `disabled_models`
/// (remove), `allowed_models` (mark `user_selectable`), `hidden_models` (mark
/// `hidden`). Special/internal models (web_search, subagents, …) resolve via
/// `find_model_by_id`/`models()` and ignore `user_selectable`, so they need no
/// exemption. Globs are validated at load (`Config::validate_model_filters`);
/// the arms here fail closed if one slips through.
pub(crate) fn resolve_model_catalog(
    cfg: &config::Config,
    prefetched: Option<IndexMap<String, ModelEntry>>,
    platform_keys: &PlatformApiKeys,
) -> IndexMap<String, ModelEntry> {
    let mut catalog: IndexMap<String, ModelEntry> =
        config::resolve_model_list(cfg, prefetched, platform_keys);

    if let Ok(Some(disabled)) = ModelGlobSet::compile(cfg.models.disabled_models.as_ref()) {
        let before = catalog.len();
        catalog.retain(|key, entry| !disabled.matches(key, &entry.model));
        let removed = before - catalog.len();
        if removed > 0 {
            tracing::info!(count = removed, "disabled_models: removed from catalog");
        }
    }

    // None/empty allowlist = allow all.
    match ModelGlobSet::compile(cfg.models.allowed_models.as_ref()) {
        Ok(None) => {
            for entry in catalog.values_mut() {
                entry.info.user_selectable = true;
            }
        }
        Ok(Some(allowed)) => {
            for (key, entry) in catalog.iter_mut() {
                entry.info.user_selectable = allowed.matches(key, &entry.model);
            }
        }
        Err(bad) => {
            tracing::error!(patterns = ?bad, "allowed_models: invalid glob(s); marking nothing selectable");
            for entry in catalog.values_mut() {
                entry.info.user_selectable = false;
            }
        }
    }

    if let Ok(Some(hidden)) = ModelGlobSet::compile(cfg.models.hidden_models.as_ref()) {
        for (key, entry) in catalog.iter_mut() {
            if hidden.matches(key, &entry.model) {
                entry.info.hidden = true;
            }
        }
    }

    // Persisted default first; CLI override below wins when set.
    // Only apply if the model supports reasoning effort.
    if let Some(effort) = cfg.models.default_reasoning_effort
        && let Some(default_id) = cfg.models.default.as_deref()
        && let Some(entry) = catalog.get_mut(default_id)
        && entry.info.supports_reasoning_effort
    {
        entry.info.reasoning_effort = Some(effort);
    }

    // Skip non-reasoning models so we don't send the field to providers that reject it.
    // Also skip models whose effort menu does not include the override (e.g. `--effort none`
    // must not stamp `none` onto kigi-4.5, which only offers low/medium/high).
    if let Some(effort) = cfg.reasoning_effort_override {
        for entry in catalog.values_mut() {
            if model_offers_reasoning_effort(&entry.info, effort) {
                entry.info.reasoning_effort = Some(effort);
            }
        }
    }

    catalog
}

/// Whether `effort` is a value this model will accept on the wire.
///
/// Uses the server `reasoning_efforts` menu when present; otherwise the
/// built-in low/medium/high/xhigh set (same as the pager legacy menu — no
/// `none`/`minimal`).
/// `max` is NOT in the legacy built-in set: models gain it only via an
/// explicit server/BYOK menu entry (Kimi K3's `max` token), so an empty-menu
/// model rejects it rather than sending a level its endpoint may 400 on.
fn model_offers_reasoning_effort(info: &config::ModelInfo, effort: ReasoningEffort) -> bool {
    if !info.supports_reasoning_effort {
        return false;
    }
    if info.reasoning_efforts.is_empty() {
        matches!(
            effort,
            ReasoningEffort::Low
                | ReasoningEffort::Medium
                | ReasoningEffort::High
                | ReasoningEffort::Xhigh
        )
    } else {
        info.reasoning_efforts.iter().any(|opt| opt.value == effort)
    }
}

/// True when an active `allowed_models` allowlist leaves no selectable model.
/// (An excluded *default* does not count — that is recoverable by reselection.)
pub(crate) fn allowlist_matches_nothing(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
) -> bool {
    cfg.models
        .allowed_models
        .as_ref()
        .is_some_and(|a| !a.is_empty())
        && !catalog.values().any(|e| e.info.user_selectable)
}

/// Reject an `allowed_models` allowlist that leaves no selectable model, or that
/// excludes an explicitly configured default (`default`/`-m`). Run only against a
/// real catalog (cache/prefetch/fetched), not the bundled bootstrap set.
pub(crate) fn validate_selectable(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
) -> Result<(), String> {
    let Some(allowed) = cfg.models.allowed_models.as_ref().filter(|a| !a.is_empty()) else {
        return Ok(());
    };
    let patterns = allowed.join(", ");
    if !catalog.values().any(|e| e.info.user_selectable) {
        return Err(format!(
            "None of your available models match allowed_models ({patterns}). \
             Broaden the patterns or remove allowed_models, then try again."
        ));
    }
    for (src, id) in [
        ("default", cfg.models.default.as_deref()),
        ("-m flag", cfg.default_model_override.as_deref()),
    ] {
        if let Some(id) = id
            && let Some(entry) = catalog
                .get(id)
                .or_else(|| catalog.values().find(|e| e.model == id))
            && !entry.info.user_selectable
        {
            return Err(format!(
                "\"{id}\" (your {src}) isn't allowed by allowed_models ({patterns}). \
                 Add it to allowed_models, or set a different model."
            ));
        }
    }
    Ok(())
}

/// Async wrapper around the gated blocking fetch. Keeps the
/// `oauth_unauthorized` signal so callers can drive the 401 refresh-retry.
pub(crate) async fn fetch_models_async(
    endpoints: config::EndpointsConfig,
    auth: Option<KimiAuth>,
    oauth_tokens: crate::agent::models_fetch::OAuthSessionTokens,
    fetch_auth: ModelFetchAuth,
    platform_keys: PlatformApiKeys,
) -> ModelsFetchOutcome {
    tokio::task::spawn_blocking(move || {
        prefetch_models_blocking_gated(
            &endpoints,
            auth.as_ref(),
            &oauth_tokens,
            fetch_auth,
            &platform_keys,
            crate::util::config::resolve_remote_fetch_enabled(),
        )
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!("model fetch task panicked/cancelled: {e}");
        ModelsFetchOutcome::failed(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manager() -> ModelsManager {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        // Use a temp dir so AuthManager finds no credentials — ensures
        // refresh_async bails at the auth check without needing a tokio runtime.
        let tmp = std::env::temp_dir().join("kigi-test-models-manager");
        let auth_manager = Arc::new(AuthManager::new(&tmp, KimiCodeConfig::default()));
        ModelsManager::new(
            None,
            IndexMap::new(),
            acp::ModelId::new("default"),
            auth_manager,
            config::Config::default(),
        )
    }

    fn config_from_toml(toml: &str) -> config::Config {
        config::Config::new_from_toml_cfg(&toml::from_str(toml).unwrap()).unwrap()
    }

    #[test]
    fn model_show_model_fingerprint_reads_catalog_flag() {
        let mgr = test_manager();

        // Entry with the catalog flag set → accessor returns true.
        let mut flagged = ModelEntry {
            info: config::ModelInfo::fallback("fp-model"),
            api_key: None,
            env_key: None,
            api_base_url: None,
        };
        flagged.info.show_model_fingerprint = true;
        mgr.insert_test_entry("fp-model", flagged);

        // Entry without the flag → defaults false.
        mgr.insert_test_entry(
            "plain-model",
            ModelEntry {
                info: config::ModelInfo::fallback("plain-model"),
                api_key: None,
                env_key: None,
                api_base_url: None,
            },
        );

        // Catalog KEY differs from the routing SLUG (custom/enterprise id): the
        // map is keyed "enterprise-key" but the model slug is "enterprise-slug".
        let mut custom = ModelEntry {
            info: config::ModelInfo::fallback("enterprise-slug"),
            api_key: None,
            env_key: None,
            api_base_url: None,
        };
        custom.info.show_model_fingerprint = true;
        mgr.insert_test_entry("enterprise-key", custom);

        assert!(mgr.model_show_model_fingerprint("fp-model"));
        assert!(!mgr.model_show_model_fingerprint("plain-model"));
        // Unknown model id → false (no catalog entry).
        assert!(!mgr.model_show_model_fingerprint("missing-model"));
        // Lookup by the routing SLUG must resolve to the differing catalog KEY —
        // a direct `.get(slug)` would miss this entry and wrongly return false.
        assert!(
            mgr.model_show_model_fingerprint("enterprise-slug"),
            "slug lookup must resolve to the catalog key and read the flag",
        );
        // Lookup by the catalog KEY itself still works (exact-match path).
        assert!(mgr.model_show_model_fingerprint("enterprise-key"));
    }

    /// The active model must be selectable, not the first entry of the
    /// un-allowlisted catalog.
    #[test]
    fn default_model_honors_allowlist_when_no_default_set() {
        let cfg = config_from_toml(
            r#"
            [models]
            allowed_models = ["keep-*"]
            [model.zzz-first]
            model = "zzz-first"
            base_url = "https://byok.example/v1"
            context_window = 256000
            [model.keep-one]
            model = "keep-one"
            base_url = "https://byok.example/v1"
            context_window = 256000
            "#,
        );
        let catalog = resolve_model_catalog(&cfg, None, &Default::default());
        let (_key, entry, _src) = resolve_default_model(&cfg, &catalog, true);
        assert!(
            entry.info.user_selectable,
            "picked non-selectable {}",
            entry.model
        );
    }

    #[test]
    fn validate_selectable_rejects_bad_allowlists() {
        // Excluded explicit default → error names the default.
        let excluded = config_from_toml(
            r#"
            [models]
            default = "kigi-3"
            allowed_models = ["kigi-4*"]
            [model.kigi-3]
            model = "kigi-3"
            base_url = "https://byok.example/v1"
            context_window = 256000
            [model.kigi-4]
            model = "kigi-4"
            base_url = "https://byok.example/v1"
            context_window = 256000
            "#,
        );
        let catalog = resolve_model_catalog(&excluded, None, &Default::default());
        assert!(
            validate_selectable(&excluded, &catalog)
                .unwrap_err()
                .contains("kigi-3")
        );

        // Matches nothing → error.
        let zero = config_from_toml(
            r#"
            [models]
            allowed_models = ["nomatch-*"]
            [model.kigi-4]
            model = "kigi-4"
            base_url = "https://byok.example/v1"
            context_window = 256000
            "#,
        );
        let catalog = resolve_model_catalog(&zero, None, &Default::default());
        assert!(validate_selectable(&zero, &catalog).is_err());
    }

    #[tokio::test]
    async fn refresh_if_new_etag_skips_when_same() {
        let mgr = test_manager();
        // Set initial etag
        *mgr.inner.etag.write() = Some("\"abc123\"".to_string());

        // Same etag — should be a no-op (etag stays the same)
        mgr.refresh_if_new_etag("\"abc123\"".to_string()).await;
        assert_eq!(
            mgr.inner.etag.read().as_deref(),
            Some("\"abc123\""),
            "etag should remain unchanged when same"
        );
    }

    #[tokio::test]
    async fn set_current_model_id_change_fires_watch_to_all_subscribers() {
        // Two subscribers (simulating two SessionActors sharing one
        // ModelsManager catalog) both observe the change. Fast-path
        // "same id" must NOT bump the generation.
        let mgr = test_manager();
        let mut rx_a = mgr.subscribe_model_switch();
        let mut rx_b = mgr.subscribe_model_switch();
        let initial_a = *rx_a.borrow_and_update();
        let initial_b = *rx_b.borrow_and_update();
        assert_eq!(initial_a, initial_b);

        // Same id is the fast path — no bump.
        mgr.set_current_model_id(acp::ModelId::new("default"));
        // Force-yield so any spurious wakeup would have a chance to
        // surface. `try_recv` on a watch channel: use a timeout-zero
        // race; if `.changed()` resolves within 25ms we have a bug.
        let same_id_ticked =
            tokio::time::timeout(std::time::Duration::from_millis(25), rx_a.changed())
                .await
                .is_ok();
        assert!(
            !same_id_ticked,
            "set_current_model_id(same id) must NOT bump the watch generation",
        );

        // Real switch: both subscribers see the change.
        mgr.set_current_model_id(acp::ModelId::new("kigi-4"));
        tokio::time::timeout(std::time::Duration::from_millis(100), rx_a.changed())
            .await
            .expect("rx_a saw the switch")
            .expect("watch channel still open");
        tokio::time::timeout(std::time::Duration::from_millis(100), rx_b.changed())
            .await
            .expect("rx_b saw the switch")
            .expect("watch channel still open");
        assert_ne!(*rx_a.borrow(), initial_a);
        assert_eq!(*rx_a.borrow(), *rx_b.borrow());
        assert!(mgr.model_switch_generation() > initial_a);
    }

    #[tokio::test]
    async fn model_switch_generation_snapshot_reflects_current_state() {
        let mgr = test_manager();
        let start = mgr.model_switch_generation();
        mgr.set_current_model_id(acp::ModelId::new("kigi-4"));
        assert_eq!(mgr.model_switch_generation(), start + 1);
        // Idempotent: same id → no bump.
        mgr.set_current_model_id(acp::ModelId::new("kigi-4"));
        assert_eq!(mgr.model_switch_generation(), start + 1);
        // Another real change: another bump.
        mgr.set_current_model_id(acp::ModelId::new("kigi-3"));
        assert_eq!(mgr.model_switch_generation(), start + 2);
    }

    #[test]
    fn rebuild_updates_models_and_available() {
        let mgr = test_manager();
        assert!(mgr.models().is_empty());
        assert!(mgr.available().is_empty());

        let cfg = config::Config::default();
        let mut prefetched = IndexMap::new();
        prefetched.insert(
            "test-model".to_string(),
            ModelEntry {
                info: config::ModelInfo::fallback("test-model"),
                api_key: None,
                env_key: None,
                api_base_url: None,
            },
        );

        mgr.rebuild(&cfg, Some(prefetched));

        assert!(
            !mgr.models().is_empty(),
            "models should be populated after rebuild"
        );
    }

    #[test]
    fn current_reasoning_effort_round_trip() {
        let mgr = test_manager();
        assert_eq!(mgr.current_reasoning_effort(), None);

        mgr.set_current_reasoning_effort(Some(ReasoningEffort::High));
        assert_eq!(mgr.current_reasoning_effort(), Some(ReasoningEffort::High));

        mgr.set_current_reasoning_effort(None);
        assert_eq!(mgr.current_reasoning_effort(), None);
    }

    #[test]
    fn current_reasoning_effort_seeded_from_config() {
        let tmp = std::env::temp_dir().join("kigi-test-models-manager-seed");
        let auth_manager = Arc::new(AuthManager::new(&tmp, KimiCodeConfig::default()));
        let mut cfg = config::Config::default();
        cfg.models.default_reasoning_effort = Some(ReasoningEffort::Xhigh);
        let mgr = ModelsManager::new(
            None,
            IndexMap::new(),
            acp::ModelId::new("default"),
            auth_manager,
            cfg,
        );
        assert_eq!(mgr.current_reasoning_effort(), Some(ReasoningEffort::Xhigh),);
    }

    #[test]
    fn default_reasoning_effort_only_stamps_supporting_model() {
        use indexmap::IndexMap;

        // Model that supports reasoning effort — effort should be applied.
        let mut cfg = config::Config::default();
        cfg.models.default = Some("reasoning-model".to_string());
        cfg.models.default_reasoning_effort = Some(ReasoningEffort::High);

        let mut prefetched = IndexMap::new();
        let mut reasoning_entry = ModelEntry {
            info: config::ModelInfo::fallback("reasoning-model"),
            api_key: None,
            env_key: None,
            api_base_url: None,
        };
        reasoning_entry.info.supports_reasoning_effort = true;
        prefetched.insert("reasoning-model".to_string(), reasoning_entry);

        let catalog = resolve_model_catalog(&cfg, Some(prefetched), &Default::default());
        assert_eq!(
            catalog["reasoning-model"].info.reasoning_effort,
            Some(ReasoningEffort::High),
            "reasoning-supporting default model should be stamped",
        );

        // Model that does NOT support reasoning effort — effort must NOT be applied.
        let mut cfg = config::Config::default();
        cfg.models.default = Some("plain-model".to_string());
        cfg.models.default_reasoning_effort = Some(ReasoningEffort::High);

        let mut prefetched = IndexMap::new();
        let plain_entry = ModelEntry {
            info: config::ModelInfo::fallback("plain-model"),
            api_key: None,
            env_key: None,
            api_base_url: None,
        };
        prefetched.insert("plain-model".to_string(), plain_entry);

        let catalog = resolve_model_catalog(&cfg, Some(prefetched), &Default::default());
        assert_eq!(
            catalog["plain-model"].info.reasoning_effort, None,
            "non-reasoning default model must NOT be stamped with persisted effort",
        );
    }

    #[test]
    fn reasoning_effort_override_skips_models_that_do_not_offer_level() {
        use indexmap::IndexMap;
        use kigi_sampling_types::ReasoningEffortOption;

        let cfg = config::Config {
            reasoning_effort_override: Some(ReasoningEffort::None),
            ..Default::default()
        };

        let mut prefetched = IndexMap::new();
        // 4.5-style: supports effort, menu is high only (no none).
        let mut no_none = ModelEntry {
            info: config::ModelInfo::fallback("kigi-4.5"),
            api_key: None,
            env_key: None,
            api_base_url: None,
        };
        no_none.info.supports_reasoning_effort = true;
        no_none.info.reasoning_efforts = vec![ReasoningEffortOption {
            id: "high".into(),
            value: ReasoningEffort::High,
            label: "High".into(),
            description: None,
            default: true,
        }];
        no_none.info.reasoning_effort = Some(ReasoningEffort::High);
        prefetched.insert("kigi-4.5".to_string(), no_none);

        // Model that explicitly offers none.
        let mut with_none = ModelEntry {
            info: config::ModelInfo::fallback("legacy-none"),
            api_key: None,
            env_key: None,
            api_base_url: None,
        };
        with_none.info.supports_reasoning_effort = true;
        with_none.info.reasoning_efforts = vec![ReasoningEffortOption {
            id: "none".into(),
            value: ReasoningEffort::None,
            label: "None".into(),
            description: None,
            default: true,
        }];
        prefetched.insert("legacy-none".to_string(), with_none);

        let catalog = resolve_model_catalog(&cfg, Some(prefetched), &Default::default());
        assert_eq!(
            catalog["kigi-4.5"].info.reasoning_effort,
            Some(ReasoningEffort::High),
            "--effort none must not stamp onto models that do not offer none"
        );
        assert_eq!(
            catalog["legacy-none"].info.reasoning_effort,
            Some(ReasoningEffort::None),
            "models that list none should still accept the override"
        );
    }

    #[test]
    fn config_menu_only_model_derives_support_and_default() {
        // The config-TOML path: a model configured with ONLY `reasoning_efforts`
        // (no `supports_reasoning_effort`, no scalar `reasoning_effort`) must read
        // as supported with the marked-default option's value on the internal
        // gates that BugBot flagged (support gate + wire default).
        let mut cfg = config::Config::default();
        cfg.config_models.insert(
            "menu-only".to_string(),
            config::ConfigModelOverride {
                reasoning_efforts: vec![
                    ReasoningEffortOption {
                        id: "balanced".to_string(),
                        value: ReasoningEffort::Medium,
                        label: "Balanced".to_string(),
                        description: None,
                        default: false,
                    },
                    ReasoningEffortOption {
                        id: "deep".to_string(),
                        value: ReasoningEffort::Xhigh,
                        label: "Deep".to_string(),
                        description: None,
                        default: true,
                    },
                ],
                ..Default::default()
            },
        );
        // A sibling with no menu must stay underived (empty-list path unchanged).
        cfg.config_models
            .insert("plain".to_string(), config::ConfigModelOverride::default());

        let catalog = resolve_model_catalog(&cfg, None, &Default::default());
        let info = &catalog["menu-only"].info;
        assert!(
            info.supports_reasoning_effort,
            "menu-only model must derive support"
        );
        assert_eq!(
            info.reasoning_effort,
            Some(ReasoningEffort::Xhigh),
            "derived default = marked-default option value"
        );
        assert!(!catalog["plain"].info.supports_reasoning_effort);
        assert_eq!(catalog["plain"].info.reasoning_effort, None);

        // The internal getters read those derived fields.
        let tmp = std::env::temp_dir().join("kigi-test-models-manager-menu-only");
        let auth_manager = Arc::new(AuthManager::new(&tmp, KimiCodeConfig::default()));
        let mgr = ModelsManager::new(
            None,
            catalog,
            acp::ModelId::new("menu-only"),
            auth_manager,
            cfg,
        );
        assert!(mgr.model_supports_reasoning_effort("menu-only"));
        assert_eq!(
            mgr.model_default_reasoning_effort("menu-only"),
            Some(ReasoningEffort::Xhigh)
        );
        assert_eq!(mgr.model_reasoning_efforts("menu-only").len(), 2);
        assert!(!mgr.model_supports_reasoning_effort("plain"));
        assert_eq!(mgr.model_default_reasoning_effort("plain"), None);
    }

    #[test]
    fn cli_reasoning_effort_override_only_stamps_supporting_models() {
        use indexmap::IndexMap;

        let cfg = config::Config {
            reasoning_effort_override: Some(ReasoningEffort::High),
            ..config::Config::default()
        };

        let mut prefetched = IndexMap::new();
        let mut reasoning_entry = ModelEntry {
            info: config::ModelInfo::fallback("reasoning-model"),
            api_key: None,
            env_key: None,
            api_base_url: None,
        };
        reasoning_entry.info.supports_reasoning_effort = true;
        prefetched.insert("reasoning-model".to_string(), reasoning_entry);

        let plain_entry = ModelEntry {
            info: config::ModelInfo::fallback("plain-model"),
            api_key: None,
            env_key: None,
            api_base_url: None,
        };
        prefetched.insert("plain-model".to_string(), plain_entry);

        let catalog = resolve_model_catalog(&cfg, Some(prefetched), &Default::default());
        assert_eq!(
            catalog["reasoning-model"].info.reasoning_effort,
            Some(ReasoningEffort::High),
            "reasoning-supporting model should be stamped",
        );
        assert_eq!(
            catalog["plain-model"].info.reasoning_effort, None,
            "non-reasoning model must NOT be stamped",
        );
    }

    #[test]
    fn apply_refresh_result_only_updates_etag_on_success() {
        let mgr = test_manager();
        let cfg = config::Config::default();
        *mgr.inner.etag.write() = Some("\"old\"".to_string());

        assert!(
            !mgr.apply_refresh_result(&cfg, None, Some("\"new\"".to_string())),
            "failed refresh should report no update"
        );
        assert_eq!(
            mgr.inner.etag.read().as_deref(),
            Some("\"old\""),
            "etag should remain unchanged when refresh fails"
        );
        assert!(
            mgr.prefetched().is_none(),
            "prefetched models should stay unchanged"
        );
    }

    fn make_model_entry(model_id: &str) -> ModelEntry {
        ModelEntry {
            info: config::ModelInfo::fallback(model_id),
            api_key: None,
            env_key: None,
            api_base_url: None,
        }
    }

    fn make_prefetched(ids: &[&str]) -> IndexMap<String, ModelEntry> {
        ids.iter()
            .map(|id| (id.to_string(), make_model_entry(id)))
            .collect()
    }

    // ── auth-change refresh: has_fetched_real_catalog flag ─────────────

    #[test]
    fn first_apply_refresh_reselects_default_model() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("kigi-3".to_string());

        assert!(!mgr.has_fetched_real_catalog());

        let prefetched = make_prefetched(&["kigi-3", "kigi-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);

        assert!(mgr.has_fetched_real_catalog());
        assert_eq!(mgr.current_model_id().0.as_ref(), "kigi-3");
    }

    #[test]
    fn subsequent_apply_refresh_preserves_user_model() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("kigi-3".to_string());

        let prefetched = make_prefetched(&["kigi-3", "kigi-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);
        mgr.set_current_model_id(acp::ModelId::new("kigi-4"));

        // Simulate on_auth_changed clearing prefetched + etag.
        *mgr.inner.prefetched.write() = None;
        *mgr.inner.etag.write() = None;

        let prefetched = make_prefetched(&["kigi-3", "kigi-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);

        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "kigi-4",
            "user's model selection must survive auth-change refresh"
        );
    }

    #[test]
    fn subsequent_refresh_reselects_when_model_removed() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("kigi-3".to_string());

        let prefetched = make_prefetched(&["kigi-3", "kigi-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);
        mgr.set_current_model_id(acp::ModelId::new("kigi-4"));

        // Second refresh with kigi-4 removed.
        let prefetched = make_prefetched(&["kigi-3", "kigi-4.5"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);

        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "kigi-3",
            "should fall back to config default when current is removed"
        );
    }

    #[test]
    fn failed_refresh_does_not_set_has_fetched_real_catalog() {
        let mgr = test_manager();
        let cfg = config::Config::default();

        mgr.apply_refresh_result(&cfg, None, None);

        assert!(
            !mgr.has_fetched_real_catalog(),
            "failed refresh must not flip has_fetched_real_catalog"
        );
    }

    // ── apply_config: honor changed preferred model from config ────────

    #[test]
    fn apply_config_honors_new_preferred_model() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("kigi-3".to_string());

        let prefetched = make_prefetched(&["kigi-3", "kigi-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);
        mgr.set_current_model_id(acp::ModelId::new("kigi-4"));

        // Simulate stale inner cfg (no default) from a racing auth refresh.
        let mut stale_cfg = config::Config::default();
        stale_cfg.models.default = None;
        *mgr.inner.cfg.write() = stale_cfg;

        let mut new_cfg = config::Config::default();
        new_cfg.models.default = Some("kigi-3".to_string());
        mgr.apply_config(new_cfg);

        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "kigi-3",
            "apply_config must honor updated preferred model from config"
        );
    }

    #[test]
    fn apply_config_preserves_current_when_preferred_unchanged() {
        let mgr = test_manager();
        let cfg = config::Config::default();

        let prefetched = make_prefetched(&["kigi-3", "kigi-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);

        mgr.set_current_model_id(acp::ModelId::new("kigi-4"));

        // Unrelated config change — preferred model unchanged.
        let new_cfg = config::Config::default();
        mgr.apply_config(new_cfg);

        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "kigi-4",
            "apply_config must not reset model when preferred hasn't changed"
        );
    }

    #[test]
    fn apply_config_falls_back_when_preferred_not_in_catalog() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("kigi-3".to_string());

        let prefetched = make_prefetched(&["kigi-3", "kigi-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);

        mgr.set_current_model_id(acp::ModelId::new("kigi-4"));

        // Preferred model not in catalog — falls back to first entry.
        let mut new_cfg = config::Config::default();
        new_cfg.models.default = Some("kigi-nonexistent".to_string());
        mgr.apply_config(new_cfg);

        let current = mgr.current_model_id();
        let first_available = mgr.available().keys().next().unwrap().clone();
        assert_eq!(
            current.0.as_ref(),
            first_available.0.as_ref(),
            "should fall back to first visible model when preferred not in catalog"
        );
    }

    #[test]
    fn apply_config_both_none_preferred_preserves_current() {
        let mgr = test_manager();
        let cfg = config::Config::default();
        let prefetched = make_prefetched(&["kigi-3", "kigi-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);
        mgr.set_current_model_id(acp::ModelId::new("kigi-4"));
        let new_cfg = config::Config::default();
        mgr.apply_config(new_cfg);

        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "kigi-4",
            "both-None preferred must preserve user's runtime model"
        );
    }

    #[test]
    fn apply_config_old_some_new_none_preserves_current() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("kigi-3".to_string());

        let prefetched = make_prefetched(&["kigi-3", "kigi-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);
        assert_eq!(mgr.current_model_id().0.as_ref(), "kigi-3");

        mgr.set_current_model_id(acp::ModelId::new("kigi-4"));

        // [models] default removed — is_some() guard prevents reset.
        let new_cfg = config::Config::default();
        mgr.apply_config(new_cfg);

        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "kigi-4",
            "old=Some new=None must not reset model (is_some guard)"
        );
    }

    // ── end-to-end: auth refresh + config reload compose correctly ───

    #[test]
    fn auth_refresh_then_config_reload_preserves_user_model() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("kigi-3".to_string());

        // Initial fetch.
        let prefetched = make_prefetched(&["kigi-3", "kigi-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);

        // User runs /model kigi-4.
        mgr.set_current_model_id(acp::ModelId::new("kigi-4"));

        // Auth refresh races — clears prefetched/etag.
        *mgr.inner.prefetched.write() = None;
        *mgr.inner.etag.write() = None;

        // Second fetch must preserve user's model.
        let prefetched = make_prefetched(&["kigi-3", "kigi-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);
        assert_eq!(mgr.current_model_id().0.as_ref(), "kigi-4");

        // Config reload with persisted preference.
        let mut new_cfg = config::Config::default();
        new_cfg.models.default = Some("kigi-4".to_string());
        mgr.apply_config(new_cfg);
        assert_eq!(mgr.current_model_id().0.as_ref(), "kigi-4");
    }

    // ── disk-cache hot-reload (external models_cache.json writes) ────

    fn test_cache_manager(dir: &std::path::Path) -> ModelsCacheManager {
        ModelsCacheManager {
            path: dir.join(MODELS_CACHE_FILE),
            ttl: CACHE_TTL,
        }
    }

    /// An external process persisting a fresh catalog must be picked up:
    /// catalog swapped, etag adopted, real-catalog flag set.
    #[test]
    fn reload_from_disk_cache_applies_external_catalog() {
        let mgr = test_manager();
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = test_cache_manager(tmp.path());

        let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
        cache.persist(
            &make_prefetched(&["kigi-4.5", "kigi-4.3"]),
            Some("etag-ext"),
            auth_method,
            &mgr.cache_origin(),
        );

        mgr.reload_from_cache_manager(&cache);

        assert!(mgr.has_fetched_real_catalog());
        assert!(mgr.models().contains_key("kigi-4.5"));
        assert!(mgr.models().contains_key("kigi-4.3"));
        assert_eq!(mgr.inner.etag.read().as_deref(), Some("etag-ext"));
    }

    /// A latched "allowlist excludes everything" prompt block must clear when
    /// an external cache write delivers a catalog the allowlist matches —
    /// `reload_from_cache_manager` recomputes `allowlist_excludes_all` after
    /// the rebuild, like `apply_refresh_result` does.
    #[test]
    fn reload_from_disk_cache_recomputes_allowlist_excludes_all() {
        let mgr = test_manager();
        let cfg = config_from_toml("[models]\nallowed_models = [\"keep-*\"]");

        // Latch the flag: neither the fetched model nor the bundled defaults
        // merged by `resolve_model_catalog` match `keep-*`.
        mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["other-1"])), None);
        assert!(
            mgr.allowlist_excludes_all(),
            "setup: allowlist should exclude the entire catalog"
        );
        // `apply_refresh_result` borrows the config without storing it, while
        // `reload_from_cache_manager` reads `inner.cfg` — install it there.
        *mgr.inner.cfg.write() = cfg.clone();

        // External process persists a catalog containing an allowed model.
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = test_cache_manager(tmp.path());
        let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
        cache.persist(
            &make_prefetched(&["keep-1"]),
            Some("etag-keep"),
            auth_method,
            &mgr.cache_origin(),
        );

        mgr.reload_from_cache_manager(&cache);

        assert!(mgr.models().contains_key("keep-1"));
        assert!(
            !mgr.allowlist_excludes_all(),
            "corrective external cache write must unlatch the prompt block"
        );
    }

    /// When the *first* real catalog arrives via an external cache write (the
    /// leader never completed its own fetch), the configured `[models]`
    /// default must be resolved — mirroring `apply_refresh_result`'s
    /// first-catalog branch — instead of staying on the bundled placeholder.
    #[test]
    fn reload_from_disk_cache_resolves_default_on_first_catalog() {
        let mgr = test_manager();
        assert!(!mgr.has_fetched_real_catalog());
        let cfg = config_from_toml("[models]\ndefault = \"keep-1\"");
        // `reload_from_cache_manager` reads the manager's stored config.
        *mgr.inner.cfg.write() = cfg.clone();

        let tmp = tempfile::TempDir::new().unwrap();
        let cache = test_cache_manager(tmp.path());
        let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
        cache.persist(
            &make_prefetched(&["keep-1", "other-1"]),
            Some("etag-first"),
            auth_method,
            &mgr.cache_origin(),
        );

        mgr.reload_from_cache_manager(&cache);

        assert!(mgr.has_fetched_real_catalog());
        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "keep-1",
            "first real catalog must resolve the configured default"
        );
    }

    /// A cache write whose catalog matches the in-memory prefetched map (the
    /// leader's own `persist`/`renew_ttl` self-writes, or a same-content fetch
    /// by another process) must be a no-op apart from adopting the etag — no
    /// rebuild, no model reselection.
    #[test]
    fn reload_from_disk_cache_skips_identical_catalog_and_adopts_etag() {
        let mgr = test_manager();
        let cfg = config::Config::default();
        let prefetched = make_prefetched(&["kigi-3", "kigi-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched.clone()), Some("etag-a".into()));
        mgr.set_current_model_id(acp::ModelId::new("kigi-4"));

        let tmp = tempfile::TempDir::new().unwrap();
        let cache = test_cache_manager(tmp.path());
        let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
        cache.persist(
            &prefetched,
            Some("etag-b"),
            auth_method,
            &mgr.cache_origin(),
        );

        mgr.reload_from_cache_manager(&cache);

        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "kigi-4",
            "identical catalog must not disturb the user's model"
        );
        assert_eq!(
            mgr.inner.etag.read().as_deref(),
            Some("etag-b"),
            "etag should be adopted so refresh_if_new_etag stays accurate"
        );
    }

    /// A cache file older than the TTL is rejected by `load_fresh` — the
    /// watcher event arrives within the debounce window of the write, so a
    /// stale file means the write was not a fresh fetch.
    #[test]
    fn reload_from_disk_cache_ignores_stale_cache() {
        let mgr = test_manager();
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = test_cache_manager(tmp.path());
        let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
        let stale = ModelsCache {
            fetched_at: Utc::now() - ChronoDuration::seconds(3600),
            kigi_version: Some(kigi_version::VERSION.to_string()),
            auth_method: Some(auth_method),
            origin: Some(mgr.cache_origin()),
            etag: Some("etag-stale".into()),
            models: make_prefetched(&["kigi-stale"]),
        };
        cache.atomic_write(&stale);

        mgr.reload_from_cache_manager(&cache);

        assert!(!mgr.models().contains_key("kigi-stale"));
        assert!(mgr.inner.etag.read().is_none());
    }

    /// A cache persisted by a process running with different credentials
    /// (e.g. an API-key `--no-leader` run next to a session-auth leader)
    /// must not poison this manager's catalog.
    #[test]
    fn reload_from_disk_cache_ignores_auth_method_mismatch() {
        let mgr = test_manager();
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = test_cache_manager(tmp.path());
        let current = mgr.inner.fetch_auth.read().cache_auth_method();
        let other = if current == CacheAuthMethod::Platforms {
            CacheAuthMethod::ApiKey
        } else {
            CacheAuthMethod::Platforms
        };
        cache.persist(
            &make_prefetched(&["kigi-other-auth"]),
            Some("etag-x"),
            other,
            &mgr.cache_origin(),
        );

        mgr.reload_from_cache_manager(&cache);

        assert!(!mgr.models().contains_key("kigi-other-auth"));
    }

    /// A cache persisted by a process pointed at a *different backend* (env
    /// override, another deployment, a test's mock server) must not poison
    /// this manager's catalog: cached entries embed absolute `base_url`s from
    /// their origin, so adopting them silently re-points inference. This is
    /// the windows-x86_64 lifecycle e2e failure mode — the shared-profile
    /// cache from test 1's mock sent test 2's prompts to a dead port.
    #[test]
    fn reload_from_disk_cache_ignores_origin_mismatch() {
        let mgr = test_manager();
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = test_cache_manager(tmp.path());
        let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
        cache.persist(
            &make_prefetched(&["kigi-other-origin"]),
            Some("etag-y"),
            auth_method,
            "http://127.0.0.1:49953/v1/models",
        );

        mgr.reload_from_cache_manager(&cache);

        assert!(!mgr.models().contains_key("kigi-other-origin"));
        assert!(mgr.inner.etag.read().is_none());
    }

    /// A legacy cache file written before the `origin` field existed must be
    /// treated as a miss (`None` origin never matches) — its entries could
    /// have come from anywhere.
    #[test]
    fn reload_from_disk_cache_ignores_legacy_cache_without_origin() {
        let mgr = test_manager();
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = test_cache_manager(tmp.path());
        let auth_method = mgr.inner.fetch_auth.read().cache_auth_method();
        let legacy = ModelsCache {
            fetched_at: Utc::now(),
            kigi_version: Some(kigi_version::VERSION.to_string()),
            auth_method: Some(auth_method),
            origin: None,
            etag: Some("etag-legacy".into()),
            models: make_prefetched(&["kigi-legacy"]),
        };
        cache.atomic_write(&legacy);

        mgr.reload_from_cache_manager(&cache);

        assert!(!mgr.models().contains_key("kigi-legacy"));
    }

    // ── clear() resets has_fetched_real_catalog ──────────────────────

    #[test]
    fn clear_resets_has_fetched_real_catalog() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("kigi-3".to_string());

        let prefetched = make_prefetched(&["kigi-3", "kigi-4"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);
        assert!(mgr.has_fetched_real_catalog());

        mgr.clear();
        assert!(!mgr.has_fetched_real_catalog());

        // New identity fetch — resolves default via reselect_default_model.
        let prefetched = make_prefetched(&["kigi-4.5", "kigi-4.3"]);
        mgr.apply_refresh_result(&cfg, Some(prefetched), None);
        let first_available = mgr.available().keys().next().unwrap().clone();
        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            first_available.0.as_ref()
        );
    }

    /// A flip is "campaign-only" iff the preferred changed and either side is an
    /// active campaign default.
    #[test]
    fn is_campaign_only_flip_detects_campaign_driven_changes() {
        let camp: std::collections::HashSet<String> = ["beta".into()].into_iter().collect();
        // New side is the campaign default (campaign appearing) → campaign-only.
        assert!(is_campaign_only_flip(
            &Some("alpha".into()),
            &Some("beta".into()),
            &camp
        ));
        // Old side was the campaign default (campaign withdrawing) → campaign-only.
        assert!(is_campaign_only_flip(
            &Some("beta".into()),
            &Some("alpha".into()),
            &camp
        ));
        // Neither side a campaign default → ordinary user/CLI/env flip.
        assert!(!is_campaign_only_flip(
            &Some("alpha".into()),
            &Some("gamma".into()),
            &camp
        ));
        // No change, cleared default, or empty campaign set → never campaign-only.
        assert!(!is_campaign_only_flip(
            &Some("beta".into()),
            &Some("beta".into()),
            &camp
        ));
        assert!(!is_campaign_only_flip(&Some("beta".into()), &None, &camp));
        assert!(!is_campaign_only_flip(
            &Some("alpha".into()),
            &Some("beta".into()),
            &std::collections::HashSet::new()
        ));
    }

    /// A campaign-only flip must NOT reselect a live session whose current model
    /// is still selectable; a non-campaign flip must. "Campaign-driven" is marked
    /// by `default_is_campaign_driven` on the incoming config.
    #[test]
    fn campaign_only_flip_does_not_reselect_live_session() {
        let mgr = test_manager();
        let mut cfg = config::Config::default();
        cfg.models.default = Some("alpha".to_string());
        mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["alpha", "beta"])), None);
        *mgr.inner.cfg.write() = cfg.clone(); // old_preferred = "alpha"
        assert_eq!(mgr.current_model_id().0.as_ref(), "alpha");

        let mut new_cfg = config::Config::default();
        new_cfg.models.default = Some("beta".to_string());
        new_cfg.models.default_is_campaign_driven = true; // campaign overriding
        mgr.apply_config(new_cfg);
        assert_eq!(
            mgr.current_model_id().0.as_ref(),
            "alpha",
            "campaign-only flip must not yank a still-selectable live session"
        );

        // Control: same flip with no campaign (no pre_campaign_default) → reselect.
        let mgr2 = test_manager();
        let mut cfg2 = config::Config::default();
        cfg2.models.default = Some("alpha".to_string());
        mgr2.apply_refresh_result(&cfg2, Some(make_prefetched(&["alpha", "beta"])), None);
        *mgr2.inner.cfg.write() = cfg2.clone();
        let mut new_cfg2 = config::Config::default();
        new_cfg2.models.default = Some("beta".to_string());
        mgr2.apply_config(new_cfg2);
        assert_eq!(
            mgr2.current_model_id().0.as_ref(),
            "beta",
            "a non-campaign preferred change must reselect"
        );
    }

    /// A campaign default missing from the catalog falls back to
    /// `pre_campaign_default`, then to the first visible model — and only when
    /// the missing pref is actually the campaign-driven config value.
    #[test]
    fn unavailable_campaign_default_falls_back_to_config_default() {
        let catalog = make_prefetched(&["real-model", "other-model"]);

        let mut cfg = config::Config::default();
        cfg.models.default = Some("missing-model".to_string());
        cfg.models.default_is_campaign_driven = true;
        cfg.models.pre_campaign_default = Some("real-model".to_string());
        let (key, _, _) = resolve_default_model(&cfg, &catalog, true);
        assert_eq!(
            key, "real-model",
            "must fall back to the pre-campaign default"
        );

        // Control: pre-campaign default also absent → first visible model.
        let mut cfg2 = config::Config::default();
        cfg2.models.default = Some("missing-model".to_string());
        cfg2.models.default_is_campaign_driven = true;
        cfg2.models.pre_campaign_default = Some("also-missing".to_string());
        let (key2, _, _) = resolve_default_model(&cfg2, &catalog, true);
        assert_eq!(&key2, catalog.keys().next().unwrap());

        // Control: not campaign-driven (e.g. stale recovery value alongside a
        // user-set default) → the campaign detour must NOT fire; a missing
        // config pref falls to the first visible model.
        let mut cfg3 = config::Config::default();
        cfg3.models.default = Some("missing-model".to_string());
        cfg3.models.pre_campaign_default = Some("real-model".to_string());
        let (key3, _, _) = resolve_default_model(&cfg3, &catalog, true);
        assert_eq!(
            &key3,
            catalog.keys().next().unwrap(),
            "non-campaign catalog miss must not recover via campaign state"
        );

        // Control: CLI override misses the catalog while campaign state is set
        // → CLI is not a campaign problem; no campaign detour.
        let mut cfg4 = config::Config {
            default_model_override: Some("missing-cli-model".to_string()),
            ..Default::default()
        };
        cfg4.models.default = Some("campaign-model".to_string());
        cfg4.models.default_is_campaign_driven = true;
        cfg4.models.pre_campaign_default = Some("real-model".to_string());
        let (key4, _, _) = resolve_default_model(&cfg4, &catalog, true);
        assert_eq!(
            &key4,
            catalog.keys().next().unwrap(),
            "a CLI pref miss must not detour through pre_campaign_default"
        );
    }

    // ── ModelFetchAuth::resolve + PlatformApiKeys tests ─────────────

    use kigi_test_support::EnvGuard;
    use serial_test::serial;

    fn keys(cn: Option<&str>, ai: Option<&str>) -> PlatformApiKeys {
        PlatformApiKeys::test_keys(cn, ai)
    }

    #[test]
    fn resolve_custom_endpoint_wins_over_platforms() {
        let endpoints = config::EndpointsConfig {
            models_base_url: Some("https://custom.example.com".to_owned()),
            ..config::EndpointsConfig::default()
        };
        assert_eq!(
            ModelFetchAuth::resolve(&endpoints),
            ModelFetchAuth::CustomEndpoint,
        );
        assert_eq!(
            ModelFetchAuth::resolve(&config::EndpointsConfig::default()),
            ModelFetchAuth::Platforms,
        );
    }

    /// Platform API keys resolve env > config, platform-scoped > generic, and
    /// values never come from unknown `[platforms.*]` tables.
    #[test]
    #[serial]
    fn platform_api_keys_env_beats_config_and_generic_fallback_applies() {
        let mut platforms = config::PlatformsConfig::default();
        platforms.entries.insert(
            "moonshot-cn".into(),
            config::PlatformCredentialConfig {
                api_key: Some("cfg-cn".into()),
            },
        );
        platforms.entries.insert(
            "moonshot-ai".into(),
            config::PlatformCredentialConfig {
                api_key: Some("cfg-ai".into()),
            },
        );

        // Injected env: scoped name set for cn only, generic set for both.
        let getenv = |name: &str| match name {
            "KIGI_MOONSHOT_CN_API_KEY" => Some("env-cn".to_string()),
            "KIGI_MOONSHOT_API_KEY" => Some("env-generic".to_string()),
            _ => None,
        };
        assert_eq!(
            config::resolve_platform_api_key_with(
                kigi_models::PlatformId::MoonshotCn,
                &platforms,
                getenv,
                |_| None,
            )
            .as_deref(),
            Some("env-cn"),
            "platform-scoped env must win over generic env and config",
        );
        assert_eq!(
            config::resolve_platform_api_key_with(
                kigi_models::PlatformId::MoonshotAi,
                &platforms,
                getenv,
                |_| None,
            )
            .as_deref(),
            Some("env-generic"),
            "generic env must win over config when scoped env is unset",
        );
        // No env at all → config file key.
        assert_eq!(
            config::resolve_platform_api_key_with(
                kigi_models::PlatformId::MoonshotAi,
                &platforms,
                |_| None,
                |_| None,
            )
            .as_deref(),
            Some("cfg-ai"),
        );
        // The OAuth platform never resolves an API key.
        assert_eq!(
            config::resolve_platform_api_key_with(
                kigi_models::PlatformId::KimiCode,
                &platforms,
                getenv,
                |_| None,
            ),
            None,
        );
    }

    /// The `Debug` impl for [`PlatformApiKeys`] must print presence only.
    #[test]
    fn platform_api_keys_debug_never_leaks_values() {
        let dbg = format!("{:?}", keys(Some("sk-super-secret"), None));
        assert!(
            !dbg.contains("sk-super-secret"),
            "debug leaked a key: {dbg}"
        );
        assert!(dbg.contains("true") && dbg.contains("false"));
    }

    // ── remote_fetch gate: resolve_prefetch_env_from_parts ───────────

    /// remote_fetch=false must return `None` against every re-arming shape at
    /// once — session auth, a moonshot platform key, AND a custom models
    /// endpoint (which normally forces the prefetch to run).
    #[test]
    fn prefetch_env_none_when_remote_fetch_disabled_despite_credentials() {
        let endpoints = config::EndpointsConfig {
            models_base_url: Some("https://custom.example.com".to_owned()),
            ..config::EndpointsConfig::default()
        };
        assert!(
            resolve_prefetch_env_from_parts(
                Some(KimiAuth::test_default()),
                endpoints.clone(),
                keys(Some("sk-cn"), None),
                true,
                false,
            )
            .is_none(),
            "session auth must not re-arm the prefetch when remote_fetch is off",
        );
        assert!(
            resolve_prefetch_env_from_parts(
                None,
                endpoints,
                keys(Some("sk-cn"), None),
                false,
                false
            )
            .is_none(),
            "platform key / custom endpoint must not re-arm it either",
        );
    }

    /// Inverse sanity: with remote_fetch enabled a moonshot key alone (the F2
    /// acceptance shape: no subscription login) DOES arm the prefetch, and the
    /// credential-less default still doesn't.
    #[test]
    fn prefetch_env_resolves_when_remote_fetch_enabled() {
        let env = resolve_prefetch_env_from_parts(
            None,
            config::EndpointsConfig::default(),
            keys(None, Some("sk-ai")),
            false,
            true,
        );
        assert!(
            env.is_some(),
            "a moonshot API key alone must arm the startup model sync (PRD F2)",
        );
        assert!(
            resolve_prefetch_env_from_parts(
                None,
                config::EndpointsConfig::default(),
                PlatformApiKeys::default(),
                false,
                true,
            )
            .is_none(),
            "no credentials and no custom endpoint must stay a no-prefetch launch",
        );
    }

    /// A stored subscription-OAuth session ALONE (claude-pro-max login, no
    /// Kimi session, no API key) must arm the startup prefetch — its models
    /// are the user's only models. Regression: the gate ignored stored OAuth
    /// sessions, so such a user booted onto the bundled Kimi table.
    #[test]
    fn prefetch_env_resolves_with_stored_oauth_session_alone() {
        assert!(
            resolve_prefetch_env_from_parts(
                None,
                config::EndpointsConfig::default(),
                PlatformApiKeys::default(),
                true,
                true,
            )
            .is_some(),
            "a stored subscription-OAuth session alone must arm the model sync",
        );
    }

    /// The `on_auth_changed` wipe guard: only the genuinely credential-less
    /// shape wipes. A stored subscription-OAuth session vetoes the wipe —
    /// the regression that left a claude-pro-max-only login with an empty
    /// catalog and an "unknown" model.
    #[test]
    fn wipe_guard_spares_stored_oauth_sessions() {
        use ModelFetchAuth::{CustomEndpoint, Platforms};
        // No credential of any kind → wipe.
        assert!(should_wipe_catalog_on_auth_change(
            false, Platforms, false, false
        ));
        // A stored subscription-OAuth session alone → NO wipe (fetch runs).
        assert!(!should_wipe_catalog_on_auth_change(
            false, Platforms, false, true
        ));
        // Primary session / platform key / custom endpoint each veto too.
        assert!(!should_wipe_catalog_on_auth_change(
            true, Platforms, false, false
        ));
        assert!(!should_wipe_catalog_on_auth_change(
            false, Platforms, true, false
        ));
        assert!(!should_wipe_catalog_on_auth_change(
            false,
            CustomEndpoint,
            false,
            false
        ));
    }

    /// remote_fetch=false: an online catalog refresh is a no-op — nothing is
    /// fetched, no real-catalog flag is set, and the static catalog keeps
    /// resolving. Covers `list_models`/`do_refresh` online strategies too,
    /// which funnel into `fetch_and_apply`/`spawn_fetch`.
    #[tokio::test]
    async fn fetch_and_apply_degrades_offline_when_remote_fetch_disabled() {
        let mgr = test_manager();
        mgr.insert_test_entry(
            "static-one",
            ModelEntry {
                info: config::ModelInfo::fallback("static-one"),
                api_key: None,
                env_key: None,
                api_base_url: None,
            },
        );

        mgr.fetch_and_apply_inner(false).await;

        assert!(
            !mgr.has_fetched_real_catalog(),
            "no catalog fetch may be recorded when remote_fetch is disabled",
        );
        assert!(
            mgr.models().contains_key("static-one"),
            "the static catalog must keep resolving",
        );
    }

    // ── supported_in_api tests ──────────────────────────────────────

    #[test]
    fn default_model_skips_oauth_only_for_api_key_users() {
        let cfg = config::Config::default();
        let mut catalog = IndexMap::new();

        let mut oauth_only = ModelEntry {
            info: config::ModelInfo::fallback("oauth-only"),
            api_key: None,
            env_key: None,
            api_base_url: None,
        };
        oauth_only.info.supported_in_api = false;
        catalog.insert("oauth-only".to_string(), oauth_only);

        let public = ModelEntry {
            info: config::ModelInfo::fallback("public-model"),
            api_key: None,
            env_key: None,
            api_base_url: None,
        };
        catalog.insert("public-model".to_string(), public);

        // API-key user: default should NOT be the oauth-only model
        let (key, _, _) = resolve_default_model(&cfg, &catalog, false);
        assert_ne!(
            key, "oauth-only",
            "API-key default must not be an OAuth-only model"
        );
        assert_eq!(key, "public-model");

        // OAuth user: oauth-only is valid as default (it's first in the map)
        let (key, _, _) = resolve_default_model(&cfg, &catalog, true);
        assert!(
            key == "oauth-only" || key == "public-model",
            "OAuth user should be able to use either model as default"
        );
    }

    #[test]
    fn visible_for_auth_logic() {
        let mut info = config::ModelInfo::fallback("test");

        // Default: visible to everyone
        assert!(info.visible_for_auth(true));
        assert!(info.visible_for_auth(false));

        // hidden = true: invisible to everyone
        info.hidden = true;
        assert!(!info.visible_for_auth(true));
        assert!(!info.visible_for_auth(false));

        // hidden = false, supported_in_api = false: visible to session only
        info.hidden = false;
        info.supported_in_api = false;
        assert!(info.visible_for_auth(true));
        assert!(!info.visible_for_auth(false));
    }

    /// SHIP-BLOCKER regression: a user who signed in with ONLY a Claude Pro/Max
    /// subscription has no PRIMARY (Kimi) session, so `is_session_auth()` is
    /// false. Stamping `supported_in_api = !uses_oauth()` therefore hid every
    /// one of their models — `available()` returned an empty picker and the
    /// whole subscription-OAuth feature was dead for its target user. The
    /// claude-pro-max entry must be visible with no primary session at all.
    #[test]
    fn claude_pro_max_only_user_sees_their_models_in_the_picker() {
        let wire: kigi_models::WireModel =
            serde_json::from_value(serde_json::json!({ "id": "claude-opus-4-8" }))
                .expect("wire model fixture");
        let entry_config = crate::agent::models_fetch::platform_wire_model_to_entry(
            kigi_models::PlatformId::ClaudeProMax,
            wire,
            "https://api.anthropic.com/v1",
        );
        let entry = ModelEntry::from_config_entry(&entry_config);
        let key = entry
            .info
            .id
            .clone()
            .expect("platform entries carry a managed catalog key");
        assert_eq!(key, "claude-pro-max/claude-opus-4-8");
        let mut catalog = IndexMap::new();
        catalog.insert(key.clone(), entry);

        // Empty home ⇒ the primary AuthManager holds no credential at all.
        let home = tempfile::tempdir().expect("tempdir");
        let mgr = ModelsManager::new(
            None,
            catalog,
            acp::ModelId::new(Arc::from(key.clone())),
            Arc::new(AuthManager::new(home.path(), KimiCodeConfig::default())),
            config::Config::default(),
        );
        assert!(
            !mgr.is_session_auth(),
            "a claude-pro-max-only user has no PRIMARY (Kimi) OAuth session"
        );
        assert!(
            mgr.available()
                .contains_key(&acp::ModelId::new(Arc::from(key.clone()))),
            "the claude-pro-max model must reach the picker without a primary session"
        );
    }

    // ── duplicate model slug re-keying (A/B experiment "auto" alias) ──

    fn make_entry_config(model: &str, name: Option<&str>) -> config::ModelEntryConfig {
        make_entry_config_with_id(None, model, name)
    }

    fn make_entry_config_with_id(
        id: Option<&str>,
        model: &str,
        name: Option<&str>,
    ) -> config::ModelEntryConfig {
        config::ModelEntryConfig {
            id: id.map(|s| s.to_owned()),
            model: model.to_owned(),
            base_url: "https://test.api/v1".to_owned(),
            name: name.map(|n| n.to_owned()),
            description: None,
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_key: None,
            env_key: None,
            api_backend: Default::default(),
            context_window: std::num::NonZeroU64::new(200_000).unwrap(),
            auto_compact_threshold_percent: None,
            system_prompt_label: None,
            extra_headers: IndexMap::new(),
            api_base_url: None,
            use_concise: false,
            agent_type: config::default_agent_type(),
            inference_idle_timeout_secs: None,
            max_retries: None,
            hidden: false,
            supported_in_api: true,
            auth_scheme: None,
            reasoning_effort: None,
            supports_reasoning_effort: false,
            reasoning_efforts: Vec::new(),
            capabilities: Vec::new(),
            supports_backend_search: false,
            compactions_remaining: None,
            compaction_at_tokens: None,
            show_model_fingerprint: false,
            stream_tool_calls: None,
            laziness_detector: config::LazinessDetectorPerModelConfig::default(),
        }
    }

    /// Experiment: two entries share the same routing slug but have distinct ids.
    /// Both survive, keyed by their respective ids.
    #[test]
    fn build_prefetched_map_distinct_ids_same_slug() {
        let entries = vec![
            make_entry_config_with_id(Some("auto"), "kigi", Some("Auto")),
            make_entry_config_with_id(Some("kigi"), "kigi", Some("Kigi")),
            make_entry_config_with_id(
                Some("kigi-composer-2.5-fast"),
                "kigi-composer-2.5-fast",
                Some("Kigi Fast"),
            ),
        ];
        let map = build_prefetched_map(entries);

        assert_eq!(map.len(), 3, "all three entries should survive");
        assert!(map.contains_key("auto"));
        assert!(map.contains_key("kigi"));
        assert!(map.contains_key("kigi-composer-2.5-fast"));
        assert_eq!(
            map["auto"].info.model, "kigi",
            "auto entry should still route to kigi"
        );
        assert_eq!(map["kigi"].info.model, "kigi");
    }

    /// No id field — falls back to model slug as key.
    #[test]
    fn build_prefetched_map_no_id_falls_back_to_slug() {
        let entries = vec![
            make_entry_config("model-a", Some("Model A")),
            make_entry_config("model-b", Some("Model B")),
        ];
        let map = build_prefetched_map(entries);

        assert_eq!(map.len(), 2);
        assert!(map.contains_key("model-a"));
        assert!(map.contains_key("model-b"));
    }

    /// Duplicate ids — second overwrites first (same as duplicate slugs before).
    #[test]
    fn build_prefetched_map_duplicate_id_overwrites() {
        let entries = vec![
            make_entry_config_with_id(Some("kigi"), "kigi", Some("First")),
            make_entry_config_with_id(Some("kigi"), "kigi", Some("Second")),
        ];
        let map = build_prefetched_map(entries);

        assert_eq!(map.len(), 1, "duplicate id: second overwrites first");
        assert_eq!(map["kigi"].info.name.as_deref(), Some("Second"));
    }

    /// Regression: resolve_default_model must match by id before scanning
    /// by model slug, otherwise entries sharing a slug resolve to whichever
    /// appears first in the catalog.
    #[test]
    fn resolve_default_model_prefers_id_over_model_slug() {
        let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
        catalog.insert("auto-kigi".to_string(), make_model_entry("kigi"));
        catalog.insert("kigi".to_string(), make_model_entry("kigi"));

        let mut cfg = config::Config::default();
        cfg.models.default = Some("kigi".to_string());

        let (key, _, _) = resolve_default_model(&cfg, &catalog, true);
        assert_eq!(key, "kigi", "must match id, not first slug hit");
    }

    /// No id field — falls back to slug as key.
    #[test]
    fn build_prefetched_map_none_id_falls_back_to_slug() {
        let entries = vec![make_entry_config_with_id(None, "kigi", Some("Kigi"))];
        let map = build_prefetched_map(entries);

        assert_eq!(map.len(), 1);
        assert!(map.contains_key("kigi"));
    }

    // ── persisted model id → catalog key (session resume) ─────────────

    #[test]
    fn resolve_catalog_key_maps_routing_slug_to_config_key() {
        let mut models = IndexMap::new();
        models.insert("enterprise-kigi".to_string(), make_model_entry("kigi-4.5"));
        models.insert("kigi-4.3".to_string(), make_model_entry("kigi-4.3"));

        let persisted = acp::ModelId::new("kigi-4.5");
        let key = resolve_catalog_key(&models, &persisted).expect("slug must resolve");
        assert_eq!(key.0.as_ref(), "enterprise-kigi");
    }

    #[test]
    fn resolve_catalog_key_prefers_exact_key_match() {
        let mut models = IndexMap::new();
        models.insert("kigi-4.5".to_string(), make_model_entry("kigi-4.5"));

        let persisted = acp::ModelId::new("kigi-4.5");
        let key = resolve_catalog_key(&models, &persisted).expect("exact key must resolve");
        assert_eq!(key.0.as_ref(), "kigi-4.5");
    }

    #[test]
    fn resolve_catalog_key_last_slug_match_wins() {
        let mut models = IndexMap::new();
        models.insert("default-kigi".to_string(), make_model_entry("kigi-4.5"));
        models.insert("user-kigi".to_string(), make_model_entry("kigi-4.5"));

        let persisted = acp::ModelId::new("kigi-4.5");
        let key = resolve_catalog_key(&models, &persisted).expect("slug must resolve");
        assert_eq!(key.0.as_ref(), "user-kigi");
    }

    #[test]
    fn selectable_catalog_key_for_persisted_none_when_resolved_not_available() {
        let mut models = IndexMap::new();
        models.insert("enterprise-kigi".to_string(), make_model_entry("kigi-4.5"));

        let available: IndexMap<_, _> = IndexMap::new();
        let persisted = acp::ModelId::new("kigi-4.5");
        assert!(selectable_catalog_key_for_persisted(&models, &available, &persisted).is_none());
    }

    #[test]
    fn selectable_prefers_available_identity_over_non_selectable_exact_key() {
        let mut models = IndexMap::new();
        models.insert("kigi".to_string(), make_model_entry("kigi"));
        models.insert("enterprise-kigi".to_string(), make_model_entry("kigi"));
        models.insert("kigi-4.3".to_string(), make_model_entry("kigi-4.3"));

        let available = test_available_keys(&["enterprise-kigi", "kigi-4.3"]);

        let persisted = acp::ModelId::new("kigi");
        assert_eq!(
            resolve_catalog_key(&models, &persisted)
                .expect("exact key exists")
                .0
                .as_ref(),
            "kigi"
        );
        let key = selectable_catalog_key_for_persisted(&models, &available, &persisted)
            .expect("must resolve to selectable section");
        assert_eq!(key.0.as_ref(), "enterprise-kigi");
    }

    #[test]
    fn selectable_matches_routing_slug_when_no_exact_key() {
        let mut models = IndexMap::new();
        models.insert("enterprise-kigi".to_string(), make_model_entry("kigi"));
        models.insert("kigi-4.3".to_string(), make_model_entry("kigi-4.3"));

        let available = test_available_keys(&["enterprise-kigi", "kigi-4.3"]);

        let persisted = acp::ModelId::new("kigi");
        let key = selectable_catalog_key_for_persisted(&models, &available, &persisted)
            .expect("slug must resolve to selectable key");
        assert_eq!(key.0.as_ref(), "enterprise-kigi");
    }

    /// A persisted *selectable* catalog key binds to itself even when a later
    /// selectable section's routing slug equals that key (exact key wins).
    #[test]
    fn selectable_prefers_exact_key_over_later_slug_match() {
        let mut models = IndexMap::new();
        models.insert("kigi".to_string(), make_model_entry("kigi-4.5"));
        models.insert("other".to_string(), make_model_entry("kigi"));

        let available = test_available_keys(&["kigi", "other"]);

        let persisted = acp::ModelId::new("kigi");
        let key = selectable_catalog_key_for_persisted(&models, &available, &persisted)
            .expect("exact selectable key must win");
        assert_eq!(key.0.as_ref(), "kigi");
    }

    /// The restore-migration inputs: a K3-shaped menu (low/high/max tokens)
    /// offers Max but NOT Xhigh; a legacy empty-menu model offers Xhigh but
    /// NOT Max. `model_switch` relies on exactly this pair to migrate
    /// pre-split `xhigh` overrides onto `max`-vocabulary models.
    #[test]
    fn offers_effort_distinguishes_k3_menu_from_legacy_set() {
        let k3_wire: kigi_models::WireModel = serde_json::from_value(serde_json::json!({
            "id": "k3",
            "context_length": 1_048_576,
            "supports_reasoning": true,
            "supports_thinking_type": "only",
            "think_efforts": {
                "support": true,
                "valid_efforts": ["low", "high", "max"],
                "default_effort": "max"
            }
        }))
        .unwrap();
        let k3_cfg = crate::agent::models_fetch::platform_wire_model_to_entry(
            kigi_models::PlatformId::KimiCode,
            k3_wire,
            "https://api.kimi.com/coding/v1",
        );
        let k3 = config::ModelInfo::from_config(&k3_cfg);
        assert!(model_offers_reasoning_effort(&k3, ReasoningEffort::Max));
        assert!(model_offers_reasoning_effort(&k3, ReasoningEffort::Low));
        assert!(
            !model_offers_reasoning_effort(&k3, ReasoningEffort::Xhigh),
            "K3's menu has no xhigh token — the migration precondition"
        );

        let mut legacy_cfg = k3_cfg.clone();
        legacy_cfg.reasoning_efforts = Vec::new();
        legacy_cfg.supports_reasoning_effort = true;
        let legacy = config::ModelInfo::from_config(&legacy_cfg);
        assert!(model_offers_reasoning_effort(
            &legacy,
            ReasoningEffort::Xhigh
        ));
        assert!(
            !model_offers_reasoning_effort(&legacy, ReasoningEffort::Max),
            "legacy built-in set must not offer max (endpoint may 400)"
        );
    }

    fn test_available_keys(keys: &[&str]) -> IndexMap<acp::ModelId, acp::ModelInfo> {
        keys.iter()
            .map(|k| {
                let id = acp::ModelId::new(*k);
                (id.clone(), acp::ModelInfo::new(id, (*k).to_string()))
            })
            .collect()
    }

    // ── PRD F2/F4 wiremock suite ─────────────────────────────────────

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn f4_listing() -> serde_json::Value {
        serde_json::json!({
            "data": [
                {
                    "id": "kimi-for-coding",
                    "context_length": 262144,
                    "supports_reasoning": true,
                    "supports_image_in": true,
                    "supports_video_in": false,
                    "display_name": "k2.6-code-preview"
                },
                { "id": "kimi-latest", "context_length": 131072 }
            ]
        })
    }

    fn proxied_endpoints(server_uri: &str) -> config::EndpointsConfig {
        config::EndpointsConfig {
            coding_api_base_url: Some(server_uri.to_string()),
            models_base_url: None,
            models_list_url: None,
            ..config::EndpointsConfig::default()
        }
    }

    /// Happy path: `GET {base}/models` with the OAuth bearer, F4 wire shape
    /// → managed `{platform_id}/{model_id}` keys, display_name → name,
    /// context_length → context_window, derived capabilities, etag captured,
    /// and NO credential material on the raw entries (cache safety).
    #[tokio::test]
    async fn wiremock_platforms_fetch_happy_path_maps_f4_contract() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("Authorization", "Bearer oauth-token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"e1\"")
                    .set_body_json(f4_listing()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let endpoints = proxied_endpoints(&server.uri());
        let auth = KimiAuth {
            key: "oauth-token".into(),
            ..KimiAuth::test_default()
        };
        let result = tokio::task::spawn_blocking(move || {
            crate::agent::models_fetch::fetch_models_blocking(
                &endpoints,
                Some(&auth),
                &Default::default(),
                ModelFetchAuth::Platforms,
                &PlatformApiKeys::default(),
            )
        })
        .await
        .unwrap()
        .expect("fetch should succeed");

        assert!(!result.oauth_unauthorized);
        assert_eq!(result.etag.as_deref(), Some("\"e1\""));
        let map = build_prefetched_map(result.models);
        assert_eq!(
            map.keys().collect::<Vec<_>>(),
            vec!["kimi-code/kimi-for-coding", "kimi-code/kimi-latest"],
            "entries must be keyed {{platform_id}}/{{model_id}} in server order"
        );
        let entry = map.get("kimi-code/kimi-for-coding").unwrap();
        assert_eq!(entry.info.model, "kimi-for-coding");
        assert_eq!(entry.info.name.as_deref(), Some("k2.6-code-preview"));
        assert_eq!(entry.info.context_window.get(), 262_144);
        assert_eq!(
            entry.info.capabilities,
            vec![
                kigi_models::ModelCapability::Thinking,
                kigi_models::ModelCapability::ImageIn
            ],
            "capabilities must derive from the wire flags"
        );
        assert!(
            !entry.info.supported_in_api,
            "subscription models are OAuth-only"
        );
        assert!(
            entry.api_key.is_none() && entry.env_key.is_none(),
            "no credential material on OAuth-platform entries"
        );
        // Missing display_name falls back to the id; missing flags default off.
        let second = map.get("kimi-code/kimi-latest").unwrap();
        assert_eq!(second.info.name.as_deref(), Some("kimi-latest"));
        assert!(second.info.capabilities.is_empty());
        assert_eq!(second.info.context_window.get(), 131_072);
    }

    /// Moonshot open-platform fetch: `kimi-k` prefix filter applied, entries
    /// carry env-var NAMES (never key values), and route at the platform base.
    #[tokio::test]
    #[serial]
    async fn wiremock_moonshot_fetch_filters_prefix_and_stamps_env_key_names() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("Authorization", "Bearer sk-cn-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    { "id": "kimi-k2-turbo-preview", "context_length": 262144 },
                    { "id": "moonshot-v1-8k", "context_length": 8192 }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let _base = EnvGuard::set(kigi_models::MOONSHOT_CN_BASE_URL_ENV, server.uri());

        let endpoints = config::EndpointsConfig::default();
        let keys = PlatformApiKeys::test_keys(Some("sk-cn-secret"), None);
        let result = tokio::task::spawn_blocking(move || {
            crate::agent::models_fetch::fetch_models_blocking(
                &endpoints,
                None,
                &Default::default(),
                ModelFetchAuth::Platforms,
                &keys,
            )
        })
        .await
        .unwrap()
        .expect("moonshot-only fetch should succeed without any OAuth session");

        let map = build_prefetched_map(result.models);
        assert_eq!(
            map.keys().collect::<Vec<_>>(),
            vec!["moonshot-cn/kimi-k2-turbo-preview"],
            "non-kimi-k ids must be filtered out"
        );
        let entry = map.get("moonshot-cn/kimi-k2-turbo-preview").unwrap();
        assert_eq!(entry.info.base_url, server.uri());
        assert!(
            entry.api_key.is_none(),
            "fetched entries must never embed key values (they are persisted to disk)"
        );
        assert_eq!(
            entry.env_key.as_ref().map(|k| k.names()),
            Some(vec!["KIGI_MOONSHOT_CN_API_KEY", "KIGI_MOONSHOT_API_KEY"]),
            "entries carry the env-key NAMES for request-time resolution"
        );
        // kimi-k2 prefix rule.
        assert_eq!(
            entry.info.capabilities,
            vec![
                kigi_models::ModelCapability::Thinking,
                kigi_models::ModelCapability::ImageIn,
                kigi_models::ModelCapability::VideoIn
            ]
        );
        assert!(entry.info.supported_in_api);
    }

    /// Port of kimi-cli `refresh_managed_models`' 401 handling: an OAuth 401
    /// forces one token refresh and one retry with the rotated bearer.
    #[tokio::test]
    #[serial]
    async fn wiremock_oauth_401_forces_refresh_and_retries_once() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        // Hermetic home + no platform keys: this path resolves PlatformApiKeys
        // (env + auth.json). A dev machine's real ~/.kigi/auth.json platform
        // scope or moonshot env var would enable a LIVE moonshot fetch here,
        // short-circuiting the refresh-retry under test.
        let hermetic_home = tempfile::tempdir().unwrap();
        let _home = EnvGuard::set("KIGI_SHARE_DIR", hermetic_home.path());
        let _cn = EnvGuard::unset(kigi_models::MOONSHOT_CN_API_KEY_ENV);
        let _ai = EnvGuard::unset(kigi_models::MOONSHOT_AI_API_KEY_ENV);
        let _gen = EnvGuard::unset(kigi_models::MOONSHOT_API_KEY_ENV);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("Authorization", "Bearer stale-token"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("Authorization", "Bearer fresh-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(f4_listing()))
            .expect(1)
            .mount(&server)
            .await;

        let cache_dir = tempfile::TempDir::new().unwrap();
        let _cache = EnvGuard::set("KIGI_MODELS_CACHE_DIR", cache_dir.path().to_str().unwrap());

        struct SwapRefresher;
        #[async_trait::async_trait]
        impl crate::auth::refresh::TokenRefresher for SwapRefresher {
            async fn refresh(
                &self,
                _r: crate::auth::manager::RefreshReason,
            ) -> crate::auth::refresh::RefreshOutcome {
                crate::auth::refresh::RefreshOutcome::Success(Box::new(KimiAuth {
                    key: "fresh-token".into(),
                    refresh_token: Some("rt-2".into()),
                    expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                    ..KimiAuth::test_default()
                }))
            }
        }

        let auth_dir = tempfile::TempDir::new().unwrap();
        let auth_manager = Arc::new(AuthManager::new(auth_dir.path(), KimiCodeConfig::default()));
        auth_manager.hot_swap(KimiAuth {
            key: "stale-token".into(),
            refresh_token: Some("rt-1".into()),
            // Minted long ago: the recovery state machine skips the refresh
            // for freshly-minted tokens (refresh-storm grace).
            create_time: chrono::Utc::now() - chrono::Duration::hours(2),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            ..KimiAuth::test_default()
        });
        auth_manager.set_refresher(Arc::new(SwapRefresher));

        let mut cfg = config::Config::default();
        cfg.endpoints.coding_api_base_url = Some(server.uri());
        let mgr = ModelsManager::new(
            None,
            IndexMap::new(),
            acp::ModelId::new("default"),
            auth_manager.clone(),
            cfg.clone(),
        );

        let models = mgr
            .fetch_catalog_with_oauth_retry(&cfg)
            .await
            .expect("401 must trigger refresh + one retry with the rotated bearer");
        assert!(models.contains_key("kimi-code/kimi-for-coding"));
        assert_eq!(
            auth_manager.current().expect("refreshed").key,
            "fresh-token",
            "the 401 recovery must have rotated the bearer"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "exactly one retry after the refresh"
        );
    }

    /// PRD F4 failure ladder: sync failure → last cache (even stale, same
    /// fetch plan only); no cache → the bundled offline table; and a FRESH
    /// cache short-circuits the network entirely.
    #[test]
    #[serial]
    fn sync_failure_uses_last_cache_then_bundled_table() {
        let cache_dir = tempfile::TempDir::new().unwrap();
        let _cache = EnvGuard::set("KIGI_MODELS_CACHE_DIR", cache_dir.path().to_str().unwrap());
        // Unroutable server: every fetch fails fast with connection refused.
        let endpoints = proxied_endpoints("http://127.0.0.1:9");
        let keys = PlatformApiKeys::default();
        let auth = KimiAuth {
            key: "tok".into(),
            ..KimiAuth::test_default()
        };

        // 1. No cache at all → fetch failure → no models: callers resolve the
        //    bundled offline table.
        let outcome = prefetch_models_blocking_gated(
            &endpoints,
            Some(&auth),
            &Default::default(),
            ModelFetchAuth::Platforms,
            &keys,
            true,
        );
        assert!(outcome.models.is_none(), "no cache and no network → None");
        let bundled = resolve_model_catalog(&config::Config::default(), None, &Default::default());
        assert!(bundled.contains_key("kimi-code/kimi-for-coding"));
        assert!(bundled.contains_key("moonshot-cn/kimi-k2-thinking-turbo"));
        assert!(bundled.contains_key("moonshot-ai/kimi-k2-turbo-preview"));

        // 2. A STALE cache for the same fetch plan is served on sync failure.
        let origin = crate::agent::models_fetch::models_fetch_origin(
            &endpoints,
            ModelFetchAuth::Platforms,
            true,
            &Default::default(),
            &keys,
        );
        let cache = ModelsCacheManager::new();
        let stale = ModelsCache {
            fetched_at: Utc::now() - ChronoDuration::seconds(86_400),
            kigi_version: Some(kigi_version::VERSION.to_string()),
            auth_method: Some(CacheAuthMethod::Platforms),
            origin: Some(origin),
            etag: None,
            models: make_prefetched(&["kimi-code/cached-model"]),
        };
        cache.atomic_write(&stale);
        let outcome = prefetch_models_blocking_gated(
            &endpoints,
            Some(&auth),
            &Default::default(),
            ModelFetchAuth::Platforms,
            &keys,
            true,
        );
        let models = outcome
            .models
            .expect("stale cache must beat the bundled table on sync failure");
        assert!(models.contains_key("kimi-code/cached-model"));

        // 3. A FRESH cache short-circuits the network (offline continuity).
        let fresh = ModelsCache {
            fetched_at: Utc::now(),
            ..stale
        };
        cache.atomic_write(&fresh);
        let outcome = prefetch_models_blocking_gated(
            &endpoints,
            Some(&auth),
            &Default::default(),
            ModelFetchAuth::Platforms,
            &keys,
            true,
        );
        assert!(
            outcome
                .models
                .expect("fresh cache must serve without network")
                .contains_key("kimi-code/cached-model")
        );
    }

    /// PRD F4 default-thinking seam: capabilities from the catalog drive the
    /// thinking default (thinking/always_thinking → on; else off).
    #[test]
    fn model_default_thinking_follows_capabilities() {
        let mgr = test_manager();
        let mut thinking = ModelEntry {
            info: config::ModelInfo::fallback("kimi-thinking-x"),
            api_key: None,
            env_key: None,
            api_base_url: None,
        };
        thinking.info.capabilities = vec![kigi_models::ModelCapability::AlwaysThinking];
        mgr.insert_test_entry("kimi-code/kimi-thinking-x", thinking);
        let mut plain = ModelEntry {
            info: config::ModelInfo::fallback("kimi-plain-x"),
            api_key: None,
            env_key: None,
            api_base_url: None,
        };
        plain.info.capabilities = vec![kigi_models::ModelCapability::ImageIn];
        mgr.insert_test_entry("kimi-code/kimi-plain-x", plain);

        assert!(mgr.model_default_thinking("kimi-code/kimi-thinking-x"));
        // Routing-slug lookup resolves to the catalog key too.
        assert!(mgr.model_default_thinking("kimi-thinking-x"));
        assert!(!mgr.model_default_thinking("kimi-code/kimi-plain-x"));
        assert!(!mgr.model_default_thinking("unknown-model"));
    }

    /// A cache written for a DIFFERENT fetch plan (different platform set)
    /// must not be served — not even as the stale last resort.
    #[test]
    #[serial]
    fn stale_cache_from_other_fetch_plan_is_not_served() {
        let cache_dir = tempfile::TempDir::new().unwrap();
        let _cache = EnvGuard::set("KIGI_MODELS_CACHE_DIR", cache_dir.path().to_str().unwrap());
        let endpoints = proxied_endpoints("http://127.0.0.1:9");
        // Cache written when a moonshot key was ALSO configured...
        let with_key_origin = crate::agent::models_fetch::models_fetch_origin(
            &endpoints,
            ModelFetchAuth::Platforms,
            true,
            &Default::default(),
            &PlatformApiKeys::test_keys(Some("sk"), None),
        );
        let cache = ModelsCacheManager::new();
        cache.atomic_write(&ModelsCache {
            fetched_at: Utc::now() - ChronoDuration::seconds(86_400),
            kigi_version: Some(kigi_version::VERSION.to_string()),
            auth_method: Some(CacheAuthMethod::Platforms),
            origin: Some(with_key_origin),
            etag: None,
            models: make_prefetched(&["moonshot-cn/poisoned"]),
        });
        // ... must be a miss for an OAuth-only plan.
        let auth = KimiAuth {
            key: "tok".into(),
            ..KimiAuth::test_default()
        };
        let outcome = prefetch_models_blocking_gated(
            &endpoints,
            Some(&auth),
            &Default::default(),
            ModelFetchAuth::Platforms,
            &PlatformApiKeys::default(),
            true,
        );
        assert!(
            outcome.models.is_none(),
            "an origin-mismatched cache must never be adopted"
        );
    }
}
